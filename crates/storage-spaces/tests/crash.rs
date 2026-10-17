//! Pools whose disks were pulled while Windows was writing (see
//! tools/vm/New-CrashPool.ps1). Every MiB we read from the crashed disks must
//! either equal what Windows shows after recovering the pool, or be refused
//! (at most 1 % of the space); silently different data is a failure.

mod common;

use std::fs::{self, File};
use std::path::Path;

use storage_spaces::{OpenOptions, Pool, UncleanParity};

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8))
}

#[test]
fn crashed_pools_read_like_windows_recovers_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/crash");
    let Ok(entries) = fs::read_dir(&root) else {
        eprintln!("no crash pools in {}, skipping", root.display());
        return;
    };
    let mut dirs: Vec<_> = entries
        .map(|e| e.unwrap().path())
        // Hidden directories are downloads in progress.
        .filter(|p| p.join("manifest.json").exists() && !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect();
    dirs.sort();
    for dir in dirs {
        let m = common::manifest(&dir);
        let name = m["space"]["name"].as_str().unwrap();
        let want: Vec<&str> = m["recovered_mib_crc32"].as_str().unwrap().split_whitespace().collect();
        let n = m["disks"].as_array().unwrap().len();
        let files = (0..n)
            .map(|i| File::open(dir.join(format!("disk{i}.img"))).unwrap())
            .collect();
        let pool = Pool::open(files).unwrap();
        let id = pool.find_space(name).unwrap().id();
        let strict = pool.open_space(id).unwrap();
        let lenient = pool
            .open_space_with(
                id,
                OpenOptions {
                    unclean_parity: UncleanParity::PreferData,
                },
            )
            .unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let (mut refused, mut lenient_diff) = (0, 0);
        for (i, w) in want.iter().enumerate() {
            let offset = (i as u64) << 20;
            match strict.read_exact_at(&mut buf, offset) {
                Ok(()) => assert_eq!(
                    format!("{:08x}", crc32(&buf)),
                    *w,
                    "{name}: MiB {i} differs from Windows"
                ),
                Err(_) => refused += 1,
            }
            lenient.read_exact_at(&mut buf, offset).unwrap();
            if format!("{:08x}", crc32(&buf)) != *w {
                lenient_diff += 1;
            }
        }
        eprintln!(
            "{name}: {} MiB, refused {refused}, differing when preferring data {lenient_diff}",
            want.len()
        );
        // Parity pools refuse a few stripes; mirrors that were written with
        // small random writes refuse the rows whose copies differ.
        assert!(refused * 100 <= want.len(), "{name}: {refused} MiB refused");
    }
}

/// The guard of `spaces attach` (its default read budget) on the crashed
/// mirrors: the copies of a run the dirty region log lists differ where a
/// write reached one disk only, and the reads there fail, so the space is
/// suspect (1.2.2: healthy, "their copies are compared when read").
#[test]
fn crashed_mirrors_are_suspect() {
    use storage_spaces::guard::{self, Options};
    use storage_spaces::report::{Status, Verdict};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/crash");
    for name in ["crashmirror", "crashmirrorwc"] {
        let dir = root.join(name);
        let files: Vec<File> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.img"))).ok())
            .collect();
        if files.is_empty() {
            eprintln!("{} missing, skipped", dir.display());
            continue;
        }
        let pool = Pool::open(files).unwrap();
        let space = pool.find_space(name).unwrap();
        let r = guard::space_report(&pool, space, &[], &Options::default(), &mut guard::ntfs_hook);
        let drl = r.checks.iter().find(|c| c.id == "space.drl").unwrap();
        assert_eq!(drl.status, Status::Suspect, "{}", r.to_text());
        assert_eq!(r.verdict(), Verdict::Suspect, "{}", r.to_text());
    }
}
