//! The root binary carries no test hooks (audit 2026-10-16, M25): 1.2.2's
//! `spaces` had a hidden `write-pattern` that wrote member devices without
//! a plan, `--yes` or the guard, and stopped management plans after the
//! step SPACES_STOP_AFTER_STEP named, as a crash would. The pattern writer
//! is `spaces-pattern` now, which the packages do not install, and the
//! step hook is in builds with the feature test-hooks only.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use storage_spaces::Pool;

fn spaces(args: &[&str], env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spaces"))
        .args(args)
        .envs(env.iter().copied())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

/// A scratch directory of three blank 4 GiB images, removed when dropped.
struct Scratch(PathBuf, Vec<String>);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let disks = (0..3)
            .map(|i| {
                let p = dir.join(format!("disk{i}.img"));
                File::create(&p).unwrap().set_len(4 << 30).unwrap();
                p.display().to_string()
            })
            .collect();
        Scratch(dir, disks)
    }

    fn args<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut all = args.to_vec();
        all.extend(self.1.iter().map(String::as_str));
        all
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pool_with_space(s: &Scratch, env: &[(&str, &str)]) {
    let o = spaces(&s.args(&["pool", "create", "--name", "hooks", "--yes"]), env);
    assert!(o.status.success(), "{}", text(&o));
    let o = spaces(
        &s.args(&[
            "space",
            "create",
            "--name",
            "tp",
            "--resiliency",
            "simple",
            "--size",
            "1G",
            "--yes",
        ]),
        &[],
    );
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn the_root_binary_writes_no_pattern() {
    let s = Scratch::new("hooks-pattern");
    pool_with_space(&s, &[]);
    let o = spaces(
        &s.args(&[
            "write-pattern",
            "--space",
            "tp",
            "--offset",
            "0",
            "--length",
            "4096",
            "--tag",
            "hooks",
        ]),
        &[],
    );
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("unrecognized subcommand"), "{}", text(&o));
    // The space as it was made: zeros where the pattern would be.
    let pool = Pool::open(s.1.iter().map(|d| File::open(d).unwrap()).collect()).unwrap();
    let r = pool.open_space(pool.find_space("tp").unwrap().id()).unwrap();
    let mut b = vec![0u8; 4096];
    r.read_exact_at(&mut b, 0).unwrap();
    assert!(b.iter().all(|&x| x == 0), "the space was written");
}

#[cfg(not(feature = "test-hooks"))]
#[test]
fn the_root_binary_ignores_the_step_hook() {
    let s = Scratch::new("hooks-step");
    // The whole plan carried out: the space is made on the pool.
    pool_with_space(&s, &[("SPACES_STOP_AFTER_STEP", "1")]);
    let o = spaces(&s.args(&["info"]), &[]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("\"tp\""), "{}", text(&o));
}

#[cfg(feature = "test-hooks")]
#[test]
fn test_builds_stop_after_the_step_named() {
    let s = Scratch::new("hooks-step");
    let o = spaces(
        &s.args(&["pool", "create", "--name", "hooks", "--yes"]),
        &[("SPACES_STOP_AFTER_STEP", "1")],
    );
    assert_eq!(o.status.code(), Some(99), "{}", text(&o));
    assert!(text(&o).contains("stopped after step 1"), "{}", text(&o));
}
