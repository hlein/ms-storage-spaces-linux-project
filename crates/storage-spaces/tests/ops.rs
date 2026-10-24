//! Management operations planned and applied on blank in-memory disks: the
//! results open as clean pools whose spaces read and write, and a crash
//! at any point between steps leaves a pool that opens in the old or the
//! new state.

use storage_spaces::Guid;
use storage_spaces::Pool;
use storage_spaces::io::{DeviceEvent, Overlay, Recorder, SparseImage, WriteAt};
use storage_spaces::ops::{BlankDisk, SpaceSpec, check_pool, plan_create_pool, plan_create_space};

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

fn blank(n: usize) -> (Vec<SparseImage>, Vec<BlankDisk>) {
    let disks = (0..n).map(|_| SparseImage::new(8 << 30)).collect();
    let specs = (0..n)
        .map(|_| BlankDisk {
            size: 8 << 30,
            logical_sector: 512,
            physical_sector: 4096,
            manufacturer: "Linux".into(),
            model: "test".into(),
        })
        .collect();
    (disks, specs)
}

fn spec(name: &str, resiliency: u8, size_mib: u64, thin: bool) -> SpaceSpec {
    SpaceSpec {
        name: name.into(),
        resiliency,
        size: size_mib << 20,
        thin,
        copies: None,
        columns: None,
        interleave: None,
        write_cache: None,
    }
}

/// A pool of three disks gets one space of each kind; after each, the pool
/// opens clean and every space reads back what was written to it.
#[test]
fn created_pools_and_spaces_open_clean_and_work() {
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let devices: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "linuxpool", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &devices.iter().collect::<Vec<_>>())
        .unwrap();
    let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    assert_eq!(pool.name, "linuxpool");
    check_pool(&pool).unwrap();
    drop(pool);
    let kinds = [
        spec("s", 1, 1024, false),
        spec("m", 2, 1024, false),
        spec("p", 3, 2048, false),
        spec("st", 1, 4096, true),
        spec("mt", 2, 4096, true),
        spec("pt", 3, 4096, true),
    ];
    for (k, s) in kinds.iter().enumerate() {
        let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
        let (plan, new) = plan_create_space(&pool, s, &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&devices.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
        assert!(pool.warnings.is_empty(), "{}: {:?}", s.name, pool.warnings);
        check_pool(&pool).unwrap();
        let space = pool.find_space(&s.name).unwrap();
        assert_eq!(space.info.size, Some(new.size));
        // Every space created so far reads back its own pattern.
        let w = pool.open_space_rw(space.id()).unwrap();
        let data: Vec<u8> = (0..1 << 20).map(|i| (i as u8) ^ (k as u8 * 37)).collect();
        w.write_all_at(&data, 3 << 20).unwrap();
        w.flush().unwrap();
        drop(w);
        let r = pool.open_space(space.id()).unwrap();
        let mut back = vec![0u8; data.len()];
        r.read_exact_at(&mut back, 3 << 20).unwrap();
        assert!(back == data, "{}", s.name);
        // The first sector is zero.
        let mut first = [1u8; 512];
        r.read_exact_at(&mut first, 0).unwrap();
        assert_eq!(first, [0; 512]);
    }
}

/// A crash after any step of creating a space leaves a pool that opens and
/// reads: without the space until a database copy is written, with it once
/// the newest copy has it (Windows' rule), and clean again after the last.
/// Writes after the last flush may land in any subset.
#[test]
fn a_crash_while_creating_a_space_leaves_a_pool_that_opens() {
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "crashpool", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base.iter().collect::<Vec<_>>())
        .unwrap();
    for (k, s) in [spec("p", 3, 2048, false), spec("m", 2, 1024, true)].iter().enumerate() {
        let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, s, &mut new_guid).unwrap();
        drop(pool);
        // Record the plan's writes and flushes.
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
        let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
            .iter()
            .enumerate()
            .map(|(i, d)| Recorder::new(d, i, log.clone()))
            .collect();
        plan.apply::<_, &Recorder<&Overlay<&Overlay<&SparseImage>>>>(&recorders.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        let events = log.lock().unwrap().clone();
        let mut last_flush = 0;
        let mut rng = 0x9e37_79b9_7f4a_7c15u64 ^ k as u64;
        let (mut without, mut with) = (0, 0);
        for end in 0..=events.len() {
            if end > 0 && matches!(events[end - 1], DeviceEvent::Flush { .. }) {
                last_flush = end;
            }
            // The flushed writes, and a random subset of the rest (four
            // subsets per point, all of them when nothing is pending).
            let pending: Vec<usize> = (last_flush..end)
                .filter(|&i| matches!(events[i], DeviceEvent::Write { .. }))
                .collect();
            for _ in 0..if pending.is_empty() { 1 } else { 4 } {
                let replay: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
                for (i, e) in events[..end].iter().enumerate() {
                    if let DeviceEvent::Write { device, offset, data } = e {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        if i < last_flush || rng & 1 == 1 {
                            replay[*device].write_all_at(data, *offset).unwrap();
                        }
                    }
                }
                let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
                match pool.find_space(&s.name) {
                    None => without += 1,
                    Some(space) => {
                        with += 1;
                        // The space is complete where the pool shows it.
                        let r = pool.open_space(space.id()).unwrap();
                        let mut first = [1u8; 512];
                        r.read_exact_at(&mut first, 0).unwrap();
                    }
                }
                if end == events.len() {
                    assert!(pool.find_space(&s.name).is_some());
                    check_pool(&pool).unwrap();
                }
                if end == 0 {
                    assert!(pool.warnings.is_empty());
                }
            }
        }
        assert!(without > 0 && with > 0, "{without} {with}");
        // Carry it out for the next space.
        plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
            .unwrap();
    }
}

fn data(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31) ^ tag).collect()
}

/// Every space of the pool reads back its data.
fn check_data<D: storage_spaces::io::ReadAt>(pool: &Pool<D>, spaces: &[(&str, u8)]) {
    for (name, tag) in spaces {
        let space = pool.find_space(name).unwrap_or_else(|| panic!("{name} is gone"));
        let r = pool.open_space(space.id()).unwrap();
        let want = data(*tag, 8 << 20);
        let mut got = vec![0u8; want.len()];
        r.read_exact_at(&mut got, 1 << 20).unwrap();
        assert!(got == want, "{name}");
    }
}

/// A disk added, a disk retired (its data moved away) and removed: the
/// data of every space stays readable throughout, also at every point a
/// crash could stop the retirement, and the pool opens clean at the end
/// with the three remaining disks.
#[test]
fn disks_are_added_retired_and_removed() {
    use storage_spaces::ops::{plan_add_disk, plan_remove_disk, plan_retire_disk};
    let mut new_guid = guids();
    let (images, disks) = blank(4);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks[..3], "lifecycle", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base[..3].iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("s", 1u8), ("m", 2), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 1024, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base[..3].iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    // Add the fourth disk.
    let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
    let plan = plan_add_disk(&pool, &disks[3], &mut new_guid).unwrap();
    drop(pool);
    plan.apply(&base[..3].iter().collect::<Vec<_>>(), &[&base[3]]).unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    assert_eq!(pool.members.len(), 4);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    // Retire disk 1, recording every write, and replay the crash points.
    let first = pool.disks.values().find(|d| d.member == Some(0)).unwrap().id;
    let plan = plan_retire_disk(&pool, first).unwrap();
    drop(pool);
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
    let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
        .iter()
        .enumerate()
        .map(|(i, d)| Recorder::new(d, i, log.clone()))
        .collect();
    plan.apply::<_, &Recorder<&Overlay<&Overlay<&SparseImage>>>>(&recorders.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let events = log.lock().unwrap().clone();
    for end in (0..=events.len()).filter(|&e| e == 0 || matches!(events[e - 1], DeviceEvent::Flush { .. })) {
        let replay: Vec<Overlay<&Overlay<&SparseImage>>> = base.iter().map(Overlay::new).collect();
        for e in &events[..end] {
            if let DeviceEvent::Write { device, offset, data } = e {
                replay[*device].write_all_at(data, *offset).unwrap();
            }
        }
        let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
        check_data(&pool, &spaces);
    }
    // Carry it out, then remove the disk.
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    assert!(
        pool.spaces
            .values()
            .all(|s| s.info.role == storage_spaces::format::SpaceRole::Metadata
                || s.extents.iter().all(|e| e.disk_id != first))
    );
    let plan = plan_remove_disk(&pool, first).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(base[1..].iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    assert_eq!(pool.members.len(), 3);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    // The removed disk is no member any more.
    assert!(Pool::open(vec![&base[0]]).is_err());
}

/// A disk lost: the repair rebuilds its copies of a mirror and a parity
/// space on the other disks (the parity column as the XOR of the others),
/// the data readable at every point a crash could stop it; afterwards the
/// missing disk is removed and the pool is clean again.
#[test]
fn a_lost_disk_is_repaired_away_and_removed() {
    use storage_spaces::ops::{plan_remove_disk, plan_repair};
    let mut new_guid = guids();
    let (images, disks) = blank(5);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "repair", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base.iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("m", 2u8), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
        let mut s = spec(name, tag, 1024, false);
        s.columns = Some(if tag == 2 { 1 } else { 3 });
        let (plan, _) = plan_create_space(&pool, &s, &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    // The disk holding the most of the spaces is lost.
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let busiest = (0..5)
        .max_by_key(|&d| {
            let id = pool.disks.values().find(|x| x.member == Some(d)).unwrap().id;
            pool.user_spaces()
                .flat_map(|s| s.extents.iter())
                .filter(|e| e.disk_id == id)
                .count()
        })
        .unwrap();
    let lost = pool.disks.values().find(|x| x.member == Some(busiest)).unwrap().id;
    drop(pool);
    let left: Vec<&Overlay<&SparseImage>> = base
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != busiest)
        .map(|(_, d)| d)
        .collect();
    let pool = Pool::open(left.clone()).unwrap();
    assert!(pool.warnings.iter().any(|w| w.ends_with("is missing")));
    check_data(&pool, &spaces);
    let plan = plan_repair(&pool).unwrap();
    assert!(plan.steps.iter().any(|s| {
        s.actions
            .iter()
            .any(|a| matches!(a, storage_spaces::plan::Action::Xor { .. }))
    }));
    drop(pool);
    // Every crash point of the repair.
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = left.iter().map(|d| Overlay::new(*d)).collect();
    let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
        .iter()
        .enumerate()
        .map(|(i, d)| Recorder::new(d, i, log.clone()))
        .collect();
    plan.apply::<_, &Recorder<&Overlay<&Overlay<&SparseImage>>>>(&recorders.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let events = log.lock().unwrap().clone();
    for end in (0..=events.len()).filter(|&e| e == 0 || matches!(events[e - 1], DeviceEvent::Flush { .. })) {
        let replay: Vec<Overlay<&Overlay<&SparseImage>>> = left.iter().map(|d| Overlay::new(*d)).collect();
        for e in &events[..end] {
            if let DeviceEvent::Write { device, offset, data } = e {
                replay[*device].write_all_at(data, *offset).unwrap();
            }
        }
        let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
        check_data(&pool, &spaces);
    }
    plan.apply::<_, &Overlay<&SparseImage>>(&left, &[]).unwrap();
    let pool = Pool::open(left.clone()).unwrap();
    for space in pool.user_spaces() {
        assert_eq!(
            pool.open_space(space.id()).unwrap().condition(),
            storage_spaces::Condition::Healthy,
            "{}",
            space.name()
        );
    }
    check_data(&pool, &spaces);
    // Nothing left to repair; the missing disk goes.
    assert!(plan_repair(&pool).unwrap().steps.is_empty());
    let plan = plan_remove_disk(&pool, lost).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&left, &[]).unwrap();
    let pool = Pool::open(left).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
}

/// After a disk is added, optimizing moves extents onto it until the disks
/// hold about as much, the data staying readable; a second optimization
/// finds nothing to do.
#[test]
fn an_added_disk_is_filled_by_optimizing() {
    use storage_spaces::ops::{plan_add_disk, plan_rebalance};
    let mut new_guid = guids();
    let (images, disks) = blank(4);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks[..3], "optimize", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base[..3].iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("s", 1u8), ("m", 2), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 2048, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base[..3].iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
    let plan = plan_add_disk(&pool, &disks[3], &mut new_guid).unwrap();
    drop(pool);
    plan.apply(&base[..3].iter().collect::<Vec<_>>(), &[&base[3]]).unwrap();
    let used = |pool: &Pool<&Overlay<&SparseImage>>| -> Vec<u64> {
        let mut u: Vec<u64> = pool
            .disks
            .keys()
            .map(|d| {
                pool.spaces
                    .values()
                    .flat_map(|s| s.extents.iter())
                    .filter(|e| e.disk_id == *d)
                    .map(|e| e.slab_count)
                    .sum()
            })
            .collect();
        u.sort();
        u
    };
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let before = used(&pool);
    assert!(before[0] <= 1, "{before:?}");
    let plan = plan_rebalance(&pool).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    let after = used(&pool);
    assert!(after[3] - after[0] <= 4, "{before:?} -> {after:?}");
    assert!(plan_rebalance(&pool).unwrap().steps.is_empty());
}

/// Scrubbing finds differences between mirror copies and parity units
/// that do not match: where nothing was being written they are mismatches;
/// in a mirror's extent runs written since Windows last had the pool (its
/// dirty region table lists them) and in parity stripes the journal does
/// not list as consistent (never written) they are unsettled. The repair
/// makes both agree and the data reads back as before.
#[test]
fn scrubbing_finds_differences_and_makes_them_agree() {
    use storage_spaces::layout::Location;
    use storage_spaces::ops::scrub;
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "scrub", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    let spaces = [("m", 2u8), ("p", 3)];
    for (name, tag) in spaces.iter().copied().chain([("quiet", 2)]) {
        let pool = Pool::open(members()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 1024, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
        drop(pool);
        if name == "quiet" {
            continue;
        }
        let pool = Pool::open(members()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        // Parity stripes 2-17 of the first row (256 KiB units, 2 data columns).
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.destage().unwrap();
        w.flush().unwrap();
    }
    let pool = Pool::open(members()).unwrap();
    let clean = scrub(&pool).unwrap();
    assert_eq!((clean.mismatches, clean.unsettled), (0, 0), "{:?}", clean.lines);
    assert!(clean.plan.steps.is_empty());

    let corrupt = |disk: u64, slab: u64, offset: u64| {
        let (device, at) = pool.slab_location(disk, slab).unwrap().unwrap();
        base[device].write_all_at(&[0xa5; 4096], at + offset).unwrap();
    };
    let space = |name: &str| pool.open_space(pool.find_space(name).unwrap().id()).unwrap();
    // The second copy of both mirrors, in rows 0 and 1: unsettled in the
    // written one, mismatches in the other.
    for name in ["m", "quiet"] {
        let r = space(name);
        let l = r.layout();
        let second = l.copies_of(0)[1];
        for (row, offset) in [(0, 3 << 20), (1, 5 << 20)] {
            let (disk, slab) = l.physical(0, second, row).unwrap();
            corrupt(disk, slab, offset);
        }
    }
    // The parity unit of stripe 8 (written: a mismatch) and of a stripe of
    // row 1 (never written: unsettled).
    let r = space("p");
    let l = r.layout();
    for row in [0, 1] {
        let loc = Location {
            column: 0,
            row,
            offset_in_slab: 2 << 20,
            contiguous: l.interleave,
        };
        let (disk, slab) = l.physical(l.parity_column(l.stripe_of(&loc)), 0, row).unwrap();
        corrupt(disk, slab, 2 << 20);
    }
    drop(r);

    let found = scrub(&pool).unwrap();
    assert_eq!((found.mismatches, found.unsettled), (3, 3), "{:?}", found.lines);
    assert_eq!(found.plan.steps.iter().map(|s| s.actions.len()).sum::<usize>(), 6);
    drop(pool);
    found.plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    let after = scrub(&pool).unwrap();
    assert_eq!((after.mismatches, after.unsettled), (0, 0), "{:?}", after.lines);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    let r = pool.open_space(pool.find_space("quiet").unwrap().id()).unwrap();
    let mut row1 = vec![1u8; 1 << 20];
    r.read_exact_at(&mut row1, (256 << 20) + (5 << 20)).unwrap();
    assert!(row1.iter().all(|&b| b == 0));
}

/// A parity space of 4 MiB units, more than the MiB scrubbing reads at a
/// time: a parity unit damaged in the last MiB of its slab is repaired
/// inside its unit. 1.2.2 recomputed a whole unit from where the damaged
/// MiB starts, past the end of the slab into the next one of each disk,
/// where another space keeps its data (audit 2026-10-16, M20).
#[test]
fn scrubbing_repairs_units_larger_than_its_reads_inside_them() {
    use storage_spaces::layout::Location;
    use storage_spaces::ops::scrub;
    use storage_spaces::plan::Action;
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "units", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    let mut wide = spec("wide", 3, 1024, false);
    wide.interleave = Some(4 << 20);
    for s in [wide, spec("after", 1, 3072, false)] {
        let pool = Pool::open(members()).unwrap();
        let (plan, _) = plan_create_space(&pool, &s, &mut new_guid).unwrap();
        drop(pool);
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    }
    let pool = Pool::open(members()).unwrap();
    let id = pool.find_space("wide").unwrap().id();
    let w = pool.open_space_rw(id).unwrap();
    w.write_all_at(&data(3, 16 << 20), 0).unwrap();
    w.destage().unwrap();
    w.flush().unwrap();
    drop(w);
    // Every slab of the simple space holds data.
    let after = pool.find_space("after").unwrap().id();
    let w = pool.open_space_rw(after).unwrap();
    let size = w.size();
    for at in (0..size).step_by(64 << 20) {
        w.write_all_at(&data(at as u8, 64 << 20), at).unwrap();
    }
    w.flush().unwrap();
    drop(w);
    // The parity unit of the last stripe of row 0, in its last MiB.
    let r = pool.open_space(id).unwrap();
    let l = r.layout().clone();
    assert_eq!(l.interleave, 4 << 20);
    let last = Location {
        column: 0,
        row: 0,
        offset_in_slab: SLAB - l.interleave,
        contiguous: l.interleave,
    };
    let (disk, slab) = l.physical(l.parity_column(l.stripe_of(&last)), 0, 0).unwrap();
    let (device, start) = pool.slab_location(disk, slab).unwrap().unwrap();
    base[device]
        .write_all_at(&[0xa5; 4096], start + SLAB - (512 << 10))
        .unwrap();
    drop(r);
    let found = scrub(&pool).unwrap();
    assert_eq!(found.mismatches + found.unsettled, 1, "{:?}", found.lines);
    for a in found.plan.steps.iter().flat_map(|s| &s.actions) {
        if let Action::Xor { to_offset, len, .. } = a {
            assert!(
                *to_offset >= start + SLAB - l.interleave && to_offset + len <= start + SLAB,
                "repair at {to_offset:#x}, {len:#x} bytes: outside the unit"
            );
        }
    }
    drop(pool);
    found.plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    let again = scrub(&pool).unwrap();
    assert_eq!((again.mismatches, again.unsettled), (0, 0), "{:?}", again.lines);
    let r = pool.open_space(after).unwrap();
    let mut got = vec![0u8; 64 << 20];
    for at in (0..size).step_by(64 << 20) {
        r.read_exact_at(&mut got, at).unwrap();
        assert!(got == data(at as u8, 64 << 20), "the simple space at {at:#x}");
    }
}

const SLAB: u64 = 256 << 20;

/// The disks of fixture `name`.
fn fixture(name: &str) -> Vec<SparseImage> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    (0..)
        .map_while(|i| std::fs::File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// A write of a pool database copy torn by a crash never passes for the
/// newest copy. A space is renamed (its record moves to other slots) on
/// pools of five, four and two copies; every write of the plan is cut
/// after each of its sectors (the first ones written, or the last ones),
/// after everything before it was written. The pool always opens with the
/// space as it was or as renamed, whole, unless copies of one sequence
/// that differ hold no majority (reported torn: the guard calls that
/// suspect, and the writer refuses it); otherwise opening the space for
/// writing brings every copy up to date. 1.2.2 wrote a copy's header
/// first, in one write with its entries: a torn copy was the newest, and
/// the space vanished (audit 2026-10-16, M20).
#[test]
fn a_torn_database_copy_never_passes_for_the_newest() {
    use std::sync::{Arc, Mutex};
    use storage_spaces::Issue;
    use storage_spaces::ops::plan_rename_space;
    for name in ["mirror3", "parity4", "simple2c"] {
        let images = fixture(name);
        let pool = Pool::open(images.iter().collect::<Vec<_>>()).unwrap();
        let space = pool.find_space(name).unwrap();
        let (id, extents) = (space.id(), space.extents.len());
        let plan = plan_rename_space(&pool, name, "renamed").unwrap();
        drop(pool);
        let log = Arc::new(Mutex::new(Vec::new()));
        let scratch: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let recorders: Vec<Recorder<&Overlay<&SparseImage>>> = scratch
            .iter()
            .enumerate()
            .map(|(i, d)| Recorder::new(d, i, log.clone()))
            .collect();
        plan.apply::<_, &Recorder<&Overlay<&SparseImage>>>(&recorders.iter().collect::<Vec<_>>(), &[])
            .unwrap();
        let events = log.lock().unwrap().clone();
        let (mut states, mut apart, mut outvoted) = (0, 0, 0);
        for (cut, event) in events.iter().enumerate() {
            let DeviceEvent::Write { device, offset, data } = event else {
                continue;
            };
            let sectors = data.len() / 512;
            for (k, first) in (1..sectors).flat_map(|k| [(k, true), (k, false)]) {
                let replay: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
                for e in &events[..cut] {
                    if let DeviceEvent::Write { device, offset, data } = e {
                        replay[*device].write_all_at(data, *offset).unwrap();
                    }
                }
                let part = if first {
                    0..k * 512
                } else {
                    (sectors - k) * 512..data.len()
                };
                replay[*device]
                    .write_all_at(&data[part.clone()], offset + part.start as u64)
                    .unwrap();
                let pool = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
                states += 1;
                let torn_apart = pool
                    .issues
                    .iter()
                    .any(|i| matches!(i, Issue::TornCopy { differs: true, held, .. } if held.0 * 2 <= held.1));
                let refusal = pool.write_refusal(id).unwrap().unwrap_or_default();
                if torn_apart {
                    // Which copy is right is not known: nothing is written.
                    assert!(refusal.contains("not in a clean state"), "{name}: {refusal}");
                    apart += 1;
                    continue;
                }
                let s = pool.spaces.get(&id);
                assert!(
                    s.is_some_and(|s| (s.name() == name || s.name() == "renamed") && s.extents.len() == extents),
                    "{name}: write {cut} cut after {k} sectors ({}): sequence {}, {:?}",
                    if first { "the first" } else { "the last" },
                    pool.database.sequence,
                    pool.warnings
                );
                // Opening the space for writing brings stale copies, and a
                // torn one the others outvote, up to date.
                assert!(!refusal.contains("not in a clean state"), "{name}: {refusal}");
                if refusal.is_empty() {
                    outvoted += pool.issues.iter().any(|i| matches!(i, Issue::TornCopy { .. })) as usize;
                    drop(pool.open_space_rw(id).unwrap());
                    let again = Pool::open(replay.iter().collect::<Vec<_>>()).unwrap();
                    assert!(again.warnings.is_empty(), "{name}: {:?}", again.warnings);
                }
            }
        }
        assert!(
            outvoted > 0 || name != "mirror3",
            "{name}: no torn copy outvoted and written over"
        );
        assert!(states > 0, "{name}");
        // Only the pool of two copies has no majority while one is torn.
        assert_eq!(apart > 0, name == "simple2c", "{name}: {apart} of {states}");
    }
}

/// The copies of the pool database stay inside their members' pool
/// partitions: a partition table that makes the first member's pool
/// partition 8 KiB long (its database takes 12 KiB) gets no write past
/// it. 1.2.2 planned a rename that wrote the copy across the partition's
/// end (audit 2026-10-16, M20); since 1.3.1 the pool is refused first, its
/// disk record claiming more data than the partition holds.
#[test]
fn database_copies_stay_inside_their_partitions() {
    use storage_spaces::ops::{plan_delete_space, plan_rename_pool};
    use storage_spaces::plan::{Action, Target};
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "short", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    // The database grows past a page, then its spaces go again.
    for n in 0..24 {
        let pool = Pool::open(members()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(&format!("s{n}"), 1, 512, true), &mut new_guid).unwrap();
        drop(pool);
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    }
    for n in 0..24 {
        let pool = Pool::open(members()).unwrap();
        let plan = plan_delete_space(&pool, &format!("s{n}")).unwrap();
        drop(pool);
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    }
    // The pool partition's last sector (the second entry of the table at
    // LBA 2), 16 sectors after its first.
    let first = 32768u64;
    base[0]
        .write_all_at(&(first + 15).to_le_bytes(), 1024 + 128 + 40)
        .unwrap();
    let pool = Pool::open(members()).unwrap();
    let m = &pool.members[0];
    assert_eq!(m.partition.length, 8192);
    let end = m.partition.offset + m.partition.length;
    match plan_rename_pool(&pool, "renamed") {
        Err(e) => assert!(
            e.to_string().contains("does not fit") || e.to_string().contains("beyond its partition"),
            "{e}"
        ),
        Ok(plan) => {
            for a in plan.steps.iter().flat_map(|s| &s.actions) {
                if let Action::Write {
                    target: Target::Member(0),
                    offset,
                    bytes,
                } = a
                {
                    assert!(
                        offset + bytes.len() as u64 <= end,
                        "a write up to {:#x}, past {end:#x}",
                        offset + bytes.len() as u64
                    );
                }
            }
        }
    }
}

/// A space of local reconstruction codes (dual parity in local groups:
/// Windows' `lrc11`, 11 columns in 2 groups, 8 data columns of 1 GiB
/// units) grows by a row and opens, with its data as before. 1.2.2 counted
/// the columns less the redundancy (9): the new extents started where no
/// row does, and the space no longer opened (audit 2026-10-16, M20).
#[test]
fn a_space_of_local_groups_grows_and_opens() {
    use storage_spaces::ops::plan_resize_space;
    let images = fixture("lrc11");
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let pool = Pool::open(members()).unwrap();
    let id = pool.find_space("lrc11").unwrap().id();
    let r = pool.open_space(id).unwrap();
    assert_eq!(r.size(), 8 << 30);
    let read = |r: &storage_spaces::SpaceReader<'_, _>| {
        [0u64, 3 << 30, (8 << 30) - (1 << 20)].map(|at| {
            let mut b = vec![0u8; 1 << 20];
            r.read_exact_at(&mut b, at).unwrap();
            b
        })
    };
    let before = read(&r);
    drop(r);
    let plan = plan_resize_space(&pool, "lrc11", 16 << 30).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    let r = pool.open_space(id).unwrap();
    assert_eq!(r.size(), 16 << 30);
    assert_eq!(r.condition(), storage_spaces::Condition::Healthy);
    assert!(read(&r) == before);
    let mut grown = vec![1u8; 1 << 20];
    r.read_exact_at(&mut grown, 12 << 30).unwrap();
}

/// Every planner updates the database as readers read it: every slot its
/// header counts. In 17 of the fixtures (pools of tiers, dual parity,
/// local groups, several spaces) the second page of the pool database is
/// formatted only up to its last slot in use, the fixtures keeping what
/// the reader reads, and 1.2.2 read the database for an update up to its
/// last page formatted whole: the update left out the records of that
/// page, and spaces, their extents and disks vanished (found by
/// `a_space_of_local_groups_grows_and_opens`; the pools of the corpus have
/// whole pages). Renamed, every fixture that can be changed keeps them all.
#[test]
fn planners_keep_every_record_of_databases_formatted_in_part() {
    use storage_spaces::ops::plan_rename_pool;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let what = |pool: &Pool<&Overlay<&SparseImage>>| {
        let mut spaces: Vec<(u64, String, usize)> = pool
            .spaces
            .iter()
            .map(|(id, s)| (*id, s.name().to_string(), s.extents.len()))
            .collect();
        spaces.sort();
        (spaces, pool.disks.keys().copied().collect::<Vec<_>>())
    };
    let mut renamed = 0;
    for name in names {
        let images = fixture(&name);
        let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let members = || base.iter().collect::<Vec<_>>();
        let Ok(pool) = Pool::open(members()) else { continue };
        let Ok(plan) = plan_rename_pool(&pool, "renamed") else {
            continue;
        };
        let old = what(&pool);
        drop(pool);
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
        let pool = Pool::open(members()).unwrap();
        assert!(pool.warnings.is_empty(), "{name}: {:?}", pool.warnings);
        assert_eq!(pool.name, "renamed", "{name}");
        assert_eq!(what(&pool), old, "{name}");
        renamed += 1;
    }
    assert!(renamed >= 50, "{renamed} renamed");
}

/// Sizes are rounded up to whole rows without wrapping: a size near 2^64
/// once became a space of 0 bytes. Spaces hold at most 2^32 slabs (1 EiB),
/// on creation and when grown.
#[test]
fn huge_sizes_are_refused_not_wrapped() {
    use storage_spaces::ops::{MAX_SPACE_SIZE, plan_resize_space};
    let mut new_guid = guids();
    let (images, disks) = blank(2);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "huge", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    let pool = Pool::open(members()).unwrap();
    for size in [u64::MAX, u64::MAX - 1000, MAX_SPACE_SIZE + 1] {
        let mut s = spec("x", 1, 0, true);
        s.size = size;
        assert!(plan_create_space(&pool, &s, &mut new_guid).is_err(), "{size}");
    }
    let mut s = spec("x", 1, 0, true);
    s.size = MAX_SPACE_SIZE;
    let (_, space) = plan_create_space(&pool, &s, &mut new_guid).unwrap();
    assert_eq!(space.size, MAX_SPACE_SIZE);
    let (plan, _) = plan_create_space(&pool, &spec("x", 1, 1024, false), &mut new_guid).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    let pool = Pool::open(members()).unwrap();
    assert!(plan_resize_space(&pool, "x", u64::MAX).is_err());
}

/// A failed disk replaced: on three disks, a parity space of three columns
/// cannot be rebuilt without a new disk. The new disk is added to the pool
/// that misses one (data readable at every point a crash could stop it),
/// the repair rebuilds the lost copies on it, and the missing disk is
/// removed; the pool is clean again with the new disk.
#[test]
fn a_failed_disk_is_replaced() {
    use storage_spaces::ops::{plan_add_disk, plan_remove_disk, plan_repair};
    let mut new_guid = guids();
    let (images, disks) = blank(4);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks[..3], "replace", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base[..3].iter().collect::<Vec<_>>())
        .unwrap();
    let spaces = [("m", 2u8), ("p", 3)];
    for (name, tag) in spaces {
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, tag, 1024, false), &mut new_guid).unwrap();
        plan.apply::<_, &Overlay<&SparseImage>>(&base[..3].iter().collect::<Vec<_>>(), &[])
            .unwrap();
        drop(pool);
        let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space(name).unwrap().id()).unwrap();
        w.write_all_at(&data(tag, 8 << 20), 1 << 20).unwrap();
        w.flush().unwrap();
    }
    // Disk 2 fails; the repair has nowhere to rebuild the parity column.
    let pool = Pool::open(base[..3].iter().collect::<Vec<_>>()).unwrap();
    let lost = pool.disks.values().find(|d| d.member == Some(2)).unwrap().id;
    drop(pool);
    let left = [&base[0], &base[1]];
    let pool = Pool::open(left.to_vec()).unwrap();
    assert!(plan_repair(&pool).is_err());
    let plan = plan_add_disk(&pool, &disks[3], &mut new_guid).unwrap();
    drop(pool);
    // Every crash point of the addition.
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let scratch: Vec<Overlay<&Overlay<&SparseImage>>> = [&base[0], &base[1], &base[3]]
        .iter()
        .map(|d| Overlay::new(*d))
        .collect();
    let recorders: Vec<Recorder<&Overlay<&Overlay<&SparseImage>>>> = scratch
        .iter()
        .enumerate()
        .map(|(i, d)| Recorder::new(d, i, log.clone()))
        .collect();
    plan.apply(&[&recorders[0], &recorders[1]], &[&recorders[2]]).unwrap();
    let events = log.lock().unwrap().clone();
    for end in (0..=events.len()).filter(|&e| e == 0 || matches!(events[e - 1], DeviceEvent::Flush { .. })) {
        let replay: Vec<Overlay<&Overlay<&SparseImage>>> = [&base[0], &base[1], &base[3]]
            .iter()
            .map(|d| Overlay::new(*d))
            .collect();
        for e in &events[..end] {
            if let DeviceEvent::Write { device, offset, data } = e {
                replay[*device].write_all_at(data, *offset).unwrap();
            }
        }
        // The new disk takes part once its partition table is written.
        let pool = Pool::open(replay.iter().collect::<Vec<_>>())
            .or_else(|_| Pool::open(replay[..2].iter().collect::<Vec<_>>()))
            .unwrap();
        check_data(&pool, &spaces);
    }
    plan.apply(&left, &[&base[3]]).unwrap();
    let now = [&base[0], &base[1], &base[3]];
    let pool = Pool::open(now.to_vec()).unwrap();
    assert_eq!(pool.disks.len(), 4);
    assert!(
        pool.warnings.iter().all(|w| w.ends_with("is missing")),
        "{:?}",
        pool.warnings
    );
    check_data(&pool, &spaces);
    let plan = plan_repair(&pool).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&now, &[]).unwrap();
    let pool = Pool::open(now.to_vec()).unwrap();
    let plan = plan_remove_disk(&pool, lost).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&now, &[]).unwrap();
    let pool = Pool::open(now.to_vec()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    check_pool(&pool).unwrap();
    check_data(&pool, &spaces);
    let h = storage_spaces::health::health(&pool).unwrap();
    assert!(h.spaces.iter().all(|s| s.state.to_string() == "Healthy / OK"), "{h:?}");
}

/// Pools of version 29 (Insider build 26340): records are edited in their
/// own layout (a rename keeps the space record's version 17), but new
/// spaces, whose records and defaults differ there, are refused.
#[test]
fn version_29_pools_are_edited_but_get_no_new_spaces() {
    use storage_spaces::ops::plan_rename_space;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror2");
    let images: Vec<SparseImage> = (0..)
        .map_while(|i| std::fs::File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect();
    let devices: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
    assert_eq!(pool.version, 29);
    let mut new_guid = guids();
    let err = plan_create_space(&pool, &spec("new", 1, 1024, false), &mut new_guid).unwrap_err();
    assert!(err.to_string().contains("version 29"), "{err}");
    let plan = plan_rename_space(&pool, "mirror2", "renamed").unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&devices.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let pool = Pool::open(devices.iter().collect::<Vec<_>>()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    let space = pool.find_space("renamed").unwrap();
    assert_eq!(space.info.record_version, 17);
    check_pool(&pool).unwrap();
}

/// A pool whose records claim more of a disk than it has (found by the fuzz
/// target `manage`: the free slabs were counted by subtracting, which
/// overflowed, and could have placed a new extent over data). Planning on
/// it fails instead.
#[test]
fn extents_beyond_their_disk_stop_the_planners() {
    use storage_spaces::database::Database;
    use storage_spaces::format::assemble_records;
    use storage_spaces::records::DiskBody;
    let mut new_guid = guids();
    let (images, disks) = blank(2);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(&disks, "beyond", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    let pool = Pool::open(members()).unwrap();
    let (plan, _) = plan_create_space(&pool, &spec("s", 1, 4096, false), &mut new_guid).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    // Disk 1 now says it holds two slabs; its extents reach further.
    let pool = Pool::open(members()).unwrap();
    let at = pool.members[0].partition.offset + 0x1000;
    drop(pool);
    let db = Database::read_formatted(&base[0], at).unwrap();
    let record = assemble_records(db.bytes(), 0x40)
        .unwrap()
        .into_iter()
        .find(|r| r.kind == 2 && DiskBody::decode(&r.body).unwrap().id == 1)
        .unwrap();
    let mut disk = DiskBody::decode(&record.body).unwrap();
    disk.data_size = 2 << 28;
    disk.sequence = db.sequence() + 1;
    let body = disk.encode().unwrap();
    let (mut patched, _) = db.updated(&[(2, record.version, &body)], &[record.id]).unwrap();
    patched.commit(db.sequence() + 1, 1);
    for d in &base {
        d.write_all_at(patched.bytes(), at).unwrap();
    }
    let pool = Pool::open(members()).unwrap();
    let err = plan_create_space(&pool, &spec("t", 1, 1024, false), &mut new_guid).unwrap_err();
    assert!(err.to_string().contains("reach beyond"), "{err}");
}

/// Disks too small for a pool are refused as such, for a new pool and
/// when added: of no bytes, one sector, 16 MiB (the reserved partition),
/// 100, 600 and 785 MiB; 786 MiB holds the pool partition with its data
/// area and a slab. So is a disk of sectors of no bytes. 1.2.2 computed
/// the partitions with wrapping arithmetic (debug builds panicked) and
/// took a disk of no bytes for one of 16 EiB (audit 2026-10-16, writers).
#[test]
fn disks_too_small_for_a_pool_are_refused() {
    use storage_spaces::ops::plan_add_disk;
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "small", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base.iter().collect::<Vec<_>>())
        .unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let sized = |size: u64| BlankDisk {
        size,
        ..disks[0].clone()
    };
    for size in [0u64, 512, 16 << 20, 100 << 20, 600 << 20, 785 << 20] {
        let e = plan_create_pool(&[sized(size)], "tiny", None, &mut new_guid)
            .err()
            .unwrap_or_else(|| panic!("a pool on {size} bytes"))
            .to_string();
        assert!(e.contains("too small"), "{size}: {e}");
        let e = plan_add_disk(&pool, &sized(size), &mut new_guid)
            .err()
            .unwrap_or_else(|| panic!("{size} bytes added"))
            .to_string();
        assert!(e.contains("too small"), "{size}: {e}");
    }
    assert!(plan_create_pool(&[sized(786 << 20)], "tiny", None, &mut new_guid).is_ok());
    assert!(plan_add_disk(&pool, &sized(786 << 20), &mut new_guid).is_ok());
    let none = BlankDisk {
        logical_sector: 0,
        ..disks[0].clone()
    };
    assert!(plan_add_disk(&pool, &none, &mut new_guid).is_err());
}

/// Columns that overflow with their copies are refused for what they are:
/// 1.2.2 multiplied them unchecked (2^63 + 1 columns of two copies made
/// two, which three disks hold), and only the size of the rows refused the
/// space later (debug builds panicked; audit 2026-10-16, writers).
#[test]
fn columns_beyond_the_disks_are_refused_without_overflow() {
    let mut new_guid = guids();
    let (images, disks) = blank(3);
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let (plan, _) = plan_create_pool(&disks, "cols", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &base.iter().collect::<Vec<_>>())
        .unwrap();
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    for columns in [(1u64 << 63) + 1, u64::MAX, 4] {
        let mut s = spec("wide", 2, 1024, true);
        s.columns = Some(columns);
        let e = plan_create_space(&pool, &s, &mut new_guid).err().unwrap().to_string();
        assert!(e.contains(&format!("{columns} columns of 2 copies need")), "{e}");
    }
}

#[test]
fn a_pool_without_disks_or_a_name_is_refused() {
    let mut new_guid = guids();
    assert!(plan_create_pool(&[], "pool", None, &mut new_guid).is_err());
    let (_, disks) = blank(1);
    assert!(plan_create_pool(&disks, "", None, &mut new_guid).is_err());
}

/// The pool "names" with the thin simple spaces "a" and "b", made on
/// overlays of the blank disks `images`.
fn two_spaces<'i>(images: &'i [SparseImage], disks: &[BlankDisk]) -> Vec<Overlay<&'i SparseImage>> {
    let mut new_guid = guids();
    let base: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let members = || base.iter().collect::<Vec<_>>();
    let (plan, _) = plan_create_pool(disks, "names", None, &mut new_guid).unwrap();
    plan.apply::<&Overlay<&SparseImage>, _>(&[], &members()).unwrap();
    for name in ["a", "b"] {
        let pool = Pool::open(members()).unwrap();
        let (plan, _) = plan_create_space(&pool, &spec(name, 1, 1024, true), &mut new_guid).unwrap();
        drop(pool);
        plan.apply::<_, &Overlay<&SparseImage>>(&members(), &[]).unwrap();
    }
    let pool = Pool::open(members()).unwrap();
    assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    drop(pool);
    base
}

/// The planners choose a space as every command does: a key that names
/// two spaces changes neither. 1.2.2 took the first space named, numbered
/// or of that GUID, so that a space named like another's GUID was deleted
/// in its place (audit 2026-10-16, M12).
#[test]
fn a_key_that_names_two_spaces_changes_neither() {
    use storage_spaces::ops::{plan_delete_space, plan_rename_space, plan_resize_space};
    let (images, disks) = blank(2);
    let base = two_spaces(&images, &disks);
    let mut pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let (a, b) = (pool.find_space("a").unwrap().id(), pool.find_space("b").unwrap().id());
    assert!(a < b);
    let (guid_a, guid_b) = (
        pool.spaces[&a].info.guid.to_string(),
        pool.spaces[&b].info.guid.to_string(),
    );
    // "a" named like b's GUID: that GUID names both.
    pool.spaces.get_mut(&a).unwrap().info.name = guid_b.clone();
    let e = plan_delete_space(&pool, &guid_b).unwrap_err().to_string();
    assert!(e.contains(&guid_a) && e.contains(&guid_b), "{e}");
    assert!(plan_rename_space(&pool, &guid_b, "c").is_err());
    assert!(plan_resize_space(&pool, &guid_b, 2 << 30).is_err());
    // A name two spaces share, and a name that is the other's id.
    pool.spaces.get_mut(&a).unwrap().info.name = "b".into();
    assert!(plan_delete_space(&pool, "b").is_err());
    pool.spaces.get_mut(&a).unwrap().info.name = b.to_string();
    assert!(plan_delete_space(&pool, &b.to_string()).is_err());
    // By its GUID (in any case) the first is still deleted, and only it.
    let plan = plan_delete_space(&pool, &guid_a.to_uppercase()).unwrap();
    drop(pool);
    plan.apply::<_, &Overlay<&SparseImage>>(&base.iter().collect::<Vec<_>>(), &[])
        .unwrap();
    let after = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let left: Vec<String> = after.user_spaces().map(|s| s.info.guid.to_string()).collect();
    assert_eq!(left, [guid_b]);
}

/// A new name that names a space already (its name, its GUID, its numeric
/// id) is refused: commands choose spaces by each. 1.2.2 let a new space,
/// or a renamed one, take another's id for its name.
#[test]
fn a_new_name_that_names_a_space_is_refused() {
    use storage_spaces::ops::plan_rename_space;
    let (images, disks) = blank(2);
    let base = two_spaces(&images, &disks);
    let pool = Pool::open(base.iter().collect::<Vec<_>>()).unwrap();
    let b = pool.find_space("b").unwrap();
    let guid_b = b.info.guid.to_string();
    let mut new_guid = guids();
    for name in [b.id().to_string(), guid_b.clone(), guid_b.to_uppercase(), "b".into()] {
        let e = plan_create_space(&pool, &spec(&name, 1, 1024, true), &mut new_guid).unwrap_err();
        assert!(
            e.to_string().contains("exists") || e.to_string().contains(&guid_b),
            "{name}: {e}"
        );
        assert!(plan_rename_space(&pool, "a", &name).is_err(), "{name}");
    }
    assert!(plan_create_space(&pool, &spec("c", 1, 1024, true), &mut new_guid).is_ok());
    assert!(plan_rename_space(&pool, "a", "c").is_ok());
}
