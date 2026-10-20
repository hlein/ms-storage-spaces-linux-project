//! Checking a volume's consistency (`refs check`): what the write tests
//! check after every change, for any volume.
//!
//! * Pages: every page the checkpoint and the object table reach parses
//!   the way Windows checks pages (its checksum on reading; the row area
//!   row by row up to its end, every key index entry naming a live row,
//!   the free bytes the area less the live rows, the last row of an index
//!   node and only it with row flag 2), and its clusters are used in the
//!   allocator that hands them out.
//! * Tables: no key twice (also across leaves), and the counts of the
//!   table's descriptor (in its root page: 0x18 the pages below the root,
//!   0x20 the leaf rows) those of the table.
//! * Files: every file's clusters (all levels of its streams, named
//!   streams, snapshots, and extent maps kept in pages) are used in the
//!   medium allocator, and a cluster several files map counts the files
//!   beyond the first in the block reference count table; no count there
//!   for a cluster fewer files map.
//! * The pages only the older checkpoint references and that are still
//!   allocated are counted (Windows frees them with its next checkpoint),
//!   and so are the clusters of data and metadata containers that the
//!   allocators mark used and nothing references (leaked: Windows leaves
//!   a few on every volume, so they are counted, not problems).
//! * Compacted (compressed) containers: their compressed clusters are used
//!   in the medium allocator, the clusters files reference there are those
//!   of the container's bitmap of referenced clusters (root 10) and as
//!   many as its row in the container table counts, and those several
//!   files map are counted as shared.

use std::collections::BTreeMap;

use storage_spaces::io::ReadAt;
use storage_spaces::report::hex;

use crate::error::{Result, format_err};
use crate::file::{Content, Target};
use crate::node::Node;
use crate::page::{PAGE_HEADER_SIZE, PageRef};
use crate::refcount;
use crate::util::{le16, le32, le64, utf16};
use crate::volume::{ROOT_DIRECTORY, Volume};

/// Problems listed in a report at most (the count goes on).
const LISTED: usize = 100;

#[derive(Debug, Default)]
pub struct Report {
    pub pages: u64,
    pub directories: u64,
    pub files: u64,
    pub data_clusters: u64,
    /// Clusters of pages only the older checkpoint references, still
    /// allocated (not a problem: Windows frees them with its next
    /// checkpoint, `refs` with its next commit; `refsutil leak` counts
    /// them as leaked meanwhile).
    pub deferred_clusters: u64,
    /// Clusters of data and metadata containers the allocators mark used
    /// that neither checkpoint references (no problem by itself: Windows
    /// leaves some; a change should not add to them).
    pub leaked_clusters: u64,
    /// The first of those runs: (allocator, the container's class, first
    /// cluster, clusters).
    pub leaks: Vec<(usize, u32, u64, u64)>,
    pub problems: Vec<String>,
    /// All problems, also those beyond the listed ones.
    pub problem_count: u64,
}

impl Report {
    fn problem(&mut self, p: String) {
        self.problem_count += 1;
        if self.problems.len() < LISTED {
            self.problems.push(p);
        }
    }
}

/// The descriptor of a table (in its root page) is this long when it
/// counts the table's pages and rows.
const DESCRIPTOR_COUNTS: usize = 0x28;

/// What the check saw of a table: its pages and leaf rows, the counts of
/// its descriptor (pages below the root, rows), the last key of the leaf
/// walked last, and whether a page could not be walked.
#[derive(Default)]
struct Table {
    pages: u64,
    rows: u64,
    descriptor: Option<(u64, u64)>,
    last_key: Option<Vec<u8>>,
    /// A directory's last name (upcased), from the leaf walked last.
    last_name: Option<Vec<u16>>,
    broken: bool,
}

/// Ranges [start, end) sorted, overlapping and adjacent ones joined.
fn merged(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = ranges.iter().copied().filter(|r| r.0 < r.1).collect();
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            Some(l) if s <= l.1 => l.1 = l.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// What two merged lists of ranges share.
fn intersect(a: &[(u64, u64)], b: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() && j < b.len() {
        let (s, e) = (a[i].0.max(b[j].0), a[i].1.min(b[j].1));
        if s < e {
            out.push((s, e));
        }
        if a[i].1 < b[j].1 { i += 1 } else { j += 1 }
    }
    out
}

/// The parts of merged ranges `a` outside merged ranges `b`.
fn subtract(a: &[(u64, u64)], b: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let (mut j, mut out) = (0, Vec::new());
    for &(s, e) in a {
        let mut at = s;
        while j < b.len() && b[j].1 <= at {
            j += 1;
        }
        let mut k = j;
        while at < e {
            match b.get(k) {
                Some(&(bs, be)) if bs < e => {
                    if at < bs {
                        out.push((at, bs));
                    }
                    at = at.max(be);
                    k += 1;
                }
                _ => {
                    out.push((at, e));
                    at = e;
                }
            }
        }
    }
    out
}

/// A stream's references, from its extents as (cluster less place, first
/// place, past the last place): the places of each shift joined, so that
/// views of a stream mapping a cluster at one place (a snapshot and the
/// live data) give it one reference.
pub(crate) fn references(mut placed: Vec<(u64, u64, u64)>) -> Vec<(u64, u64, u64)> {
    placed.sort_unstable();
    let mut out: Vec<(u64, u64, u64)> = Vec::with_capacity(placed.len());
    for (shift, from, to) in placed {
        match out.last_mut() {
            Some(l) if l.0 == shift && from <= l.2 => l.2 = l.2.max(to),
            _ => out.push((shift, from, to)),
        }
    }
    out
}

/// From runs of references (first physical cluster, clusters, file),
/// sorted: the ranges [start, end) of referenced clusters, with how often
/// each is referenced and one file that does.
fn referenced_runs(runs: &[(u64, u64, usize)]) -> Vec<(u64, u64, u32, usize)> {
    let mut events: Vec<(u64, bool, usize)> = Vec::with_capacity(2 * runs.len());
    for &(lcn, n, file) in runs {
        events.push((lcn, true, file));
        events.push((lcn.saturating_add(n), false, file));
    }
    // Ends before starts at one cluster.
    events.sort_unstable_by_key(|e| (e.0, e.1));
    let mut active: BTreeMap<usize, u32> = BTreeMap::new();
    let mut count = 0u32;
    let mut out: Vec<(u64, u64, u32, usize)> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let at = events[i].0;
        while i < events.len() && events[i].0 == at {
            let (_, start, file) = events[i];
            if start {
                *active.entry(file).or_default() += 1;
                count += 1;
            } else if let Some(n) = active.get_mut(&file) {
                *n -= 1;
                count -= 1;
                if *n == 0 {
                    active.remove(&file);
                }
            }
            i += 1;
        }
        let (Some(&(next, _, _)), Some((&file, _))) = (events.get(i), active.first_key_value()) else {
            continue;
        };
        match out.last_mut() {
            Some(l) if l.1 == at && l.2 == count && l.3 == file => l.1 = next,
            _ => out.push((at, next, count, file)),
        }
    }
    out
}

/// The first cluster of [start, end) that no range of `mapped` holds.
fn unmapped(mapped: &[(u64, u64, u32, usize)], start: u64, end: u64) -> Option<u64> {
    let mut at = start;
    let mut i = mapped.partition_point(|m| m.1 <= at);
    while at < end {
        match mapped.get(i) {
            Some(m) if m.0 <= at => {
                at = m.1;
                i += 1;
            }
            _ => return Some(at),
        }
    }
    None
}

/// The reference count of virtual cluster `v` (0 without a row for it):
/// the rows (first, clusters, value) in key order.
fn count_at(rows: &[(u64, u64, Vec<u8>)], v: u64) -> u16 {
    let i = rows.partition_point(|r| r.0 <= v);
    i.checked_sub(1)
        .and_then(|i| refcount::count_of(&rows[i].2, v))
        .unwrap_or(0)
}

/// An allocator row's clusters: a bitmap, or a uniform range (wholly used
/// or not).
enum Bits {
    Map(Vec<u8>),
    Uniform(bool),
}

/// An allocator's rows: start, count, clusters.
pub(crate) struct Allocator(Vec<(u64, u64, Bits)>);

impl Allocator {
    /// Allocator `root`'s rows (the small allocator, 12, is kept at
    /// physical clusters).
    pub(crate) fn read<D: ReadAt>(vol: &Volume<D>, root: usize) -> Result<Self> {
        let mut rows = Vec::new();
        vol.walk(&vol.root(root)?, root == 12, &mut |row| {
            let v = row.value;
            if v.len() >= 0x18 {
                let (start, count) = (le64(v, 0), le64(v, 8));
                if start.checked_add(count).is_none() {
                    return Err(format_err!(
                        "allocator {root}: a row of {count:#x} clusters from {start:#x}"
                    ));
                }
                let bits = match le16(v, 0x12) {
                    1 | 5 | 9 => Bits::Map(v[0x18..].to_vec()),
                    // A uniform range: no free clusters when wholly used
                    // (0xffff when wholly free).
                    _ => Bits::Uniform(le16(v, 0x10) == 0),
                };
                rows.push((start, count, bits));
            }
            Ok(())
        })?;
        // ReFS before 3.14 (3.1, 3.4) leaves a container whose clusters are
        // all used out of the medium allocator, data or metadata; its row
        // in the container table counts its used clusters (u64 at 0x20).
        if root == 1 && vol.legacy_records() {
            let cpc = vol.clusters_per_container;
            let mut full = Vec::new();
            vol.walk(&vol.root(crate::volume::ROOT_CONTAINERS)?, true, &mut |row| {
                let v = row.value;
                if v.len() >= 0x30 && matches!(le32(v, 0x14), 0 | 1) && le64(v, 0x20) == cpc {
                    full.push(le64(v, v.len() - 16));
                }
                Ok(())
            })?;
            for start in full {
                let overlaps = rows
                    .iter()
                    .any(|&(s, n, _)| s < start.saturating_add(cpc) && start < s.saturating_add(n));
                if !overlaps {
                    rows.push((start, cpc, Bits::Uniform(true)));
                }
            }
        }
        rows.sort_by_key(|r| r.0);
        Ok(Allocator(rows))
    }

    /// The ranges [start, end) of the clusters it marks used, merged.
    fn used_runs(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for (start, count, bits) in &self.0 {
            match bits {
                Bits::Uniform(true) => out.push((*start, start.saturating_add(*count))),
                Bits::Uniform(false) => {}
                Bits::Map(b) => {
                    let mut j = 0;
                    while j < *count {
                        let set = |j: u64| {
                            usize::try_from(j / 8)
                                .ok()
                                .and_then(|k| b.get(k))
                                .is_some_and(|byte| byte >> (j % 8) & 1 != 0)
                        };
                        if !set(j) {
                            j += 1;
                            continue;
                        }
                        let from = j;
                        while j < *count && set(j) {
                            j += 1;
                        }
                        out.push((start + from, start + j));
                    }
                }
            }
        }
        merged(&out)
    }

    /// Whether cluster `c` is used.
    pub(crate) fn used(&self, c: u64) -> bool {
        self.first_free(c, 1).is_none()
    }

    /// The first cluster of the `n` from `from` on that is not used, if
    /// any: in steps of whole rows where they are uniform, so that the work
    /// is bounded by the allocator's rows and bitmaps, not by `n`.
    pub(crate) fn first_free(&self, from: u64, n: u64) -> Option<u64> {
        let end = from.saturating_add(n);
        let mut c = from;
        while c < end {
            // The row of `c`: the last that starts at it or before.
            let i = self.0.partition_point(|r| r.0 <= c);
            let Some((start, count, bits)) = i.checked_sub(1).map(|i| &self.0[i]) else {
                return Some(c);
            };
            if c - start >= *count {
                return Some(c);
            }
            let row_end = start.saturating_add(*count).min(end);
            match bits {
                Bits::Uniform(true) => c = row_end,
                Bits::Uniform(false) => return Some(c),
                Bits::Map(b) => {
                    while c < row_end {
                        let j = c - start;
                        if !usize::try_from(j / 8)
                            .ok()
                            .and_then(|k| b.get(k))
                            .is_some_and(|byte| byte >> (j % 8) & 1 != 0)
                        {
                            return Some(c);
                        }
                        c += 1;
                    }
                }
            }
        }
        None
    }
}

/// What is wrong with the attribute rows of a file's record: a key twice
/// (a lookup finds one of the rows; 1.2.1's writer gave a second named
/// stream the stream set of the first, so that both had its rows), a
/// named stream in clusters naming a stream set that has no rows (its
/// extents are gone).
fn record_problems(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut keys: Vec<&[u8]> = rows.iter().map(|(k, _)| k.as_slice()).collect();
    keys.sort_unstable();
    if let Some(w) = keys.windows(2).find(|w| w[0] == w[1]) {
        out.push(format!("two attribute rows with the key {}", hex(w[0], 0x50)));
    }
    let sets: std::collections::HashSet<u64> = rows
        .iter()
        .filter(|(k, _)| k.len() >= 0x38 && le32(k, 8) == 3)
        .map(|(k, _)| le64(k, 0x30))
        .collect();
    for (k, v) in rows {
        // A named stream or a snapshot of one, in clusters: its set at
        // 0x3c (0: the levels of the file's own data).
        let in_clusters = k.len() >= 0x10
            && le32(k, 8) == 0x8000_0002
            && le32(k, 12) & 0xffff == 0xb0
            && v.len() >= 0x44
            && matches!(le16(v, 0x10), 0 | 2)
            && le16(v, 2) & 0x1000 != 0;
        let set = le64(v, 0x3c);
        if in_clusters && set != 0 && !sets.contains(&set) {
            out.push(format!(
                "stream {:?} names stream set {set:#x}, which has no rows",
                utf16(&k[0x10..])
            ));
        }
    }
    out
}

/// A page for the check to read: (its reference, physical, its allocator,
/// what it is of, the level and depth of the node naming it).
type Page = (PageRef, bool, Option<u8>, String, Option<(u8, usize)>);

/// The page structure check: problems found in one page.
fn page_problems(page: &[u8]) -> Option<String> {
    let u32_at = |o: usize| {
        page.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    let h = PAGE_HEADER_SIZE + u32_at(PAGE_HEADER_SIZE)?;
    let (start, end, free, index, count) = (
        u32_at(h)?,
        u32_at(h + 4)?,
        u32_at(h + 8)?,
        u32_at(h + 0x10)?,
        u32_at(h + 0x14)?,
    );
    let mut rows = BTreeMap::new();
    let mut o = start;
    while o < end {
        let size = u32_at(h + o)?;
        if size < 0x10 || !size.is_multiple_of(8) || o + size > end {
            return Some(format!("row at {o:#x} of {size:#x} bytes"));
        }
        rows.insert(o, (size, le16(page, h + o + 8)));
        o += size;
    }
    if o != end {
        return Some(format!("rows end at {o:#x}, the header says {end:#x}"));
    }
    let index_node = page.get(h + 0x0c).is_some_and(|&l| l > 0);
    // Node flag 8: the entries' high halves carry their keys' first u64
    // less the base at 0x18 (where it fits).
    let deltas = page.get(h + 0x0d).is_some_and(|&f| f & 8 != 0);
    let base = page
        .get(h + 0x18..h + 0x20)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
    let mut live = 0;
    for i in 0..count {
        let entry = u32_at(h + index + 4 * i)?;
        let e = entry & 0xffff;
        let Some(&(size, flags)) = rows.get(&e) else {
            return Some(format!("key index entry {i} names no row ({e:#x})"));
        };
        let (ko, kl) = (le16(page, h + e + 4) as usize, le16(page, h + e + 6) as usize);
        let key0 = page
            .get(h + e + ko..h + e + ko + 8)
            .filter(|_| kl >= 8)
            .map(|k| u64::from_le_bytes(k.try_into().unwrap()));
        if deltas
            && let Some(k) = key0
            && k >= base
            && k - base < 0xffff
            && (entry >> 16) as u64 != k - base
        {
            return Some(format!(
                "key index entry {i} carries {:#x}, its key less the base {:#x}",
                entry >> 16,
                k - base
            ));
        }
        if flags & 4 != 0 {
            return Some(format!("key index entry {i} names a removed row"));
        }
        if index_node && (flags & 2 != 0) != (i + 1 == count) {
            return Some(format!("index row {i} of {count} with row flags {flags:#x}"));
        }
        live += size;
    }
    match index.checked_sub(start + live) {
        Some(left) if left == free => None,
        Some(left) => Some(format!("{free:#x} free bytes, the rows leave {left:#x}")),
        // Rows the key index names more than once.
        None => Some(format!(
            "{live:#x} bytes of rows from {start:#x}, past the key index at {index:#x}"
        )),
    }
}

impl<D: ReadAt> Volume<D> {
    /// Checks the volume (see the module); `skip` names objects not to
    /// look into (fixtures that leave some out).
    pub fn check(&self, skip: &[u64]) -> Result<Report> {
        if self.v1 {
            return self.check_v1(skip);
        }
        let mut report = Report::default();
        // What each allocator's used clusters are (medium, container,
        // small): ranges of physical clusters.
        let mut referenced: [Vec<(u64, u64)>; 3] = Default::default();
        let slot = |allocator: usize| match allocator {
            1 => 0,
            2 => 1,
            _ => 2,
        };
        match self.deferred_pages() {
            Ok(d) => {
                report.deferred_clusters = d.iter().map(|(_, l)| l.len() as u64).sum();
                for (allocator, lcns) in d {
                    referenced[slot(allocator)].extend(lcns.iter().map(|&c| (c, c + 1)));
                }
            }
            Err(e) => report.problem(format!("the older checkpoint's pages: {e}")),
        }
        let medium = Allocator::read(self, 1)?;
        let container = Allocator::read(self, 2)?;
        let small = Allocator::read(self, 12)?;
        let per_page = (self.page_size / self.cluster) as usize;
        let mut tables: BTreeMap<String, Table> = BTreeMap::new();
        let mut todo: Vec<Page> = Vec::new();
        for (i, r) in self.checkpoint.roots.iter().enumerate() {
            let (physical, allocator) = match i {
                7 | 8 | 12 => (true, None),
                1 | 2 | 6 | 11 => (false, Some(2)),
                _ => (false, Some(1)),
            };
            todo.push((r.clone(), physical, allocator, format!("root {i}"), None));
        }
        for oid in self.object_ids().collect::<Vec<_>>() {
            if !skip.contains(&oid) {
                todo.push((
                    self.object(oid)?.clone(),
                    false,
                    Some(1),
                    format!("object {oid:#x}"),
                    None,
                ));
            }
        }
        // Each page once: rows naming a page again (or an ancestor) would
        // make the walk read it as often, or forever. Children come off the
        // stack in key order, a table's pages one after the other.
        let mut seen = std::collections::HashSet::new();
        // Directories (objects from the root directory on) keep their
        // names in the order of the upcase table, where Windows looks for
        // them (`Volume::upcase`; none to compare by on ReFS 1.x).
        let up = if self.v1 { None } else { Some(self.upcase()?) };
        let directory = |what: &str| {
            what.strip_prefix("object 0x")
                .and_then(|h| u64::from_str_radix(h, 16).ok())
                .is_some_and(|oid| oid >= crate::volume::ROOT_DIRECTORY)
        };
        while let Some((r, physical, allocator, what, parent)) = todo.pop() {
            let table = tables.entry(what.clone()).or_default();
            if !seen.insert((physical, r.lcns[0])) {
                report.problem(format!("{what}: page {:#x} named twice", r.lcns[0]));
                table.broken = true;
                continue;
            }
            report.pages += 1;
            table.pages += 1;
            let page = match self.read_page(&r, physical) {
                Ok(p) => p,
                Err(e) => {
                    report.problem(format!("{what}: {e}"));
                    table.broken = true;
                    continue;
                }
            };
            for &l in &r.lcns[..per_page] {
                let lcn = if physical { Ok(l) } else { self.translate(l) };
                if let Ok(lcn) = lcn {
                    referenced[allocator.map_or(2, |a| slot(a.into()))].push((lcn, lcn + 1));
                }
                match (lcn, allocator) {
                    (Ok(lcn), Some(1)) if !medium.used(lcn) => {
                        report.problem(format!("{what}: page cluster {lcn:#x} free in the medium allocator"))
                    }
                    (Ok(lcn), Some(2)) if !container.used(lcn) => {
                        report.problem(format!("{what}: page cluster {lcn:#x} free in the container allocator"))
                    }
                    (Err(e), _) => report.problem(format!("{what}: {e}")),
                    _ => {}
                }
            }
            if let Some(p) = page_problems(&page) {
                report.problem(format!("{what}: page {:#x}: {p}", r.lcns[0]));
                table.broken = true;
                continue;
            }
            let node = Node::at(&page, PAGE_HEADER_SIZE)?;
            // The table's descriptor, in its root page.
            if parent.is_none() && le32(&page, PAGE_HEADER_SIZE) as usize >= DESCRIPTOR_COUNTS {
                table.descriptor = Some((
                    le64(&page, PAGE_HEADER_SIZE + 0x18),
                    le64(&page, PAGE_HEADER_SIZE + 0x20),
                ));
            }
            // Each key once (a lookup finds one of two rows with a key),
            // also across the leaves of a table.
            if node.is_leaf() {
                let keys: Vec<&[u8]> = node.rows().flatten().map(|r| r.key).collect();
                if let Some(w) = keys.windows(2).find(|w| w[0] == w[1]) {
                    report.problem(format!(
                        "{what}: page {:#x}: two rows with the key {}",
                        r.lcns[0],
                        hex(w[0], 0x20)
                    ));
                }
                if let (Some(last), Some(first)) = (&table.last_key, keys.first())
                    && last.as_slice() == *first
                {
                    report.problem(format!(
                        "{what}: page {:#x}: the key {} in the leaf before too",
                        r.lcns[0],
                        hex(first, 0x20)
                    ));
                }
                table.rows += keys.len() as u64;
                if let Some(k) = keys.last() {
                    table.last_key = Some(k.to_vec());
                }
                if let Some(up) = up.as_deref().filter(|_| directory(&what)) {
                    let names = keys.iter().filter(|k| k.len() > 4 && le16(k, 0) == 0x30).map(|k| {
                        let units: Vec<u16> = k[4..]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| up[u16::from_le_bytes(*c) as usize])
                            .collect();
                        (units, *k)
                    });
                    for (name, key) in names {
                        if table.last_name.as_ref().is_some_and(|last| *last > name) {
                            report.problem(format!(
                                "{what}: page {:#x}: the name {:?} out of the upcase table's order",
                                r.lcns[0],
                                utf16(&key[4..])
                            ));
                        }
                        table.last_name = Some(name);
                    }
                }
            }
            let depth = match parent {
                Some((level, _)) if node.level.checked_add(1) != Some(level) => {
                    report.problem(format!(
                        "{what}: page {:#x} of level {} below a node of level {level}",
                        r.lcns[0], node.level
                    ));
                    table.broken = true;
                    continue;
                }
                Some((_, depth)) => depth + 1,
                None => 0,
            };
            if !node.is_leaf() && depth >= crate::volume::MAX_DEPTH {
                report.problem(format!(
                    "{what}: B+-tree deeper than {} levels",
                    crate::volume::MAX_DEPTH
                ));
                table.broken = true;
            } else if !node.is_leaf() {
                let mut children = Vec::new();
                for row in node.rows() {
                    match row.and_then(|row| PageRef::parse(row.value)) {
                        Ok(child) => {
                            children.push((child, physical, allocator, what.clone(), Some((node.level, depth))))
                        }
                        Err(e) => {
                            report.problem(format!("{what}: {e}"));
                            table.broken = true;
                        }
                    }
                }
                todo.extend(children.into_iter().rev());
            }
        }
        // ReFS 3.1 counts a row more in the descriptor of root 11 than the
        // table has (on all four volumes of the corpus).
        let v31 = (self.checkpoint.major, self.checkpoint.minor) < (3, 4);
        for (what, t) in &tables {
            let Some((pages, rows)) = t.descriptor.filter(|_| !t.broken && !(v31 && what == "root 11")) else {
                continue;
            };
            if pages != t.pages - 1 {
                report.problem(format!(
                    "{what}: its descriptor counts {pages} pages below the root, the table has {}",
                    t.pages - 1
                ));
            }
            if rows != t.rows {
                report.problem(format!(
                    "{what}: its descriptor counts {rows} rows, the table has {}",
                    t.rows
                ));
            }
        }
        // Files: their clusters, and how often they are referenced: once
        // for each cluster of a stream that maps it (several places of a
        // file can map one cluster, as deduplication leaves them; a
        // stream's snapshots map the clusters the newer levels did not
        // replace, at the same places: those are one reference). Physical
        // runs of references, and in compacted containers references by
        // virtual cluster (the last file, how many). Files by number, in
        // `names`.
        let mut names: Vec<String> = Vec::new();
        let mut runs: Vec<(u64, u64, usize)> = Vec::new();
        let mut compacted: BTreeMap<u64, (usize, u32)> = BTreeMap::new();
        let mut records = std::collections::HashSet::new();
        let mut dirs = vec![(ROOT_DIRECTORY, String::new())];
        while let Some((oid, path)) = dirs.pop() {
            report.directories += 1;
            let entries = match self.read_dir(oid) {
                Ok(e) => e,
                Err(e) => {
                    report.problem(format!("{path}/: {e}"));
                    continue;
                }
            };
            for e in entries {
                let name = format!("{path}/{}", e.name);
                if let Target::Directory(child) = e.target {
                    if e.attributes & 0x400 == 0 && !skip.contains(&child) {
                        dirs.push((child, name));
                    }
                    continue;
                }
                // The names of a moved or linked file share its record.
                if let Target::Split { home, ordinal } = e.target
                    && !records.insert((home, ordinal))
                {
                    continue;
                }
                report.files += 1;
                // Its record's rows first: what they lack makes the file
                // fail to open, and they say why.
                if let Ok(rows) = self.attribute_rows(&e) {
                    for p in record_problems(&rows) {
                        report.problem(format!("{name}: {p}"));
                    }
                }
                let file = match self.open_file(&e) {
                    Ok(f) => f,
                    Err(err) => {
                        report.problem(format!("{name}: {err}"));
                        continue;
                    }
                };
                let me = names.len();
                names.push(name.clone());
                // The data with its snapshots, then each named stream.
                let mut groups: Vec<Vec<&crate::file::Stream>> =
                    vec![file.data.iter().chain(file.snapshots.iter().map(|(_, s)| s)).collect()];
                groups.extend(file.streams.iter().map(|(_, s)| vec![s]));
                for group in groups {
                    // (cluster less place, first place, past the last place).
                    let mut placed: Vec<(u64, u64, u64)> = Vec::new();
                    let mut packed: Vec<(u64, u64, u64)> = Vec::new();
                    for s in group {
                        let Content::Extents(x) = &s.content else { continue };
                        for x in x.iter().filter(|x| x.written) {
                            // In a compacted container: its compressed
                            // clusters stand for them (checked below).
                            if self.is_compacted(x.vlcn) {
                                report.data_clusters = report.data_clusters.saturating_add(x.clusters);
                                let n = x.clusters.min(self.clusters_per_container);
                                packed.push((x.vlcn.wrapping_sub(x.vcn), x.vcn, x.vcn.saturating_add(n)));
                                continue;
                            }
                            match self.translate(x.vlcn) {
                                Ok(lcn) => {
                                    placed.push((lcn.wrapping_sub(x.vcn), x.vcn, x.vcn.saturating_add(x.clusters)))
                                }
                                Err(err) => report.problem(format!("{name}: {err}")),
                            }
                        }
                    }
                    for (shift, from, to) in references(placed) {
                        runs.push((from.wrapping_add(shift), to - from, me));
                    }
                    for (shift, from, to) in references(packed) {
                        for v in from.wrapping_add(shift)..to.wrapping_add(shift) {
                            let e = compacted.entry(v).or_insert((me, 0));
                            *e = (me, e.1 + 1);
                        }
                    }
                }
                if let Ok(record) = self.record(&e) {
                    let pages = self.map_pages_of_record(&record);
                    if !pages.is_empty() {
                        let map = names.len();
                        names.push(format!("{name} (extent map)"));
                        runs.extend(pages.into_iter().map(|page| (page, 1, map)));
                    }
                }
            }
        }
        // The compressed clusters of compacted containers, a container's
        // worth at a time (the run they are in may be any length).
        let cpc = self.clusters_per_container;
        for (id, c) in &self.compacted {
            let mut k = 0;
            while k < c.clusters {
                let Some(v) = c.data.checked_add(k) else {
                    report.problem(format!(
                        "compacted container {id:#x}: {} compressed clusters from {:#x}",
                        c.clusters, c.data
                    ));
                    break;
                };
                let n = (cpc - (v & (cpc - 1))).min(c.clusters - k);
                match self.translate(v).map(|lcn| (lcn, medium.first_free(lcn, n))) {
                    Ok((_, Some(free))) => {
                        report.problem(format!(
                            "compacted container {id:#x}: compressed cluster {free:#x} free in the medium allocator"
                        ));
                        break;
                    }
                    Ok((lcn, None)) => referenced[0].push((lcn, lcn + n)),
                    Err(e) => {
                        report.problem(format!("compacted container {id:#x}: {e}"));
                        break;
                    }
                }
                k += n;
            }
        }
        runs.sort();
        let (refcounts, row_problems) = self.refcount_rows()?;
        for p in row_problems {
            report.problem(p);
        }
        let shift = 64 - self.clusters_per_container.leading_zeros();
        for (id, c) in &self.compacted {
            let first = u128::from(*id) << shift;
            let last = (first + (1u128 << shift) - 1).min(u128::from(u64::MAX));
            let mine = compacted.range(u64::try_from(first).unwrap_or(u64::MAX)..=last as u64);
            let referenced = mine.clone().count() as u64;
            if referenced != u64::from(c.referenced) {
                report.problem(format!(
                    "compacted container {id:#x}: files reference {referenced} of its clusters, its row counts {}",
                    c.referenced
                ));
            }
            // Its bitmap of the clusters files reference (root 10).
            if c.referenced_map.is_empty() {
                report.problem(format!(
                    "compacted container {id:#x}: no bitmap of the clusters files reference"
                ));
            } else if let Some(i) = (0..self.clusters_per_container).find(|&i| {
                let bit = c
                    .referenced_map
                    .get((i / 8) as usize)
                    .is_some_and(|b| b >> (i % 8) & 1 != 0);
                bit != compacted.contains_key(&((first as u64) | i))
            }) {
                report.problem(format!(
                    "compacted container {id:#x}: cluster {i:#x} {}",
                    if compacted.contains_key(&((first as u64) | i)) {
                        "referenced by a file, not in its bitmap of referenced clusters"
                    } else {
                        "in its bitmap of referenced clusters, referenced by no file"
                    }
                ));
            }
            // Its clusters' reference counts: those beyond the first.
            for (&v, &(file, references)) in mine {
                let count = count_at(&refcounts, v);
                if u32::from(count) + 1 != references && (skip.is_empty() || u32::from(count) + 1 < references) {
                    report.problem(format!(
                        "{}: compressed cluster {v:#x} referenced {references} time(s), its reference count {count}",
                        names[file]
                    ));
                    break;
                }
            }
        }
        for &(lcn, n, file) in &runs {
            report.data_clusters = report.data_clusters.saturating_add(n);
            if let Some(c) = medium.first_free(lcn, n) {
                report.problem(format!(
                    "{}: data cluster {c:#x} free in the medium allocator",
                    names[file]
                ));
            }
        }
        // A cluster's reference count counts its references beyond the
        // first; a cluster referenced once or not at all has none. With
        // objects left out (`skip`), their files' references are not seen:
        // only counts too low for what is seen are wrong.
        let mapped = referenced_runs(&runs);
        let mut wrong = 0;
        for &(start, end, references, file) in &mapped {
            if refcounts.is_empty() && references == 1 {
                continue;
            }
            for (from, to, count) in self.counts_over(&refcounts, start, end) {
                let off = u32::from(count) + 1 != references && (skip.is_empty() || u32::from(count) + 1 < references);
                if off && wrong < LISTED {
                    wrong += 1;
                    report.problem(format!(
                        "{}: clusters {from:#x}..{to:#x} referenced {references} time(s), their reference count {count}",
                        names[file]
                    ));
                }
            }
        }
        for (first, n, value) in refcounts.iter().filter(|_| skip.is_empty()) {
            for (v, to, count) in refcount::pieces(value, *first, *n).into_iter().filter(|p| p.2 > 0) {
                if self.is_compacted(v) {
                    continue;
                }
                // The parts no file maps (those files map are checked above).
                let mut at = v;
                while at < to {
                    let Ok(lcn) = self.translate(at) else { break };
                    let len = (cpc - (at & (cpc - 1))).min(to - at);
                    if let Some(c) = unmapped(&mapped, lcn, lcn + len) {
                        report.problem(format!(
                            "reference count {count} for virtual cluster {:#x}, which no file maps",
                            at + (c - lcn)
                        ));
                        break;
                    }
                    at += len;
                }
            }
        }
        // Clusters used that nothing references: data and metadata
        // containers only (the log's and reserved areas are used whole), on
        // the version `refs` writes (older ones keep full containers out of
        // the allocators, and more).
        if self.writable_version().is_err() {
            return Ok(report);
        }
        for &(start, n, _) in &runs {
            referenced[0].push((start, start.saturating_add(n)));
        }
        let counted: Vec<(u64, u64)> = self
            .container_classes
            .iter()
            .filter(|&(_, &class)| matches!(class, 0 | 1 | 0x4000))
            .map(|(&start, _)| (start, start.saturating_add(cpc)))
            .collect();
        for (k, allocator) in [&medium, &container, &small].into_iter().enumerate() {
            let used = intersect(&allocator.used_runs(), &merged(&counted));
            for (start, end) in subtract(&used, &merged(&referenced[k])) {
                report.leaked_clusters += end - start;
                if report.leaks.len() < LISTED {
                    let class = self
                        .container_classes
                        .range(..=start)
                        .next_back()
                        .map_or(u32::MAX, |(_, &c)| c);
                    report.leaks.push(([1, 2, 12][k], class, start, end - start));
                }
            }
        }
        Ok(report)
    }

    /// ReFS 1.x: every page of every table and object (its checksum, its
    /// own block number, its node and rows), every file's record and runs
    /// (inside the volume, and no cluster mapped by two files: 1.x has no
    /// block clones). Its allocators are laid out otherwise and not
    /// checked.
    fn check_v1(&self, skip: &[u64]) -> Result<Report> {
        let mut report = Report::default();
        let mut todo: Vec<(PageRef, String)> = Vec::new();
        for (i, r) in self.checkpoint.roots.iter().enumerate() {
            todo.push((r.clone(), format!("root {i}")));
        }
        for oid in self.object_ids().collect::<Vec<_>>() {
            if !skip.contains(&oid) {
                todo.push((self.object(oid)?.clone(), format!("object {oid:#x}")));
            }
        }
        let mut seen = std::collections::HashSet::new();
        while let Some((r, what)) = todo.pop() {
            if !seen.insert(r.lcns[0]) {
                continue;
            }
            report.pages += 1;
            let page = match self.read_page(&r, false) {
                Ok(p) => p,
                Err(e) => {
                    report.problem(format!("{what}: {e}"));
                    continue;
                }
            };
            let node = match self.node_in(&page, self.node_offset()) {
                Ok(n) => n,
                Err(e) => {
                    report.problem(format!("{what}: page {:#x}: {e}", r.lcns[0]));
                    continue;
                }
            };
            for row in node.rows() {
                match row.and_then(|row| {
                    if node.is_leaf() {
                        Ok(None)
                    } else {
                        self.page_ref(row.value).map(Some)
                    }
                }) {
                    Ok(Some(child)) => todo.push((child, what.clone())),
                    Ok(None) => {}
                    Err(e) => report.problem(format!("{what}: page {:#x}: {e}", r.lcns[0])),
                }
            }
        }
        let total = self.boot.volume_size() / self.cluster;
        let mut runs: Vec<(u64, u64, String)> = Vec::new();
        let mut dirs = vec![(ROOT_DIRECTORY, String::new())];
        while let Some((oid, path)) = dirs.pop() {
            report.directories += 1;
            let entries = match self.read_dir(oid) {
                Ok(e) => e,
                Err(e) => {
                    report.problem(format!("{path}/: {e}"));
                    continue;
                }
            };
            for e in entries {
                let name = format!("{path}/{}", e.name);
                if let Target::Directory(child) = e.target {
                    if e.attributes & 0x400 == 0 && !skip.contains(&child) {
                        dirs.push((child, name));
                    }
                    continue;
                }
                report.files += 1;
                let file = match self.open_file(&e) {
                    Ok(f) => f,
                    Err(err) => {
                        report.problem(format!("{name}: {err}"));
                        continue;
                    }
                };
                let streams = file.data.iter().chain(file.streams.iter().map(|(_, s)| s));
                for s in streams {
                    if let Content::Extents(x) = &s.content {
                        for x in x {
                            report.data_clusters = report.data_clusters.saturating_add(x.clusters);
                            if x.vlcn.checked_add(x.clusters).is_none_or(|end| end > total) {
                                report.problem(format!(
                                    "{name}: a run of {} clusters at {:#x}, past the volume's end",
                                    x.clusters, x.vlcn
                                ));
                            } else {
                                runs.push((x.vlcn, x.clusters, name.clone()));
                            }
                        }
                    }
                }
            }
        }
        runs.sort();
        let mut last: Option<(u64, String)> = None;
        for (lcn, n, name) in runs {
            if let Some((end, other)) = &last
                && lcn < *end
                && *other != name
            {
                report.problem(format!("{name}: cluster {lcn:#x} also mapped by {other}"));
            }
            if last.as_ref().is_none_or(|(end, _)| lcn + n > *end) {
                last = Some((lcn + n, name));
            }
        }
        Ok(report)
    }

    /// The pages of extent maps a record keeps outside it (its levels'
    /// values whose node is an index over pages).
    fn map_pages_of_record(&self, record: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        let Ok(node) = Node::at(record, 0) else {
            return out;
        };
        for row in node.rows().flatten() {
            let k = row.key;
            let level = k.len() >= 0x18
                && le32(k, 8) == 0x8000_0002
                && le32(k, 12) & 0xffff == 0x80
                && le64(k, 0x10) >= crate::file::LIVE_STREAM;
            let set = k.len() >= 0x40 && le32(k, 8) == 3 && le64(k, 0x38) >= crate::file::LIVE_STREAM;
            if (level || set)
                && let Ok(pages) = self.extent_map_pages(row.value)
            {
                out.extend(pages);
            }
        }
        out
    }

    /// The reference counts of the physical clusters [start, end): ranges
    /// of one count (0 where no row counts them), container by container.
    fn counts_over(&self, rows: &[(u64, u64, Vec<u8>)], start: u64, end: u64) -> Vec<(u64, u64, u16)> {
        let cpc = self.clusters_per_container;
        let mut out: Vec<(u64, u64, u16)> = Vec::new();
        let mut push = |from: u64, to: u64, count: u16| match out.last_mut() {
            Some(l) if l.1 == from && l.2 == count => l.1 = to,
            _ => out.push((from, to, count)),
        };
        let mut lcn = start;
        while lcn < end {
            let Ok(v) = self.virtual_of(lcn) else {
                push(lcn, end, 0);
                break;
            };
            let len = (cpc - (v & (cpc - 1))).min(end - lcn);
            // The rows over [v, v + len), and the gaps between them.
            let mut at = v;
            let mut i = rows.partition_point(|r| r.0.saturating_add(r.1) <= v);
            while at < v + len {
                match rows.get(i) {
                    Some((first, n, value)) if *first < v + len => {
                        if at < *first {
                            push(lcn + (at - v), lcn + (first - v), 0);
                            at = *first;
                        }
                        let stop = first.saturating_add(*n).min(v + len);
                        for (from, to, count) in refcount::pieces(value, at, stop - at) {
                            push(lcn + (from - v), lcn + (to - v), count);
                        }
                        at = stop;
                        i += 1;
                    }
                    _ => {
                        push(lcn + (at - v), lcn + len, 0);
                        at = v + len;
                    }
                }
            }
            lcn += len;
        }
        out
    }

    /// The block reference count table's rows (see `refcount`): their
    /// ranges and values, and the problems found in them (a row of counts
    /// too short for its range, an unknown kind).
    #[allow(clippy::type_complexity)]
    fn refcount_rows(&self) -> Result<(Vec<(u64, u64, Vec<u8>)>, Vec<String>)> {
        let (mut rows, mut problems) = (Vec::new(), Vec::new());
        self.walk(&self.root(6)?, false, &mut |row| {
            let Some((first, n)) = refcount::range(row.value) else {
                problems.push(format!("reference count row of {} bytes", row.value.len()));
                return Ok(());
            };
            if first.checked_add(n).is_none() {
                problems.push(format!("reference count row {first:#x} of {n:#x} clusters"));
                return Ok(());
            }
            // A u16 per cluster from 0x1c.
            let short = n
                .checked_mul(2)
                .and_then(|b| b.checked_add(0x1c))
                .is_none_or(|need| (row.value.len() as u64) < need);
            match le32(row.value, 0x14) {
                refcount::COUNTS if short => problems.push(format!(
                    "reference count row {first:#x}: {n:#x} clusters, {} bytes",
                    row.value.len()
                )),
                refcount::COUNTS | refcount::UNIFORM => {}
                kind => problems.push(format!("reference count row {first:#x} of kind {kind}")),
            }
            rows.push((first, n, row.value.to_vec()));
            Ok(())
        })?;
        Ok((rows, problems))
    }
}
