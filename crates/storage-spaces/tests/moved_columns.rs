//! A parity space whose column 0 Windows moved to another disk
//! (`replacedpar`, Windows 11 build 26340: a disk added, the one under
//! column 0 retired, the space repaired, the old disk removed). Windows
//! numbers the moved column copy 1. 1.2.2 knew copy 0 of column 0 alone in
//! the parity paths: it could not read the space with a disk missing, nor
//! write it (audit 2026-10-16, S9). The fixture holds the first 8 KiB of
//! every unit of the first three stripes, which hold the pattern.
//!
//! `replacedrun` (build 26340): a thin parity space of four rows, a run
//! each, whose column 0 Windows moved for the last two rows only (the disk
//! under them replaced): copy 0 holds column 0 of rows 0 and 1, copy 1 that
//! of rows 2 and 3. Its fixture holds the first 8 KiB of every unit of the
//! first three stripes of each row.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use storage_spaces::Pool;
use storage_spaces::format::SpaceRole;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, SparseImage, WriteAt};
use storage_spaces::layout::Layout;
use storage_spaces::report::Verdict;
use storage_spaces::testpattern;

fn images(name: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
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

/// Reads `len` bytes of space `name` at each of `offsets` with each disk
/// failing in turn.
fn read_degraded<D: ReadAt>(disks: &[D], name: &str, offsets: &[u64], len: usize) -> Vec<Vec<Vec<u8>>> {
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
            let r = pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
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
    let images = images("replacedpar");
    let pool = Pool::open(images.iter().collect::<Vec<_>>()).unwrap();
    let space = pool.find_space("replacedpar").unwrap();
    let r = pool.open_space(space.id()).unwrap();
    let l = r.layout();
    assert_eq!((l.columns, l.data_columns, l.copies_of(0)), (3, 2, vec![1]));
    assert_eq!((l.copies_of(1), l.copies_of(2)), (vec![0], vec![0]));
    let (unit, stripe) = (l.interleave, l.interleave * 2);
    let offsets: Vec<u64> = (0..3).flat_map(|s| [s * stripe, s * stripe + unit]).collect();
    for (victim, reads) in read_degraded(&images, "replacedpar", &offsets, 8192).iter().enumerate() {
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
    let images = images("replacedpar");
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
    let reads = read_degraded(&disks, "replacedpar", &[0, 4096], 4096);
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

/// The offsets of `replacedrun`'s fixture: the first 8 KiB of every data
/// unit of the first three stripes of each row.
fn run_offsets(l: &storage_spaces::layout::Layout, rows: u64) -> Vec<u64> {
    let (unit, stripe) = (l.interleave, l.interleave * l.data_columns);
    let row = storage_spaces::format::SLAB_SIZE * l.data_columns;
    (0..rows)
        .flat_map(|r| (0..3).flat_map(move |s| (0..2).map(move |u| r * row + s * stripe + u * unit)))
        .collect()
}

/// Column 0 of `replacedrun` is copy 0 in rows 0 and 1 and copy 1 in rows 2
/// and 3, and every run counts its parity rotation from its own first row:
/// every data unit reads as written, healthy and with any disk failing.
/// 1.3.0 took the runs of column 0's copy 0 alone, so that rows 2 and 3
/// counted their stripes from row 0: row 2 read wrong data from the healthy
/// pool, and no error said so (audit 2026-10-21, SPB-01).
#[test]
fn a_column_moved_for_some_rows_is_read_as_written() {
    let images = images("replacedrun");
    let pool = Pool::open(images.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("replacedrun").unwrap().id();
    let r = pool.open_space(id).unwrap();
    let l = r.layout();
    assert_eq!((l.columns, l.data_columns, l.copies_of(0)), (3, 2, vec![0, 1]));
    let firsts: Vec<u64> = l.column_runs(0).unwrap().iter().map(|r| r.first_row).collect();
    assert_eq!(firsts, vec![0, 1, 2, 3], "column 0's runs, of either copy");
    let offsets = run_offsets(l, 4);
    let mut v = vec![0u8; 8192];
    for &at in &offsets {
        r.read_exact_at(&mut v, at).unwrap();
        assert_eq!(testpattern::verify(&v, at, "replacedrun"), None, "at {at:#x}");
    }
    for (victim, reads) in read_degraded(&images, "replacedrun", &offsets, 8192).iter().enumerate() {
        for (&at, v) in offsets.iter().zip(reads) {
            assert_eq!(
                testpattern::verify(v, at, "replacedrun"),
                None,
                "without disk {victim} at {at:#x}"
            );
        }
    }
}

/// Written in row 2, whose column 0 is copy 1, through the write-back cache
/// and destaged: the block reads back from the pool opened again, also with
/// any disk failing, and the stripes match their parity (1.3.0's writer
/// found no run of column 0 for the row and refused the write).
#[test]
fn a_column_moved_for_some_rows_is_written() {
    let images = images("replacedrun");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("replacedrun").unwrap().id();
    let at = 2 * storage_spaces::format::SLAB_SIZE * 2 + 4096;
    let w = pool.open_space_rw(id).unwrap();
    let mut block = vec![0u8; testpattern::BLOCK];
    testpattern::fill_block(&mut block, at, "linux");
    w.write_all_at(&block, at).unwrap();
    w.flush().unwrap();
    w.destage().unwrap();
    drop(w);
    drop(pool);
    let reads = read_degraded(&disks, "replacedrun", &[at - 4096, at], 4096);
    for (victim, r) in reads.iter().enumerate() {
        assert_eq!(
            testpattern::verify(&r[0], at - 4096, "replacedrun"),
            None,
            "without disk {victim}"
        );
        assert!(r[1] == block, "without disk {victim}: the written block");
    }
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let found = storage_spaces::ops::scrub_spaces(&pool, Some(id)).unwrap();
    assert_eq!((found.mismatches, found.unsettled), (0, 0), "{:?}", found.lines);
}

/// The write-back cache of `replacedrun` is a two-column mirror whose copies
/// the disk replacement numbered column by column: column 0 of its first row
/// has copies 0 and 1, column 1 copies 0 and 2. Its slot area is read from
/// both copies: of a chunk cached by one slot and destaged by the next, the
/// second damaged on one copy, the destage is read from the other copy, and
/// the copies' disagreement makes the space suspect. 1.3.0 read the slot area
/// of copy 0 alone, where the damaged slot ended the log: the chunk was cached
/// again with what its block held, and the space healthy (the oracle of plan
/// item 3.2, replacedrun damage mutant 252).
#[test]
fn the_slot_area_of_a_cache_moved_column_by_column_is_read_from_both_copies() {
    let images = images("replacedrun");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("replacedrun").unwrap().id();
    let container = pool.children(id).find(|c| c.info.role == SpaceRole::Cache).unwrap();
    let child = pool.children(container.id()).find(|c| !c.extents.is_empty()).unwrap();
    let l = Layout::with_base(child.info.policy.as_ref().unwrap(), &child.extents, 0).unwrap();
    // Writes `bytes` at cache offset `at` on every copy of its unit, or on
    // the first.
    let put = |at: u64, bytes: &[u8], every: bool| {
        let loc = l.locate(at);
        let places: Vec<(u64, u64)> = l
            .copies_of(loc.column)
            .iter()
            .filter_map(|&copy| l.physical(loc.column, copy, loc.row))
            .collect();
        assert_eq!(places.len(), 2);
        for &(disk, slab) in &places[..if every { 2 } else { 1 }] {
            let (device, start) = pool.slab_location(disk, slab).unwrap().unwrap();
            disks[device].write_all_at(bytes, start + loc.offset_in_slab).unwrap();
        }
    };
    let reader = pool.open_space(id).unwrap();
    let index = reader.cache().unwrap();
    let header = index.header.clone();
    let mut w = index.writer(64);
    let mut slots = w.write(0, 4096);
    slots.extend(w.destage(&[0]));
    drop(reader);
    let slot_at = |i: usize| header.slot_offset + i as u64 * u64::from(header.slot_size);
    for s in &slots {
        let (i, page) = s.slot().unwrap();
        put(slot_at(i), page, true);
    }
    let report = |pool: &Pool<&Overlay<&SparseImage>>| {
        guard::space_report(pool, &pool.spaces[&id], &[], &Options::default(), &mut |_, _, _| {
            Vec::new()
        })
    };
    let r = pool.open_space(id).unwrap();
    assert_eq!(r.cache().unwrap().cached_chunks(), 0);
    assert_eq!(report(&pool).verdict(), Verdict::Healthy, "{}", report(&pool).to_text());
    drop(r);

    let (last, _) = slots.last().unwrap().slot().unwrap();
    put(slot_at(last), &[0u8; 512], false);
    let r = pool.open_space(id).unwrap();
    let cache = r.cache().unwrap();
    assert_eq!(cache.cached_chunks(), 0, "the log rolled back");
    assert_eq!(cache.conflicting_offsets(), [0]);
    assert_eq!(report(&pool).verdict(), Verdict::Suspect, "{}", report(&pool).to_text());
}
