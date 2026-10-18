//! Attaching spaces as block devices and detaching them again.
//!
//! Every attached space appears as `/dev/mapper/ss-<pool>-<space>` with one
//! `…-p<N>` device per partition, whatever backend serves it:
//!
//! * `dm`: a device-mapper table straight over the member devices (kernel
//!   only, no process); used when the space has a linear description and
//!   its sector size matches the members'.
//! * `ublk`, `nbd`, `fuse`: a `spaces serve-*` process run as a transient
//!   systemd unit, wrapped by a linear device-mapper device.
//!
//! Spaces are attached read-only unless `--rw` is given. Writable spaces
//! must pass the checks of the space writer (clean pool, healthy space, a
//! resiliency writes can keep consistent). dm serves them only for simple
//! spaces that are fully allocated and whose write-back cache holds
//! nothing (dm cannot log to the cache, and its zero targets would drop
//! writes into unallocated rows); others are served by `serve-ublk --rw`
//! or `serve-nbd --rw`, which flush when they are stopped. FUSE is
//! read-only.
//!
//! Every space is checked first (`spaces check`): only healthy ones are
//! attached as asked, others read-only with `--degraded` or `--force`, and
//! failed ones not at all. The report goes to
//! `/run/storage-spaces/reports/<space guid>.{txt,json}`.
//!
//! State lives in `/run/storage-spaces/<space guid>.state`. It is written
//! before anything is set up, marked attaching, with the server unit and
//! the device-mapper devices the attach makes, and again after each step;
//! marked attached at the end. Every device-mapper device carries the UUID
//! `STORAGESPACES-<pool guid>-<space guid>[-p<n>]`, and only devices with
//! the UUID of the space are removed for it. An attach that was cut off
//! (killed, a crash, a timeout) leaves its state attaching: `attach` and
//! `detach` undo it first. Devices with such a UUID and no state (a state
//! that was lost) are leftovers, reported by `status` and removed by
//! `detach --leftovers`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use storage_spaces::report::readable_dir;

use anyhow::{Context, Result, bail};
use storage_spaces::gpt::Partition;
use storage_spaces::report::{Report, Verdict};
use storage_spaces::segments::SegmentKind;
use storage_spaces::{Pool, Space, SpaceReader};

pub const STATE_DIR: &str = "/run/storage-spaces";
/// Where the reports of the checks go, one per space.
pub const REPORT_DIR: &str = "/run/storage-spaces/reports";
/// Where attach leaves udev the properties of the devices it creates
/// (`<name>.props`, imported by contrib/udev/69-storage-spaces.rules).
pub const UDEV_DIR: &str = "/run/storage-spaces/udev";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    Auto,
    Dm,
    Ublk,
    Nbd,
    Fuse,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Dm => "dm",
            Backend::Ublk => "ublk",
            Backend::Nbd => "nbd",
            Backend::Fuse => "fuse",
        }
    }
}

/// How far the attach of a space got.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Attached by 1.2, whose devices carry no UUID; all it lists are its.
    #[default]
    Legacy,
    /// Being attached, or an attach that was cut off: of the devices it
    /// lists, those that exist with the space's UUID are its.
    Attaching,
    Attached,
}

/// The prefix of the device-mapper UUIDs of attached spaces.
pub const DM_UUID_PREFIX: &str = "STORAGESPACES-";

/// What was set up for one space; removed in reverse order.
#[derive(Debug, Default, Clone)]
pub struct State {
    pub status: Status,
    pub space_guid: String,
    pub pool_guid: String,
    pub backend: String,
    /// systemd unit of the serving process, if any.
    pub unit: Option<String>,
    /// Kernel device of the backend (ublk, nbd or loop device).
    pub device: Option<String>,
    pub fuse_mount: Option<String>,
    /// Device-mapper names, whole space first.
    pub dm: Vec<String>,
    /// Attached read-write.
    pub rw: bool,
    /// The verdict of the checks when it was attached.
    pub verdict: Option<String>,
    /// Attached read-only past a verdict other than healthy (--degraded,
    /// --force).
    pub forced: bool,
    /// The report of the checks.
    pub report: Option<String>,
    /// The arguments of `spaces attach` that attach the space again as it
    /// is (`detach --outdated` does after an upgrade).
    pub args: Vec<String>,
}

impl State {
    fn path(space_guid: &str) -> PathBuf {
        Path::new(STATE_DIR).join(format!("{space_guid}.state"))
    }

    /// What the state lists, for a message.
    fn what(&self) -> String {
        let mut parts = Vec::new();
        if !self.dm.is_empty() {
            parts.push(format!("device-mapper {}", self.dm.join(", ")));
        }
        if let Some(d) = &self.device {
            parts.push(format!("device {d}"));
        }
        if let Some(u) = &self.unit {
            parts.push(format!("server {u}"));
        }
        if let Some(m) = self.fuse_mount.as_ref().filter(|m| is_mount_point(m)) {
            parts.push(format!("mount {m}"));
        }
        if parts.is_empty() {
            "its state".into()
        } else {
            parts.join("; ")
        }
    }

    /// What [`State::save`] writes.
    fn text(&self) -> String {
        let mut text = format!(
            "space={}\npool={}\nbackend={}\n",
            self.space_guid, self.pool_guid, self.backend
        );
        match self.status {
            Status::Legacy => {}
            Status::Attaching => text += "status=attaching\n",
            Status::Attached => text += "status=attached\n",
        }
        for (key, value) in [
            ("unit", &self.unit),
            ("device", &self.device),
            ("fuse_mount", &self.fuse_mount),
            ("verdict", &self.verdict),
            ("report", &self.report),
        ] {
            if let Some(v) = value {
                text += &format!("{key}={v}\n");
            }
        }
        for d in &self.dm {
            text += &format!("dm={d}\n");
        }
        for a in &self.args {
            text += &format!("arg={a}\n");
        }
        if self.rw {
            text += "mode=rw\n";
        }
        if self.forced {
            text += "forced=1\n";
        }
        text
    }

    /// Writes the state (a new file renamed over the old one, both synced).
    fn save(&self) -> Result<()> {
        ensure_state_dir()?;
        let text = self.text();
        let tmp = Self::path(&self.space_guid).with_extension("tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
            f.sync_all()?;
        }
        fs::rename(tmp, Self::path(&self.space_guid))?;
        File::open(STATE_DIR)?.sync_all()?;
        Ok(())
    }

    fn parse(text: &str) -> State {
        let mut s = State::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.to_string();
            match key {
                "status" if value == "attaching" => s.status = Status::Attaching,
                "status" => s.status = Status::Attached,
                "space" => s.space_guid = value,
                "pool" => s.pool_guid = value,
                "backend" => s.backend = value,
                "unit" => s.unit = Some(value),
                "device" => s.device = Some(value),
                "fuse_mount" => s.fuse_mount = Some(value),
                "dm" => s.dm.push(value),
                "mode" => s.rw = value == "rw",
                "verdict" => s.verdict = Some(value),
                "forced" => s.forced = value == "1",
                "report" => s.report = Some(value),
                "arg" => s.args.push(value),
                _ => {}
            }
        }
        s
    }

    /// The attached spaces. A state directory this user may not read is an
    /// error, not "none attached".
    pub fn load_all() -> Result<Vec<State>> {
        load_states(Path::new(STATE_DIR))
    }
}

fn load_states(dir: &Path) -> Result<Vec<State>> {
    let denied = |e: &std::io::Error| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            " (run as root)"
        } else {
            ""
        }
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("cannot read {}: {e}{}", dir.display(), denied(&e)),
    };
    let mut states = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "state") {
            match fs::read_to_string(&path) {
                Ok(text) => states.push(State::parse(&text)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => bail!("cannot read {}: {e}{}", path.display(), denied(&e)),
            }
        }
    }
    states.sort_by(|a, b| a.dm.first().cmp(&b.dm.first()));
    Ok(states)
}

/// Creates the state directory, readable by everyone (`spaces status`
/// works without root) whatever the umask (the attach unit runs with 0077),
/// and makes an existing one so. What it holds is not secret: which spaces
/// are attached and how; the NBD sockets in it are 0600 themselves.
fn ensure_state_dir() -> Result<()> {
    readable_dir(Path::new(STATE_DIR)).with_context(|| format!("cannot make {STATE_DIR}"))
}

/// Writes `report` (on one space) as `<space guid>.txt` and `.json` into
/// the report directory, readable by everyone as the state directory is;
/// returns the text's path.
pub fn write_report(report: &Report) -> Result<PathBuf> {
    write_report_in(Path::new(REPORT_DIR), report).with_context(|| format!("cannot write a report into {REPORT_DIR}"))
}

fn write_report_in(dir: &Path, report: &Report) -> Result<PathBuf> {
    let guid = &report.space.as_ref().context("a report on no space")?.1;
    Ok(storage_spaces::report::write_files(report, dir, guid)?)
}

/// Removes the report on space `guid`.
pub fn remove_report(guid: &str) {
    for ext in ["txt", "json"] {
        let _ = fs::remove_file(Path::new(REPORT_DIR).join(format!("{guid}.{ext}")));
    }
}

/// The spaces checked and not attached: each report's path and first line
/// (the space, its verdict and why it was refused). A report whose devices
/// are all gone (unplugged since) is left out: 1.2.2 listed it until the
/// next boot.
pub fn refused(attached: &[State]) -> Vec<(PathBuf, String)> {
    refused_in(Path::new(REPORT_DIR), attached)
}

fn refused_in(dir: &Path, attached: &[State]) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, String)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .filter(|p| {
            let guid = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            !attached.iter().any(|s| s.space_guid.eq_ignore_ascii_case(guid))
        })
        .filter_map(|p| {
            let text = fs::read_to_string(&p).ok()?;
            let devices = storage_spaces::report::devices_in_text(&text);
            if !devices.is_empty() && !devices.iter().any(|d| Path::new(d).exists()) {
                return None;
            }
            Some((p, text.lines().next()?.to_string()))
        })
        .collect();
    out.sort();
    out
}

/// What the udev rule imports for a device of attached space `state`:
/// whose it is, its verdict and whether it was attached past it, and
/// SS_BACKEND=1 for the device the space is served through (ublk, nbd or
/// loop: it and its kernel partitions duplicate the space's devices).
fn props_text(state: &State, backend: bool) -> String {
    let mut s = format!(
        "SS_SPACE={}\nSS_POOL={}\nSS_VERDICT={}\nSS_FORCED={}\n",
        state.space_guid,
        state.pool_guid,
        state.verdict.as_deref().unwrap_or("healthy"),
        u8::from(state.forced)
    );
    if backend {
        s += "SS_BACKEND=1\n";
    }
    s
}

fn write_props(name: &str, state: &State, backend: bool) -> Result<()> {
    write_props_in(Path::new(UDEV_DIR), name, state, backend).with_context(|| format!("cannot write into {UDEV_DIR}"))
}

fn write_props_in(dir: &Path, name: &str, state: &State, backend: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    readable_dir(dir)?;
    let path = dir.join(format!("{name}.props"));
    let tmp = dir.join(format!("{name}.props.tmp"));
    fs::write(&tmp, props_text(state, backend))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn remove_props(name: &str) {
    let _ = fs::remove_file(Path::new(UDEV_DIR).join(format!("{name}.props")));
}

/// The kernel name of a device path (/dev/ublkb0: ublkb0).
fn kernel_name(dev: &str) -> &str {
    dev.rsplit('/').next().unwrap_or(dev)
}

/// Writes the properties of the device a space is served through and has
/// udev take them in for it and its partitions (it announced them before
/// they were written).
fn mark_backend(dev: &str, state: &State) -> Result<()> {
    let kernel = kernel_name(dev);
    write_props(kernel, state, true)?;
    let names = [
        format!("--sysname-match={kernel}"),
        format!("--sysname-match={kernel}p*"),
    ];
    let args = [
        "trigger",
        "--action=change",
        "--subsystem-match=block",
        &names[0],
        &names[1],
    ];
    // Not `trigger --settle`: at boot, while udev worked through the
    // coldplug, it waited for good for events udev had handled (systemd
    // 261), and the attach unit timed out. settle waits for udev's queue,
    // and not for ever.
    run("udevadm", &args, None).ok();
    settle();
    Ok(())
}

/// Waits for udev to work through its queue: 30 s at most at a time, and
/// 60 s in all in one run of `spaces attach` (the attach unit may take 120
/// s; past that, devices appear when udev gets to them).
fn settle() {
    static SPENT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let left = 60_000u64.saturating_sub(SPENT_MS.load(std::sync::atomic::Ordering::Relaxed));
    let left = crate::check::time_left().map_or(left, |t| left.min(t.as_millis() as u64));
    if left == 0 {
        return;
    }
    let timeout = format!("--timeout={}", (left / 1000).clamp(1, 30));
    let start = std::time::Instant::now();
    run("udevadm", &["settle", &timeout], None).ok();
    SPENT_MS.fetch_add(start.elapsed().as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
}

/// The most partitions of a space that become devices: a GPT holds 128
/// by default, and each device costs two programs and a wait for udev.
const MAX_PARTITION_DEVICES: usize = 128;

/// The partitions of a space of `size` bytes that become devices: those of
/// its table, if they lie inside it and apart and are not more than
/// [`MAX_PARTITION_DEVICES`]; else why none do.
fn partition_devices(parts: Vec<Partition>, size: u64) -> std::result::Result<Vec<Partition>, String> {
    if parts.len() > MAX_PARTITION_DEVICES {
        return Err(format!(
            "{} partitions, more than the {MAX_PARTITION_DEVICES} attach makes devices for",
            parts.len()
        ));
    }
    storage_spaces::gpt::check_partitions(&parts, size).map_err(|e| e.to_string())?;
    Ok(parts)
}

/// The device-mapper name the space `space_guid` of the pool `pool_guid`
/// gets: `base` (its `dm_name`), unless another attached space has it. Of
/// another pool (pools of one name, as Windows calls every pool "Storage
/// pool" by default), the first eight hex digits of this pool's GUID
/// follow; of the same pool (two names `dm_name` makes alike, such as
/// "Фото" and "Игры", or one name twice), those of the space's GUID. A
/// device of that name without an attached space's state stays an error
/// (left over from an interrupted run).
fn unique_dm_name(base: String, pool_guid: &str, space_guid: &str, attached: &[State]) -> String {
    let short = |guid: &str| -> String { guid.chars().filter(char::is_ascii_hexdigit).take(8).collect() };
    let holder = attached
        .iter()
        .find(|s| s.dm.first() == Some(&base) && !s.space_guid.eq_ignore_ascii_case(space_guid));
    match holder {
        None => base,
        Some(s) if !s.pool_guid.eq_ignore_ascii_case(pool_guid) => format!("{base}-{}", short(pool_guid)),
        Some(_) => format!("{base}-{}", short(space_guid)),
    }
}

/// The device-mapper UUID of the device `name` of `state`: the space's,
/// `STORAGESPACES-<pool guid>-<space guid>`, with `-p<n>` for a partition.
pub fn dm_uuid(state: &State, name: &str) -> String {
    let base = format!("{DM_UUID_PREFIX}{}-{}", state.pool_guid, state.space_guid);
    match state.dm.first().and_then(|whole| name.strip_prefix(whole.as_str())) {
        Some(part) => base + part,
        None => base,
    }
}

/// The UUID of the device-mapper device `name`, if it exists ("" for a
/// device without one).
fn dm_uuid_of(name: &str) -> Option<String> {
    if !Path::new("/dev/mapper").join(name).exists() {
        return None;
    }
    let out = run("dmsetup", &["info", "-c", "--noheadings", "-o", "uuid", name], None).ok()?;
    Some(out.trim().to_string())
}

/// Whether the device-mapper name `dm` is that of a space named `space`:
/// `ss-<pool>-<space>`, or with a pool GUID's eight hex digits after it.
pub fn names_space(dm: &str, space: &str) -> bool {
    let suffix = format!("-{space}");
    let unsuffixed = match dm.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_hexdigit()) => head,
        _ => dm,
    };
    dm.ends_with(&suffix) || unsuffixed.ends_with(&suffix)
}

/// The attached spaces `key` names (all without one): by space GUID (in
/// any case), device-mapper name, or space name as the device-mapper name
/// ends ([`names_space`]). A key that names several (spaces of one name in
/// two pools) is an error that lists them: `spaces detach` takes no space
/// for another.
pub fn select_attached(states: Vec<State>, key: Option<&str>) -> Result<Vec<State>> {
    let Some(key) = key else {
        return Ok(states);
    };
    let found: Vec<State> = states
        .into_iter()
        .filter(|s| {
            key.eq_ignore_ascii_case(&s.space_guid) || s.dm.first().is_some_and(|d| d == key || names_space(d, key))
        })
        .collect();
    if found.len() > 1 {
        bail!(
            "{key:?} names {} attached spaces: {}; name the one meant by its GUID or device-mapper name \
             (spaces status lists them)",
            found.len(),
            found
                .iter()
                .map(|s| format!("{} ({})", s.dm.first().map_or("-", |d| d.as_str()), s.space_guid))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(found)
}

/// Device-mapper name for a space: `ss-<pool>-<space>` with unsafe characters replaced.
pub fn dm_name(pool: &str, space: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "_.+".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("ss-{}-{}", clean(pool), clean(space));
    name.truncate(100);
    name
}

/// How long an external program may take (dmsetup, losetup, nbd-client,
/// systemctl, udevadm, journalctl): at most 120 s, beyond systemd's default
/// stop timeout, and no longer than what is left of the run's time. 1.2.2
/// waited for ever: a program hung on a device held the run, and the lock
/// of every later one.
const PROGRAM_TIME: Duration = Duration::from_secs(120);

fn run(program: &str, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let limit = crate::check::time_left().map_or(PROGRAM_TIME, |left| left.min(PROGRAM_TIME));
    run_within(program, args, stdin, limit.max(Duration::from_secs(5)))
}

/// Runs `program`, stopped (SIGKILL) when it takes longer than `limit`.
fn run_within(program: &str, args: &[&str], stdin: Option<&str>, limit: Duration) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().with_context(|| format!("cannot run {program}"))?;
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    // The output is read beside the wait: a full pipe would stop it.
    fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut v);
            }
            v
        })
    }
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{program} {} did not finish within {} s, and was stopped",
                args.join(" "),
                limit.as_secs_f32()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&err).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Makes the device-mapper device `name` and waits for udev to take it in.
fn dm_create(name: &str, uuid: &str, table: &str, rw: bool) -> Result<()> {
    dm_create_unsettled(name, uuid, table, rw)?;
    settle();
    Ok(())
}

/// [`dm_create`] without waiting for udev (several in a row, then one wait).
fn dm_create_unsettled(name: &str, uuid: &str, table: &str, rw: bool) -> Result<()> {
    if Path::new("/dev/mapper").join(name).exists() {
        bail!(
            "device-mapper device {name} already exists (left over from an interrupted run?); spaces status names \
             leftovers, and spaces detach --leftovers removes them"
        );
    }
    let mut args = vec!["create", "--uuid", uuid];
    if !rw {
        args.push("--readonly");
    }
    args.push(name);
    run("dmsetup", &args, Some(table))?;
    Ok(())
}

fn dm_remove(name: &str) -> Result<()> {
    if Path::new("/dev/mapper").join(name).exists() {
        run("dmsetup", &["remove", "--retry", name], None)?;
    }
    Ok(())
}

/// Logical block size of a block device (through its parent for partitions).
fn logical_block_size(dev: &Path) -> Option<u32> {
    crate::members::block_attr(dev, "queue/logical_block_size")?
        .parse()
        .ok()
}

fn dm_table(reader: &SpaceReader<'_, File>, paths: &[PathBuf]) -> Result<String> {
    let mut table = String::new();
    for seg in reader.segments()? {
        let (start, length) = (seg.start / 512, seg.length / 512);
        let line = match seg.kind {
            SegmentKind::Zero => format!("{start} {length} zero"),
            SegmentKind::Striped { stripes, .. } if stripes.len() == 1 => {
                format!(
                    "{start} {length} linear {} {}",
                    paths[stripes[0].device].display(),
                    stripes[0].offset / 512
                )
            }
            SegmentKind::Striped { chunk, stripes } => {
                let devs: Vec<String> = stripes
                    .iter()
                    .map(|s| format!("{} {}", paths[s.device].display(), s.offset / 512))
                    .collect();
                format!(
                    "{start} {length} striped {} {} {}",
                    stripes.len(),
                    chunk / 512,
                    devs.join(" ")
                )
            }
        };
        table += &line;
        table.push('\n');
    }
    Ok(table)
}

fn available(backend: Backend) -> bool {
    let has = |program: &str| run("sh", &["-c", &format!("command -v {program}")], None).is_ok();
    let module = |m: &str| Path::new("/sys/module").join(m).exists() || run("modprobe", &[m], None).is_ok();
    match backend {
        Backend::Dm | Backend::Auto => has("dmsetup"),
        Backend::Ublk => cfg!(feature = "ublk") && module("ublk_drv") && Path::new("/dev/ublk-control").exists(),
        Backend::Nbd => has("nbd-client") && module("nbd"),
        Backend::Fuse => cfg!(feature = "fuse") && Path::new("/dev/fuse").exists() && has("losetup"),
    }
}

/// Why dm cannot serve the space read-write, if it cannot.
fn dm_write_refusal(reader: &SpaceReader<'_, File>) -> Option<&'static str> {
    if reader.layout().resiliency != storage_spaces::format::Resiliency::Simple {
        return Some("dm writes one copy only; mirror and parity spaces need their logs kept");
    }
    if reader.cache().is_some_and(|c| c.cached_chunks() > 0) {
        return Some("the write-back cache holds data, which dm would not see");
    }
    match reader.segments() {
        Ok(segments) if segments.iter().any(|s| matches!(s.kind, SegmentKind::Zero)) => {
            Some("the space has unallocated rows, whose writes dm would drop")
        }
        Ok(_) => None,
        Err(_) => Some("dm cannot map this space"),
    }
}

/// Picks the backend for a space.
fn choose(
    requested: Backend,
    reader: &SpaceReader<'_, File>,
    paths: &[PathBuf],
    sector: u32,
    rw: bool,
) -> Result<Backend> {
    if rw && requested == Backend::Fuse {
        bail!("the fuse backend is read-only");
    }
    if requested != Backend::Auto {
        if requested == Backend::Dm {
            reader.segments().context("the dm backend cannot map this space")?;
            if let Some(why) = dm_write_refusal(reader).filter(|_| rw) {
                bail!("the dm backend cannot serve this space read-write: {why}");
            }
        }
        if !available(requested) {
            bail!("backend {} is not available on this system", requested.name());
        }
        return Ok(requested);
    }
    // dm maps one copy only, so it is used for simple spaces, where no
    // redundancy is lost; mirror and parity spaces keep failing over to
    // other copies when served by a process.
    let sectors_match = paths.iter().all(|p| logical_block_size(p) == Some(sector));
    let simple = reader.layout().resiliency == storage_spaces::format::Resiliency::Simple;
    let dm_ok = if rw {
        dm_write_refusal(reader).is_none()
    } else {
        reader.segments().is_ok()
    };
    if simple && dm_ok && sectors_match && available(Backend::Dm) {
        return Ok(Backend::Dm);
    }
    let fallbacks: &[Backend] = if rw {
        &[Backend::Ublk, Backend::Nbd]
    } else {
        &[Backend::Ublk, Backend::Nbd, Backend::Fuse]
    };
    for &b in fallbacks {
        if available(b) {
            return Ok(b);
        }
    }
    bail!("no usable backend (install dmsetup, or load ublk_drv or nbd)")
}

/// Waits for `path` to appear while the systemd unit `unit` runs, at most
/// `timeout`.
fn wait_for(path: &Path, unit: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut checked = Instant::now();
    while !path.exists() {
        if start.elapsed() > timeout {
            bail!("timed out waiting for {}", path.display());
        }
        if checked.elapsed() > Duration::from_secs(2) {
            checked = Instant::now();
            if run("systemctl", &["is-active", "--quiet", unit], None).is_err() && !path.exists() {
                bail!("the serving process ended before it was ready");
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Starts `spaces serve-<kind>` as a transient systemd unit and waits for
/// its ready file.
/// What a server parsing a pool's disks (hostile input, run as root) may
/// not do. Nothing here changes what it can open or mount.
const SERVER_RESTRICTIONS: &[&str] = &[
    "NoNewPrivileges=yes",
    "ProtectControlGroups=yes",
    "ProtectKernelLogs=yes",
    "ProtectHostname=yes",
    "RestrictNamespaces=yes",
    "RestrictRealtime=yes",
    "RestrictSUIDSGID=yes",
    "LockPersonality=yes",
    "SystemCallArchitectures=native",
    "MemoryDenyWriteExecute=yes",
    "UMask=0077",
    "RestrictAddressFamilies=AF_UNIX AF_NETLINK",
    "CapabilityBoundingSet=~CAP_SYS_PTRACE CAP_SYS_BOOT CAP_SYS_TIME CAP_SYS_PACCT CAP_SYS_TTY_CONFIG \
        CAP_SYSLOG CAP_NET_ADMIN CAP_NET_RAW CAP_AUDIT_CONTROL CAP_AUDIT_READ CAP_AUDIT_WRITE \
        CAP_LINUX_IMMUTABLE CAP_MAC_ADMIN CAP_MAC_OVERRIDE CAP_WAKE_ALARM CAP_BLOCK_SUSPEND \
        CAP_PERFMON CAP_BPF CAP_CHECKPOINT_RESTORE CAP_SETPCAP CAP_SYS_CHROOT CAP_SETUID CAP_SETGID \
        CAP_KILL",
];

/// Restrictions that give the server its own view of the file system: not
/// for the FUSE server, whose mount must be seen by the host. Home
/// directories stay readable (pool disks may be image files there).
const SERVER_PRIVATE_VIEW: &[&str] = &[
    "ProtectSystem=strict",
    "ReadWritePaths=/run",
    "ProtectHome=read-only",
    // Not PrivateTmp=yes: it orders the server after
    // systemd-tmpfiles-setup.service, that is after local-fs.target, which
    // at boot waits for the attach service that waits for this server.
    // Not /var/tmp: with /var a file system of its own, not mounted yet at
    // boot, systemd would make the directory on the root, still read-only
    // then, and the unit would fail.
    "TemporaryFileSystem=/tmp",
    "ProtectKernelTunables=yes",
    "ProtectProc=invisible",
];

/// A path quoted for a unit file's list of paths: in double quotes, with
/// backslashes and quotes escaped (a path may hold spaces).
fn unit_quoted(path: &str) -> String {
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

fn start_server(unit: &str, kind: &str, paths: &[PathBuf], space: &str, extra: &[&str]) -> Result<String> {
    ensure_state_dir()?;
    let ready = Path::new(STATE_DIR).join(format!("{unit}.ready"));
    let _ = fs::remove_file(&ready);
    let exe = std::env::current_exe()?;
    // The attach service runs early at boot (before local-fs-pre.target);
    // with default dependencies the server unit would wait for
    // sysinit.target and deadlock with it.
    let mut args: Vec<String> = vec![
        format!("--unit={unit}"),
        "--collect".into(),
        "--quiet".into(),
        "--property=DefaultDependencies=no".into(),
        "--property=Conflicts=shutdown.target".into(),
        "--property=Before=shutdown.target".into(),
        // Stopped at shutdown after local-fs-pre.target, which comes after
        // every local mount is unmounted: the volumes on the space first.
        "--property=Before=local-fs-pre.target".into(),
    ];
    let private = if kind == "fuse" { &[][..] } else { SERVER_PRIVATE_VIEW };
    let mut restrictions: Vec<String> = SERVER_RESTRICTIONS
        .iter()
        .chain(private)
        .map(|p| format!("--property={p}"))
        .collect();
    // A --rw server writes the image files among the members, which its
    // read-only view of the system and of /home would refuse (1.2.2: EROFS).
    if !private.is_empty() && extra.contains(&"--rw") {
        for p in paths.iter().filter(|p| fs::metadata(p).is_ok_and(|m| m.is_file())) {
            let real = fs::canonicalize(p).with_context(|| format!("cannot resolve {}", p.display()))?;
            restrictions.push(format!(
                "--property=ReadWritePaths={}",
                unit_quoted(&real.display().to_string())
            ));
        }
    }
    let head = std::mem::take(&mut args);
    args.extend(["--".into(), exe.display().to_string()]);
    args.push(format!("serve-{kind}"));
    args.extend(paths.iter().map(|p| p.display().to_string()));
    args.extend([
        "--space".into(),
        space.into(),
        "--ready-file".into(),
        ready.display().to_string(),
    ]);
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend(crate::inherited_args());
    let launch = |limits: &[String]| {
        let all: Vec<&str> = head.iter().chain(limits).chain(&args).map(String::as_str).collect();
        run("systemd-run", &all, None)
    };
    // A systemd that does not know one of the properties (they date from
    // 231 to 247) refuses the whole unit: start the server without limits
    // rather than not at all, and say so.
    if let Err(e) = launch(&restrictions) {
        eprintln!("warning: starting {unit} with limits failed ({e:#}); starting it without");
        launch(&[])?;
    }
    // Opening a space read-write destages its write-back cache first, which
    // can take minutes; never past the time of the run.
    let timeout = Duration::from_secs(if extra.contains(&"--rw") { 3600 } else { 30 });
    let timeout = crate::check::time_left().map_or(timeout, |left| timeout.min(left));
    if let Err(e) = wait_for(&ready, unit, timeout) {
        let _ = run("systemctl", &["stop", unit], None);
        let log = run("journalctl", &["-u", unit, "-n", "20", "--no-pager"], None).unwrap_or_default();
        bail!("{e}; unit log:\n{log}");
    }
    let content = fs::read_to_string(&ready)?;
    let _ = fs::remove_file(&ready);
    Ok(content.trim().to_string())
}

fn free_nbd() -> Result<String> {
    for i in 0..128 {
        let size = fs::read_to_string(format!("/sys/block/nbd{i}/size")).unwrap_or_default();
        if size.trim() == "0" && !Path::new(&format!("/sys/block/nbd{i}/pid")).exists() {
            return Ok(format!("/dev/nbd{i}"));
        }
    }
    bail!("no free NBD device")
}

/// How long a run started by hand waits for another attach or detach.
const LOCK_WAIT: Duration = Duration::from_secs(600);

/// Serializes attach and detach runs (udev can start one while another
/// runs): waits for the other one, saying which it is, until the time of
/// the run is up (`SPACES_ATTACH_TIMEOUT` of the units), or 10 minutes for
/// a run started by hand. 1.2.2 waited for ever: a run hung on a dead disk
/// held up every later one, `spaces detach` included.
pub fn lock() -> Result<File> {
    ensure_state_dir()?;
    lock_in(Path::new(STATE_DIR), crate::check::time_left().unwrap_or(LOCK_WAIT))
}

fn lock_in(dir: &Path, wait: Duration) -> Result<File> {
    let file = File::create(dir.join("lock"))?;
    let start = Instant::now();
    let mut said = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {}
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let elapsed = start.elapsed();
        if elapsed >= wait {
            bail!(
                "another attach or detach still runs after {} s: {}",
                wait.as_secs(),
                storage_spaces::holders::of_lock(&file)
            );
        }
        if !said && elapsed >= Duration::from_secs(2) {
            eprintln!(
                "waiting for another attach or detach: {}",
                storage_spaces::holders::of_lock(&file)
            );
            said = true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How far a space is attached: `None` when it is not.
pub fn attached(space: &Space) -> Option<Status> {
    let text = fs::read_to_string(State::path(&space.info.guid.to_string())).ok()?;
    Some(State::parse(&text).status)
}

/// Test builds (the feature test-hooks) stop as a crash would after the
/// attach step SPACES_ATTACH_STOP_AFTER names: state, props, server, dm,
/// partition (the first one), partitions (all, udev settled).
fn step(name: &str) {
    #[cfg(feature = "test-hooks")]
    if std::env::var("SPACES_ATTACH_STOP_AFTER").is_ok_and(|s| s == name) {
        eprintln!("stopped after the attach step {name} (SPACES_ATTACH_STOP_AFTER)");
        std::process::exit(99);
    }
    let _ = name;
}

/// How `spaces attach` attaches a space.
pub struct Request<'a> {
    pub backend: Backend,
    /// Read-write.
    pub rw: bool,
    /// The verdict of the checks, and where their report is.
    pub verdict: Verdict,
    pub report: Option<&'a Path>,
    /// The arguments of `spaces attach` that attach it again as it is.
    pub args: Vec<String>,
}

/// Attaches one space as `request` says (read-write with `rw`; the checks
/// gave it `verdict`, their report at `report`). Returns the state describing
/// what was created. The state comes first, marked attaching, with the
/// server unit and the devices the attach makes; it is written again as
/// each device appears, and marked attached at the end. What fails is
/// undone; what cannot be undone stays in the state, for `spaces detach`
/// and the next attach.
pub fn attach_space(pool: &Pool<File>, space: &Space, paths: &[PathBuf], request: Request<'_>) -> Result<State> {
    let Request {
        backend: requested,
        rw,
        verdict,
        report,
        args,
    } = request;
    if verdict != Verdict::Healthy && rw {
        bail!("writing needs a healthy space, and this one is {}", verdict.as_str());
    }
    let guid = space.info.guid.to_string();
    if rw && let Some(why) = pool.write_refusal(space.id())? {
        bail!("it cannot be written: {why}");
    }
    let reader = crate::open_space(pool, space.id())?;
    let sector = pool.logical_sector_size;
    let backend = choose(requested, &reader, paths, sector, rw)?;
    let name = unique_dm_name(
        dm_name(&pool.name, space.name()),
        &pool.guid.to_string(),
        &guid,
        &State::load_all()?,
    );
    // Partitions become devices only when they lie inside the space and
    // apart, and are not too many (an attach forced past the partition
    // check gets the whole space only).
    let parts = storage_spaces::guard::partitions(&reader, reader.size(), sector as u64);
    let parts = partition_devices(parts, reader.size()).unwrap_or_else(|why| {
        eprintln!("{:?}: no devices for its partitions: {why}", space.name());
        Vec::new()
    });
    let unit = format!("storage-spaces-{guid}");
    let served = !matches!(backend, Backend::Dm | Backend::Auto);
    let mount = Path::new(STATE_DIR).join(format!("{unit}.fuse"));
    let mut state = State {
        status: Status::Attaching,
        space_guid: guid.clone(),
        pool_guid: pool.guid.to_string(),
        backend: backend.name().into(),
        unit: served.then(|| unit.clone()),
        fuse_mount: (backend == Backend::Fuse).then(|| mount.display().to_string()),
        dm: std::iter::once(name.clone())
            .chain(parts.iter().map(|p| format!("{name}-p{}", p.number)))
            .collect(),
        rw,
        verdict: Some(verdict.as_str().into()),
        forced: verdict != Verdict::Healthy,
        report: report.map(|p| p.display().to_string()),
        // An argument with a line break in it does not fit the state file.
        args: if args.iter().any(|a| a.contains('\n')) {
            Vec::new()
        } else {
            args
        },
        ..Default::default()
    };
    state.save()?;
    step("state");
    // The server checks the space too, and needs the same flag to read
    // one that is not healthy.
    let mut flags: Vec<&str> = if rw { vec!["--rw"] } else { Vec::new() };
    flags.extend(verdict.flag());
    let rw_arg: &[&str] = &flags;
    let sectors = reader.size() / 512;
    let result = (|| -> Result<()> {
        // udev learns whose each device is before it appears (the backend
        // device right after: it appears when its server starts).
        for d in &state.dm {
            write_props(d, &state, false)?;
        }
        step("props");
        let uuid = dm_uuid(&state, &name);
        match backend {
            Backend::Dm | Backend::Auto => dm_create(&name, &uuid, &dm_table(&reader, paths)?, rw)?,
            Backend::Ublk => {
                let dev = start_server(&unit, "ublk", paths, &guid, rw_arg)?;
                state.device = Some(dev.clone());
                state.save()?;
                step("server");
                mark_backend(&dev, &state)?;
                dm_create(&name, &uuid, &format!("0 {sectors} linear {dev} 0\n"), rw)?;
            }
            Backend::Nbd => {
                let socket = Path::new(STATE_DIR).join(format!("{unit}.sock"));
                let _ = fs::remove_file(&socket);
                let socket_arg = socket.display().to_string();
                let mut extra = vec!["--socket", socket_arg.as_str()];
                extra.extend(rw_arg);
                start_server(&unit, "nbd", paths, &guid, &extra)?;
                let dev = free_nbd()?;
                state.device = Some(dev.clone());
                state.save()?;
                step("server");
                let bs = sector.to_string();
                let mut args = vec![
                    "-N",
                    space.name(),
                    "-u",
                    socket_arg.as_str(),
                    dev.as_str(),
                    "-b",
                    bs.as_str(),
                ];
                if !rw {
                    args.push("-R");
                }
                run("nbd-client", &args, None)?;
                mark_backend(&dev, &state)?;
                dm_create(&name, &uuid, &format!("0 {sectors} linear {dev} 0\n"), rw)?;
            }
            Backend::Fuse => {
                // A mount left there by a server that was killed (dead: it
                // fails every access) goes first.
                let point = mount.display().to_string();
                if is_mount_point(&point) {
                    let _ = run("umount", &["-l", &point], None);
                }
                fs::create_dir_all(&mount)?;
                let mountpoint = mount.display().to_string();
                let mut extra = vec!["--mountpoint", mountpoint.as_str()];
                extra.extend(verdict.flag());
                let file = start_server(&unit, "fuse", paths, &guid, &extra)?;
                let bs = sector.to_string();
                let dev = run("losetup", &["-r", "-b", &bs, "-f", "--show", &file], None)?
                    .trim()
                    .to_string();
                state.device = Some(dev.clone());
                state.save()?;
                step("server");
                mark_backend(&dev, &state)?;
                dm_create(&name, &uuid, &format!("0 {sectors} linear {dev} 0\n"), false)?;
            }
        }
        step("dm");
        // udev is waited for once for all partitions.
        for p in &parts {
            let part = format!("{name}-p{}", p.number);
            let table = format!("0 {} linear /dev/mapper/{name} {}\n", p.length / 512, p.offset / 512);
            dm_create_unsettled(&part, &dm_uuid(&state, &part), &table, rw)?;
            step("partition");
        }
        if !parts.is_empty() {
            settle();
        }
        step("partitions");
        Ok(())
    })();
    if let Err(e) = result {
        // What could not be removed stays known, for `spaces detach`.
        if teardown(&state).is_err() {
            let _ = state.save();
        }
        return Err(e);
    }
    state.status = Status::Attached;
    state.save()?;
    Ok(state)
}

/// The kernel name a device path leads to (/dev/mapper/ss-x: dm-3).
fn kernel_of(path: &Path) -> Option<String> {
    Some(fs::canonicalize(path).ok()?.file_name()?.to_str()?.to_string())
}

/// A mount point as /proc/self/mountinfo writes it (\040 for a space).
fn unescape_mount(field: &str) -> String {
    String::from_utf8_lossy(&storage_spaces::holders::unescape_mountinfo(field)).into_owned()
}

/// The process ids in /proc.
fn processes() -> Vec<u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

/// The device number in /sys/class/block/`kernel`/dev.
fn device_number(kernel: &str) -> Option<(String, libc::dev_t)> {
    let text = fs::read_to_string(Path::new("/sys/class/block").join(kernel).join("dev")).ok()?;
    let text = text.trim().to_string();
    let (major, minor) = text.split_once(':')?;
    let number = libc::makedev(major.parse().ok()?, minor.parse().ok()?);
    Some((text, number))
}

/// How many openers the kernel counts for the device-mapper device `name`.
fn dm_open_count(name: &str) -> Option<u64> {
    run("dmsetup", &["info", "-c", "--noheadings", "-o", "open", name], None)
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// What uses the devices of `state` besides the space's own: mounts of its
/// device-mapper devices or of the backend device and its kernel
/// partitions, in any mount namespace; devices built on them (LVM,
/// dm-crypt); swaps on them; processes that hold one of the space's
/// devices open; and, where none of that shows, the openers the kernel
/// counts beyond the space's own partitions. Detaching would pull the space
/// out from under them, and it removes nothing while there are any.
pub fn users(state: &State) -> Vec<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mapper = |d: &String| Path::new("/dev/mapper").join(d);
    let own = own_devices(state);
    let ours: Vec<String> = own.iter().filter_map(|d| kernel_of(&mapper(d))).collect();
    // (shown, kernel name, whether a process holding it open counts: not
    // for the backend device, which the helpers of its server may keep
    // open, nbd-client among them)
    let mut devices: Vec<(String, String, bool)> = own
        .iter()
        .filter_map(|d| Some((mapper(d).display().to_string(), kernel_of(&mapper(d))?, true)))
        .collect();
    if let Some(kernel) = state.device.as_deref().and_then(|d| kernel_of(Path::new(d))) {
        let sys = Path::new("/sys/class/block").join(&kernel);
        for entry in fs::read_dir(&sys).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&kernel) && entry.path().join("partition").exists() {
                devices.push((format!("/dev/{name}"), name, true));
            }
        }
        devices.push((format!("/dev/{kernel}"), kernel, false));
    }
    let numbers: Vec<Option<(String, libc::dev_t)>> = devices.iter().map(|d| device_number(&d.1)).collect();
    let index_of = |text: &str| numbers.iter().position(|n| n.as_ref().is_some_and(|(t, _)| t == text));
    let mut found = Vec::new();
    // Mounts, in every mount namespace (one process's view of each, this
    // one's first). A mount that propagated into another namespace (most
    // services see the mounts made here) is its peer group's (shared:N
    // there, master:N in the copy): reported once.
    let mut namespaces = std::collections::HashSet::new();
    let mut groups = std::collections::HashSet::new();
    let views = std::iter::once("self".to_string()).chain(processes().into_iter().map(|p| p.to_string()));
    for (n, pid) in views.enumerate() {
        let Ok(ns) = fs::read_link(format!("/proc/{pid}/ns/mnt")) else {
            continue;
        };
        if !namespaces.insert(ns) {
            continue;
        }
        let Ok(info) = fs::read_to_string(format!("/proc/{pid}/mountinfo")) else {
            continue;
        };
        let whose = if n == 0 {
            String::new()
        } else {
            format!(" in the mount namespace of pid {pid}")
        };
        for line in info.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            let Some(i) = f.get(2).and_then(|numbers| index_of(numbers)).filter(|_| f.len() > 6) else {
                continue;
            };
            let optional = f[6..].iter().take_while(|&&t| t != "-");
            let peers: Vec<&str> = optional
                .filter_map(|t| t.strip_prefix("shared:").or_else(|| t.strip_prefix("master:")))
                .collect();
            let seen = peers.iter().any(|g| groups.contains(*g));
            groups.extend(peers.iter().map(|g| g.to_string()));
            if !seen {
                found.push(format!(
                    "{} is mounted on {}{whose}",
                    devices[i].0,
                    unescape_mount(f[4])
                ));
            }
        }
    }
    for (shown, kernel, _) in &devices {
        let sys = Path::new("/sys/class/block").join(kernel);
        for holder in fs::read_dir(sys.join("holders")).into_iter().flatten().flatten() {
            let holder = holder.file_name().to_string_lossy().into_owned();
            if !ours.contains(&holder) {
                let name = fs::read_to_string(format!("/sys/block/{holder}/dm/name"))
                    .map(|n| format!("/dev/mapper/{}", n.trim()))
                    .unwrap_or(format!("/dev/{holder}"));
                found.push(format!("{shown} is held by {name}"));
            }
        }
    }
    for line in fs::read_to_string("/proc/swaps").unwrap_or_default().lines().skip(1) {
        let Some(path) = line.split_whitespace().next() else {
            continue;
        };
        if let Some(kernel) = kernel_of(Path::new(&unescape_mount(path)))
            && let Some(d) = devices.iter().find(|d| d.1 == kernel)
        {
            found.push(format!("{} is used as swap", d.0));
        }
    }
    let watched: Vec<(libc::dev_t, &str)> = devices
        .iter()
        .zip(&numbers)
        .filter(|(d, _)| d.2)
        .filter_map(|(d, n)| Some((n.as_ref()?.1, d.0.as_str())))
        .collect();
    let me = std::process::id();
    for pid in processes().into_iter().filter(|&p| p != me) {
        let mut held = std::collections::BTreeSet::new();
        for fd in fs::read_dir(format!("/proc/{pid}/fd")).into_iter().flatten().flatten() {
            if let Ok(meta) = fs::metadata(fd.path())
                && meta.file_type().is_block_device()
                && let Some((_, shown)) = watched.iter().find(|(n, _)| *n == meta.rdev() as libc::dev_t)
            {
                held.insert(*shown);
            }
        }
        if !held.is_empty() {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            for shown in held {
                found.push(format!("{shown} is open in pid {pid} ({})", comm.trim()));
            }
        }
    }
    if found.is_empty() {
        // What none of that shows (a file system spanning devices, a
        // process this one cannot see): the openers the kernel counts. The
        // space's partition devices hold the whole one.
        for (i, name) in own.iter().enumerate() {
            let expected = if i == 0 && state.dm.first() == Some(name) {
                own.len() as u64 - 1
            } else {
                0
            };
            if let Some(n) = dm_open_count(name).filter(|&n| n > expected) {
                found.push(format!(
                    "{} is open {n} time(s), by something not found",
                    mapper(name).display()
                ));
            }
        }
    }
    found
}

/// Whether the server of `state` runs a copy of `program` that was replaced
/// since it started (by an upgrade): its executable is gone from that path.
pub fn outdated(state: &State, program: &Path) -> bool {
    let Some(unit) = &state.unit else {
        return false;
    };
    let pid = run("systemctl", &["show", "-p", "MainPID", "--value", unit], None).unwrap_or_default();
    let pid = pid.trim();
    if pid.is_empty() || pid == "0" {
        return false;
    }
    fs::read_link(format!("/proc/{pid}/exe"))
        .is_ok_and(|exe| exe.as_os_str() == format!("{} (deleted)", program.display()).as_str())
}

/// Whether `path` is a mount point (in this mount namespace).
fn is_mount_point(path: &str) -> bool {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .any(|l| l.split(' ').nth(4).is_some_and(|m| unescape_mount(m) == path))
}

/// Removes what `state` describes, most dependent parts first. Refuses,
/// changing nothing, while something uses the space ([`users`]). A step
/// that fails (a device-mapper device that stays, a backend device that
/// stays connected, a server that does not stop, a mount that stays) ends
/// it: the state then lists what is left, which the error names, for the
/// next detach.
pub fn teardown(state: &State) -> Result<()> {
    // udev's probes of the devices (it opens them) come first.
    settle();
    let users = users(state);
    if !users.is_empty() {
        bail!(
            "in use: {}; it stays attached, nothing removed (unmount, swapoff or close what uses it first)",
            users.join("; ")
        );
    }
    let mapper = |name: &String| Path::new("/dev/mapper").join(name);
    for name in state.dm.iter().filter(|n| mapper(n).exists()) {
        if !own_devices(state).contains(name) {
            eprintln!("device-mapper device {name} is not this space's (another UUID): left alone");
        }
    }
    let mut left = state.clone();
    let stop = |left: &State, why: String| -> anyhow::Error {
        let kept = match left.save() {
            Ok(()) => String::new(),
            Err(e) => format!(" (and its state could not be written: {e:#})"),
        };
        anyhow::anyhow!("{why}; left: {}{kept}", left.what())
    };
    // Partitions first. An attach that was cut off may still make a device
    // (a dmsetup it ran goes on): the next pass finds it.
    let mut errors = Vec::new();
    for _ in 0..3 {
        let own = own_devices(state);
        errors.clear();
        if own.is_empty() {
            break;
        }
        for name in own.iter().rev() {
            match dm_remove(name) {
                Ok(()) => remove_props(name),
                Err(e) => errors.push(e.to_string()),
            }
        }
        settle();
    }
    for name in state.dm.iter().filter(|n| !mapper(n).exists()) {
        remove_props(name);
    }
    if !errors.is_empty() {
        left.dm.retain(|n| mapper(n).exists());
        return Err(stop(&left, format!("{}; its server keeps running", errors.join("; "))));
    }
    left.dm.clear();
    if let Some(dev) = &state.device {
        let sys = Path::new("/sys/block").join(kernel_name(dev));
        match state.backend.as_str() {
            "nbd" => {
                let _ = run("nbd-client", &["-d", dev], None);
                if sys.join("pid").exists() {
                    return Err(stop(&left, format!("{dev} stays connected (nbd-client -d)")));
                }
            }
            "fuse" => {
                let _ = run("losetup", &["-d", dev], None);
                if sys.join("loop/backing_file").exists() {
                    return Err(stop(&left, format!("{dev} stays set up (losetup -d)")));
                }
            }
            _ => {}
        }
        remove_props(kernel_name(dev));
        left.device = None;
    }
    if let Some(mount) = &state.fuse_mount
        && is_mount_point(mount)
    {
        // Before its server stops, which would leave the mount dead.
        let _ = run("umount", &[mount], None);
        if is_mount_point(mount) {
            return Err(stop(&left, format!("{mount} stays mounted")));
        }
    }
    if let Some(unit) = &state.unit {
        let _ = run("systemctl", &["stop", unit], None);
        if run("systemctl", &["is-active", "--quiet", unit], None).is_ok() {
            return Err(stop(&left, format!("its server {unit} does not stop")));
        }
        left.unit = None;
    }
    // A ublk device a server left behind, killed before it deleted it.
    #[cfg(feature = "ublk")]
    if state.backend == "ublk"
        && let Some(n) = state
            .device
            .as_deref()
            .and_then(|d| d.strip_prefix("/dev/ublkb"))
            .and_then(|n| n.parse::<u32>().ok())
        && Path::new(&format!("/sys/class/ublk-char/ublkc{n}")).exists()
    {
        match crate::ublk::remove_left(n) {
            Ok(true) => eprintln!("removed the ublk device {n}, which its server left"),
            Ok(false) => {}
            Err(e) => eprintln!("the ublk device {n}, which its server left, stays: {e:#}"),
        }
    }
    if let Some(mount) = &state.fuse_mount {
        let _ = fs::remove_dir(mount);
    }
    if let Some(unit) = &state.unit {
        let _ = fs::remove_file(Path::new(STATE_DIR).join(format!("{unit}.sock")));
    }
    let _ = fs::remove_file(State::path(&state.space_guid));
    remove_report(&state.space_guid);
    Ok(())
}

/// The device-mapper devices of `state` that exist and are the space's: of
/// a state 1.2 wrote, all it lists; else those with the space's UUID (a
/// name an attach meant to use may be another device's).
fn own_devices(state: &State) -> Vec<String> {
    state
        .dm
        .iter()
        .filter(|name| {
            dm_uuid_of(name).is_some_and(|uuid| state.status == Status::Legacy || uuid == dm_uuid(state, name))
        })
        .cloned()
        .collect()
}

/// Undoes the attaches that were cut off (a state left attaching), so that
/// their spaces can be attached again: their server and backend device,
/// those of their devices that carry the space's UUID, their state. One
/// whose devices are in use and all there is completed instead. Run under
/// [`lock`]; what it does goes to standard error.
pub fn reconcile() {
    let Ok(states) = State::load_all() else {
        return;
    };
    let cut: Vec<State> = states.into_iter().filter(|s| s.status == Status::Attaching).collect();
    if !cut.is_empty() {
        // What the attach that was cut off left udev to do comes first.
        settle();
    }
    for mut state in cut {
        let name = state.dm.first().cloned().unwrap_or_else(|| state.space_guid.clone());
        match teardown(&state) {
            Ok(()) => eprintln!("{name}: undid an attach that was cut off"),
            Err(e) if own_devices(&state).len() == state.dm.len() => {
                state.status = Status::Attached;
                match state.save() {
                    Ok(()) => eprintln!("{name}: completed an attach that was cut off ({e:#})"),
                    Err(e) => eprintln!("{name}: an attach was cut off, and completing it failed: {e:#}"),
                }
            }
            Err(e) => eprintln!("{name}: an attach was cut off, and undoing it failed: {e:#}"),
        }
    }
}

/// Device-mapper devices with the UUID of a space and no state that lists
/// them (a state that was lost): their names, partitions first.
pub fn leftovers(states: &[State]) -> Vec<String> {
    let listed: std::collections::HashSet<&str> = states.iter().flat_map(|s| s.dm.iter().map(String::as_str)).collect();
    let out = run(
        "dmsetup",
        &["info", "-c", "--noheadings", "-o", "name,uuid", "--separator", " "],
        None,
    )
    .unwrap_or_default();
    let mut found: Vec<String> = out
        .lines()
        .filter_map(|l| l.trim().rsplit_once(' '))
        .filter(|(name, uuid)| uuid.starts_with(DM_UUID_PREFIX) && !listed.contains(name))
        .map(|(name, _)| name.to_string())
        .collect();
    found.sort_by_key(|n| std::cmp::Reverse(n.len()));
    found
}

/// Removes the device-mapper devices `names` ([`leftovers`]); those in use
/// stay, with the reason. The number that stayed.
pub fn remove_leftovers(names: &[String]) -> usize {
    let mut failed = 0;
    for name in names {
        match run("dmsetup", &["remove", "--retry", name], None) {
            Ok(_) => {
                remove_props(name);
                println!("removed {name}");
            }
            Err(e) => {
                eprintln!("{name} stays: {e:#}");
                failed += 1;
            }
        }
    }
    failed
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn server_properties_are_systemd_assignments() {
        // They go to `systemd-run --property=`: a typo would make attach
        // fail at boot, where nobody sees it.
        for p in SERVER_RESTRICTIONS.iter().chain(SERVER_PRIVATE_VIEW) {
            let (key, value) = p.split_once('=').unwrap_or_else(|| panic!("{p}"));
            assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric()) && !value.is_empty(),
                "{p}"
            );
        }
        // The FUSE server must not get a private view of the file system:
        // its mount has to be visible to the host.
        assert!(SERVER_PRIVATE_VIEW.iter().any(|p| p.starts_with("ProtectSystem")));
        assert!(
            !SERVER_RESTRICTIONS
                .iter()
                .any(|p| p.starts_with("Protect") && p.contains("System"))
        );
    }

    #[test]
    fn servers_do_not_wait_for_tmpfiles_at_boot() {
        // PrivateTmp= orders a unit after systemd-tmpfiles-setup.service
        // (also with DefaultDependencies=no), which comes after
        // local-fs.target: the attach service, before local-fs-pre.target,
        // would wait for a server that waits for it.
        for p in SERVER_RESTRICTIONS.iter().chain(SERVER_PRIVATE_VIEW) {
            assert!(!p.starts_with("PrivateTmp"), "{p}");
        }
        assert!(SERVER_PRIVATE_VIEW.contains(&"TemporaryFileSystem=/tmp"));
    }

    /// No server needs /var at boot: a /var of its own is not mounted when
    /// the attach unit starts them, before local-fs-pre.target (1.2.2 had
    /// /var/tmp in TemporaryFileSystem=, a directory systemd then makes on
    /// a root file system still read-only).
    #[test]
    fn servers_need_nothing_of_var_at_boot() {
        for p in SERVER_RESTRICTIONS.iter().chain(SERVER_PRIVATE_VIEW) {
            assert!(!p.contains("/var"), "{p}");
        }
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spaces-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// Whether this process reads what its permissions forbid (root).
    fn overrides_permissions(dir: &Path) -> bool {
        let probe = dir.join("probe");
        fs::write(&probe, b"x").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o000)).unwrap();
        let read = fs::read(&probe).is_ok();
        fs::remove_file(&probe).unwrap();
        read
    }

    /// The state directory ends up readable by everyone, also when it was
    /// made 0700 before (the attach unit's umask in 1.1.0).
    #[test]
    fn the_state_directory_is_readable() {
        let dir = scratch("mode");
        readable_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        readable_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What attach records of the checks reads back.
    #[test]
    fn the_verdict_and_report_are_kept_in_the_state() {
        let s = State {
            space_guid: "g".into(),
            dm: vec!["ss-p-a".into()],
            verdict: Some("suspect".into()),
            forced: true,
            report: Some("/run/storage-spaces/reports/g.txt".into()),
            ..Default::default()
        };
        let dir = scratch("verdict");
        fs::create_dir_all(&dir).unwrap();
        // As save writes it.
        let text = format!(
            "space=g\npool=\nbackend=\nverdict=suspect\nreport={}\ndm=ss-p-a\nforced=1\n",
            s.report.as_deref().unwrap()
        );
        fs::write(dir.join("g.state"), text).unwrap();
        let back = &load_states(&dir).unwrap()[0];
        assert_eq!(
            (back.verdict.as_deref(), back.forced, back.report.as_deref()),
            (s.verdict.as_deref(), true, s.report.as_deref())
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// What udev imports for a device of an attached space.
    #[test]
    fn udev_properties_say_whose_device_and_its_verdict() {
        let state = State {
            space_guid: "g".into(),
            pool_guid: "p".into(),
            verdict: Some("suspect".into()),
            forced: true,
            ..Default::default()
        };
        assert_eq!(
            props_text(&state, false),
            "SS_SPACE=g\nSS_POOL=p\nSS_VERDICT=suspect\nSS_FORCED=1\n"
        );
        assert!(props_text(&state, true).ends_with("SS_BACKEND=1\n"));
        let dir = scratch("udev");
        write_props_in(&dir, "ss-p-a", &state, false).unwrap();
        let path = dir.join("ss-p-a.props");
        assert_eq!(fs::read_to_string(&path).unwrap(), props_text(&state, false));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Reports are files everyone may read; those of spaces not attached
    /// are what was refused.
    #[test]
    fn reports_are_written_and_refused_ones_listed() {
        let dir = scratch("reports");
        let mut r = Report::new("space \"data\" (g1) of pool \"p\" (pg)");
        r.space = Some(("data".into(), "g1".into()));
        r.action = Some("not attached: a suspect space needs --force (and is then read-only)".into());
        let path = write_report_in(&dir, &r).unwrap();
        assert_eq!(path, dir.join("g1.txt"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(fs::read_to_string(dir.join("g1.json")).unwrap().contains("\"space\""));
        r.space = Some(("other".into(), "g2".into()));
        write_report_in(&dir, &r).unwrap();
        let attached = State {
            space_guid: "G2".into(),
            ..Default::default()
        };
        let refused = refused_in(&dir, &[attached]);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, dir.join("g1.txt"));
        assert!(
            refused[0]
                .1
                .ends_with("not attached: a suspect space needs --force (and is then read-only)")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Paths go into a unit's list of paths whole: spaces, quotes and
    /// backslashes kept.
    #[test]
    fn paths_are_quoted_for_units() {
        assert_eq!(unit_quoted("/srv/pool/disk0.img"), "\"/srv/pool/disk0.img\"");
        assert_eq!(unit_quoted("/home/a b/c\"d\\e"), "\"/home/a b/c\\\"d\\\\e\"");
    }

    /// An external program that hangs is stopped in time and the run says
    /// so (1.2.2 waited for it for ever); one that finishes gives its
    /// output, one that fails its error.
    #[test]
    fn programs_are_stopped_in_time() {
        let start = Instant::now();
        let e = run_within("sleep", &["30"], None, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
        assert!(e.contains("did not finish within"), "{e}");
        assert_eq!(
            run_within("echo", &["hello"], None, Duration::from_secs(10)).unwrap(),
            "hello\n"
        );
        assert_eq!(
            run_within("cat", &[], Some("in"), Duration::from_secs(10)).unwrap(),
            "in"
        );
        let e = run_within("sh", &["-c", "echo why >&2; exit 3"], None, Duration::from_secs(10)).unwrap_err();
        assert!(e.to_string().ends_with("failed: why"), "{e}");
    }

    /// A run waits for the one holding the lock only as long as it may,
    /// then says so (1.2.2 waited for ever).
    #[test]
    fn the_lock_is_waited_for_in_time() {
        let dir = scratch("lock");
        fs::create_dir_all(&dir).unwrap();
        let held = lock_in(&dir, Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        let e = lock_in(&dir, Duration::from_millis(500)).unwrap_err().to_string();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(e.contains("another attach or detach still runs"), "{e}");
        drop(held);
        lock_in(&dir, Duration::from_millis(500)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The report of a pool whose disks were unplugged since is not listed
    /// as refused (1.2.2 listed it until the next boot); one whose devices
    /// are there, or that names none (made before 1.3), is.
    #[test]
    fn reports_of_devices_since_gone_are_not_listed() {
        let dir = scratch("gone");
        let present = dir.join("disk0.img");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&present, b"").unwrap();
        for (guid, devices) in [
            ("g1", vec![dir.join("unplugged0"), dir.join("unplugged1")]),
            ("g2", vec![dir.join("unplugged2"), present.clone()]),
            ("g3", vec![]),
        ] {
            let mut r = Report::new(format!("space \"{guid}\""));
            r.space = Some((guid.into(), guid.into()));
            r.devices = devices.iter().map(|p| p.display().to_string()).collect();
            write_report_in(&dir, &r).unwrap();
        }
        let listed: Vec<PathBuf> = refused_in(&dir, &[]).into_iter().map(|r| r.0).collect();
        assert_eq!(listed, [dir.join("g2.txt"), dir.join("g3.txt")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// No state directory: nothing attached. One this user may not read:
    /// an error that says so, not "none attached".
    #[test]
    fn unreadable_state_is_an_error() {
        let dir = scratch("denied");
        assert!(load_states(&dir).unwrap().is_empty());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.state"), "space=a\ndm=ss-p-a\n").unwrap();
        assert_eq!(load_states(&dir).unwrap()[0].dm, ["ss-p-a"]);
        if overrides_permissions(&dir) {
            eprintln!("permissions do not apply to this user (root): skipped");
        } else {
            fs::set_permissions(dir.join("a.state"), fs::Permissions::from_mode(0o000)).unwrap();
            let err = load_states(&dir).unwrap_err().to_string();
            assert!(err.contains("a.state") && err.contains("run as root"), "{err}");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
            let err = load_states(&dir).unwrap_err().to_string();
            assert!(err.contains("cannot read") && err.contains("run as root"), "{err}");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;

    fn parts(n: u32) -> Vec<Partition> {
        (0..n)
            .map(|i| Partition {
                number: i + 1,
                offset: (u64::from(i) + 1) << 20,
                length: 1 << 20,
                kind: String::new(),
                name: String::new(),
            })
            .collect()
    }

    /// A table of more partitions than a GPT holds by default makes none
    /// of them devices (4096 entries made attach run 8192 programs and wait
    /// for udev as often), nor do partitions that overlap.
    #[test]
    fn too_many_or_overlapping_partitions_get_no_devices() {
        assert_eq!(partition_devices(parts(128), 1 << 30).unwrap().len(), 128);
        let why = partition_devices(parts(129), 1 << 30).unwrap_err();
        assert!(why.contains("129 partitions"), "{why}");
        assert!(partition_devices(parts(4096), 1 << 40).is_err());
        let mut overlap = parts(2);
        overlap[1].offset = overlap[0].offset;
        assert!(partition_devices(overlap, 1 << 30).is_err());
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    fn attached(dm: &str, pool: &str) -> State {
        State {
            pool_guid: pool.into(),
            dm: vec![dm.into(), format!("{dm}-p1")],
            ..Default::default()
        }
    }

    /// Two pools of the same name with a space of the same name: the second
    /// gets the first eight hex digits of its pool's GUID after the name.
    /// Two spaces of one pool whose names dm_name makes alike ("Фото" and
    /// "Игры": one name for both in 1.2.2, whose attach of the second
    /// failed and removed the first one's device) get those of the space's
    /// GUID; the space itself keeps its name, other names are left alone.
    #[test]
    fn spaces_whose_names_are_alike_get_apart() {
        let base = dm_name("Storage pool", "data");
        assert_eq!(base, "ss-Storage_pool-data");
        let first = "249b3eb3-30f0-4b3c-8261-cd3732f7f7ae";
        let other = "0735434C-B460-4A90-A3BA-B04FCE198982";
        let (space, again) = (
            "c6f8e8a1-0000-4000-8000-000000000001",
            "5d0e2b77-0000-4000-8000-000000000002",
        );
        let held = [State {
            space_guid: space.into(),
            ..attached(&base, first)
        }];
        assert_eq!(
            unique_dm_name(base.clone(), other, again, &held),
            "ss-Storage_pool-data-0735434C"
        );
        assert_eq!(
            unique_dm_name(base.clone(), &first.to_uppercase(), again, &held),
            "ss-Storage_pool-data-5d0e2b77"
        );
        assert_eq!(unique_dm_name(base.clone(), first, &space.to_uppercase(), &held), base);
        assert_eq!(unique_dm_name(base.clone(), other, again, &[]), base);
        assert_eq!(
            unique_dm_name("ss-Storage_pool-games".into(), other, again, &held),
            "ss-Storage_pool-games"
        );
        assert_eq!(dm_name("p", "Фото"), dm_name("p", "Игры"));
    }

    /// Every device of a space carries its UUID, a partition's with its
    /// number.
    #[test]
    fn devices_carry_the_uuid_of_their_space() {
        let s = State {
            pool_guid: "p".into(),
            space_guid: "s".into(),
            dm: vec!["ss-a-b".into(), "ss-a-b-p1".into(), "ss-a-b-p12".into()],
            ..Default::default()
        };
        assert_eq!(dm_uuid(&s, "ss-a-b"), "STORAGESPACES-p-s");
        assert_eq!(dm_uuid(&s, "ss-a-b-p1"), "STORAGESPACES-p-s-p1");
        assert_eq!(dm_uuid(&s, "ss-a-b-p12"), "STORAGESPACES-p-s-p12");
    }

    /// What a detach that stopped leaves is named, as its state keeps it.
    #[test]
    fn what_a_state_lists_is_named() {
        let s = State {
            device: Some("/dev/ublkb3".into()),
            unit: Some("storage-spaces-s".into()),
            ..attached("ss-a-b", "p")
        };
        let what = s.what();
        for part in [
            "device-mapper ss-a-b, ss-a-b-p1",
            "device /dev/ublkb3",
            "server storage-spaces-s",
        ] {
            assert!(what.contains(part), "{what}");
        }
        assert_eq!(State::default().what(), "its state");
    }

    /// A state says how far its attach got; one 1.2 wrote, without, is
    /// attached and its devices are all its own.
    #[test]
    fn states_say_how_far_their_attach_got() {
        for status in [Status::Attaching, Status::Attached] {
            let s = State {
                status,
                dm: vec!["ss-a-b".into(), "ss-a-b-p1".into()],
                unit: Some("storage-spaces-s".into()),
                args: ["--space", "s", "--backend", "nbd", "/images/a b=c/disk0.img"]
                    .map(String::from)
                    .to_vec(),
                ..attached("ss-a-b", "p")
            };
            let back = State::parse(&s.text());
            assert_eq!(back.status, status);
            assert_eq!(back.dm, s.dm);
            assert_eq!(back.unit, s.unit);
            assert_eq!(back.args, s.args);
        }
        assert_eq!(
            State::parse("space=s\npool=p\nbackend=dm\ndm=ss-a-b\n").status,
            Status::Legacy
        );
    }

    /// `spaces detach data` finds the spaces named data, with or without
    /// the pool GUID's digits after the name.
    #[test]
    fn a_space_name_finds_its_devices() {
        assert!(names_space("ss-Storage_pool-data", "data"));
        assert!(names_space("ss-Storage_pool-data-0735434c", "data"));
        assert!(!names_space("ss-Storage_pool-metadata", "data"));
        assert!(!names_space("ss-Storage_pool-data2-0735434c", "data"));
        assert!(names_space("ss-p-backup-2024abcd", "backup-2024abcd"));
    }

    /// A space name that two attached spaces share (pools of one name)
    /// names neither: `spaces detach data` refuses, listing both (1.2.2
    /// detached both; audit M12). Each is named by its GUID or device.
    #[test]
    fn a_name_two_attached_spaces_share_names_neither() {
        let (one, two) = (
            "249b3eb3-30f0-4b3c-8261-cd3732f7f7ae",
            "0735434c-b460-4a90-a3ba-b04fce198982",
        );
        let states = || {
            vec![
                State {
                    space_guid: one.into(),
                    ..attached("ss-Storage_pool-data", "p1")
                },
                State {
                    space_guid: two.into(),
                    ..attached("ss-Storage_pool-data-0735434c", "p2")
                },
                State {
                    space_guid: "3".into(),
                    ..attached("ss-Storage_pool-logs", "p1")
                },
            ]
        };
        let e = select_attached(states(), Some("data")).unwrap_err().to_string();
        assert!(e.contains(one) && e.contains(two), "{e}");
        let pick = |key: &str| -> Vec<String> {
            select_attached(states(), Some(key))
                .unwrap()
                .into_iter()
                .map(|s| s.space_guid)
                .collect()
        };
        assert_eq!(pick(&two.to_uppercase()), [two]);
        assert_eq!(pick("ss-Storage_pool-data"), [one]);
        assert_eq!(pick("ss-Storage_pool-data-0735434c"), [two]);
        assert_eq!(pick("logs"), ["3"]);
        assert!(pick("nothing").is_empty());
        assert_eq!(select_attached(states(), None).unwrap().len(), 3);
    }
}

#[cfg(test)]
mod user_tests {
    use super::*;

    /// Mount points come escaped from /proc/self/mountinfo.
    #[test]
    fn mount_points_are_unescaped() {
        assert_eq!(unescape_mount("/run/media/a\\040b"), "/run/media/a b");
        assert_eq!(unescape_mount("/mnt/tab\\011x\\134y"), "/mnt/tab\tx\\y");
        assert_eq!(unescape_mount("/mnt/plain"), "/mnt/plain");
        // An escape past a byte (the kernel writes none; 1.2.2 overflowed).
        assert_eq!(unescape_mount("/mnt/\\777"), "/mnt/?");
        assert_eq!(unescape_mount("/mnt/odd\\9"), "/mnt/odd\\9");
    }

    /// A space whose devices are gone is used by nothing, and its server
    /// (none) runs no older program: detach may go ahead.
    #[test]
    fn a_space_without_devices_has_no_users() {
        let state = State {
            dm: vec!["ss-no-such-space-test".into()],
            device: Some("/dev/no-such-device-test".into()),
            ..Default::default()
        };
        assert!(users(&state).is_empty());
        assert!(!outdated(&state, Path::new("/usr/sbin/spaces")));
    }
}
