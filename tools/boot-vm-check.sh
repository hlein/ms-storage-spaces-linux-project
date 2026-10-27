#!/bin/bash
# Check the deb package across a restart of a Linux test VM, one line PASS or
# FAIL per check:
#   cycles   the boot journal has no ordering cycle
#   early    the pool whose disks are there at boot is attached before
#            local-fs-pre.target, by the attach unit's first run
#   servers  that run waited for no server (none timed out; it took < 25 s)
#   fstab    a volume of the space listed in /etc/fstab is mounted at boot
#   runs     the attach unit ran without hitting its start limit
#   shutdown at the next restart the volume is unmounted before the server
#            of its space begins to stop (1.2.2 stopped both at once, and the
#            ublk device could go under a mounted volume)
# Usage: LINUX_VM_HOST=user@address tools/boot-vm-check.sh DEB
# The VM is restarted: use one nothing else needs (the 26.10 VM, not the
# 22.04 one that linuxreflect shares). Test pool: testdata/pools/ntfstier
# (NTFS in it), copied to /sstest-boot on the root file system (there before
# anything of local-fs) and set up at boot as read-only loop devices by a
# test unit ordered before the attach unit. Afterwards the package, the unit,
# the fstab line and the loop devices are removed; /sstest-boot stays.
set -euo pipefail
cd "$(dirname "$0")/.."
deb=${1:?usage: tools/boot-vm-check.sh DEB}
host=${LINUX_VM_HOST:-claude@192.168.189.143}
[[ $host == *@* ]] || host=claude@$host
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o IdentitiesOnly=yes -i "$HOME/.ssh/rustadmin_vm_ed25519")
if [[ $host == *@192.168.189.142 ]]; then
  echo "boot-vm-check: not on the 22.04 VM, which linuxreflect shares" >&2
  exit 2
fi
vm() { ssh "${ssh_opts[@]}" "$host" "$@"; }

# The pool's images, once (local to the VM when its ~/sstest has them).
if ! vm 'test -e /sstest-boot/ntfstier/.complete'; then
  vm 'sudo mkdir -p /sstest-boot/ntfstier && sudo chown "$(id -u):$(id -g)" /sstest-boot/ntfstier'
  if vm 'test -e ~/sstest/pools/ntfstier/disk0.img'; then
    vm 'cp --sparse=always ~/sstest/pools/ntfstier/disk*.img /sstest-boot/ntfstier/'
  else
    rsync -a --sparse --partial --bwlimit=8m -e "ssh ${ssh_opts[*]}" testdata/pools/ntfstier/ "$host:/sstest-boot/ntfstier/"
  fi
  vm 'touch /sstest-boot/ntfstier/.complete'
fi
scp -q "${ssh_opts[@]}" "$deb" "$host:/tmp/boot-check.deb"

# Before the restart: the package, the loop unit, the fstab line.
vm 'bash -s' <<'REMOTE'
set -euo pipefail
mark='# sstest boot-vm-check'
sudo sed -i "\|$mark|d" /etc/fstab
for l in $(losetup -a | grep /sstest-boot/ | cut -d: -f1); do sudo losetup -d "$l"; done
sudo dpkg -i /tmp/boot-check.deb >/dev/null
# The space's NTFS partition, from one attach now (its devices' names).
loops=()
for img in /sstest-boot/ntfstier/disk*.img; do loops+=("$(sudo losetup -r -f --show "$img")"); done
sudo /usr/sbin/spaces attach "${loops[@]}" >/dev/null
sleep 2
part=$(lsblk -nro NAME,FSTYPE /dev/mapper/ss-ss_ntfstier-ntfstier* 2>/dev/null | awk '$2 == "ntfs" {print $1; exit}')
sudo /usr/sbin/spaces detach ntfstier >/dev/null
for l in "${loops[@]}"; do sudo losetup -d "$l"; done
[[ -n $part ]] || { echo "no NTFS partition found in ntfstier" >&2; exit 1; }
# The device timeout the user guide gives for a space's volume: since 1.3.1
# the attach of this tiered pool compares its mirror tier's listed runs
# (23-24 s at boot on the 26.10 VM; 1.3.0 4 s), past the 20 s this line
# waited. The attach's time is the case servers'.
echo "/dev/mapper/$part /mnt/sstest-boot ntfs3 ro,nofail,x-systemd.device-timeout=60s 0 0 $mark" | sudo tee -a /etc/fstab >/dev/null
sudo mkdir -p /mnt/sstest-boot
sudo tee /etc/systemd/system/sstest-boot-loops.service >/dev/null <<'UNIT'
[Unit]
Description=sstest: loop devices of a test pool at boot (tools/boot-vm-check.sh)
DefaultDependencies=no
After=systemd-udevd.service
Before=storage-spaces-attach.service local-fs-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'for i in /sstest-boot/ntfstier/disk*.img; do losetup -r -f -P "$i"; done'

[Install]
WantedBy=local-fs-pre.target
UNIT
sudo systemctl daemon-reload
sudo systemctl enable sstest-boot-loops.service 2>/dev/null
echo "$part" > /var/tmp/boot-check.part
REMOTE
restart() {
  local old_boot b=
  old_boot=$(vm 'cat /proc/sys/kernel/random/boot_id')
  vm 'sudo systemctl reboot' || true
  echo "restarting $host" >&2
  for _ in $(seq 60); do
    sleep 5
    b=$(vm 'cat /proc/sys/kernel/random/boot_id' 2>/dev/null || true)
    [[ -n $b && $b != "$old_boot" ]] && break
  done
  [[ -n $b && $b != "$old_boot" ]] || { echo "the VM did not come back" >&2; exit 1; }
  vm 'systemctl is-system-running --wait >/dev/null 2>&1 || true'
}
restart

# Its status is kept (the restart and the cleanup below come all the same).
first=0
vm 'bash -s' <<'REMOTE' || first=$?
# A command that fails where no case expects it ends this part, naming it.
set -Eeuo pipefail
trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
pass=0 fail=0
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1));
  else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
part=$(cat /var/tmp/boot-check.part)
j() { sudo journalctl -b --no-pager "$@"; }
mono() { systemctl show -p "$2" --value "$1"; } # monotonic microseconds

# The boot's journal, which must be there to look in (without it 1.2.2's
# harness found no cycle and passed).
lines=$(j -o cat | wc -l)
n=$(j -o cat | grep -c 'ordering cycle' || true)
if ((lines < 100)); then result fail cycles "the boot's journal has $lines lines: nothing to look in"
elif [[ $n == 0 ]]; then result ok cycles "in $lines lines"
else result fail cycles "$n lines: $(j -o cat | grep -m1 'ordering cycle')"; fi

target=$(mono local-fs-pre.target ActiveEnterTimestampMonotonic)
# Empty when the space is not attached: "early" says so.
f=$(sudo sh -c 'grep -l "^dm=ss-ss_ntfstier-ntfstier$" /run/storage-spaces/*.state' 2>/dev/null | head -1 || true)
unit=
[[ -n $f ]] && unit=$(sudo sed -n 's/^unit=//p' "$f")
started=0
[[ -n $unit ]] && started=$(mono "$unit" ExecMainStartTimestampMonotonic)
if [[ -b /dev/mapper/ss-ss_ntfstier-ntfstier && $started -gt 0 && $started -lt $target ]]; then
  result ok early "server started at $((started / 1000)) ms, local-fs-pre.target at $((target / 1000)) ms"
else
  result fail early "server ${unit:-none} at $((started / 1000)) ms, local-fs-pre.target at $((target / 1000)) ms"
fi

first=$(j -u storage-spaces-attach.service -o short-monotonic | grep -m1 -E 'Finished|Failed|failed' || true)
waits=$(j -u storage-spaces-attach.service -o cat | grep -c -E 'unit log:|before it was ready|timed out' || true)
secs=$(sudo journalctl -b -u storage-spaces-attach.service -o json --no-pager | python3 -c '
import json, sys
start = end = None
for line in sys.stdin:
    e = json.loads(line)
    m = e.get("MESSAGE", "")
    if isinstance(m, str) and m.startswith("Starting") and start is None:
        start = int(e["__MONOTONIC_TIMESTAMP"])
    if isinstance(m, str) and (m.startswith("Finished") or "Failed" in m) and start is not None:
        end = int(e["__MONOTONIC_TIMESTAMP"]); break
print(-1 if end is None else (end - start) // 1000000)')
if [[ $waits == 0 && $secs -ge 0 && $secs -lt 25 ]]; then result ok servers "first run took ${secs} s"; else result fail servers "first run: ${secs} s, $waits waits; ${first}"; fi

if findmnt -n /mnt/sstest-boot >/dev/null && ls /mnt/sstest-boot >/dev/null; then
  result ok fstab "$(findmnt -n -o SOURCE,FSTYPE /mnt/sstest-boot)"
else
  result fail fstab "/dev/mapper/$part not mounted on /mnt/sstest-boot"
fi

runs=$(j -u storage-spaces-attach.service -o cat | grep -c '^Starting' || true)
limit=$(j -o cat | grep -c -E 'storage-spaces-attach(@[^ ]*)?\.service.*(start-limit|repeated too quickly)' || true)
if [[ $limit == 0 ]]; then result ok runs "$runs runs"; else result fail runs "$runs runs, start limit hit"; fi

# The server, for the shutdown check after the next restart.
echo "$unit" > /var/tmp/boot-check.unit
echo "$pass passed, $fail failed" > /var/tmp/boot-check.first
REMOTE
restart

vm 'bash -s' <<'REMOTE'
set -Eeuo pipefail
trap 'echo "FAIL harness: line $LINENO: $BASH_COMMAND (exit $?)"' ERR
pass=0 fail=1
# The first part's counts (none when it ended early: one failure then).
[[ -s /var/tmp/boot-check.first ]] && read -r pass _ fail _ < /var/tmp/boot-check.first
result() {
  if [[ $1 == ok ]]; then echo "PASS $2${3:+ ($3)}"; pass=$((pass + 1));
  else echo "FAIL $2: $3"; fail=$((fail + 1)); fi
}
unit=$(cat /var/tmp/boot-check.unit)
# The previous boot's shutdown: when the volume was unmounted, and when its
# server began to stop (monotonic seconds).
pj() { sudo journalctl -b -1 --no-pager -o short-monotonic "$@"; }
at() { sed -n 's/^\[ *\([0-9.]*\)\].*/\1/p' | head -1; }
# Either line may be missing (the cases below say what that means): grep
# finding nothing is no failure of the harness.
unmounted=$(pj | { grep -E 'Unmounted (mnt-sstest\\x2dboot\.mount|/mnt/sstest-boot)' || true; } | at)
stopping=$(pj | { grep -E "(Stopping|Stopped) $unit" || true; } | at)
# A server that stops after local-fs-pre.target stops after journald too:
# its stop is then not in the journal.
if [[ -z $unit ]]; then
  result fail shutdown "no server of ntfstier was recorded"
elif [[ -z $unmounted ]]; then
  result fail shutdown "the unmount of /mnt/sstest-boot is not in the journal"
elif [[ -z $stopping ]] || python3 -c "import sys; sys.exit(0 if $unmounted < $stopping else 1)"; then
  result ok shutdown "unmounted at ${unmounted} s, the server stopping ${stopping:+at $stopping s}${stopping:-after the journal ends}"
else
  result fail shutdown "unmounted at ${unmounted} s, the server ($unit) stopping at ${stopping} s"
fi

# Cleanup: what this check set up, and the package (each step best
# effort: what is not there need not go).
trap - ERR
set +e
mountpoint -q /mnt/sstest-boot && sudo umount /mnt/sstest-boot
sudo sed -i '\|# sstest boot-vm-check|d' /etc/fstab
sudo rmdir /mnt/sstest-boot 2>/dev/null
sudo systemctl disable sstest-boot-loops.service 2>/dev/null
sudo rm -f /etc/systemd/system/sstest-boot-loops.service
sudo /usr/sbin/spaces detach ntfstier >/dev/null 2>&1
for l in $(losetup -a | grep /sstest-boot/ | cut -d: -f1); do sudo losetup -d "$l"; done
sudo dpkg -r storage-spaces >/dev/null || { echo "FAIL cleanup: dpkg -r storage-spaces"; fail=$((fail + 1)); }
sudo systemctl daemon-reload
rm -f /tmp/boot-check.deb /var/tmp/boot-check.part /var/tmp/boot-check.unit /var/tmp/boot-check.first
echo "$pass passed, $fail failed (systemd $(systemctl --version | awk 'NR==1 {print $2}'))"
[[ $fail == 0 ]]
REMOTE
((first == 0)) || { echo "the checks after the first restart ended early (exit $first)" >&2; exit 1; }
