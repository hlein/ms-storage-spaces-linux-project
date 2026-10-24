#!/bin/bash
# Crash safety of `spaces attach` on a Linux test VM (by default the Ubuntu
# 26.10 one, which this project has to itself): the spaces binary given is
# copied there and run as root on loop devices of test pools, one line
# PASS, FAIL or SKIP per check:
#   steps     for each step an attach can be cut off after (state, props,
#             server, dm, partition, partitions: SPACES_ATTACH_STOP_AFTER,
#             in builds with the feature test-hooks only), on wc4k (served by
#             ublk) and on a simple space made here with one partition
#             (device-mapper): the attach exits 99, the next one attaches
#             the space with its partitions, and detach leaves no device,
#             unit or state of it
#   kill      the same with attach killed (SIGKILL) as soon as the space's
#             device appears, which needs no test build (1.2.2: the next
#             attach failed, the server of the killed one still running)
#   names     two spaces of one pool named "Фото" and "Игры" attach side by
#             side under two names, and detaching one leaves the other
#             (1.2.2 gave both one name, and the failed attach of the second
#             removed the first one's device)
#   leftovers a device-mapper device with the UUID prefix of spaces and no
#             state is named by status and removed by detach --leftovers
#   busy      detach refuses, removing nothing, while a device of the space
#             is held open by a process, mounted in another mount namespace,
#             used as swap, or set up as a loop device (which only the
#             kernel's count of openers shows), and detaches once released
#             (1.2.2 removed the devices it could and stopped at the busy
#             one)
#   children  one attach of two pools, the checks of the second made to
#             allocate without bound, then to hang (SPACES_TEST_HOG_POOL,
#             SPACES_TEST_HANG_POOL; test builds only): the first pool's
#             space is attached, the second pool refused with the limit it
#             reached (SPACES_POOL_MEMORY, SPACES_POOL_TIMEOUT), in time
#             (1.2.2 checked every pool in one process: such a pool took
#             the whole run with it)
#   budget    the same with the second pool's checks hanging under
#             SPACES_ATTACH_TIMEOUT=40 alone, as the attach units run: the
#             first pool's space is attached, in time (1.3.0 gave the hung
#             checks all the time left, and the first pool was not attached:
#             "the time of this run is up")
#   onepool   attach --pool of the first pool beside a pool that cannot be
#             assembled (one of its two disks given): the first pool's space
#             is attached and the run succeeds, the other pool named
#   detachname two spaces named "Storage space" and "my-data" attached:
#             detach "Storage space" detaches that one and leaves the other
#             (1.3.0 found none: it matched the name against the end of the
#             device-mapper name, where the space is "_")
#   relative  attach of wc4k (served by ublk) through the image's path
#             relative to the directory it runs in: attached (1.3.0's server,
#             a unit started in /, found no such file)
#   wholedisk a pool on an image set up with its partitions (loop -P): served
#             read-only through its partition, a management write through the
#             whole disk is refused, and served through the whole disk, one
#             through the partition (1.3.0 locked the path given alone: both
#             went ahead, the database written under the server)
#   holders   the space attached (device-mapper) through the partition: a
#             management write through the whole disk names the device-mapper
#             device that holds the partition (1.3.0: "the kernel")
#   promote   a state of a cut-off attach that lists no devices and whose
#             device is held open (as a teardown that removed the devices and
#             failed later leaves it): the next attach does not take it for
#             attached (1.3.0 did, 0 devices of 0: the space was "already
#             attached" with nothing attached)
# Build: cargo build --release --target x86_64-unknown-linux-musl -p spaces-cli --features test-hooks
# Usage: [LINUX_VM_HOST=user@address] tools/attach-crash-check.sh SPACES [CASE...]
# Pools: wc4k in ~/sstest/pools on the VM (tools/guard-check.sh copies it
# there); pools made here go to ~/sstest/scratch/attach. Only the spaces of
# these pools are attached and detached, by their GUIDs.
set -euo pipefail
cd "$(dirname "$0")/.."
binary=${1:?usage: tools/attach-crash-check.sh SPACES [CASE...]}
shift
cases=${*:-steps kill names leftovers busy children budget onepool detachname relative promote wholedisk holders}
host=${LINUX_VM_HOST:-claude@192.168.189.143}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }
vm "mkdir -p ~/sstest/attach-check && test -s ~/sstest/pools/wc4k/disk0.img" ||
  { echo "no ~/sstest/pools/wc4k on the VM (tools/guard-check.sh copies it)" >&2; exit 2; }
scp -q "${ssh_opts[@]}" "$binary" "$host:sstest/attach-check/spaces"
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
spaces=~/sstest/attach-check/spaces
chmod +x "$spaces"
scratch=~/sstest/scratch/attach
guids=() loops=()
leftover=ss-attach-check-leftover
newloop() { # newloop IMAGE [losetup options]: LOOP = a loop device of IMAGE
  LOOP=$(sudo losetup -f --show "$@")
  loops+=("$LOOP")
}
guid() { # the GUID of space $1 in the pool of the devices that follow
  local name=$1
  shift
  { sudo "$spaces" check --json "$@" 2>/dev/null || true; } | tr -d '\n ' |
    sed -n "s/.*\"space\":{\"name\":\"$name\",\"guid\":\"\([0-9a-f-]*\)\".*/\1/p"
}
left() { # what is left of space $1 (GUID): devices with its UUID, its unit, its state
  local g=$1 out=
  if sudo dmsetup info -c --noheadings -o uuid 2>/dev/null | grep -qi -- "-$g"; then out+=" devices"; fi
  if systemctl is-active --quiet "storage-spaces-$g"; then out+=" unit"; fi
  if [[ -e /run/storage-spaces/$g.state ]]; then out+=" state"; fi
  printf '%s' "$out"
}
cleanup() {
  # Best effort: what is not there need not go.
  set +e
  trap - ERR
  local g
  # What the busy case may have left holding a device.
  [[ -s $scratch/holder.pid ]] && sudo kill -9 "$(cat "$scratch/holder.pid")" 2>/dev/null
  sudo swapoff /dev/mapper/ss-ss_crash-crash-p1 2>/dev/null
  [[ -n ${busy_loop:-} ]] && sudo losetup -d "$busy_loop" 2>/dev/null
  for g in "${guids[@]}"; do
    sudo "$spaces" detach "$g" >/dev/null 2>&1 || true
    sudo systemctl stop "storage-spaces-$g" >/dev/null 2>&1 || true
    for d in $(sudo dmsetup info -c --noheadings -o name,uuid --separator ' ' 2>/dev/null |
      awk -v g="$g" 'tolower($2) ~ g {print $1}' | sort -r); do
      sudo dmsetup remove --retry "$d" >/dev/null 2>&1 || true
    done
    sudo rm -f "/run/storage-spaces/$g.state"
  done
  # Devices without a UUID (1.2.2's) by the names of these pools.
  for d in $(ls /dev/mapper | grep -E '^ss-ss_(wc4k|crash|names|names2|hog|part|whole)-' | sort -r); do
    sudo dmsetup remove --retry "$d" >/dev/null 2>&1 || true
  done
  sudo dmsetup remove "$leftover" >/dev/null 2>&1 || true
  if [[ ${fwupd:-no} == yes ]]; then sudo systemctl unmask --runtime fwupd.service >/dev/null 2>&1; fwupd=no; fi
  for l in "${loops[@]}"; do sudo losetup -d "$l" 2>/dev/null || true; done
  loops=()
  # As before (it also runs before the cases, for what a cut-off run left).
  set -e
  trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
}
trap cleanup EXIT
rm -rf "${scratch:?}"
mkdir -p "$scratch"
# fwupd probes block devices as they appear and holds one open for a
# moment, which makes a detach refuse ("in use ... (fwupd)", seen on the
# 26.10 VM): it is kept from running for this run, and let run again after.
fwupd=no
if systemctl list-unit-files fwupd.service >/dev/null 2>&1; then
  sudo systemctl mask --runtime --now fwupd.service >/dev/null 2>&1 && fwupd=yes
fi

# The pools: wc4k (a write-back cache: ublk), and a simple space with one
# partition (device-mapper).
cp --sparse=always ~/sstest/pools/wc4k/disk0.img "$scratch/wc4k.img"
truncate -s 3G "$scratch/crash.img"
"$spaces" pool create --name ss_crash --yes "$scratch/crash.img" >/dev/null &&
  "$spaces" space create --name crash --resiliency simple --size 1G --yes "$scratch/crash.img" >/dev/null ||
  { echo "FAIL cannot make the pool ss_crash"; exit 1; }
newloop "$scratch/crash.img"
crash_loop=$LOOP
crash=$(guid crash "$crash_loop")
guids+=("$crash")
sudo "$spaces" attach --rw --space "$crash" "$crash_loop" >/dev/null 2>&1 &&
  sudo sgdisk -o -n 1:2048:+50M -t 1:8200 -n 2:0:+100M -t 2:8300 /dev/mapper/ss-ss_crash-crash >/dev/null 2>&1 &&
  sudo "$spaces" detach "$crash" >/dev/null 2>&1 &&
  sudo "$spaces" attach --rw --space "$crash" "$crash_loop" >/dev/null 2>&1 &&
  sudo mkswap /dev/mapper/ss-ss_crash-crash-p1 >/dev/null 2>&1 &&
  sudo mkfs.ext4 -q /dev/mapper/ss-ss_crash-crash-p2 >/dev/null 2>&1 &&
  sudo "$spaces" detach "$crash" >/dev/null 2>&1 ||
  { echo "FAIL cannot give ss_crash its partitions"; exit 1; }
newloop "$scratch/wc4k.img" -r
wc4k_loop=$LOOP
wc4k=$(guid wc4k "$wc4k_loop")
guids+=("$wc4k")

# kind: wc4k or crash -> its GUID, loop device, device-mapper name, partitions.
space_of() {
  case $1 in
  wc4k) g=$wc4k loop=$wc4k_loop name=ss-ss_wc4k-wc4k parts="p1 p2" ;;
  crash) g=$crash loop=$crash_loop name=ss-ss_crash-crash parts="p1 p2" ;;
  esac
}
# Attaches the space again, checks its devices, detaches it, and checks that
# nothing of it is left: "" when all went well, else what did not.
recover() {
  local out rc missing= p
  rc=0
  out=$(sudo "$spaces" attach --space "$g" "$loop" 2>&1) || rc=$?
  [[ $rc == 0 ]] || { printf 'attach again: exit %s: %s' "$rc" "$(one "$out")"; return; }
  [[ -e /dev/mapper/$name ]] || missing+=" $name"
  for p in $parts; do [[ -e /dev/mapper/$name-$p ]] || missing+=" $name-$p"; done
  [[ -z $missing ]] || { printf 'attached without%s: %s' "$missing" "$(one "$out")"; return; }
  rc=0
  out=$(sudo "$spaces" detach "$g" 2>&1) || rc=$?
  [[ $rc == 0 ]] || { printf 'detach: exit %s: %s' "$rc" "$(one "$out")"; return; }
  local l
  l=$(left "$g")
  [[ -z $l ]] || printf 'left after detach:%s' "$l"
}

# holdon PIDFILE COMMAND...: COMMAND as root in the background, its pid
# in PIDFILE once it runs.
holdon() {
  local pidfile=$1
  shift
  rm -f "$pidfile"
  sudo bash -c 'echo $$ > "$1"; shift; exec "$@"' _ "$pidfile" "$@" >/dev/null 2>&1 &
  for _ in $(seq 100); do [[ -s $pidfile ]] && break; sleep 0.05; done
  sleep 0.5
}
release() { sudo kill -9 "$(cat "$1")" 2>/dev/null; wait 2>/dev/null; }
# refused CASE WORDS: detach of space $g fails saying WORDS, and every device
# and the state of the space are still there.
refused() {
  local out rc missing= p
  rc=0
  out=$(sudo "$spaces" detach "$g" 2>&1) || rc=$?
  [[ -e /dev/mapper/$name ]] || missing+=" $name"
  for p in $parts; do [[ -e /dev/mapper/$name-$p ]] || missing+=" $name-$p"; done
  [[ -e /run/storage-spaces/$g.state ]] || missing+=" state"
  if [[ $rc != 0 && $out == *"$2"* && -z $missing ]]; then
    result ok "$1" "$(one "$out" | sed 's/.*in use: //' | cut -c1-110)"
  else
    result fail "$1" "exit $rc, gone:${missing:- nothing}: $(one "$out")"
  fi
}
# detached CASE: detach of space $g now succeeds and leaves nothing.
detached() {
  local out rc l
  rc=0
  out=$(sudo "$spaces" detach "$g" 2>&1) || rc=$?
  l=$(left "$g")
  if [[ $rc == 0 && -z $l ]]; then result ok "$1"; else result fail "$1" "exit $rc, left:$l: $(one "$out")"; fi
}

make_hog() { # the pool ss_hog (a simple space "hog"), its loop device hog_loop
  [[ -n ${hog_loop:-} ]] && return 0
  truncate -s 3G "$scratch/hog.img"
  "$spaces" pool create --name ss_hog --yes "$scratch/hog.img" >/dev/null 2>&1 &&
    "$spaces" space create --name hog --resiliency simple --size 1G --yes "$scratch/hog.img" >/dev/null 2>&1 || return 1
  newloop "$scratch/hog.img" -r
  hog_loop=$LOOP
  hog=$(guid hog "$hog_loop")
  guids+=("$hog")
}
for c in $CASES; do
  case $c in
  busy)
    pidfile=$scratch/holder.pid
    space_of wc4k
    sudo "$spaces" attach --space "$g" "$loop" >/dev/null 2>&1
    holdon "$pidfile" bash -c 'exec 3<"$0"; exec sleep 600' "/dev/mapper/$name"
    refused "busy fd" "is open in pid"
    release "$pidfile"
    detached "busy fd released"
    space_of crash
    sudo "$spaces" attach --rw --space "$g" "$loop" >/dev/null 2>&1
    sudo mkdir -p /mnt/attach-check
    holdon "$pidfile" unshare -m sh -c "mount -o ro /dev/mapper/$name-p2 /mnt/attach-check && exec sleep 600"
    refused "busy mount namespace" "in the mount namespace of pid"
    release "$pidfile"
    detached "busy mount namespace released"
    sudo "$spaces" attach --rw --space "$g" "$loop" >/dev/null 2>&1
    sudo swapon "/dev/mapper/$name-p1"
    refused "busy swap" "is used as swap"
    sudo swapoff "/dev/mapper/$name-p1"
    detached "busy swap released"
    sudo "$spaces" attach --rw --space "$g" "$loop" >/dev/null 2>&1
    busy_loop=$(sudo losetup -f --show -r "/dev/mapper/$name-p2")
    refused "busy loop" "is open"
    sudo losetup -d "$busy_loop"
    busy_loop=
    detached "busy loop released" ;;
  steps)
    rc=0
    out=$(sudo env SPACES_ATTACH_STOP_AFTER=state "$spaces" attach --space "$crash" "$crash_loop" 2>&1) || rc=$?
    if [[ $rc != 99 ]]; then
      sudo "$spaces" detach "$crash" >/dev/null 2>&1 || true
      result skip steps "this build has no test hooks (exit $rc)"
      continue
    fi
    for kind in wc4k crash; do
      space_of $kind
      for step in state props server dm partition partitions; do
        [[ $kind == crash && $step == server ]] && continue
        rc=0
        out=$(sudo env SPACES_ATTACH_STOP_AFTER=$step "$spaces" attach --space "$g" "$loop" 2>&1) || rc=$?
        if [[ $rc != 99 ]]; then
          result fail "steps $kind $step" "the attach did not stop there (exit $rc): $(one "$out")"
          sudo "$spaces" detach "$g" >/dev/null 2>&1 || true
          continue
        fi
        st=$("$spaces" status 2>&1) || true
        why=$(recover)
        if [[ -z $why && $st == *"cut off"* ]]; then
          result ok "steps $kind $step"
        else
          result fail "steps $kind $step" "${why:-status did not say: $(one "$st")}"
        fi
      done
    done ;;
  kill)
    for kind in wc4k crash; do
      space_of $kind
      pidfile=$scratch/attach.pid
      rm -f "$pidfile"
      sudo bash -c 'echo $$ > "$1"; exec "$2" attach --space "$3" "$4" >/dev/null 2>&1' _ "$pidfile" "$spaces" "$g" "$loop" &
      killed=
      for _ in $(seq 3000); do
        if [[ -e /dev/mapper/$name && -s $pidfile ]]; then
          sudo kill -9 "$(cat "$pidfile")" 2>/dev/null && killed=1
          break
        fi
        sleep 0.01
      done
      wait 2>/dev/null || true
      if [[ -z $killed ]]; then
        result fail "kill $kind" "no device appeared to kill the attach at"
        sudo "$spaces" detach "$g" >/dev/null 2>&1 || true
        continue
      fi
      why=$(recover)
      if [[ -z $why ]]; then result ok "kill $kind"; else result fail "kill $kind" "$why"; fi
      sudo "$spaces" detach "$g" >/dev/null 2>&1 || true
    done ;;
  names)
    truncate -s 4G "$scratch/names.img"
    if ! "$spaces" pool create --name ss_names --yes "$scratch/names.img" >/dev/null 2>&1; then
      result fail names "cannot make the pool"
      continue
    fi
    for n in Фото Игры; do
      "$spaces" space create --name "$n" --resiliency simple --size 1G --yes "$scratch/names.img" >/dev/null
    done
    newloop "$scratch/names.img" -r
    names_loop=$LOOP
    photo=$(guid Фото "$names_loop") games=$(guid Игры "$names_loop")
    guids+=("$photo" "$games")
    sudo "$spaces" attach --space "$photo" "$names_loop" >/dev/null 2>&1
    rc=0
    out=$(sudo "$spaces" attach --space "$games" "$names_loop" 2>&1) || rc=$?
    a=$(sudo sed -n 's/^dm=//p' "/run/storage-spaces/$photo.state" 2>/dev/null | head -1) || true
    b=$(sudo sed -n 's/^dm=//p' "/run/storage-spaces/$games.state" 2>/dev/null | head -1) || true
    if [[ $rc == 0 && -n $a && -n $b && $a != "$b" && -e /dev/mapper/$a && -e /dev/mapper/$b ]]; then
      result ok "names attach" "$a $b"
    else
      result fail "names attach" "exit $rc: $(one "$out"); devices: ${a:-none} ${b:-none}, $(ls /dev/mapper | grep ss-ss_names | tr '\n' ' ')"
    fi
    sudo "$spaces" detach "$games" >/dev/null 2>&1 || true
    if [[ -n $a && -e /dev/mapper/$a && -z $(left "$games") ]]; then
      result ok "names detach" "$a stays"
    else
      result fail "names detach" "left of the second:$(left "$games"); first: ${a:-none}"
    fi
    sudo "$spaces" detach "$photo" >/dev/null 2>&1 ;;
  leftovers)
    zeros=00000000-0000-0000-0000-000000000000
    echo "0 8 zero" | sudo dmsetup create "$leftover" --uuid "STORAGESPACES-$zeros-$zeros" >/dev/null
    st=$(sudo "$spaces" status 2>&1) || true
    rc=0
    out=$(sudo "$spaces" detach --leftovers 2>&1) || rc=$?
    if [[ $st == *"/dev/mapper/$leftover"* && $rc == 0 && ! -e /dev/mapper/$leftover ]]; then
      result ok leftovers "named and removed"
    else
      result fail leftovers "status: $(one "$st"); detach --leftovers: exit $rc: $(one "$out")"
    fi ;;
  children)
    if ! make_hog; then
      result fail children "cannot make the pool ss_hog"
      continue
    fi
    for hook in "SPACES_TEST_HOG_POOL SPACES_POOL_MEMORY=512 512 MiB" "SPACES_TEST_HANG_POOL SPACES_POOL_TIMEOUT=5 5 s"; do
      read -r var limit want <<<"$hook"
      start=$(date +%s)
      rc=0
      out=$(sudo env "$var=ss_hog" "$limit" "$spaces" attach "$crash_loop" "$hog_loop" 2>&1) || rc=$?
      took=$(($(date +%s) - start))
      sudo "$spaces" detach "$crash" >/dev/null 2>&1 || true
      sudo "$spaces" detach "$hog" >/dev/null 2>&1 || true
      if [[ $out != *"$want"* && $out == *'attached "hog"'* ]]; then
        result skip "children ${var#SPACES_TEST_}" "this build has no test hooks"
      elif [[ $rc != 0 && $out == *'attached "crash"'* && $out != *'attached "hog"'* && $out == *"$want"* && $took -lt 60 ]]; then
        result ok "children ${var#SPACES_TEST_}" "${took} s"
      else
        result fail "children ${var#SPACES_TEST_}" "exit $rc after ${took} s: $(one "$out")"
      fi
    done ;;
  budget)
    if ! make_hog; then
      result fail budget "cannot make the pool ss_hog"
      continue
    fi
    start=$(date +%s)
    rc=0
    out=$(sudo env SPACES_TEST_HANG_POOL=ss_hog SPACES_ATTACH_TIMEOUT=40 "$spaces" attach "$crash_loop" "$hog_loop" 2>&1) || rc=$?
    took=$(($(date +%s) - start))
    sudo "$spaces" detach "$crash" >/dev/null 2>&1 || true
    sudo "$spaces" detach "$hog" >/dev/null 2>&1 || true
    if [[ $out == *'attached "hog"'* ]]; then
      result skip budget "this build has no test hooks"
    elif [[ $out == *'attached "crash"'* && $out != *'attached "hog"'* && $took -lt 45 ]]; then
      result ok budget "${took} s"
    else
      result fail budget "exit $rc after ${took} s: $(one "$out")"
    fi ;;
  onepool)
    if [[ -z ${part_loop:-} ]]; then
      truncate -s 3G "$scratch/part0.img" "$scratch/part1.img"
      if ! "$spaces" pool create --name ss_part --yes "$scratch/part0.img" "$scratch/part1.img" >/dev/null 2>&1 ||
        ! "$spaces" space create --name part --resiliency simple --size 1G --yes "$scratch/part0.img" "$scratch/part1.img" >/dev/null 2>&1; then
        result fail onepool "cannot make the pool ss_part"
        continue
      fi
      newloop "$scratch/part0.img" -r
      part_loop=$LOOP
    fi
    rc=0
    out=$(sudo "$spaces" attach --pool ss_crash "$crash_loop" "$part_loop" 2>&1) || rc=$?
    sudo "$spaces" detach "$crash" >/dev/null 2>&1 || true
    if [[ $rc == 0 && $out == *'attached "crash"'* ]]; then
      result ok onepool "$(one "$out" | cut -c1-160)"
    else
      result fail onepool "exit $rc: $(one "$out")"
    fi ;;
  detachname)
    truncate -s 4G "$scratch/names2.img"
    if ! "$spaces" pool create --name ss_names2 --yes "$scratch/names2.img" >/dev/null 2>&1; then
      result fail detachname "cannot make the pool"
      continue
    fi
    for n in "Storage space" my-data; do
      "$spaces" space create --name "$n" --resiliency simple --size 1G --yes "$scratch/names2.img" >/dev/null
    done
    newloop "$scratch/names2.img" -r
    names2_loop=$LOOP
    # guid takes the JSON without its blanks: the name as it is there.
    sp=$(guid Storagespace "$names2_loop") md=$(guid my-data "$names2_loop")
    if [[ -z $sp || -z $md ]]; then
      result fail detachname "no GUIDs of the spaces (${sp:-none}, ${md:-none})"
      continue
    fi
    guids+=("$sp" "$md")
    sudo "$spaces" attach --space "$sp" "$names2_loop" >/dev/null 2>&1 || true
    sudo "$spaces" attach --space "$md" "$names2_loop" >/dev/null 2>&1 || true
    rc=0
    out=$(sudo "$spaces" detach "Storage space" 2>&1) || rc=$?
    if [[ $rc == 0 && -z $(left "$sp") && -n $(left "$md") ]]; then
      result ok detachname "$(one "$out" | cut -c1-120)"
    else
      result fail detachname "exit $rc: $(one "$out"); left of it:$(left "$sp"); of my-data:$(left "$md")"
    fi
    sudo "$spaces" detach "$sp" >/dev/null 2>&1 || true
    sudo "$spaces" detach "$md" >/dev/null 2>&1 || true ;;
  relative)
    rc=0
    out=$(cd "$scratch" && sudo "$spaces" attach --space "$wc4k" wc4k.img 2>&1) || rc=$?
    if [[ $rc == 0 && -e /dev/mapper/ss-ss_wc4k-wc4k ]]; then
      result ok relative "$(one "$out" | cut -c1-120)"
    else
      result fail relative "exit $rc: $(one "$out")"
    fi
    sudo "$spaces" detach "$wc4k" >/dev/null 2>&1 || true ;;
  promote)
    fake=00000000-0000-4000-8000-0000000000ab
    truncate -s 64M "$scratch/held.img"
    newloop "$scratch/held.img"
    held_loop=$LOOP
    sudo bash -c 'sleep 600 < "$1" & echo $! > "$2"' _ "$held_loop" "$scratch/holder.pid"
    printf 'space=%s\npool=00000000-0000-4000-8000-0000000000aa\nbackend=fuse\nstatus=attaching\ndevice=%s\n' "$fake" "$held_loop" |
      sudo tee "/run/storage-spaces/$fake.state" >/dev/null
    out=$(sudo "$spaces" attach --space "$fake" "$crash_loop" 2>&1) || true
    st=$(sudo sed -n 's/^status=//p' "/run/storage-spaces/$fake.state" 2>/dev/null) || true
    sudo kill "$(cat "$scratch/holder.pid")" 2>/dev/null || true
    sudo rm -f -- "/run/storage-spaces/$fake.state"
    if [[ $st == attaching ]]; then
      result ok promote "$(one "$out" | cut -c1-140)"
    else
      result fail promote "status ${st:-none} after: $(one "$out")"
    fi ;;
  wholedisk | holders)
    # A partitioned loop device shows the pool partition to udev: where the
    # package's rule is installed (the 22.04 VM's, linuxreflect's install),
    # that starts the machine's attach unit, which attaches every pool there
    # is, not only these (seen 2026-10-24). Not there.
    rule=$(grep -l -r storage-spaces-attach /etc/udev/rules.d /lib/udev/rules.d /usr/lib/udev/rules.d 2>/dev/null | head -1) || true
    if [[ -n $rule ]]; then
      result skip "$c" "udev attaches pools here ($rule): a partitioned loop device would attach the machine's pools"
      continue
    fi
    if [[ -z ${whole_loop:-} ]]; then
      truncate -s 4G "$scratch/whole.img"
      if ! "$spaces" pool create --name ss_whole --yes "$scratch/whole.img" >/dev/null 2>&1 ||
        ! "$spaces" space create --name whole --resiliency simple --size 1G --yes "$scratch/whole.img" >/dev/null 2>&1; then
        result fail "$c" "cannot make the pool ss_whole"
        continue
      fi
      newloop "$scratch/whole.img" -P
      whole_loop=$LOOP
      sudo udevadm settle
      # The pool partition (Storage Spaces' type).
      whole_part=/dev/$(lsblk -nro NAME,PARTTYPE "$whole_loop" | awk 'tolower($2) ~ /e75caf8f/ {print $1; exit}')
      whole=$(guid whole "$whole_part")
      guids+=("$whole")
    fi
    if [[ $whole_part == /dev/ || -z $whole ]]; then
      result fail "$c" "no pool partition on $whole_loop"
      continue
    fi
    if [[ $c == wholedisk ]]; then
      for pair in "$whole_part $whole_loop" "$whole_loop $whole_part"; do
        read -r served written <<<"$pair"
        sudo bash -c 'exec "$1" serve-nbd --space "$2" --socket "$3" "$4" > /dev/null 2>&1 & echo $! > "$5"' _ \
          "$spaces" "$whole" "$scratch/whole.sock" "$served" "$scratch/server.pid"
        for _ in $(seq 50); do [[ -S $scratch/whole.sock ]] && break; sleep 0.1; done
        rc=0
        out=$(sudo "$spaces" space create --name more --resiliency simple --size 64M --yes "$written" 2>&1) || rc=$?
        sudo kill "$(cat "$scratch/server.pid")" 2>/dev/null || true
        for _ in $(seq 50); do [[ -S $scratch/whole.sock ]] || break; sleep 0.1; done
        sudo rm -f -- "$scratch/whole.sock"
        if [[ $rc != 0 && $out == *"in use by"* ]]; then
          result ok "wholedisk ${served#/dev/} served" "$(one "$out" | cut -c1-120)"
        else
          result fail "wholedisk ${served#/dev/} served" "exit $rc: $(one "$out")"
          sudo "$spaces" space delete --space more --yes "$whole_part" >/dev/null 2>&1 || true
        fi
      done
    else
      sudo "$spaces" attach --space "$whole" "$whole_part" >/dev/null 2>&1 || true
      rc=0
      out=$(sudo "$spaces" space create --name more --resiliency simple --size 64M --yes "$whole_loop" 2>&1) || rc=$?
      sudo "$spaces" detach "$whole" >/dev/null 2>&1 || true
      if [[ $rc != 0 && $out == *"device-mapper ss-ss_whole-whole"* ]]; then
        result ok holders "$(one "$out" | cut -c1-140)"
      else
        result fail holders "exit $rc: $(one "$out")"
      fi
    fi ;;
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed ($("$spaces" --version), kernel $(uname -r), systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
((fail == 0))
REMOTE
