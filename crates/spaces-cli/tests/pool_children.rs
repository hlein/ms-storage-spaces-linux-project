//! Each pool is checked in a child process with a memory limit and a
//! deadline (audit 2026-10-16, E9): a pool whose checks allocate without
//! bound or hang no longer takes the process, and the pools beside it,
//! with it. 1.2.2 checked every pool on threads of one process: a thread
//! that hung stayed to the end, and memory had no bound. The test hooks
//! (feature test-hooks) make the checks of the pool they name allocate or
//! hang: SPACES_TEST_HOG_POOL, SPACES_TEST_HANG_POOL.
#![cfg(feature = "test-hooks")]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

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

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pool `name` of three blank 4 GiB images in `dir` with a simple space
/// of its name.
fn pool(dir: &Path, name: &str) -> Vec<String> {
    let disks: Vec<String> = (0..3)
        .map(|i| {
            let p = dir.join(format!("{name}{i}.img"));
            File::create(&p).unwrap().set_len(4 << 30).unwrap();
            p.display().to_string()
        })
        .collect();
    let with = |args: &[&str]| -> Vec<String> {
        args.iter()
            .map(|a| a.to_string())
            .chain(disks.iter().cloned())
            .collect()
    };
    let run = |args: Vec<String>| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let o = spaces(&args, &[]);
        assert!(o.status.success(), "{}", text(&o));
    };
    run(with(&["pool", "create", "--name", name, "--yes"]));
    run(with(&[
        "space",
        "create",
        "--name",
        name,
        "--resiliency",
        "simple",
        "--size",
        "1G",
        "--yes",
    ]));
    disks
}

/// Two pools; the checks of "bad" misbehave as `hook` says. Returns the
/// reports `spaces check --json` prints for both (pools given together
/// are checked each on its own), and the output.
fn check_beside(hook: &str, env: &[(&str, &str)]) -> (serde_json::Value, String, Duration) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("children-{hook}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _scratch = Scratch(dir.clone());
    let mut disks = pool(&dir, "good");
    disks.extend(pool(&dir, "bad"));
    let mut args = vec!["check", "--json"];
    args.extend(disks.iter().map(String::as_str));
    let mut all = vec![(hook, "bad")];
    all.extend(env);
    let start = Instant::now();
    let o = spaces(&args, &all);
    let took = start.elapsed();
    let out = text(&o);
    let reports = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
    (reports, out, took)
}

/// The verdicts by space name (or the subject of a report on no space),
/// with the summary of `space.checks` where it failed.
fn verdicts(reports: &serde_json::Value) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = reports
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let name = r["space"]["name"]
                .as_str()
                .or(r["subject"].as_str())
                .unwrap()
                .to_owned();
            let failed = r["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == "space.checks")
                .map(|c| c["summary"].as_str().unwrap().to_owned())
                .unwrap_or_default();
            (name, r["verdict"].as_str().unwrap().to_owned(), failed)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn a_pool_whose_checks_allocate_without_bound_fails_alone() {
    let (reports, out, _) = check_beside("SPACES_TEST_HOG_POOL", &[("SPACES_POOL_MEMORY", "512")]);
    let v = verdicts(&reports);
    assert_eq!(v.len(), 2, "{out}");
    assert_eq!((v[0].0.as_str(), v[0].1.as_str()), ("good", "healthy"), "{out}");
    assert_eq!(v[1].1, "failed", "{out}");
    assert!(v[1].0.contains("bad0.img"), "{out}");
    assert!(v[1].2.contains("512 MiB"), "{out}");
}

#[test]
fn a_pool_whose_checks_hang_fails_alone_in_time() {
    let (reports, out, took) = check_beside("SPACES_TEST_HANG_POOL", &[("SPACES_POOL_TIMEOUT", "5")]);
    let v = verdicts(&reports);
    assert_eq!(v.len(), 2, "{out}");
    assert_eq!((v[0].0.as_str(), v[0].1.as_str()), ("good", "healthy"), "{out}");
    assert_eq!(v[1].1, "failed", "{out}");
    assert!(v[1].2.contains("5 s"), "{out}");
    assert!(took < Duration::from_secs(60), "{took:?}");
}
