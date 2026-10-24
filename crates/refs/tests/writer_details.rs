//! Details of the ReFS writer the audit of 2026-10-21 named (RFW-03,
//! RFW-04; the auditor's rfw_01 and rfw_02, their assertions turned to the
//! right outcome): the clock of pages a held commit writes, the tail of a
//! file's last cluster when it shortens, a file with snapshots shortened
//! and grown.

// Of the forge, what these tests need.
#[allow(dead_code)]
mod forge;

use std::fs::File;
use std::path::Path;

use forge::{Forge, le32};
use refs::page::PageRef;
use refs::{Content, Volume};
use storage_spaces::io::{Overlay, ReadAt, SparseImage};

fn load(name: &str) -> (SparseImage, u64) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let text = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap();
    let image = SparseImage::read_from(File::open(dir.join("disk.fixture")).unwrap()).unwrap();
    (image, manifest["partition_offset"].as_u64().unwrap())
}

fn read_all<D: ReadAt>(vol: &Volume<D>, path: &str) -> Vec<u8> {
    let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
    let data = file.data.unwrap();
    let mut buf = vec![0u8; data.size as usize];
    assert_eq!(vol.read_stream(&data, 0, &mut buf).unwrap(), buf.len());
    buf
}

/// An integrity stream of 2 MiB (8 runs, their checksums too many for the
/// record: an extent map page) written in the second step of one commit:
/// the map page carries the commit's clock, as every page of it does
/// (the auditor's rfw_01: one more there).
#[test]
fn pages_of_a_later_step_of_one_commit_carry_its_clock() {
    let (image, offset) = load("r314empty");
    let overlay = Overlay::new(&image);
    let mut vol = Volume::open(&overlay, offset).unwrap();
    let c0 = vol.checkpoint.clock;
    let data = vec![0x42u8; 512 * 4096];
    vol.in_one_commit(|v| {
        v.create_file("/integ.bin", &[], 1000)?;
        v.set_integrity("/integ.bin", true)?;
        v.write_file("/integ.bin", &data, 1001)
    })
    .unwrap();
    assert_eq!(vol.checkpoint.clock, c0 + 1);
    let entry = vol.lookup("/integ.bin").unwrap();
    let rows = vol.attribute_rows(&entry).unwrap();
    let (_, value) = rows.iter().find(|(_, v)| v.len() >= 0x88 + 0x28).unwrap();
    let map = vol.node_in(value, 0).unwrap();
    let child = PageRef::parse(map.rows().next().unwrap().unwrap().value).unwrap();
    let page = vol.read_page(&child, false).unwrap();
    assert_eq!(u64::from_le_bytes(page[0x10..0x18].try_into().unwrap()), c0 + 1);
    assert_eq!(read_all(&vol, "/integ.bin"), data);
}

/// A file of one cluster of 0xaa shortened in place to 3000 bytes (an
/// integrity stream and a plain one): the rest of its cluster holds zeros
/// on the disk, and its checksum is that of the cluster as it is. 1.3.0
/// left the old bytes there (the auditor's rfw_02).
#[test]
fn a_file_shortened_in_place_has_the_rest_of_its_last_cluster_zeroed() {
    for integrity in [true, false] {
        let (image, offset) = load("r314empty");
        let overlay = Overlay::new(&image);
        let mut vol = Volume::open(&overlay, offset).unwrap();
        let initial = vec![0xaau8; 4096];
        vol.create_file("/trunc.bin", &[], 999).unwrap();
        if integrity {
            vol.set_integrity("/trunc.bin", true).unwrap();
        }
        vol.write_file("/trunc.bin", &initial, 1000).unwrap();
        vol.update_file("/trunc.bin", &&initial[..3000], &[], 1001).unwrap();
        assert_eq!(read_all(&vol, "/trunc.bin"), &initial[..3000]);
        let file = vol.open_file(&vol.lookup("/trunc.bin").unwrap()).unwrap();
        let Content::Extents(x) = &file.data.unwrap().content else {
            panic!("resident")
        };
        let cluster = vol.read_physical(vol.translate(x[0].vlcn).unwrap(), 1).unwrap();
        assert!(
            cluster[3000..].iter().all(|&b| b == 0),
            "integrity {integrity}: the old bytes stayed"
        );
        assert_eq!(
            vol.check(&[]).unwrap().problems,
            Vec::<String>::new(),
            "integrity {integrity}"
        );
    }
}

/// The live level of r314feat's /snap/file.txt (stream snapshots s1 and
/// s2) shortened into a cluster, one the live level has and one only a
/// snapshot has: the cluster it reads now holds zeros past the new end,
/// the snapshots as they were. 1.3.0 left the old bytes in its own and
/// did not touch the snapshot's (which it reads still: no zeros).
#[test]
fn a_file_with_snapshots_shortened_has_the_rest_of_its_last_cluster_zeroed() {
    let path = "/snap/file.txt";
    fn snapshots<D: ReadAt>(vol: &Volume<D>, path: &str) -> Vec<(String, Vec<u8>)> {
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        file.snapshots
            .iter()
            .map(|(name, s)| {
                let mut b = vec![0u8; s.size as usize];
                assert_eq!(vol.read_stream(s, 0, &mut b).unwrap(), b.len());
                (name.clone(), b)
            })
            .collect()
    }
    // Cluster 128 the live level's own (written after the last
    // snapshot), cluster 1 an older level's (tests/write.rs
    // changing_files_with_snapshots).
    for new_size in [128 * 4096 + 100u64, 4096 + 100] {
        let (image, offset) = load("r314feat");
        let overlay = Overlay::new(&image);
        let mut vol = Volume::open(&overlay, offset).unwrap();
        let before = snapshots(&vol, path);
        let data = read_all(&vol, path);
        vol.update_file(path, &&data[..new_size as usize], &[], 1).unwrap();
        assert_eq!(read_all(&vol, path), &data[..new_size as usize]);
        assert_eq!(snapshots(&vol, path), before, "{new_size}: snapshots changed");
        let file = vol.open_file(&vol.lookup(path).unwrap()).unwrap();
        let Content::Extents(x) = &file.data.unwrap().content else {
            panic!("resident")
        };
        let c = new_size / vol.cluster;
        let run = x.iter().find(|x| x.vcn <= c && c < x.vcn + x.clusters).unwrap();
        let lcn = vol.translate(run.vlcn + (c - run.vcn)).unwrap();
        let cluster = vol.read_physical(lcn, 1).unwrap();
        assert!(
            cluster[(new_size % vol.cluster) as usize..].iter().all(|&b| b == 0),
            "{new_size}: the old bytes stayed"
        );
    }
}

/// r314small's checkpoint with the reference to its object table moved
/// into its second cluster (4 KiB clusters, pages of 16 KiB), the array of
/// root offsets naming it there, the checkpoint sealed again: the volume
/// does not open. The checksum covers the first cluster alone, as Windows
/// keeps a checkpoint; 1.3.0 took the root from the rest of the page, and
/// a commit, which writes the first cluster, indexed past it (a panic; the
/// audit's RFW-03 point 2).
#[test]
fn a_checkpoint_root_past_its_first_cluster_is_refused() {
    let mut forge = Forge::fixture("r314small");
    let (cluster, size) = {
        let vol = forge.volume().unwrap();
        (vol.cluster as usize, vol.checkpoint.reference_size)
    };
    forge.edit_checkpoint(|chkp| {
        assert!(chkp.len() > cluster, "a checkpoint page of one cluster");
        let array = if le32(chkp, 0x78) & 0x200 != 0 {
            le32(chkp, 0x94)
        } else {
            0x94
        };
        let at = le32(chkp, array);
        let to = cluster + 0x100;
        let reference = chkp[at..at + size].to_vec();
        chkp[to..to + size].copy_from_slice(&reference);
        chkp[array..array + 4].copy_from_slice(&(to as u32).to_le_bytes());
    });
    let e = forge.volume().err().expect("opened");
    assert!(e.to_string().contains("outside its cluster"), "{e}");
}
