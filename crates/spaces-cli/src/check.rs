//! `spaces check`: the guard's report on each space (storage_spaces::guard)
//! with the file systems of its partitions checked (NTFS by the library,
//! ReFS by the refs crate); on request the deep checks, and a bundle of the
//! pool's metadata for a report.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use storage_spaces::gpt::{MSR_PARTITION_TYPE, Partition};
use storage_spaces::guard::{self, Options};
use storage_spaces::health::Health;
use storage_spaces::io::ReadAt;
use storage_spaces::isolate::{self, Stopped};
use storage_spaces::report::{Check, Evidence, Report, Status, Truth, Verdict, reports_to_json};
use storage_spaces::{Pool, Space, SpaceReader};

use crate::bundle;

// The checks run on threads of their own (`isolate::isolated`), which share
// the pool.
const _: fn() = || {
    fn shared<T: Send + Sync + 'static>() {}
    shared::<Pool<File>>();
};

/// How long the quick checks of a space, and the opening of a pool, may
/// take: SPACES_CHECK_TIMEOUT seconds (0: no limit), 40 by default, and no
/// longer than what is left of the time of `spaces attach` ([`time_left`]).
/// Real ones take a second or two; a hostile disk can make them loop, and
/// `spaces attach` checks every pool in one process.
pub fn deadline() -> Option<Duration> {
    let check = deadline_of(std::env::var("SPACES_CHECK_TIMEOUT").ok().as_deref());
    match (check, time_left()) {
        (Some(check), Some(left)) => Some(check.min(left)),
        (check, left) => check.or(left),
    }
}

/// When the run of `spaces attach` has to be done.
static ATTACH_UNTIL: OnceLock<Instant> = OnceLock::new();

/// Starts the time of `spaces attach`: SPACES_ATTACH_TIMEOUT seconds, which
/// the attach units set below their start timeout, so that every wait
/// (for udev, a server, the checks) ends in time and the run says what it
/// did not get to, rather than systemd killing it. None without it.
pub fn start_attach_time() {
    let seconds = std::env::var("SPACES_ATTACH_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0);
    if let Some(s) = seconds {
        let _ = ATTACH_UNTIL.set(Instant::now() + Duration::from_secs(s));
    }
}

/// What is left of the time of `spaces attach` (`None`: no limit).
pub fn time_left() -> Option<Duration> {
    ATTACH_UNTIL
        .get()
        .map(|until| until.saturating_duration_since(Instant::now()))
}

/// The [`deadline`] a value of SPACES_CHECK_TIMEOUT sets.
fn deadline_of(value: Option<&str>) -> Option<Duration> {
    match value.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(s) => Some(Duration::from_secs(s)),
        None => Some(Duration::from_secs(40)),
    }
}

/// Opens the pool on `paths` to check it (read-only, not exclusively; its
/// warnings go into the reports), on a thread of its own: a panic or a hang
/// in reading its metadata is an error, not the end of the process.
pub fn open_pool(paths: &[PathBuf]) -> Result<Pool<File>> {
    let owned = paths.to_vec();
    let opened = isolate::isolated(deadline(), move || -> Result<Pool<File>> {
        let files = owned
            .iter()
            .map(|p| crate::open_member(p, false, crate::Hold::Read))
            .collect::<Result<Vec<File>>>()?;
        Ok(Pool::open(files)?)
    });
    opened.unwrap_or_else(|stopped| Err(anyhow::anyhow!("reading its metadata {stopped}")))
}

/// Which of `pools` (their names and GUIDs) `key` names: the one of that
/// GUID (in any case) or name; `None` when none is. A key that names
/// several (a name two pools share, a name that is another pool's GUID) is
/// an error that lists them: no command takes one pool for another.
pub fn select_pool(pools: &[(&str, storage_spaces::Guid)], key: &str) -> Result<Option<usize>> {
    let found: Vec<usize> = (0..pools.len())
        .filter(|&i| pools[i].0 == key || pools[i].1.to_string().eq_ignore_ascii_case(key))
        .collect();
    match found[..] {
        [] => Ok(None),
        [i] => Ok(Some(i)),
        _ => bail!(
            "{key:?} names {} pools: {}; name the one meant by its GUID",
            found.len(),
            found
                .iter()
                .map(|&i| format!("{:?} ({})", pools[i].0, pools[i].1))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// What may be done with a space of a verdict, given the flags (attach,
/// the servers and export ask the same).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// As asked (read-write with --rw): the space is healthy.
    Attach { rw: bool },
    /// Read-only: the flag given allows a space that is not healthy.
    ReadOnly,
    /// Nothing, for this reason.
    Refuse(String),
}

/// The guard's rule: healthy spaces as asked; degraded ones read-only with
/// `--degraded` (or `--force`), suspect ones read-only with `--force`;
/// failed ones not at all; writing only healthy ones.
pub fn gate(verdict: Verdict, rw: bool, degraded: bool, force: bool) -> Decision {
    match verdict {
        Verdict::Healthy => Decision::Attach { rw },
        v if rw => Decision::Refuse(format!("writing needs a healthy space, and this one is {}", v.as_str())),
        v if v.allowed(degraded, force) => Decision::ReadOnly,
        Verdict::Failed => Decision::Refuse("a failed space is not read at all".into()),
        v => Decision::Refuse(format!(
            "a {} space needs {} (and is then read-only)",
            v.as_str(),
            v.flag().unwrap_or("--force")
        )),
    }
}

/// What to do about a space the gate refused: the command that reads it
/// anyway, read-only, or where to turn.
pub fn refusal_note(verdict: Verdict, rw: bool, command: &str) -> String {
    match verdict.flag() {
        _ if rw && verdict != Verdict::Failed => {
            format!("Without --rw it is read as its verdict allows: {command}")
        }
        Some(flag) => format!("To read it anyway, read-only: {command} {flag}"),
        None => "Nothing reads a failed space: spaces check --bundle FILE.tar.gz packs the reports \
                 and the pool's metadata for a bug report."
            .into(),
    }
}

/// The health of a pool as the checks of its spaces take it, worked out
/// once for them all, on a thread of its own ([`pool_health`]).
pub type PoolHealth = Arc<Result<Result<Health, String>, Stopped>>;

/// Works out the health of `pool` for [`space_report`].
pub fn pool_health<D: ReadAt + 'static>(pool: &Arc<Pool<D>>, limit: Option<Duration>) -> PoolHealth {
    let shared = Arc::clone(pool);
    Arc::new(isolate::isolated(limit, move || guard::pool_health(&shared)))
}

/// The report on space `space` of `pool` (quick checks, as attach makes),
/// made on a thread of its own: checks that panic give a failed
/// `space.checks`, checks past the [`deadline`] a suspect one.
pub fn space_report<D: ReadAt + 'static>(
    pool: &Arc<Pool<D>>,
    health: &PoolHealth,
    paths: &[PathBuf],
    space: &Space,
) -> Report {
    report(pool, health, paths, space, false, deadline(), fs_checks)
}

/// What the file systems of a space are checked with: [`fs_checks`] (the
/// tests give others).
type FsChecks<D> = fn(&SpaceReader<'_, D>, &Partition, &[String], bool) -> Vec<Check>;

/// The report on `space` (with `deep`, the deep checks too), made on a
/// thread of its own that may take `limit`.
fn report<D: ReadAt + 'static>(
    pool: &Arc<Pool<D>>,
    health: &PoolHealth,
    paths: &[PathBuf],
    space: &Space,
    deep: bool,
    limit: Option<Duration>,
    fs: FsChecks<D>,
) -> Report {
    if let Err(stopped) = health.as_ref() {
        return stopped_report(pool, space, stopped);
    }
    let (shared, health, names, id) = (Arc::clone(pool), Arc::clone(health), names(paths), space.id());
    let work = move || {
        let space = &shared.spaces[&id];
        let options = Options {
            journal_budget: if deep { None } else { Options::default().journal_budget },
        };
        let health = health.as_ref().as_ref().expect("a stopped health returned above");
        let mut r = guard::space_report_with(&shared, space, health, &names, &options, &mut |reader, part, names| {
            fs(reader, part, names, deep)
        });
        if deep {
            r.checks.push(deep_scrub(&shared, space));
        }
        r
    };
    let mut r = isolate::isolated(limit, work).unwrap_or_else(|stopped| stopped_report(pool, space, &stopped));
    r.environment = environment(pool);
    r.devices = self::names(paths);
    r
}

/// The report on a space whose checks stopped.
fn stopped_report<D>(pool: &Pool<D>, space: &Space, stopped: &Stopped) -> Report {
    let mut r = guard::empty_report(pool, space);
    r.checks.push(stopped.check("space.checks", "the checks of the space"));
    r
}

/// Refuses what the gate refuses for the servers and export: an error
/// with the report's headline and the reason. (The checks run on this
/// thread; a panic in them fails the space.)
pub fn require<D: ReadAt>(
    pool: &Pool<D>,
    paths: &[PathBuf],
    space: &Space,
    rw: bool,
    degraded: bool,
    force: bool,
) -> Result<()> {
    let names = names(paths);
    let report = isolate::caught(|| {
        let mut r = guard::space_report(pool, space, &names, &Options::default(), &mut |reader, part, names| {
            fs_checks(reader, part, names, false)
        });
        r.environment = environment(pool);
        r
    })
    .unwrap_or_else(|stopped| stopped_report(pool, space, &stopped));
    match gate(report.verdict(), rw, degraded, force) {
        Decision::Refuse(why) => {
            for c in report.problems() {
                eprintln!(
                    "  {} {}: {}",
                    c.status.as_str().to_uppercase(),
                    c.id,
                    storage_spaces::report::printable(&c.summary)
                );
            }
            bail!("{}: {why} (spaces check tells more)", report.headline())
        }
        _ => Ok(()),
    }
}

/// Device names for evidence: the paths the devices were opened by.
pub fn names(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

/// The checks of the file system in a partition of a space (`number` 0:
/// the whole space): NTFS and ReFS, found by their boot sectors; with
/// `deep`, a ReFS volume is also checked whole, as `refs check` does.
pub fn fs_checks<D: ReadAt>(reader: &SpaceReader<'_, D>, part: &Partition, names: &[String], deep: bool) -> Vec<Check> {
    let label = if part.number == 0 {
        "the space".to_string()
    } else {
        format!("p{}", part.number)
    };
    let locate = |o: u64| reader.describe(o, names);
    if let Some(c) = guard::ntfs_check(reader, part.offset, part.length, &label, &locate) {
        return vec![c];
    }
    let mut checks = refs::health::quick_checks_in(reader, part.offset, Some(part.length), &label, &locate);
    if checks.is_empty() {
        if part.kind == MSR_PARTITION_TYPE {
            return Vec::new();
        }
        return vec![Check::skipped(
            "fs",
            format!("{label}: neither NTFS nor ReFS, not checked"),
        )];
    }
    if deep
        && checks
            .iter()
            .all(|c| c.status != Status::Failed && c.status != Status::Warning)
    {
        checks.push(deep_refs(reader, part.offset, &label));
    }
    checks
}

/// `deep.refs`: the ReFS volume checked as `refs check` does.
fn deep_refs<D: ReadAt>(reader: &SpaceReader<'_, D>, offset: u64, label: &str) -> Check {
    const ID: &str = "deep.refs";
    let vol = match refs::Volume::open(reader, offset) {
        Ok(v) => v,
        Err(e) => {
            return Check::new(ID, Status::Suspect, format!("{label}: the volume does not open: {e}"))
                .truth(Truth::Format);
        }
    };
    match vol.check(&[]) {
        Ok(r) if r.problem_count == 0 => Check::ok(
            ID,
            format!(
                "{label}: {} pages, {} directories and {} files checked, no problems",
                r.pages, r.directories, r.files
            ),
        )
        .truth(Truth::CrossCheck),
        Ok(r) => {
            let mut check = Check::new(
                ID,
                Status::Suspect,
                format!("{label}: {} problem(s) in the volume's structures", r.problem_count),
            )
            .truth(Truth::CrossCheck)
            .advice("Windows repairs what it can of a ReFS volume when it attaches it (or with refsutil).");
            for p in r.problems.iter().take(5) {
                check = check.evidence(Evidence::new(p.clone()));
            }
            check
        }
        Err(e) => Check::new(ID, Status::Suspect, format!("{label}: the check stopped: {e}")).truth(Truth::CrossCheck),
    }
}

/// `deep.scrub`: every copy of the space's mirror rows and every stripe of
/// a single parity space compared (`spaces pool scrub`, read-only).
fn deep_scrub<D: ReadAt>(pool: &Pool<D>, space: &Space) -> Check {
    const ID: &str = "deep.scrub";
    let scrub = match storage_spaces::ops::scrub_spaces(pool, Some(space.id())) {
        Ok(s) => s,
        // Asked for, and not made: no pass.
        Err(e) => {
            return Check::new(ID, Status::Suspect, "the copies of the space cannot be compared")
                .evidence(
                    Evidence::new("every copy of the space")
                        .expected("read and compared")
                        .found(e.to_string()),
                )
                .truth(Truth::CrossCheck)
                .advice(
                    "The deep check did not run: what it would have found is not known. The scrub needs a \
                     clean pool (every disk, no copy out of date, every record understood).",
                );
        }
    };
    let line = scrub.lines.first().cloned().unwrap_or_default();
    if line.ends_with("not scrubbed") {
        return Check::skipped(ID, line);
    }
    if scrub.mismatches > 0 {
        return Check::new(ID, Status::Suspect, line).truth(Truth::CrossCheck).advice(
            "Copies of the space differ where nothing was being written: one is wrong, and which is not \
             known. spaces pool scrub --repair makes them agree (keeping the first copy, and the data \
             of parity spaces).",
        );
    }
    if scrub.unsettled > 0 {
        return Check::new(ID, Status::Suspect, line).truth(Truth::CrossCheck).advice(
            "Copies differ where writes were under way when the pool was last in use; Windows settles \
             them when it next attaches the pool. Reads of them fail (--unclean-parity data reads one).",
        );
    }
    Check::ok(ID, "every copy and every parity unit read agrees").truth(Truth::CrossCheck)
}

/// What a report says about the system it was made on.
pub fn environment<D>(pool: &Pool<D>) -> Vec<(String, String)> {
    let mut env = vec![("spaces".to_string(), env!("CARGO_PKG_VERSION").to_string())];
    if let Ok(k) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        env.push(("kernel".into(), k.trim().into()));
    }
    env.push(("pool version".into(), pool.version.to_string()));
    env
}

/// The reports on the user spaces of `pool`, or on the space `only` names
/// ([`Pool::select_space`]: none when it names none here, an error when it
/// names several).
pub fn reports<D: ReadAt + 'static>(
    pool: &Arc<Pool<D>>,
    paths: &[PathBuf],
    only: Option<&str>,
    deep: bool,
) -> Result<Vec<Report>> {
    let chosen = match only.map(|key| pool.select_space(key)).transpose()? {
        Some(None) => return Ok(Vec::new()),
        Some(Some(space)) => Some(space.id()),
        None => None,
    };
    // The deep checks read every copy: no time limit.
    let limit = if deep { None } else { deadline() };
    let health = pool_health(pool, deadline());
    Ok(pool
        .user_spaces()
        .filter(|s| chosen.is_none_or(|id| s.id() == id))
        .map(|space| report(pool, &health, paths, space, deep, limit, fs_checks))
        .collect())
}

/// The report on devices that do not open as a pool.
fn unopened(paths: &[PathBuf], error: &str) -> Report {
    let mut r = Report::new(format!("the pool on {}", names(paths).join(", ")));
    r.checks.push(
        Check::new("pool.open", Status::Failed, "the pool cannot be assembled")
            .evidence(Evidence::new("the pool's metadata").expected("readable").found(error))
            .truth(Truth::Format),
    );
    r
}

/// Files of a bundle for the pool on `paths` (under `pool-<guid>/`).
fn bundle_files<D: ReadAt>(pool: &Pool<D>, reports: &[Report], out: &mut bundle::Files) {
    let dir = format!("pool-{}", pool.guid);
    let mut add = |name: String, data: Vec<u8>| out.push((format!("{dir}/{name}"), data));
    for r in reports {
        let guid = r.space.as_ref().map_or("pool".to_string(), |s| s.1.clone());
        add(format!("reports/{guid}.txt"), r.to_text().into_bytes());
        add(format!("reports/{guid}.json"), r.to_json().into_bytes());
    }
    add(
        "dump.txt".into(),
        crate::dump::dump(pool)
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<String>()
            .into_bytes(),
    );
    for space in pool.user_spaces() {
        let guid = space.info.guid.to_string();
        add(
            format!("extents/{guid}.txt"),
            crate::extents_text(pool, space).into_bytes(),
        );
        // The partition table's sectors at both ends of the space.
        const ENDS: u64 = 64 << 10;
        match pool.open_space(space.id()) {
            Ok(reader) => {
                for (what, at) in [("start", 0), ("end", reader.size().saturating_sub(ENDS))] {
                    let len = ENDS.min(reader.size()) as usize;
                    let mut buf = vec![0u8; len];
                    match reader.read_exact_at(&mut buf, at) {
                        Ok(()) => add(format!("gpt/{guid}-{what}.bin"), buf),
                        Err(e) => add(format!("gpt/{guid}-{what}.error"), e.to_string().into_bytes()),
                    }
                }
            }
            Err(e) => add(format!("gpt/{guid}.error"), e.to_string().into_bytes()),
        }
    }
}

/// `spaces check`: prints the reports; returns whether every space is
/// healthy. An error when there was nothing to check: no pool found (or
/// none that could be read), or no space that `only` names (1.2.1 said so
/// and exited 0, so that a script took it for healthy; `--json` printed
/// that text, not `[]`).
pub fn run(
    json: bool,
    deep: bool,
    only: Option<&str>,
    bundle_path: Option<&Path>,
    devices: &[PathBuf],
    here: bool,
) -> Result<bool> {
    let groups: Vec<Vec<PathBuf>> = if devices.is_empty() {
        scanned()
    } else if here {
        vec![devices.to_vec()]
    } else {
        groups_of(devices)
    };
    let mut all = Vec::new();
    let mut files = bundle::Files::new();
    let mut pools = 0;
    if here || bundle_path.is_some() {
        // In this process: the child of `check` and `attach` (`--here`),
        // and a bundle, which packs what it reads of each pool.
        for paths in groups {
            match open_pool(&paths) {
                Ok(pool) => {
                    pools += 1;
                    #[cfg(feature = "test-hooks")]
                    test_hooks(&pool);
                    let pool = Arc::new(pool);
                    let reports = reports(&pool, &paths, only, deep)?;
                    if bundle_path.is_some()
                        && let Err(stopped) = isolate::caught(|| bundle_files(&pool, &reports, &mut files))
                    {
                        files.push((
                            format!("pool-{}/bundle-error.txt", pool.guid),
                            format!("packing the pool's files {stopped}\n").into_bytes(),
                        ));
                    }
                    all.extend(reports);
                }
                Err(e) => all.push(unopened(&paths, &format!("{e:#}"))),
            }
        }
        if here {
            print!("{}", reports_to_json(&all));
            return Ok(all.iter().all(|r| r.verdict() == Verdict::Healthy));
        }
    } else {
        // Each pool in a child process of its own, with a memory limit
        // and a deadline (`reports_in_children`).
        for result in reports_in_children(&groups, only, deep) {
            pools += 1;
            all.extend(result?);
        }
    }
    if json {
        print!("{}", reports_to_json(&all));
    }
    if all.is_empty() {
        match only {
            Some(key) if pools > 0 => bail!("no space named {key:?} in the pools found"),
            _ if pools == 0 => bail!("no Storage Spaces pool members found"),
            _ => println!("no spaces in the pools found"),
        }
    } else if !json {
        for (i, r) in all.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print!("{}", r.to_text());
        }
    }
    if let Some(path) = bundle_path {
        files.insert(
            0,
            (
                "README.txt".into(),
                b"spaces check --bundle: the reports, `spaces dump` and `spaces extents` of each pool,\n\
                  and the first and last 64 KiB of each space (its partition table). No file data,\n\
                  but the names and GUIDs of pools, spaces and disks: review it before sharing it.\n"
                    .to_vec(),
            ),
        );
        let mut versions = format!("spaces {}\n", env!("CARGO_PKG_VERSION"));
        for (what, file) in [("kernel", "/proc/sys/kernel/osrelease"), ("os", "/etc/os-release")] {
            if let Ok(text) = std::fs::read_to_string(file) {
                let value = if what == "os" {
                    text.lines()
                        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string()
                } else {
                    text.trim().to_string()
                };
                versions += &format!("{what} {value}\n");
            }
        }
        files.insert(1, ("versions.txt".into(), versions.into_bytes()));
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        std::fs::write(path, bundle::tar_gz(&files, mtime))
            .with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!(
            "bundle: {} (pool, space and disk names and GUIDs; no file data: review it before sharing)",
            path.display()
        );
    }
    Ok(all.iter().all(|r| r.verdict() == Verdict::Healthy))
}

/// Devices given together, by pool: those of several pools (their disk
/// headers name the pool) are checked each pool on its own; devices of one
/// pool, or whose headers cannot be read, stay together (as before 1.3,
/// when every device given was one pool's).
pub fn groups_of(devices: &[PathBuf]) -> Vec<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        let found: Option<Vec<_>> = devices
            .iter()
            .map(|p| crate::scan::probe(p).ok().flatten().map(|c| c.pool_guid))
            .collect();
        if let Some(guids) = found {
            let mut groups: Vec<(storage_spaces::Guid, Vec<PathBuf>)> = Vec::new();
            for (guid, path) in guids.into_iter().zip(devices) {
                match groups.iter_mut().find(|g| g.0 == guid) {
                    Some(g) => g.1.push(path.clone()),
                    None => groups.push((guid, vec![path.clone()])),
                }
            }
            return groups.into_iter().map(|g| g.1).collect();
        }
    }
    vec![devices.to_vec()]
}

/// How much memory the checks of one pool may take: SPACES_POOL_MEMORY MiB
/// (0: no limit), 2048 by default.
fn pool_memory() -> Option<u64> {
    match std::env::var("SPACES_POOL_MEMORY")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(0) => None,
        Some(mib) => Some(mib),
        None => Some(2048),
    }
}

/// How long the checks of one pool may take: SPACES_POOL_TIMEOUT seconds
/// (0: no limit), 120 by default, and no longer than what is left of the
/// time of `spaces attach`. The deep checks, which read every copy, have
/// no limit unless SPACES_POOL_TIMEOUT sets one (a scrub of a large pool
/// takes hours).
fn pool_deadline(deep: bool) -> Option<Duration> {
    let own = pool_deadline_of(std::env::var("SPACES_POOL_TIMEOUT").ok().as_deref(), deep);
    match (own, time_left()) {
        (Some(own), Some(left)) => Some(own.min(left)),
        (own, left) => own.or(left),
    }
}

/// The [`pool_deadline`] a value of SPACES_POOL_TIMEOUT sets.
fn pool_deadline_of(value: Option<&str>, deep: bool) -> Option<Duration> {
    match value.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(s) => Some(Duration::from_secs(s)),
        None if deep => None,
        None => Some(Duration::from_secs(120)),
    }
}

/// The reports on each pool of `groups` (its member devices), each made by
/// `spaces check --json --here` in a child process of its own, all at once:
/// with a memory limit (`ulimit -v`, [`pool_memory`]) and killed at the
/// [`pool_deadline`]. A child that ended without reports gives a failed
/// `space.checks` report on the pool naming why (the limit it reached);
/// one that refused the request (a `--space` that names two spaces) its
/// error. The threads that bound each check stay inside the child, which
/// ends with them: 1.2.2 checked every pool on threads of one process,
/// where a thread that hung stayed to the end and memory had no bound
/// (audit 2026-10-16, E9).
pub fn reports_in_children(groups: &[Vec<PathBuf>], only: Option<&str>, deep: bool) -> Vec<Result<Vec<Report>>> {
    let (memory, deadline) = (pool_memory(), pool_deadline(deep));
    let children: Vec<_> = groups
        .iter()
        .map(|paths| spawn_child(paths, only, deep, memory))
        .collect();
    let start = Instant::now();
    children
        .into_iter()
        .zip(groups)
        .map(|(child, paths)| {
            child.and_then(|c| {
                c.wait(
                    paths,
                    deadline.map(|d| d.saturating_sub(start.elapsed())),
                    memory,
                    deadline,
                )
            })
        })
        .collect()
}

/// A child checking one pool, its output read beside it.
struct Child {
    process: std::process::Child,
    out: std::thread::JoinHandle<Vec<u8>>,
    err: std::thread::JoinHandle<Vec<u8>>,
}

fn spawn_child(paths: &[PathBuf], only: Option<&str>, deep: bool, memory: Option<u64>) -> Result<Child> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().context("cannot find this program to check the pool in a child")?;
    let mut cmd = match memory {
        // The shell sets the limit and becomes the child (exec).
        Some(mib) => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg(format!("ulimit -v {} 2>/dev/null; exec \"$0\" \"$@\"", mib * 1024))
                .arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    // Global options first (`--unclean-parity`), then the command.
    cmd.args(crate::inherited_args()).args(["check", "--json", "--here"]);
    if deep {
        cmd.arg("--deep");
    }
    if let Some(key) = only {
        cmd.args(["--space", key]);
    }
    cmd.arg("--").args(paths);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut process = cmd.spawn().context("cannot start the child that checks the pool")?;
    fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut v);
            }
            v
        })
    }
    let (out, err) = (drain(process.stdout.take()), drain(process.stderr.take()));
    Ok(Child { process, out, err })
}

impl Child {
    /// Waits for the child until `left` is up (then kills it) and reads its
    /// reports; `memory` and `deadline` name the limits in a report.
    fn wait(
        mut self,
        paths: &[PathBuf],
        left: Option<Duration>,
        memory: Option<u64>,
        deadline: Option<Duration>,
    ) -> Result<Vec<Report>> {
        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = self.process.try_wait()? {
                break status;
            }
            if left.is_some_and(|l| start.elapsed() >= l) {
                let _ = self.process.kill();
                timed_out = true;
                break self.process.wait()?;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let out = self.out.join().unwrap_or_default();
        let err = String::from_utf8_lossy(&self.err.join().unwrap_or_default()).into_owned();
        // What it said besides its reports (warnings), as said.
        eprint!(
            "{}",
            err.lines()
                .filter(|l| !l.starts_with("Error: "))
                .map(|l| format!("{l}\n"))
                .collect::<String>()
        );
        let reports = std::str::from_utf8(&out).ok().and_then(Report::list_from_json);
        if let (false, Some(reports)) = (timed_out, reports) {
            return Ok(reports);
        }
        // Its own refusal of the request: the error it gave.
        if !timed_out && status.code() == Some(2) {
            let error = err
                .lines()
                .filter_map(|l| l.strip_prefix("Error: "))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("{error}");
        }
        let why = if timed_out {
            format!(
                "did not end within {} s (SPACES_POOL_TIMEOUT), and were stopped",
                deadline.map_or(0, |d| d.as_secs())
            )
        } else if err.contains("memory allocation of") {
            format!(
                "needed more than {} MiB of memory (SPACES_POOL_MEMORY), and were stopped",
                memory.unwrap_or(0)
            )
        } else {
            let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
            format!(
                "ended without reports ({status}): {}",
                storage_spaces::report::printable(last)
            )
        };
        Ok(vec![stopped_pool(paths, &why)])
    }
}

/// The report on a space of `pool` the child that checked the pool gave no
/// report on: failed (the pool changed between the child's look and this
/// one).
pub fn no_report<D>(pool: &Pool<D>, space: &Space, paths: &[PathBuf]) -> Report {
    let mut r = guard::empty_report(pool, space);
    r.devices = names(paths);
    r.checks.push(Check::new(
        "space.checks",
        Status::Failed,
        "the checks of the pool gave no report on this space",
    ));
    r
}

/// The report on the pool on `paths` whose checks in a child process ended
/// without reports (`why`).
fn stopped_pool(paths: &[PathBuf], why: &str) -> Report {
    let mut r = Report::new(format!("the pool on {}", names(paths).join(", ")));
    r.devices = names(paths);
    r.checks.push(
        Check::new("space.checks", Status::Failed, format!("the checks of the pool {why}")).advice(
            "A pool whose checks take that much is damaged or made to be read so; nothing of it is attached. \
                 The limits are SPACES_POOL_MEMORY (MiB) and SPACES_POOL_TIMEOUT (seconds); 0 lifts one.",
        ),
    );
    r
}

/// The test hooks of the checks (feature test-hooks): the checks of the
/// pool SPACES_TEST_HOG_POOL names allocate without bound, those of
/// SPACES_TEST_HANG_POOL hang (tests/pool_children.rs).
#[cfg(feature = "test-hooks")]
fn test_hooks<D>(pool: &Pool<D>) {
    if std::env::var("SPACES_TEST_HOG_POOL").is_ok_and(|n| n == pool.name) {
        let mut held: Vec<Vec<u8>> = Vec::new();
        loop {
            held.push(vec![1u8; 64 << 20]);
        }
    }
    if std::env::var("SPACES_TEST_HANG_POOL").is_ok_and(|n| n == pool.name) {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// The member devices of every pool found by scanning.
#[cfg(target_os = "linux")]
fn scanned() -> Vec<Vec<PathBuf>> {
    let found = crate::scan::scan();
    if found.denied > 0 {
        eprintln!(
            "{} block device(s) could not be opened (permission denied); run as root to see them all",
            found.denied
        );
    }
    found
        .pools
        .into_values()
        .map(|m| m.into_iter().map(|c| c.path).collect())
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn scanned() -> Vec<Vec<PathBuf>> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbers in a pool's extents beyond any disk (a physical slab of
    /// 2^36, a run of 2^36 slabs): `spaces extents` and `spaces info` show
    /// them, and `spaces check --bundle` packs them, instead of overflowing
    /// (1.2.1 panicked in each, and the bundle is what a user with a bad
    /// pool is told to make).
    #[test]
    fn numbers_beyond_any_disk_are_shown_not_computed_with() {
        use storage_spaces::io::SparseImage;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../storage-spaces/tests/fixtures/mirror2");
        let disks: Vec<SparseImage> = (0..)
            .map_while(|i| std::fs::File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let mut pool = Pool::open(disks).unwrap();
        let id = pool.user_spaces().next().unwrap().id();
        let e = &mut pool.spaces.get_mut(&id).unwrap().extents[0];
        (e.physical_slab, e.slab_count) = (1 << 36, 1 << 36);
        let pool = Arc::new(pool);
        let space = &pool.spaces[&id];
        let text = crate::extents_text(&pool, space);
        let offset = (1u128 << 36) * u128::from(storage_spaces::format::SLAB_SIZE);
        assert!(text.contains(&format!("{offset:#x}")), "{text}");
        assert_eq!(space.allocated(), u64::MAX);
        let reports = reports(&pool, &[], None, false).unwrap();
        let mut files = bundle::Files::new();
        bundle_files(&pool, &reports, &mut files);
        assert!(files.iter().any(|(path, _)| path.contains("/extents/")));
    }

    /// 40 s unless SPACES_CHECK_TIMEOUT says otherwise; 0 is no limit.
    #[test]
    fn the_checks_take_40_s_unless_the_variable_says_otherwise() {
        assert_eq!(deadline_of(None), Some(Duration::from_secs(40)));
        assert_eq!(deadline_of(Some("5")), Some(Duration::from_secs(5)));
        assert_eq!(deadline_of(Some(" 120\n")), Some(Duration::from_secs(120)));
        assert_eq!(deadline_of(Some("0")), None);
        assert_eq!(deadline_of(Some("soon")), Some(Duration::from_secs(40)));
    }

    /// A pool's child has 120 s unless SPACES_POOL_TIMEOUT says otherwise;
    /// with the deep checks, which may take hours, none unless it sets one
    /// (the first version of 1.3's children killed a deep scrub at 120 s).
    #[test]
    fn a_pools_checks_take_120_s_and_the_deep_ones_their_time() {
        assert_eq!(pool_deadline_of(None, false), Some(Duration::from_secs(120)));
        assert_eq!(pool_deadline_of(None, true), None);
        assert_eq!(pool_deadline_of(Some("30"), true), Some(Duration::from_secs(30)));
        assert_eq!(pool_deadline_of(Some("0"), false), None);
        assert_eq!(pool_deadline_of(Some("soon"), false), Some(Duration::from_secs(120)));
    }

    /// A device that panics on every read once `armed`.
    struct Trap {
        disk: storage_spaces::io::SparseImage,
        armed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ReadAt for Trap {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
            assert!(!self.armed.load(std::sync::atomic::Ordering::Relaxed), "a defect");
            self.disk.read_exact_at(buf, offset)
        }

        fn size(&self) -> std::io::Result<u64> {
            self.disk.size()
        }
    }

    /// The gate of export and the servers (`require`): checks that panic
    /// refuse the space with a failed `space.checks`, whatever the flags.
    #[test]
    fn checks_that_panic_refuse_the_space_to_the_servers_and_export() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../storage-spaces/tests/fixtures/mirror3");
        let armed = Arc::new(AtomicBool::new(false));
        let disks: Vec<Trap> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| Trap {
                disk: storage_spaces::io::SparseImage::read_from(f).unwrap(),
                armed: Arc::clone(&armed),
            })
            .collect();
        let pool = Pool::open(disks).unwrap();
        let space = pool.user_spaces().next().unwrap();
        require(&pool, &[], space, false, false, false).unwrap();
        armed.store(true, Ordering::Relaxed);
        for (degraded, force) in [(false, false), (true, true)] {
            let e = require(&pool, &[], space, false, degraded, force)
                .unwrap_err()
                .to_string();
            assert!(e.contains("FAILED") && e.contains("space.checks"), "{e}");
        }
    }

    #[test]
    fn the_gate_follows_the_verdict_and_the_flags() {
        use Decision::*;
        assert_eq!(gate(Verdict::Healthy, false, false, false), Attach { rw: false });
        assert_eq!(gate(Verdict::Healthy, true, false, false), Attach { rw: true });
        assert!(matches!(gate(Verdict::Degraded, false, false, false), Refuse(w) if w.contains("--degraded")));
        assert_eq!(gate(Verdict::Degraded, false, true, false), ReadOnly);
        assert_eq!(gate(Verdict::Degraded, false, false, true), ReadOnly);
        assert!(matches!(gate(Verdict::Suspect, false, true, false), Refuse(w) if w.contains("--force")));
        assert_eq!(gate(Verdict::Suspect, false, false, true), ReadOnly);
        assert!(matches!(gate(Verdict::Failed, false, true, true), Refuse(w) if w.contains("failed")));
        // Writing needs healthy, whatever the flags.
        for v in [Verdict::Degraded, Verdict::Suspect, Verdict::Failed] {
            assert!(
                matches!(gate(v, true, true, true), Refuse(w) if w.contains("writing needs")),
                "{v:?}"
            );
        }
        assert!(refusal_note(Verdict::Suspect, false, "spaces attach").ends_with("spaces attach --force"));
        assert!(refusal_note(Verdict::Failed, false, "spaces attach").contains("--bundle"));
    }

    /// `--pool` names a pool by its GUID (in any case) or name; a name two
    /// pools share, or one that is another pool's GUID, names neither
    /// (1.2.2: attach took both; audit M12).
    #[test]
    fn a_pool_key_that_names_two_pools_is_refused() {
        use storage_spaces::Guid;
        let (g1, g2) = (Guid([1; 16]), Guid([2; 16]));
        let pools = [("Storage pool", g1), ("Storage pool", g2)];
        let e = select_pool(&pools, "Storage pool").unwrap_err().to_string();
        assert!(e.contains(&g1.to_string()) && e.contains(&g2.to_string()), "{e}");
        assert_eq!(select_pool(&pools, &g2.to_string().to_uppercase()).unwrap(), Some(1));
        assert_eq!(select_pool(&pools, "other").unwrap(), None);
        let named = g1.to_string();
        let pools = [("data", g1), (named.as_str(), g2)];
        assert!(select_pool(&pools, &named).is_err());
        assert_eq!(select_pool(&pools, "data").unwrap(), Some(0));
        assert_eq!(select_pool(&pools, &g2.to_string()).unwrap(), Some(1));
    }

    /// A deep check that cannot run (here the scrub refuses a pool with a
    /// warning, an out-of-date database copy) is no pass: the space is
    /// suspect, and `spaces check --deep` fails (1.2.2: skipped, healthy,
    /// exit 0; audit M11).
    #[test]
    fn a_deep_check_that_cannot_run_is_no_pass() {
        use storage_spaces::io::SparseImage;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../storage-spaces/tests/fixtures/mirror3");
        let disks: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let mut pool = Pool::open(disks).unwrap();
        pool.warnings.push("a copy of the pool database is out of date".into());
        let pool = Arc::new(pool);
        let space = pool.user_spaces().next().unwrap();
        let health = pool_health(&pool, None);
        let quick = report(&pool, &health, &[], space, false, None, |_, _, _, _| Vec::new());
        assert_eq!(quick.verdict(), Verdict::Healthy, "{}", quick.to_text());
        let deep = report(&pool, &health, &[], space, true, None, |_, _, _, _| Vec::new());
        let scrub = deep.checks.iter().find(|c| c.id == "deep.scrub").unwrap();
        assert_ne!(scrub.status, Status::Skipped, "{}", deep.to_text());
        assert_eq!(deep.verdict(), Verdict::Suspect, "{}", deep.to_text());
    }

    /// Checks that panic fail their space, checks that hang past the limit
    /// make it suspect; either way the report comes back.
    #[test]
    fn checks_that_panic_or_hang_stop_their_space_only() {
        use storage_spaces::io::SparseImage;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../storage-spaces/tests/fixtures/mirror3");
        let disks: Vec<SparseImage> = (0..)
            .map_while(|i| File::open(dir.join(format!("disk{i}.fixture"))).ok())
            .map(|f| SparseImage::read_from(f).unwrap())
            .collect();
        let pool = Arc::new(Pool::open(disks).unwrap());
        let space = pool.user_spaces().next().unwrap();
        let limit = Some(Duration::from_secs(30));
        let health = pool_health(&pool, limit);
        let fine = report(&pool, &health, &[], space, false, limit, |_, _, _, _| Vec::new());
        assert_eq!(fine.verdict(), Verdict::Healthy, "{}", fine.to_text());
        assert!(fine.subject.starts_with("space \""), "{}", fine.subject);

        let r = report(&pool, &health, &[], space, false, limit, |_, _, _, _| {
            panic!("a defect")
        });
        assert_eq!(r.verdict(), Verdict::Failed, "{}", r.to_text());
        let c = r.checks.iter().find(|c| c.id == "space.checks").unwrap();
        assert!(c.evidence[0].found.as_deref().unwrap().contains("a defect"), "{c:?}");
        assert_eq!(r.space, fine.space);
        assert!(!r.environment.is_empty());

        let r = report(
            &pool,
            &health,
            &[],
            space,
            false,
            Some(Duration::from_millis(200)),
            |_, _, _, _| loop {
                std::thread::sleep(Duration::from_millis(50));
            },
        );
        assert_eq!(r.verdict(), Verdict::Suspect, "{}", r.to_text());
        assert!(
            r.headline()
                .contains("space.checks: the checks of the space did not finish in 0 s")
        );
    }
}
