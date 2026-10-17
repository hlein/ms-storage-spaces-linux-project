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
# Build: cargo build --release --target x86_64-unknown-linux-musl -p spaces-cli --features test-hooks
# Usage: [LINUX_VM_HOST=user@address] tools/attach-crash-check.sh SPACES [CASE...]
# Pools: wc4k in ~/sstest/pools on the VM (tools/guard-check.sh copies it
# there); pools made here go to ~/sstest/scratch/attach. Only the spaces of
# these pools are attached and detached, by their GUIDs.
set -euo pipefail
cd "$(dirname "$0")/.."
binary=${1:?usage: tools/attach-crash-check.sh SPACES [CASE...]}
shift
cases=${*:-steps kill names leftovers}
host=${LINUX_VM_HOST:-claude@192.168.189.143}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }
vm "mkdir -p ~/sstest/attach-check && test -s ~/sstest/pools/wc4k/disk0.img" ||
  { echo "no ~/sstest/pools/wc4k on the VM (tools/guard-check.sh copies it)" >&2; exit 2; }
scp -q "${ssh_opts[@]}" "$binary" "$host:sstest/attach-check/spaces"
vm "CASES='$cases' bash -s" <<'REMOTE'
set -u
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
  sudo "$spaces" check --json "$@" 2>/dev/null | tr -d '\n ' |
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
  local g
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
  for d in $(ls /dev/mapper | grep -E '^ss-ss_(wc4k|crash|names)-' | sort -r); do
    sudo dmsetup remove --retry "$d" >/dev/null 2>&1 || true
  done
  sudo dmsetup remove "$leftover" >/dev/null 2>&1 || true
  for l in "${loops[@]}"; do sudo losetup -d "$l" 2>/dev/null || true; done
  loops=()
}
trap cleanup EXIT
rm -rf "${scratch:?}"
mkdir -p "$scratch"

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
  sudo sgdisk -o -n 1:2048:+100M -t 1:0700 /dev/mapper/ss-ss_crash-crash >/dev/null 2>&1 &&
  sudo "$spaces" detach "$crash" >/dev/null 2>&1 ||
  { echo "FAIL cannot give ss_crash a partition"; exit 1; }
newloop "$scratch/wc4k.img" -r
wc4k_loop=$LOOP
wc4k=$(guid wc4k "$wc4k_loop")
guids+=("$wc4k")

# kind: wc4k or crash -> its GUID, loop device, device-mapper name, partitions.
space_of() {
  case $1 in
  wc4k) g=$wc4k loop=$wc4k_loop name=ss-ss_wc4k-wc4k parts="p1 p2" ;;
  crash) g=$crash loop=$crash_loop name=ss-ss_crash-crash parts="p1" ;;
  esac
}
# Attaches the space again, checks its devices, detaches it, and checks that
# nothing of it is left: "" when all went well, else what did not.
recover() {
  local out rc missing= p
  out=$(sudo "$spaces" attach --space "$g" "$loop" 2>&1)
  rc=$?
  [[ $rc == 0 ]] || { printf 'attach again: exit %s: %s' "$rc" "$(one "$out")"; return; }
  [[ -e /dev/mapper/$name ]] || missing+=" $name"
  for p in $parts; do [[ -e /dev/mapper/$name-$p ]] || missing+=" $name-$p"; done
  [[ -z $missing ]] || { printf 'attached without%s: %s' "$missing" "$(one "$out")"; return; }
  out=$(sudo "$spaces" detach "$g" 2>&1)
  rc=$?
  [[ $rc == 0 ]] || { printf 'detach: exit %s: %s' "$rc" "$(one "$out")"; return; }
  local l
  l=$(left "$g")
  [[ -z $l ]] || printf 'left after detach:%s' "$l"
}

for c in $CASES; do
  case $c in
  steps)
    out=$(sudo env SPACES_ATTACH_STOP_AFTER=state "$spaces" attach --space "$crash" "$crash_loop" 2>&1)
    rc=$?
    if [[ $rc != 99 ]]; then
      sudo "$spaces" detach "$crash" >/dev/null 2>&1
      result skip steps "this build has no test hooks (exit $rc)"
      continue
    fi
    for kind in wc4k crash; do
      space_of $kind
      for step in state props server dm partition partitions; do
        [[ $kind == crash && $step == server ]] && continue
        out=$(sudo env SPACES_ATTACH_STOP_AFTER=$step "$spaces" attach --space "$g" "$loop" 2>&1)
        rc=$?
        if [[ $rc != 99 ]]; then
          result fail "steps $kind $step" "the attach did not stop there (exit $rc): $(one "$out")"
          sudo "$spaces" detach "$g" >/dev/null 2>&1 || true
          continue
        fi
        st=$("$spaces" status 2>&1)
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
      wait 2>/dev/null
      if [[ -z $killed ]]; then
        result fail "kill $kind" "no device appeared to kill the attach at"
        sudo "$spaces" detach "$g" >/dev/null 2>&1
        continue
      fi
      why=$(recover)
      if [[ -z $why ]]; then result ok "kill $kind"; else result fail "kill $kind" "$why"; fi
      sudo "$spaces" detach "$g" >/dev/null 2>&1
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
    out=$(sudo "$spaces" attach --space "$games" "$names_loop" 2>&1)
    rc=$?
    a=$(sudo sed -n 's/^dm=//p' "/run/storage-spaces/$photo.state" 2>/dev/null | head -1)
    b=$(sudo sed -n 's/^dm=//p' "/run/storage-spaces/$games.state" 2>/dev/null | head -1)
    if [[ $rc == 0 && -n $a && -n $b && $a != "$b" && -e /dev/mapper/$a && -e /dev/mapper/$b ]]; then
      result ok "names attach" "$a $b"
    else
      result fail "names attach" "exit $rc: $(one "$out"); devices: ${a:-none} ${b:-none}, $(ls /dev/mapper | grep ss-ss_names | tr '\n' ' ')"
    fi
    sudo "$spaces" detach "$games" >/dev/null 2>&1
    if [[ -n $a && -e /dev/mapper/$a && -z $(left "$games") ]]; then
      result ok "names detach" "$a stays"
    else
      result fail "names detach" "left of the second:$(left "$games"); first: ${a:-none}"
    fi
    sudo "$spaces" detach "$photo" >/dev/null 2>&1 ;;
  leftovers)
    zeros=00000000-0000-0000-0000-000000000000
    echo "0 8 zero" | sudo dmsetup create "$leftover" --uuid "STORAGESPACES-$zeros-$zeros" >/dev/null
    st=$(sudo "$spaces" status 2>&1)
    out=$(sudo "$spaces" detach --leftovers 2>&1)
    rc=$?
    if [[ $st == *"/dev/mapper/$leftover"* && $rc == 0 && ! -e /dev/mapper/$leftover ]]; then
      result ok leftovers "named and removed"
    else
      result fail leftovers "status: $(one "$st"); detach --leftovers: exit $rc: $(one "$out")"
    fi ;;
  *) result fail "$c" "unknown case" ;;
  esac
done
echo "$pass passed, $fail failed ($("$spaces" --version), kernel $(uname -r), systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
((fail == 0))
REMOTE
