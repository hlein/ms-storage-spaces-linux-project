//! The parity journal's writer continuing a fuzzed journal (audit
//! 2026-10-21, plan item 3.14): the input is the journal space from its
//! start, read as parity_journal reads it; a journal that loads is
//! continued (`ParityJournal::writer`) with writes the input's tail names,
//! put where its header puts slots and checkpoints, and loaded again: no
//! stripe that was dirty and written by none may load consistent, and the
//! newest write of each run must load so. The writer must take
//! memory and time by the log, not by what the log names (1.3.0 expanded a
//! run into a flag per stripe).
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::Guid;
use storage_spaces::cache::LogWrite;
use storage_spaces::journal::ParityJournal;

const OWNER: [u8; 16] = [9; 16];

fn set_crc(b: &mut [u8], field: usize) {
    b[field..field + 4].fill(0);
    let crc = crc32fast::hash(b);
    b[field..field + 4].copy_from_slice(&crc.to_le_bytes());
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 0x80 {
        return;
    }
    // The last 64 bytes name the writes, the rest is the journal space.
    let (journal_bytes, writes) = data.split_at(data.len() - 64);
    let mut space = journal_bytes.to_vec();
    space.resize(space.len().max(0x60), 0);
    space[0..8].copy_from_slice(b"SPVDT\0\0\0");
    space[8..24].copy_from_slice(&OWNER);
    let slot_size = 0x40 + (le32(&space, 0x38) % 0x1000);
    let slot_count = le32(&space, 0x3c) % 64;
    space[0x30..0x38].copy_from_slice(&0x60u64.to_le_bytes());
    space[0x38..0x3c].copy_from_slice(&slot_size.to_le_bytes());
    space[0x3c..0x40].copy_from_slice(&slot_count.to_le_bytes());
    set_crc(&mut space[..0x60], 0x24);
    let slots_end = (0x60 + (slot_size * slot_count) as usize).min(space.len());
    for slot in space[0x60..slots_end].chunks_exact_mut(slot_size as usize) {
        if slot[0] & 1 == 1 {
            slot[0..8].copy_from_slice(b"SPSLOT\0\0");
            slot[0x1c..0x20].copy_from_slice(&slot_size.to_le_bytes());
            slot[0x20..0x24].fill(0);
            set_crc(slot, 0x24);
        }
    }
    let mut owner = OWNER;
    owner[0..4].reverse();
    owner[4..6].reverse();
    owner[6..8].reverse();
    let owner = Guid::from_slice(&owner).unwrap();
    let load = |space: &[u8]| {
        ParityJournal::load(owner, |offset: u64, buf: &mut [u8]| {
            buf.fill(0);
            if let Some(src) = usize::try_from(offset).ok().and_then(|o| space.get(o..)) {
                let n = src.len().min(buf.len());
                buf[..n].copy_from_slice(&src[..n]);
            }
            Ok(())
        })
    };
    let Ok(Some(journal)) = load(&space) else {
        return;
    };
    let mut writer = journal.writer(owner);
    // Where the header puts its slots and checkpoints.
    let (slot_offset, slot_size) = (le64(&space, 0x30) as usize, le32(&space, 0x38) as usize);
    let (cp_offset, cp_size) = (le64(&space, 0x40) as usize, le32(&space, 0x48) as usize);
    // Checkpoint areas over the header or the slots are refused before any
    // writer sees them (the guard's check of the regions a header names,
    // since 1.3.1): not a case for the writer.
    let cp_count = journal.checkpoint_geometry().2;
    let slots_end = 0x60 + slot_size.saturating_mul(le32(&space, 0x3c) as usize);
    let cp_end = cp_offset.saturating_add(cp_size.saturating_mul(cp_count as usize));
    if cp_count > 0 && cp_offset < slots_end && cp_end > 0 {
        return;
    }
    // One stripe count for every run (a run has one).
    let stripes = 1 + u64::from(le32(writes, 60) % (1 << 20));
    let step = (stripes / 64).max(1) as usize;
    let runs = [0u64, 1 << 28, 2 << 28, 3 << 28];
    // Which sampled stripes of each run were dirty before.
    let dirty_before: Vec<(u64, u64)> = runs
        .iter()
        .flat_map(|&r| (0..stripes).step_by(step).map(move |s| (r, s)))
        .filter(|&(r, s)| journal.is_dirty(r, s))
        .collect();
    // Each write, and how many slots were written before it: its slots
    // stay while no more than the ring holds were written after them.
    let ring = le32(&space, 0x3c) as usize;
    let mut slots_written = 0usize;
    let mut done: Vec<(u64, u64, u64, usize)> = Vec::new();
    for w in writes.chunks_exact(15).take(4) {
        let run_start = runs[usize::from(w[0] % 4)];
        let first = u64::from(u32::from_le_bytes(w[5..9].try_into().unwrap())) % stripes;
        let count = (1 + u64::from(u16::from_le_bytes([w[9], w[10]]))).min(stripes - first);
        let Ok(records) = writer.write(run_start, stripes, first, count) else {
            return;
        };
        let before = slots_written;
        for record in records {
            let (at, page) = match record {
                LogWrite::Slot(index, page) => {
                    slots_written += 1;
                    (slot_offset.saturating_add(index.saturating_mul(slot_size)), page)
                }
                LogWrite::Checkpoint(area, page) => (cp_offset.saturating_add(area.saturating_mul(cp_size)), page),
            };
            // A page the space has no room for: the model ends there.
            let Some(target) = space.get_mut(at..at.saturating_add(page.len())) else {
                return;
            };
            target.copy_from_slice(&page);
        }
        done.push((run_start, first, count, before));
    }
    let Ok(Some(again)) = load(&space) else {
        return;
    };
    // Never consistent where nothing logged it so: a stripe dirty before
    // and written by none of the writes stays dirty (an entry too long for
    // its slot keeps the newest write's stripes consistent alone, the
    // others' going back to dirty: the safe side).
    for &(r, s) in &dirty_before {
        let written = done
            .iter()
            .any(|&(run, first, count, _)| run == r && (first..first + count).contains(&s));
        if !written {
            assert!(
                again.is_dirty(r, s),
                "run {r:#x} stripe {s}: consistent, written by none"
            );
        }
    }
    // The newest write of each run is there, where its slots stay and can
    // hold any run list four writes make (9 ranges of 33 words at most;
    // where neither the list nor a bitmap fits, the writer records the run
    // not consistent as a whole, the newest write too: the safe side).
    for &r in runs.iter().filter(|_| slot_size >= 0x400) {
        let Some(&(_, first, count, before)) = done.iter().rev().find(|d| d.0 == r) else {
            continue;
        };
        if slots_written - before > ring {
            continue;
        }
        for stripe in [first, first + count / 2, first + count - 1] {
            assert!(
                !again.is_dirty(r, stripe),
                "run {r:#x} stripe {stripe}: the newest write lost"
            );
        }
    }
});
