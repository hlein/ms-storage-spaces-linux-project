#!/bin/bash
# Build a Debian/Ubuntu package from a release build.
# Usage: [BIN_DIR=dir] contrib/deb/build-deb.sh [output dir]
#   (after cargo build --release -p spaces-cli -p refs-cli)
# BIN_DIR is where spaces and refs are (default target/release). A package for
# every distribution wants the static musl build: BIN_DIR=target/x86_64-unknown-linux-musl/release
# (a build on a new distribution links its glibc, which older ones lack).
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${BIN_DIR:-$root/target/release}
out=${1:-$root/target/deb}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
arch=$(dpkg --print-architecture)
stage=$(mktemp -d)
chmod 755 "$stage"
trap 'rm -rf "$stage"' EXIT

install -D -m 755 "$bin/spaces" "$stage/usr/sbin/spaces"
install -D -m 755 "$bin/refs" "$stage/usr/bin/refs"
install -D -m 755 "$root/contrib/mount.refs" "$stage/usr/sbin/mount.ReFS"
ln -sf mount.ReFS "$stage/usr/sbin/mount.refs"
install -D -m 644 "$root/contrib/udev/69-storage-spaces.rules" "$stage/usr/lib/udev/rules.d/69-storage-spaces.rules"
for u in storage-spaces-attach.service storage-spaces-attach@.service; do
  sed 's#/usr/local/sbin/spaces#/usr/sbin/spaces#' "$root/contrib/systemd/$u" > "$stage/$u"
  install -D -m 644 "$stage/$u" "$stage/usr/lib/systemd/system/$u"
  rm "$stage/$u"
done
install -D -m 644 "$root/contrib/man/spaces.8" "$stage/usr/share/man/man8/spaces.8"
gzip -9n "$stage/usr/share/man/man8/spaces.8"
install -D -m 644 "$root/contrib/man/refs.1" "$stage/usr/share/man/man1/refs.1"
gzip -9n "$stage/usr/share/man/man1/refs.1"
install -D -m 644 /dev/stdin "$stage/usr/lib/modules-load.d/storage-spaces.conf" <<<$'ublk_drv\nnbd'
install -D -m 644 "$root/LICENSE" "$stage/usr/share/doc/storage-spaces/copyright"

mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<CONTROL
Package: storage-spaces
Version: $version
Architecture: $arch
Maintainer: storage-spaces developers <noreply@example.invalid>
Depends: dmsetup, systemd, udev
Recommends: nbd-client, fuse3
Suggests: ntfs-3g
Section: admin
Homepage: https://github.com/woffko/ms-storage-spaces-linux-project
Priority: optional
Description: read, write and manage Microsoft Storage Spaces pools on Linux
 Assembles Windows 11 Storage Spaces pools from their member disks and
 exposes every virtual disk as a block device under
 /dev/mapper/ss-<pool>-<space>, attached automatically (read-only) when the
 disks appear; spaces can be attached read-write, and pools, spaces and
 disks created and changed the way Windows does. Includes refs, which reads
 ReFS volumes (also inside a space) and mounts them read-only with FUSE.
CONTROL
cat > "$stage/DEBIAN/preinst" <<'PREINST'
#!/bin/sh
set -e
# Installed again after a removal (its configuration files kept): up to 1.2
# the removal disabled the attach unit, which is then no choice of the
# admin's; the postinst enables it as on a first installation.
if [ "$1" = install ] && [ -n "$2" ] && dpkg --compare-versions "$2" lt 1.3~; then
  touch /run/storage-spaces-enable-again
fi
PREINST
cat > "$stage/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
unit=storage-spaces-attach.service
again=
if [ -e /run/storage-spaces-enable-again ]; then
  rm -f /run/storage-spaces-enable-again
  again=yes
fi
# A unit of that name in /etc is the admin's or another installation's
# (contrib/install.sh), or a mask: whether it is enabled is theirs to say.
own=yes
if [ -e "/etc/systemd/system/$unit" ] || [ -L "/etc/systemd/system/$unit" ]; then
  own=
fi
if [ "$1" = configure ]; then
  systemctl daemon-reload || true
  # Enabled on the first installation; an upgrade keeps a unit the admin
  # disabled so (deb-systemd-helper records it; a package before 1.3 did
  # not, and then what systemd says now is the choice).
  if [ -n "$own" ] && [ -x /usr/bin/deb-systemd-helper ]; then
    deb-systemd-helper unmask "$unit" >/dev/null || true
    if [ -n "$2" ] && [ -z "$again" ] && ! deb-systemd-helper --quiet debian-installed "$unit" &&
      ! systemctl --quiet is-enabled "$unit" 2>/dev/null; then
      deb-systemd-helper update-state "$unit" >/dev/null || true
    fi
    if [ -n "$again" ]; then
      # Without links, deb-systemd-helper would keep it disabled: what it
      # recorded goes, and the unit is enabled as on a first installation.
      deb-systemd-helper purge "$unit" >/dev/null || true
      deb-systemd-helper enable "$unit" >/dev/null || true
    elif deb-systemd-helper --quiet was-enabled "$unit"; then
      deb-systemd-helper enable "$unit" >/dev/null || true
    else
      deb-systemd-helper update-state "$unit" >/dev/null || true
    fi
  elif [ -n "$own" ] && { [ -z "$2" ] || [ -n "$again" ]; }; then
    systemctl enable "$unit" || true
  fi
  udevadm control --reload || true
  modprobe ublk_drv 2>/dev/null || true
  modprobe nbd 2>/dev/null || true
  # After an upgrade the spaces attached before are still served by the
  # previous program (prerm detaches only on remove): detach those nothing
  # uses, and attach them again as they were, with this one.
  if [ -n "$2" ]; then
    /usr/sbin/spaces detach --outdated || true
  fi
  # The udev rule acts on disks that appear later: attach the pools whose
  # disks are here now, unless the unit is turned off.
  if [ -n "$own" ] && [ -d /run/systemd/system ] && systemctl --quiet is-enabled "$unit" 2>/dev/null; then
    systemctl start --no-block "$unit" || true
  fi
fi
POSTINST
cat > "$stage/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = remove ]; then
  # Detach what this package's spaces serves (and spaces without a server,
  # device-mapper only); the spaces another installation's spaces serves
  # (contrib/install.sh's in /usr/local) stay attached. A space that stays
  # (in use) stops the removal: its server would run on with no program
  # left to detach it.
  busy=
  for f in /run/storage-spaces/*.state; do
    [ -e "$f" ] || continue
    unit=$(sed -n 's/^unit=//p' "$f")
    space=$(sed -n 's/^space=//p' "$f")
    if [ -z "$unit" ] || systemctl show -p ExecStart --value "$unit" 2>/dev/null | grep -q 'path=/usr/sbin/spaces '; then
      /usr/sbin/spaces detach "$space" || busy="$busy $space"
    fi
  done
  if [ -n "$busy" ]; then
    echo "storage-spaces is not removed: the spaces$busy are in use and stay attached." >&2
    echo "Stop using them (spaces status lists them), run spaces detach, then remove the package again." >&2
    exit 1
  fi
fi
PRERM
cat > "$stage/DEBIAN/postrm" <<'POSTRM'
#!/bin/sh
set -e
unit=storage-spaces-attach.service
# What the postinst enabled goes with the purge; a removed package's unit
# file is gone, its link with it on the purge.
if [ "$1" = purge ] && [ -x /usr/bin/deb-systemd-helper ]; then
  deb-systemd-helper purge "$unit" >/dev/null || true
fi
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload || true
fi
POSTRM
chmod 755 "$stage/DEBIAN/preinst" "$stage/DEBIAN/postinst" "$stage/DEBIAN/prerm" "$stage/DEBIAN/postrm"
mkdir -p "$out"
dpkg-deb --root-owner-group --build "$stage" "$out/storage-spaces_${version}_${arch}.deb" >/dev/null
echo "$out/storage-spaces_${version}_${arch}.deb"
