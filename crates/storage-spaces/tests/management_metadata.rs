//! Management plans on pools whose metadata names places beyond where they
//! may write (audit 2026-10-21, SPW-01, SPW-02, and the auditor's spa_03,
//! spa_07): refused with an error, never a panic or a write out of place.
//! The pools are made by the planner on images in memory.

use storage_spaces::database::Database;
use storage_spaces::format::{DATA_AREA_OFFSET, POOL_DB_OFFSET, SLAB_SIZE, assemble_records};
use storage_spaces::io::{Overlay, SparseImage, WriteAt};
use storage_spaces::ops::{
    BlankDisk, SpaceSpec, check_pool, plan_add_disk, plan_create_pool, plan_create_space, plan_resize_space,
    plan_retire_disk,
};
use storage_spaces::plan::Plan;
use storage_spaces::records::SpaceBody;
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

fn blank(size: u64) -> BlankDisk {
    BlankDisk {
        size,
        logical_sector: 512,
        physical_sector: 4096,
        manufacturer: "Linux".into(),
        model: "test".into(),
    }
}

/// A pool of `n` disks of `size` bytes with a simple space "s" of one
/// column and `mib` MiB.
fn pool_with_space(n: usize, size: u64, mib: u64) -> Vec<Overlay<SparseImage>> {
    pool_with_columns(n, size, mib, 1)
}

/// The same with `columns` columns.
fn pool_with_columns(n: usize, size: u64, mib: u64, columns: u64) -> Vec<Overlay<SparseImage>> {
    let mut new_guid = guids();
    let disks: Vec<Overlay<SparseImage>> = (0..n).map(|_| Overlay::new(SparseImage::new(size))).collect();
    let blanks: Vec<BlankDisk> = (0..n).map(|_| blank(size)).collect();
    let (plan, _) = plan_create_pool(&blanks, "p", None, &mut new_guid).unwrap();
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(&none, &disks).unwrap();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let spec = SpaceSpec {
        name: "s".into(),
        resiliency: 1,
        size: mib << 20,
        thin: false,
        copies: None,
        columns: Some(columns),
        interleave: None,
        write_cache: None,
    };
    let (plan, _) = plan_create_space(&pool, &spec, &mut new_guid).unwrap();
    plan.apply(&disks, &none).unwrap();
    disks
}

/// A pool of `n` disks of `size` bytes without spaces.
fn empty_pool(n: usize, size: u64) -> Vec<Overlay<SparseImage>> {
    let disks: Vec<Overlay<SparseImage>> = (0..n).map(|_| Overlay::new(SparseImage::new(size))).collect();
    let blanks: Vec<BlankDisk> = (0..n).map(|_| blank(size)).collect();
    let (plan, _) = plan_create_pool(&blanks, "p", None, &mut guids()).unwrap();
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(&none, &disks).unwrap();
    disks
}

/// Writes `db`, committed as the next update, over every copy of the pool
/// database.
fn store(disks: &[Overlay<SparseImage>], mut db: Database) {
    let (sequence, time) = (db.sequence() + 1, db.timestamp() + 1);
    db.commit(sequence, time);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    for m in pool.members.iter().filter(|m| m.header.database_copy) {
        disks[m.device]
            .write_all_at(db.bytes(), m.partition.offset + POOL_DB_OFFSET)
            .unwrap();
    }
}

/// The pool database with user space "s" numbered `number`.
fn renumbered(disks: &[Overlay<SparseImage>], number: u64) -> Database {
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let db = check_pool(&pool).unwrap();
    let (id, version, mut body) = assemble_records(db.bytes(), 0x40)
        .unwrap()
        .into_iter()
        .find_map(|r| {
            let s = SpaceBody::decode(false, &r.body).ok().filter(|_| r.kind == 3)?;
            (s.role == 2).then_some((r.id, r.version, s))
        })
        .unwrap();
    body.number = number;
    let encoded = body.encode().unwrap();
    db.updated(&[(3, version, &encoded)], &[id]).unwrap().0
}

/// Whether a plan writes or copies only inside the partitions of `pool`'s
/// members.
fn inside(pool: &Pool<&Overlay<SparseImage>>, plan: &Plan) -> bool {
    use storage_spaces::plan::{Action, Target};
    let end = |t: &Target| match t {
        Target::Member(m) => Some(pool.members[*m].partition.offset + pool.members[*m].partition.length),
        _ => None,
    };
    plan.steps.iter().flat_map(|s| &s.actions).all(|a| match a {
        Action::Write { target, offset, bytes } => end(target).is_none_or(|e| offset + bytes.len() as u64 <= e),
        Action::Copy { to, to_offset, len, .. } => end(to).is_none_or(|e| to_offset + len <= e),
        _ => true,
    })
}

/// A user space numbered 2^42 or 70: its database would lie at 16 EiB or
/// 280 MiB into the metadata space of 256 MiB. Plans refuse the pool. 1.3.0
/// overflowed adding or retiring a disk (a panic), and with 70 wrote the
/// database past the slab (the auditor's repro_spw01 and spa_02, whose
/// record was another space's).
#[test]
fn a_space_number_beyond_the_metadata_space_is_refused() {
    for number in [1u64 << 42, 70] {
        let disks = pool_with_space(2, 4 << 30, 256);
        store(&disks, renumbered(&disks, number));
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let e = check_pool(&pool)
            .err()
            .unwrap_or_else(|| panic!("{number}: the pool passed"));
        assert!(e.to_string().contains("beyond the metadata space"), "{number}: {e}");
        let added = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            plan_add_disk(&pool, &blank(4 << 30), &mut guids()).map(|_| ())
        }));
        assert!(matches!(added, Ok(Err(_))), "{number}: adding a disk was not refused");
        let retired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plan_retire_disk(&pool, 2).map(|_| ())));
        assert!(
            matches!(retired, Ok(Err(_))),
            "{number}: retiring a disk was not refused"
        );
    }
}

/// Rewrites the GPT of `dev` with a pool partition of `length` bytes.
fn shorten_partition(dev: &Overlay<SparseImage>, length: u64) {
    let table = storage_spaces::gpt::PoolDiskTable {
        disk_size: (16 << 20) + length + (2 << 20),
        sector: 512,
        disk_guid: Guid([0x11; 16]),
        msr_guid: Guid([0x22; 16]),
        pool_partition_guid: Guid([0x33; 16]),
        pool_name: "p".into(),
    };
    for (offset, bytes) in table.regions().unwrap().into_iter().take(3) {
        dev.write_all_at(&bytes, offset).unwrap();
    }
}

/// A disk whose record claims more data than its partition holds (the
/// partition cut to two slabs): plans refuse the pool. 1.3.0 placed the
/// slabs of a retired disk's extent of 1 GiB there, copying 768 MiB past the
/// partition's end, and a resize allocated beyond it (the auditor's
/// repro_spw02).
#[test]
fn a_disk_claiming_more_than_its_partition_is_refused() {
    let disks = pool_with_space(2, 2 << 30, 1024);
    shorten_partition(&disks[1], DATA_AREA_OFFSET + 2 * SLAB_SIZE);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let Err(e) = check_pool(&pool) else {
        panic!("the pool passed")
    };
    assert!(e.to_string().contains("beyond its partition"), "{e}");
    for (what, plan) in [
        ("retire", plan_retire_disk(&pool, 1)),
        ("resize", plan_resize_space(&pool, "s", 2 << 30)),
    ] {
        match plan {
            Ok(plan) => panic!("{what}: planned, inside the partitions: {}", inside(&pool, &plan)),
            Err(e) => assert!(e.to_string().contains("beyond its partition"), "{what}: {e}"),
        }
    }
}

/// A pool of two SSD and two HDD disks of 8 GiB with tier templates "SSD"
/// (mirror) and "HDD" (simple), as `spaces tier create` makes them.
fn tiered_pool() -> Vec<Overlay<SparseImage>> {
    use storage_spaces::ops::{plan_create_tier, plan_set_disk};
    let disks = empty_pool(4, 8 << 30);
    let none: [Overlay<SparseImage>; 0] = [];
    for (id, media) in [(1, 2), (2, 2), (3, 1), (4, 1)] {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        plan_set_disk(&pool, id, Some(media), None)
            .unwrap()
            .apply(&disks, &none)
            .unwrap();
    }
    for (name, ssd, resiliency) in [("SSD", true, 2), ("HDD", false, 1)] {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        plan_create_tier(&pool, name, ssd, resiliency, None, &mut guids())
            .unwrap()
            .apply(&disks, &none)
            .unwrap();
    }
    disks
}

/// Tiered spaces of a template whose resiliency is none Windows has (0),
/// of one tier, and of two tiers larger than a space together: refused.
/// 1.3.0 indexed the resiliency's name by it (a panic), took `wanted[1]` of
/// one tier (a panic), and bounded each tier alone (the auditor's
/// repro_spw03).
#[test]
fn tiered_spaces_of_templates_or_sizes_no_space_has_are_refused() {
    use storage_spaces::limits::MAX_SPACE_SIZE;
    use storage_spaces::ops::plan_create_tiered_space;
    let disks = tiered_pool();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let both = [("SSD".to_string(), SLAB_SIZE), ("HDD".to_string(), SLAB_SIZE)];
    assert!(plan_create_tiered_space(&pool, "t", &both, &mut guids()).is_ok());
    let one = [("SSD".to_string(), SLAB_SIZE)];
    let huge = [
        ("SSD".to_string(), MAX_SPACE_SIZE / 2 + SLAB_SIZE),
        ("HDD".to_string(), MAX_SPACE_SIZE / 2 + SLAB_SIZE),
    ];
    for (what, tiers) in [("one tier", &one[..]), ("too large", &huge[..])] {
        let planned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            plan_create_tiered_space(&pool, "t", tiers, &mut guids()).map(|_| ())
        }));
        let Ok(Err(e)) = planned else {
            panic!("{what}: not refused (a panic, or a plan)")
        };
        assert!(!e.to_string().contains("no room"), "{what}: {e}");
    }
    // The SSD template of resiliency 0.
    let db = check_pool(&pool).unwrap();
    let (id, version, mut template) = assemble_records(db.bytes(), 0x40)
        .unwrap()
        .into_iter()
        .find_map(|r| {
            let t = SpaceBody::decode(true, &r.body).ok().filter(|_| r.kind == 6)?;
            (t.name == "SSD").then_some((r.id, r.version, t))
        })
        .unwrap();
    template.resiliency = 0;
    let body = template.encode().unwrap();
    let (next, _) = db.updated(&[(6, version, &body)], &[id]).unwrap();
    drop(pool);
    store(&disks, next);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let planned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        plan_create_tiered_space(&pool, "t", &both, &mut guids()).map(|_| ())
    }));
    let Ok(Err(e)) = planned else {
        panic!("resiliency 0: not refused (a panic, or a plan)")
    };
    assert!(e.to_string().contains("resiliency 0"), "{e}");
}

/// The pool database with the allocation unit of user space "s" set to
/// `unit`, or its size to `size`.
fn respaced(disks: &[Overlay<SparseImage>], unit: Option<u64>, size: Option<u64>) -> Database {
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let db = check_pool(&pool).unwrap();
    let (id, version, mut body) = assemble_records(db.bytes(), 0x40)
        .unwrap()
        .into_iter()
        .find_map(|r| {
            let s = SpaceBody::decode(false, &r.body).ok().filter(|_| r.kind == 3)?;
            (s.role == 2).then_some((r.id, r.version, s))
        })
        .unwrap();
    if let Some(unit) = unit {
        body.allocation_unit = unit;
    }
    if let Some(size) = size {
        body.size = size;
    }
    let encoded = body.encode().unwrap();
    db.updated(&[(3, version, &encoded)], &[id]).unwrap().0
}

/// Resizing a fixed space of two columns whose allocation unit is half a
/// slab (rows of one slab): refused. 1.3.0 wrote extents of no slabs, and
/// the space no longer opened (the auditor's repro_spw04, part A).
#[test]
fn a_resize_of_units_of_half_a_slab_is_refused() {
    let disks = pool_with_columns(2, 8 << 30, 512, 2);
    store(&disks, respaced(&disks, Some(SLAB_SIZE / 2), None));
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let Err(e) = plan_resize_space(&pool, "s", 4 << 30) else {
        panic!("planned")
    };
    assert!(e.to_string().contains("not whole slabs"), "{e}");
}

/// A fixed space of 1 GiB (one extent of four slabs) whose allocation unit
/// is set to twice that: its extents map half a group, and a resize is
/// refused. 1.3.0 counted the new groups from the size (group 0) and wrote
/// extents over the space's own: "overlapping extents" (the auditor's
/// repro_spw04, part B).
#[test]
fn a_resize_of_units_larger_than_the_extents_map_is_refused() {
    let disks = pool_with_space(2, 8 << 30, 256);
    let size = {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        pool.find_space("s").unwrap().info.size.unwrap()
    };
    store(&disks, respaced(&disks, Some(2 * size), None));
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let Err(e) = plan_resize_space(&pool, "s", 8 * size) else {
        panic!("planned")
    };
    assert!(e.to_string().contains("not whole groups"), "{e}");
}

/// A fixed space recorded as 812 MiB over extents that map 1 GiB (rows of a
/// slab), resized to 1 GiB: its extents already hold it, and the resize
/// changes the size alone. 1.3.0 counted the groups from the size (3) and
/// wrote a fourth extent over the space's own (the auditor's spa_03).
#[test]
fn a_resize_counts_the_groups_the_extents_map() {
    let disks = pool_with_space(1, 4 << 30, 1024);
    store(&disks, respaced(&disks, None, Some(3 * SLAB_SIZE + (44 << 20))));
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let before = pool.find_space("s").unwrap().extents.len();
    let none: [Overlay<SparseImage>; 0] = [];
    plan_resize_space(&pool, "s", 4 * SLAB_SIZE)
        .unwrap()
        .apply(&disks, &none)
        .unwrap();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let s = pool.find_space("s").unwrap();
    assert_eq!((s.info.size, s.extents.len()), (Some(4 * SLAB_SIZE), before));
    assert!(pool.open_space(s.id()).is_ok());
}

/// Removing a disk from a pool whose metadata space records no copies:
/// an error. 1.3.0 underflowed (a panic; the auditor's repro_spw04).
#[test]
fn a_metadata_space_of_no_copies_loses_no_disk() {
    let disks = empty_pool(2, 4 << 30);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let db = check_pool(&pool).unwrap();
    let (id, version, mut meta) = assemble_records(db.bytes(), 0x40)
        .unwrap()
        .into_iter()
        .find_map(|r| {
            let s = SpaceBody::decode(false, &r.body).ok().filter(|_| r.kind == 3)?;
            (s.internal == 1 && s.role == 1).then_some((r.id, r.version, s))
        })
        .unwrap();
    meta.copies = 0;
    let body = meta.encode().unwrap();
    let (next, _) = db.updated(&[(3, version, &body)], &[id]).unwrap();
    let removed = std::panic::catch_unwind(|| storage_spaces::manage::remove_disk_updates(&next, 2, 1).map(|_| ()));
    let Ok(Err(e)) = removed else {
        panic!("not refused (a panic, or updates)")
    };
    assert!(e.to_string().contains("no copies"), "{e}");
}

/// A thin space of one column on two disks of five slabs each: disk 1 holds
/// rows 0 and 6 (slabs 1 and 2), disk 2 rows 7 to 9 (slabs 2 to 4). A new
/// row goes to disk 1, which has two free slabs, not disk 2 with one, whose
/// first free slab comes first; and once a write the pool's being full cuts
/// short has written rows, the writer refuses the next one. 1.3.0 picked
/// disk 2 and wrote on after the cut write (the auditor's repro_spw06).
#[test]
fn a_thin_space_takes_the_disk_with_most_free_slabs_and_stops_after_a_cut_write() {
    use storage_spaces::format::ExtentRecord;
    let disks = empty_pool(2, 2 << 30);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let spec = SpaceSpec {
        name: "thin".into(),
        resiliency: 1,
        size: 10 * SLAB_SIZE,
        thin: true,
        copies: None,
        columns: Some(1),
        interleave: None,
        write_cache: None,
    };
    let (plan, _) = plan_create_space(&pool, &spec, &mut guids()).unwrap();
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(&disks, &none).unwrap();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("thin").unwrap().id();
    let w = pool.open_space_rw(id).unwrap();
    w.write_all_at(&[0x11; 512], 0).unwrap();
    w.flush().unwrap();
    drop(w);
    let db = check_pool(&pool).unwrap();
    let row0 = db
        .extents()
        .unwrap()
        .into_iter()
        .find(|(_, e)| e.space_id == id)
        .unwrap()
        .1;
    assert_eq!((row0.disk_id, row0.physical_slab), (1, 1));
    let bodies: Vec<Vec<u8>> = [(6, 1, 2), (7, 2, 2), (8, 2, 3), (9, 2, 4)]
        .iter()
        .map(|&(virtual_slab, disk_id, physical_slab)| {
            ExtentRecord {
                virtual_slab,
                disk_id,
                physical_slab,
                ..row0
            }
            .encode(db.sequence() + 1)
        })
        .collect();
    let writes: Vec<(u8, u8, &[u8])> = bodies.iter().map(|b| (4u8, 6u8, b.as_slice())).collect();
    let (next, _) = db.updated(&writes, &[]).unwrap();
    drop(pool);
    store(&disks, next);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let w = pool.open_space_rw(id).unwrap();
    w.write_all_at(&[0x55; 512], SLAB_SIZE).unwrap();
    let db = check_pool(&pool).unwrap();
    let row1 = db
        .extents()
        .unwrap()
        .into_iter()
        .find(|(_, e)| e.space_id == id && e.virtual_slab == 1)
        .unwrap()
        .1;
    assert_eq!(row1.disk_id, 1, "row 1 went to disk {}", row1.disk_id);
    // Rows 2 and 3 take the last free slabs; row 4 finds none.
    let cut = w.write_all_at(&vec![0xaa; (2 * SLAB_SIZE + 4096) as usize], 2 * SLAB_SIZE);
    assert!(cut.is_err(), "the pool had room");
    let after = w.write_all_at(&[0x77; 512], 0);
    assert!(after.is_err(), "written after a write cut short");
}

/// A database whose header counts slots 0 to 8 and whose slot 9 holds the
/// start of a record a crash cut short ("SDBB", slot number 999): read for
/// an update, the slot is formatted free whole, and the updated database
/// reads back. 1.3.0 freed the slot but kept its number, and the database
/// the update wrote no longer read (the auditor's spa_07).
#[test]
fn a_cut_record_past_the_counted_slots_is_formatted_free() {
    use storage_spaces::format::{ExtentRecord, read_database};
    let mut db = Database::new(Guid([1; 16]), 64);
    let extent = ExtentRecord {
        flags: 4,
        stale_marker: ExtentRecord::CURRENT,
        space_id: 1,
        virtual_slab: 0,
        column: 0,
        copy: 0,
        slab_count: 1,
        disk_id: 1,
        physical_slab: 0,
    };
    let body = extent.encode(1);
    db.insert(4, 6, &body).unwrap();
    db.commit(1, 100);
    let mut raw = vec![0u8; 8192];
    raw[..4096].copy_from_slice(db.bytes());
    // Slot 8's record claims slot 9 as well, so that an update leaves it.
    raw[8 * 64 + 8..8 * 64 + 12].copy_from_slice(&9u32.to_be_bytes());
    let slot9 = &mut raw[9 * 64..10 * 64];
    slot9[0..4].copy_from_slice(b"SDBB");
    slot9[4..8].copy_from_slice(&999u32.to_be_bytes());
    slot9[8..12].copy_from_slice(&55u32.to_be_bytes());
    let mut dev = SparseImage::new(1 << 20);
    dev.insert(0, &raw);
    let formatted = Database::read_formatted(&dev, 0).unwrap();
    let (mut updated, _) = formatted.updated(&[(4, 6, &body)], &[]).unwrap();
    updated.commit(2, 200);
    let mut back = SparseImage::new(1 << 20);
    back.insert(0, updated.bytes());
    read_database(&back, 0).unwrap().expect("no database");
}
