#!/bin/bash
# The lifecycle of the servers (`spaces serve-*`) on a Linux test VM (by
# default the Ubuntu 26.10 one): the spaces binary given is copied there and
# run as root on wc4k (~/sstest/pools, which tools/guard-check.sh copies
# there), one line PASS, FAIL or SKIP per check:
#   nbd       serve-nbd with 24 descriptors, 40 idle connections opened and
#             closed again: the next client is served (1.2.2 stopped
#             accepting at the first EMFILE and lived on, deaf)
#   fuse      serve-fuse ends on SIGTERM and leaves no mount (1.2.2 died and
#             left a dead one, which failed the next attach)
#   ublkkill  the server of an attached space killed (SIGKILL): detach also
#             deletes the ublk device it left (1.2.2 left it DEAD)
#   ublkpanic a ublk queue that panics on a read (SPACES_UBLK_PANIC_AT, test
#             builds): the read fails, the server ends with an error and its
#             device goes (1.2.2 exited 0)
#   ublkterm  SIGTERM before the device starts (SPACES_UBLK_DELAY_START, test
#             builds): the server ends without serving (1.2.2 lost the signal
#             and served on)
#   ublkready a ready file that cannot be written (its directory missing):
#             the server ends with an error and its device goes (1.3.0 served
#             on unreported, and an attach waited for the report to its end)
# Build for the test-build cases:
#   cargo build --release --target x86_64-unknown-linux-musl -p spaces-cli --features test-hooks
# Usage: [LINUX_VM_HOST=user@address] tools/server-check.sh SPACES [CASE...]
# Only the servers and devices this script starts are stopped or removed.
set -euo pipefail
cd "$(dirname "$0")/.."
binary=${1:?usage: tools/server-check.sh SPACES [CASE...]}
shift
cases=${*:-nbd fuse ublkkill ublkpanic ublkterm ublkready}
host=${LINUX_VM_HOST:-claude@192.168.189.143}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }
vm "mkdir -p ~/sstest/server-check && test -s ~/sstest/pools/wc4k/disk0.img" ||
  { echo "no ~/sstest/pools/wc4k on the VM (tools/guard-check.sh copies it)" >&2; exit 2; }
scp -q "${ssh_opts[@]}" "$binary" "$host:sstest/server-check/spaces"
vm "CASES='$cases' bash -s" <<'REMOTE'
# A command that fails where no case expects it ends the run, naming it
# (1.2.2's remote parts went on past it).
set -Eeuo pipefail
trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
pass=0 fail=0
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1));
  elif [[ $1 == skip ]]; then echo "SKIP $2: $3";
  else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
one() { printf '%s' "${1//$'\n'/ | }"; }
spaces=~/sstest/server-check/spaces
chmod +x "$spaces"
dir=/run/server-check
img=~/sstest/pools/wc4k/disk0.img
sudo modprobe ublk_drv
sudo modprobe nbd
sudo mkdir -p "$dir" && sudo chown "$(id -u)" "$dir"
pids=() loops=() nbds=()
wc4k=$({ sudo "$spaces" check --json "$img" 2>/dev/null || true; } | tr -d '\n ' |
  sed -n 's/.*"space":{"name":"wc4k","guid":"\([0-9a-f-]*\)".*/\1/p')
cleanup() {
  # Best effort: what is not there need not go.
  set +e
  trap - ERR
  local p
  for p in "${pids[@]}"; do sudo kill -9 "$p" 2>/dev/null; done
  for n in "${nbds[@]}"; do sudo nbd-client -d "$n" >/dev/null 2>&1; done
  mountpoint -q "$dir/fuse" && sudo umount -l "$dir/fuse"
  [[ -n $wc4k ]] && sudo "$spaces" detach "$wc4k" >/dev/null 2>&1
  for l in "${loops[@]}"; do sudo losetup -d "$l" 2>/dev/null; done
  sudo rm -rf "$dir"
  # As before (it also runs before the cases, for what a cut-off run left).
  set -e
  trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
}
trap cleanup EXIT
# background PIDFILE COMMAND...: COMMAND as root, its pid in PIDFILE.
background() {
  local pidfile=$1
  shift
  sudo rm -f "$pidfile"
  sudo bash -c 'echo $$ > "$1"; shift; exec "$@"' _ "$pidfile" "$@" >/dev/null 2>>"$dir/server.log" &
  for _ in $(seq 100); do sudo test -s "$pidfile" && break; sleep 0.05; done
  pids+=("$(sudo cat "$pidfile")")
}
ready() { # ready FILE: wait up to 30 s for a ready file
  for _ in $(seq 300); do sudo test -s "$1" && return 0; sleep 0.1; done
  return 1
}
gone() { # gone PID: whether it ended within 15 s
  for _ in $(seq 150); do sudo kill -0 "$1" 2>/dev/null || return 0; sleep 0.1; done
  return 1
}

for c in $CASES; do
  case $c in
  nbd)
    sock=$dir/nbd.sock
    background "$dir/nbd.pid" prlimit --nofile=24:24 "$spaces" serve-nbd "$img" --space "$wc4k" \
      --socket "$sock" --ready-file "$dir/nbd.ready"
    if ! ready "$dir/nbd.ready"; then result fail nbd "the server did not get ready: $(one "$(sudo cat "$dir/server.log")")"; continue; fi
    held=$(sudo python3 - "$sock" <<'PY'
import socket, sys, time
held = []
for _ in range(40):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sys.argv[1])
    held.append(s)
time.sleep(2)
print(len(held))
for s in held:
    s.close()
time.sleep(2)
PY
    )
    n=$(for i in $(seq 0 15); do [[ $(cat /sys/block/nbd$i/size 2>/dev/null) == 0 && ! -e /sys/block/nbd$i/pid ]] && echo /dev/nbd$i && break; done)
    nbds+=("$n")
    rc=0
    out=$(sudo timeout 20 nbd-client -N wc4k -u "$sock" "$n" -b 4096 -R 2>&1) || rc=$?
    read=no
    [[ $rc == 0 ]] && sudo dd if="$n" of=/dev/null bs=4096 count=16 status=none 2>/dev/null && read=yes
    sudo nbd-client -d "$n" >/dev/null 2>&1 || true
    alive=no
    sudo kill -0 "${pids[-1]}" 2>/dev/null && alive=yes
    # All 40 idle connections made, or the case proves nothing (1.2.2's
    # harness passed it with none).
    if [[ $held == 40 && $rc == 0 && $read == yes && $alive == yes ]]; then
      result ok nbd "after $held idle connections: a client served"
    else
      result fail nbd "after $held idle connections: nbd-client exit $rc, read: $read, server alive: $alive: $(one "$out")"
    fi
    sudo kill "${pids[-1]}" 2>/dev/null || true ;;
  fuse)
    sudo mkdir -p "$dir/fuse"
    background "$dir/fuse.pid" "$spaces" serve-fuse "$img" --space "$wc4k" --mountpoint "$dir/fuse" \
      --ready-file "$dir/fuse.ready"
    if ! ready "$dir/fuse.ready"; then result fail fuse "the server did not get ready: $(one "$(sudo cat "$dir/server.log")")"; continue; fi
    p=${pids[-1]}
    sudo kill -TERM "$p"
    ended=no
    gone "$p" && ended=yes
    mounted=no
    grep -q " $dir/fuse " /proc/self/mountinfo && mounted=yes
    usable=no
    sudo stat "$dir/fuse" >/dev/null 2>&1 && usable=yes
    if [[ $ended == yes && $mounted == no && $usable == yes ]]; then
      result ok fuse "ended on SIGTERM, nothing left mounted"
    else
      result fail fuse "ended: $ended, still mounted: $mounted, the mount point usable: $usable"
    fi
    mountpoint -q "$dir/fuse" 2>/dev/null || grep -q " $dir/fuse " /proc/self/mountinfo && sudo umount -l "$dir/fuse" ;;
  ublkkill)
    l=$(sudo losetup -r -f --show "$img")
    loops+=("$l")
    if ! sudo "$spaces" attach --backend ublk --space "$wc4k" "$l" >/dev/null 2>&1; then result fail ublkkill "cannot attach wc4k"; continue; fi
    f=/run/storage-spaces/$wc4k.state
    unit=$(sudo sed -n 's/^unit=//p' "$f") dev=$(sudo sed -n 's/^device=//p' "$f")
    n=${dev#/dev/ublkb}
    pid=$(systemctl show -p MainPID --value "$unit")
    sudo kill -9 "$pid"
    for _ in $(seq 50); do systemctl is-active --quiet "$unit" || break; sleep 0.1; done
    left=no
    [[ -e /sys/class/ublk-char/ublkc$n ]] && left=yes
    rc=0
    out=$(sudo "$spaces" detach "$wc4k" 2>&1) || rc=$?
    after=no
    [[ -e /sys/class/ublk-char/ublkc$n ]] && after=yes
    if [[ $left == yes && $rc == 0 && $after == no ]]; then
      result ok ublkkill "ublkc$n left by the killed server, deleted by detach"
    else
      result fail ublkkill "ublkc$n after the kill: $left; detach exit $rc, still there: $after: $(one "$out")"
    fi
    sudo losetup -d "$l" 2>/dev/null ;;
  ublkpanic)
    background "$dir/panic.pid" env SPACES_UBLK_PANIC_AT=1048576 "$spaces" serve-ublk "$img" --space "$wc4k" \
      --ready-file "$dir/panic.ready"
    if ! ready "$dir/panic.ready"; then result fail ublkpanic "the server did not get ready"; continue; fi
    p=${pids[-1]}
    dev=$(sudo cat "$dir/panic.ready")
    n=${dev#/dev/ublkb}
    rc_read=0
    sudo timeout 20 dd if="$dev" of=/dev/null bs=4096 skip=256 count=1 iflag=direct status=none 2>/dev/null || rc_read=$?
    ended=no
    gone "$p" && ended=yes
    hooked=yes
    sudo grep -q "SPACES_UBLK_PANIC_AT" "$dir/server.log" || hooked=no
    status=$(sudo grep -c "a ublk queue stopped on an internal error" "$dir/server.log" || true)
    after=no
    [[ -e /sys/class/ublk-char/ublkc$n ]] && after=yes
    if [[ $hooked == no ]]; then
      result skip ublkpanic "this build has no test hooks"
      sudo kill "$p" 2>/dev/null || true
    elif [[ $rc_read != 0 && $ended == yes && $status -ge 1 && $after == no ]]; then
      result ok ublkpanic "the read failed, the server ended with an error, ublkc$n deleted"
    else
      result fail ublkpanic "read exit $rc_read, ended: $ended, error said: $status, ublkc$n still there: $after"
    fi ;;
  ublkterm)
    background "$dir/term.pid" env SPACES_UBLK_DELAY_START=3000 "$spaces" serve-ublk "$img" --space "$wc4k" \
      --ready-file "$dir/term.ready"
    p=${pids[-1]}
    sleep 1
    # A build without the hook has started its device by now.
    if sudo test -s "$dir/term.ready"; then
      result skip ublkterm "this build has no test hooks"
      sudo kill "$p" 2>/dev/null || true
      continue
    fi
    sudo kill -TERM "$p"
    sleep 4
    served=no
    sudo test -s "$dir/term.ready" && served=yes
    ended=no
    gone "$p" && ended=yes
    if [[ $ended == yes && $served == no ]]; then
      result ok ublkterm "ended without serving"
    else
      result fail ublkterm "SIGTERM before the device started: served: $served, ended: $ended"
    fi ;;
  ublkready)
    before=$(ls /sys/class/ublk-char/ 2>/dev/null | sort)
    background "$dir/unready.pid" "$spaces" serve-ublk "$img" --space "$wc4k" \
      --ready-file "$dir/missing/ready"
    p=${pids[-1]}
    ended=no
    gone "$p" && ended=yes
    said=$(sudo grep -c "cannot report the device ready\|cannot write" "$dir/server.log" || true)
    after=$(ls /sys/class/ublk-char/ 2>/dev/null | sort)
    if [[ $ended == yes && $said -ge 1 && $after == "$before" ]]; then
      result ok ublkready "ended with an error, no device left"
    else
      result fail ublkready "ended: $ended, said: $said, devices: $(one "$after") (before: $(one "$before"))"
      sudo kill "$p" 2>/dev/null || true
    fi
    sudo kill "$p" 2>/dev/null || true ;;
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed ($("$spaces" --version), kernel $(uname -r))"
((fail == 0))
REMOTE
