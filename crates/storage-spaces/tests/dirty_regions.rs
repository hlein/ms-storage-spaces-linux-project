//! The dirty region log of mirror spaces and mirror tiers, and scrubbing
//! tiered spaces, on fixtures patched in memory (audit 2026-10-21, SPB-05 and
//! SPG-01: the auditor's spb_05, spg03 and spg04): what the guard compares,
//! what it says it compared, and copies of a log page that disagree.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use storage_spaces::drt::DrtWriter;
use storage_spaces::format::{SLAB_SIZE, SpaceRole};
use storage_spaces::gpt::Partition;
use storage_spaces::guard::{self, Options};
use storage_spaces::io::{Overlay, ReadAt, SparseImage, WriteAt};
use storage_spaces::ops::scrub_spaces;
use storage_spaces::report::{Check, Report, Status, Verdict};
use storage_spaces::{Part, Pool, SpaceReader};

fn images(name: &str) -> Vec<SparseImage> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    (0..)
        .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
        .map(|f| SparseImage::read_from(f).unwrap())
        .collect()
}

fn no_fs<D: ReadAt>(_: &SpaceReader<'_, D>, _: &Partition, _: &[String]) -> Vec<Check> {
    Vec::new()
}

fn report<D: ReadAt>(pool: &Pool<D>, id: u64) -> Report {
    guard::space_report(pool, &pool.spaces[&id], &[], &Options::default(), &mut no_fs)
}

fn drl_check(r: &Report) -> &Check {
    r.checks.iter().find(|c| c.id == "space.drl").unwrap()
}

/// Where each copy of the dirty region log of user space `id` is: (device,
/// byte) of its one slab, whose header pages are at its start and 8 KiB
/// before its end.
fn log_slabs<D: ReadAt>(pool: &Pool<D>, id: u64) -> Vec<(usize, u64)> {
    let container = pool
        .children(id)
        .find(|c| c.info.role == SpaceRole::Other(0x06))
        .unwrap();
    let child = pool.children(container.id()).find(|c| !c.extents.is_empty()).unwrap();
    assert_eq!(child.info.range, Some((0, SLAB_SIZE)));
    child
        .extents
        .iter()
        .map(|e| pool.slab_location(e.disk_id, e.physical_slab).unwrap().unwrap())
        .collect()
}

/// Writes a log listing `runs` into both header pages of every copy.
fn list<D: ReadAt, W: WriteAt>(pool: &Pool<D>, disks: &[W], id: u64, runs: &[u64]) {
    let mut w = DrtWriter::new();
    for &run in runs {
        w.write(run).unwrap();
    }
    for (device, at) in log_slabs(pool, id) {
        disks[device].write_all_at(&w.page(), at).unwrap();
        disks[device].write_all_at(&w.page(), at + SLAB_SIZE - 0x2000).unwrap();
    }
}

/// Makes copy 0 of `row` of column 0 of `space` differ from the others,
/// 64 MiB into the slab (outside what the write-back caches of the fixtures
/// hold).
fn differ<D: ReadAt, W: WriteAt>(pool: &Pool<D>, disks: &[W], space: u64, virtual_slab: u64) {
    let e = pool.spaces[&space]
        .extents
        .iter()
        .find(|e| e.virtual_slab == virtual_slab && e.column == 0 && e.copy == 0)
        .unwrap();
    let (device, at) = pool.slab_location(e.disk_id, e.physical_slab).unwrap().unwrap();
    disks[device].write_all_at(&[0xab; 4096], at + (64 << 20)).unwrap();
}

/// The copies of a header page of the log at the same generation, listing
/// different runs: not something Windows writes (each update has the next
/// generation), so which runs were being written is not known. The space is
/// suspect; 1.3.0 called the difference a note, read one copy's runs and
/// called the space healthy (the auditor's spb_05).
#[test]
fn copies_of_a_log_page_listing_different_runs_at_one_generation_are_suspect() {
    let images = images("drtdisc");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("drtdisc").unwrap().id();
    for (copy, (device, at)) in log_slabs(&pool, id).into_iter().enumerate() {
        let (_, page) = DrtWriter::new().write(copy as u64).unwrap();
        disks[device].write_all_at(&page, at).unwrap();
    }
    let reader = pool.open_space(id).unwrap();
    let notes = reader.header_notes();
    assert!(
        notes
            .iter()
            .any(|n| n.part == Part::DirtyRegions && n.status == Status::Suspect),
        "{notes:?}"
    );
    let r = report(&pool, id);
    assert_eq!(r.verdict(), Verdict::Suspect, "{}", r.to_text());
}

/// A two-way mirror with one disk at hand whose log lists a run: nothing to
/// compare its copies with, and the check does not say they agree. 1.3.0
/// counted what it read of the one copy as compared ("compared, the copies
/// agree"; the auditor's spg04).
#[test]
fn listed_runs_with_one_copy_at_hand_are_not_called_compared() {
    let images = images("mirror2");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.user_spaces().next().unwrap().id();
        list(&pool, &disks, id, &[0]);
    }
    let pool = Pool::open(vec![&disks[0]]).unwrap();
    let id = pool.user_spaces().next().unwrap().id();
    let reader = pool.open_space(id).unwrap();
    let c = reader.check_dirty_regions(Some(16 << 20)).unwrap().unwrap();
    assert_eq!(c.compared, 0, "{c:?}");
    assert_eq!(c.unreadable, 16 << 20, "{c:?}");
    let r = report(&pool, id);
    let drl = drl_check(&r);
    assert!(!drl.summary.contains("the copies agree"), "{}", drl.summary);
}

/// A device that counts the bytes read from it.
struct Counting<D> {
    inner: D,
    read: AtomicU64,
}

impl<D: ReadAt> ReadAt for Counting<D> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.read.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.inner.read_exact_at(buf, offset)
    }
    fn size(&self) -> std::io::Result<u64> {
        self.inner.size()
    }
}

/// The same with the one copy at hand counted: it is not read, there being
/// nothing to compare it with. Before 1.3.1 was released, the checks of a
/// degraded tiered space read its mirror tier's one copy up to the read
/// budget (`ntfstier` without a disk: 2 s, 54 s on a test VM, past the 40 s
/// the checks of a space may take; found by the VM checks of plan item
/// 3.17).
#[test]
fn listed_runs_with_one_copy_at_hand_are_not_read() {
    let images = images("mirror2");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    {
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.user_spaces().next().unwrap().id();
        list(&pool, &disks, id, &[0]);
    }
    let one = Counting {
        inner: &disks[0],
        read: AtomicU64::new(0),
    };
    let pool = Pool::open(vec![&one]).unwrap();
    let id = pool.user_spaces().next().unwrap().id();
    let reader = pool.open_space(id).unwrap();
    let before = one.read.load(Ordering::Relaxed);
    let c = reader.check_dirty_regions(Some(16 << 20)).unwrap().unwrap();
    assert_eq!(c.unreadable, 16 << 20, "{c:?}");
    assert_eq!(one.read.load(Ordering::Relaxed) - before, 0, "bytes read");
}

/// A thin mirror whose log lists four unallocated runs (1 GiB) before an
/// allocated one whose copies differ: unallocated rows hold nothing to
/// compare and leave the read budget (1 GiB) to the run that has data, so
/// the difference is found. 1.3.0 spent the budget on the unallocated rows
/// and found nothing (the auditor's spg04).
#[test]
fn unallocated_listed_runs_leave_the_read_budget_alone() {
    let images = images("mirrorthin");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let mut pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.user_spaces().next().unwrap().id();
    // Row 0's data moved to virtual slab 10, behind four unallocated runs.
    for e in &mut pool.spaces.get_mut(&id).unwrap().extents {
        if e.virtual_slab == 0 {
            e.virtual_slab = 10;
        }
    }
    list(&pool, &disks, id, &[4, 5, 6, 7, 10]);
    differ(&pool, &disks, id, 10);
    let reader = pool.open_space(id).unwrap();
    let c = reader
        .check_dirty_regions(Options::default().journal_budget)
        .unwrap()
        .unwrap();
    assert!(!c.differ.is_empty(), "{c:?}");
    assert!(!c.incomplete, "{c:?}");
}

/// A tiered space whose log lists the first run of its mirror tier, the
/// copies of which differ: the guard compares the tier's copies and the
/// space is suspect, as its reads there fail. 1.3.0 compared nothing in a
/// tiered space ("compared when read") and called it healthy (the auditor's
/// spg03).
#[test]
fn listed_runs_of_a_mirror_tier_are_compared() {
    let images = images("tiered");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.user_spaces().next().unwrap().id();
    let ssd = pool
        .children(id)
        .find(|c| c.info.is_child && c.info.range.is_some_and(|(start, _)| start == 0))
        .unwrap()
        .id();
    list(&pool, &disks, id, &[0]);
    differ(&pool, &disks, ssd, 0);
    let r = report(&pool, id);
    assert_eq!(drl_check(&r).status, Status::Suspect, "{}", r.to_text());
    assert_eq!(r.verdict(), Verdict::Suspect, "{}", r.to_text());
    let reader = pool.open_space(id).unwrap();
    let mut buf = [0u8; 4096];
    let e = reader.read_exact_at(&mut buf, 64 << 20).unwrap_err();
    assert!(e.to_string().contains("mirror copies differ"), "{e}");
}

/// Scrub compares the copies of a mirror tier and the parity of a parity
/// tier: 1.3.0 skipped tiered spaces ("tiered, not scrubbed").
#[test]
fn scrub_compares_the_tiers_of_a_tiered_space() {
    for (name, tier_start) in [("tiered", 0), ("mapar", 1 << 30)] {
        let images = images(name);
        let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
        let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
        let id = pool.user_spaces().next().unwrap().id();
        let clean = scrub_spaces(&pool, Some(id)).unwrap();
        assert_eq!((clean.mismatches, clean.unsettled), (0, 0), "{name}: {:?}", clean.lines);
        let tier = pool
            .children(id)
            .find(|c| c.info.is_child && c.info.range.is_some_and(|(start, _)| start == tier_start))
            .unwrap();
        let first = tier.extents.iter().map(|e| e.virtual_slab).min().unwrap();
        let tier = tier.id();
        differ(&pool, &disks, tier, first);
        let s = scrub_spaces(&pool, Some(id)).unwrap();
        assert_eq!(s.mismatches + s.unsettled, 1, "{name}: {:?}", s.lines);
        assert!(
            s.lines.iter().all(|l| !l.ends_with(": tiered, not scrubbed")),
            "{name}: {:?}",
            s.lines
        );
    }
}

/// A dirty region log whose range is too small for its two header pages
/// (at its start and 8 KiB before its end): the newer page, at the end of
/// its slab, listing runs, was not read where the range was 8 KiB or less,
/// and the space was healthy with "no extent runs listed" (the auditor's
/// spb_05). Such a log is not understood: the space fails.
#[test]
fn a_log_too_small_for_its_two_pages_fails() {
    let images = images("drtdisc");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let mut pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("drtdisc").unwrap().id();
    let mut w = DrtWriter::new();
    w.write(0).unwrap();
    w.write(2).unwrap();
    for (device, at) in log_slabs(&pool, id) {
        disks[device].write_all_at(&w.page(), at + SLAB_SIZE - 0x2000).unwrap();
    }
    let reader = pool.open_space(id).unwrap();
    assert_eq!(reader.dirty_regions().unwrap().dirty_runs(), 2);
    let container = pool
        .children(id)
        .find(|c| c.info.role == SpaceRole::Other(0x06))
        .unwrap()
        .id();
    let child = pool.children(container).next().unwrap().id();
    for len in [0x2000, 0x1000] {
        pool.spaces.get_mut(&child).unwrap().info.range = Some((0, len));
        let Err(e) = pool.open_space(id) else {
            panic!("{len:#x}: the space opened")
        };
        assert!(e.to_string().contains("dirty region log"), "{len:#x}: {e}");
        let r = report(&pool, id);
        assert_eq!(r.verdict(), Verdict::Failed, "{len:#x}: {}", r.to_text());
    }
}

/// A mirror whose dirty region log lists a run whose copies differ (a crash
/// with writes in flight) has no device-mapper segments: they map one copy,
/// where reads compare the copies and fail. Listed runs whose copies agree
/// are mapped. 1.3.0 mapped the first copy of a listed run whose copies
/// differed (the auditor's spb_07).
#[test]
fn a_mirror_whose_listed_run_differs_has_no_segments() {
    let images = images("drtdisc");
    let disks: Vec<Overlay<&SparseImage>> = images.iter().map(Overlay::new).collect();
    let pool = Pool::open(disks.iter().collect::<Vec<_>>()).unwrap();
    let id = pool.find_space("drtdisc").unwrap().id();
    list(&pool, &disks, id, &[0]);
    assert!(pool.open_space(id).unwrap().segments().is_ok());
    differ(&pool, &disks, id, 0);
    let reader = pool.open_space(id).unwrap();
    assert!(!reader.check_dirty_regions(None).unwrap().unwrap().differ.is_empty());
    let Err(e) = reader.segments() else {
        panic!("mapped: {:?}", reader.segments())
    };
    assert!(
        e.to_string()
            .contains("copies of an extent run the dirty region log lists differ"),
        "{e}"
    );
}
