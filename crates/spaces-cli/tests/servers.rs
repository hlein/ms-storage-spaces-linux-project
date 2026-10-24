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
    serve_under(dir, disks, space, "022")
}

/// `serve` under the umask `umask`.
fn serve_under(dir: &Path, disks: &[String], space: &str, umask: &str) -> Result<Server, String> {
    serve_with(dir, disks, space, umask, &[])
}

/// `serve_under` with `extra` arguments (`--rw`).
fn serve_with(dir: &Path, disks: &[String], space: &str, umask: &str, extra: &[&str]) -> Result<Server, String> {
    let socket = dir.join(format!("{space}.sock"));
    let ready = dir.join(format!("{space}.ready"));
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!("umask {umask}; exec \"$0\" \"$@\""),
        env!("CARGO_BIN_EXE_spaces"),
    ])
    .arg("serve-nbd")
    .args(disks)
    .args(["--space", space, "--socket"])
    .arg(&socket)
    .arg("--ready-file")
    .arg(&ready)
    .args(extra)
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

/// The socket is the owner's alone whatever the umask, from the moment it
/// can be connected to: bound in a directory of its own (0700), made 0600,
/// then moved into place, the directory gone (1.2.2 made it 0600 after
/// `bind`, which listens at once). A file at the path stays, refused.
#[test]
fn sockets_are_the_owners_alone_whatever_the_umask() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("spaces-sockets-{}", std::process::id()));
    let _scratch = Scratch(dir.clone());
    let disks = pool(&dir);
    let one = serve_under(&dir, &disks, "one", "000").unwrap();
    let mode = std::fs::metadata(dir.join("one.sock")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    drop(one);
    std::fs::write(dir.join("two.sock"), b"a file").unwrap();
    let e = serve(&dir, &disks, "two").map(|_| ()).unwrap_err();
    assert!(e.contains("cannot listen on"), "{e}");
    assert_eq!(std::fs::read(dir.join("two.sock")).unwrap(), b"a file");
}

/// A connection to an NBD server at `socket`, past the handshake (the
/// default export).
fn nbd_connect(socket: &Path) -> std::os::unix::net::UnixStream {
    use std::io::{Read, Write};
    let mut c = std::os::unix::net::UnixStream::connect(socket).unwrap();
    let mut hello = [0u8; 18];
    c.read_exact(&mut hello).unwrap();
    c.write_all(&2u32.to_be_bytes()).unwrap(); // no zeroes
    c.write_all(&0x4948_4156_454f_5054u64.to_be_bytes()).unwrap(); // IHAVEOPT
    c.write_all(&1u32.to_be_bytes()).unwrap(); // OPT_EXPORT_NAME
    c.write_all(&0u32.to_be_bytes()).unwrap(); // the default export
    let mut export = [0u8; 10];
    c.read_exact(&mut export).unwrap();
    c
}

/// SIGTERM to a `--rw` server while a write is under way (its request and
/// half its data sent): the write ends and is answered, what it wrote is on
/// the disks, the server exits 0 and its socket is gone. 1.3.0 flushed and
/// exited at the signal, under the write (its reply never came), and left
/// its socket (audit 2026-10-21, SCB-01).
#[test]
fn a_server_stopped_during_a_write_ends_it_flushes_and_removes_its_socket() {
    use std::io::{Read, Write};
    let dir = std::env::temp_dir().join(format!("spaces-shutdown-{}", std::process::id()));
    let _scratch = Scratch(dir.clone());
    let disks = pool(&dir);
    let mut server = serve_with(&dir, &disks, "one", "022", &["--rw"]).unwrap();
    let socket = dir.join("one.sock");
    let mut c = nbd_connect(&socket);
    let data: Vec<u8> = (0..1u32 << 20).map(|i| (i % 251) as u8).collect();
    let mut req = 0x2560_9513u32.to_be_bytes().to_vec();
    req.extend_from_slice(&0u16.to_be_bytes());
    req.extend_from_slice(&1u16.to_be_bytes()); // CMD_WRITE
    req.extend_from_slice(&7u64.to_be_bytes());
    req.extend_from_slice(&(4u64 << 20).to_be_bytes());
    req.extend_from_slice(&(data.len() as u32).to_be_bytes());
    c.write_all(&req).unwrap();
    c.write_all(&data[..data.len() / 2]).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(unsafe { libc::kill(server.0.id() as i32, libc::SIGTERM) }, 0);
    std::thread::sleep(Duration::from_millis(300));
    let sent = c.write_all(&data[data.len() / 2..]);
    let mut reply = [0u8; 16];
    let answered = sent.and_then(|()| c.read_exact(&mut reply));
    let status = server.0.wait().unwrap();
    assert!(answered.is_ok(), "the write was not answered: {answered:?}");
    assert_eq!(
        u32::from_be_bytes(reply[4..8].try_into().unwrap()),
        0,
        "the write failed"
    );
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "the socket stayed");
    let files: Vec<File> = disks.iter().map(|d| File::open(d).unwrap()).collect();
    let pool = storage_spaces::Pool::open(files).unwrap();
    let space = pool.find_space("one").unwrap();
    let reader = pool.open_space(space.id()).unwrap();
    let mut back = vec![0u8; data.len()];
    storage_spaces::io::ReadAt::read_exact_at(&reader, &mut back, 4 << 20).unwrap();
    assert!(back == data, "the write is not on the disks");
}
