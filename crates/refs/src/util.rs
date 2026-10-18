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

/// A set of clusters kept as ranges [start, end), joined where they meet
/// (the clusters a transaction frees: a file of 100 GiB is one range, not
/// 26 million entries).
#[derive(Debug, Clone, Default)]
pub(crate) struct Ranges(std::collections::BTreeMap<u64, u64>);

impl Ranges {
    /// Adds [start, end).
    pub(crate) fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let (mut start, mut end) = (start, end);
        // A range that ends at or after `start` and begins at or before `end`
        // joins it.
        let first = self
            .0
            .range(..=start)
            .next_back()
            .filter(|(_, e)| **e >= start)
            .map(|(&s, _)| s);
        if let Some(s) = first {
            start = s;
        }
        let joined: Vec<(u64, u64)> = self.0.range(start..=end).map(|(&s, &e)| (s, e)).collect();
        for (s, e) in joined {
            end = end.max(e);
            self.0.remove(&s);
        }
        self.0.insert(start, end);
    }

    pub(crate) fn contains(&self, c: u64) -> bool {
        self.0.range(..=c).next_back().is_some_and(|(_, &e)| c < e)
    }

    pub(crate) fn extend(&mut self, other: &Ranges) {
        for (&s, &e) in &other.0 {
            self.insert(s, e);
        }
    }

    pub(crate) fn extend_clusters(&mut self, clusters: impl IntoIterator<Item = u64>) {
        for c in clusters {
            self.insert(c, c + 1);
        }
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.0.iter().map(|(&s, &e)| (s, e))
    }
}

/// Clusters as runs (first, clusters), sorted and joined.
pub(crate) fn runs_of(clusters: &[u64]) -> Vec<(u64, u64)> {
    let mut sorted = clusters.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for c in sorted {
        match out.last_mut() {
            Some(l) if l.0 + l.1 == c => l.1 += 1,
            _ => out.push((c, 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_join_and_answer() {
        let mut r = Ranges::default();
        r.insert(10, 20);
        r.insert(30, 40);
        r.insert(20, 25);
        r.insert(5, 11);
        r.insert(24, 31);
        assert_eq!(r.iter().collect::<Vec<_>>(), [(5, 40)]);
        r.insert(50, 51);
        assert!(r.contains(5) && r.contains(39) && !r.contains(40) && r.contains(50) && !r.contains(4));
        assert_eq!(runs_of(&[7, 3, 4, 5, 9, 4]), [(3, 3), (7, 1), (9, 1)]);
    }
}
