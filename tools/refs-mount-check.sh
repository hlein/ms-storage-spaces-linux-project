#!/bin/bash
# Mount every ReFS corpus volume (testdata/refs/NAME, or the NAMEs given)
# with `refs mount` and compare what Linux sees with Windows' listing
# (manifest.json): every path and nothing else, kinds, sizes, modification
# times, the SHA-256 of every file and of every named stream up to 64 KiB
# (extended attributes user.<name>; stream snapshots are not mounted),
# link targets, and one inode for all
# names of a hard-linked file. Needs FUSE; nothing is written to the images
# (mounted with --force: a volume the guard calls suspect is read too).
# Usage: tools/refs-mount-check.sh [NAME...]
#
# With --rw IMAGE: a copy of IMAGE (a ReFS 3.14 image: r314small) mounted
# with `refs mount --rw`, the semantics of writing through it, one line
# PASS or FAIL per case:
#   hold      `refs create` on the image while it is mounted is refused, as is
#             a second mount, naming the mount (1.2.2 wrote, and what it wrote
#             was lost when the mount wrote its view of the volume over it)
#   readdir   600 files (names of 120 characters, so that a listing takes
#             several calls) listed while each is unlinked as it comes: all
#             listed, none left (1.2.2's offsets counted the names left at
#             each call, and names the unlinks moved were skipped)
#   openread  a file open for reading, unlinked: it reads on (1.2.2: ENOENT)
#   openwrite a file open for writing, unlinked: writes, reads and the close
#             succeed (1.2.2: the close failed with ENOENT)
#   tmpmode   the buffer of a file open for writing: mode 0600, no name
#             (1.2.2: 0644 under the usual umask)
#   casename  a file open for writing renamed through another case of its
#             name: its writes are in the renamed file (1.3.0 lost them at
#             the close)
#   times     a file written, its times set (as cp -p does) and closed: they
#             stay what was set (1.3.0's close set the modification time)
# Usage: [REFS=binary] tools/refs-mount-check.sh --rw IMAGE [CASE...]
# REFS names the refs binary to check (built here otherwise).
set -uo pipefail
cd "$(dirname "$0")/.." || exit
refs=${REFS:-target/release/refs}
[[ -n ${REFS:-} ]] || cargo build --release -q -p refs-cli || exit 1
if [[ ${1:-} == --rw ]]; then
  image=${2:?usage: tools/refs-mount-check.sh --rw IMAGE [CASE...]}
  shift 2
  cases=${*:-hold readdir openread openwrite tmpmode casename times}
  work=$(mktemp -d)
  unmount() { fusermount3 -uz "$1" 2>/dev/null || fusermount -uz "$1" 2>/dev/null; }
  trap 'unmount "$work/mnt2"; unmount "$work/mnt"; [[ -n ${pid:-} ]] && wait "$pid"; rm -rf "$work"' EXIT
  cp --sparse=always "$image" "$work/disk.img"
  mkdir "$work/mnt"
  REFS_TMPDIR=$work "$refs" mount --rw "$work/disk.img" "$work/mnt" 2>"$work/mount.log" &
  pid=$!
  for _ in $(seq 100); do mountpoint -q "$work/mnt" && break; sleep 0.1; done
  if ! mountpoint -q "$work/mnt"; then
    echo "FAIL mount: $(tr '\n' ' ' < "$work/mount.log")"
    exit 1
  fi
  pass=0 fail=0
  result() {
    if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1)); else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
  }
  for c in $cases; do
    case $c in
    hold)
      echo "hello" > "$work/hello.txt"
      out=$("$refs" create "$work/disk.img" --path /cli.txt --from "$work/hello.txt" --yes 2>&1)
      rc=$?
      mkdir -p "$work/mnt2"
      second=$(timeout 20 "$refs" mount "$work/disk.img" "$work/mnt2" 2>&1)
      rc2=$?
      unmount "$work/mnt2"
      if [[ $rc != 0 && $out == *"in use by"* && $rc2 != 0 && $second == *"in use by"* ]]; then
        result ok hold "$(printf '%s' "$out" | tail -1)"
      else
        result fail hold "refs create exit $rc: $(printf '%s' "$out" | tail -1 | tr '\n' ' '); a second mount exit $rc2"
      fi ;;
    readdir)
      out=$(python3 - "$work/mnt" <<'EOF' 2>&1
import os, sys
d = os.path.join(sys.argv[1], "many")
os.mkdir(d)
for i in range(600):
    open(os.path.join(d, f"f{i:03}-" + "x" * 116), "w").close()
seen = 0
with os.scandir(d) as it:
    for e in it:
        os.unlink(e.path)
        seen += 1
print(f"{seen} listed, {len(os.listdir(d))} left")
EOF
      )
      if [[ $out == "600 listed, 0 left" ]]; then result ok readdir "$out"; else result fail readdir "$out"; fi ;;
    openread)
      out=$(python3 - "$work/mnt" <<'EOF' 2>&1
import os, sys
p = os.path.join(sys.argv[1], "r.bin")
data = bytes(range(256)) * 40
with open(p, "wb") as f:
    f.write(data)
f = open(p, "rb")
os.unlink(p)
got = f.read()
size = os.fstat(f.fileno()).st_size
f.close()
print("same" if got == data and size == len(data) else f"read {len(got)} bytes, fstat {size}")
EOF
      )
      if [[ $out == same ]]; then result ok openread "read on after the unlink"; else result fail openread "$(printf '%s' "$out" | tail -1)"; fi ;;
    openwrite)
      out=$(python3 - "$work/mnt" <<'EOF' 2>&1
import os, sys
p = os.path.join(sys.argv[1], "w.bin")
f = open(p, "w+b")
f.write(b"before")
f.flush()
os.unlink(p)
f.write(b" after")
f.flush()
f.seek(0)
got = f.read()
f.close()
print("same" if got == b"before after" else f"read {got!r}")
EOF
      )
      if [[ $out == same ]]; then result ok openwrite "written, read and closed after the unlink"; else result fail openwrite "$(printf '%s' "$out" | tail -1)"; fi ;;
    tmpmode)
      out=$(python3 - "$work/mnt" "$pid" <<'EOF' 2>&1
import os, stat, sys
f = open(os.path.join(sys.argv[1], "t.bin"), "wb")
f.write(b"x")
f.flush()
modes = []
for fd in os.listdir(f"/proc/{sys.argv[2]}/fd"):
    link = os.readlink(f"/proc/{sys.argv[2]}/fd/{fd}")
    if link.endswith("(deleted)"):
        st = os.stat(f"/proc/{sys.argv[2]}/fd/{fd}")
        if stat.S_ISREG(st.st_mode):
            modes.append((oct(st.st_mode & 0o777), st.st_nlink))
f.close()
seen = " ".join(f"mode {m}, {n} links" for m, n in modes) or "no buffer found"
print(("private: " if modes and all(m == "0o600" and n == 0 for m, n in modes) else "open: ") + seen)
EOF
      )
      if [[ $out == private:* ]]; then result ok tmpmode "${out#private: }"; else result fail tmpmode "${out#open: }"; fi ;;
    casename)
      out=$(python3 - "$work/mnt" <<'EOF' 2>&1
import os, sys
d = sys.argv[1]
f = open(os.path.join(d, "Case.bin"), "w+b")
f.write(b"before")
f.flush()
os.rename(os.path.join(d, "case.bin"), os.path.join(d, "moved.bin"))
f.write(b" after")
f.close()
got = open(os.path.join(d, "moved.bin"), "rb").read()
print("same" if got == b"before after" else f"read {got!r}")
EOF
      )
      if [[ $out == same ]]; then result ok casename "the writes in the renamed file"; else result fail casename "$(printf '%s' "$out" | tail -1)"; fi ;;
    times)
      out=$(python3 - "$work/mnt" <<'EOF' 2>&1
import os, sys
p = os.path.join(sys.argv[1], "copied.bin")
f = open(p, "wb")
f.write(b"copied")
f.flush()
os.utime(f.fileno(), (1_000_000_000, 1_000_000_000))
f.close()
# Through another case of the name: a lookup of its own, what the volume
# holds (the kernel keeps the attributes setattr returned for the name).
st = os.stat(os.path.join(sys.argv[1], "COPIED.bin"))
print("kept" if int(st.st_mtime) == 1_000_000_000 else f"mtime {st.st_mtime}")
EOF
      )
      if [[ $out == kept ]]; then result ok times "set before the close, kept"; else result fail times "$(printf '%s' "$out" | tail -1)"; fi ;;
    *) result fail "$c" "unknown case" ;;
    esac
  done
  echo "$pass passed, $fail failed ($("$refs" --version 2>&1 | head -1))"
  ((fail == 0))
  exit
fi
names=("$@")
given=${#names[@]}
((given)) || names=(testdata/refs/*) names=("${names[@]#testdata/refs/}")
mnt=$(mktemp -d)
trap 'fusermount -u "$mnt" 2>/dev/null; rmdir "$mnt"' EXIT
trap 'exit 130' INT TERM
failed=0 checked=0
for name in "${names[@]}"; do
  dir=testdata/refs/$name
  if [[ ! -f $dir/manifest.json ]]; then
    # A volume asked for and not there is a failure (1.2.2 skipped it).
    if ((given)); then echo "FAIL $name: no testdata/refs/$name/manifest.json"; failed=1; fi
    continue
  fi
  # A volume inside a space: the pool's disks and the space's name.
  space=$(python3 -c 'import json, sys; p = json.load(open(sys.argv[1], encoding="utf-8-sig")).get("pool"); print(p["space"] if p else "")' "$dir/manifest.json")
  if [[ -n $space ]]; then
    source=(--space "$space" "$dir"/disk[0-9]*.img)
  else
    source=("$dir/disk.img")
  fi
  if [[ ! -f ${source[-1]} ]]; then
    if ((given)); then echo "FAIL $name: no ${source[-1]}"; failed=1; fi
    continue
  fi
  checked=$((checked + 1))
  # Read as it is (the guard's verdict has tests of its own): since 1.2.1
  # two volumes of the corpus, taken with log records past their checkpoint,
  # are suspect and were not mounted, so not compared.
  "$refs" mount --force "${source[@]}" "$mnt" &
  pid=$!
  for _ in $(seq 100); do mountpoint -q "$mnt" && break; sleep 0.1; done
  python3 - "$dir/manifest.json" "$mnt" <<'EOF' || failed=1
import hashlib, json, os, sys
manifest = json.load(open(sys.argv[1], encoding="utf-8-sig"))
mnt = sys.argv[2]
problems = []
listed = {e["path"]: e for e in manifest["entries"]}
seen = set()
for root, dirs, files in os.walk(mnt):
    rel = os.path.relpath(root, mnt)
    for n in dirs + files:
        p = n if rel == "." else f"{rel}/{n}"
        if p != "System Volume Information" and not p.startswith("System Volume Information/"):
            seen.add(p)
problems += [f"{p}: not listed by Windows" for p in sorted(seen - set(listed))]
inodes = {}
for path, e in listed.items():
    full = os.path.join(mnt, path)
    if not os.path.lexists(full):
        problems.append(f"{path}: missing")
        continue
    st = os.lstat(full)
    if "link_target" in e:
        target = os.readlink(full)
        windows = e["link_target"].replace("\\", "/")
        if len(windows) > 1 and windows[1] == ":":
            windows = os.path.join(mnt, windows[3:])
        if target != windows:
            problems.append(f"{path}: link to {target!r}, Windows {e['link_target']!r}")
        continue
    if (e["kind"] == "dir") != os.path.isdir(full):
        problems.append(f"{path}: kind")
    if st.st_mtime_ns != (e["written"] - 116444736000000000) * 100:
        problems.append(f"{path}: mtime {st.st_mtime_ns}")
    if e["kind"] != "file":
        continue
    if st.st_size != e["size"]:
        problems.append(f"{path}: size {st.st_size} != {e['size']}")
    h = hashlib.sha256()
    with open(full, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    if h.hexdigest() != e["sha256"]:
        problems.append(f"{path}: data differs")
    for s in e.get("streams", []):
        # Snapshots are not in the mount; Linux takes 64 KiB attributes.
        if s["name"].endswith(":$SNAPSHOT") or s["size"] > 65536:
            continue
        try:
            value = os.getxattr(full, "user." + s["name"], follow_symlinks=False)
        except OSError as err:
            problems.append(f"{path}:{s['name']}: {err}")
            continue
        if hashlib.sha256(value).hexdigest() != s["sha256"]:
            problems.append(f"{path}:{s['name']}: data differs")
    inodes.setdefault(e.get("hard_link_of", path), set()).add(st.st_ino)
problems += [f"{p}: hard links with inodes {sorted(i)}" for p, i in inodes.items() if len(i) > 1]
print(f"{manifest['name']}: {len(listed)} entries listed by Windows, {len(seen)} mounted, {len(problems)} problems")
for p in problems[:30]:
    print("  " + p)
sys.exit(1 if problems else 0)
EOF
  fusermount -u "$mnt"
  wait "$pid"
done
# Nothing compared proves nothing.
if ((checked == 0)); then echo "FAIL: no volume was compared"; exit 1; fi
exit $failed
