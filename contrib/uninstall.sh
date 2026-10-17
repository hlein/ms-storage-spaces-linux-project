#!/bin/sh
# Remove what contrib/install.sh installed. Refuses while spaces its spaces
# serves (or spaces without a server) are attached: they would stay with no
# program left to detach them; detach them first.
# Usage: contrib/uninstall.sh [-n]   (run as root; -n only lists what it would do)
set -eu
run() { if [ "${dry:-}" = 1 ]; then echo "would: $*"; else "$@"; fi; }
[ "${1:-}" = -n ] && dry=1
attached=
for f in /run/storage-spaces/*.state; do
  [ -e "$f" ] || continue
  unit=$(sed -n 's/^unit=//p' "$f")
  if [ -z "$unit" ] || systemctl show -p ExecStart --value "$unit" 2>/dev/null | grep -q 'path=/usr/local/sbin/spaces '; then
    attached="$attached $(sed -n 's/^space=//p' "$f")"
  fi
done
if [ -n "$attached" ]; then
  echo "attached:$attached; detach them first (spaces detach), then run this again" >&2
  [ "${dry:-}" = 1 ] || exit 1
fi
if command -v systemctl >/dev/null 2>&1; then
  run systemctl disable storage-spaces-attach.service 2>/dev/null || true
fi
for f in \
  /usr/local/sbin/spaces \
  /usr/local/bin/refs \
  /sbin/mount.ReFS /sbin/mount.refs \
  /etc/udev/rules.d/69-storage-spaces.rules \
  /etc/systemd/system/storage-spaces-attach.service \
  /etc/modules-load.d/storage-spaces.conf \
  /usr/local/share/man/man8/spaces.8 \
  /usr/local/share/man/man1/refs.1; do
  if [ -e "$f" ] || [ -L "$f" ]; then run rm -f "$f"; fi
done
if command -v systemctl >/dev/null 2>&1; then
  run systemctl daemon-reload
  run udevadm control --reload
fi
echo "removed (the state in /run/storage-spaces is gone at the next boot; the kernel modules stay loaded)"
