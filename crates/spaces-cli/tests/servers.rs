//! Servers of several spaces of one pool at once: read-only servers share
//! the pool's members, and what writes them (a `--rw` server, management)
//! refuses while they are served, naming who serves them. In 1.2.2 every
//! server held the members exclusively, so a pool's second space could not
//! be served (audit 2026-10-16, O1).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

/// A server process, killed when dropped.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Three image files with a pool holding a simple space "one" (3 GiB: its
/// three columns of a 1 GiB allocation unit) and a mirror space "two" of
/// 1 GiB.
fn pool(dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    let disks: Vec<String> = (0..3)
        .map(|i| {
            let p = dir.join(format!("disk{i}.img"));
            File::create(&p).unwrap().set_len(6 << 30).unwrap();
            p.display().to_string()
        })
        .collect();
    let mut args = vec!["pool", "create", "--name", "served", "--yes"];
    args.extend(disks.iter().map(String::as_str));
    let o = spaces(&args);
    assert!(o.status.success(), "{}", text(&o));
    for (name, resiliency) in [("one", "simple"), ("two", "mirror")] {
        let mut args = vec![
            "space",
            "create",
            "--name",
            name,
            "--resiliency",
            resiliency,
            "--size",
            "1G",
            "--yes",
        ];
        args.extend(disks.iter().map(String::as_str));
        let o = spaces(&args);
        assert!(o.status.success(), "{}", text(&o));
    }
    disks
}

/// Starts `spaces serve-nbd` for `space` on `disks` and waits until it
/// listens; its output if it ends first.
fn serve(dir: &Path, disks: &[String], space: &str) -> Result<Server, String> {
    let socket = dir.join(format!("{space}.sock"));
    let ready = dir.join(format!("{space}.ready"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_spaces"));
    cmd.arg("serve-nbd")
        .args(disks)
        .args(["--space", space, "--socket"])
        .arg(&socket)
        .arg("--ready-file")
        .arg(&ready)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut server = Server(cmd.spawn().unwrap());
    let start = Instant::now();
    while !ready.exists() {
        if server.0.try_wait().unwrap().is_some() {
            let mut err = String::new();
            if let Some(mut s) = server.0.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut s, &mut err);
            }
            return Err(err);
        }
        assert!(start.elapsed() < Duration::from_secs(60), "{space}: no ready file");
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(server)
}

#[test]
fn the_spaces_of_a_pool_are_served_at_once_and_writers_wait() {
    let dir = std::env::temp_dir().join(format!("spaces-servers-{}", std::process::id()));
    let _scratch = Scratch(dir.clone());
    let disks = pool(&dir);
    let one = serve(&dir, &disks, "one").unwrap();
    // A second space of the same pool, served beside the first.
    let two = serve(&dir, &disks, "two").unwrap_or_else(|e| panic!("the second server failed: {e}"));
    // A writer of the pool refuses while it is served, and names the
    // servers.
    let sock = dir.join("rw.sock");
    let mut args = vec!["serve-nbd"];
    args.extend(disks.iter().map(String::as_str));
    args.extend(["--space", "one", "--rw", "--socket", sock.to_str().unwrap()]);
    let o = spaces(&args);
    assert!(!o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("in use") && t.contains("serve-nbd"), "{t}");
    let mut args = vec![
        "space",
        "create",
        "--name",
        "three",
        "--resiliency",
        "mirror",
        "--size",
        "1G",
        "--yes",
    ];
    args.extend(disks.iter().map(String::as_str));
    let o = spaces(&args);
    assert!(!o.status.success(), "{}", text(&o));
    let t = text(&o);
    assert!(t.contains("in use") && t.contains("serve-nbd"), "{t}");
    // Once they stop, the pool can be changed.
    drop((one, two));
    let o = spaces(&args);
    assert!(o.status.success(), "{}", text(&o));
}
