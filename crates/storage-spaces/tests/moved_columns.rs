//! A parity space whose column 0 Windows moved to another disk
//! (`replacedpar`, Windows 11 build 26340: a disk added, the one under
//! column 0 retired, the space repaired, the old disk removed). Windows
//! numbers the moved column copy 1. 1.2.2 knew copy 0 of column 0 alone in
//! the parity paths: it could not read the space with a disk missing, nor
//! write it (audit 2026-10-16, S9). The fixture holds the first 8 KiB of
//! every unit of the first three stripes, which hold the pattern.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use storage_spaces::Pool;
use storage_spaces::io::{Overlay, ReadAt, SparseImage};
use storage_spaces::testpattern;

fn images() -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replacedpar");
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// A device that fails its reads once `broken` is set.
struct Failing<D> {
    inner: D,
    broken: Arc<AtomicBool>,
}

impl<D: ReadAt> ReadAt for Failing<D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        if self.broken.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("failed"));
        }
        self.inner.read_exact_at(buf, offset)
    }
    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }
}

/// Reads `len` bytes at each of `offsets` with each disk failing in turn.
fn read_degraded<D: ReadAt>(disks: &[D], offsets: &[u64], len: usize) -> Vec<Vec<Vec<u8>>> {
    (0..disks.len())
        .map(|victim| {
            let flags: Vec<Arc<AtomicBool>> = (0..disks.len()).map(|_| Arc::new(AtomicBool::new(false))).collect();
            let devices: Vec<Failing<&D>> = disks
                .iter()
                .zip(&flags)
                .map(|(d, f)| Failing {
                    inner: d,
                    broken: f.clone(),
                })
                .collect();
            let pool = Pool::open(devices).unwrap();
            let r = pool.open_space(pool.find_space("replacedpar").unwrap().id()).unwrap();
            flags[victim].store(true, Ordering::Relaxed);
            offsets
                .iter()
                .map(|&at| {
                    let mut v = vec![0u8; len];
                    r.read_exact_at(&mut v, at)
                        .unwrap_or_else(|e| panic!("without disk {victim} at {at:#x}: {e}"));
                    v
                })
                .collect()
        })
        .collect()
}

/// Column 0 is copy 1 in the records, as Windows' extents say, and every
/// data unit of the first stripes reads as written with any disk failing.
#[test]
fn a_moved_parity_column_is_read_degraded() {
    let images = images();
    let pool = Pool::open(images.iter().collect::<Vec<_>>()).unwrap();
    let space = pool.find_space("replacedpar").unwrap();
    let r = pool.open_space(space.id()).unwrap();
    let l = r.layout();
    assert_eq!((l.columns, l.data_columns, l.copies_of(0)), (3, 2, vec![1]));
    assert_eq!((l.copies_of(1), l.copies_of(2)), (vec![0], vec![0]));
    let (unit, stripe) = (l.interleave, l.interleave * 2);
    let offsets: Vec<u64> = (0..3).flat_map(|s| [s * stripe, s * stripe + unit]).collect();
    for (victim, reads) in read_degraded(&images, &offsets, 8192).iter().enumerate() {
        for (&at, v) in offsets.iter().zip(reads) {
            assert_eq!(
                testpattern::verify(v, at, "replacedpar"),
                None,
                "without disk {victim} at {at:#x}"
            );
        }
    }
}

/// Written through the write-back cache and destaged, a block in the first
/// stripe reads back from the pool opened again, also with any disk
/// failing (the parity the destage wrote covers the moved column).
#[test]
fn a_moved_parity_column_is_written() {
    let images = images();
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("replacedpar").unwrap().id();
    let w = pool.open_space_rw(id).unwrap();
    let mut block = vec![0u8; testpattern::BLOCK];
    testpattern::fill_block(&mut block, 4096, "linux");
    w.write_all_at(&block, 4096).unwrap();
    w.flush().unwrap();
    w.destage().unwrap();
    drop(w);
    drop(pool);
    let reads = read_degraded(&disks, &[0, 4096], 4096);
    for (victim, r) in reads.iter().enumerate() {
        assert_eq!(
            testpattern::verify(&r[0], 0, "replacedpar"),
            None,
            "without disk {victim}"
        );
        assert!(r[1] == block, "without disk {victim}: the written block");
    }
    // Every disk at hand: the same, and the stripe matches its parity.
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let found = storage_spaces::ops::scrub_spaces(&pool, Some(id)).unwrap();
    assert_eq!((found.mismatches, found.unsettled), (0, 0), "{:?}", found.lines);
}
