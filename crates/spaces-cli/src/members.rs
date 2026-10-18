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
    let locked = match hold {
        Hold::Read => return Ok(file),
        Hold::Shared => file.try_lock_shared(),
        Hold::Exclusive => file.try_lock(),
    };
    locked.map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => anyhow::anyhow!(
            "{} is in use by {}",
            p.display(),
            storage_spaces::holders::of_lock(&file)
        ),
        std::fs::TryLockError::Error(e) => anyhow::Error::new(e).context(format!("cannot lock {}", p.display())),
    })?;
    Ok(file)
}

/// What holds block device `p` in the kernel (device-mapper tables, from
/// /sys), as people read it.
fn kernel_holders(p: &Path) -> String {
    let held: Vec<String> = std::fs::canonicalize(p)
        .ok()
        .and_then(|real| Some(real.file_name()?.to_str()?.to_string()))
        .and_then(|name| {
            let dir = Path::new("/sys/class/block").join(&name).join("holders");
            std::fs::read_dir(dir).ok()
        })
        .into_iter()
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
