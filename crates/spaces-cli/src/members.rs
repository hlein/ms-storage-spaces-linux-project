//! Opening the member devices of a pool, held as a process needs them
//! (`spaces` and the test tool `spaces-pattern`).

use std::fs::File;
use std::path::Path;

use anyhow::{Result, bail};

/// How a process holds the members of a pool it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    /// Reading them for a moment (`info`, `check`): no lock.
    Read,
    /// Serving or exporting a space read-only: a shared lock on every
    /// member, which the servers of the pool's other spaces share and its
    /// writers do not get.
    Shared,
    /// Writing them (`--rw` servers, management): `O_EXCL` on block devices
    /// (also through a symlink), which fails while they are mounted,
    /// assembled or held by device-mapper, and an exclusive lock on every
    /// member, which fails while another process serves or writes them.
    Exclusive,
}

/// Opens a member, held as `hold` says.
pub(crate) fn open_member(p: &Path, write: bool, hold: Hold) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(write);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        if hold == Hold::Exclusive && std::fs::metadata(p).is_ok_and(|m| m.file_type().is_block_device()) {
            options.custom_flags(libc::O_EXCL);
        }
    }
    let file = match options.open(p) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
            bail!("{} is in use by {}", p.display(), kernel_holders(p))
        }
        Err(e) => return Err(anyhow::Error::new(e).context(format!("cannot open {}", p.display()))),
    };
    if hold == Hold::Read {
        return Ok(file);
    }
    lock(&file, p, hold)?;
    // A partition's whole disk is locked too, as its own path takes it: the
    // two paths of one pool exclude each other (1.3.0 locked the path given
    // alone: a writer through /dev/sdb went ahead while a server held
    // /dev/sdb2, and the other way round; audit 2026-10-21, SCA-03). The
    // disk is not opened with O_EXCL, which the partition's claim refuses.
    // A whole disk this process may not open (a user given the partition
    // alone) is passed over: the partition's own lock stands.
    if let Some((disk, whole)) = whole_disk(p).and_then(|d| Some((d.clone(), File::open(d).ok()?))) {
        lock(&whole, &disk, hold)?;
        HELD.lock().unwrap_or_else(|e| e.into_inner()).push(whole);
    }
    Ok(file)
}

/// The whole disks locked for the partitions opened, held to the end.
static HELD: std::sync::Mutex<Vec<File>> = std::sync::Mutex::new(Vec::new());

/// Locks `file` (of path `p`), shared or exclusive as `hold` says.
fn lock(file: &File, p: &Path, hold: Hold) -> Result<()> {
    let locked = match hold {
        Hold::Read => return Ok(()),
        Hold::Shared => file.try_lock_shared(),
        Hold::Exclusive => file.try_lock(),
    };
    locked.map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => anyhow::anyhow!(
            "{} is in use by {}",
            p.display(),
            storage_spaces::holders::of_lock(file)
        ),
        std::fs::TryLockError::Error(e) => anyhow::Error::new(e).context(format!("cannot lock {}", p.display())),
    })
}

/// The whole disk of block device `p` when it is a partition (from /sys:
/// a partition's directory is in its disk's).
fn whole_disk(p: &Path) -> Option<std::path::PathBuf> {
    let real = std::fs::canonicalize(p).ok()?;
    let name = real.file_name()?.to_str()?;
    let sys = std::fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?;
    if !sys.join("partition").exists() {
        return None;
    }
    let disk = sys.parent()?.file_name()?.to_str()?;
    Some(Path::new("/dev").join(disk))
}

/// What holds block device `p` in the kernel (device-mapper tables, from
/// /sys), as people read it: of a disk, what holds its partitions too
/// (1.3.0 looked at the disk's own holders, and a disk whose partition a
/// device-mapper table held was "in use by the kernel"; audit 2026-10-21,
/// SCA-03).
fn kernel_holders(p: &Path) -> String {
    let sys = std::fs::canonicalize(p)
        .ok()
        .and_then(|real| Some(real.file_name()?.to_str()?.to_string()))
        .and_then(|name| std::fs::canonicalize(Path::new("/sys/class/block").join(name)).ok());
    // Its own holders, and its partitions' (directories with a `partition`
    // file in its own).
    let mut dirs: Vec<std::path::PathBuf> = sys.iter().map(|s| s.join("holders")).collect();
    if let Some(entries) = sys.as_ref().and_then(|s| std::fs::read_dir(s).ok()) {
        dirs.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|d| d.join("partition").exists())
                .map(|d| d.join("holders")),
        );
    }
    let held: Vec<String> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|e| {
            let holder = e.ok()?.file_name().to_string_lossy().into_owned();
            let dm = std::fs::read_to_string(format!("/sys/block/{holder}/dm/name")).ok();
            Some(match dm {
                Some(name) => format!("device-mapper {} ({holder})", name.trim()),
                None => holder,
            })
        })
        .collect();
    if held.is_empty() {
        "the kernel (mounted, assembled or opened exclusively)".into()
    } else {
        held.join(", ")
    }
}

/// A sysfs attribute of a block device (through its parent for partitions:
/// `queue/logical_block_size` is the disk's), from its kernel name
/// (/dev/mapper and /dev/disk/by-* paths are links to it). The one reader
/// of `spaces` (1.2.2 had one in attach and one in the management commands).
pub(crate) fn block_attr(dev: &Path, attr: &str) -> Option<String> {
    let dev = std::fs::canonicalize(dev).ok()?;
    let name = dev.file_name()?.to_str()?;
    let sys = std::fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?;
    let base = if sys.join("partition").exists() {
        sys.parent()?.to_path_buf()
    } else {
        sys
    };
    std::fs::read_to_string(base.join(attr))
        .ok()
        .map(|s| s.trim().to_string())
}
