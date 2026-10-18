//! A ReFS volume mounted through FUSE: read-only, or with `--rw` for
//! writing through `refs::write`.
//!
//! Inodes stand for paths (the write API takes paths), except that the
//! names of a file whose record is kept in its home directory (moved, or
//! hard-linked) share one inode. Symbolic links and junctions are
//! links: a relative target keeps its path, an absolute one
//! ("\\??\\E:\\dir") becomes the path inside the mount. Named streams are
//! extended attributes "user.<name>".
//!
//! Writing: every change is one `refs::write` transaction (copy on write,
//! then a new checkpoint). A file opened for writing is read into memory
//! and written back whole when it is flushed (closed or synced), up to the
//! size `refs write` takes; the listings and records read before are
//! dropped after every change.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fuser::{
    BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo,
    LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use refs::{Entry, File as RefsFile, Target, Times, Volume};
use storage_spaces::io::WriteAt;

/// The device of a mounted volume (written only when mounted with --rw).
pub type Rw = Box<dyn WriteAt>;

const TTL: Duration = Duration::from_secs(1);

/// The largest file the mount writes (a file is rewritten whole; the
/// volume may take less: its extent map must fit one page).
const MAX_WRITTEN: u64 = 64 << 30;

/// Linux's open(2) and setxattr(2) flags.
const O_TRUNC: i32 = 0o1000;
const XATTR_CREATE: i32 = 1;
const XATTR_REPLACE: i32 = 2;

/// A directory's entries, read once: a lookup or a listing in a large
/// directory would otherwise read the whole directory again.
struct Dir {
    entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

/// Directories kept read (the cache is emptied when full), and the entries
/// they may hold together (a hostile directory can have millions).
const DIRECTORIES_KEPT: usize = 256;
const DIR_ENTRIES_KEPT: usize = 200_000;
/// Files whose decoded record is kept (each holds its whole extent list).
const FILES_KEPT: usize = 1024;
/// Inodes kept that the kernel has not looked up (a listing makes them; the
/// kernel looks them up one by one, or never, and tells us when it forgets
/// the ones it did).
const INODES_KEPT: usize = 1 << 16;

/// The guard of a lock whose holder panicked: one failing handler must not
/// make every later one fail with it (the data is consistent: handlers
/// only change it in single steps).
trait Unpoison<T> {
    type Guard<'a>
    where
        Self: 'a;
    fn lock_ok(&self) -> Self::Guard<'_>;
}
impl<T> Unpoison<T> for Mutex<T> {
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        Self: 'a;
    fn lock_ok(&self) -> Self::Guard<'_> {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
trait UnpoisonRw<T> {
    fn read_ok(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_ok(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}
impl<T> UnpoisonRw<T> for RwLock<T> {
    fn read_ok(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn write_ok(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A file opened for writing: its whole content in a temporary file
/// (unlinked; in $REFS_TMPDIR, else the system's), written back on flush.
struct Pending {
    file: std::fs::File,
    len: u64,
    dirty: bool,
    handles: usize,
    /// The byte ranges written (refs::write changes them in place), or
    /// `whole`: the file is written anew.
    ranges: Vec<(u64, u64)>,
    whole: bool,
}

impl Pending {
    fn new() -> Result<Self, Errno> {
        let file = crate::temp_file().map_err(|e| {
            eprintln!("a buffer for a file open for writing: {e}");
            Errno::EIO
        })?;
        Ok(Pending {
            file,
            len: 0,
            dirty: false,
            handles: 1,
            ranges: Vec::new(),
            whole: false,
        })
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        std::os::unix::fs::FileExt::write_all_at(&self.file, data, offset).map_err(|_| Errno::EIO)?;
        self.len = self.len.max(offset + data.len() as u64);
        let (a, b) = (offset, offset + data.len() as u64);
        match self.ranges.last_mut() {
            Some(last) if a <= last.1 && b >= last.0 => *last = (last.0.min(a), last.1.max(b)),
            _ => self.ranges.push((a, b)),
        }
        Ok(())
    }

    /// Shortens or extends the buffer. What a shortening drops is changed
    /// too: grown again before it is written back, the file reads zeros
    /// there, not what the volume held (1.2.2 recorded nothing, and the
    /// old bytes came back; audit M16). Growing needs no range:
    /// `update_file` takes what lies past the file's end as written.
    fn set_len(&mut self, len: u64) -> Result<(), Errno> {
        self.file.set_len(len).map_err(|_| Errno::EIO)?;
        if len < self.len {
            self.ranges.push((len, self.len));
        }
        self.len = len;
        Ok(())
    }

    fn read_at(&self, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let from = offset.min(self.len);
        let to = (from + u64::from(size)).min(self.len);
        let mut buf = vec![0u8; (to - from) as usize];
        std::os::unix::fs::FileExt::read_exact_at(&self.file, &mut buf, from).map_err(|_| Errno::EIO)?;
        Ok(buf)
    }
}

/// A pending file as the data `refs::write` writes from.
struct PendingData<'a>(&'a std::fs::File, u64);

impl refs::write::Source for PendingData<'_> {
    fn len(&self) -> u64 {
        self.1
    }
    fn read_into(&self, offset: u64, buf: &mut [u8]) -> refs::Result<()> {
        Ok(std::os::unix::fs::FileExt::read_exact_at(self.0, buf, offset)?)
    }
}

struct State {
    /// The paths of each inode (the first is used).
    paths: HashMap<u64, Vec<String>>,
    inodes: HashMap<String, u64>,
    /// Inodes of files whose record is kept in their home directory, by
    /// (home, ordinal): all their names.
    records: HashMap<(u64, u64), u64>,
    next: u64,
    dirs: HashMap<u64, Arc<Dir>>,
    /// The entries the cached listings hold together.
    dir_entries: usize,
    files: HashMap<String, Arc<RefsFile>>,
    pending: HashMap<u64, Pending>,
    /// The handles open for writing: the inode each holds the buffer of.
    writers: HashMap<u64, u64>,
    /// The handles open for reading only: their inode.
    readers: HashMap<u64, u64>,
    /// Files whose last name went while they were open: their attributes
    /// then. Their buffer holds their content for their handles until the
    /// last closes, and nothing is written back (1.2.2 failed their reads
    /// and fstat, and their close with ENOENT, and kept the buffer).
    orphans: HashMap<u64, FileAttr>,
    /// Open directories: the listing each was opened on, which its readdir
    /// calls page through (their offsets count its entries, so that what a
    /// change removes or adds meanwhile moves none; 1.2.2 counted the
    /// entries the directory had at each call, and entries were skipped
    /// when names went while it was read).
    dir_handles: HashMap<u64, Arc<Dir>>,
    next_handle: u64,
    /// The lookups the kernel holds per inode (`forget` gives them back).
    lookups: HashMap<u64, u64>,
}

impl State {
    fn new() -> Self {
        let mut s = State {
            paths: HashMap::new(),
            inodes: HashMap::new(),
            records: HashMap::new(),
            next: 2,
            dirs: HashMap::new(),
            dir_entries: 0,
            files: HashMap::new(),
            pending: HashMap::new(),
            writers: HashMap::new(),
            readers: HashMap::new(),
            orphans: HashMap::new(),
            dir_handles: HashMap::new(),
            next_handle: 1,
            lookups: HashMap::new(),
        };
        s.paths.insert(INodeNo::ROOT.0, vec!["/".to_owned()]);
        s.inodes.insert("/".to_owned(), INodeNo::ROOT.0);
        s
    }

    /// A new handle open for writing inode `ino` (handles for reading are
    /// 0: they hold no buffer).
    fn writer(&mut self, ino: u64) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.writers.insert(fh, ino);
        fh
    }

    /// A new handle open for reading inode `ino` (it holds no buffer).
    fn reader(&mut self, ino: u64) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.readers.insert(fh, ino);
        fh
    }

    /// Whether inode `ino` is open, for reading or writing.
    fn is_open(&self, ino: u64) -> bool {
        self.readers.values().chain(self.writers.values()).any(|&i| i == ino)
    }

    /// A new handle of an open directory, its listing `dir`.
    fn dir_handle(&mut self, dir: Arc<Dir>) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.dir_handles.insert(fh, dir);
        fh
    }

    /// The inode of a path, made on first sight; the names of one record
    /// (`record`: its home and ordinal) share it.
    fn inode(&mut self, path: &str, record: Option<(u64, u64)>) -> u64 {
        if let Some(&ino) = self.inodes.get(path) {
            if let Some(r) = record {
                self.records.entry(r).or_insert(ino);
            }
            return ino;
        }
        // Those the kernel holds stay: trimming waits for as many idle ones
        // as are kept (1.2.2 trimmed at every new inode once the held ones
        // alone were more: 1.6 ms each with 100 000 held; audit M17).
        if self.paths.len() > INODES_KEPT + self.lookups.len() {
            self.trim();
        }
        let ino = match record.and_then(|r| self.records.get(&r).copied()) {
            Some(ino) => ino,
            None => {
                let ino = self.next;
                self.next += 1;
                if let Some(r) = record {
                    self.records.insert(r, ino);
                }
                ino
            }
        };
        self.inodes.insert(path.to_owned(), ino);
        self.paths.entry(ino).or_default().push(path.to_owned());
        ino
    }

    /// The kernel was told about the inode (a reply with an entry).
    fn looked_up(&mut self, ino: u64) {
        *self.lookups.entry(ino).or_default() += 1;
    }

    /// The kernel gives `n` lookups back; with none left it forgets the
    /// inode, and so do we (the root stays).
    fn forget(&mut self, ino: u64, n: u64) {
        let Some(held) = self.lookups.get_mut(&ino) else {
            return;
        };
        *held = held.saturating_sub(n);
        if *held == 0 {
            self.lookups.remove(&ino);
            self.drop_inode(ino);
        }
    }

    fn drop_inode(&mut self, ino: u64) {
        if ino == INodeNo::ROOT.0 || self.pending.contains_key(&ino) {
            return;
        }
        for name in self.paths.remove(&ino).unwrap_or_default() {
            self.inodes.remove(&name);
        }
        self.records.retain(|_, i| *i != ino);
    }

    /// Drops the inodes the kernel never looked up (a later lookup makes
    /// them again, under a new number the kernel has not seen).
    fn trim(&mut self) {
        let idle: Vec<u64> = self
            .paths
            .keys()
            .copied()
            .filter(|i| !self.lookups.contains_key(i))
            .collect();
        for ino in idle {
            self.drop_inode(ino);
        }
    }

    /// Keeps a listing, within the budget of entries; one larger than the
    /// budget alone (every lookup there would read it all again: 1.2.2 kept
    /// none, and each name in a directory of 200 000 cost a reading of it;
    /// audit M17), until another is kept.
    fn cache_dir(&mut self, oid: u64, d: Arc<Dir>) {
        let n = d.entries.len();
        if self.dirs.len() >= DIRECTORIES_KEPT || self.dir_entries + n > DIR_ENTRIES_KEPT {
            self.dirs.clear();
            self.dir_entries = 0;
        }
        if let Some(old) = self.dirs.insert(oid, d) {
            self.dir_entries = self.dir_entries.saturating_sub(old.entries.len());
        }
        self.dir_entries += n;
    }

    fn cache_file(&mut self, path: String, f: Arc<RefsFile>) {
        if self.files.len() >= FILES_KEPT {
            self.files.clear();
        }
        self.files.insert(path, f);
    }
}

struct RefsFs {
    vol: RwLock<Volume<Rw>>,
    writable: bool,
    mountpoint: PathBuf,
    owner: (u32, u32),
    state: Mutex<State>,
    /// A change stopped on a panic: the volume in memory may be half
    /// changed, and nothing is served any more.
    broken: AtomicBool,
}

/// FILETIME to SystemTime.
fn system_time(t: u64) -> SystemTime {
    let unix_100ns = t.saturating_sub(116_444_736_000_000_000);
    UNIX_EPOCH + Duration::from_nanos(unix_100ns.saturating_mul(100))
}

/// SystemTime to FILETIME.
fn filetime(t: SystemTime) -> u64 {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since.as_nanos() / 100) as u64 + 116_444_736_000_000_000
}

fn now() -> u64 {
    filetime(SystemTime::now())
}

/// A name as the mount keeps it in its paths: the volume's name, with each
/// lone surrogate (a UTF-16 unit UTF-8 cannot carry, which Windows allows
/// in names) as `\uXXXX`. No name on the volume has a backslash, so these
/// stand for nothing else; one from the kernel is kept as `\\`. (1.2.2 had
/// U+FFFD for each, and two names differing only there were one: one file
/// unreachable, listed twice.)
fn internal_name(e: &Entry) -> String {
    let Some(units) = &e.units else {
        return e.name.clone();
    };
    char::decode_utf16(units.iter().copied())
        .map(|r| match r {
            Ok(c) => c.to_string(),
            Err(lone) => format!("\\u{:04x}", lone.unpaired_surrogate()),
        })
        .collect()
}

/// A name from the kernel as the mount keeps it (see `internal_name`): its
/// bytes UTF-8, a lone surrogate as UTF-8 would have it (WTF-8: three
/// bytes, 0xed 0xa0..0xbf ..) escaped; other bytes are refused.
fn kernel_name(name: &OsStr) -> Result<String, Errno> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut at = 0;
    loop {
        let (valid, rest) = match std::str::from_utf8(&bytes[at..]) {
            Ok(text) => (text, None),
            Err(e) => (
                std::str::from_utf8(&bytes[at..at + e.valid_up_to()]).unwrap_or_default(),
                Some(at + e.valid_up_to()),
            ),
        };
        out.push_str(&valid.replace('\\', "\\\\"));
        let Some(bad) = rest else {
            return Ok(out);
        };
        match bytes.get(bad..bad + 3) {
            Some(&[0xed, b1 @ 0xa0..=0xbf, b2 @ 0x80..=0xbf]) => {
                let unit = 0xd000 | u16::from(b1 & 0x3f) << 6 | u16::from(b2 & 0x3f);
                out.push_str(&format!("\\u{unit:04x}"));
                at = bad + 3;
            }
            _ => return Err(Errno::EINVAL),
        }
    }
}

/// A name the mount keeps (see `internal_name`) as the kernel takes it:
/// lone surrogates as WTF-8, `\\` as a backslash.
fn os_name(internal: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    if !internal.contains('\\') {
        return internal.into();
    }
    let mut out: Vec<u8> = Vec::with_capacity(internal.len());
    let mut rest = internal;
    while let Some(i) = rest.find('\\') {
        out.extend(&rest.as_bytes()[..i]);
        let after = &rest[i + 1..];
        if let Some(r) = after.strip_prefix('\\') {
            out.push(b'\\');
            rest = r;
        } else if let Some(unit) = after
            .strip_prefix('u')
            .and_then(|h| h.get(..4))
            .and_then(|h| u16::from_str_radix(h, 16).ok())
        {
            out.extend([
                0xe0 | (unit >> 12) as u8,
                0x80 | (unit >> 6 & 0x3f) as u8,
                0x80 | (unit & 0x3f) as u8,
            ]);
            rest = &after[5..];
        } else {
            out.push(b'\\');
            rest = after;
        }
    }
    out.extend(rest.as_bytes());
    std::ffi::OsString::from_vec(out)
}

fn child(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn split(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((parent, name)) => (parent, name),
        None => ("/", path),
    }
}

/// A library error as an errno (the message goes to stderr).
fn errno(what: &str, e: refs::Error) -> Errno {
    eprintln!("{what}: {e}");
    match e {
        refs::Error::NotFound(_) => Errno::ENOENT,
        refs::Error::Unsupported(m) if m.contains("not empty") => Errno::ENOTEMPTY,
        refs::Error::Unsupported(m) if m.contains("exists") => Errno::EEXIST,
        refs::Error::Unsupported(_) => Errno::EOPNOTSUPP,
        // 1.2.2 said "Operation not supported".
        refs::Error::Full(_) => Errno::ENOSPC,
        _ => Errno::EIO,
    }
}

impl RefsFs {
    /// Serves one request (`f` holds its reply). A panic while serving it
    /// answers it with EIO (fuser answers a reply dropped unsent so) and
    /// the mount goes on: unguarded, it ends the thread serving it, and the
    /// mount ends with its last thread (4 read-only, 1 with --rw). Once a
    /// change stopped on a panic nothing is served: `f` is dropped unrun.
    fn guard(&self, what: &str, f: impl FnOnce()) {
        if self.broken.load(Ordering::SeqCst) {
            return;
        }
        if let Err(e) = storage_spaces::isolate::caught(f) {
            eprintln!("{what}: {e}");
        }
    }

    /// The inode of a path (`entry`: what it names, if known), made on
    /// first sight; the names of one record share it.
    fn inode(&self, path: &str, entry: Option<&Entry>) -> u64 {
        let record = match entry.map(|e| &e.target) {
            Some(Target::Split { home, ordinal }) => Some((*home, *ordinal)),
            _ => None,
        };
        self.state.lock_ok().inode(path, record)
    }

    fn path(&self, ino: u64) -> Result<String, Errno> {
        self.state
            .lock_ok()
            .paths
            .get(&ino)
            .and_then(|p| p.first().cloned())
            .ok_or(Errno::ENOENT)
    }

    /// Forgets what was read before a change (listings and records), and
    /// the paths below `gone` when it went away.
    fn changed(&self, gone: Option<&str>) {
        let mut s = self.state.lock_ok();
        s.dirs.clear();
        s.dir_entries = 0;
        s.files.clear();
        if let Some(gone) = gone {
            let below = format!("{gone}/");
            let lost: Vec<(String, u64)> = s
                .inodes
                .iter()
                .filter(|(p, _)| *p == gone || p.starts_with(&below))
                .map(|(p, &i)| (p.clone(), i))
                .collect();
            for (p, i) in lost {
                s.inodes.remove(&p);
                let empty = s.paths.get_mut(&i).is_some_and(|names| {
                    names.retain(|n| *n != p);
                    names.is_empty()
                });
                if empty {
                    s.paths.remove(&i);
                    s.records.retain(|_, ino| *ino != i);
                }
            }
        }
    }

    /// Moves the paths of `from` (and below it) to `to`.
    fn moved(&self, from: &str, to: &str) {
        self.changed(Some(to));
        let mut s = self.state.lock_ok();
        let below = format!("{from}/");
        let moved: Vec<(String, u64)> = s
            .inodes
            .iter()
            .filter(|(p, _)| *p == from || p.starts_with(&below))
            .map(|(p, &i)| (p.clone(), i))
            .collect();
        for (p, i) in moved {
            let new = format!("{to}{}", &p[from.len()..]);
            s.inodes.remove(&p);
            s.inodes.insert(new.clone(), i);
            if let Some(names) = s.paths.get_mut(&i) {
                for n in names.iter_mut().filter(|n| **n == p) {
                    n.clone_from(&new);
                }
            }
        }
    }

    /// The entries of directory `oid` (cached).
    fn listing(&self, vol: &Volume<Rw>, oid: u64) -> Result<Arc<Dir>, Errno> {
        if let Some(d) = self.state.lock_ok().dirs.get(&oid) {
            return Ok(d.clone());
        }
        let entries = vol
            .read_dir(oid)
            .map_err(|e| errno(&format!("directory {oid:#x}"), e))?;
        let by_name = entries.iter().enumerate().map(|(i, e)| (internal_name(e), i)).collect();
        let d = Arc::new(Dir { entries, by_name });
        self.state.lock_ok().cache_dir(oid, d.clone());
        Ok(d)
    }

    /// The entry of a path (None: the root).
    fn resolve(&self, vol: &Volume<Rw>, path: &str) -> Result<Option<Entry>, Errno> {
        let mut oid = refs::volume::ROOT_DIRECTORY;
        let mut found = None;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if found.is_some()
                && !matches!(
                    found,
                    Some(Entry {
                        target: Target::Directory(_),
                        ..
                    })
                )
            {
                return Err(Errno::ENOTDIR);
            }
            let listing = self.listing(vol, oid)?;
            let e = listing
                .by_name
                .get(part)
                .map(|&i| &listing.entries[i])
                .or_else(|| {
                    listing
                        .entries
                        .iter()
                        .find(|e| internal_name(e).eq_ignore_ascii_case(part))
                })
                .ok_or(Errno::ENOENT)?
                .clone();
            if let Target::Directory(child) = e.target {
                oid = child;
            }
            found = Some(e);
        }
        Ok(found)
    }

    /// The directory object a path names.
    fn directory(&self, vol: &Volume<Rw>, path: &str) -> Result<u64, Errno> {
        match self.resolve(vol, path)? {
            None => Ok(refs::volume::ROOT_DIRECTORY),
            Some(Entry {
                target: Target::Directory(oid),
                ..
            }) => Ok(oid),
            Some(_) => Err(Errno::ENOTDIR),
        }
    }

    /// The decoded record of a file (cached).
    fn file(&self, vol: &Volume<Rw>, path: &str) -> Result<Arc<RefsFile>, Errno> {
        if let Some(f) = self.state.lock_ok().files.get(path) {
            return Ok(f.clone());
        }
        let entry = self.resolve(vol, path)?.ok_or(Errno::EISDIR)?;
        let file = Arc::new(vol.open_file(&entry).map_err(|e| errno(path, e))?);
        self.state.lock_ok().cache_file(path.to_owned(), file.clone());
        Ok(file)
    }

    fn is_link(e: &Entry) -> bool {
        e.attributes & refs::file::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    fn attr(&self, vol: &Volume<Rw>, ino: u64) -> Result<FileAttr, Errno> {
        {
            let s = self.state.lock_ok();
            if let Some(&a) = s.orphans.get(&ino) {
                let size = s.pending.get(&ino).map_or(a.size, |p| p.len);
                return Ok(FileAttr {
                    size,
                    blocks: size.div_ceil(vol.cluster) * vol.cluster / 512,
                    nlink: 0,
                    ..a
                });
            }
        }
        let path = self.path(ino)?;
        let mut entry = self.resolve(vol, &path)?;
        // A moved or linked file: its record has the current sizes and
        // times (the entry of a name is updated only when written through).
        let mut links = 1;
        if let Some(e) = entry.as_mut()
            && matches!(e.target, Target::Split { .. })
            && let Ok(r) = vol.record(e)
            && r.len() >= 0xa0
        {
            let u64_at = |at: usize| u64::from_le_bytes(r[at..at + 8].try_into().unwrap());
            e.times = Times {
                created: u64_at(0x28),
                modified: u64_at(0x30),
                changed: u64_at(0x38),
                accessed: u64_at(0x40),
            };
            e.size = u64_at(0x58);
            e.allocated = u64_at(0x60);
            e.attributes = (e.attributes & !0xffff) | u32::from_le_bytes(r[0x48..0x4c].try_into().unwrap()) & 0xffff;
            links = u32::from_le_bytes(r[0x98..0x9c].try_into().unwrap()).max(1);
        }
        let (rw, ro) = if self.writable { (0o200, 0o555) } else { (0, 0o555) };
        let (kind, mut size, perm, times) = match &entry {
            None => (
                FileType::Directory,
                0,
                ro | rw,
                vol.directory_times(refs::volume::ROOT_DIRECTORY).unwrap_or_default(),
            ),
            Some(e) if Self::is_link(e) => (
                FileType::Symlink,
                self.link(vol, &path).map_or(0, |l| l.len() as u64),
                0o777,
                e.times,
            ),
            Some(e) if e.is_dir() => (FileType::Directory, 0, ro | rw, e.times),
            Some(e) => {
                let writable = if e.attributes & 1 == 0 { rw } else { 0 };
                (FileType::RegularFile, e.size, 0o444 | writable, e.times)
            }
        };
        // What it has on the volume (a sparse file less than its size; 1.2.2
        // counted the size), or, written to, its size in whole clusters.
        let mut allocated = match &entry {
            Some(e) if kind == FileType::RegularFile => e.allocated,
            _ => 0,
        };
        if let Some(p) = self.state.lock_ok().pending.get(&ino) {
            size = p.len;
            allocated = size.div_ceil(vol.cluster) * vol.cluster;
        }
        let (uid, gid) = if self.writable { self.owner } else { (0, 0) };
        Ok(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: allocated.div_ceil(512),
            atime: system_time(times.accessed),
            mtime: system_time(times.modified),
            ctime: system_time(times.changed),
            crtime: system_time(times.created),
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { links },
            uid,
            gid,
            rdev: 0,
            flags: 0,
            blksize: vol.cluster as u32,
        })
    }

    /// Where a link points, as a Linux path.
    fn link(&self, vol: &Volume<Rw>, path: &str) -> Option<String> {
        let target = self.file(vol, path).ok()?.reparse.as_ref()?.link_target()?;
        let path = target.substitute.replace('\\', "/");
        if target.relative {
            return Some(path);
        }
        // "\??\E:\dir": the path inside this volume.
        let inside = path.trim_start_matches("/??/");
        let inside = match inside.as_bytes() {
            [_, b':', ..] => &inside[2..],
            _ => inside,
        };
        Some(format!(
            "{}/{}",
            self.mountpoint.display(),
            inside.trim_start_matches('/')
        ))
    }

    /// A file's whole content, in a pending buffer.
    fn content(&self, vol: &Volume<Rw>, path: &str) -> Result<Pending, Errno> {
        let mut p = Pending::new()?;
        let file = self.file(vol, path)?;
        let Some(stream) = &file.data else {
            return Ok(p);
        };
        if stream.size > MAX_WRITTEN {
            return Err(Errno::EFBIG);
        }
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0;
        while at < stream.size {
            let n = vol.read_stream(stream, at, &mut buf).map_err(|e| errno(path, e))?;
            if n == 0 {
                break;
            }
            p.write_at(at, &buf[..n])?;
            at += n as u64;
        }
        p.set_len(stream.size)?;
        // What it holds is the file's own: nothing written yet.
        p.ranges.clear();
        Ok(p)
    }

    /// Runs a change on the volume (refused unless mounted for writing).
    /// A panic inside it leaves the volume in memory as far as it got, and
    /// a later change could write that: the mount stops serving.
    fn change<T>(&self, what: &str, f: impl FnOnce(&mut Volume<Rw>) -> refs::Result<T>) -> Result<T, Errno> {
        if !self.writable {
            return Err(Errno::EROFS);
        }
        // A name with a lone surrogate (or a backslash) is read, not changed:
        // the library takes names as UTF-8.
        if what.contains('\\') {
            eprintln!(
                "{}: names with a lone surrogate or a backslash are not changed",
                os_name(what).to_string_lossy()
            );
            return Err(Errno::EACCES);
        }
        let mut vol = self.vol.write_ok();
        let r = storage_spaces::isolate::caught(|| f(&mut vol));
        drop(vol);
        self.changed(None);
        match r {
            Ok(r) => r.map_err(|e| errno(what, e)),
            Err(e) => {
                self.broken.store(true, Ordering::SeqCst);
                eprintln!("{what}: the change {e}; nothing more is read or written: unmount the volume");
                Err(Errno::EIO)
            }
        }
    }

    /// Writes back a file opened for writing, if it changed: in place
    /// what was written (`update_file`), or the file anew. It stays
    /// changed until that is done, so that every later flush tries again
    /// and reports the error (1.2.1 forgot the changes at the first try).
    fn write_back(&self, ino: u64) -> Result<(), Errno> {
        let (file, len, ranges, whole) = {
            let mut s = self.state.lock_ok();
            // A file without a name: nothing to write it to.
            if s.orphans.contains_key(&ino) {
                if let Some(p) = s.pending.get_mut(&ino) {
                    p.dirty = false;
                }
                return Ok(());
            }
            match s.pending.get(&ino) {
                Some(p) if p.dirty => (
                    p.file.try_clone().map_err(|_| Errno::EIO)?,
                    p.len,
                    p.ranges.clone(),
                    p.whole,
                ),
                _ => return Ok(()),
            }
        };
        let path = self.path(ino)?;
        let source = PendingData(&file, len);
        if whole {
            self.change(&path, |v| v.write_file_from(&path, &source, now()))?;
        } else {
            self.change(&path, |v| v.update_file(&path, &source, &ranges, now()))?;
        }
        // Written (a mount for writing has one thread: nothing was written
        // meanwhile).
        if let Some(p) = self.state.lock_ok().pending.get_mut(&ino) {
            p.dirty = false;
            p.ranges.clear();
            p.whole = false;
        }
        Ok(())
    }

    /// Opens inode `ino` with open(2)'s `flags`: for writing, its content
    /// in a buffer its writers share. The handle.
    fn open_inode(&self, ino: u64, flags: i32) -> Result<u64, Errno> {
        let path = self.path(ino)?;
        let vol = self.vol.read_ok();
        self.file(&vol, &path)?;
        if flags & 3 == 0 {
            return Ok(self.state.lock_ok().reader(ino));
        }
        if !self.writable {
            return Err(Errno::EROFS);
        }
        if path.contains('\\') {
            return Err(Errno::EACCES);
        }
        let truncate = flags & O_TRUNC != 0;
        let mut s = self.state.lock_ok();
        if let Some(p) = s.pending.get_mut(&ino) {
            if truncate {
                p.set_len(0)?;
                p.dirty = true;
                p.whole = true;
            }
            p.handles += 1;
            return Ok(s.writer(ino));
        }
        drop(s);
        let mut p = if truncate {
            Pending::new()?
        } else {
            self.content(&vol, &path)?
        };
        p.dirty = truncate;
        p.whole = truncate;
        let mut s = self.state.lock_ok();
        s.pending.insert(ino, p);
        Ok(s.writer(ino))
    }

    /// Writes `data` at `offset` into the buffer of inode `ino`.
    fn write_inode(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let mut s = self.state.lock_ok();
        let p = s.pending.get_mut(&ino).ok_or(Errno::EBADF)?;
        if offset.saturating_add(data.len() as u64) > MAX_WRITTEN {
            return Err(Errno::EFBIG);
        }
        p.write_at(offset, data)?;
        p.dirty = true;
        Ok(data.len() as u32)
    }

    /// Closes handle `fh` of inode `ino`: what was written goes to the
    /// volume, and the buffer goes with the last handle open for writing
    /// once it is written (a handle for reading holds none: 1.2.1 counted
    /// its close against the writers, and a writer still open then wrote
    /// into nothing, EBADF).
    fn release_inode(&self, ino: u64, fh: u64) -> Result<(), Errno> {
        let r = self.write_back(ino);
        let mut s = self.state.lock_ok();
        s.readers.remove(&fh);
        if s.writers.remove(&fh).is_some()
            && let Some(p) = s.pending.get_mut(&ino)
        {
            p.handles = p.handles.saturating_sub(1);
        }
        if s.orphans.contains_key(&ino) {
            // Its buffer serves its readers too, until the last handle.
            if !s.is_open(ino) {
                s.pending.remove(&ino);
                s.orphans.remove(&ino);
            }
        } else if s.pending.get(&ino).is_some_and(|p| p.handles == 0 && !p.dirty) {
            s.pending.remove(&ino);
        }
        r
    }

    /// Reads `size` bytes from `offset` of inode `ino`: from its buffer,
    /// else from the volume.
    fn read_inode(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        if let Some(p) = self.state.lock_ok().pending.get(&ino) {
            return p.read_at(offset, size);
        }
        let vol = self.vol.read_ok();
        let file = self.path(ino).and_then(|p| self.file(&vol, &p))?;
        let Some(stream) = &file.data else {
            return Ok(Vec::new());
        };
        let mut buf = vec![0u8; size as usize];
        let n = vol.read_stream(stream, offset, &mut buf).map_err(|e| {
            eprintln!("read of {size} bytes at {offset:#x} failed: {e}");
            Errno::EIO
        })?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Before name `path` goes: when it is the last name of a file that is
    /// open, its content in a buffer (unless it has one), the file's
    /// attributes, and whether the buffer was made for this. None for a
    /// file not open, or with names left.
    fn keep_open(&self, path: &str) -> Option<(u64, FileAttr, bool)> {
        let ino = *self.state.lock_ok().inodes.get(path)?;
        if !self.state.lock_ok().is_open(ino) {
            return None;
        }
        let vol = self.vol.read_ok();
        let entry = self.resolve(&vol, path).ok()??;
        if entry.is_dir() {
            return None;
        }
        let names = match entry.target {
            Target::Split { .. } => vol
                .record(&entry)
                .ok()
                .filter(|r| r.len() >= 0x9c)
                .map_or(1, |r| u32::from_le_bytes(r[0x98..0x9c].try_into().unwrap())),
            _ => 1,
        };
        if names > 1 {
            return None;
        }
        let attr = self.attr(&vol, ino).ok()?;
        let made = !self.state.lock_ok().pending.contains_key(&ino);
        if made {
            match self.content(&vol, path) {
                Ok(mut p) => {
                    p.handles = 0;
                    self.state.lock_ok().pending.insert(ino, p);
                }
                Err(e) => eprintln!("{path}: open, its content not kept: {e:?}"),
            }
        }
        Some((ino, attr, made))
    }

    /// After the change `keep_open` prepared for: the file has no name now
    /// (`gone`), or, the change refused, the buffer made for it goes.
    fn settle_open(&self, kept: Option<(u64, FileAttr, bool)>, gone: bool) {
        let Some((ino, attr, made)) = kept else {
            return;
        };
        let mut s = self.state.lock_ok();
        if gone {
            s.orphans.insert(ino, attr);
        } else if made {
            s.pending.remove(&ino);
        }
    }

    /// Removes name `path` (unlink, rmdir); the last name of a file that is
    /// open leaves it to its handles (see `orphans`).
    fn unlink_path(&self, path: &str) -> Result<(), Errno> {
        let kept = self.keep_open(path);
        let r = self.change(path, |v| v.delete_file(path, now()));
        self.settle_open(kept, r.is_ok());
        r?;
        self.changed(Some(path));
        Ok(())
    }

    /// Renames `from` to `to` (paths), replacing what `to` names unless
    /// `flags` say otherwise, in one change (`Volume::rename_replacing`):
    /// a crash leaves the target or the renamed file, never neither (1.2.2
    /// deleted the target in one change and renamed in the next).
    fn rename_paths(&self, from: &str, to: &str, flags: RenameFlags) -> Result<(), Errno> {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return Err(Errno::EINVAL);
        }
        let exists = match self.resolve(&self.vol.read_ok(), to) {
            Err(Errno::ENOENT) => None,
            r => r?,
        };
        if let Some(target) = exists {
            if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                return Err(Errno::EEXIST);
            }
            // The name only changes case: Windows' names ignore case.
            if !from.eq_ignore_ascii_case(to) {
                let source = self.resolve(&self.vol.read_ok(), from)?.ok_or(Errno::EBUSY)?;
                if source.is_dir() != target.is_dir() {
                    return Err(if target.is_dir() { Errno::EISDIR } else { Errno::ENOTDIR });
                }
            }
            let kept = if from.eq_ignore_ascii_case(to) {
                None
            } else {
                self.keep_open(to)
            };
            let r = self.change(from, |v| v.rename_replacing(from, to, now()));
            self.settle_open(kept, r.is_ok());
            r?;
        } else {
            let (from_dir, _) = split(from);
            let (to_dir, to_name) = split(to);
            if from_dir == to_dir {
                self.change(from, |v| v.rename(from, to_name, now()))?;
            } else {
                self.change(from, |v| v.move_file(from, to, now()))?;
            }
        }
        self.moved(from, to);
        Ok(())
    }

    /// Opens directory `ino`: a handle on its listing now.
    fn open_dir(&self, ino: u64) -> Result<u64, Errno> {
        let vol = self.vol.read_ok();
        let path = self.path(ino)?;
        let listing = self.directory(&vol, &path).and_then(|d| self.listing(&vol, d))?;
        Ok(self.state.lock_ok().dir_handle(listing))
    }

    /// The entries of directory `ino` from `offset` on (".", "..", then
    /// its names; each with the offset of the next): of the listing handle
    /// `fh` was opened on, else of the listing now.
    fn dir_page(&self, ino: u64, fh: u64, offset: u64) -> Result<Vec<(u64, FileType, String, u64)>, Errno> {
        let vol = self.vol.read_ok();
        let path = self.path(ino)?;
        let opened = self.state.lock_ok().dir_handles.get(&fh).cloned();
        let listing = match opened {
            Some(l) => l,
            None => self.directory(&vol, &path).and_then(|d| self.listing(&vol, d))?,
        };
        let mut page = Vec::new();
        for (i, name) in [".", ".."].into_iter().enumerate().skip(offset as usize) {
            page.push((ino, FileType::Directory, name.to_owned(), i as u64 + 1));
        }
        for (i, e) in listing
            .entries
            .iter()
            .enumerate()
            .skip((offset as usize).saturating_sub(2))
        {
            let kind = if Self::is_link(e) {
                FileType::Symlink
            } else if e.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let name = internal_name(e);
            let child = self.inode(&child(&path, &name), Some(e));
            page.push((child, kind, name, i as u64 + 3));
        }
        Ok(page)
    }

    /// The entry reply for a path just made.
    fn entry_reply(&self, path: &str, reply: ReplyEntry) {
        let vol = self.vol.read_ok();
        let entry = self.resolve(&vol, path).ok().flatten();
        let ino = self.inode(path, entry.as_ref());
        match self.attr(&vol, ino) {
            Ok(attr) => {
                self.state.lock_ok().looked_up(ino);
                reply.entry(&TTL, &attr, fuser::Generation(0))
            }
            Err(e) => reply.error(e),
        }
    }

    fn name_path(&self, parent: INodeNo, name: &OsStr) -> Result<String, Errno> {
        Ok(child(&self.path(parent.0)?, &kernel_name(name)?))
    }
}

impl Filesystem for RefsFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.guard("lookup", move || {
            let path = match self.name_path(parent, name) {
                Ok(p) => p,
                Err(e) => return reply.error(e),
            };
            let vol = self.vol.read_ok();
            if let Err(e) = self.directory(&vol, split(&path).0) {
                return reply.error(e);
            }
            // The name as the volume spells it (lookups ignore case).
            let (path, entry) = match self.resolve(&vol, &path) {
                Ok(Some(e)) => (child(split(&path).0, &internal_name(&e)), Some(e)),
                Ok(None) => (path, None),
                Err(e) => return reply.error(e),
            };
            let ino = self.inode(&path, entry.as_ref());
            match self.attr(&vol, ino) {
                Ok(attr) => {
                    self.state.lock_ok().looked_up(ino);
                    reply.entry(&TTL, &attr, fuser::Generation(0))
                }
                Err(e) => reply.error(e),
            }
        })
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.guard("forget", move || {
            self.state.lock_ok().forget(ino.0, nlookup);
        })
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.guard("getattr", move || {
            let vol = self.vol.read_ok();
            match self.attr(&vol, ino.0) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(e) => reply.error(e),
            }
        })
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.guard("setattr", move || {
            let r = (|| -> Result<(), Errno> {
                {
                    // A file without a name: its buffer's length, nothing else.
                    let mut s = self.state.lock_ok();
                    if s.orphans.contains_key(&ino.0) {
                        if let (Some(size), Some(p)) = (size, s.pending.get_mut(&ino.0)) {
                            p.set_len(size)?;
                        }
                        return Ok(());
                    }
                }
                let path = self.path(ino.0)?;
                if let Some(size) = size {
                    if size > MAX_WRITTEN {
                        return Err(Errno::EFBIG);
                    }
                    let pending = self.state.lock_ok().pending.contains_key(&ino.0);
                    if pending {
                        let mut s = self.state.lock_ok();
                        let p = s.pending.get_mut(&ino.0).unwrap();
                        p.set_len(size)?;
                        p.dirty = true;
                    } else {
                        let mut p = self.content(&self.vol.read_ok(), &path)?;
                        p.set_len(size)?;
                        let ranges = std::mem::take(&mut p.ranges);
                        let source = PendingData(&p.file, p.len);
                        self.change(&path, |v| v.update_file(&path, &source, &ranges, now()))?;
                    }
                }
                if atime.is_some() || mtime.is_some() || crtime.is_some() {
                    let mut times = self.resolve(&self.vol.read_ok(), &path)?.ok_or(Errno::EPERM)?.times;
                    let pick = |t: TimeOrNow| match t {
                        TimeOrNow::Now => now(),
                        TimeOrNow::SpecificTime(t) => filetime(t),
                    };
                    if let Some(t) = atime {
                        times.accessed = pick(t);
                    }
                    if let Some(t) = mtime {
                        times.modified = pick(t);
                    }
                    if let Some(t) = crtime {
                        times.created = filetime(t);
                    }
                    times.changed = now();
                    self.change(&path, |v| v.set_times(&path, &times))?;
                }
                if let Some(mode) = mode {
                    // The write bits: Windows' read-only attribute.
                    let entry = self.resolve(&self.vol.read_ok(), &path)?.ok_or(Errno::EPERM)?;
                    if !entry.is_dir() && !Self::is_link(&entry) {
                        let ro = mode & 0o222 == 0;
                        if ro != (entry.attributes & 1 != 0) {
                            let a = (entry.attributes & !1) | u32::from(ro);
                            self.change(&path, |v| v.set_attributes(&path, a))?;
                        }
                    }
                }
                Ok(())
            })();
            match r {
                Ok(()) => self.getattr(_req, ino, None, reply),
                Err(e) => reply.error(e),
            }
        })
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        self.guard("readlink", move || {
            let vol = self.vol.read_ok();
            match self.path(ino.0).ok().and_then(|p| self.link(&vol, &p)) {
                Some(target) => reply.data(target.as_bytes()),
                None => reply.error(Errno::EINVAL),
            }
        })
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        self.guard("mkdir", move || {
            let path = match self.name_path(parent, name) {
                Ok(p) => p,
                Err(e) => return reply.error(e),
            };
            match self.change(&path, |v| v.create_directory(&path, now())) {
                Ok(()) => self.entry_reply(&path, reply),
                Err(e) => reply.error(e),
            }
        })
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.guard("unlink", move || {
            let path = match self.name_path(parent, name) {
                Ok(p) => p,
                Err(e) => return reply.error(e),
            };
            match self.unlink_path(&path) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        })
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.guard("rmdir", move || self.unlink(req, parent, name, reply))
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.guard("rename", move || {
            let r = (|| -> Result<(), Errno> {
                let from = self.name_path(parent, name)?;
                let to = self.name_path(newparent, newname)?;
                self.rename_paths(&from, &to, flags)
            })();
            match r {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        })
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        self.guard("link", move || {
            let r = (|| -> Result<String, Errno> {
                let path = self.path(ino.0)?;
                let to = self.name_path(newparent, newname)?;
                self.change(&to, |v| v.link_file(&path, &to, now()))?;
                // The new name is the same inode.
                let mut s = self.state.lock_ok();
                s.inodes.insert(to.clone(), ino.0);
                s.paths.entry(ino.0).or_default().push(to.clone());
                drop(s);
                let vol = self.vol.read_ok();
                if let Ok(Some(e)) = self.resolve(&vol, &to) {
                    self.inode(&to, Some(&e));
                }
                Ok(to)
            })();
            match r {
                Ok(to) => self.entry_reply(&to, reply),
                Err(e) => reply.error(e),
            }
        })
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.guard("open", move || match self.open_inode(ino.0, flags.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        })
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        self.guard("create", move || {
            let r = (|| -> Result<(FileAttr, u64), Errno> {
                let path = self.name_path(parent, name)?;
                self.change(&path, |v| v.create_file(&path, b"", now()))?;
                let ino = self.inode(&path, None);
                let mut p = Pending::new()?;
                p.whole = true;
                let mut s = self.state.lock_ok();
                s.pending.insert(ino, p);
                let fh = s.writer(ino);
                drop(s);
                Ok((self.attr(&self.vol.read_ok(), ino)?, fh))
            })();
            match r {
                Ok((attr, fh)) => {
                    self.state.lock_ok().looked_up(attr.ino.0);
                    reply.created(&TTL, &attr, fuser::Generation(0), FileHandle(fh), FopenFlags::empty())
                }
                Err(e) => reply.error(e),
            }
        })
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.guard("read", move || match self.read_inode(ino.0, offset, size) {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(e),
        })
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.guard("write", move || match self.write_inode(ino.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        })
    }

    /// A whole file copied into an empty one (as `cp` and file managers
    /// copy with copy_file_range) becomes a block clone, as Windows'
    /// Copy-Item makes on a Dev Drive: the copy shares the clusters until
    /// one of them changes. Other ranges are copied through the files'
    /// buffers, a MiB at a time.
    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        _fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        _fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        self.guard("copy_file_range", move || {
            let r = (|| -> Result<u32, Errno> {
                if !self.writable {
                    return Err(Errno::EROFS);
                }
                let (src, dst) = (self.path(ino_in.0)?, self.path(ino_out.0)?);
                let (buffered, src_dirty, dst_len) = {
                    let s = self.state.lock_ok();
                    let dst = s.pending.get(&ino_out.0).ok_or(Errno::EBADF)?;
                    let src = s.pending.get(&ino_in.0);
                    (src.map(|p| p.len), src.is_some_and(|p| p.dirty), dst.len)
                };
                let size = match buffered {
                    Some(len) => len,
                    None => {
                        let vol = self.vol.read_ok();
                        self.file(&vol, &src)?.data.as_ref().map_or(0, |s| s.size)
                    }
                };
                if ino_in != ino_out
                    && (offset_in, offset_out, dst_len) == (0, 0, 0)
                    && len >= size
                    && size > 0
                    && size <= u64::from(u32::MAX)
                    && !src_dirty
                {
                    self.write_back(ino_out.0)?;
                    if self.change(&dst, |v| v.clone_file(&src, &dst, now())).is_ok() {
                        // The copy's buffer: its new content, nothing to write.
                        let vol = self.vol.read_ok();
                        let mut p = self.content(&vol, &dst)?;
                        drop(vol);
                        let mut s = self.state.lock_ok();
                        p.handles = s.pending.get(&ino_out.0).map_or(1, |old| old.handles);
                        s.pending.insert(ino_out.0, p);
                        return Ok(size as u32);
                    }
                }
                let n = len.min(1 << 20).min(size.saturating_sub(offset_in));
                if offset_out.saturating_add(n) > MAX_WRITTEN {
                    return Err(Errno::EFBIG);
                }
                let data = match self.state.lock_ok().pending.get(&ino_in.0) {
                    Some(p) => Some(p.read_at(offset_in, n as u32)?),
                    None => None,
                };
                let data = match data {
                    Some(d) => d,
                    None => {
                        let vol = self.vol.read_ok();
                        let file = self.file(&vol, &src)?;
                        let mut buf = vec![0u8; n as usize];
                        if let Some(stream) = &file.data {
                            let got = vol
                                .read_stream(stream, offset_in, &mut buf)
                                .map_err(|e| errno(&src, e))?;
                            buf.truncate(got);
                        }
                        buf
                    }
                };
                let mut s = self.state.lock_ok();
                let p = s.pending.get_mut(&ino_out.0).ok_or(Errno::EBADF)?;
                p.write_at(offset_out, &data)?;
                p.dirty = true;
                Ok(data.len() as u32)
            })();
            match r {
                Ok(n) => reply.written(n),
                Err(e) => reply.error(e),
            }
        })
    }

    fn flush(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        self.guard("flush", move || match self.write_back(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        })
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.guard("fsync", move || match self.write_back(ino.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        })
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.guard("release", move || match self.release_inode(ino.0, fh.0) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        })
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.guard("opendir", move || match self.open_dir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        })
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.guard("releasedir", move || {
            self.state.lock_ok().dir_handles.remove(&fh.0);
            reply.ok()
        })
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        self.guard("readdir", move || {
            let page = match self.dir_page(ino.0, fh.0, offset) {
                Ok(p) => p,
                Err(e) => return reply.error(e),
            };
            for (child, kind, name, next) in page {
                if reply.add(INodeNo(child), next, kind, os_name(&name)) {
                    break;
                }
            }
            reply.ok();
        })
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.guard("statfs", move || {
            let vol = self.vol.read_ok();
            let blocks = vol.boot.volume_size() / vol.cluster;
            let free = vol.free_clusters().unwrap_or(0);
            reply.statfs(blocks, free, free, 0, 0, vol.cluster as u32, 255, vol.cluster as u32);
        })
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.guard("listxattr", move || {
            let vol = self.vol.read_ok();
            let names: Vec<u8> = match self.path(ino.0).and_then(|p| self.file(&vol, &p)) {
                Ok(f) => f
                    .streams
                    .iter()
                    .flat_map(|(n, _)| format!("user.{n}\0").into_bytes())
                    .collect(),
                Err(_) => Vec::new(),
            };
            if size == 0 {
                reply.size(names.len() as u32)
            } else if names.len() > size as usize {
                reply.error(Errno::ERANGE)
            } else {
                reply.data(&names)
            }
        })
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.guard("getxattr", move || {
            let name = name.to_string_lossy();
            let Some(stream_name) = name.strip_prefix("user.") else {
                return reply.error(Errno::NO_XATTR);
            };
            let vol = self.vol.read_ok();
            let Ok(file) = self.path(ino.0).and_then(|p| self.file(&vol, &p)) else {
                return reply.error(Errno::NO_XATTR);
            };
            let Some((_, stream)) = file.streams.iter().find(|(n, _)| n == stream_name) else {
                return reply.error(Errno::NO_XATTR);
            };
            // Linux takes extended attributes up to 64 KiB; larger streams are
            // read with `refs cat --stream`.
            if stream.size > 65536 {
                return reply.error(Errno::E2BIG);
            }
            if size == 0 {
                return reply.size(stream.size as u32);
            }
            if stream.size > size as u64 {
                return reply.error(Errno::ERANGE);
            }
            let mut buf = vec![0u8; stream.size as usize];
            match vol.read_stream(stream, 0, &mut buf) {
                Ok(n) => reply.data(&buf[..n]),
                Err(_) => reply.error(Errno::EIO),
            }
        })
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        self.guard("setxattr", move || {
            let r = (|| -> Result<(), Errno> {
                let name = name.to_str().ok_or(Errno::EINVAL)?;
                let stream = name.strip_prefix("user.").ok_or(Errno::EOPNOTSUPP)?;
                let path = self.path(ino.0)?;
                let file = self.file(&self.vol.read_ok(), &path)?;
                let exists = file.streams.iter().any(|(n, _)| n.eq_ignore_ascii_case(stream));
                if exists && flags & XATTR_CREATE != 0 {
                    return Err(Errno::EEXIST);
                }
                if !exists && flags & XATTR_REPLACE != 0 {
                    return Err(Errno::NO_XATTR);
                }
                self.change(&path, |v| v.write_stream(&path, stream, value, now()))
            })();
            match r {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        })
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.guard("removexattr", move || {
            let r = (|| -> Result<(), Errno> {
                let name = name.to_str().ok_or(Errno::EINVAL)?;
                let stream = name.strip_prefix("user.").ok_or(Errno::NO_XATTR)?;
                let path = self.path(ino.0)?;
                self.change(&path, |v| v.delete_stream(&path, stream, now()))
                    .map_err(|e| if e == Errno::ENOENT { Errno::NO_XATTR } else { e })
            })();
            match r {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        })
    }
}

/// What `refs mount --status-file PATH` tells whoever started it
/// (`mount.refs`): `state=opening` with `read=` the bytes read from the
/// device so far, renewed every second while the volume is opened; then
/// `state=mounted` once the mount is in place, or `state=failed` with
/// `error=`. Each state is written whole into PATH.tmp and renamed over
/// PATH, so a reader never sees half of one. So the one waiting needs no
/// timeout: a slow device shows as a growing count.
pub struct MountStatus {
    path: Option<PathBuf>,
    read: Arc<std::sync::atomic::AtomicU64>,
    /// Set once mounted or failed; the lock orders the writes.
    done: Arc<Mutex<bool>>,
}

impl MountStatus {
    /// Starts reporting into `path` (nothing without one).
    pub fn start(path: Option<PathBuf>) -> Self {
        let status = MountStatus {
            path,
            read: Default::default(),
            done: Default::default(),
        };
        if let Some(path) = status.path.clone() {
            let (read, done) = (status.read.clone(), status.done.clone());
            std::thread::spawn(move || {
                loop {
                    {
                        let done = done.lock().unwrap_or_else(|e| e.into_inner());
                        if *done {
                            break;
                        }
                        let n = read.load(std::sync::atomic::Ordering::Relaxed);
                        let _ = Self::write(&path, &format!("state=opening\nread={n}\n"));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
        }
        status
    }

    /// The counter of bytes read that the progress shows.
    pub fn counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.read.clone()
    }

    pub fn mounted(&self) {
        self.finish("state=mounted\n");
    }

    pub fn failed(&self, error: &str) {
        self.finish(&format!("state=failed\nerror={}\n", error.replace('\n', " | ")));
    }

    fn finish(&self, text: &str) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        if let (false, Some(path)) = (*done, &self.path) {
            let _ = Self::write(path, text);
        }
        *done = true;
    }

    fn write(path: &Path, text: &str) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }
}

/// Mounts the volume at `mountpoint` and serves it until unmounted; with
/// `writable` changes go to the volume. `source` names the mount (the
/// device, as mount(8) and udisks2 expect to find it in the mount table);
/// with `blkdev` (root, a block device) the mount is of type fuseblk on
/// that device, as ntfs-3g's are, so that udisks2 counts it as the
/// device's mount. `status` learns when the mount is in place.
pub fn serve(
    vol: Volume<Rw>,
    source: &str,
    mountpoint: &Path,
    allow_other: bool,
    writable: bool,
    blkdev: bool,
    status: &MountStatus,
) -> Result<()> {
    let mut config = Config::default();
    config.mount_options.extend([
        MountOption::FSName(source.into()),
        MountOption::Subtype("refs".into()),
        MountOption::DefaultPermissions,
    ]);
    if !writable {
        config.mount_options.push(MountOption::RO);
    }
    if allow_other {
        config.acl = fuser::SessionACL::All;
    }
    // Changes are made one at a time; reads may run beside each other.
    config.n_threads = Some(if writable { 1 } else { 4 });
    let mountpoint = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let owner = std::fs::metadata(&mountpoint)
        .map(|m| (m.uid(), m.gid()))
        .unwrap_or((0, 0));
    let state = State::new();
    let fs = RefsFs {
        vol: RwLock::new(vol),
        writable,
        mountpoint: mountpoint.clone(),
        owner,
        state: Mutex::new(state),
        broken: AtomicBool::new(false),
    };
    if blkdev {
        return serve_blkdev(fs, config, source, &mountpoint, allow_other, writable, status);
    }
    let session = fuser::spawn_mount(fs, &mountpoint, &config)
        .with_context(|| format!("cannot mount on {}", mountpoint.display()))?;
    status.mounted();
    session.join().context("FUSE session failed")?;
    Ok(())
}

/// Root mounting a block device: a mount of type fuseblk.refs on the
/// device, as ntfs-3g's are (udisks2 counts it as the device's mount),
/// made here with mount(2). As root nothing needs fusermount3, whose
/// AppArmor profile (Ubuntu) refuses that type and udisks2's mount points.
/// The mount ends with the process: unmounted on SIGTERM and SIGINT, and by
/// a watcher process (`refs mount-watch`) if this one is killed.
fn serve_blkdev(
    fs: RefsFs,
    config: Config,
    source: &str,
    mountpoint: &Path,
    allow_other: bool,
    writable: bool,
    status: &MountStatus,
) -> Result<()> {
    use rustix::mount::{UnmountFlags, unmount};
    let fuse = mount_blkdev(source, mountpoint, allow_other, writable)?;
    let started = std::fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .and_then(|m| top_mount(&m, mountpoint))
        .filter(|(_, fstype)| fstype == "fuseblk.refs")
        .context("the new mount is not in /proc/self/mountinfo")
        .and_then(|(id, _)| {
            let watcher = watch(id, mountpoint)?;
            let acl = config.acl;
            let session = fuser::Session::from_fd(fs, fuse, acl, config)?.spawn()?;
            Ok((watcher, session))
        });
    let (mut watcher, session) = match started {
        Ok(started) => started,
        Err(e) => {
            let _ = unmount(mountpoint, UnmountFlags::DETACH);
            return Err(e.context(format!("cannot serve the mount on {}", mountpoint.display())));
        }
    };
    status.mounted();
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])?;
    let target = mountpoint.to_path_buf();
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = unmount(&target, UnmountFlags::DETACH);
        }
    });
    let result = session.join();
    // Its pipe closes: it finds the mount gone and ends.
    drop(watcher.stdin.take());
    let _ = watcher.wait();
    result.context("FUSE session failed")
}

/// The mount(2) of a fuseblk.refs mount of `source`; the /dev/fuse
/// connection it serves. Its block size is the device's logical sector
/// size: the kernel sets the device's block size to it, and the default
/// (512 bytes) fails (EINVAL) on a device with larger sectors, such as a
/// space with 4 KiB sectors.
fn mount_blkdev(source: &str, mountpoint: &Path, allow_other: bool, writable: bool) -> Result<std::os::fd::OwnedFd> {
    use rustix::mount::MountFlags;
    let dev = std::fs::File::open(source).with_context(|| format!("cannot open {source}"))?;
    let sector =
        rustix::fs::ioctl_blksszget(&dev).with_context(|| format!("cannot read the sector size of {source}"))?;
    let fuse = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .context("cannot open /dev/fuse")?;
    let data = blkdev_options(
        std::os::fd::AsRawFd::as_raw_fd(&fuse),
        std::fs::metadata(mountpoint)?.mode(),
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
        sector,
        allow_other,
    );
    let mut flags = MountFlags::NOSUID | MountFlags::NODEV;
    if !writable {
        flags |= MountFlags::RDONLY;
    }
    let data = std::ffi::CString::new(data)?;
    rustix::mount::mount(source, mountpoint, "fuseblk.refs", flags, data.as_c_str())
        .with_context(|| format!("cannot mount {source} on {}", mountpoint.display()))?;
    Ok(fuse.into())
}

/// The data of the mount(2) of a fuseblk mount: the connection, the root's
/// file type (as libfuse passes it), the owner, the block size, and who may
/// enter.
fn blkdev_options(fd: i32, rootmode: u32, uid: u32, gid: u32, blksize: u32, allow_other: bool) -> String {
    let mut data = format!(
        "fd={fd},rootmode={:o},user_id={uid},group_id={gid},blksize={blksize},default_permissions",
        rootmode & 0o170000
    );
    if allow_other {
        data.push_str(",allow_other");
    }
    data
}

/// Starts the watcher of mount `id` at `mountpoint`: `refs mount-watch`,
/// whose stdin is a pipe from this process, so it learns when this
/// process ends however it ends.
fn watch(id: u64, mountpoint: &Path) -> Result<std::process::Child> {
    std::process::Command::new(std::env::current_exe()?)
        .args(["mount-watch", "--id", &id.to_string()])
        .arg(mountpoint)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .context("cannot start the mount watcher")
}

/// `refs mount-watch`: waits until its stdin, the pipe from the process
/// serving the mount, ends, or for SIGTERM, SIGINT or SIGHUP (the unit or
/// terminal going away); then unmounts mount `id` if it is still the one
/// on top at `mountpoint`, and nothing else.
pub fn watch_main(id: u64, mountpoint: &Path) -> Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ])?;
    let target = mountpoint.to_path_buf();
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            unmount_if_still(id, &target);
            std::process::exit(0);
        }
    });
    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
    unmount_if_still(id, mountpoint);
    Ok(())
}

fn unmount_if_still(id: u64, mountpoint: &Path) {
    if let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo")
        && top_mount(&mountinfo, mountpoint).is_some_and(|(top, _)| top == id)
    {
        let _ = rustix::mount::unmount(mountpoint, rustix::mount::UnmountFlags::DETACH);
    }
}

/// The mount on top at `mountpoint` in mountinfo text (proc_pid_mountinfo(5)):
/// its id and type. Mount points there have space, tab, newline and
/// backslash escaped as octal (\040).
fn top_mount(mountinfo: &str, mountpoint: &Path) -> Option<(u64, String)> {
    use std::os::unix::ffi::OsStrExt;
    let want = mountpoint.as_os_str().as_bytes();
    mountinfo
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            if unescape_octal(fields.get(4)?) != want {
                return None;
            }
            let separator = fields.iter().position(|f| *f == "-")?;
            Some((fields[0].parse().ok()?, (*fields.get(separator + 1)?).to_owned()))
        })
        .next_back()
}

fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let digits = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match digits {
            Some(d) if b[i] == b'\\' => {
                let value = d.iter().fold(0u32, |v, c| v * 8 + u32::from(c - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                i += 4;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

/// The refs tests' forging of fixtures (pages changed, their checksums
/// stored again), for names Windows allows and `refs` does not write.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../refs/tests/forge/mod.rs"]
mod forge;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use storage_spaces::io::{Overlay, ReadAt as _, SparseImage};

    /// The status file of `refs mount --status-file`: opening with the
    /// bytes read while the volume opens, then mounted or failed, each
    /// state whole; nothing changes it after that.
    #[test]
    fn mount_status_reports_progress_then_the_outcome() {
        let dir = std::env::temp_dir().join(format!("refs-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let path = dir.join("status");
        let status = MountStatus::start(Some(path.clone()));
        status.counter().fetch_add(4096, std::sync::atomic::Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while read(&path) != "state=opening\nread=4096\n" {
            assert!(std::time::Instant::now() < deadline, "{:?}", read(&path));
            std::thread::sleep(Duration::from_millis(50));
        }
        status.mounted();
        assert_eq!(read(&path), "state=mounted\n");
        std::thread::sleep(Duration::from_millis(1200));
        status.failed("too late");
        assert_eq!(read(&path), "state=mounted\n");
        let failed = dir.join("failed");
        MountStatus::start(Some(failed.clone())).failed("no ReFS volume\non the device");
        assert_eq!(read(&failed), "state=failed\nerror=no ReFS volume | on the device\n");
        assert!(!dir.join("status.tmp").exists() && !dir.join("failed.tmp").exists());
        MountStatus::start(None).mounted();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The data of the fuseblk mount: the root's file type only (as libfuse
    /// passes it), the device's sector size as the block size.
    #[test]
    fn blkdev_mount_data() {
        assert_eq!(
            blkdev_options(5, 0o40755, 0, 0, 4096, true),
            "fd=5,rootmode=40000,user_id=0,group_id=0,blksize=4096,default_permissions,allow_other"
        );
        assert_eq!(
            blkdev_options(7, 0o40700, 1000, 100, 512, false),
            "fd=7,rootmode=40000,user_id=1000,group_id=100,blksize=512,default_permissions"
        );
    }

    /// The watcher finds the mount on top at its mount point: the last
    /// line for it, with escaped characters in the path, and nothing for
    /// other paths.
    #[test]
    fn finds_the_mount_on_top() {
        let mountinfo = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
61 22 0:52 / /run/media/root/My\\040disk rw,nosuid,nodev,relatime shared:33 - fuseblk.refs /dev/dm-8 ro,user_id=0,group_id=0,default_permissions,allow_other,blksize=4096
62 22 0:53 / /mnt/x rw,relatime shared:34 - fuseblk.refs /dev/loop1 ro
63 62 0:54 / /mnt/x rw,relatime shared:35 - tmpfs tmpfs rw
";
        assert_eq!(
            top_mount(mountinfo, Path::new("/run/media/root/My disk")),
            Some((61, "fuseblk.refs".to_owned()))
        );
        assert_eq!(
            top_mount(mountinfo, Path::new("/mnt/x")),
            Some((63, "tmpfs".to_owned()))
        );
        assert_eq!(top_mount(mountinfo, Path::new("/mnt")), None);
        assert_eq!(unescape_octal("a\\134b\\011\\0"), b"a\\b\t\\0");
        assert_eq!(unescape_octal("\\777"), b"?");
    }

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            units: None,
            attributes: 0,
            times: Default::default(),
            size: 0,
            allocated: 0,
            target: Target::Embedded(Vec::new()),
        }
    }

    #[test]
    fn inodes_the_kernel_forgot_are_dropped() {
        let mut s = State::new();
        let a = s.inode("/a", None);
        s.looked_up(a);
        s.looked_up(a);
        s.forget(a, 1);
        assert!(s.paths.contains_key(&a), "one lookup is still held");
        s.forget(a, 1);
        assert!(!s.paths.contains_key(&a) && !s.inodes.contains_key("/a"));
        // The root is not the kernel's to forget.
        s.forget(INodeNo::ROOT.0, 1000);
        assert!(s.paths.contains_key(&INodeNo::ROOT.0));
    }

    #[test]
    fn inodes_nothing_looked_up_are_bounded() {
        // Listing a directory makes inodes the kernel then looks up one by
        // one, or never; a walk over a big volume must not keep them all.
        let mut s = State::new();
        let held = s.inode("/held", None);
        s.looked_up(held);
        for i in 0..INODES_KEPT + 100 {
            s.inode(&format!("/f{i}"), None);
        }
        assert!(s.inodes.len() <= INODES_KEPT + 1, "{} inodes", s.inodes.len());
        assert!(s.paths.contains_key(&held) && s.paths.contains_key(&INodeNo::ROOT.0));
    }

    #[test]
    fn cached_listings_and_files_are_bounded() {
        let mut s = State::new();
        let big = Arc::new(Dir {
            entries: (0..DIR_ENTRIES_KEPT / 2 + 1).map(|i| entry(&i.to_string())).collect(),
            by_name: HashMap::new(),
        });
        for oid in 0..5 {
            s.cache_dir(oid, big.clone());
            assert!(s.dir_entries <= DIR_ENTRIES_KEPT, "{} entries", s.dir_entries);
        }
        let huge = Arc::new(Dir {
            entries: (0..DIR_ENTRIES_KEPT + 1).map(|i| entry(&i.to_string())).collect(),
            by_name: HashMap::new(),
        });
        s.cache_dir(9, huge);
        // One over the budget is kept alone (a lookup there reads it whole).
        assert!(
            s.dirs.contains_key(&9) && s.dirs.len() == 1,
            "a listing over the budget is kept alone"
        );
        let file = Arc::new(RefsFile::default());
        for i in 0..FILES_KEPT + 10 {
            s.cache_file(format!("/f{i}"), file.clone());
        }
        assert!(s.files.len() <= FILES_KEPT);
    }

    /// A fixture's volume served from the device `device` makes of its
    /// image.
    fn serve(name: &str, writable: bool, device: impl FnOnce(SparseImage) -> Rw) -> RefsFs {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../refs/tests/fixtures")
            .join(name);
        let image = SparseImage::read_from(std::fs::File::open(dir.join("disk.fixture")).unwrap()).unwrap();
        let text = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap();
        let offset = manifest["partition_offset"].as_u64().unwrap();
        let vol = Volume::open(device(image), offset).unwrap();
        RefsFs {
            vol: RwLock::new(vol),
            writable,
            mountpoint: PathBuf::from("/mnt"),
            owner: (0, 0),
            state: Mutex::new(State::new()),
            broken: AtomicBool::new(false),
        }
    }

    /// The fixture r314small served from memory (changes stay in an
    /// overlay).
    fn served(writable: bool) -> RefsFs {
        serve("r314small", writable, |image| Box::new(Overlay::new(image)))
    }

    /// An image at hand served from memory (changes in an overlay).
    fn serve_image(image: SparseImage, offset: u64, writable: bool) -> RefsFs {
        RefsFs {
            vol: RwLock::new(Volume::open(Box::new(Overlay::new(image)) as Rw, offset).unwrap()),
            writable,
            mountpoint: PathBuf::from("/mnt"),
            owner: (0, 0),
            state: Mutex::new(State::new()),
            broken: AtomicBool::new(false),
        }
    }

    /// New inodes while the kernel holds 100 000 (as `ls -l` of a large
    /// directory leaves them): in time, the idle ones trimmed (1.2.2 trimmed
    /// at every new inode once the held ones alone were more than it keeps:
    /// 1.6 ms each; audit M17).
    #[test]
    fn many_held_inodes_do_not_slow_new_ones() {
        let mut s = State::new();
        for i in 0..100_000 {
            let ino = s.inode(&format!("/held/{i}"), None);
            s.looked_up(ino);
        }
        let t = std::time::Instant::now();
        for i in 0..20_000 {
            s.inode(&format!("/new/{i}"), None);
        }
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(s.paths.len() <= 100_000 + INODES_KEPT + 1);
    }

    /// A listing of more names than the cache keeps is kept, alone, so that
    /// a lookup there reads it no more (1.2.2 kept none: every name looked up
    /// in a directory of 200 000 cost a reading of all of it; audit M17).
    #[test]
    fn a_listing_of_many_names_is_kept() {
        let mut s = State::new();
        let small: Vec<Entry> = (0..10).map(|i| entry(&format!("f{i}"))).collect();
        let big: Vec<Entry> = (0..DIR_ENTRIES_KEPT + 50_000)
            .map(|i| entry(&format!("f{i}")))
            .collect();
        let dir = |entries: Vec<Entry>| {
            let by_name = entries.iter().enumerate().map(|(i, e)| (e.name.clone(), i)).collect();
            Arc::new(Dir { entries, by_name })
        };
        s.cache_dir(1, dir(small.clone()));
        s.cache_dir(2, dir(big));
        assert!(s.dirs.contains_key(&2), "the large listing kept");
        assert!(!s.dirs.contains_key(&1), "alone");
        s.cache_dir(3, dir(small));
        assert!(
            s.dirs.contains_key(&3) && !s.dirs.contains_key(&2),
            "until another is kept"
        );
    }

    /// The buffer of a file open for writing has no name, and only its user
    /// may read it (1.2.2 made it with the default mode, 0644 under the
    /// usual umask, and a name others could guess, for the moment it had
    /// one; audit M18).
    #[test]
    fn buffers_of_files_open_for_writing_are_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let p = Pending::new().unwrap();
        let m = p.file.metadata().unwrap();
        assert_eq!((m.permissions().mode() & 0o777, m.nlink()), (0o600, 0));
    }

    /// st_blocks counts what a file has on the volume: Windows' sparse file
    /// has half its size (1.2.2 counted the size; audit M18).
    #[test]
    fn st_blocks_count_what_a_file_has() {
        let fs = serve("r314sparsemap", false, |image| Box::new(Overlay::new(image)));
        let ino = fs.inode("/holes.bin", None);
        let attr = fs.attr(&fs.vol.read_ok(), ino).unwrap();
        assert_eq!((attr.size, attr.blocks), (204_800_000, 25_000 * 4096 / 512));
    }

    /// Data of zeros, as much as asked for.
    struct Zeros(u64);

    impl refs::write::Source for Zeros {
        fn len(&self) -> u64 {
            self.0
        }
        fn read_into(&self, _offset: u64, buf: &mut [u8]) -> refs::Result<()> {
            buf.fill(0);
            Ok(())
        }
    }

    /// A change the volume has no room for fails with ENOSPC (1.2.2:
    /// EOPNOTSUPP, "Operation not supported"; audit M18).
    #[test]
    fn a_change_without_room_is_enospc() {
        let fs = served(true);
        let free = {
            let vol = fs.vol.read_ok();
            vol.free_clusters().unwrap() * vol.cluster
        };
        let more = Zeros(free + (1 << 30));
        let r = fs.change("/big.bin", |v| v.create_file_from("/big.bin", &more, now()));
        assert_eq!(r, Err(Errno::ENOSPC));
    }

    /// A directory listed while its names go: every name it had when it
    /// was opened comes once, none is skipped (1.2.2's offsets counted the
    /// names left at each call: 10 unlinked, the next page skipped 10;
    /// audit M18).
    #[test]
    fn a_listing_survives_unlinks_between_pages() {
        let fs = served(true);
        {
            let mut vol = fs.vol.write_ok();
            vol.create_directory("/many", now()).unwrap();
            for i in 0..40 {
                vol.create_file(&format!("/many/f{i:02}"), b"x", now()).unwrap();
            }
        }
        fs.changed(None);
        let ino = fs.inode("/many", None);
        let fh = fs.open_dir(ino).unwrap();
        let first = fs.dir_page(ino, fh, 0).unwrap();
        let mut seen: Vec<String> = first[..12].iter().map(|e| e.2.clone()).collect();
        for name in &seen[2..] {
            fs.unlink_path(&format!("/many/{name}")).unwrap();
        }
        seen.extend(fs.dir_page(ino, fh, first[11].3).unwrap().into_iter().map(|e| e.2));
        let mut want = vec![".".to_owned(), "..".to_owned()];
        want.extend((0..40).map(|i| format!("f{i:02}")));
        assert_eq!(seen, want);
        // A new handle lists what is left.
        let fresh = fs.open_dir(ino).unwrap();
        assert_eq!(fs.dir_page(ino, fresh, 0).unwrap().len(), 2 + 30);
    }

    /// A file open for reading whose name goes (unlink, a rename over it)
    /// reads on as it was, and fstat says so, until it is closed (1.2.2:
    /// ENOENT; audit M18).
    #[test]
    fn a_file_whose_name_goes_while_open_reads_on() {
        let fs = served(true);
        for how in ["unlink", "rename over"] {
            {
                let mut vol = fs.vol.write_ok();
                vol.create_file("/o.bin", &[0x6f; 9000], now()).unwrap();
                vol.create_file("/n.bin", b"new", now()).unwrap();
            }
            fs.changed(None);
            let ino = fs.inode("/o.bin", None);
            let fh = fs.open_inode(ino, 0).unwrap();
            if how == "unlink" {
                fs.unlink_path("/o.bin").unwrap();
            } else {
                fs.rename_paths("/n.bin", "/o.bin", RenameFlags::empty()).unwrap();
            }
            assert_eq!(fs.read_inode(ino, 0, 9000).unwrap(), vec![0x6f; 9000], "{how}");
            let a = fs.attr(&fs.vol.read_ok(), ino).unwrap();
            assert_eq!((a.size, a.nlink), (9000, 0), "{how}");
            fs.release_inode(ino, fh).unwrap();
            assert!(!fs.state.lock_ok().pending.contains_key(&ino), "{how}");
            let _ = fs.unlink_path("/o.bin");
            let _ = fs.unlink_path("/n.bin");
        }
    }

    /// A file open for writing whose name goes takes writes and reads them
    /// back until it is closed, which writes nothing and reports nothing
    /// (1.2.2: the close failed with ENOENT and the buffer stayed).
    #[test]
    fn a_file_whose_name_goes_while_open_for_writing_closes_cleanly() {
        let fs = served(true);
        fs.vol.write_ok().create_file("/w.bin", &[1; 5000], now()).unwrap();
        fs.changed(None);
        let ino = fs.inode("/w.bin", None);
        let fh = fs.open_inode(ino, 1).unwrap();
        assert_eq!(fs.write_inode(ino, 0, b"before"), Ok(6));
        fs.unlink_path("/w.bin").unwrap();
        assert_eq!(fs.write_inode(ino, 6, b" after"), Ok(6));
        assert_eq!(&fs.read_inode(ino, 0, 12).unwrap(), b"before after");
        fs.release_inode(ino, fh).unwrap();
        assert!(!fs.state.lock_ok().pending.contains_key(&ino));
        assert!(fs.vol.read_ok().lookup("/w.bin").is_err());
    }

    /// Names between the kernel and the mount: lone surrogates as WTF-8,
    /// backslashes kept apart from the escapes, other bytes refused.
    #[test]
    fn names_round_trip_between_the_kernel_and_the_mount() {
        use std::os::unix::ffi::OsStrExt;
        for bytes in [
            b"plain".to_vec(),
            b"back\\slash".to_vec(),
            b"a\\u0041".to_vec(),
            vec![0xed, 0xa0, 0x80, b'x', 0xed, 0xbf, 0xbf],
            "\u{1f600}".as_bytes().to_vec(),
        ] {
            let name = OsStr::from_bytes(&bytes);
            assert_eq!(os_name(&kernel_name(name).unwrap()), name, "{bytes:x?}");
        }
        assert_eq!(kernel_name(OsStr::new("a\\u0041")).unwrap(), "a\\\\u0041");
        assert_eq!(kernel_name(OsStr::from_bytes(&[0xff])), Err(Errno::EINVAL));
    }

    /// Two names that differ only in a lone surrogate (Windows allows them):
    /// each listed once, as WTF-8, each opening its own file; read, not
    /// changed (1.2.2 read U+FFFD for both: one file unreachable, listed
    /// twice; audit M18).
    #[test]
    fn names_with_lone_surrogates_stay_apart() {
        use std::os::unix::ffi::OsStringExt;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../refs/tests/fixtures/r314small");
        let base = SparseImage::read_from(std::fs::File::open(dir.join("disk.fixture")).unwrap()).unwrap();
        let offset = 16 << 20;
        let overlay = Overlay::new(base.clone());
        {
            let mut vol = Volume::open(&overlay, offset).unwrap();
            vol.create_file("/amall.txt", b"first", now()).unwrap();
            vol.create_file("/bmall.txt", b"second", now()).unwrap();
        }
        let mut image = base;
        for at in overlay.written_pages() {
            let mut page = vec![0u8; 4096];
            overlay.read_exact_at(&mut page, at).unwrap();
            image.insert(at, &page);
        }
        let mut forge = forge::Forge { image, offset };
        for (first, unit) in [(b'a', 0xd800u16), (b'b', 0xd801)] {
            let key: Vec<u8> = [0x30, 0, 1, 0]
                .into_iter()
                .chain(
                    format!("{}mall.txt", first as char)
                        .encode_utf16()
                        .flat_map(u16::to_le_bytes),
                )
                .collect();
            let at = move |page: &[u8]| page.windows(key.len()).position(|w| w == key.as_slice());
            let pick = at.clone();
            forge.patch(
                forge::Tree::Object(refs::volume::ROOT_DIRECTORY),
                move |page| pick(page).is_some(),
                move |page| {
                    let i = at(page).unwrap() + 4;
                    page[i..i + 2].copy_from_slice(&unit.to_le_bytes());
                },
            );
        }
        let fs = serve_image(forge.image, offset, true);
        {
            let vol = fs.vol.read_ok();
            let listing = fs.listing(&vol, refs::volume::ROOT_DIRECTORY).unwrap();
            assert_eq!(listing.by_name.len(), listing.entries.len(), "every name its own");
        }
        let page = fs.dir_page(INodeNo::ROOT.0, 0, 0).unwrap();
        let names: Vec<std::ffi::OsString> = page.iter().map(|e| os_name(&e.2)).collect();
        for (unit, data) in [(0xd800u16, &b"first"[..]), (0xd801, &b"second"[..])] {
            let wtf8 = [0xed, 0x80 | (unit >> 6 & 0x3f) as u8, 0x80 | (unit & 0x3f) as u8];
            let name = std::ffi::OsString::from_vec([&wtf8[..], b"mall.txt"].concat());
            assert_eq!(names.iter().filter(|n| **n == name).count(), 1, "{name:?}");
            let path = child("/", &kernel_name(&name).unwrap());
            let ino = fs.inode(&path, None);
            let fh = fs.open_inode(ino, 0).unwrap();
            assert_eq!(fs.read_inode(ino, 0, 16).unwrap(), data);
            fs.release_inode(ino, fh).unwrap();
            assert_eq!(fs.open_inode(ino, 1), Err(Errno::EACCES), "read, not changed");
            assert_eq!(fs.unlink_path(&path), Err(Errno::EACCES));
        }
    }

    /// A device whose writes fail while `fail` is set.
    struct Flaky(Overlay<SparseImage>, Arc<AtomicBool>);

    impl storage_spaces::io::ReadAt for Flaky {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
            self.0.read_exact_at(buf, offset)
        }

        fn size(&self) -> std::io::Result<u64> {
            self.0.size()
        }
    }

    impl WriteAt for Flaky {
        fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
            if self.1.load(Ordering::SeqCst) {
                return Err(std::io::Error::other("a write error"));
            }
            self.0.write_all_at(buf, offset)
        }

        fn flush(&self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    /// Renaming a file over another to a name the library does not write
    /// (it takes printable ASCII only, for now) is refused before anything
    /// changes: the file there stays (1.2.1 deleted it, then the rename
    /// failed).
    #[test]
    fn a_refused_rename_over_a_file_leaves_it() {
        let fs = serve("r314basic4k", true, |image| Box::new(Overlay::new(image)));
        let r = fs.rename_paths("/names/plain.txt", "/names/кириллица.txt", RenameFlags::empty());
        assert_eq!(r, Err(Errno::EOPNOTSUPP));
        let vol = fs.vol.read_ok();
        assert!(vol.lookup("/names/кириллица.txt").is_ok(), "the target is gone");
        let size = vol.lookup("/names/plain.txt").unwrap().size;
        drop(vol);
        // Over an ASCII name: replaced.
        fs.rename_paths("/names/plain.txt", "/names/with spaces.txt", RenameFlags::empty())
            .unwrap();
        let vol = fs.vol.read_ok();
        assert!(vol.lookup("/names/plain.txt").is_err());
        assert_eq!(vol.lookup("/names/with spaces.txt").unwrap().size, size);
    }

    /// Renaming a file over another is one change, one commit: a crash
    /// leaves the file there or the renamed one in its place (1.2.2
    /// deleted the target in one commit and renamed in the next).
    #[test]
    fn a_rename_over_a_file_is_one_commit() {
        let fs = serve("r314basic4k", true, |image| Box::new(Overlay::new(image)));
        let before = fs.vol.read_ok().checkpoint.clock;
        let size = fs.vol.read_ok().lookup("/names/plain.txt").unwrap().size;
        fs.rename_paths("/names/plain.txt", "/names/with spaces.txt", RenameFlags::empty())
            .unwrap();
        let vol = fs.vol.read_ok();
        assert_eq!(vol.checkpoint.clock, before + 1);
        assert!(vol.lookup("/names/plain.txt").is_err());
        assert_eq!(vol.lookup("/names/with spaces.txt").unwrap().size, size);
    }

    /// A file open for writing and for reading: closing the reader leaves
    /// the writer's buffer (1.2.1 counted every close against the writers:
    /// the writer's next write failed with EBADF, and what it wrote before
    /// was written back only if the reader closed last).
    #[test]
    fn closing_a_reader_leaves_the_writers_buffer() {
        let fs = served(true);
        let ino = fs.inode("/small.txt", None);
        let w = fs.open_inode(ino, 1).unwrap();
        let r = fs.open_inode(ino, 0).unwrap();
        assert_ne!(r, w);
        assert_eq!(fs.write_inode(ino, 0, b"first"), Ok(5));
        fs.release_inode(ino, r).unwrap();
        assert_eq!(fs.write_inode(ino, 5, b" then"), Ok(5));
        fs.release_inode(ino, w).unwrap();
        assert!(!fs.state.lock_ok().pending.contains_key(&ino));
        let vol = fs.vol.read_ok();
        let file = vol.open_file(&vol.lookup("/small.txt").unwrap()).unwrap();
        let mut buf = [0u8; 10];
        vol.read_stream(file.data.as_ref().unwrap(), 0, &mut buf).unwrap();
        assert_eq!(&buf, b"first then");
    }

    /// A file truncated and then written past its new end before the
    /// write-back: the gap reads zeros (1.2.2 recorded neither the bytes
    /// the truncation dropped nor the gap, and the volume's old bytes came
    /// back there; audit M16).
    #[test]
    fn a_truncated_file_written_past_its_end_reads_zeros_between() {
        let fs = served(true);
        fs.vol.write_ok().create_file("/t.bin", &[0xaa; 8000], now()).unwrap();
        let ino = fs.inode("/t.bin", None);
        let w = fs.open_inode(ino, 1).unwrap();
        {
            // As setattr truncates a file open for writing.
            let mut s = fs.state.lock_ok();
            let p = s.pending.get_mut(&ino).unwrap();
            p.set_len(5000).unwrap();
            p.dirty = true;
        }
        assert_eq!(fs.write_inode(ino, 6990, &[0xbb; 20]), Ok(20));
        fs.release_inode(ino, w).unwrap();
        let mut want = vec![0xaa; 5000];
        want.resize(6990, 0);
        want.extend([0xbb; 20]);
        let vol = fs.vol.read_ok();
        let file = vol.open_file(&vol.lookup("/t.bin").unwrap()).unwrap();
        let data = file.data.as_ref().unwrap();
        let mut got = vec![0u8; data.size as usize];
        assert_eq!(vol.read_stream(data, 0, &mut got).unwrap(), got.len());
        assert_eq!(got, want);
    }

    /// A write-back that fails keeps what was written: every later flush
    /// tries again and reports the error until it is written (1.2.1 forgot
    /// it at the first error, and the next flush said all was well).
    #[test]
    fn a_failed_write_back_is_kept_and_reported_until_written() {
        let fail = Arc::new(AtomicBool::new(false));
        let device = fail.clone();
        let fs = serve("r314small", true, move |image| {
            Box::new(Flaky(Overlay::new(image), device))
        });
        let ino = fs.inode("/small.txt", None);
        let fh = fs.open_inode(ino, 1).unwrap();
        fs.write_inode(ino, 0, b"kept").unwrap();
        fail.store(true, Ordering::SeqCst);
        assert_eq!(fs.write_back(ino), Err(Errno::EIO));
        assert_eq!(fs.write_back(ino), Err(Errno::EIO), "the second flush");
        assert_eq!(fs.release_inode(ino, fh), Err(Errno::EIO));
        assert!(fs.state.lock_ok().pending.contains_key(&ino), "the buffer went");
        fail.store(false, Ordering::SeqCst);
        fs.write_back(ino).unwrap();
        let vol = fs.vol.read_ok();
        let file = vol.open_file(&vol.lookup("/small.txt").unwrap()).unwrap();
        let mut buf = [0u8; 4];
        vol.read_stream(file.data.as_ref().unwrap(), 0, &mut buf).unwrap();
        assert_eq!(&buf, b"kept");
        assert_eq!(vol.check(&[]).unwrap().problems, Vec::<String>::new());
    }

    /// Every handler runs in `guard`: a panic in one answers its request
    /// (EIO, from its reply dropped unsent) and the next is served (1.2.1
    /// lost the serving thread with each panic: a read-only mount ended
    /// after four, a --rw mount after the first). A panic inside a change
    /// leaves the volume in memory half changed: nothing more is served.
    #[test]
    fn a_panic_in_one_handler_does_not_end_the_mount() {
        let served_after = std::cell::Cell::new(0);
        for writable in [false, true] {
            let fs = served(writable);
            fs.guard("read", || panic!("a handler fails"));
            fs.guard("getattr", || served_after.set(served_after.get() + 1));
            let root = fs.attr(&fs.vol.read_ok(), INodeNo::ROOT.0).unwrap();
            assert_eq!(root.kind, FileType::Directory);
        }
        assert_eq!(served_after.get(), 2);
        let fs = served(true);
        let r: Result<(), Errno> = fs.change("/a", |_| panic!("a change fails"));
        assert_eq!(r, Err(Errno::EIO));
        fs.guard("getattr", || served_after.set(served_after.get() + 1));
        assert_eq!(served_after.get(), 2, "nothing is served after a change stopped");
    }

    /// A lock whose holder panicked is used again (the handlers change what
    /// it guards in single steps).
    #[test]
    fn a_poisoned_lock_is_used_again() {
        let m = Arc::new(Mutex::new(1u32));
        let l = Arc::new(RwLock::new(1u32));
        let (m2, l2) = (m.clone(), l.clone());
        let _ = std::thread::spawn(move || {
            let _a = m2.lock_ok();
            let _b = l2.write_ok();
            panic!("a handler fails");
        })
        .join();
        assert!(m.is_poisoned() && l.is_poisoned());
        assert_eq!(*m.lock_ok(), 1);
        assert_eq!(*l.read_ok(), 1);
        *l.write_ok() = 2;
    }
}
