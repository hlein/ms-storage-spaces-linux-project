//! Mapping virtual space offsets to physical slabs.

use std::collections::BTreeMap;

use crate::error::{Error, Result, format_err};
use crate::format::{ExtentRecord, Policy, Resiliency, SLAB_SIZE};
use crate::limits::{MAX_COLUMNS, MAX_COPIES, MAX_GROUPS, MAX_REDUNDANCY, MAX_SLABS, MAX_SPACE_SIZE};

/// Redundancy state of a space or layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    /// Every copy and column is on a present disk.
    Healthy,
    /// Some copies or columns are on missing disks or out of date, but all
    /// data can still be read or rebuilt.
    Degraded,
    /// Some data is only on missing disks.
    Failed,
}

/// What is left of a space's redundancy (see [`Layout::redundancy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Redundancy {
    /// Disk failures still survived; `None` when some data is lost.
    pub left: Option<u64>,
    /// Some copies or columns are on missing disks.
    pub missing: bool,
    /// Some copies are out of date.
    pub stale: bool,
    /// Some rows have fewer current copies than the policy keeps (one being
    /// rebuilt, or none recorded) with every disk at hand.
    pub short: bool,
}

impl Redundancy {
    /// The worse of two parts of one space (its tiers, its cache).
    pub fn and(self, other: Redundancy) -> Redundancy {
        Redundancy {
            left: self.left.min(other.left),
            missing: self.missing || other.missing,
            stale: self.stale || other.stale,
            short: self.short || other.short,
        }
    }
}

/// Out-of-date copies of one column: disjoint ranges of rows, each with
/// the disks of the copies over it, for lookups in log time (a space can
/// have thousands of them, and its health asks of every row and column).
#[derive(Debug, Clone, Default)]
struct Coverage {
    /// Where each range starts; one ends where the next starts, the last
    /// covers no copy.
    starts: Vec<u64>,
    disks: Vec<Vec<u64>>,
}

impl Coverage {
    /// More copies of a row than any column can have are not believed.
    const MAX_DEPTH: usize = 4 * MAX_COPIES as usize;

    fn new(runs: &[Run]) -> Result<Coverage> {
        // Ends sort before starts at the same row.
        let mut events: Vec<(u64, bool, u64)> = runs
            .iter()
            .filter(|r| r.rows > 0)
            .flat_map(|r| [(r.first_row, true, r.disk_id), (r.first_row + r.rows, false, r.disk_id)])
            .collect();
        events.sort_unstable_by_key(|&(row, start, _)| (row, start));
        let mut cover = Coverage::default();
        let mut active: Vec<u64> = Vec::new();
        let mut i = 0;
        while i < events.len() {
            let row = events[i].0;
            while let Some(&(_, start, disk)) = events.get(i).filter(|e| e.0 == row) {
                if start {
                    active.push(disk);
                } else if let Some(p) = active.iter().position(|&d| d == disk) {
                    active.swap_remove(p);
                }
                i += 1;
            }
            if active.len() > Self::MAX_DEPTH {
                return Err(format_err!(
                    "{} out-of-date copies of row {row} of a column: implausible",
                    active.len()
                ));
            }
            cover.starts.push(row);
            cover.disks.push(active.clone());
        }
        Ok(cover)
    }

    /// The disks of the copies over row `row`.
    fn at(&self, row: u64) -> &[u64] {
        match self.starts.partition_point(|&s| s <= row) {
            0 => &[],
            i => &self.disks[i - 1],
        }
    }
}

/// A run of consecutive physical slabs backing consecutive rows of one column copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub first_row: u64,
    pub rows: u64,
    pub disk_id: u64,
    pub physical_slab: u64,
}

/// Where a virtual offset lives within the column layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub column: u64,
    /// Slab row within the column.
    pub row: u64,
    pub offset_in_slab: u64,
    /// Bytes that stay contiguous from this point (up to the interleave boundary).
    pub contiguous: u64,
}

/// Striping layout of a space: data is split into `interleave`-sized units
/// written round-robin across the columns; each column is a sequence of slab
/// rows, each backed by one physical slab per copy.
///
/// Parity spaces keep `r` parity units per stripe in consecutive columns
/// starting at `(C - r - r * s) mod C` for stripe `s`; the data units follow
/// them, wrapping around. For r = 1 this is the left-symmetric RAID-5 layout;
/// the stripe number counts from the first row of the extent run the stripe
/// lies in. `r` is the redundancy (1 or 2), or `groups + 1` for dual parity
/// with several groups (a local reconstruction code). What the parity units
/// hold is described by [`Layout::parity_code`].
#[derive(Debug, Clone)]
pub struct Layout {
    pub resiliency: Resiliency,
    pub columns: u64,
    pub data_columns: u64,
    /// Parity units per stripe (0 for simple and mirror).
    pub parity_units: u64,
    /// Local groups of a dual parity space (1 unless it uses a local
    /// reconstruction code).
    pub groups: u64,
    /// Start of the layout in the owner's address space (storage tiers);
    /// rows and parity rotation count from here.
    pub base: u64,
    pub copies: u64,
    /// The copies of each row the policy keeps (1 for simple and parity).
    pub required: u64,
    pub interleave: u64,
    runs: BTreeMap<(u64, u64), Vec<Run>>,
    /// Each column's runs as one sequence of rows ([`Layout::column_runs`]).
    merged: BTreeMap<u64, Vec<Run>>,
    /// Out-of-date copies per column.
    stale: BTreeMap<u64, Coverage>,
    /// The copies of `stale` with a stale marker (not those being rebuilt).
    outdated: BTreeMap<u64, Coverage>,
    /// The columns with extents, current or out of date, with the copy
    /// numbers of their current ones.
    used: BTreeMap<u64, Vec<u64>>,
}

impl Layout {
    pub fn new(policy: &Policy, extents: &[ExtentRecord]) -> Result<Self> {
        Self::with_base(policy, extents, 0)
    }

    /// A layout that starts at byte `base` of the owner space (a tier).
    pub fn with_base(policy: &Policy, extents: &[ExtentRecord], base: u64) -> Result<Self> {
        if !base.is_multiple_of(SLAB_SIZE) || base > MAX_SPACE_SIZE {
            return Err(format_err!(
                "layout starts at {base:#x}, not on a slab boundary inside 1 EiB"
            ));
        }
        // Numbers far beyond any pool's are not computed with (see
        // `crate::limits`): the space is not read.
        if !(1..=MAX_COLUMNS).contains(&policy.columns)
            || policy.copies > MAX_COPIES
            || policy.groups > MAX_GROUPS
            || policy.redundancy > MAX_REDUNDANCY
        {
            return Err(format_err!(
                "implausible placement policy: {} columns, {} copies, {} groups, redundancy {}",
                policy.columns,
                policy.copies,
                policy.groups,
                policy.redundancy
            ));
        }
        let base_slab = base / SLAB_SIZE;
        let data_columns = match policy.resiliency {
            Resiliency::Simple | Resiliency::Mirror => policy.columns,
            Resiliency::Parity if policy.groups > 1 => {
                // One local parity unit per group and one global one.
                if policy.redundancy != 2 || policy.groups > policy.columns / 2 {
                    return Err(Error::Unsupported(format!(
                        "parity with {} groups, {} columns and redundancy {}",
                        policy.groups, policy.columns, policy.redundancy
                    )));
                }
                policy.columns - policy.groups - 1
            }
            Resiliency::Parity if (1..=2).contains(&policy.redundancy) && policy.columns >= policy.redundancy + 2 => {
                policy.columns - policy.redundancy
            }
            Resiliency::Parity => {
                return Err(Error::Unsupported(format!(
                    "parity with {} columns and redundancy {}",
                    policy.columns, policy.redundancy
                )));
            }
            Resiliency::Other(r) => return Err(Error::Unsupported(format!("resiliency type {r}"))),
        };
        if !SLAB_SIZE.is_multiple_of(policy.interleave) {
            return Err(Error::Unsupported(format!("interleave {:#x}", policy.interleave)));
        }
        let mut runs: BTreeMap<(u64, u64), Vec<Run>> = BTreeMap::new();
        // Out-of-date copies and copies being rebuilt are not read, but
        // remembered so that a row without a current copy is an error.
        let mut stale: BTreeMap<u64, Vec<Run>> = BTreeMap::new();
        let mut outdated: BTreeMap<u64, Vec<Run>> = BTreeMap::new();
        for e in extents {
            // Slab numbers beyond 2^32 (1 EiB) cannot be real; the limit keeps
            // all offset arithmetic far from overflowing. An extent maps one
            // slab at least: one of none would leave its rows unmapped, read
            // as zeros.
            if e.slab_count == 0
                || [e.virtual_slab, e.slab_count, e.physical_slab]
                    .iter()
                    .any(|&n| n > MAX_SLABS)
            {
                return Err(format_err!(
                    "extent with implausible slab numbers: virtual {}, count {}, physical {}",
                    e.virtual_slab,
                    e.slab_count,
                    e.physical_slab
                ));
            }
            // Copy numbers can exceed the policy's while a copy is
            // regenerated, but not by much.
            if e.column >= policy.columns || e.copy >= MAX_COPIES {
                return Err(format_err!(
                    "extent column {} copy {} outside a {}x{} layout",
                    e.column,
                    e.copy,
                    policy.columns,
                    policy.copies
                ));
            }
            let slab = e
                .virtual_slab
                .checked_sub(base_slab)
                .ok_or_else(|| format_err!("extent at virtual slab {} lies before the layout start", e.virtual_slab))?;
            if slab % data_columns != 0 {
                return Err(format_err!(
                    "extent starts at virtual slab {} not aligned to a row",
                    e.virtual_slab
                ));
            }
            let run = Run {
                first_row: slab / data_columns,
                rows: e.slab_count,
                disk_id: e.disk_id,
                physical_slab: e.physical_slab,
            };
            if e.is_current() {
                runs.entry((e.column, e.copy)).or_default().push(run);
            } else {
                stale.entry(e.column).or_default().push(run);
                if e.stale_marker != ExtentRecord::CURRENT {
                    outdated.entry(e.column).or_default().push(run);
                }
            }
        }
        for list in runs.values_mut() {
            list.sort_by_key(|r| r.first_row);
            if list.windows(2).any(|w| w[0].first_row + w[0].rows > w[1].first_row) {
                return Err(format_err!("overlapping extents"));
            }
        }
        // What the extents map must be addressable (rows of at most 2^33).
        let rows = runs.values().flatten().map(|r| r.first_row + r.rows).max().unwrap_or(0);
        let mapped = rows
            .checked_mul(SLAB_SIZE * data_columns)
            .and_then(|b| b.checked_add(base))
            .filter(|&end| end <= MAX_SPACE_SIZE);
        if mapped.is_none() {
            return Err(format_err!(
                "extents reaching row {rows} of {data_columns} slabs from {base:#x}: beyond 1 EiB"
            ));
        }
        let copies = extents
            .iter()
            .map(|e| e.copy + 1)
            .max()
            .unwrap_or(1)
            .max(policy.copies.max(1));
        let mut used: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for &(column, copy) in runs.keys() {
            used.entry(column).or_default().push(copy);
        }
        for &column in stale.keys() {
            used.entry(column).or_default();
        }
        let cover = |m: BTreeMap<u64, Vec<Run>>| -> Result<BTreeMap<u64, Coverage>> {
            m.into_iter().map(|(c, runs)| Ok((c, Coverage::new(&runs)?))).collect()
        };
        let (stale, outdated) = (cover(stale)?, cover(outdated)?);
        let merged = merge_copies(&runs);
        Ok(Layout {
            resiliency: policy.resiliency,
            columns: policy.columns,
            data_columns,
            parity_units: policy.columns - data_columns,
            groups: if policy.resiliency == Resiliency::Parity {
                policy.groups.max(1)
            } else {
                1
            },
            base,
            copies,
            required: match policy.resiliency {
                Resiliency::Mirror => policy.copies.max(1),
                _ => 1,
            },
            interleave: policy.interleave,
            runs,
            merged,
            stale,
            outdated,
            used,
        })
    }

    /// Locates an offset of the owner space; `offset` must be at least `base`.
    pub fn locate(&self, offset: u64) -> Location {
        let offset = offset - self.base;
        let unit = offset / self.interleave;
        let within = offset % self.interleave;
        let stripe = unit / self.data_columns;
        let index = unit % self.data_columns;
        let column = match self.resiliency {
            Resiliency::Parity => {
                (self.parity_column(self.rotation_stripe(stripe)) + self.parity_units + index) % self.columns
            }
            _ => index,
        };
        let column_offset = stripe * self.interleave + within;
        Location {
            column,
            row: column_offset / SLAB_SIZE,
            offset_in_slab: column_offset % SLAB_SIZE,
            contiguous: self.interleave - within,
        }
    }

    /// Column holding the first (XOR) parity unit of a stripe; a second
    /// parity unit follows it.
    pub fn parity_column(&self, stripe: u64) -> u64 {
        let c = self.columns;
        let r = self.parity_units;
        (c - r + c * r - (r * stripe) % c) % c
    }

    /// What the parity units of a stripe hold, in column order: parity unit
    /// `i` is the sum over the data units `k` of `code[i][k] * D_k` in GF(16)
    /// (see `gf16`); a coefficient of 1 is plain XOR. `None` stands for a
    /// parity unit whose code is not known for this width.
    ///
    /// * single parity: P, the XOR of the data units;
    /// * dual parity: P and Q (`gf16::coefficients`);
    /// * dual parity with `g` groups: the data units are split into `g`
    ///   consecutive groups (the first `D mod g` of them one unit larger);
    ///   one local XOR parity per group, then a global one with the
    ///   coefficients 1, 2, 3, ... restarting in every group.
    pub fn parity_code(&self) -> Vec<Option<Vec<u8>>> {
        let d = self.data_columns as usize;
        if self.groups > 1 {
            let g = self.groups as usize;
            let mut code = Vec::with_capacity(g + 1);
            let mut global = Vec::with_capacity(d);
            let mut start = 0;
            for i in 0..g {
                let len = d / g + usize::from(i < d % g);
                let mut row = vec![0u8; d];
                row[start..start + len].fill(1);
                code.push(Some(row));
                global.extend((1..=len).map(|c| c as u8));
                start += len;
            }
            code.push(Some(global));
            return code;
        }
        let mut code = vec![Some(vec![1u8; d])];
        if self.parity_units == 2 {
            code.push(crate::gf16::coefficients(self.data_columns).map(<[u8]>::to_vec));
        }
        code
    }

    /// Stripe number of a location (parity layouts).
    pub fn stripe_of(&self, loc: &Location) -> u64 {
        self.rotation_stripe((loc.row * SLAB_SIZE + loc.offset_in_slab) / self.interleave)
    }

    /// Parity rotation restarts at the first row of every extent run.
    fn rotation_stripe(&self, stripe: u64) -> u64 {
        let per_row = SLAB_SIZE / self.interleave;
        stripe - self.run_first_row(stripe / per_row) * per_row
    }

    /// The extent runs of `column` in row order: those of its first current
    /// copy, and of a later copy for the rows no earlier one maps. A column
    /// of one copy is numbered 0 until Windows moves it to another disk,
    /// which numbers what it moved 1: all of it (**verified**: column 0 of
    /// the parity space of `replacedpar`) or the runs on the disk it
    /// replaced only (**verified**: `replacedrun`, column 0 copy 0 in rows 0
    /// and 1, copy 1 in rows 2 and 3). Parity rotation, the logs' runs and
    /// the writer count from these runs. 1.2.2 looked at copy 0 alone and
    /// could not read the first degraded (audit 2026-10-16, S9); 1.3.0
    /// looked at the first copy present alone and read rows of the second
    /// whose run it did not see with the rotation of row 0, wrong data from
    /// a healthy pool (audit 2026-10-21, SPB-01).
    pub fn column_runs(&self, column: u64) -> Option<&Vec<Run>> {
        self.merged.get(&column)
    }

    /// Where `row` of `column` lives in its first current copy that maps
    /// it: (disk, slab).
    pub fn current(&self, column: u64, row: u64) -> Option<(u64, u64)> {
        self.copies_of(column)
            .into_iter()
            .find_map(|copy| self.physical(column, copy, row))
    }

    /// First row of the extent run holding `row` (0 if unallocated).
    pub fn run_first_row(&self, row: u64) -> u64 {
        self.column_runs(0)
            .and_then(|runs| {
                let i = runs.partition_point(|r| r.first_row + r.rows <= row);
                runs.get(i).filter(|r| r.first_row <= row)
            })
            .map_or(0, |r| r.first_row)
    }

    /// Owner offset where the extent run holding `row` starts.
    pub fn run_start_offset(&self, row: u64) -> u64 {
        self.base + self.run_first_row(row) * SLAB_SIZE * self.data_columns
    }

    /// Owner offset where the extent run holding `row` ends (where the row
    /// ends, if unallocated).
    pub fn run_end_offset(&self, row: u64) -> u64 {
        let end = self
            .column_runs(0)
            .and_then(|runs| {
                let i = runs.partition_point(|r| r.first_row + r.rows <= row);
                runs.get(i).filter(|r| r.first_row <= row)
            })
            .map_or(row + 1, |r| r.first_row + r.rows);
        self.base + end * SLAB_SIZE * self.data_columns
    }

    /// The copy numbers of a column (a moved copy may be numbered beyond the
    /// policy's copies).
    pub fn copies_of(&self, column: u64) -> Vec<u64> {
        self.used.get(&column).cloned().unwrap_or_default()
    }

    /// Physical slab backing a row of a column copy, if allocated.
    pub fn physical(&self, column: u64, copy: u64, row: u64) -> Option<(u64, u64)> {
        let runs = self.runs.get(&(column, copy))?;
        let i = runs.partition_point(|r| r.first_row + r.rows <= row);
        let run = runs.get(i).filter(|r| r.first_row <= row)?;
        Some((run.disk_id, run.physical_slab + (row - run.first_row)))
    }

    /// The first of rows `0..rows` that a column has no extent for,
    /// current or out of date: a row a fixed space maps (a thin one reads
    /// zeros there).
    pub fn unmapped_row(&self, rows: u64) -> Option<u64> {
        (0..self.columns)
            .filter_map(|column| {
                let stale = self.stale.get(&column).into_iter().flat_map(|c| {
                    c.starts
                        .windows(2)
                        .zip(&c.disks)
                        .filter(|(_, disks)| !disks.is_empty())
                        .map(|(w, _)| (w[0], w[1]))
                });
                let mut covered: Vec<(u64, u64)> = self
                    .runs
                    .range((column, 0)..=(column, u64::MAX))
                    .flat_map(|(_, runs)| runs.iter().map(|r| (r.first_row, r.first_row.saturating_add(r.rows))))
                    .chain(stale)
                    .collect();
                covered.sort_unstable();
                let mut next = 0;
                for (start, end) in covered {
                    if start > next {
                        break;
                    }
                    next = next.max(end);
                }
                (next < rows).then_some(next)
            })
            .min()
    }

    /// Whether the row of a column has an out-of-date copy only.
    pub fn has_stale_copy(&self, column: u64, row: u64) -> bool {
        self.stale.get(&column).is_some_and(|c| !c.at(row).is_empty())
    }

    /// The rows where what the extents map changes, in order: the state of
    /// the space is the same from one to the next.
    fn bounds(&self) -> Vec<u64> {
        let mut bounds: Vec<u64> = self
            .runs
            .values()
            .flatten()
            .flat_map(|r| [r.first_row, r.first_row + r.rows])
            .chain(self.stale.values().flat_map(|c| c.starts.iter().copied()))
            .collect();
        bounds.sort_unstable();
        bounds.dedup();
        bounds
    }

    /// How much redundancy is left when only the disks for which `present`
    /// returns true can be read. Rows without any extent (thin provisioning)
    /// do not count.
    pub fn condition(&self, present: impl Fn(u64) -> bool) -> Condition {
        // Disk failures a row survives.
        let tolerance = match self.resiliency {
            Resiliency::Parity if self.groups > 1 => 2,
            Resiliency::Parity => self.parity_units,
            _ => self.copies.saturating_sub(1),
        };
        let mut worst = Condition::Healthy;
        for row in self.bounds() {
            let mut lost_columns = 0;
            let mut reduced = false;
            let mut allocated = false;
            let mut unmapped = 0;
            for column in 0..self.columns {
                let numbers = self.used.get(&column).map_or(&[][..], Vec::as_slice);
                let stale = self.has_stale_copy(column, row);
                let copies: Vec<u64> = numbers
                    .iter()
                    .filter_map(|&copy| self.physical(column, copy, row).map(|(disk, _)| disk))
                    .collect();
                if copies.is_empty() && !stale {
                    unmapped += 1;
                    continue;
                }
                allocated = true;
                let readable = copies.iter().filter(|&&d| present(d)).count();
                if readable == 0 {
                    lost_columns += 1;
                } else if readable < copies.len() || stale || (copies.len() as u64) < self.required {
                    reduced = true;
                }
            }
            if !allocated {
                continue;
            }
            // Windows allocates a row's columns together (thin or fixed): a
            // column without an extent in a row others have is lost (1.3.0
            // took it for unallocated; audit 2026-10-21, SPA-01).
            lost_columns += unmapped;
            let row_condition = match self.resiliency {
                Resiliency::Parity if lost_columns > tolerance => Condition::Failed,
                Resiliency::Parity if lost_columns > 0 || reduced => Condition::Degraded,
                _ if lost_columns > 0 => Condition::Failed,
                _ if reduced => Condition::Degraded,
                _ => Condition::Healthy,
            };
            worst = worst.max(row_condition);
        }
        worst
    }

    /// Disk failures the space still survives with only the disks for
    /// which `present` returns true (`None`: some data is lost), and
    /// whether copies are on missing disks or out of date. Out-of-date
    /// copies on disks for which `evacuating` returns true (present disks
    /// being retired: their copies stay valid until moved, and Windows
    /// counts them) count as current; copies being rebuilt do not count.
    /// Rows without any extent (thin provisioning) do not count.
    pub fn redundancy(&self, present: impl Fn(u64) -> bool, evacuating: impl Fn(u64) -> bool) -> Redundancy {
        let tolerance = match self.resiliency {
            Resiliency::Parity if self.groups > 1 => 2,
            Resiliency::Parity => self.parity_units,
            _ => self.copies.saturating_sub(1),
        };
        let mut out = Redundancy {
            left: Some(tolerance),
            missing: false,
            stale: false,
            short: false,
        };
        for row in self.bounds() {
            let mut lost_columns = 0;
            let mut least_copies = u64::MAX;
            let mut allocated = false;
            let mut unmapped = 0;
            for column in 0..self.columns {
                let numbers = self.used.get(&column).map_or(&[][..], Vec::as_slice);
                let outdated = self.outdated.get(&column).map_or(&[][..], |c| c.at(row));
                let copies: Vec<u64> = numbers
                    .iter()
                    .filter_map(|&copy| self.physical(column, copy, row).map(|(disk, _)| disk))
                    .chain(outdated.iter().copied().filter(|&d| evacuating(d)))
                    .collect();
                if copies.is_empty() && !self.has_stale_copy(column, row) {
                    unmapped += 1;
                    continue;
                }
                allocated = true;
                out.stale |= outdated.iter().any(|&d| !evacuating(d));
                out.short |= (copies.len() as u64) < self.required;
                let readable = copies.iter().filter(|&&d| present(d)).count() as u64;
                out.missing |= readable < copies.len() as u64;
                if readable == 0 {
                    lost_columns += 1;
                }
                least_copies = least_copies.min(readable);
            }
            if !allocated {
                continue;
            }
            // A column without an extent in a row others have is lost (see
            // `condition`).
            if unmapped > 0 {
                lost_columns += unmapped;
                least_copies = 0;
            }
            let left = match self.resiliency {
                Resiliency::Parity => tolerance.checked_sub(lost_columns),
                _ => least_copies.checked_sub(1),
            };
            out.left = out.left.min(left);
        }
        out
    }

    /// Bytes of the owner space the extents cover, up to the end of the
    /// last row (the size of spaces whose record gives none; at most 1 EiB,
    /// as [`Layout::with_base`] checks).
    pub fn mapped_size(&self) -> u64 {
        let rows = self
            .runs
            .values()
            .flatten()
            .map(|r| r.first_row + r.rows)
            .max()
            .unwrap_or(0);
        rows * SLAB_SIZE * self.data_columns
    }

    /// All runs, keyed by (column, copy).
    pub fn runs(&self) -> &BTreeMap<(u64, u64), Vec<Run>> {
        &self.runs
    }
}

/// Each column's runs merged over its copies ([`Layout::column_runs`]): the
/// copies in order, each adding what of its runs lies in rows the earlier
/// ones do not map (whole runs as Windows numbers them; a run that overlaps
/// the rows of an earlier copy, which a mirror's later copies do, adds only
/// the rows outside them, as a run of their own). Each list sorted, its
/// runs apart.
fn merge_copies(runs: &BTreeMap<(u64, u64), Vec<Run>>) -> BTreeMap<u64, Vec<Run>> {
    let mut merged: BTreeMap<u64, Vec<Run>> = BTreeMap::new();
    for (&(column, _), list) in runs {
        let earlier = merged.entry(column).or_default();
        let mut added = Vec::new();
        for r in list {
            let end = r.first_row + r.rows;
            // The rows of `r` that no run of an earlier copy maps.
            let mut row = r.first_row;
            let i = earlier.partition_point(|e| e.first_row + e.rows <= row);
            for e in &earlier[i..] {
                if e.first_row >= end {
                    break;
                }
                if e.first_row > row {
                    added.push(piece(r, row, e.first_row));
                }
                row = row.max(e.first_row + e.rows);
            }
            if row < end {
                added.push(piece(r, row, end));
            }
        }
        earlier.extend(added);
        earlier.sort_by_key(|r| r.first_row);
    }
    merged
}

/// The rows `from..to` of run `r`, as a run.
fn piece(r: &Run, from: u64, to: u64) -> Run {
    Run {
        first_row: from,
        rows: to - from,
        disk_id: r.disk_id,
        physical_slab: r.physical_slab + (from - r.first_row),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(columns: u64, interleave: u64) -> Policy {
        Policy {
            resiliency: Resiliency::Simple,
            redundancy: 0,
            copies: 1,
            groups: 1,
            columns,
            interleave,
        }
    }

    #[test]
    fn stripes_round_robin() {
        let l = Layout::new(&policy(2, 0x10000), &[]).unwrap();
        assert_eq!(l.locate(0x10000).column, 1);
        let loc = l.locate(0x2_0000 + 5);
        assert_eq!((loc.column, loc.row, loc.offset_in_slab), (0, 0, 0x10005));
        let loc = l.locate(2 * SLAB_SIZE + 0x10000);
        assert_eq!((loc.column, loc.row, loc.offset_in_slab), (1, 1, 0));
    }

    #[test]
    fn rotates_parity_left_symmetric() {
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 1,
            copies: 1,
            groups: 1,
            columns: 3,
            interleave: 0x10000,
        };
        let l = Layout::new(&p, &[]).unwrap();
        // Units D0..D5 as observed on a Windows-created 3-column parity space.
        let columns: Vec<u64> = (0..6).map(|u| l.locate(u * 0x10000).column).collect();
        assert_eq!(columns, [0, 1, 2, 0, 1, 2]);
        assert_eq!(l.locate(3 * 0x10000).column, 0);
        assert_eq!(l.locate(2 * 0x10000).column, 2);
        assert_eq!((0..3).map(|s| l.parity_column(s)).collect::<Vec<_>>(), [2, 1, 0]);
        assert_eq!(l.locate(5 * 0x10000).offset_in_slab, 2 * 0x10000);
    }

    #[test]
    fn rotates_dual_parity() {
        // Seven columns, two parity units per stripe, as Windows lays them out.
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 2,
            copies: 1,
            groups: 1,
            columns: 7,
            interleave: 0x10000,
        };
        let l = Layout::new(&p, &[]).unwrap();
        assert_eq!(l.data_columns, 5);
        assert_eq!(
            (0..8).map(|s| l.parity_column(s)).collect::<Vec<_>>(),
            [5, 3, 1, 6, 4, 2, 0, 5]
        );
        // Stripe 1: P in 3, Q in 4, data units 5..9 in columns 5, 6, 0, 1, 2.
        let cols: Vec<u64> = (5..10).map(|u| l.locate(u * 0x10000).column).collect();
        assert_eq!(cols, [5, 6, 0, 1, 2]);
    }

    #[test]
    fn rejects_implausible_values() {
        let lrc = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 2,
            copies: 1,
            groups: u64::MAX,
            columns: 12,
            interleave: 0x10000,
        };
        assert!(Layout::new(&lrc, &[]).is_err());
        let e = ExtentRecord {
            flags: 0,
            stale_marker: 0xffff_ffff,
            space_id: 1,
            virtual_slab: 0,
            column: 0,
            copy: 0,
            slab_count: u64::MAX,
            disk_id: 1,
            physical_slab: 0,
        };
        assert!(Layout::new(&policy(1, 0x10000), &[e]).is_err());
    }

    #[test]
    fn local_reconstruction_code() {
        // Twelve columns in two groups, as measured on a Windows-created
        // space: nine data units, the local parities of units 0-4 and 5-8,
        // then the global parity; three parity units rotating by three.
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 2,
            copies: 1,
            groups: 2,
            columns: 12,
            interleave: 0x10000,
        };
        let l = Layout::new(&p, &[]).unwrap();
        assert_eq!((l.data_columns, l.parity_units), (9, 3));
        assert_eq!((0..5).map(|s| l.parity_column(s)).collect::<Vec<_>>(), [9, 6, 3, 0, 9]);
        let code: Vec<Vec<u8>> = l.parity_code().into_iter().map(Option::unwrap).collect();
        assert_eq!(
            code,
            [
                vec![1, 1, 1, 1, 1, 0, 0, 0, 0],
                vec![0, 0, 0, 0, 0, 1, 1, 1, 1],
                vec![1, 2, 3, 4, 5, 1, 2, 3, 4],
            ]
        );
        // Seventeen columns: three groups of 13 data units.
        let l = Layout::new(
            &Policy {
                groups: 3,
                columns: 17,
                ..p
            },
            &[],
        )
        .unwrap();
        assert_eq!((l.data_columns, l.parity_units), (13, 4));
        assert_eq!(
            l.parity_code()[3].as_deref(),
            Some(&[1, 2, 3, 4, 5, 1, 2, 3, 4, 1, 2, 3, 4][..])
        );
    }

    #[test]
    fn finds_runs() {
        let e = |virtual_slab, slab_count, physical_slab| ExtentRecord {
            flags: 0,
            stale_marker: 0xffff_ffff,
            space_id: 5,
            virtual_slab,
            column: 0,
            copy: 0,
            slab_count,
            disk_id: 1,
            physical_slab,
        };
        let l = Layout::new(&policy(1, 0x40000), &[e(0, 2, 10), e(5, 1, 3)]).unwrap();
        assert_eq!(l.physical(0, 0, 1), Some((1, 11)));
        assert_eq!(l.physical(0, 0, 2), None);
        assert_eq!(l.physical(0, 0, 5), Some((1, 3)));
    }

    /// Single parity of three columns, a run per row: column 0 of row 0 at
    /// copy 0, of row 1 at copy 1 (as Windows leaves a thin parity space
    /// whose disk under row 1's column 0 it replaced: `replacedrun`).
    /// Column 0's runs are both rows', row 1 starts a run of its own and its
    /// first stripe rotates from 0, unit 0 in column 0. 1.3.0 saw copy 0
    /// alone: row 1 rotated from row 0, unit 0 in column 2 (audit
    /// 2026-10-21, SPB-01; the auditor's case).
    #[test]
    fn a_column_moved_for_some_rows_keeps_their_runs() {
        let p = Policy {
            resiliency: Resiliency::Parity,
            redundancy: 1,
            copies: 1,
            groups: 1,
            columns: 3,
            interleave: 256 << 10,
        };
        let e = |virtual_slab, column, copy, disk_id, physical_slab| ExtentRecord {
            flags: 0,
            stale_marker: ExtentRecord::CURRENT,
            space_id: 1,
            virtual_slab,
            column,
            copy,
            slab_count: 1,
            disk_id,
            physical_slab,
        };
        let extents = [
            e(0, 0, 0, 1, 0),
            e(0, 1, 0, 2, 0),
            e(0, 2, 0, 3, 0),
            e(2, 0, 1, 4, 0),
            e(2, 1, 0, 2, 1),
            e(2, 2, 0, 3, 1),
        ];
        let l = Layout::new(&p, &extents).unwrap();
        let runs: Vec<(u64, u64)> = l
            .column_runs(0)
            .unwrap()
            .iter()
            .map(|r| (r.first_row, r.disk_id))
            .collect();
        assert_eq!(runs, [(0, 1), (1, 4)]);
        assert_eq!((l.run_first_row(1), l.run_start_offset(1)), (1, 2 * SLAB_SIZE));
        let loc = l.locate(2 * SLAB_SIZE);
        assert_eq!((loc.row, l.stripe_of(&loc), loc.column), (1, 0, 0));
        assert_eq!(l.current(0, 1), Some((4, 0)));
    }

    /// A mirror whose second copy maps a row the first does not: the row is
    /// in column 0's runs, the rows both copies map once, the first copy's.
    #[test]
    fn copies_are_merged_without_doubling_rows() {
        let p = Policy {
            resiliency: Resiliency::Mirror,
            redundancy: 1,
            copies: 2,
            groups: 1,
            columns: 1,
            interleave: 256 << 10,
        };
        let e = |virtual_slab, copy, slab_count, disk_id, physical_slab| ExtentRecord {
            flags: 0,
            stale_marker: ExtentRecord::CURRENT,
            space_id: 1,
            virtual_slab,
            column: 0,
            copy,
            slab_count,
            disk_id,
            physical_slab,
        };
        let l = Layout::new(&p, &[e(0, 0, 2, 1, 5), e(0, 1, 3, 2, 7)]).unwrap();
        let runs: Vec<(u64, u64, u64, u64)> = l
            .column_runs(0)
            .unwrap()
            .iter()
            .map(|r| (r.first_row, r.rows, r.disk_id, r.physical_slab))
            .collect();
        assert_eq!(runs, [(0, 2, 1, 5), (2, 1, 2, 9)]);
    }
}
