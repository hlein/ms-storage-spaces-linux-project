//! `refs check` and the readers on hostile volumes: every check ends, in
//! time, with what it found (audit 2026-10-21, RFR-01; the auditor's
//! rfr_01, rfr_04, rfr_08 and rfr_10, their assertions turned to the right
//! outcome).

// Of the forge, what these tests need.
#[allow(dead_code)]
mod forge;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use forge::{Forge, Tree, le16, le32, le64, place};
use refs::Volume;
use refs::node::Node;
use refs::volume::{ROOT_DIRECTORY, SUPERBLOCK_LCN, UPCASE_TABLE};
use storage_spaces::io::{ReadAt, SparseImage};

/// The problems `refs check` finds on `forge`'s volume; a panic, or no
/// end within `limit` (the thread is left behind), fails the test.
fn check_within(forge: &Forge, limit: Duration) -> refs::Result<Vec<String>> {
    let (tx, rx) = mpsc::channel();
    let forge = forge.clone();
    std::thread::spawn(move || {
        let report = forge.volume().and_then(|v| v.check(&[])).map(|r| r.problems);
        let _ = tx.send(report);
    });
    match rx.recv_timeout(limit) {
        Ok(report) => report,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("the check did not end in {limit:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the check panicked"),
    }
}

/// r314uniform's block reference count table with its first leaf's first
/// row made to reach cluster 2^64 and its second to lie inside the first:
/// the check ends with the rows' problems. 1.3.0 went over the first row
/// cluster by cluster (more than 10 minutes, the auditor's rfr_01).
#[test]
fn a_reference_count_row_reaching_2_64_ends_the_check() {
    let mut forge = Forge::fixture("r314uniform");
    forge.patch(
        Tree::Root(6),
        |page| Node::at(page, 0x50).is_ok_and(|n| n.is_leaf() && n.len() >= 2),
        |page| {
            let node = Node::at(page, 0x50).unwrap();
            let rows: Vec<(usize, u64)> = node
                .rows()
                .flatten()
                .map(|r| (place(page, r.value), le64(r.value, 0)))
                .collect();
            let ((at0, first0), (at1, _)) = (rows[0], rows[1]);
            page[at0 + 8..at0 + 16].copy_from_slice(&(u64::MAX - first0).to_le_bytes());
            page[at1..at1 + 8].copy_from_slice(&(first0 + 1).to_le_bytes());
            page[at1 + 8..at1 + 16].copy_from_slice(&1u64.to_le_bytes());
        },
    );
    let problems = check_within(&forge, Duration::from_secs(60)).unwrap();
    assert!(problems.iter().any(|p| p.contains("overlap")), "{problems:?}");
}

/// A directory of r314basic64k's root made to name the root as its
/// object: the check visits the root once and says so. 1.3.0 walked the
/// root again and again (the auditor's rfr_04 stopped it after 5000
/// reads).
#[test]
fn a_directory_naming_its_parent_is_walked_once() {
    let mut forge = Forge::fixture("r314basic64k");
    let entry = |page: &[u8]| -> Option<usize> {
        let node = Node::at(page, 0x50).ok()?;
        if !node.is_leaf() {
            return None;
        }
        node.rows()
            .flatten()
            .find(|r| {
                r.key.len() >= 4
                    && le16(r.key, 0) == 0x30
                    && le16(r.key, 2) == 2
                    && r.value.len() >= 0x44
                    && le32(r.value, 0x40) & 0x1000_0000 != 0
            })
            .map(|r| place(page, r.value))
    };
    forge.patch(
        Tree::Object(ROOT_DIRECTORY),
        |page| entry(page).is_some(),
        |page| {
            let at = entry(page).unwrap();
            page[at + 8..at + 16].copy_from_slice(&ROOT_DIRECTORY.to_le_bytes());
        },
    );
    struct Counted<'a>(&'a SparseImage, AtomicUsize);
    impl ReadAt for Counted<'_> {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
            if self.1.fetch_add(1, Ordering::Relaxed) > 100_000 {
                return Err(std::io::Error::other("more than 100 000 reads"));
            }
            self.0.read_exact_at(buf, offset)
        }
        fn size(&self) -> std::io::Result<u64> {
            self.0.size()
        }
    }
    let dev = Counted(&forge.image, AtomicUsize::new(0));
    let report = Volume::open(&dev, forge.offset).unwrap().check(&[]).unwrap();
    assert!(report.directories < 100, "{} directories walked", report.directories);
    assert!(
        report.problems.iter().any(|p| p.contains("named again")),
        "{:?}",
        report.problems
    );
}

/// A directory page whose key index ends at 0 (rfr_08 part 1): the check
/// names the page and goes on. 1.3.0 ended with the node's error, the rest
/// of the volume unchecked.
#[test]
fn a_page_whose_key_index_does_not_parse_is_a_problem() {
    let mut forge = Forge::fixture("r314small");
    forge.patch(
        Tree::Object(ROOT_DIRECTORY),
        |page| Node::at(page, 0x50).is_ok_and(|n| n.is_leaf()),
        |page| {
            let h = 0x50 + le32(page, 0x50);
            page[h + 0x20..h + 0x24].copy_from_slice(&0u32.to_le_bytes());
        },
    );
    let problems = forge.volume().unwrap().check(&[]).unwrap().problems;
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("object 0x600: page") && p.contains("key index")),
        "{problems:?}"
    );
}

/// The upcase table's row of its units count made 0 (rfr_08 part 2): the
/// check says the table is damaged and checks the rest. 1.3.0 ended with
/// the table's error.
#[test]
fn a_damaged_upcase_table_is_a_problem() {
    let mut forge = Forge::fixture("r314small");
    forge.patch(
        Tree::Object(UPCASE_TABLE),
        |page| Node::at(page, 0x50).is_ok_and(|n| n.is_leaf()),
        |page| {
            let node = Node::at(page, 0x50).unwrap();
            let value = node
                .rows()
                .flatten()
                .find(|r| r.key.len() >= 4 && le32(r.key, 0) == 1)
                .unwrap()
                .value;
            let at = place(page, value);
            page[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        },
    );
    let report = forge.volume().unwrap().check(&[]).unwrap();
    assert!(
        report.problems.iter().any(|p| p.contains("upcase")),
        "{:?}",
        report.problems
    );
    assert!(report.files > 0);
}

/// The primary superblock zeroed (rfr_10): the log's state comes from a
/// copy at the volume's end, as the volume itself opened from one. 1.3.0
/// said the volume had no log control page.
#[test]
fn the_log_is_found_through_a_superblock_copy() {
    let mut forge = Forge::fixture("r314small");
    let (state, cluster) = {
        let vol = forge.volume().unwrap();
        (format!("{:?}", vol.log_state().unwrap()), vol.cluster)
    };
    forge
        .image
        .insert(forge.offset + SUPERBLOCK_LCN * cluster, &vec![0u8; cluster as usize]);
    let vol = forge.volume().unwrap();
    assert_eq!(format!("{:?}", vol.log_state().unwrap()), state);
}

/// r314uniform's first reference count row cut to 0x10 clusters, the
/// second made to lie inside it: the check reports the rows and counts
/// the clusters files map. 1.3.0 counted on from the second row's end,
/// before where it was (the plan's unsorted-rows case of rfr_01: a
/// subtraction overflow).
#[test]
fn reference_count_rows_inside_others_are_a_problem() {
    let mut forge = Forge::fixture("r314uniform");
    forge.patch(
        Tree::Root(6),
        |page| Node::at(page, 0x50).is_ok_and(|n| n.is_leaf() && n.len() >= 2),
        |page| {
            let node = Node::at(page, 0x50).unwrap();
            let rows: Vec<(usize, u64)> = node
                .rows()
                .flatten()
                .map(|r| (place(page, r.value), le64(r.value, 0)))
                .collect();
            let ((at0, first0), (at1, _)) = (rows[0], rows[1]);
            page[at0 + 8..at0 + 16].copy_from_slice(&0x10u64.to_le_bytes());
            page[at1..at1 + 8].copy_from_slice(&(first0 + 1).to_le_bytes());
            page[at1 + 8..at1 + 16].copy_from_slice(&1u64.to_le_bytes());
        },
    );
    let problems = check_within(&forge, Duration::from_secs(60)).unwrap();
    assert!(problems.iter().any(|p| p.contains("overlap")), "{problems:?}");
}

/// Two rows of r314small's medium allocator made uniform and free over
/// 2^63 + 2 clusters each (the auditor's rfr_06): the free clusters are
/// at most the volume's. 1.3.0 added them up past 2^64, a panic in builds
/// that check overflows (the auditor's crate did not: "not reproduced").
#[test]
fn free_counts_past_2_64_are_the_volumes_at_most() {
    let mut forge = Forge::fixture("r314small");
    forge.patch(
        Tree::Root(1),
        |page| Node::at(page, 0x50).is_ok_and(|n| n.is_leaf() && n.len() >= 2),
        |page| {
            let node = Node::at(page, 0x50).unwrap();
            let values: Vec<usize> = node.rows().flatten().map(|r| place(page, r.value)).collect();
            for &at in &values[..2] {
                page[at + 8..at + 16].copy_from_slice(&(u64::MAX / 2 + 2).to_le_bytes());
                page[at + 0x10..at + 0x12].copy_from_slice(&0xffffu16.to_le_bytes());
                page[at + 0x12..at + 0x14].copy_from_slice(&2u16.to_le_bytes());
            }
        },
    );
    let vol = forge.volume().unwrap();
    assert!(vol.free_clusters().unwrap() <= vol.boot.volume_size() / vol.cluster);
}
