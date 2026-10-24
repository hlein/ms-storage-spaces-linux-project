//! Spaces whose structure is not what Windows leaves (a damaged pool
//! database, or a hostile one): not called healthy, and not written where
//! writing would make the database worse (audit 2026-10-21, SPA-01). The
//! pools are made by the planner on images in memory.
use std::fs::File;
use std::path::Path;

use storage_spaces::cache::{CacheHeader, SPCACHE_SIGNATURE};
use storage_spaces::database::Database;
use storage_spaces::format::{ExtentRecord, POOL_DB_OFFSET, Policy, Resiliency, SLAB_SIZE, SpaceRole};
use storage_spaces::guard::{self, Options};
use storage_spaces::health::{HealthStatus, health};
use storage_spaces::io::{Overlay, SparseImage, WriteAt};
use storage_spaces::journal::SPVDT_SIGNATURE;
use storage_spaces::layout::{Condition, Layout};
use storage_spaces::ops::{BlankDisk, SpaceSpec, check_pool, plan_create_pool, plan_create_space};
use storage_spaces::report::Verdict;
use storage_spaces::{Guid, Pool};

fn guids(seed: u8) -> impl FnMut() -> Guid {
    let mut n = 0u64;
    move || {
        n += 1;
        let mut b = [seed; 16];
        b[..8].copy_from_slice(&n.to_le_bytes());
        Guid(b)
    }
}

/// A new pool of `disks` blank disks of `size` bytes, in memory.
fn new_pool(disks: usize, size: u64) -> Vec<Overlay<SparseImage>> {
    let blanks: Vec<BlankDisk> = (0..disks)
        .map(|i| BlankDisk {
            size,
            logical_sector: 512,
            physical_sector: 4096,
            manufacturer: "Test".into(),
            model: format!("Disk{i}"),
        })
        .collect();
    let (plan, _) = plan_create_pool(&blanks, "pool", Some(512), &mut guids(0x10)).unwrap();
    let images: Vec<Overlay<SparseImage>> = (0..disks).map(|_| Overlay::new(SparseImage::new(size))).collect();
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(&none, &images).unwrap();
    images
}

/// Writes `db` over every copy of the pool database.
fn store_database(disks: &[Overlay<SparseImage>], db: &Database) {
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    for m in pool.members.iter().filter(|m| m.header.database_copy) {
        disks[m.device]
            .write_all_at(db.bytes(), m.partition.offset + POOL_DB_OFFSET)
            .unwrap();
    }
}

/// A thin simple space of two columns whose first row lost the extent
/// record of column 1 (Windows allocates a row's columns together: a
/// damaged database). The row is lost: not healthy for the condition, the
/// health or the guard, and not written. 1.3.0 called it healthy, and a
/// write into the row allocated it again, writing the pool database with
/// two extents for column 0 to every disk: the pool did not open any more
/// (the auditor's spa_01a).
#[test]
fn a_thin_row_missing_a_column_is_failed_and_not_written() {
    let disks = new_pool(2, 4 << 30);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let spec = SpaceSpec {
        name: "thin2c".into(),
        resiliency: 1,
        size: 2 * SLAB_SIZE,
        thin: true,
        copies: Some(1),
        columns: Some(2),
        interleave: None,
        write_cache: None,
    };
    let (plan, _) = plan_create_space(&pool, &spec, &mut guids(0x20)).unwrap();
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(&disks, &none).unwrap();
    {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let w = pool.open_space_rw(pool.find_space("thin2c").unwrap().id()).unwrap();
        w.write_all_at(&[0xaa; 4096], 0).unwrap();
        w.flush().unwrap();
    }
    {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let db = check_pool(&pool).unwrap();
        let column1 = db
            .extents()
            .unwrap()
            .into_iter()
            .find(|(_, e)| e.column == 1)
            .unwrap()
            .0;
        let (mut next, _) = db.updated(&[], &[column1]).unwrap();
        next.commit(db.sequence() + 1, db.timestamp() + 1);
        store_database(&disks, &next);
    }
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let space = pool.find_space("thin2c").unwrap();
    let reader = pool.open_space(space.id()).unwrap();
    assert_eq!(reader.condition(), Condition::Failed);
    let h = health(&pool).unwrap();
    assert_eq!(h.spaces[0].state.health, HealthStatus::Unhealthy);
    let report = guard::space_report(&pool, space, &[], &Options::default(), &mut |_, _, _| Vec::new());
    assert_eq!(report.verdict(), Verdict::Failed, "{}", report.to_text());
    assert!(pool.write_refusal(space.id()).unwrap().is_some());
    assert!(pool.open_space_rw(space.id()).is_err(), "opened for writing");
    // The database is as it was: the space still opens.
    let again = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    assert!(again.open_space(space.id()).is_ok());
}

/// A row of a two-way mirror with one current copy: the other being rebuilt
/// (flag 0x01), or its record missing. One disk failure loses it: degraded,
/// not healthy, short of the copies its policy keeps. 1.3.0's health called
/// both healthy, surviving no more failure, and its condition the second
/// (the auditor's spa_01b). A copy rebuilt beside two current ones leaves
/// the row its copies (Windows calls `repairint`, detached during its
/// repair, healthy).
#[test]
fn a_mirror_row_short_of_a_copy_is_degraded() {
    let policy = Policy {
        resiliency: Resiliency::Mirror,
        redundancy: 1,
        copies: 2,
        columns: 1,
        interleave: 256 << 10,
        groups: 1,
    };
    let copy0 = ExtentRecord {
        flags: 4,
        stale_marker: ExtentRecord::CURRENT,
        space_id: 10,
        virtual_slab: 0,
        column: 0,
        copy: 0,
        slab_count: 1,
        disk_id: 1,
        physical_slab: 1,
    };
    let rebuilt = ExtentRecord {
        flags: 4 | ExtentRecord::FLAG_REGENERATING,
        copy: 1,
        disk_id: 2,
        ..copy0
    };
    for (what, extents) in [("rebuilt", vec![copy0, rebuilt]), ("missing", vec![copy0])] {
        let l = Layout::new(&policy, &extents).unwrap();
        assert_eq!(l.condition(|_| true), Condition::Degraded, "{what}");
        let r = l.redundancy(|_| true, |_| false);
        assert_eq!((r.left, r.short), (Some(0), true), "{what}");
    }
    let third = ExtentRecord {
        copy: 2,
        disk_id: 3,
        ..rebuilt
    };
    let copy1 = ExtentRecord {
        copy: 1,
        disk_id: 2,
        ..copy0
    };
    let l = Layout::new(&policy, &[copy0, copy1, third]).unwrap();
    let r = l.redundancy(|_| true, |_| false);
    assert_eq!((r.left, r.short), (Some(1), false));
}

fn images(name: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

/// A write-back cache whose header names its chunks where its space has no
/// extent (at 512 GiB in a cache of 1 GiB) or over its own slot area: the
/// space is not read or written, and the guard fails it. 1.3.0 called the
/// first healthy and read the chunks the cache holds as zeros (the
/// auditor's spb_02).
#[test]
fn a_cache_header_naming_regions_its_space_does_not_hold_fails() {
    let images = images("parity3_26100");
    for (what, data_offset) in [("unmapped", 512u64 << 30), ("over the slots", 0x2000)] {
        let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        {
            let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
            let id = pool.find_space("parity3_26100").unwrap().id();
            let w = pool.open_space_rw(id).unwrap();
            w.write_all_at(&[0xa5; 4096], 0).unwrap();
            w.flush().unwrap();
            drop(w);
            let mut header = pool.open_space(id).unwrap().cache().unwrap().header.clone();
            header.data_offset = data_offset;
            let bytes = header.encode(SPCACHE_SIGNATURE);
            let container = pool.children(id).find(|c| c.info.role == SpaceRole::Cache).unwrap();
            let child = pool.children(container.id()).next().unwrap();
            let l = Layout::new(child.info.policy.as_ref().unwrap(), &child.extents).unwrap();
            for copy in l.copies_of(0) {
                let (disk, slab) = l.physical(0, copy, 0).unwrap();
                let (device, at) = pool.slab_location(disk, slab).unwrap().unwrap();
                disks[device].write_all_at(&bytes, at).unwrap();
            }
        }
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let space = pool.find_space("parity3_26100").unwrap();
        let Err(e) = pool.open_space(space.id()) else {
            panic!("{what}: the space opened")
        };
        assert!(e.to_string().contains("of the write-back cache"), "{what}: {e}");
        let report = guard::space_report(&pool, space, &[], &Options::default(), &mut |_, _, _| Vec::new());
        assert_eq!(report.verdict(), Verdict::Failed, "{what}: {}", report.to_text());
        assert!(pool.open_space_rw(space.id()).is_err(), "{what}: opened for writing");
    }
}

/// A parity journal whose header puts its slot area beyond its space (at
/// 512 GiB in a journal of 256 MiB) or its checkpoint areas over the slots:
/// the space is not read, and the guard fails it. 1.3.0 read the slots there
/// as zeros, an empty journal, and called the space healthy.
#[test]
fn a_journal_header_naming_regions_its_space_does_not_hold_fails() {
    let images = images("parity3_26100");
    for what in ["slots beyond", "checkpoints over the slots"] {
        let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        {
            let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
            let id = pool.find_space("parity3_26100").unwrap().id();
            let container = pool
                .children(id)
                .find(|c| c.info.role == SpaceRole::Other(0x0a))
                .unwrap();
            let child = pool.children(container.id()).next().unwrap();
            let mut b = [0u8; CacheHeader::SIZE];
            pool.open_space(child.id()).unwrap().read_exact_at(&mut b, 0).unwrap();
            let (u64_at, u32_at) = (
                |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap()),
                |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap()),
            );
            let mut header = CacheHeader {
                owner_guid: Guid::from_mixed_endian(b[8..24].try_into().unwrap()),
                sequence: u64_at(0x28),
                slot_offset: u64_at(0x30),
                slot_size: u32_at(0x38),
                slot_count: u32_at(0x3c),
                checkpoint_offset: u64_at(0x40),
                checkpoint_size: u32_at(0x48),
                checkpoint_count: u32_at(0x4c),
                data_offset: u64_at(0x50),
                chunk_size: u32_at(0x58),
                chunk_count: u32_at(0x5c),
            };
            match what {
                "slots beyond" => header.slot_offset = 512 << 30,
                _ => header.checkpoint_offset = 0x3000,
            }
            let bytes = header.encode(SPVDT_SIGNATURE);
            let l = Layout::new(child.info.policy.as_ref().unwrap(), &child.extents).unwrap();
            for copy in l.copies_of(0) {
                let (disk, slab) = l.physical(0, copy, 0).unwrap();
                let (device, at) = pool.slab_location(disk, slab).unwrap().unwrap();
                disks[device].write_all_at(&bytes, at).unwrap();
            }
        }
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let space = pool.find_space("parity3_26100").unwrap();
        let Err(e) = pool.open_space(space.id()) else {
            panic!("{what}: the space opened")
        };
        assert!(e.to_string().contains("of the parity journal"), "{what}: {e}");
        let report = guard::space_report(&pool, space, &[], &Options::default(), &mut |_, _, _| Vec::new());
        assert_eq!(report.verdict(), Verdict::Failed, "{what}: {}", report.to_text());
    }
}

/// A fixed tiered space one of whose tiers lost all its extent records, or
/// whose tiers end before the space does: its state is not known to
/// health(), the guard fails it and it is not read. 1.3.0 called both
/// healthy and read the rest as zeros (the auditor's spg02).
#[test]
fn a_tiered_space_whose_tiers_do_not_hold_it_fails() {
    for what in ["a tier without extents", "tiers short of the space"] {
        let mut pool = Pool::open(images("tiered")).unwrap();
        let id = pool.user_spaces().next().unwrap().id();
        let hdd = pool
            .children(id)
            .find(|c| c.info.is_child && c.info.range.is_some_and(|(start, _)| start > 0))
            .unwrap()
            .id();
        match what {
            "a tier without extents" => pool.spaces.get_mut(&hdd).unwrap().extents.clear(),
            _ => pool.spaces.get_mut(&id).unwrap().info.size = Some(3 << 30),
        }
        let h = health(&pool).unwrap();
        assert!(h.spaces.iter().all(|s| s.id != id), "{what}: {:?}", h.spaces);
        assert!(h.unknown.iter().any(|(s, _)| *s == id), "{what}: {:?}", h.unknown);
        let space = &pool.spaces[&id];
        let report = guard::space_report(&pool, space, &[], &Options::default(), &mut |_, _, _| Vec::new());
        assert_eq!(report.verdict(), Verdict::Failed, "{what}: {}", report.to_text());
        assert!(pool.open_space(id).is_err(), "{what}: the space opened");
    }
}

/// A tiered space whose first tier is one slab and whose second has two
/// columns: the second tier's rows count from its range's start, so the
/// space is healthy, as the reader takes it. 1.3.0's health() laid the tier
/// out from 0 and found its extent "not aligned to a row" (the auditor's
/// spa_05).
#[test]
fn a_tier_is_laid_out_from_its_own_start_by_health() {
    let mut pool = Pool::open(images("tiered")).unwrap();
    let id = pool.user_spaces().next().unwrap().id();
    let tiers: Vec<u64> = pool
        .children(id)
        .filter(|c| c.info.is_child && !c.extents.is_empty())
        .map(|c| c.id())
        .collect();
    assert_eq!(tiers.len(), 2);
    let t0 = pool.spaces.get_mut(&tiers[0]).unwrap();
    let mut policy = t0.info.policy.unwrap();
    policy.columns = 1;
    t0.info.policy = Some(policy);
    t0.info.range = Some((0, SLAB_SIZE));
    t0.extents.retain(|e| e.column == 0 && e.virtual_slab == 0);
    for e in &mut t0.extents {
        e.slab_count = 1;
    }
    let t1 = pool.spaces.get_mut(&tiers[1]).unwrap();
    let mut policy = t1.info.policy.unwrap();
    policy.columns = 2;
    (policy.resiliency, policy.redundancy, policy.copies) = (Resiliency::Simple, 0, 1);
    t1.info.policy = Some(policy);
    t1.info.range = Some((SLAB_SIZE, 2 * SLAB_SIZE));
    let column = |column: u64, e: &ExtentRecord| ExtentRecord {
        flags: 4,
        stale_marker: ExtentRecord::CURRENT,
        space_id: tiers[1],
        virtual_slab: 1,
        column,
        copy: 0,
        slab_count: 1,
        ..*e
    };
    t1.extents = vec![column(0, &t1.extents[0]), column(1, &t1.extents[1])];
    pool.spaces.get_mut(&id).unwrap().info.size = Some(3 * SLAB_SIZE);
    assert!(pool.open_space(id).is_ok());
    let h = health(&pool).unwrap();
    assert!(h.unknown.is_empty(), "{:?}", h.unknown);
    assert_eq!(h.spaces[0].state.to_string(), "Healthy / OK");
}
