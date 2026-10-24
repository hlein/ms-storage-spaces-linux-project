//! Metadata Windows does not write, met where 1.3.0 went quietly wrong
//! (audit 2026-10-21, SPB-04, SPB-06 and the auditor's spa_06, spa_08,
//! spg05, spg06, scb_06): local groups beyond GF(16), a primary GPT that
//! does not check out, MBR slots, an NTFS volume of no sectors, spaces that
//! share a GUID or hold a NUL in their name, reads past a space's end.

use storage_spaces::format::{ExtentRecord, POOL_DB_OFFSET, Policy, Resiliency};
use storage_spaces::gpt::{find_spaces_partition, read_gpt};
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{MemDevice, Overlay, ReadAt, SparseImage, WriteAt};
use storage_spaces::layout::Layout;
use storage_spaces::ops::{BlankDisk, SpaceSpec, check_pool, plan_create_pool, plan_create_space};
use storage_spaces::report::{Status, Verdict};
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

/// A pool of one blank disk of `size` bytes, in memory.
fn new_pool(size: u64) -> Vec<Overlay<SparseImage>> {
    let blank = BlankDisk {
        size,
        logical_sector: 512,
        physical_sector: 4096,
        manufacturer: "Test".into(),
        model: "Disk".into(),
    };
    let (plan, _) = plan_create_pool(&[blank], "pool", Some(512), &mut guids(0x10)).unwrap();
    let disks = vec![Overlay::new(SparseImage::new(size))];
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(&none, &disks).unwrap();
    disks
}

/// Adds a thin simple space `name` of 256 MiB with the GUIDs of `seed`.
fn add_space(disks: &[Overlay<SparseImage>], name: &str, seed: u8) {
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let spec = SpaceSpec {
        name: name.into(),
        resiliency: 1,
        size: 256 << 20,
        thin: true,
        copies: Some(1),
        columns: Some(1),
        interleave: None,
        write_cache: None,
    };
    let (plan, _) = plan_create_space(&pool, &spec, &mut guids(seed)).unwrap();
    let none: [Overlay<SparseImage>; 0] = [];
    plan.apply(disks, &none).unwrap();
}

/// A local reconstruction code of 35 columns in 2 groups: 32 data columns,
/// 16 a group. GF(16) has 15 coefficients besides 0, so the global parity
/// row of such groups is not known (as for dual parity of more columns) and
/// reconstruction from it is refused; groups of 15 have one. 1.3.0 used
/// 16, which multiplies by 0, and rebuilt wrong data (the auditor's
/// spb_04).
#[test]
fn local_groups_beyond_gf16_have_no_global_row() {
    for (columns, known) in [(35, false), (33, true)] {
        let policy = Policy {
            resiliency: Resiliency::Parity,
            columns,
            copies: 1,
            redundancy: 2,
            groups: 2,
            interleave: 4096,
        };
        let extents: Vec<ExtentRecord> = (0..columns)
            .map(|column| ExtentRecord {
                space_id: 1,
                virtual_slab: 0,
                column,
                copy: 0,
                slab_count: 1,
                disk_id: column + 1,
                physical_slab: 0,
                flags: 4,
                stale_marker: ExtentRecord::CURRENT,
            })
            .collect();
        let code = Layout::new(&policy, &extents).unwrap().parity_code();
        assert_eq!(code.last().unwrap().is_some(), known, "{columns} columns: {code:?}");
        if let Some(Some(global)) = code.last() {
            assert!(global.iter().all(|&c| (1..=15).contains(&c)), "{global:?}");
        }
    }
}

/// The pool disk of a pool of one disk whose primary GPT header is lost
/// (zeroed): its pool partition is found from the backup at the last LBA,
/// and the pool opens. 1.3.0 found no partition (the auditor's spa_06,
/// part 1).
#[test]
fn a_member_whose_primary_gpt_header_is_lost_is_found_by_its_backup() {
    let disks = new_pool(1 << 30);
    disks[0].write_all_at(&[0u8; 512], 512).unwrap();
    let parts = read_gpt(&disks[0], 512).unwrap().expect("no GPT");
    assert_eq!(parts.len(), 2, "{parts:?}");
    assert!(find_spaces_partition(&disks[0]).unwrap().is_some());
    assert!(Pool::open(disks.iter().collect::<Vec<_>>()).is_ok());
}

/// A primary GPT header still saying "EFI PART" whose entry count, entries
/// or header bytes were damaged (its CRC32s then fail), the backup sound:
/// the guard reads the partitions from the backup (suspect). 1.3.0 took the
/// primary's entries, none at all (the auditor's spa_06 part 2 and spg06).
#[test]
fn a_primary_gpt_that_does_not_check_out_gives_way_to_the_backup() {
    let disks = new_pool(1 << 30);
    let size = disks[0].size().unwrap();
    assert_eq!(guard::partitions(&disks[0], size, 512).len(), 2);
    for what in ["no entries counted", "entries zeroed", "header bytes"] {
        let disk = Overlay::new(&disks[0]);
        match what {
            "no entries counted" => disk.write_all_at(&0u32.to_le_bytes(), 512 + 80).unwrap(),
            "entries zeroed" => disk.write_all_at(&[0u8; 4096], 1024).unwrap(),
            _ => disk.write_all_at(&[0xff; 84], 512 + 8).unwrap(),
        }
        let parts = guard::partitions(&disk, size, 512);
        assert_eq!(parts.len(), 2, "{what}: {parts:?}");
        assert!(read_gpt(&disk, 512).unwrap().is_some_and(|p| p.len() == 2), "{what}");
    }
}

/// An MBR whose first slot is empty and whose second holds a partition: it
/// is partition 2, as the kernel numbers it. 1.3.0 called it 1 (the
/// auditor's spa_06, part 3).
#[test]
fn mbr_partitions_are_numbered_by_their_slot() {
    let mut mbr = vec![0u8; 4096];
    mbr[462 + 4] = 0x07;
    mbr[462 + 8..462 + 12].copy_from_slice(&2u32.to_le_bytes());
    mbr[462 + 12..462 + 16].copy_from_slice(&4u32.to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    let parts = guard::partitions(&MemDevice(mbr), 4096, 512);
    assert_eq!(parts.iter().map(|p| p.number).collect::<Vec<_>>(), [2]);
}

/// An NTFS boot sector naming a volume of no sectors: not a sound volume.
/// 1.3.0 looked for the boot sector's copy after the volume's last sector,
/// that is at the boot sector itself, and called it sound (the auditor's
/// spg05).
#[test]
fn an_ntfs_volume_of_no_sectors_is_not_sound() {
    let mut part = vec![0u8; 2 * 4096];
    part[3..11].copy_from_slice(b"NTFS    ");
    part[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
    part[510..512].copy_from_slice(&[0x55, 0xaa]);
    let check = guard::ntfs_check(&MemDevice(part), 0, 2 * 4096, "p1", &|o| format!("{o:#x}")).unwrap();
    assert_ne!(check.status, Status::Ok, "{check:?}");
}

/// Two spaces of one GUID (here the second planned with the first one's
/// GUIDs): a pool issue, so that the pool is not clean. Devices, units and
/// `--space` name a space by its GUID; 1.3.0 let them pass (the auditor's
/// spa_08).
#[test]
fn two_spaces_of_one_guid_are_a_pool_issue() {
    let disks = new_pool(4 << 30);
    add_space(&disks, "s1", 0x60);
    add_space(&disks, "s2", 0x60);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let spaces: Vec<_> = pool.user_spaces().collect();
    assert_eq!(spaces[0].info.guid, spaces[1].info.guid);
    assert!(pool.warnings.iter().any(|w| w.contains("GUID")), "{:?}", pool.warnings);
}

/// A space whose name holds a NUL: a pool issue, so that the guard does not
/// call the space healthy and attach does not reach a command line with
/// it (`nbd-client -N` failed to start). 1.3.0 called it healthy (the
/// auditor's scb_06).
#[test]
fn a_name_holding_a_nul_is_a_pool_issue() {
    let disks = new_pool(4 << 30);
    add_space(&disks, "mirror", 0x60);
    let needle: Vec<u8> = "mirror".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let at = pool.members[0].partition.offset + POOL_DB_OFFSET;
    drop(pool);
    let mut db = vec![0u8; 1 << 20];
    disks[0].read_exact_at(&mut db, at).unwrap();
    let pos = db.windows(needle.len()).position(|w| w == needle).unwrap();
    disks[0].write_all_at(&[0, 0], at + pos as u64 + 6).unwrap();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let space = pool.user_spaces().next().unwrap();
    assert_eq!(space.name(), "mir\0or");
    assert!(pool.warnings.iter().any(|w| w.contains("NUL")), "{:?}", pool.warnings);
    assert!(check_pool(&pool).is_err());
    let report = guard::space_report(&pool, space, &[], &Options::default(), &mut |_, _, _| Vec::new());
    assert_ne!(report.verdict(), Verdict::Healthy, "{}", report.to_text());
}

/// Reading a space's own extents past its end, or across 2^64: an error,
/// as for `read_exact_at`. 1.3.0 read zeros past the end and overflowed
/// near 2^64 (the auditor's spb_08).
#[test]
fn reads_of_the_extents_alone_stay_inside_the_space() {
    let disks = new_pool(4 << 30);
    add_space(&disks, "s", 0x60);
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let reader = pool.open_space(pool.find_space("s").unwrap().id()).unwrap();
    let mut buf = [0u8; 4096];
    assert!(reader.read_uncached_at(&mut buf, reader.size() - 4096).is_ok());
    assert!(reader.read_uncached_at(&mut buf, reader.size()).is_err());
    let wrapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut b = [0u8; 200];
        reader.read_uncached_at(&mut b, u64::MAX - 100).is_err()
    }));
    assert!(matches!(wrapped, Ok(true)), "not refused (a panic, or read)");
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// A GPT, its CRC32s sealed, whose entries are 192 bytes long (UEFI has
/// 128 * 2^n, Linux 128 alone): no table, for the pool's disks as for a
/// space's partitions. 1.3.0 read its partitions, which neither Windows nor
/// Linux sees (audit 2026-10-21, SPA-02).
#[test]
fn gpt_entries_of_a_size_uefi_does_not_have_are_no_table() {
    let (lbas, entry) = (2048u64, 192usize);
    let mut disk = vec![0u8; (lbas * 512) as usize];
    let mut entries = vec![0u8; 4 * entry];
    entries[..16].copy_from_slice(&[0x11; 16]);
    entries[32..40].copy_from_slice(&34u64.to_le_bytes());
    entries[40..48].copy_from_slice(&1000u64.to_le_bytes());
    for (lba, alternate, entries_lba) in [(1u64, lbas - 1, 2u64), (lbas - 1, 1, lbas - 33)] {
        let mut h = vec![0u8; 92];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&lba.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(lbas - 34).to_le_bytes());
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&4u32.to_le_bytes());
        h[84..88].copy_from_slice(&(entry as u32).to_le_bytes());
        h[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        let crc = crc32(&h);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        disk[(lba * 512) as usize..(lba * 512) as usize + 92].copy_from_slice(&h);
        let at = (entries_lba * 512) as usize;
        disk[at..at + entries.len()].copy_from_slice(&entries);
    }
    let dev = MemDevice(disk);
    assert_eq!(read_gpt(&dev, 512).unwrap(), None);
    assert!(guard::partitions(&dev, lbas * 512, 512).is_empty());
}
