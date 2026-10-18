//! `spaces export` writes a new file that only its owner may read (it holds
//! the space's data), whole or not at all: 1.2.2 created it with the
//! umask's mode and left what it had written when a write failed, without
//! saying where (audit 2026-10-16, Low "spaces CLI").

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use storage_spaces::testpattern;

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

/// `args`, then the disks.
fn with<'a>(disks: &'a [String], args: &[&'a str]) -> Vec<&'a str> {
    let mut all = args.to_vec();
    all.extend(disks.iter().map(String::as_str));
    all
}

/// Three image files with a pool holding a simple space "data" of 3 GiB,
/// its first 4 MiB the pattern.
fn pool(dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    let disks: Vec<String> = (0..3)
        .map(|i| {
            let p = dir.join(format!("disk{i}.img"));
            File::create(&p).unwrap().set_len(4 << 30).unwrap();
            p.display().to_string()
        })
        .collect();
    let o = spaces(&with(&disks, &["pool", "create", "--name", "exported", "--yes"]));
    assert!(o.status.success(), "{}", text(&o));
    let o = spaces(&with(
        &disks,
        &[
            "space",
            "create",
            "--name",
            "data",
            "--resiliency",
            "simple",
            "--size",
            "3G",
            "--yes",
        ],
    ));
    assert!(o.status.success(), "{}", text(&o));
    let o = Command::new(env!("CARGO_BIN_EXE_spaces-pattern"))
        .args(with(
            &disks,
            &[
                "--space", "data", "--offset", "0", "--length", "4194304", "--tag", "data",
            ],
        ))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    disks
}

/// `spaces export` of space "data" to `output` under `sh` with `limits`
/// (shell commands) before it.
fn export(disks: &[String], output: &Path, limits: &str) -> Output {
    Command::new("sh")
        .args([
            "-c",
            &format!("{limits}; exec \"$0\" \"$@\""),
            env!("CARGO_BIN_EXE_spaces"),
        ])
        .args(["export", "--space", "data", "--output"])
        .arg(output)
        .args(disks)
        .output()
        .unwrap()
}

/// What is in `dir` beside the disks.
fn others(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("disk"))
        .collect();
    names.sort();
    names
}

#[test]
fn exports_are_private_and_whole_or_absent() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("export");
    let _ = std::fs::remove_dir_all(&dir);
    let _scratch = Scratch(dir.clone());
    let disks = pool(&dir);

    // A file larger than the process may write (writes fail with EFBIG,
    // SIGXFSZ ignored): an error naming the file, nothing left.
    let out = dir.join("small.img");
    let o = export(&disks, &out, "trap '' XFSZ; ulimit -f 2048");
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains(&out.display().to_string()), "{}", text(&o));
    assert_eq!(others(&dir), Vec::<String>::new());

    // Under a umask that lets everyone read: 0600 all the same, whole.
    let out = dir.join("data.img");
    let o = export(&disks, &out, "umask 022");
    assert!(o.status.success(), "{}", text(&o));
    let meta = std::fs::metadata(&out).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(meta.len(), 3 << 30);
    let mut head = vec![0u8; 4 << 20];
    std::os::unix::fs::FileExt::read_exact_at(&File::open(&out).unwrap(), &mut head, 0).unwrap();
    assert_eq!(testpattern::verify(&head, 0, "data"), None);
    assert_eq!(others(&dir), vec!["data.img".to_string()]);

    // An existing file is not replaced.
    let o = export(&disks, &out, "umask 022");
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("exists"), "{}", text(&o));
    assert_eq!(std::fs::metadata(&out).unwrap().len(), 3 << 30);
}
