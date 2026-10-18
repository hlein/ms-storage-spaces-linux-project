//! What a writer reads where a parity stripe the journal does not record
//! as consistent does not match its parity (a write cut off at a crash, or
//! 1.2.2's writer on slabs that held other data): as `--unclean-parity`
//! says, as the readers do. 1.2.2's writer ignored the option and refused,
//! so that such a space could not be written at all (audit 2026-10-16,
//! W3).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use storage_spaces::Pool;
use storage_spaces::testpattern::{BLOCK, fill_block};

fn spaces(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spaces")).args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(disks: &[String]) -> Pool<File> {
    Pool::open(disks.iter().map(|d| File::open(d).unwrap()).collect()).unwrap()
}

/// `spaces-pattern` of `length` bytes at `offset` of space "tp",
/// destaged, with `options`.
fn write(disks: &[String], options: &[&str], offset: u64, length: u64, tag: &str) -> Output {
    let (offset, length) = (offset.to_string(), length.to_string());
    let mut args = options.to_vec();
    args.extend(["--space", "tp", "--offset", &offset, "--length", &length, "--tag", tag]);
    args.push("--destage");
    args.extend(disks.iter().map(String::as_str));
    Command::new(env!("CARGO_BIN_EXE_spaces-pattern"))
        .args(&args)
        .output()
        .unwrap()
}

#[test]
fn a_writer_reads_unclean_stripes_as_it_is_told() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("unclean");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _scratch = Scratch(dir.clone());
    let disks: Vec<String> = (0..3)
        .map(|i| {
            let p = dir.join(format!("disk{i}.img"));
            File::create(&p).unwrap().set_len(4 << 30).unwrap();
            p.display().to_string()
        })
        .collect();
    let mut args = vec!["pool", "create", "--name", "unclean", "--yes"];
    args.extend(disks.iter().map(String::as_str));
    let o = spaces(&args);
    assert!(o.status.success(), "{}", text(&o));
    let mut args = vec![
        "space",
        "create",
        "--name",
        "tp",
        "--resiliency",
        "parity",
        "--size",
        "2G",
        "--thin",
        "--yes",
    ];
    args.extend(disks.iter().map(String::as_str));
    let o = spaces(&args);
    assert!(o.status.success(), "{}", text(&o));
    // One stripe written: the journal lists the first run.
    let o = write(&disks, &[], 1 << 20, 512 << 10, "first");
    assert!(o.status.success(), "{}", text(&o));

    // The parity unit of the stripe at 4 MiB, never written, overwritten.
    let at = 4u64 << 20;
    {
        let pool = open(&disks);
        let id = pool.find_space("tp").unwrap().id();
        let r = pool.open_space(id).unwrap();
        let layout = r.layout().clone();
        let loc = layout.locate(at);
        let column = layout.parity_column(layout.stripe_of(&loc));
        let (disk, slab) = layout.physical(column, 0, loc.row).unwrap();
        let (device, start) = pool.slab_location(disk, slab).unwrap().unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&disks[device]).unwrap();
        f.write_all_at(&[0xa5; 4096], start + loc.offset_in_slab).unwrap();
        f.sync_all().unwrap();
        let mut b = vec![0u8; 4096];
        assert!(
            r.read_exact_at(&mut b, at).is_err(),
            "the stripe still matches its parity"
        );
    }

    // A part of that stripe rewritten: the rest of it is read from the
    // space, and refused by default.
    let o = write(&disks, &[], at, BLOCK as u64, "second");
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("parity"), "{}", text(&o));
    // With --unclean-parity data the writer takes the data on disk, and
    // the stripe is written whole with its parity.
    let o = write(&disks, &["--unclean-parity", "data"], at, BLOCK as u64, "second");
    assert!(o.status.success(), "{}", text(&o));

    let pool = open(&disks);
    let r = pool.open_space(pool.find_space("tp").unwrap().id()).unwrap();
    let mut b = vec![0u8; BLOCK];
    r.read_exact_at(&mut b, at).unwrap();
    let mut expected = vec![0u8; BLOCK];
    fill_block(&mut expected, at, "second");
    assert!(b == expected);
    // The rest of the stripe as it was (zeros).
    r.read_exact_at(&mut b, at + BLOCK as u64).unwrap();
    assert!(b.iter().all(|&x| x == 0));
    let o = spaces(
        &[
            &["check", "--json"][..],
            &disks.iter().map(String::as_str).collect::<Vec<_>>(),
        ]
        .concat(),
    );
    assert!(o.status.success(), "{}", text(&o));
}
