//! Spaces whose structure is not what Windows leaves (a damaged pool
//! database, or a hostile one): not called healthy, and not written where
//! writing would make the database worse (audit 2026-10-21, SPA-01). The
//! pools are made by the planner on images in memory.
use storage_spaces::database::Database;
use storage_spaces::format::{ExtentRecord, POOL_DB_OFFSET, Policy, Resiliency, SLAB_SIZE};
use storage_spaces::guard::{self, Options};
use storage_spaces::health::{HealthStatus, health};
use storage_spaces::io::{Overlay, SparseImage, WriteAt};
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
