//! The write-back cache's writer continuing a fuzzed cache (audit
//! 2026-10-21, plan item 3.14): the input is the cache space from its
//! start, read as cache_index reads it; an index that loads is continued
//! (`CacheIndex::writer`) with writes the input's tail names, put where its
//! header puts slots and checkpoints, and loaded again: what loads must map
//! the sectors written where the writer put them. The writer must take
//! memory and time by the log, not by what the log names (1.3.0 expanded
//! every cached chunk into a flag per sector).
#![no_main]

use libfuzzer_sys::fuzz_target;
use storage_spaces::cache::{CacheHeader, CacheIndex, LogWrite, Lookup};

const OWNER: [u8; 16] = [7; 16];

fn set_crc(b: &mut [u8], field: usize) {
    b[field..field + 4].fill(0);
    let crc = crc32fast::hash(b);
    b[field..field + 4].copy_from_slice(&crc.to_le_bytes());
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fuzz_target!(|data: &[u8]| {
    if data.len() < CacheHeader::SIZE + 64 {
        return;
    }
    // The last 64 bytes name the writes, the rest is the cache space.
    let (cache_bytes, writes) = data.split_at(data.len() - 64);
    let mut space = cache_bytes.to_vec();
    space[0..8].copy_from_slice(b"SPCACHE\0");
    space[8..24].copy_from_slice(&OWNER);
    space[0x1c..0x20].copy_from_slice(&(CacheHeader::SIZE as u32).to_le_bytes());
    let slot_size = 0x40 + (le32(&space, 0x38) % 0x1000);
    let slot_count = le32(&space, 0x3c) % 64;
    space[0x30..0x38].copy_from_slice(&(CacheHeader::SIZE as u64).to_le_bytes());
    space[0x38..0x3c].copy_from_slice(&slot_size.to_le_bytes());
    space[0x3c..0x40].copy_from_slice(&slot_count.to_le_bytes());
    let slots_end = (CacheHeader::SIZE + (slot_size * slot_count) as usize).min(space.len());
    for slot in space[CacheHeader::SIZE..slots_end].chunks_exact_mut(slot_size as usize) {
        if slot[0] & 1 == 1 {
            slot[0..8].copy_from_slice(b"SPSLOT\0\0");
            slot[8..24].copy_from_slice(&OWNER);
            slot[0x1c..0x20].copy_from_slice(&slot_size.to_le_bytes());
            set_crc(slot, 0x24);
        }
    }
    let cp_size = 0x200 + (le32(&space, 0x48) % 0x2000);
    space[0x40..0x48].copy_from_slice(&(slots_end as u64).to_le_bytes());
    space[0x48..0x4c].copy_from_slice(&cp_size.to_le_bytes());
    space[0x4c..0x50].copy_from_slice(&2u32.to_le_bytes());
    set_crc(&mut space[..CacheHeader::SIZE], 0x24);
    let Ok(Some(header)) = CacheHeader::parse(&space) else {
        return;
    };
    let sector = if writes[0] & 1 == 0 { 512 } else { 4096 };
    let load = |space: &[u8]| {
        CacheIndex::load(header.clone(), sector, |offset: u64, buf: &mut [u8]| {
            buf.fill(0);
            if let Some(src) = usize::try_from(offset).ok().and_then(|o| space.get(o..)) {
                let n = src.len().min(buf.len());
                buf[..n].copy_from_slice(&src[..n]);
            }
            Ok(())
        })
    };
    let Ok(index) = load(&space) else {
        return;
    };
    let mut writer = index.writer(u32::from(writes[1]));
    let (slot_offset, slot_size) = (header.slot_offset as usize, header.slot_size as usize);
    let (cp_offset, cp_size) = (header.checkpoint_offset as usize, header.checkpoint_size as usize);
    let span = u64::from(header.chunk_size).saturating_mul(4).max(u64::from(sector));
    let mut written: Vec<u64> = Vec::new();
    for w in writes[2..].chunks_exact(6).take(8) {
        let offset = u64::from(le32(w, 0)) % (span / u64::from(sector)) * u64::from(sector);
        let len = (u64::from(u16::from_le_bytes([w[4], w[5]]) % 64) + 1) * u64::from(sector);
        if writer.is_full_for(offset, len) {
            break;
        }
        for record in writer.write(offset, len) {
            let (at, page) = match record {
                LogWrite::Slot(index, page) => (slot_offset.saturating_add(index.saturating_mul(slot_size)), page),
                LogWrite::Checkpoint(area, page) => (cp_offset.saturating_add(area.saturating_mul(cp_size)), page),
            };
            // A page the space has no room for: the model ends there.
            let Some(target) = space.get_mut(at..at.saturating_add(page.len())) else {
                return;
            };
            target.copy_from_slice(&page);
        }
        written.extend([offset, offset + len - u64::from(sector)]);
    }
    // What loads again maps the sectors written where the writer did.
    if let Ok(again) = load(&space) {
        for &offset in &written {
            match (again.lookup(offset), writer.lookup(offset)) {
                (Lookup::Hit { cache_offset, .. }, Some(expected)) => assert_eq!(cache_offset, expected, "{offset:#x}"),
                (Lookup::Miss { .. }, None) => {}
                (loaded, model) => panic!("{offset:#x}: loaded {loaded:?}, writer {model:?}"),
            }
        }
    }
});
