//! A parity space on slabs that held other data (audit 2026-10-16, W3).
//! New spaces get the free slabs as they are, not cleared, as on Windows;
//! where a deleted space left data, the stripes of a parity space never
//! written do not match their parity. Windows' journal lists a run whole
//! when a write first touches it, its stripes never written as not
//! consistent: readers check those, so that in 1.2.2 the next session's
//! reads of them failed, the space could not be opened for writing again
//! and the guard called it suspect.

use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, SparseImage};
use storage_spaces::ops::{BlankDisk, SpaceSpec, plan_create_pool, plan_create_space, plan_delete_space};
use storage_spaces::report::Verdict;
use storage_spaces::{Guid, Pool};

fn guids() -> impl FnMut() -> Guid {
    let mut n = 0u64;
    move || {
        n += 1;
        let mut g = [0u8; 16];
        g[..8].copy_from_slice(&n.to_be_bytes());
        g[8..].copy_from_slice(&0x5a5a_4c69_6e75_7800u64.to_be_bytes());
        Guid(g)
    }
}

fn spec(name: &str, resiliency: u8, size: u64) -> SpaceSpec {
    SpaceSpec {
        name: name.into(),
        resiliency,
        size,
        thin: true,
        copies: None,
        columns: None,
        interleave: None,
        write_cache: None,
    }
}

/// The verdict of the guard on space `name` (the quick checks, file
/// systems as `spaces check` reads them), and the report.
fn verdict(pool: &Pool<&Overlay<&SparseImage>>, name: &str) -> (Verdict, String) {
    let reports = guard::pool_reports(pool, &[], &Options::default(), &mut guard::ntfs_hook);
    let r = reports
        .iter()
        .find(|r| r.space.as_ref().is_some_and(|(n, _)| n == name))
        .unwrap();
    (r.verdict(), r.to_text())
}

/// Three disks; a thin simple space filled in part with other data and
/// deleted; a thin parity space on its slabs. Session one writes a few
/// sectors and closes; the space opens for writing again, the guard calls
/// it healthy, what was written reads back and what never was reads as
/// the slabs hold it; and so after a second and a third session.
#[test]
fn a_parity_space_on_recycled_slabs_is_written_again() {
    let mut new_guid = guids();
    let images: Vec<SparseImage> = (0..3).map(|_| SparseImage::new(8 << 30)).collect();
    let blank: Vec<BlankDisk> = (0..3)
        .map(|_| BlankDisk {
            size: 8 << 30,
            logical_sector: 512,
            physical_sector: 4096,
            manufacturer: "Linux".into(),
            model: "test".into(),
        })
        .collect();
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&blank, "recycled", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();

    // The old space: 24 MiB of other data at the start of ten rows.
    let pool = Pool::open(members()).unwrap();
    let (plan, _) = plan_create_space(&pool, &spec("old", 1, 8 << 30), &mut new_guid).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    let w = pool.open_space_rw(pool.find_space("old").unwrap().id()).unwrap();
    let old: Vec<u8> = (0..24 << 20).map(|i: usize| (i % 253) as u8 | 1).collect();
    for row in 0..10u64 {
        w.write_all_at(&old, row * (768 << 20)).unwrap();
    }
    w.flush().unwrap();
    drop(w);
    let plan = plan_delete_space(&pool, "old").unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();

    // The parity space on those slabs.
    let pool = Pool::open(members()).unwrap();
    let (plan, _) = plan_create_space(&pool, &spec("tp", 3, 4 << 30), &mut new_guid).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();

    let writes: [(u64, u8, usize); 3] = [
        (10 << 20, 0x77, 4096),
        (4 << 20, 0x55, 4096),
        ((5 << 20) + 512, 0x66, 512),
    ];
    for session in 0..writes.len() {
        let pool = Pool::open(members()).unwrap();
        let id = pool.find_space("tp").unwrap().id();
        let w = pool
            .open_space_rw(id)
            .unwrap_or_else(|e| panic!("session {session}: open for writing: {e}"));
        let (at, byte, len) = writes[session];
        w.write_all_at(&vec![byte; len], at).unwrap();
        w.flush().unwrap();
        w.destage().unwrap();
        w.flush().unwrap();
        drop(w);
        drop(pool);

        let pool = Pool::open(members()).unwrap();
        let (v, text) = verdict(&pool, "tp");
        assert_eq!(v, Verdict::Healthy, "after session {session}:\n{text}");
        let r = pool.open_space(id).unwrap();
        for &(at, byte, len) in &writes[..=session] {
            let mut b = vec![0u8; len];
            r.read_exact_at(&mut b, at).unwrap();
            assert!(b.iter().all(|&x| x == byte), "after session {session}: at {at:#x}");
        }
        // Stripes never written read as the slabs hold them: the old data.
        let mut b = vec![0u8; 4096];
        r.read_exact_at(&mut b, 2 << 20)
            .unwrap_or_else(|e| panic!("after session {session}: a stripe never written: {e}"));
        assert!(
            b.iter().all(|&x| x != 0),
            "after session {session}: zeros where the old data was"
        );
    }
}
