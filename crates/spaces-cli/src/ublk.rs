//! Exposing a space as a ublk block device (`/dev/ublkbN`): read-only, or
//! writable through a space writer (`--rw`).
//!
//! Each hardware queue runs in its own thread with a synchronous handler
//! that reads the requested range through the space reader (or writer)
//! straight into the queue's I/O buffer, or writes it from there. A
//! writable device declares a volatile cache, so the kernel sends flushes
//! (and turns FUA writes into a write and a flush); what is written is
//! flushed once more when the device goes away. The device lives until the
//! process receives SIGINT/SIGTERM or the device is deleted.

use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkFlags, UblkIORes};
use storage_spaces::io::ReadAt;
use storage_spaces::{Pool, SpaceWriter};

const QUEUES: u16 = 2;
const DEPTH: u16 = 64;
const IO_BUF_BYTES: u32 = 1 << 20;

/// Serves `source` (the space reader, or the writer of a writable device)
/// until the device is removed. `on_ready` receives the block device path
/// once the kernel exposes it.
/// A signal came (SIGINT, SIGTERM): the device goes as soon as it is there.
static STOPPING: AtomicBool = AtomicBool::new(false);
/// A queue stopped on a panic: the process ends with an error.
static PANICKED: AtomicBool = AtomicBool::new(false);

/// Stops the ublk device `dev_id`: its requests fail, its queues end.
fn kill(dev_id: u32) {
    if let Ok(ctrl) = libublk::ctrl::UblkCtrl::new_simple(dev_id as i32) {
        let _ = ctrl.kill_dev();
    }
}

/// Deletes the ublk device `dev_id` that a server of a space left behind
/// (killed before it could), unless a live process serves it: whether it
/// was there to delete.
pub fn remove_left(dev_id: u32) -> Result<bool> {
    let ctrl = libublk::ctrl::UblkCtrl::new_simple(dev_id as i32).context("cannot reach the ublk device")?;
    let info = ctrl.dev_info();
    let served = info.ublksrv_pid > 0 && std::path::Path::new(&format!("/proc/{}", info.ublksrv_pid)).exists();
    if served && u32::from(info.state) == libublk::sys::UBLK_S_DEV_LIVE {
        return Ok(false);
    }
    ctrl.del_dev().context("cannot delete the ublk device")?;
    Ok(true)
}

pub fn serve(
    pool: &'static Pool<File>,
    source: &'static dyn ReadAt,
    writer: Option<&'static SpaceWriter<'static, File>>,
    on_ready: impl FnOnce(&str) + Send + Sync + 'static,
) -> Result<()> {
    let ctrl = libublk::ctrl::UblkCtrlBuilder::default()
        .name("storage-spaces")
        .nr_queues(QUEUES)
        .depth(DEPTH)
        .io_buf_bytes(IO_BUF_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .context("cannot create the ublk device (is the ublk_drv module loaded?)")?;
    let dev_id = ctrl.dev_info().dev_id;

    // Killing a device that has not started yet does nothing: the ready
    // callback below stops one a signal came before.
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            STOPPING.store(true, Ordering::SeqCst);
            kill(dev_id);
        }
    });

    let size = source.size()?;
    let logical_shift = pool.logical_sector_size.trailing_zeros() as u8;
    let physical_shift = pool.physical_sector_size.max(pool.logical_sector_size).trailing_zeros() as u8;
    let tgt_init = move |dev: &mut UblkDev| {
        // Crash tests: time before the device starts, for a signal to come.
        #[cfg(feature = "test-hooks")]
        if let Some(ms) = std::env::var("SPACES_UBLK_DELAY_START")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        dev.set_default_params(size);
        let basic = &mut dev.tgt.params.basic;
        basic.attrs = if writer.is_some() {
            libublk::sys::UBLK_ATTR_VOLATILE_CACHE
        } else {
            libublk::sys::UBLK_ATTR_READ_ONLY
        };
        basic.logical_bs_shift = logical_shift;
        basic.physical_bs_shift = physical_shift;
        basic.io_min_shift = physical_shift;
        basic.io_opt_shift = physical_shift;
        if writer.is_some_and(|w| w.discards()) {
            // Discards give whole rows of thin spaces back to the pool.
            dev.tgt.params.types |= libublk::sys::UBLK_PARAM_TYPE_DISCARD;
            dev.tgt.params.discard = libublk::sys::ublk_param_discard {
                discard_granularity: 1 << physical_shift,
                max_discard_sectors: 1 << 22,
                max_discard_segments: 1,
                ..Default::default()
            };
        }
        Ok(())
    };
    let queue_fn = move |qid: u16, dev: &UblkDev| run_queue(qid, dev, source, writer);
    let ready = move |ctrl: &libublk::ctrl::UblkCtrl| {
        if STOPPING.load(Ordering::SeqCst) {
            let _ = ctrl.kill_dev();
            return;
        }
        on_ready(&format!("/dev/ublkb{}", ctrl.dev_info().dev_id));
    };
    ctrl.run_target(tgt_init, queue_fn, ready)
        .context("ublk device failed")?;
    let _ = ctrl.del_dev();
    if let Some(w) = writer {
        w.flush().context("cannot flush the space")?;
    }
    // libublk's run_target only reports a queue thread that panicked.
    if PANICKED.load(Ordering::SeqCst) {
        bail!("a ublk queue stopped on an internal error; the device is removed");
    }
    Ok(())
}

fn run_queue(qid: u16, dev: &UblkDev, source: &dyn ReadAt, writer: Option<&SpaceWriter<'_, File>>) {
    let mut bufs = dev.alloc_queue_io_bufs();
    let queue = match UblkQueue::new(qid, dev)
        .and_then(|q| q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs))))
    {
        Ok(q) => q,
        Err(e) => {
            eprintln!("ublk queue {qid} setup failed: {e}");
            return;
        }
    };
    let dev_id = dev.dev_info.dev_id;
    queue.wait_and_handle_io(move |q: &UblkQueue, tag: u16, _: &UblkIOCtx| {
        let iod = q.get_iod(tag);
        let (offset, len, op) = (
            iod.start_sector << 9,
            (iod.nr_sectors as usize) << 9,
            iod.op_flags & 0xff,
        );
        let buf = bufs[tag as usize].as_mut_slice();
        // A panic in the reader or writer fails this request and every one
        // after it (the device stops), and the process ends with an error.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle(op, offset, len, buf, source, writer)
        }))
        .unwrap_or_else(|_| {
            // Stopped from another thread: the kernel waits for the
            // requests in flight, this one among them.
            if !PANICKED.swap(true, Ordering::SeqCst) {
                std::thread::spawn(move || kill(dev_id));
            }
            -libc_errno::EIO
        });
        let desc = BufDesc::Slice(bufs[tag as usize].as_slice());
        if let Err(e) = q.complete_io_cmd_unified(tag, desc, Ok(UblkIORes::Result(result))) {
            eprintln!("ublk completion failed: {e}");
        }
    });
}

/// One request: the bytes done, or a negative errno.
fn handle(
    op: u32,
    offset: u64,
    len: usize,
    buf: &mut [u8],
    source: &dyn ReadAt,
    writer: Option<&SpaceWriter<'_, File>>,
) -> i32 {
    // Crash tests: a read that reaches this byte panics.
    #[cfg(feature = "test-hooks")]
    if op == libublk::sys::UBLK_IO_OP_READ
        && std::env::var("SPACES_UBLK_PANIC_AT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|at| (offset..offset + len as u64).contains(&at))
    {
        panic!("a read at {offset:#x} reached SPACES_UBLK_PANIC_AT");
    }
    {
        match op {
            libublk::sys::UBLK_IO_OP_READ if len <= buf.len() => match source.read_exact_at(&mut buf[..len], offset) {
                Ok(()) => len as i32,
                Err(e) => {
                    eprintln!("read of {len} bytes at {offset:#x} failed: {e}");
                    -libc_errno::EIO
                }
            },
            libublk::sys::UBLK_IO_OP_READ => -libc_errno::EINVAL,
            libublk::sys::UBLK_IO_OP_WRITE => match writer {
                Some(_) if len > buf.len() => -libc_errno::EINVAL,
                Some(w) => match w.write_all_at(&buf[..len], offset) {
                    Ok(()) => len as i32,
                    Err(e) => {
                        eprintln!("write of {len} bytes at {offset:#x} failed: {e}");
                        -libc_errno::EIO
                    }
                },
                None => -libc_errno::EROFS,
            },
            libublk::sys::UBLK_IO_OP_DISCARD => match writer.map(|w| w.discard(offset, len as u64)) {
                Some(Ok(())) => 0,
                Some(Err(e)) => {
                    eprintln!("discard of {len} bytes at {offset:#x} failed: {e}");
                    -libc_errno::EIO
                }
                None => -libc_errno::EROFS,
            },
            libublk::sys::UBLK_IO_OP_FLUSH => match writer.map_or(Ok(()), |w| w.flush()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("flush failed: {e}");
                    -libc_errno::EIO
                }
            },
            _ if writer.is_some() => -libc_errno::EOPNOTSUPP,
            _ => -libc_errno::EROFS,
        }
    }
}

/// The few errno values the handler returns (Linux numbering).
mod libc_errno {
    pub const EIO: i32 = 5;
    pub const EINVAL: i32 = 22;
    pub const EROFS: i32 = 30;
    pub const EOPNOTSUPP: i32 = 95;
}
