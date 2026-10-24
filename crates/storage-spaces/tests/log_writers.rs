//! The writers of the write-back cache and the parity journal, continued
//! from what a small hostile log names (audit 2026-10-21, SPB-03 and
//! SPW-02): what they take in memory and time stays a multiple of the log,
//! and an entry is never cut off or misencoded in its slot.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use storage_spaces::Guid;
use storage_spaces::cache::{CacheHeader, CacheIndex, CacheWriter, Checkpoint, LogWrite, Validity};
use storage_spaces::journal::{JournalWriter, ParityJournal, SPVDT_SIGNATURE};

/// Counts the bytes each thread allocated now and at most: the tests run
/// side by side, and what the others take is not the measured one's (the
/// counters were the process's, and a test allocating 64 MiB beside it
/// failed it now and then).
struct Counting;

thread_local! {
    static NOW: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let _ = NOW.try_with(|now| {
                now.set(now.get() + layout.size());
                let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now.get())));
            });
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        // Freed by another thread than took it: counted where it is freed.
        let _ = NOW.try_with(|now| now.set(now.get().saturating_sub(layout.size())));
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// What `f` returns, the bytes it took at most beyond what its thread had
/// allocated before, and how long it took.
fn measured<T>(f: impl FnOnce() -> T) -> (T, usize, Duration) {
    let before = NOW.with(Cell::get);
    PEAK.with(|peak| peak.set(before));
    let start = Instant::now();
    let out = f();
    let took = start.elapsed();
    (out, PEAK.with(Cell::get) - before, took)
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data
        .iter()
        .fold(!0u32, |c, &b| table[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

const OWNER: Guid = Guid([0x11; 16]);

/// A slot of `size` bytes, sequence `sequence`, with `count` entries.
fn slot(size: usize, sequence: u64, count: u32, entries: &[u8]) -> Vec<u8> {
    let mut s = vec![0u8; size];
    s[0..8].copy_from_slice(b"SPSLOT\0\0");
    s[8..24].copy_from_slice(&OWNER.to_mixed_endian());
    s[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());
    s[0x1c..0x20].copy_from_slice(&(size as u32).to_le_bytes());
    s[0x28..0x30].copy_from_slice(&sequence.to_le_bytes());
    s[0x30..0x34].copy_from_slice(&count.to_le_bytes());
    s[0x38..0x38 + entries.len()].copy_from_slice(entries);
    let crc = crc32(&s);
    s[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
    s
}

/// A journal entry (state 2) of the run at `run` with these run words.
fn run_entry(run: u64, words: &[u16]) -> Vec<u8> {
    let mut e = run.to_le_bytes().to_vec();
    e.extend_from_slice(&2u16.to_le_bytes());
    e.extend_from_slice(&((2 * words.len()) as u16).to_le_bytes());
    words.iter().for_each(|w| e.extend_from_slice(&w.to_le_bytes()));
    e
}

/// A journal header: slots of `slot_size` bytes from 0x1000, `slots` of
/// them, no checkpoint areas, extent runs of `run_size` bytes in stripes of
/// `stripe` (0: stripes per run unknown).
fn journal_header(slot_size: u32, slots: u32, run_size: u64, stripe: u32) -> Vec<u8> {
    CacheHeader {
        owner_guid: OWNER,
        sequence: 1,
        slot_offset: 0x1000,
        slot_size,
        slot_count: slots,
        checkpoint_offset: 0,
        checkpoint_size: 0,
        checkpoint_count: 0,
        data_offset: run_size,
        chunk_size: stripe,
        chunk_count: 1,
    }
    .encode(SPVDT_SIGNATURE)
}

fn load(area: &[u8]) -> storage_spaces::Result<Option<ParityJournal>> {
    ParityJournal::load(OWNER, |off: u64, buf: &mut [u8]| {
        buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
        Ok(())
    })
}

/// A cache of chunks of 64 MiB of 512-byte sectors whose checkpoint (4 KiB)
/// holds 256 whole chunks: continuing it takes kilobytes. 1.3.0 expanded
/// each into a flag per sector, 32 MiB (the auditor's spb_03; a checkpoint
/// of 1 MiB took 8 GiB).
#[test]
fn a_cache_of_whole_chunks_of_64_mib_is_continued_in_little_memory() {
    let header = CacheHeader {
        owner_guid: OWNER,
        sequence: 10,
        slot_offset: 0x2000,
        slot_size: 0x1000,
        slot_count: 4,
        checkpoint_offset: 0x6000,
        checkpoint_size: 0x4000,
        checkpoint_count: 1,
        data_offset: 0x10_0000,
        chunk_size: 64 << 20,
        chunk_count: 1024,
    };
    let mut entries = Vec::new();
    for i in 0..256u64 {
        entries.extend_from_slice(&(i * (64 << 20)).to_le_bytes());
        entries.extend_from_slice(&(i as u32).to_le_bytes());
        entries.extend_from_slice(&3u16.to_le_bytes());
        entries.extend_from_slice(&0u16.to_le_bytes());
    }
    let checkpoint = Checkpoint::encode(OWNER, 1, 10, 0, 256, &entries);
    let mut area = vec![0u8; 0x10000];
    area[..CacheHeader::SIZE].copy_from_slice(&header.encode(storage_spaces::cache::SPCACHE_SIGNATURE));
    area[0x6000..0x6000 + checkpoint.len()].copy_from_slice(&checkpoint);
    let index = CacheIndex::load(header, 512, |off: u64, buf: &mut [u8]| {
        buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
        Ok(())
    })
    .unwrap();
    assert_eq!(index.cached_chunks(), 256);
    let (writer, bytes, _) = measured(|| index.writer(0));
    assert!(bytes < 1 << 20, "{bytes} bytes");
    assert_eq!(writer.chunk_runs(0).unwrap().1, [(true, 64 << 20)]);
}

/// A write of 32 MiB into a chunk of 64 MiB of 512-byte sectors: its runs
/// of 65 536 sectors are logged in words of at most 0x7fff sectors and load
/// back as written. 1.3.0 cut a run's length to 15 bits: the entry said 0
/// sectors.
#[test]
fn runs_longer_than_a_word_are_logged_in_several() {
    let header = CacheHeader {
        owner_guid: OWNER,
        sequence: 1,
        slot_offset: 0x2000,
        slot_size: 0x1000,
        slot_count: 4,
        checkpoint_offset: 0,
        checkpoint_size: 0,
        checkpoint_count: 0,
        data_offset: 0x10_0000,
        chunk_size: 64 << 20,
        chunk_count: 4,
    };
    let mut writer = CacheWriter::new(header.clone(), 512, 0).unwrap();
    let mut area = vec![0u8; 0x6000];
    area[0x2000..0x3000].copy_from_slice(&writer.init_slot());
    for w in writer.write(0, 32 << 20) {
        let LogWrite::Slot(i, page) = w else {
            panic!("a checkpoint")
        };
        area[0x2000 + i * 0x1000..0x3000 + i * 0x1000].copy_from_slice(&page);
    }
    let index = CacheIndex::load(header, 512, |off: u64, buf: &mut [u8]| {
        buf.copy_from_slice(&area[off as usize..off as usize + buf.len()]);
        Ok(())
    })
    .unwrap();
    let mappings = index.mappings();
    assert_eq!(mappings.len(), 1);
    let Validity::Runs(runs) = mappings[0].2 else {
        panic!("{:?}", mappings[0])
    };
    let mut merged: Vec<(bool, u64)> = Vec::new();
    for (valid, n) in runs.iter() {
        match merged.last_mut() {
            Some((v, m)) if *v == valid => *m += n,
            _ => merged.push((valid, n)),
        }
    }
    assert_eq!(merged, [(true, 65536), (false, 65536)]);
}

/// A journal entry of 10 000 runs of 0x7fff stripes each (327 670 000
/// stripes, 20 KB in its slot): continuing it takes kilobytes and no time.
/// 1.3.0 expanded the run into a flag per stripe (327 MB), each looked up
/// among the 10 000 runs (the auditor's spb_03; plan item 3.3).
#[test]
fn a_journal_entry_of_ten_thousand_runs_is_continued_in_one_pass() {
    let (n, stripe) = (10_000u64, 0x40000u32);
    let stripes = n * 0x7fff;
    let words: Vec<u16> = (0..n).map(|i| ((i as u16 & 1) << 15) | 0x7fff).collect();
    let entry = run_entry(0, &words);
    let mut area = vec![0u8; 0x9000];
    area[..0x60].copy_from_slice(&journal_header(0x8000, 1, stripes * u64::from(stripe), stripe));
    area[0x1000..0x9000].copy_from_slice(&slot(0x8000, 1, 1, &entry));
    let journal = load(&area).unwrap().unwrap();
    let mut writer = journal.writer(OWNER);
    let (consistent, bytes, took) = measured(|| [0, 0x7fff, stripes - 1].map(|s| writer.is_consistent(0, stripes, s)));
    assert_eq!(consistent, [false, true, true]);
    assert!(bytes < 1 << 20, "{bytes} bytes");
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// A run word of no stripes (0x8000) is not one Windows writes: the journal
/// is not loaded. 1.3.0 took it for a run.
#[test]
fn a_journal_run_word_of_no_stripes_is_refused() {
    let entry = run_entry(0, &[0x8005, 0x8000, 0x0003]);
    let mut area = vec![0u8; 0x2000];
    area[..0x60].copy_from_slice(&journal_header(0x1000, 1, 0, 0));
    area[0x1000..0x2000].copy_from_slice(&slot(0x1000, 1, 1, &entry));
    let Err(e) = load(&area) else { panic!("loaded") };
    assert!(e.to_string().contains("run word of no stripes"), "{e}");
}

/// A slot area of 64 MiB (the most a header may name) of entries of 2 000
/// one-stripe runs each, for as many runs: decoded, more than the 256 MiB
/// the cache index may take, so the journal is not loaded. 1.3.0 had no
/// bound (about 600 MB here).
#[test]
fn a_journals_entries_have_the_cache_index_budget() {
    let (size, slots) = (0x1000usize, 16384usize);
    let mut area = vec![0u8; 0x1000 + size * slots];
    area[..0x60].copy_from_slice(&journal_header(size as u32, slots as u32, 0, 0));
    let words = vec![0x0001u16; 2000];
    for k in 0..slots {
        let entry = run_entry(k as u64 * (512 << 20), &words);
        let at = 0x1000 + k * size;
        area[at..at + size].copy_from_slice(&slot(size, k as u64 + 1, 1, &entry));
    }
    let (result, bytes, _) = measured(|| load(&area).map(|j| j.is_some()));
    let Err(e) = result else { panic!("loaded") };
    assert!(e.to_string().contains("would take more than"), "{e}");
    assert!(bytes < 400 << 20, "{bytes} bytes");
}

/// A journal header naming two checkpoint areas of no bytes at 0 (the
/// header's own place): they count as none, and the writer never writes a
/// checkpoint, however often its log wraps. 1.3.0 wrote one over the header
/// (the auditor's repro_spw05).
#[test]
fn checkpoint_areas_of_no_bytes_are_none() {
    let mut header = journal_header(0x1000, 4, 0, 0);
    header[0x4c..0x50].copy_from_slice(&2u32.to_le_bytes());
    let crc = {
        let mut h = header.clone();
        h[0x24..0x28].fill(0);
        crc32(&h[..0x60])
    };
    header[0x24..0x28].copy_from_slice(&crc.to_le_bytes());
    let mut area = vec![0u8; 0x5000];
    area[..header.len().min(0x1000)].copy_from_slice(&header[..header.len().min(0x1000)]);
    let journal = load(&area).unwrap().unwrap();
    assert_eq!(journal.checkpoint_geometry().2, 0);
    let mut writer = journal.writer(OWNER);
    for k in 0..12 {
        let writes = writer.write(0, 64, k, 1).unwrap();
        assert!(writes.iter().all(|w| w.slot().is_some()), "write {k}: a checkpoint");
    }
}

/// A run of 66 000 000 stripes: even recorded as not consistent as a whole
/// its run words (2 015) do not fit a slot of 4 KiB, and writing it is an
/// error. 1.3.0 cut the entry off at the end of its slot with its length
/// left as it was: the journal no longer loaded (the auditor's
/// repro_spw05).
#[test]
fn a_run_too_long_for_a_slot_is_an_error_not_a_cut_entry() {
    let mut writer = JournalWriter::new(OWNER, 0x1000, 16);
    let Err(e) = writer.write(0, 66_000_000, 0, 1) else {
        panic!("written")
    };
    assert!(e.to_string().contains("more than an entry"), "{e}");
}
