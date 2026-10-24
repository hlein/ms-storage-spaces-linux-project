//! Describing a space as linear/striped segments over member devices.
//!
//! This is the form device-mapper tables (`linear`, `striped`, `zero`) need,
//! which lets the kernel serve a space without a userspace daemon. Only
//! simple and mirror spaces whose write-back cache holds no data can be
//! described this way; mirrors are mapped to one present copy per row.

use crate::error::{Error, Result, format_err};
use crate::format::{Resiliency, SLAB_SIZE};
use crate::io::ReadAt;
use crate::reader::SpaceReader;

/// Bytes of each copy of the listed mirror runs [`SpaceReader::segments`]
/// compares before mapping them: the guard's read budget.
const LISTED_BUDGET: u64 = 1 << 30;

/// One column of a striped segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stripe {
    /// Index into the device list the pool was opened with.
    pub device: usize,
    /// Byte offset on that device.
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentKind {
    /// Unallocated range that reads as zeros.
    Zero,
    /// Data striped over the columns in `chunk`-byte units (one stripe = linear).
    Striped { chunk: u64, stripes: Vec<Stripe> },
}

/// A byte range of the space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub length: u64,
    pub kind: SegmentKind,
}

impl<D: ReadAt> SpaceReader<'_, D> {
    /// Describes the space as segments, or explains why it cannot be.
    pub fn segments(&self) -> Result<Vec<Segment>> {
        let segments = self.mapping()?;
        // A segment maps one copy, where reads compare the copies of the
        // runs the dirty region log lists: those are compared first, as the
        // guard does, and a difference, or more than the read budget, is
        // refused (audit 2026-10-21, SPB-06: 1.3.0 mapped one copy of runs
        // whose copies differed). Windows lists the runs written since the
        // space was last disconnected, so most of its mirrors list some.
        if let Some(c) = self.check_dirty_regions(Some(LISTED_BUDGET))? {
            if let Some(at) = c.differ.first() {
                return Err(Error::Pool(format!(
                    "the copies of an extent run the dirty region log lists differ at {at:#x}: one copy cannot be \
                     mapped"
                )));
            }
            if c.incomplete {
                return Err(Error::Unsupported(format!(
                    "the extent runs the dirty region log lists exceed {} MiB: their copies were not all compared \
                     (the other backends compare them when read)",
                    LISTED_BUDGET >> 20
                )));
            }
        }
        Ok(segments)
    }

    /// The segments [`SpaceReader::segments`] maps, without comparing the
    /// copies of the runs a mirror's dirty region log lists first: for
    /// callers that compared them (the guard does, within its budget).
    pub fn mapping(&self) -> Result<Vec<Segment>> {
        let l = self.layout();
        if self.is_tiered() {
            return Err(Error::Unsupported("tiered spaces have no linear mapping yet".into()));
        }
        if !matches!(l.resiliency, Resiliency::Simple | Resiliency::Mirror) {
            return Err(Error::Unsupported(format!(
                "{:?} spaces have no linear mapping",
                l.resiliency
            )));
        }
        if self.cache().is_some_and(|c| c.cached_chunks() > 0) {
            return Err(Error::Unsupported("the space has data in its write-back cache".into()));
        }
        let row_size = SLAB_SIZE * l.columns;
        if !self.size().is_multiple_of(l.interleave * l.columns) {
            return Err(format_err!(
                "space size {:#x} is not a whole number of stripes",
                self.size()
            ));
        }
        let rows = self.size().div_ceil(row_size);
        // Where what maps a column changes, out-of-date copies included:
        // between these rows every row is mapped as the first, a slab
        // further on, and makes one segment (a space of 1 EiB has 2^32 rows,
        // its runs are what count).
        let mut bounds: Vec<u64> = l.bounds().into_iter().filter(|&b| b < rows).chain([0, rows]).collect();
        bounds.sort_unstable();
        bounds.dedup();
        let mut out: Vec<Segment> = Vec::new();
        for stretch in bounds.windows(2) {
            let (row, next) = (stretch[0], stretch[1]);
            let start = row * row_size;
            let length = (next * row_size).min(self.size()) - start;
            let mut stripes = Vec::with_capacity(l.columns as usize);
            let mut allocated = 0;
            for column in 0..l.columns {
                let mut found = None;
                for copy in 0..l.copies {
                    if let Some((disk, slab)) = l.physical(column, copy, row) {
                        allocated += 1;
                        if let Some(location) = self.pool().slab_location(disk, slab)? {
                            found = Some(location);
                            break;
                        }
                    }
                }
                if let Some((device, offset)) = found {
                    stripes.push(Stripe { device, offset });
                } else if l.has_stale_copy(column, row) {
                    // Reads of it fail; mapped as zeros it would not
                    // (audit 2026-10-21, SPB-06).
                    return Err(Error::Pool(format!(
                        "column {column} row {row} has only an out-of-date copy (its disk missed writes)"
                    )));
                }
            }
            let kind = if allocated == 0 {
                SegmentKind::Zero
            } else if stripes.len() as u64 == l.columns {
                SegmentKind::Striped {
                    chunk: l.interleave,
                    stripes,
                }
            } else if allocated < l.columns {
                return Err(format_err!("row {row} is only partly allocated"));
            } else {
                return Err(Error::Pool(format!("row {row} has a column without a present copy")));
            };
            if let Some(prev) = out.last_mut()
                && continues(prev, &kind)
            {
                prev.length += length;
                continue;
            }
            out.push(Segment { start, length, kind });
        }
        Ok(out)
    }
}

/// Whether `next` (one row) directly continues `prev` on the same devices.
fn continues(prev: &Segment, next: &SegmentKind) -> bool {
    match (&prev.kind, next) {
        (SegmentKind::Zero, SegmentKind::Zero) => true,
        (SegmentKind::Striped { chunk: a, stripes: p }, SegmentKind::Striped { chunk: b, stripes: n }) => {
            let per_column = prev.length / p.len() as u64;
            a == b
                && p.len() == n.len()
                && p.iter()
                    .zip(n)
                    .all(|(p, n)| p.device == n.device && p.offset + per_column == n.offset)
        }
        _ => false,
    }
}
