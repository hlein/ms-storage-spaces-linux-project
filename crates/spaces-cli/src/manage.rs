//! `spaces pool`, `spaces space` and `spaces disk`: pool management. Every
//! command computes its plan first and prints it; it writes only with
//! `--yes`. Devices are opened exclusively (O_EXCL on block devices, and the
//! lock every `spaces` process serving or writing them takes).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage_spaces::report::printable;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use storage_spaces::io::ReadAt;
use storage_spaces::ops::{self, BlankDisk, SpaceSpec};
use storage_spaces::plan::{Action, Plan, Target};
use storage_spaces::{Guid, Pool};

#[derive(Subcommand)]
pub enum PoolCommand {
    /// Create a pool of blank disks (Windows 11 24H2 layout, pool version 28).
    Create {
        /// The pool's name.
        #[arg(long)]
        name: String,
        /// Logical sector size of the pool's spaces (default: the largest of
        /// the disks', 512 or 4096).
        #[arg(long)]
        logical_sector: Option<u32>,
        /// Overwrite disks that are not blank (partition tables, file systems:
        /// everything on them is lost).
        #[arg(long)]
        wipe: bool,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Rename the pool (its partitions keep the old name, as on Windows).
    Rename {
        /// The new name.
        #[arg(long)]
        name: String,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Rebuild every copy on a missing disk or out of date (mirror copies
    /// from another copy, parity columns from the others) on other disks;
    /// a missing disk can be removed afterwards (spaces disk remove).
    Repair {
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// The health Windows would show for the pool with the disks given
    /// (HealthStatus / OperationalStatus of the pool, its disks and spaces;
    /// read only). Disks of the pool that are not given count as missing.
    Health {
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Read every copy of the mirror spaces and every stripe of the single
    /// parity spaces and report what disagrees (read only); `--repair`
    /// makes it agree, keeping the first mirror copy and the parity data.
    Scrub {
        /// Plan making what disagrees agree (written with --yes).
        #[arg(long)]
        repair: bool,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Spread the extents evenly over the disks (after adding a disk), as
    /// Optimize-StoragePool does.
    Optimize {
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Remove a pool without spaces: the pool partition leaves every
    /// member's partition table.
    Remove {
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum TierCommand {
    /// Create a tier template (New-StorageTier) for `space create --tier`.
    Create {
        /// The template's name.
        #[arg(long)]
        name: String,
        /// ssd or hdd: the disks its tiers go on (spaces disk set --media).
        #[arg(long)]
        media: String,
        /// simple, mirror (two-way) or parity (single parity).
        #[arg(long)]
        resiliency: String,
        /// Columns (default: chosen when a space is created).
        #[arg(long)]
        columns: Option<u64>,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
}

pub fn tier(command: TierCommand) -> Result<()> {
    match command {
        TierCommand::Create {
            name,
            media,
            resiliency,
            columns,
            yes,
            devices,
        } => {
            let ssd = match media.as_str() {
                "ssd" => true,
                "hdd" => false,
                other => bail!("media {other}: ssd or hdd"),
            };
            let resiliency = match resiliency.as_str() {
                "simple" => 1,
                "mirror" => 2,
                "parity" => 3,
                other => bail!("resiliency {other}: simple, mirror or parity"),
            };
            on_pool(&devices, yes, |p| {
                ops::plan_create_tier(p, &name, ssd, resiliency, columns, &mut random_guids())
            })
        }
    }
}

#[derive(Subcommand)]
pub enum DiskCommand {
    /// Add a blank disk (`--new`) to the pool of the other devices.
    Add {
        /// The blank disk.
        #[arg(long)]
        new: PathBuf,
        /// Overwrite the new disk if it is not blank.
        #[arg(long)]
        wipe: bool,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Set a disk's media type (unspecified, hdd, ssd) or usage
    /// (auto-select, manual-select, hot-spare).
    Set {
        /// The disk's id (spaces info).
        #[arg(long)]
        disk: u64,
        /// unspecified, hdd or ssd.
        #[arg(long)]
        media: Option<String>,
        /// auto-select, manual-select or hot-spare.
        #[arg(long)]
        usage: Option<String>,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Retire a disk: move everything on it to the other disks.
    Retire {
        /// The disk's id (spaces info).
        #[arg(long)]
        disk: u64,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Remove a disk from the pool: a retired one that holds nothing any
    /// more, or a missing one after `spaces pool repair`.
    Remove {
        /// The disk's id (spaces info).
        #[arg(long)]
        disk: u64,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum SpaceCommand {
    /// Create a space, with Windows' defaults for what is not given.
    Create {
        /// The space's name.
        #[arg(long)]
        name: String,
        /// simple, mirror or parity (single parity).
        #[arg(long, required_unless_present = "tier")]
        resiliency: Option<String>,
        /// Size, e.g. 10G (rounded up to whole rows).
        #[arg(long, value_parser = parse_size, required_unless_present = "tier")]
        size: Option<u64>,
        /// A tiered space instead: an SSD tier, then an HDD tier, each from
        /// a template (`spaces tier create`) with its size, e.g.
        /// `--tier fast=100G --tier big=2T`.
        #[arg(
            long,
            value_name = "TEMPLATE=SIZE",
            conflicts_with_all = ["resiliency", "size", "thin", "copies", "columns", "interleave", "write_cache"]
        )]
        tier: Vec<String>,
        /// Thin provisioning (rows allocated as they are written).
        #[arg(long)]
        thin: bool,
        /// Mirror: 2 (default) or 3 copies.
        #[arg(long)]
        copies: Option<u64>,
        /// Columns (default as Windows: simple one per disk up to 8, mirror
        /// disks / copies, parity 3).
        #[arg(long)]
        columns: Option<u64>,
        /// Interleave, e.g. 256K (default).
        #[arg(long, value_parser = parse_size)]
        interleave: Option<u64>,
        /// Parity: size of the write-back cache (default 1G, at least 512M).
        #[arg(long, value_parser = parse_size)]
        write_cache: Option<u64>,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Delete a space and everything on it.
    Delete {
        /// The space: its GUID, numeric id or name.
        #[arg(long)]
        space: String,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Rename a space.
    Rename {
        /// The space: its GUID, numeric id or name.
        #[arg(long)]
        space: String,
        /// The new name.
        #[arg(long)]
        name: String,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
    /// Grow a space (the file system inside is not grown).
    Resize {
        /// The space: its GUID, numeric id or name.
        #[arg(long)]
        space: String,
        /// The new size, e.g. 20G (rounded up to whole rows; only growing).
        #[arg(long, value_parser = parse_size)]
        size: u64,
        /// Write; without it the plan is only printed.
        #[arg(long)]
        yes: bool,
        /// The pool's disks (block devices or image files).
        #[arg(required = true)]
        devices: Vec<PathBuf>,
    },
}

/// Parses a size with an optional binary suffix (K, M, G, T).
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    let (digits, shift) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => (
            &s[..i],
            match c.to_ascii_uppercase() {
                'K' => 10,
                'M' => 20,
                'G' => 30,
                'T' => 40,
                _ => return Err(format!("unknown size suffix in {s}")),
            },
        ),
        _ => (s, 0),
    };
    let n: u64 = digits.parse().map_err(|_| format!("not a size: {s}"))?;
    n.checked_mul(1 << shift).ok_or_else(|| format!("size too large: {s}"))
}

/// Random (version 4) GUIDs from the kernel.
fn random_guids() -> impl FnMut() -> Guid {
    guids_from(File::open("/dev/urandom").ok())
}

/// Version 4 GUIDs from `source`, or where it gives none (no such file, a
/// read that fails), from the time, the process and a count, so that they
/// still differ. 1.2.2 panicked when /dev/urandom could not be read, and
/// without it made the same GUID twice within 100 ns (audit 2026-10-16,
/// writers).
fn guids_from(mut source: Option<File>) -> impl FnMut() -> Guid {
    let mut count = 0u64;
    move || {
        let mut g = [0u8; 16];
        let read = source
            .as_mut()
            .is_some_and(|f| std::io::Read::read_exact(f, &mut g).is_ok());
        if !read {
            source = None;
            count += 1;
            // splitmix64 of what tells this GUID apart.
            let mut z =
                ops::filetime_now() ^ (u64::from(std::process::id()) << 40) ^ count.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            g[..8].copy_from_slice(&ops::filetime_now().to_le_bytes());
            g[8..].copy_from_slice(&((z ^ (z >> 31)) ^ count).to_le_bytes());
        }
        g[6] = (g[6] & 0x0f) | 0x40;
        g[8] = (g[8] & 0x3f) | 0x80;
        Guid(g)
    }
}

fn open_rw(paths: &[PathBuf]) -> Result<Vec<File>> {
    paths
        .iter()
        .map(|p| crate::open_member(p, true, crate::Hold::Exclusive))
        .collect()
}

fn describe(path: &Path, file: &File) -> Result<BlankDisk> {
    let size = file.size()?;
    // What the file is, not where its name points (1.2.2 took every path
    // under /dev for a block device and every other one for an image:
    // audit 2026-10-16, writers).
    #[cfg(unix)]
    let block = std::os::unix::fs::FileTypeExt::is_block_device(&file.metadata()?.file_type());
    #[cfg(not(unix))]
    let block = false;
    let number = |attr: &str, default: u64| {
        crate::members::block_attr(path, attr)
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    Ok(BlankDisk {
        size,
        logical_sector: if block {
            number("queue/logical_block_size", 512)
        } else {
            512
        },
        physical_sector: if block {
            number("queue/physical_block_size", 4096)
        } else {
            4096
        },
        manufacturer: crate::members::block_attr(path, "device/vendor").unwrap_or_else(|| "Linux".into()),
        model: crate::members::block_attr(path, "device/model")
            .unwrap_or_else(|| if block { "Disk".into() } else { "Image".into() }),
    })
}

/// Whether the first and last MiB of a disk are zero.
fn is_blank(file: &File) -> Result<bool> {
    let size = file.size()?;
    let mut buf = vec![0u8; (1 << 20).min(size as usize)];
    file.read_exact_at(&mut buf, 0)?;
    if buf.iter().any(|&b| b != 0) {
        return Ok(false);
    }
    let end = size - buf.len() as u64;
    file.read_exact_at(&mut buf, end)?;
    Ok(buf.iter().all(|&b| b == 0))
}

/// Prints the plan; carries it out only with `yes`. Test builds (the
/// feature test-hooks) carry out only the first N steps with the
/// environment variable SPACES_STOP_AFTER_STEP=N, as a crash after them
/// would leave the disks (tools/mgmt-crash.sh).
fn run<M: storage_spaces::io::WriteAt, N: storage_spaces::io::WriteAt>(
    plan: &Plan,
    yes: bool,
    members: &[M],
    new: &[N],
) -> Result<bool> {
    print!("{plan}");
    if !yes {
        println!("nothing written (add --yes to carry the plan out)");
        return Ok(false);
    }
    #[cfg(feature = "test-hooks")]
    if let Some(n) = std::env::var("SPACES_STOP_AFTER_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        plan.apply_steps(members, new, n)?;
        println!(
            "stopped after step {n} of {} (SPACES_STOP_AFTER_STEP)",
            plan.steps.len()
        );
        std::process::exit(99);
    }
    plan.apply(members, new)?;
    println!("done");
    Ok(true)
}

/// Plans with the pool of `devices`, then carries the plan out on the same
/// handles: a path opened again could name another disk by then (1.2.2
/// opened the devices again for the writes: audit 2026-10-16, writers).
fn on_pool(
    devices: &[PathBuf],
    yes: bool,
    make: impl FnOnce(&Pool<Arc<File>>) -> storage_spaces::Result<Plan>,
) -> Result<()> {
    let members: Vec<Arc<File>> = open_rw(devices)?.into_iter().map(Arc::new).collect();
    let plan = make(&Pool::open(members.clone())?)?;
    if run::<Arc<File>, File>(&plan, yes, &members, &[])? {
        let pool = Pool::open(members)?;
        for w in &pool.warnings {
            eprintln!("warning: {w}");
        }
    }
    Ok(())
}

/// A first step that clears what the pool's writes leave of the old start
/// and end of the new disks `disks` (index of the new disk, size):
/// partition tables, file system signatures, the header and database of an
/// earlier pool in the first 17 MiB, the last MiB.
fn wipe_step(disks: &[(usize, u64)]) -> storage_spaces::plan::Step {
    let mut actions = Vec::new();
    for &(i, size) in disks {
        actions.push(Action::Write {
            target: Target::New(i),
            offset: 0,
            bytes: vec![0; (17 << 20).min(size as usize)],
        });
        actions.push(Action::Write {
            target: Target::New(i),
            offset: size - (1 << 20).min(size),
            bytes: vec![0; (1 << 20).min(size as usize)],
        });
    }
    storage_spaces::plan::Step {
        what: "wipe".into(),
        actions,
    }
}

pub fn pool(command: PoolCommand) -> Result<()> {
    match command {
        PoolCommand::Rename { name, yes, devices } => on_pool(&devices, yes, |p| ops::plan_rename_pool(p, &name)),
        PoolCommand::Remove { yes, devices } => on_pool(&devices, yes, ops::plan_remove_pool),
        PoolCommand::Repair { yes, devices } => on_pool(&devices, yes, ops::plan_repair),
        PoolCommand::Optimize { yes, devices } => on_pool(&devices, yes, ops::plan_rebalance),
        PoolCommand::Health { devices } => {
            let files = devices
                .iter()
                .map(|p| File::open(p).with_context(|| p.display().to_string()))
                .collect::<Result<Vec<_>>>()?;
            let pool = Pool::open(files)?;
            let h = storage_spaces::health::health(&pool)?;
            println!(
                "pool \"{}\": {} ({} of {} database copies at hand)",
                printable(&pool.name),
                h.pool,
                h.database_copies.0,
                h.database_copies.1
            );
            for d in &h.disks {
                let disk = &pool.disks[&d.id];
                println!("  disk {} {}: {} (usage {:?})", d.id, disk.guid, d.state, disk.usage);
            }
            for s in &h.spaces {
                let left = s
                    .failures_left
                    .map_or("data lost".to_owned(), |n| format!("survives {n} more disk failure(s)"));
                println!("  space \"{}\": {} ({left})", printable(&s.name), s.state);
            }
            for (id, why) in &h.unknown {
                println!(
                    "  space \"{}\": not worked out ({why})",
                    printable(pool.spaces[id].name())
                );
            }
            Ok(())
        }
        PoolCommand::Scrub { repair, yes, devices } => {
            if repair {
                return on_pool(&devices, yes, |p| {
                    let scrub = ops::scrub(p)?;
                    scrub.lines.iter().for_each(|l| println!("{l}"));
                    Ok(scrub.plan)
                });
            }
            let files = devices
                .iter()
                .map(|p| File::open(p).with_context(|| p.display().to_string()))
                .collect::<Result<Vec<_>>>()?;
            let scrub = ops::scrub(&Pool::open(files)?)?;
            scrub.lines.iter().for_each(|l| println!("{l}"));
            if scrub.mismatches > 0 {
                bail!(
                    "scrubbing found {} mismatches; --repair makes them agree",
                    scrub.mismatches
                );
            }
            if scrub.unsettled > 0 {
                println!("scrub: differences only where writes were under way (Windows settles them)");
            } else {
                println!("scrub: no differences");
            }
            Ok(())
        }
        PoolCommand::Create {
            name,
            logical_sector,
            wipe,
            yes,
            devices,
        } => {
            let files = open_rw(&devices)?;
            let mut disks = Vec::new();
            let mut dirty = Vec::new();
            for (i, (path, file)) in devices.iter().zip(&files).enumerate() {
                disks.push(describe(path, file)?);
                if !is_blank(file)? {
                    dirty.push(i);
                }
            }
            if !dirty.is_empty() && !wipe {
                bail!(
                    "not blank (partition table, file system or pool data at the start or end): {}; \
                     --wipe overwrites them",
                    dirty
                        .iter()
                        .map(|&i| devices[i].display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let (mut plan, _) = ops::plan_create_pool(&disks, &name, logical_sector, &mut random_guids())?;
            if !dirty.is_empty() {
                // First clear what the pool's writes leave of the old start
                // and end (partition tables, file system signatures).
                let sizes: Vec<(usize, u64)> = dirty.iter().map(|&i| (i, disks[i].size)).collect();
                plan.summary
                    .push(format!("wipe the start and end of {} disks", dirty.len()));
                plan.steps.insert(0, wipe_step(&sizes));
            }
            if run::<File, File>(&plan, yes, &[], &files)? {
                let pool = Pool::open(files.iter().collect::<Vec<_>>())?;
                if !pool.warnings.is_empty() {
                    bail!("the new pool reports: {}", pool.warnings.join("; "));
                }
                println!("pool \"{}\" {}", printable(&pool.name), pool.guid);
            }
            Ok(())
        }
    }
}

pub fn disk(command: DiskCommand) -> Result<()> {
    match command {
        DiskCommand::Add {
            new,
            wipe,
            yes,
            devices,
        } => {
            let files = open_rw(&devices)?;
            let members: Vec<&File> = files.iter().collect();
            let pool = Pool::open(members.clone())?;
            let file = open_rw(std::slice::from_ref(&new))?.remove(0);
            let dirty = !is_blank(&file)?;
            if dirty && !wipe {
                bail!("{} is not blank; --wipe overwrites it", new.display());
            }
            let blank = describe(&new, &file)?;
            let mut plan = ops::plan_add_disk(&pool, &blank, &mut random_guids())?;
            drop(pool);
            // (1.2.2 took --wipe and wiped nothing: audit 2026-10-16, writers.)
            if dirty {
                plan.summary.push("wipe the start and end of the new disk".into());
                plan.steps.insert(0, wipe_step(&[(0, blank.size)]));
            }
            run(&plan, yes, &members, &[&file])?;
            Ok(())
        }
        DiskCommand::Set {
            disk,
            media,
            usage,
            yes,
            devices,
        } => {
            let media = media
                .map(|m| match m.as_str() {
                    "unspecified" => Ok(0),
                    "hdd" => Ok(1),
                    "ssd" => Ok(2),
                    other => Err(anyhow::anyhow!("media {other}: unspecified, hdd or ssd")),
                })
                .transpose()?;
            let usage = usage
                .map(|u| match u.as_str() {
                    "auto-select" => Ok(1),
                    "manual-select" => Ok(2),
                    "hot-spare" => Ok(3),
                    other => Err(anyhow::anyhow!(
                        "usage {other}: auto-select, manual-select or hot-spare"
                    )),
                })
                .transpose()?;
            on_pool(&devices, yes, |p| ops::plan_set_disk(p, disk, media, usage))
        }
        DiskCommand::Retire { disk, yes, devices } => on_pool(&devices, yes, |p| ops::plan_retire_disk(p, disk)),
        DiskCommand::Remove { disk, yes, devices } => {
            let files = open_rw(&devices)?;
            let members: Vec<&File> = files.iter().collect();
            let pool = Pool::open(members.clone())?;
            let plan = ops::plan_remove_disk(&pool, disk)?;
            let removed = pool
                .disks
                .get(&disk)
                .and_then(|d| d.member)
                .map(|m| pool.members[m].device);
            drop(pool);
            if run::<&File, File>(&plan, yes, &members, &[])? {
                let rest: Vec<&File> = members
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| Some(*i) != removed)
                    .map(|(_, f)| *f)
                    .collect();
                let pool = Pool::open(rest)?;
                for w in &pool.warnings {
                    eprintln!("warning: {w}");
                }
                if let Some(i) = removed {
                    println!("{} is no pool member any more", devices[i].display());
                }
            }
            Ok(())
        }
    }
}

pub fn space(command: SpaceCommand) -> Result<()> {
    match command {
        SpaceCommand::Delete { space, yes, devices } => on_pool(&devices, yes, |p| ops::plan_delete_space(p, &space)),
        SpaceCommand::Rename {
            space,
            name,
            yes,
            devices,
        } => on_pool(&devices, yes, |p| ops::plan_rename_space(p, &space, &name)),
        SpaceCommand::Resize {
            space,
            size,
            yes,
            devices,
        } => on_pool(&devices, yes, |p| ops::plan_resize_space(p, &space, size)),
        SpaceCommand::Create {
            name,
            resiliency,
            size,
            tier,
            thin,
            copies,
            columns,
            interleave,
            write_cache,
            yes,
            devices,
        } => {
            let files = open_rw(&devices)?;
            let members: Vec<&File> = files.iter().collect();
            let pool = Pool::open(members.clone())?;
            let (plan, new) = if tier.is_empty() {
                let resiliency = match resiliency.as_deref() {
                    Some("simple") => 1,
                    Some("mirror") => 2,
                    Some("parity") => 3,
                    other => bail!("resiliency {other:?}: simple, mirror or parity"),
                };
                let spec = SpaceSpec {
                    name,
                    resiliency,
                    size: size.unwrap_or(0),
                    thin,
                    copies,
                    columns,
                    interleave,
                    write_cache,
                };
                ops::plan_create_space(&pool, &spec, &mut random_guids())?
            } else {
                let tiers = tier
                    .iter()
                    .map(|t| {
                        let (template, size) = t
                            .split_once('=')
                            .ok_or_else(|| anyhow::anyhow!("--tier {t}: TEMPLATE=SIZE"))?;
                        Ok((template.to_owned(), parse_size(size).map_err(anyhow::Error::msg)?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                ops::plan_create_tiered_space(&pool, &name, &tiers, &mut random_guids())?
            };
            drop(pool);
            if run::<&File, File>(&plan, yes, &members, &[])? {
                let pool = Pool::open(members)?;
                if !pool.warnings.is_empty() {
                    bail!("the pool reports: {}", pool.warnings.join("; "));
                }
                println!("space \"{}\" {} ({} bytes)", printable(&new.name), new.guid, new.size);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    /// A directory removed when the test ends.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Sparse image files of 8 GiB in a new directory.
    fn images(name: &str, n: usize) -> (Scratch, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("spaces-manage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = (0..n).map(|i| dir.join(format!("disk{i}.img"))).collect();
        for p in &paths {
            File::create(p).unwrap().set_len(8 << 30).unwrap();
        }
        (Scratch(dir), paths)
    }

    fn create(name: &str, devices: &[PathBuf]) {
        pool(PoolCommand::Create {
            name: name.into(),
            logical_sector: None,
            wipe: false,
            yes: true,
            devices: devices.to_vec(),
        })
        .unwrap();
    }

    fn zero(path: &Path, offset: u64, len: usize) -> bool {
        let mut b = vec![1u8; len];
        FileExt::read_exact_at(&File::open(path).unwrap(), &mut b, offset).unwrap();
        b.iter().all(|&x| x == 0)
    }

    /// A plan is carried out on the handles it was made with: here the
    /// second disk's path names a blank file by the time the plan is
    /// carried out, which keeps nothing of it. 1.2.2 opened the paths again
    /// and wrote the second disk's copy of the database into that file
    /// (audit 2026-10-16, writers).
    #[test]
    fn plans_are_carried_out_on_the_disks_they_were_made_with() {
        let (_dir, paths) = images("handles", 2);
        create("handles", &paths);
        let moved = paths[1].with_extension("moved");
        let _ = on_pool(&paths, true, |p| {
            std::fs::rename(&paths[1], &moved).unwrap();
            File::create(&paths[1]).unwrap().set_len(8 << 30).unwrap();
            ops::plan_rename_pool(p, "renamed")
        });
        assert!(zero(&paths[1], 0, 32 << 20), "the other file was written");
        let pool = Pool::open(vec![File::open(&paths[0]).unwrap(), File::open(&moved).unwrap()]).unwrap();
        assert_eq!(pool.name, "renamed");
        assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    }

    /// `disk add --wipe` clears the start and end of a disk that is not
    /// blank, as `pool create --wipe` does: a file system's signature at
    /// 64 KiB and data in the last MiB are gone. 1.2.2 took the flag and
    /// wiped nothing (audit 2026-10-16, writers).
    #[test]
    fn disk_add_wipe_clears_the_new_disk() {
        let (_dir, mut paths) = images("wipe", 3);
        let new = paths.pop().unwrap();
        create("wipe", &paths);
        let f = File::options().write(true).open(&new).unwrap();
        f.write_all_at(b"_BHRfS_M", (64 << 10) + 0x40).unwrap();
        f.write_all_at(&[0xee; 4096], (8 << 30) - (512 << 10)).unwrap();
        drop(f);
        let refused = disk(DiskCommand::Add {
            new: new.clone(),
            wipe: false,
            yes: true,
            devices: paths.clone(),
        });
        assert!(refused.unwrap_err().to_string().contains("not blank"));
        disk(DiskCommand::Add {
            new: new.clone(),
            wipe: true,
            yes: true,
            devices: paths.clone(),
        })
        .unwrap();
        assert!(zero(&new, 64 << 10, 4096), "the signature at 64 KiB stayed");
        assert!(zero(&new, (8 << 30) - (512 << 10), 4096), "the data at the end stayed");
        paths.push(new);
        let pool = Pool::open(paths.iter().map(|p| File::open(p).unwrap()).collect()).unwrap();
        assert_eq!(pool.members.len(), 3);
        assert!(pool.warnings.is_empty(), "{:?}", pool.warnings);
    }

    /// Whether a disk is a block device follows from what the file is, not
    /// from its path: an image under /dev (here /dev/shm) is an image. 1.2.2
    /// took every path under /dev for a block device, and a block device
    /// named by a path elsewhere for an image of 512-byte sectors (audit
    /// 2026-10-16, writers).
    #[test]
    fn block_devices_are_told_by_what_they_are() {
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            eprintln!("no /dev/shm: skipped");
            return;
        }
        let path = shm.join(format!("spaces-describe-{}", std::process::id()));
        let file = File::create(&path).unwrap();
        file.set_len(8 << 30).unwrap();
        let d = describe(&path, &file);
        let _ = std::fs::remove_file(&path);
        let d = d.unwrap();
        assert_eq!(
            (d.model.as_str(), d.logical_sector, d.physical_sector),
            ("Image", 512, 4096)
        );
    }

    /// GUIDs come without a panic when the kernel's random bytes cannot be
    /// read, and still differ. 1.2.2 panicked when /dev/urandom could not
    /// be read (the old binary under a bind mount of /dev/null there), and
    /// without the file made the same GUID twice within 100 ns (audit
    /// 2026-10-16, writers).
    #[test]
    fn guids_differ_without_random_bytes() {
        for source in [File::open("/dev/null").ok(), None, File::open("/dev/urandom").ok()] {
            let mut guid = guids_from(source);
            let all: std::collections::BTreeSet<[u8; 16]> = (0..10_000).map(|_| guid().0).collect();
            assert_eq!(all.len(), 10_000);
            assert!(all.iter().all(|g| g[6] >> 4 == 4 && g[8] >> 6 == 2));
        }
    }
}
