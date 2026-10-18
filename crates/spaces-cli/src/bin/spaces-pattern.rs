#![forbid(unsafe_code)]
//! Writes the verification pattern with a tag into a space of a pool: a
//! tool of the write tests (tools/rw-roundtrip.sh, the management scripts,
//! tools/guard-check.sh), not installed by the packages. It writes the
//! member devices without a plan, `--yes` or the health guard.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use clap::Parser;
use storage_spaces::io::{ReadAt, WriteAt};
use storage_spaces::{OpenOptions, Pool, SpaceWriter, UncleanParity, testpattern};

#[allow(dead_code)]
#[path = "../members.rs"]
mod members;

/// Write the verification pattern with a tag into a space (tests of write
/// support: the member devices are written).
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[arg(required = true)]
    devices: Vec<PathBuf>,
    /// The space by name, GUID or id.
    #[arg(short, long)]
    space: String,
    /// Byte offset (a multiple of 4096).
    #[arg(long)]
    offset: u64,
    /// Bytes to write (a multiple of 4096).
    #[arg(long)]
    length: u64,
    #[arg(long)]
    tag: String,
    /// Stop the process before the Nth write reaches a member, as a power
    /// loss would (for crash tests); without a crash, the number of member
    /// writes is printed.
    #[arg(long)]
    crash_after_writes: Option<usize>,
    /// Destage the write-back cache after the writes.
    #[arg(long)]
    destage: bool,
    /// What the writer takes where a parity stripe does not match its
    /// parity or mirror copies differ, as `spaces --unclean-parity` says.
    #[arg(long, value_enum, default_value = "refuse")]
    unclean_parity: UncleanArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum UncleanArg {
    Refuse,
    Data,
}

/// A member that ends the process before a given write, as a power loss
/// would leave the disks.
struct CrashAfter {
    file: File,
    writes: Arc<AtomicUsize>,
    limit: Option<usize>,
}

impl ReadAt for CrashAfter {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        ReadAt::read_exact_at(&self.file, buf, offset)
    }
    fn size(&self) -> std::io::Result<u64> {
        ReadAt::size(&self.file)
    }
}

impl WriteAt for CrashAfter {
    fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(limit) = self.limit
            && n >= limit
        {
            eprintln!("crash before member write {limit}");
            std::process::exit(99);
        }
        WriteAt::write_all_at(&self.file, buf, offset)
    }
    fn flush(&self) -> std::io::Result<()> {
        WriteAt::flush(&self.file)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let block = testpattern::BLOCK as u64;
    if !cli.offset.is_multiple_of(block) || !cli.length.is_multiple_of(block) {
        bail!("offset and length must be multiples of {block}");
    }
    let writes = Arc::new(AtomicUsize::new(0));
    let files = cli
        .devices
        .iter()
        .map(|p| {
            Ok(CrashAfter {
                file: members::open_member(p, true, members::Hold::Exclusive)?,
                writes: writes.clone(),
                limit: cli.crash_after_writes,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let pool = Pool::open(files)?;
    for w in &pool.warnings {
        eprintln!("warning: {w}");
    }
    let space = match pool.select_space(&cli.space)? {
        Some(s) => s,
        // A hidden space by its id (`spaces info --all` lists them).
        None => match cli.space.parse().ok().and_then(|id: u64| pool.spaces.get(&id)) {
            Some(s) => s,
            None => bail!("no space named {:?}", cli.space),
        },
    };
    let unclean_parity = match cli.unclean_parity {
        UncleanArg::Refuse => UncleanParity::Refuse,
        UncleanArg::Data => UncleanParity::PreferData,
    };
    let writer = pool.open_space_rw_with(space.id(), OpenOptions { unclean_parity })?;
    write(&writer, cli.offset, cli.length, &cli.tag, cli.destage)?;
    eprintln!("member writes: {}", writes.load(Ordering::SeqCst));
    Ok(())
}

fn write<D: WriteAt>(writer: &SpaceWriter<'_, D>, offset: u64, length: u64, tag: &str, destage: bool) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let mut at = offset;
    while at < offset + length {
        let n = (offset + length - at).min(buf.len() as u64) as usize;
        for (i, b) in buf[..n].chunks_mut(testpattern::BLOCK).enumerate() {
            testpattern::fill_block(b, at + (i * testpattern::BLOCK) as u64, tag);
        }
        writer.write_all_at(&buf[..n], at)?;
        at += n as u64;
    }
    writer.flush()?;
    if destage {
        writer.destage()?;
    }
    println!("wrote {length} bytes at {offset:#x} with tag {tag:?}");
    Ok(())
}
