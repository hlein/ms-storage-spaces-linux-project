//! Who holds a file: the processes with a lock on it (from /proc/locks),
//! named as people read them, for the refusals of `spaces` and `refs`.

use std::fs::File;
use std::path::Path;

/// The processes that hold a lock on `file` (from /proc/locks), as people
/// read them: "pid 1234 (spaces serve-nbd ..., unit ...)".
pub fn of_lock(file: &File) -> String {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(m), Ok(locks)) = (file.metadata(), std::fs::read_to_string("/proc/locks")) {
            let dev = m.dev();
            let (major, minor) = (
                ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
                (dev & 0xff) | ((dev >> 12) & !0xff),
            );
            let key = format!("{major:02x}:{minor:02x}:{}", m.ino());
            let mut pids: Vec<u32> = locks
                .lines()
                .filter_map(|line| {
                    // "1: FLOCK  ADVISORY  READ  1234 08:02:5678 0 EOF", a
                    // waiter with "->" after the number.
                    let f: Vec<&str> = line.split_whitespace().filter(|w| *w != "->").collect();
                    (f.get(1) == Some(&"FLOCK") && f.get(5) == Some(&key.as_str()))
                        .then(|| f.get(4)?.parse().ok())
                        .flatten()
                })
                .filter(|&pid| pid != std::process::id())
                .collect();
            pids.sort_unstable();
            pids.dedup();
            if !pids.is_empty() {
                return pids.iter().map(|&pid| process(pid)).collect::<Vec<_>>().join(", ");
            }
        }
    }
    let _ = file;
    "another process".into()
}

/// Process `pid` as people read it: its program, command and space, and
/// its systemd unit: "pid 1234 (spaces serve-ublk --space data, unit
/// ss-....service)".
pub fn process(pid: u32) -> String {
    let args: Vec<String> = std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let Some((program, rest)) = args.split_first() else {
        return format!("pid {pid}");
    };
    let mut what = vec![
        Path::new(program)
            .file_name()
            .map_or(program.clone(), |n| n.to_string_lossy().into_owned()),
    ];
    what.extend(rest.first().filter(|a| !a.starts_with(['-', '/'])).cloned());
    if let Some(i) = rest.iter().position(|a| a == "--space" || a == "-s") {
        what.extend(rest.get(i + 1).map(|v| format!("--space {v}")));
    }
    let unit = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|c| {
            c.lines().find_map(|l| {
                l.rsplit('/')
                    .next()
                    .filter(|u| u.ends_with(".service") || u.ends_with(".scope"))
                    .map(str::to_owned)
            })
        });
    let mut line = what.join(" ");
    if let Some(unit) = unit {
        line = format!("{line}, unit {unit}");
    }
    format!("pid {pid} ({})", crate::report::printable(&line))
}

/// A field of /proc/self/mountinfo as it was before the kernel escaped it
/// (`\040` for a space, `\134` for a backslash): the one unescaping of
/// `spaces` and `refs` (1.2.2 had two, and the one of `spaces` overflowed
/// on `\4xx`, which the kernel does not write). An escape past a byte
/// gives `?`.
pub fn unescape_mountinfo(field: &str) -> Vec<u8> {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let digits = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match digits {
            Some(d) if b[i] == b'\\' => {
                let value = d.iter().fold(0u32, |v, c| v * 8 + u32::from(c - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                i += 4;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::unescape_mountinfo;

    #[test]
    fn mountinfo_fields_are_unescaped() {
        assert_eq!(unescape_mountinfo("/run/media/a\\040b"), b"/run/media/a b");
        assert_eq!(unescape_mountinfo("a\\134b\\011\\0"), b"a\\b\t\\0");
        assert_eq!(unescape_mountinfo("\\777"), b"?");
        assert_eq!(unescape_mountinfo("/mnt/plain\\"), b"/mnt/plain\\");
    }
}
