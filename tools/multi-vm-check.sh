#!/bin/bash
# Check several spaces of one pool attached at once through the installed
# package on a Linux test VM, one line PASS or FAIL per check:
#   all      every space of multi (simple, parity and mirror, made by
#            Windows) attached together, through device-mapper and servers:
#            each reads as its test pattern, and detaching one leaves the
#            others reading (1.2.2: the second server-backed space of a pool
#            failed, "in use by another process")
#   writers  a pool made here on image files, with a simple and a mirror
#            space: while its spaces are attached, a --rw server and a
#            management command refuse and name what holds the members
#            (device-mapper, then the server of the mirror space alone);
#            once both are detached, the management command works
# Usage: [LINUX_VM_HOST=user@address] tools/multi-vm-check.sh DEB [CASE...]
# multi is copied to the VM when missing (to /srv/spaces where the VM has its
# corpus disk, as a compressed tar at 8 MB/s) and attached through read-only
# loop devices; the pool of writers lives in a scratch directory there and is
# removed afterwards.
set -euo pipefail
cd "$(dirname "$0")/.."
deb=${1:?usage: tools/multi-vm-check.sh DEB [CASE...]}
shift
cases=${*:-all writers}
host=${LINUX_VM_HOST:-192.168.189.142}
[[ $host == *@* ]] || host=codex@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then ssh_opts+=(-o HostKeyAlias=192.168.189.144); fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }
store=$(vm 'd=/srv/spaces; if [ -d $d ] && [ -w $d ]; then echo $d; else echo sstest; fi')
vm "mkdir -p ~/sstest/pools $store/pools $store/scratch"
copy() { # copy NAME: testdata/pools/NAME to the VM, unless there already
  local name=$1 tarball=target/multi-check/pools-$1.tar.gz
  vm "ls $store/pools/$name/.complete" >/dev/null 2>&1 || {
    mkdir -p target/multi-check
    [[ -s $tarball ]] || tar -C testdata/pools -cSzf "$tarball" "$name"
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
      if rsync --partial --bwlimit=8m \
        -e "ssh ${ssh_opts[*]} -o ServerAliveInterval=10 -o ServerAliveCountMax=30" \
        "$tarball" "$host:$store/pools/.$name.tar.gz"; then
        break
      fi
      echo "copying $name: attempt $attempt failed, again" >&2
      sleep 20
    done
    vm "cd $store/pools && rm -rf $name && tar -xSzf .$name.tar.gz && rm .$name.tar.gz && touch $name/.complete"
  }
  [[ $store == /* ]] && vm "ln -sfn $store/pools/$name ~/sstest/pools/$name"
  return 0
}
[[ " $cases " == *" all "* ]] && copy multi
scp -q "${ssh_opts[@]}" "$deb" "$host:sstest/multi.deb"
vm "CASES='$cases' STORE='$store' bash -s" <<'REMOTE'
# A command that fails where no case expects it ends the run, naming it
# (1.2.2's remote parts went on past it).
set -Eeuo pipefail
trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
pass=0 fail=0
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1)); else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
one() { printf '%s' "${1//$'\n'/ | }"; }
spaces=/usr/sbin/spaces
pools=~/sstest/pools scratch=$STORE/scratch/multi
loops=() wraps=()
# Loop devices without partition scanning (so that the udev rule does not
# attach them) and a linear device-mapper device over the Storage Spaces
# partition of each image: PARTS. With "ro", read-only.
members() { # members PREFIX ro|rw IMAGE...
  local prefix=$1 mode=$2 k=0 l start size
  shift 2
  PARTS=()
  for img in "$@"; do
    if [[ $mode == ro ]]; then l=$(sudo losetup -r -f --show "$img"); else l=$(sudo losetup -f --show "$img"); fi
    loops+=("$l")
    read -r start size < <(sudo sfdisk -d "$l" 2>/dev/null | awk -F'[=,]' '/type=E75CAF8F/{gsub(/ /,""); print $2, $4}')
    local ro=--readonly
    [[ $mode == rw ]] && ro=
    # shellcheck disable=SC2086
    echo "0 $size linear $l $start" | sudo dmsetup create $ro "$prefix-$k"
    wraps+=("$prefix-$k")
    PARTS+=("/dev/mapper/$prefix-$k")
    k=$((k + 1))
  done
}
state_of() { # state_of SPACE-GUID: the attach state of a space ("" when detached)
  sudo cat "/run/storage-spaces/$1.state" 2>/dev/null || true
}
dev_of() { # dev_of SPACE-GUID: the device of an attached space
  local dm
  dm=$(state_of "$1" | sed -n 's/^dm=//p' | head -1)
  [[ -n $dm ]] && echo "/dev/mapper/$dm"
}
detach_guid() { # detach_guid SPACE-GUID
  local dm
  dm=$(state_of "$1" | sed -n 's/^dm=//p' | head -1)
  if [[ -n $dm ]]; then sudo "$spaces" detach "$dm" >/dev/null 2>&1 || true; fi
  return 0
}
cleanup() {
  # Best effort: what is not there need not go.
  set +e
  trap - ERR
  for f in /run/storage-spaces/*.state; do
    [[ -e $f ]] || continue
    if sudo grep -qE '^pool=(dd2ff933-d79d-4ec4-9344-20361115a818|'"${WPOOL:-none}"')$' "$f"; then
      detach_guid "$(basename "$f" .state)"
    fi
  done
  for w in "${wraps[@]}"; do sudo dmsetup remove "$w" 2>/dev/null || true; done
  for l in "${loops[@]}"; do sudo losetup -d "$l" 2>/dev/null || true; done
  wraps=() loops=()
  # As before (it also runs before the cases, for what a cut-off run left).
  set -e
  trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
}
trap 'cleanup; sudo dpkg -r storage-spaces >/dev/null 2>&1; rm -f ~/sstest/multi.deb; rm -rf "${scratch:?}"' EXIT
sudo dpkg -i ~/sstest/multi.deb >/dev/null

for c in $CASES; do
  case $c in
  all) # every space of multi at once, then one detached
    declare -A guid=([multi]=88b8e216-1e3a-43b0-9320-5ea7824909a8 [multi-s]=95f93b20-7c9c-4d7b-981c-6df1d382ef52
      [multi-p]=0c294630-7cb9-4ba0-bde4-1060dc82948c)
    declare -A length=([multi]=1073741824 [multi-s]=3221225472 [multi-p]=2147483648)
    members mvc ro $pools/multi/disk0.img $pools/multi/disk1.img $pools/multi/disk2.img
    out=$(sudo "$spaces" attach --pool ss-multi "${PARTS[@]}" 2>&1) || true
    attached=() backends=()
    for s in multi multi-s multi-p; do
      st=$(state_of "${guid[$s]}")
      [[ -n $st ]] && attached+=("$s") && backends+=("$s:$(sed -n 's/^backend=//p' <<<"$st")")
    done
    if ((${#attached[@]} != 3)); then
      result fail all "attached ${attached[*]:-nothing}: $(one "$out")"
    else
      bad=
      for s in multi multi-s multi-p; do
        dev=$(dev_of "${guid[$s]}")
        sudo "$spaces" verify-pattern "$dev" --tag "$s" --length "${length[$s]}" >/dev/null 2>&1 &&
          sudo "$spaces" verify-pattern "$dev" --tag "$s" --length "${length[$s]}" --random 300 >/dev/null 2>&1 ||
          bad+=" $s"
      done
      if [[ -n $bad ]]; then result fail all "attached (${backends[*]}), but read otherwise:$bad"
      else result ok all "${backends[*]}"; fi
      # One detached, the others still read.
      detach_guid "${guid[multi-p]}"
      left=
      for s in multi multi-s; do
        dev=$(dev_of "${guid[$s]}")
        [[ -n $dev ]] && sudo "$spaces" verify-pattern "$dev" --tag "$s" --length "${length[$s]}" --random 300 \
          >/dev/null 2>&1 && left+=" $s"
      done
      if [[ -z $(state_of "${guid[multi-p]}") && $left == " multi multi-s" ]]; then
        result ok all-detach-one "multi-p detached, multi and multi-s read"
      else
        result fail all-detach-one "after detaching multi-p:${left:- nothing} reads"
      fi
    fi
    cleanup
    ;;
  writers) # what writes a pool refuses while its spaces are attached
    rm -rf "${scratch:?}" && mkdir -p "$scratch"
    imgs=()
    for i in 0 1 2; do truncate -s 6G "$scratch/disk$i.img"; imgs+=("$scratch/disk$i.img"); done
    "$spaces" pool create --name ss_writers --yes "${imgs[@]}" >/dev/null &&
      "$spaces" space create --name one --resiliency simple --size 1G --yes "${imgs[@]}" >/dev/null &&
      "$spaces" space create --name two --resiliency mirror --size 1G --yes "${imgs[@]}" >/dev/null || {
      result fail writers "the pool could not be made"; continue
    }
    WPOOL=$({ "$spaces" check --json "${imgs[@]}" 2>/dev/null || true; } | tr -d '\n ' | sed -n 's/.*"pool":{"name":"[^"]*","guid":"\([0-9a-f-]*\)".*/\1/p')
    members mvw rw "${imgs[@]}"
    out=$(sudo "$spaces" attach --pool ss_writers "${PARTS[@]}" 2>&1) || true
    pairs=$("$spaces" check --json "${imgs[@]}" 2>/dev/null | tr -d '\n ' | grep -o '"space":{"name":"[a-z]*","guid":"[0-9a-f-]*"' | sed 's/.*"name":"\([a-z]*\)","guid":"\([0-9a-f-]*\)"/\1=\2/') || true
    declare -A wguid=()
    for kv in $pairs; do wguid[${kv%%=*}]=${kv#*=}; done
    if [[ -z $(state_of "${wguid[one]:-x}") || -z $(state_of "${wguid[two]:-x}") ]]; then
      result fail writers "not both attached: $(one "$out")"
    else
      # (A server that starts would serve until killed.)
      rw=$(sudo timeout 30 "$spaces" serve-nbd "${PARTS[@]}" --space two --rw \
        --socket /run/storage-spaces/multi-check.sock 2>&1) && rwok=1 || rwok=0
      mg=$(sudo "$spaces" space create --name three --resiliency simple --size 1G --yes "${PARTS[@]}" 2>&1) && mgok=1 || mgok=0
      if ((rwok == 0 && mgok == 0)) && [[ $rw == *"in use by"* && $mg == *"in use by device-mapper"* ]]; then
        result ok writers-dm "$(one "$mg")"
      else
        result fail writers-dm "--rw: $(one "$rw"); management: $(one "$mg")"
      fi
      # The simple space (device-mapper) away: the mirror's server alone
      # holds the members, through its shared lock.
      detach_guid "${wguid[one]}"
      mg=$(sudo "$spaces" space create --name three --resiliency simple --size 1G --yes "${PARTS[@]}" 2>&1) && mgok=1 || mgok=0
      if ((mgok == 0)) && [[ $mg == *"in use by pid"*"serve-"*"--space ${wguid[two]}"* ]]; then
        result ok writers-server "$(one "$mg")"
      else
        result fail writers-server "management: $(one "$mg")"
      fi
      detach_guid "${wguid[two]}"
      mg=$(sudo "$spaces" space create --name three --resiliency simple --size 1G --yes "${PARTS[@]}" 2>&1) && mgok=1 || mgok=0
      if ((mgok == 1)); then result ok writers-after "the pool is changed once nothing serves it"
      else result fail writers-after "$(one "$mg")"; fi
    fi
    cleanup
    ;;
  *) result fail "$c" "no such case" ;;
  esac
done
echo "passed $pass, failed $fail"
((fail == 0))
REMOTE
