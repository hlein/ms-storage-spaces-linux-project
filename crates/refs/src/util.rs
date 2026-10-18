//! Little-endian field access that never panics on short input (the
//! callers check lengths first; a short buffer reads as zeros).

pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2)
        .map_or(0, |s| u16::from_le_bytes(s.try_into().unwrap()))
}

pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

pub(crate) fn le64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// UTF-16LE text, lossily.
pub(crate) fn utf16(b: &[u8]) -> String {
    utf16_exact(b).0
}

/// UTF-16LE text, and its units when the text cannot hold them (lone
/// surrogates, which Windows allows in names; each is U+FFFD in the text).
pub(crate) fn utf16_exact(b: &[u8]) -> (String, Option<Vec<u16>>) {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
    match String::from_utf16(&units) {
        Ok(s) => (s, None),
        Err(_) => (String::from_utf16_lossy(&units), Some(units)),
    }
}
