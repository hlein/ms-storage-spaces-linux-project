# User guide

`spaces` reads Microsoft Storage Spaces pools created by Windows 11 (24H2:
pool version 28; Insider builds: pool version 29) and exposes their virtual
disks ("spaces") as Linux block devices. They are read-only, and the pool
disks are never written, unless a space is attached read-write
(`attach --rw`, see [Writing](#writing)) or the pool is changed with
`spaces pool`, `spaces space` or `spaces disk` (see
[Managing pools](#managing-pools)).

## Supported configurations

| Configuration | Status |
|---|---|
| Simple (striped), any column count and interleave | read |
| Two-way and three-way mirror | read, survives missing or failing disks |
| Single parity | read, one missing or failing disk rebuilt from parity |
| Dual parity, 7-10 columns | read; any two missing disks rebuilt |
| Dual parity, 11+ columns (local reconstruction code) | read; any two missing disks rebuilt |
| Fixed and thin provisioning | read |
| Write-back cache (default for new spaces) | read, including data not yet moved out of the cache |
| Storage tiers, mirror-accelerated parity | read |
| 512-byte and 4 KiB logical sectors, interleave 16 KiB to 1 MiB | read |
| Pools after a crash or power loss | read; parity stripes and mirror rows left inconsistent by writes in flight are refused (see below) |
| Several spaces per pool, fragmented and extended spaces | read, every space of a pool at once (see below) |
| Member disks with 512-byte, 512e and 4Kn sectors | read |
| Pools after a disk was retired, replaced or removed | read |
| Pools created by Windows 8/10/Server | not supported |
| Writing: simple, mirror, single parity, fixed and thin (256 MiB allocation units) | `attach --rw` |
| Writing: dual parity, tiers, degraded pools | not supported (refused) |
| Creating pools (version 28) and simple, mirror and single parity spaces, fixed and thin; deleting, renaming, growing spaces | `spaces pool`, `spaces space` |
| Adding, retiring, removing and replacing disks; repair, optimize, scrub, health | `spaces disk`, `spaces pool` |
| Creating tiered spaces: an SSD mirror over an HDD simple or parity tier (mirror-accelerated parity) | `spaces tier`, `spaces space create --tier` |
| Creating dual parity spaces; new spaces in version 29 pools | not supported |

## Installing

### What it needs

* Linux with device-mapper (`dmsetup`), systemd and udev for the attach at
  boot. The backends beyond device-mapper: `ublk` (kernel 6.0 or later,
  module `ublk_drv`) for parity, caches, tiers and degraded pools; `nbd`
  (`nbd-client`, module `nbd`) where there is no ublk; FUSE (`fuse3`,
  `losetup`) as the last resort and for `refs mount`.
* To use what is inside a space: a file system driver (`ntfs3`, `ntfs-3g`,
  ...); ReFS volumes need none, `refs` reads them.
* What has been tried: Ubuntu 22.04 (kernel 6.8, systemd 249) and Ubuntu
  26.10 (kernel 7.3, systemd 261, AppArmor), x86_64, as packaged and from
  `contrib/install.sh`. Not tried: Fedora and other systems with SELinux
  enforcing (the attach unit, the servers it starts and `mount.ReFS` run
  as transient systemd units and open block devices, device-mapper and
  `/dev/ublk-control`; if a policy denies them, `ausearch -m avc -ts recent`
  shows it, and `spaces attach` run by hand from a root shell tells
  whether the policy is the cause), other architectures, WSL. WSL2's kernel
  (6.18.54.1-microsoft-standard-WSL2) has device-mapper, loop and FUSE
  built in and `nbd` as a module, but no ublk: simple and mirror spaces
  attach through device-mapper, the others through `nbd` (`nbd-client`)
  or FUSE; WSL sees a disk only once Windows passes it with `wsl --mount
  --bare` (not tried), and Windows must not have the pool attached then.
* Kernels before 6.0 have no ublk: the attach picks device-mapper, `nbd`
  or FUSE instead (`spaces attach --backend` names one). Where the kernel
  has `ublk_drv` or `nbd` as a module, `spaces attach` loads it when it
  needs it; until 1.3 the packages listed both in `modules-load.d`, and
  `systemd-modules-load.service` failed at every boot on a kernel without
  `ublk_drv`.
* To build: Rust 1.89 or later and `libclang` (the ublk bindings; on
  Debian/Ubuntu `libclang-dev`, or just the library, `libclang1-21` with
  `LIBCLANG_PATH=/usr/lib/llvm-21/lib`). `--no-default-features` builds
  without the ublk and FUSE backends and without `libclang`.

### Which way

| Way | Gives | For |
|---|---|---|
| From a checkout: `cargo build --release && sudo contrib/install.sh` | `spaces`, `refs`, `mount.ReFS`, man pages, udev rule, systemd unit | any distribution |
| Debian/Ubuntu package: `contrib/deb/build-deb.sh && sudo apt install ./target/deb/storage-spaces_*.deb` | the same, as a package | Debian, Ubuntu |
| Arch Linux: `(cd contrib/arch && makepkg -si)` | the same, as a package | Arch |
| The releases page of the repository | static `spaces` and `refs` binaries (x86_64, musl) and a `.deb`, with `SHA256SUMS` (from 1.1.0; 1.0.0 had no `refs`) and, from 1.3, `THIRD-PARTY-NOTICES.txt` (the licences of what the binaries link) | trying it out; `sha256sum -c SHA256SUMS` first (it is not signed: it shows a damaged download, not a file swapped on the page) |
| `cargo install --locked --path crates/spaces-cli --bin spaces` (and `crates/refs-cli`) | the binary only, no udev rule or unit (without `--bin spaces` also `spaces-pattern`, the write tests' pattern writer, which writes member devices without a plan, `--yes` or the guard) | development |

`contrib/install.sh` (as root; it takes the path of `spaces` if it was
built elsewhere) puts `spaces` in `/usr/local/sbin`, `refs` and its man
page in `/usr/local/bin` when it was built next to `spaces`,
`mount.ReFS` (and `mount.refs`) in `/sbin`, the udev rule and the
unit, loads `ublk_drv` and `nbd` where the kernel has them (and removes the
list of them an earlier version put in `/etc/modules-load.d`), and enables
`storage-spaces-attach.service`. `contrib/uninstall.sh` (`-n` lists what it
would do) removes it again; it refuses while spaces its `spaces` serves are
attached (they would run on with no program left to detach them): detach
them first (`spaces detach`).

`storage-spaces-attach.service` attaches the pools at boot, before
`local-fs-pre.target`, and the udev rule starts a run of its own,
`storage-spaces-attach@MAJOR:MINOR.service`, for every pool member that
appears later, so that pools are attached when their disks are plugged in
(read-only; see Hardening below); installing starts the unit once too, for
the pools whose disks are there already. A disk that comes while a run goes
on gets its run all the same (1.2.2 started the one unit again, which only
joined the run going on, and the pool of the late disk stayed detached). A
run has 80 s (`SPACES_ATTACH_TIMEOUT`) after its 30 s of waiting for udev,
within the units' start timeout of 120 s: what it does not get to, it
names, and the next run attaches it. At shutdown the servers of spaces stop
after the volumes on them are unmounted (1.2.2 stopped both at once). To attach by hand only: after
`contrib/install.sh`, remove `/etc/udev/rules.d/69-storage-spaces.rules`;
with the packages, whose rule is
`/usr/lib/udev/rules.d/69-storage-spaces.rules` (an upgrade would bring a
removed one back), override it with an empty one, `sudo ln -s /dev/null
/etc/udev/rules.d/69-storage-spaces.rules`, or mask the unit, `sudo
systemctl mask storage-spaces-attach.service`. The deb enables the unit
when it is first installed; an upgrade keeps it disabled or masked if the
admin made it so (1.2.2 enabled it again on every upgrade), and a unit of
that name in `/etc` (another installation's, or a mask) is left as it is.
Removing the package detaches the spaces its `spaces` serves; the spaces
another installation's `spaces` serves (such as `contrib/install.sh`'s in
`/usr/local`) stay attached. A space in use (a volume mounted) stops the
removal of the deb, which says so: detach it, then remove the package
again (1.2.2 removed the package and left its server running). pacman
cannot be stopped so: the Arch package warns and goes on.

Check the installation: `spaces --version`, `refs --version`,
`systemctl status storage-spaces-attach.service`, and `spaces scan` with a
pool's disks plugged in. Man pages: `man spaces`, `man refs`. Upgrading is
installing again. Spaces attached by the previous version are attached
anew with the new one, as they were attached (`spaces detach --outdated`
detaches them and attaches them again with the arguments their state
records: image files, `--backend` and the flags given; 1.2.2 left the spaces no
unit attaches, such as those attached by hand from image files, detached),
except those in use (a volume mounted), attached read-write or past their
verdict: they keep the old server, as the upgrade says, until they are
detached and attached again or the machine restarts. 1.2.0 and earlier
left all of them on the old servers.

## Attaching and mounting

```sh
sudo spaces scan                     # pools and their members
sudo spaces attach                   # every complete pool
spaces status                        # what is attached (no root needed)
lsblk /dev/mapper/ss-*
sudo mount -o ro /dev/mapper/ss-<pool>-<space>-p2 /mnt
sudo umount /mnt
sudo spaces detach                   # or: spaces detach <space>
```

`spaces scan` needs root to open the disks; without it, it says how many
it could not open. `spaces status` works for every user (the state in
`/run/storage-spaces` is readable).

`--space` takes a space's GUID, its numeric id (`spaces info` lists them)
or its name, the same for every command. A key that names several spaces
of a pool (a name two spaces share, or a name that is another space's GUID
or id) is refused, with the spaces it names; give the id of the one meant.
`--pool` takes a pool's GUID or name; a name two pools share is refused
likewise, naming them by their GUIDs. `spaces detach <space>` naming no
attached space fails, and one naming attached spaces of two pools (pools
of one name) is refused, naming them: give the GUID or device-mapper name.

`spaces detach` removes nothing of a space while anything uses one of its
devices, and names what does: a mount (in any mount namespace: a container
or a service with its own counts), another device built on it (LVM,
dm-crypt, a loop device), a swap, or a process that holds it open; what
none of these shows, the kernel's count of the device's openers still
does. 1.2.2 removed the devices it could and stopped at the busy one. A step
that fails (a device that stays, a server that does not stop) keeps the
state with what is left, and the next detach goes on from there.

`attach` checks every space first, as `spaces check` does, and attaches
only healthy spaces without being asked; a space that is not healthy is
refused with its report (see "Health checks and forced attach"), which
`spaces status` lists too. udev learns which devices belong to attached spaces (`udevadm
info` shows `SS_SPACE`, `SS_VERDICT`): desktops do not mount the devices of
a space attached with `--degraded` or `--force` by themselves, nor list
the ublk or nbd device a space is served through.

Device names: `ss-<pool>-<space>` for the whole virtual disk and
`ss-<pool>-<space>-p<N>` for partition `N` (characters other than letters,
digits, `_ . +` become `_`). Pools can share a name (Windows calls every new
pool "Storage pool"): when a space of another pool has the name already,
the first eight hex digits of the pool's GUID follow,
`ss-<pool>-<space>-<guid>` (and `-p<N>` after that); `spaces detach
<space>` finds both. Two spaces of one pool whose names give one device
name (`Фото` and `Игры` both give `____`) get the first eight hex digits of
the space's GUID after the second (1.2.2 gave both one name, failed to
attach the second and removed the first one's device). Every device
carries the device-mapper UUID `STORAGESPACES-<pool guid>-<space guid>`
(`-p<N>` for a partition, `dmsetup info -c -o name,uuid`), and only
devices with the UUID of a space are removed for it. Partition devices are created from the partitions
the checks read (`space.partitions`: the space's own GPT, from its backup
when the primary header is lost, or its MBR), so they exist even when
Windows left out the protective MBR;
none are made when the partitions overlap or leave the space, or when there
are more than 128 (the whole space is attached all the same).

An attach that is cut off (killed, a crash, a timeout) leaves nothing
unknown: `spaces attach` writes the space's state before it sets anything
up, marked attaching, with the server and the devices it will make, and
again after each step. The next `spaces attach` or `spaces detach` undoes
such an attach first (or completes it, where its devices are all there and
in use); `spaces status` lists it meanwhile. Devices with the UUID of a
space and no state (a state that was lost) are listed by `spaces status`
and removed by `sudo spaces detach --leftovers`.

For `/etc/fstab`, use the partition device and `nofail`:

```
/dev/mapper/ss-Storage_pool-Data-p2  /mnt/data  ntfs3  ro,nofail,x-systemd.device-timeout=60  0 0
```

### Backends

`--backend auto` (default) picks device-mapper for simple spaces whose
logical sector size matches the disks, and ublk otherwise. ublk keeps
mirror and parity spaces readable when a disk fails while mounted; the
device-mapper mapping reads one copy only.

Every space of a pool can be attached at once, through device-mapper and
servers side by side: a space served read-only by a process (ublk, nbd,
fuse: mirror and parity spaces, and the simple ones device-mapper does not
serve, see above and Writing) shares the pool's disks with the others,
and so does `spaces export`. What writes the disks needs them alone: `attach
--rw` through a server, and the management commands, refuse while another
space of the pool is attached and name what holds the disks (until 1.3 the
first server-backed space of a pool held its disks exclusively, and the
pool's other spaces failed to attach, "in use by another process").

| Backend | Requirement |
|---|---|
| `dm` | `dmsetup` |
| `ublk` | Linux 6.0 or newer, module `ublk_drv` |
| `nbd` | module `nbd`, `nbd-client` |
| `fuse` | FUSE, `losetup` |

## Writing

```sh
sudo spaces attach --rw --space Data          # one space, read-write
sudo mount /dev/mapper/ss-<pool>-Data-p2 /mnt
...
sudo umount /mnt && sudo spaces detach Data   # flushes everything
```

A space opens for writing only when everything about its state is
understood, and the writes keep it consistent the way Windows would, so
that Windows reads the result back. `attach --rw` refuses, with the reason:

* pools that are not clean: missing disks, copies that missed writes,
  diverging metadata (attach them to Windows first);
* degraded spaces, dual parity spaces, storage tiers and mirror-accelerated
  parity;
* write-back caches whose mirrored copies disagree after a crash.

Thin spaces allocate a row (256 MiB per column and copy) when a write first
reaches it, as Windows does: the pool database on every member records the
new slabs before the data is written. A slab is not cleared when it is
allocated (Windows does not clear it either), so the parts of a new row not
written yet read whatever the disk held there. Thin spaces with other
allocation units can be written only where they are allocated; writes
elsewhere fail with an I/O error. When the pool has no free slab left, so do
writes into rows not allocated yet. Once a write or a flush of a disk fails,
nothing more is written to the space: every later write and flush fails
with an I/O error until the space is attached again (what reached which
disk, and what its logs say, is not known after such a failure; until 1.3
the writes went on, and a later flush could report as durable what was
lost). In a parity space the stripes not
written yet hold whatever the disks held too (another space's data, where
one was deleted), and their parity need not match it. The parity journal
lists the stripes of an extent run (an allocation unit of every column:
256 MiB for thin spaces) when a write first reaches it, as Windows does, and
those never written as not consistent, which readers check; so before the
first write into a run `spaces` reads the run once and writes the parity of
every stripe that does not match. In 1.2.2 it did not, and a parity space
written on slabs a deleted space had used was suspect after its first
session and could not be written again. Writers read as
`--unclean-parity` says, as readers do.

Discards (`fstrim`, the `discard` mount option, `blkdiscard`) give the rows
of thin simple and mirror spaces back to the pool once they are discarded
whole, as Windows does with TRIM; a row given back reads as zeros. Discards
of parts of rows are remembered while the space stays attached and count
towards the whole row until it is written again. Parity and fixed spaces
do not offer discards.

How the space types are written:

* Simple: in place. `--backend auto` uses device-mapper when the space is
  fully allocated and its write-back cache holds nothing, ublk otherwise.
* Mirror: every copy; the extent run is listed in the dirty region log,
  durably, before its first write (Windows compares those runs' copies).
  Served by ublk (or NBD).
* Single parity: through the write-back cache, like Windows' own writes;
  the cache is destaged in whole stripes under the parity journal when it
  fills up, so a crash never leaves a stripe whose parity is stale while
  its data exists nowhere else. Served by ublk (or NBD).

Guarantees and risks:

* Writes are durable once flushed (`sync`, `fsync`, FUA, unmounting) or
  after a clean `spaces detach`. A crash, power loss or `kill -9` of the
  serving process loses what was not flushed, as with a disk's volatile
  cache; the pool stays consistent and Windows attaches it as healthy.
* A write-back cache holding data when a space is attached read-write is
  destaged first.
* Do not attach the same pool to Windows (or another Linux system) while it
  is attached read-write here.
* NTFS: `ntfs3` of Linux 6.8 corrupts small files that are truncated to zero
  (on any disk, not only here); ntfs-3g is not affected.
* Writing is new: keep a backup of data you care about.

## Managing pools

`spaces pool`, `spaces space` and `spaces disk` change pools the way
Windows 11 24H2 does, so that Windows takes the result as its own: pools
created and changed on Linux attach in Windows as healthy, and its Repair
and Optimize work on them. Every command works on the disks given (block
devices or image files) and:

* computes a plan and prints it; nothing is written without `--yes`;
* opens the disks exclusively: a disk that is mounted, attached (also by
  `spaces attach`) or used by another program fails with "in use by" and
  what holds it (a device-mapper device, a `spaces` server and its unit, or
  the kernel); detach the pool first;
* works on clean pools only: every disk present, no copy out of date,
  every record understood. Attach a pool that is not clean to Windows
  first, or see [Replacing a failed disk](#replacing-a-failed-disk);
* writes in steps that each end with a flush, so that after a crash or
  power loss at any point the pool opens in the state before or after a
  step; data being moved is copied before the metadata points at it.

### Creating a pool and spaces

```sh
sudo spaces pool create --name Data /dev/sdb /dev/sdc /dev/sdd            # prints the plan
sudo spaces pool create --name Data --yes /dev/sdb /dev/sdc /dev/sdd
sudo spaces space create --name Files --resiliency mirror --size 1T --yes /dev/sdb /dev/sdc /dev/sdd
sudo spaces attach
```

`pool create` takes blank disks only: a partition table, file system or
other data in the first or last MiB makes it refuse, unless `--wipe` (which
destroys everything on those disks). The disks get the layout
`New-StoragePool` gives them (a Microsoft reserved partition and the pool
partition); up to five of them carry a copy of the pool database.
`--logical-sector 4096` makes the spaces use 4 KiB sectors.

`space create` takes Windows' defaults for what is not given:

| Option | Default |
|---|---|
| `--resiliency simple` | one column per disk (up to 8) |
| `--resiliency mirror` | two copies (`--copies 3` needs five disks), disks / copies columns (up to 8) |
| `--resiliency parity` | single parity, three columns, a write-back cache of 1 GiB (`--write-cache`, at least 512 MiB) |
| `--thin` | fixed provisioning otherwise; thin spaces take 256 MiB allocation units |
| `--interleave` | 256K |

Sizes (`10G`, `500M`, `2T`) are rounded up to whole rows (fixed spaces:
1 GiB per data column). As on Windows, a new space's slabs are not cleared:
only its first sector (a parity space: its first stripe) is, so that no old
partition table shows up. Create a partition table and a file system as on
any disk, through `spaces attach --rw`.

`--space` names the space as for every command (its GUID, numeric id or
name); a new name that is the name, GUID or id of a space already there is
refused.

```sh
sudo spaces space rename --space Files --name Archive --yes DISKS...
sudo spaces space resize --space Archive --size 2T --yes DISKS...   # grows; grow the file system yourself
sudo spaces space delete --space Archive --yes DISKS...
sudo spaces pool rename --name Data2 --yes DISKS...
sudo spaces pool remove --yes DISKS...                               # a pool without spaces
```

### Tiered spaces

A tiered space puts its first part on SSD disks (a two-way mirror) and the
rest on HDD disks (simple or single parity: mirror-accelerated parity), as
`New-StorageTier` and `New-VirtualDisk -StorageTiers` make it. Mark the
disks' media first, then create a template per tier and the space over
them:

```sh
sudo spaces disk set --disk 1 --media ssd --yes DISKS...      # each SSD disk
sudo spaces disk set --disk 3 --media hdd --yes DISKS...      # each HDD disk
sudo spaces tier create --name fast --media ssd --resiliency mirror --yes DISKS...
sudo spaces tier create --name big --media hdd --resiliency parity --columns 3 --yes DISKS...
sudo spaces space create --name Files --tier fast=100G --tier big=2T --yes DISKS...
```

The space gets, on the SSD disks, a write-back cache of 1 GiB and the dirty
region log and parity journal its tiers need. It needs at least two SSD
disks; tier sizes are rounded up to whole rows. Linux reads tiered spaces
but does not write them: format and fill them on Windows.

### Disks

`spaces info DISKS...` lists the disks of a pool with their ids.

```sh
sudo spaces disk add --new /dev/sde --yes /dev/sdb /dev/sdc /dev/sdd   # the pool's disks, then the new one
sudo spaces pool optimize --yes /dev/sdb /dev/sdc /dev/sdd /dev/sde    # spread the spaces over it
sudo spaces disk set --disk 4 --media ssd --yes DISKS...               # or --usage manual-select, hot-spare
sudo spaces disk retire --disk 2 --yes DISKS...                        # move everything off disk 2
sudo spaces disk remove --disk 2 --yes DISKS...                        # then take it out of the pool
```

`disk add` takes a blank disk (`--wipe` takes one that is not and clears
its first 17 MiB and last MiB first, as `pool create --wipe` does; until
1.3 it cleared nothing but what the new partition table overwrites) and a
pool of at most four disks. Every management command carries its plan out
on the devices it opened to make it (until 1.3 it opened the paths again,
which by then could name other disks). `disk retire` moves every extent of the disk to the others
(pools of up to five disks); it needs room there, and the rows of every
space need enough other disks (a three-column parity space needs three
disks besides the retired one). `disk remove` removes a disk that holds
nothing any more; the disk keeps its data behind a partition table without
the pool partition.

### Replacing a failed disk

With a disk missing, the pool database and the spaces' redundancy still
allow a repair:

```sh
sudo spaces pool health /dev/sdb /dev/sdc                   # what is missing, what is degraded
sudo spaces disk add --new /dev/sdf --yes /dev/sdb /dev/sdc  # a new disk, if the others have no room
sudo spaces pool repair --yes /dev/sdb /dev/sdc /dev/sdf    # rebuild the lost copies
sudo spaces disk remove --disk 3 --yes /dev/sdb /dev/sdc /dev/sdf   # the missing disk
```

`pool repair` rebuilds every copy that is on a missing disk or out of date
on another disk: mirror copies from a current copy, single parity columns
from the other columns. Without the new disk it uses the disks at hand if
their rows allow it.

### Health, scrub

`spaces pool health DISKS...` shows what Windows would show
(`Get-StoragePool`, `Get-VirtualDisk`, `Get-PhysicalDisk`) for the pool with
the disks given, and how many more disk failures every space survives:

| Shown | Meaning |
|---|---|
| Healthy / OK | every disk of the pool at hand, every copy current |
| Warning / Degraded | a disk of the pool is missing (even with nothing of this space on it), or copies are out of date |
| Warning / Degraded Incomplete | copies of this space are on missing disks; its data is still complete |
| Unhealthy / No Redundancy Degraded | data of this space is lost; reads of it fail |
| Unhealthy / Detached | the pool lost its quorum |
| pool Warning / Degraded | disks missing, more than half of the database copies at hand |
| pool Unhealthy / Read-only | half of the database copies or fewer at hand |
| disk Warning / Lost Communication | the disk is missing |

These are the states Windows showed for the same pools, created on Linux
and by Windows, after disks were detached while in use.

`spaces pool scrub DISKS...` reads every copy of every mirror space and
every stripe of every single parity space, and of such tiers of a tiered
space, and reports what disagrees (read-only). Differences where writes were under way (extent runs the
dirty region log lists, stripes the parity journal does not list as
consistent) are what a crash leaves and are counted apart; Windows settles
them when it takes the pool. `--repair --yes` makes everything agree,
keeping the first mirror copy and recomputing parity from the data.

### Limits

* Dual parity spaces are not created, nor tiered spaces of other shapes
  than an SSD mirror over an HDD tier; existing ones are read, and their
  pools can be changed otherwise.
* Pools of version 29 (Insider builds) get no new spaces; their records
  differ there. Other changes keep each record's own layout.
* Windows 11 24H2 crashed (a bugcheck in its Storage Spaces driver) when a
  pool of four disks with simple, mirror and parity spaces arrived with one
  disk absent,
  whether Linux or Windows had created the pool. Replace or remove a failed
  disk on Linux before handing the pool to Windows.

## Health checks and forced attach

Before `spaces attach` reads a space (also when the udev rule and the
attach unit run it at boot), and before `refs mount` mounts a ReFS volume
(so also `mount -t ReFS` and udisks2), the space or volume is checked. The
checks are quick: a few reads per space and partition, and at most 1 GiB
of parity stripes. `spaces check` runs the same checks and prints the
report; `refs check --quick` does it for a ReFS volume.

### Verdicts

| Verdict | Means | Without a flag | With a flag |
|---|---|---|---|
| healthy | every check passes (warnings and notes allowed) | attached and mounted as asked; written with `--rw` | |
| degraded | Windows' metadata shows redundancy reduced (a disk away, copies out of date); the data is complete | not attached | `--degraded`: read-only |
| suspect | what was read disagrees with itself: the pool lacks its quorum, copies of the cache, the partition table or a ReFS superblock disagree, parity stripes do not match, a ReFS log holds changes its checkpoint lacks | not attached, not mounted | `--force` (`-o force` for a mount): read-only |
| failed | data is lost, or metadata is not understood or breaks what the format implies | never | never: the report only |

Writing (`spaces attach --rw`, `refs mount --rw`, the writing `refs`
commands) needs a healthy verdict. Each layer asks on its own: a ReFS
volume in a space attached with `--degraded` or `--force` mounts only with
`-o force` too.

### The checks

* `pool.quorum`: more than half of the copies of the pool database are at
  hand (Windows takes the pool read-only otherwise).
* `pool.members`: every disk of the pool is there.
* `pool.database`: the copies of the pool database agree; older or
  unusable copies that are not used are warnings, a newer copy that does
  not decode is suspect, as are copies of the sequence read that say
  something else when most copies do not hold the version read (copies that
  differ only in their bytes are a warning). One copy is a note: there is
  nothing to compare it with.
* `pool.clean`: the metadata agrees with itself and with the devices (no
  extent on a disk it does not list, no disk given twice or cloned, no two
  records with one id).
* `space.state`: the state Windows would show (Healthy, Warning /
  Degraded, Unhealthy).
* `space.layout`: the slab map parses; no slab is mapped twice, no extent
  lies outside its space or beyond its disk's partition; a fixed space maps
  every row of its size (one with rows left without an extent lost extent
  records, and fails).
* `space.cache`: the write-back cache loads and its copies agree (cached
  chunks are a note).
* `space.journal`: the stripes the parity journal does not record as
  consistent match their parity (Windows lists stripes never written and
  those its cache holds too; up to 1 GiB is read, the rest is checked when
  read).
* `space.drl`: the extent runs the mirror's dirty region log lists: their
  copies are compared, up to the same 1 GiB of each (what the write-back
  cache holds is read from the cache and not compared); copies that differ
  (a crash or a pulled disk while Windows wrote, a disk back after it
  missed writes, before Windows repaired the space) make the space
  suspect, and reads of those places fail (`--unclean-parity data` reads
  the highest copy).
* For all three, the header on every copy of its mirror: a damaged copy
  (another valid) is degraded, copies that disagree suspect, none valid
  failed; a header zero everywhere is a structure never written (a note).
* `space.partitions`: the protective MBR, both GPT headers with their
  CRCs and positions, both copies of the partition entries the same, the
  partitions inside the space and apart; a primary header lost with the backup
  valid is suspect, and the partitions are read from the backup (their
  file systems are checked); a protective MBR without a GPT is suspect.
* `fs.ntfs`: the NTFS boot sector and its copy, in the sector after the
  volume (where Windows writes it; 1.2.2 looked in the partition's last
  sector), agree.
* `fs.refs.boot`, `fs.refs.superblock`, `fs.refs.checkpoint`,
  `fs.refs.log`: the ReFS boot sector (the volume fits its partition), the
  superblock and its two copies, both checkpoints, and that the log holds
  nothing newer than the checkpoint (ReFS 1.x keeps a log `refs` does not
  read; 3.1's is read since 1.3). A log whose control page is not found is suspect when
  the checkpoint shows records written to it (whether Windows would
  replay some is not known), a warning when it shows none.
* `fs`: a partition of neither file system (skipped: not checked).
* `pool.open`: disks that do not assemble into a pool (failed, with why).
* `space.verdict` (`refs check --quick` and `refs mount` on a device of an
  attached space): the verdict attach gave the space.
* `space.checks` (`fs.refs.checks` for a volume `refs` checks on its
  own): only when the checks of a space did not finish. A panic
  in them (a defect of this program that the metadata reaches) fails the
  space; checks that take longer than 40 s make it suspect
  (`SPACES_CHECK_TIMEOUT=SECONDS` sets the limit, 0 none; `--deep` has
  none). Either way the other pools and spaces are checked and attached
  as usual. Since 1.3 each pool is checked in a child process of its own,
  with a memory limit of 2048 MiB (`SPACES_POOL_MEMORY=MIB`, 0 none; the
  84 pools of the corpus check within 512) and a deadline of 120 s
  (`SPACES_POOL_TIMEOUT=SECONDS`, 0 none; never past the time of
  `spaces attach`; with `--deep`, which may take hours, none unless the
  variable sets one; `--bundle` checks in its own process, without the
  child's limits): a pool whose checks reach either gets one failed
  `space.checks` report naming the limit, and nothing of it is attached
  (1.2.2 checked every pool in one process, where such a pool took the
  whole run with it). `spaces check` and `spaces attach` take the devices
  of several pools at once and check each pool on its own.
* With `spaces check --deep`: every listed parity stripe, `deep.scrub`
  (every copy compared, as `spaces pool scrub` does; a scrub that cannot
  run, as on a pool that is not clean, makes the space suspect) and
  `deep.refs` (ReFS volumes checked whole, as `refs check` does).
* `pool.quorum` also: a pool database that names no disk keeping a copy of
  it, read from devices that keep one, is suspect.

### The report

```
$ sudo spaces check /dev/sdb /dev/sdc /dev/sdd
space "data" (6af700fc-207e-4c13-ab93-4123223e1d17) of pool "Storage pool" (78bb278c-1f67-4a13-bd4e-980e2bd17a49): SUSPECT
  ok       pool.quorum       3 of 3 copies of the pool database at hand
  ok       pool.members      3 of 3 disks (/dev/sdb, /dev/sdc, /dev/sdd)
  ok       pool.database     3 copies of sequence 2, all the same
  ok       pool.clean        the pool's metadata agrees with itself and with the devices
  ok       space.state       Healthy / OK, as Windows would show it (survives 1 more disk failure)
  ok       space.layout      Mirror, 1 column(s), 2 copies, interleave 256 KiB, 2 extent(s) on 2 disk(s)
  skipped  space.cache       no write-back cache
  skipped  space.journal     no parity journal (not a parity space)
  info     space.drl         1 extent run(s) written since the space was last disconnected: compared, the copies agree
  SUSPECT  space.partitions  the backup GPT header is not valid
           what      the backup GPT header
           where     LBA 2097151 (space byte 0x3ffffe00 (virtual slab 3), column 0 row 3: copy 0: disk 3 slab 4 -> /dev/sdd at 0x70fffe00, copy 1: disk 1 slab 5 -> /dev/sdb at 0x80fffe00)
           expected  "EFI PART" (the primary at LBA 1 names LBA 2097151 as its backup)
           found     00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 ... (all zero)
           truth     our cross-check of copies Windows keeps or of checksums
  ok       fs.ntfs           p1: the NTFS boot sector and its copy after the volume agree
  what to do:
    - space.partitions: Windows keeps two copies of the partition table, and
      they disagree here (or one is not valid). If Windows shows this space as
      healthy, spaces probably reads part of it wrongly: please report it with
      the bundle of spaces check --bundle.
  environment: spaces 1.2.0, kernel 6.8.0, pool version 28
```

(`spaces attach` adds what it did, `not attached: a suspect space needs
--force (and is then read-only)`, and the command that reads it anyway.)

Every check has an id, a status (ok, info, warning, skipped, degraded,
suspect, failed) and a summary; problems have their evidence: where (the
device and offset, the LBA, and for a byte of a space the cache block or
the slab, disk and offset that hold it), what was expected and what was
found (bytes in hex), and where the truth comes from (Windows' metadata,
a cross-check of copies or checksums, an invariant of the format). The
report goes to:

* standard error of the command that refused: whole on a terminal;
  under the attach unit a line for the space and one per failing check,
  so `journalctl -u 'storage-spaces-attach*'` shows them;
* `/run/storage-spaces/reports/<space GUID>.txt` and `.json` (readable by
  every user), and `refs-<serial>.txt` for a refused ReFS mount (as root);
* `spaces status` (the spaces not attached, and the verdict of those
  attached; since 1.3 a report names the devices it was made from, and
  one whose devices are all gone, unplugged since, is left out),
  `spaces info` (the verdict of each space);
* `mount -t ReFS` prints the first failing check and the report's path.
  udisks2 shows "Unknown error when mounting" for whatever a mount helper
  refuses; the same line is in `journalctl -u udisks2`.

Names come from the disks: in reports for people, refusals and listings
their control and format characters (escape sequences, line breaks,
bidirectional overrides) are shown escaped as `\u{..}`.

`spaces check --json` prints the reports for programs, with stable check
ids and statuses (names as they are, JSON-escaped); `spaces check --bundle FILE.tar.gz` also packs the
reports, `spaces dump` and `spaces extents` of each pool and both ends of
each space (its partition table) for a bug report: no file data, but the
names and GUIDs of pools, spaces and disks, so look it over before sharing
it.

### Reading anyway

```sh
sudo spaces attach --degraded            # degraded spaces, read-only
sudo spaces attach --force               # degraded and suspect spaces, read-only
sudo mount -t ReFS -o force /dev/mapper/ss-<pool>-<space>-p2 /mnt
```

A space attached so is read-only, and udev hides its devices from
udisks2, so that no desktop mounts them by itself. `spaces export`,
`serve-ublk`, `serve-nbd` and `serve-fuse` take the same flags.

### Pools with missing disks

With a disk of its pool missing a space is degraded. `spaces info
<disks...>` shows which disk is missing and whether each space is degraded
(still complete) or failed (data lost with the disks at hand). If the
redundancy of every space still covers the loss (mirror: at least one copy
of every slab; single parity: at most one missing disk; dual parity: at
most two), `--degraded` attaches it, read-only. Reads that need a missing
disk fail with an I/O error instead of returning wrong data. Copies that
missed writes while their disk was away are never used.

If no more than half of the copies of the pool database are at hand, the
pool lacks its quorum, as Windows counts it (it takes such a pool
read-only and detaches its spaces): the disks at hand may all be disks
that dropped out earlier, and their metadata would describe an old state
of the pool. Its spaces are suspect; use `--force` only when you know
these disks were the last ones written.

### After a crash or power loss

Windows records which mirror extent runs (dirty region tracking) and which
parity stripes (parity journal) had writes in flight. For such stripes whose parity does not match the data, it is not
known which side Windows would keep, so reads of them fail by default,
`spaces info` reports

```
parity journal: 1 extent run(s) not cleanly shut down; mismatching stripes are refused
```

and the space is suspect when the check finds them (or when the copies of
the write-back cache disagree, which a crash leaves too). Options:

* Attach the pool to Windows 11 once; it repairs the stripes. Then read it on
  Linux again.
* Read the on-disk data as it is, accepting that at most the stripes being
  written at the crash may differ from what Windows would show:

  ```sh
  sudo spaces --unclean-parity data attach --force
  ```

Mirror spaces are handled differently: Windows lists the extent runs
written since the space was last disconnected (a restart does not clear
the list, so nearly every mirror shows some, and `spaces info` reports
`dirty region log: N extent run(s) written since ...`). The copies of such
runs are compared on every read, and rows whose copies differ, which only a
crash with writes in flight leaves behind, are refused the same way:
Windows itself never reconciles them and reads either copy, so there is no
right answer. Comparing costs reading every copy of those runs. The
write-back cache and the parity journal are mirrored too; what their copies
disagree about is treated the same way. `--unclean-parity data` reads the
highest mirror copy and the newest cache and journal slots. Simple spaces
need nothing special after a crash.

### Volumes Windows did not dismount

A ReFS volume whose log holds changes its checkpoint lacks (the disk was
pulled or the computer turned off while Windows had it; Windows' fast
startup leaves volumes so) is suspect: what `refs` reads is the older
state. Start Windows and shut it down fully (`shutdown /s /t 0`, or with
fast startup turned off), and Windows writes a checkpoint that covers the
log; or read the older state anyway with `-o force`.

### Other diagnostics

`spaces info <disks...>` prints the pool, its disks and every space with its
state:

* `healthy`: every copy and column is on a disk at hand.
* `degraded`: some copies or columns are on missing or out-of-date disks,
  but all data can still be read or rebuilt; attach with `--degraded`.
* `failed`: some data is only on missing disks; reads of it fail.

Warnings about the pool database: a `stale` copy belongs to a disk that was
away while the pool changed (normal); a `torn` copy was cut short by an
interrupted update and is ignored in favour of the copy most disks hold; an
`unusable` newer copy could not be decoded, so an older one is used.

`spaces dump DEVICES...` prints the whole metadata one fact per line: every
copy of the pool database with its records, and the dirty region log,
parity journal and write-back cache of each space. `spaces diff --old
DEVICES... --new DEVICES...` lists what changed between two states of a
pool (for example copies of the disks before and after Windows used it),
which helps with bug reports.

## Copying a space out

```sh
spaces export /dev/sdb /dev/sdc --space Data --output data.img
sudo losetup -r -P -b <sector size> -f --show data.img
```

`spaces info` prints the sector size; 4 KiB spaces need `-b 4096`, or the
partition table will not be found. The image is a new file only its owner
may read (0600: it holds the space's data), written beside its name and
renamed into place once it is complete and on disk, so that a failed
export leaves no partial image (1.2.2 left one, with the umask's mode).

## Reading ReFS volumes

`refs` reads ReFS 3.x volumes (Dev Drives, ReFS data volumes, ReFS inside a
space) without Windows, and writes to them only when asked (the writing
commands with `--yes`, `refs mount --rw`; see Writing ReFS volumes). The
source is an image, a
disk or a partition (on a disk the first ReFS partition of its GPT), or,
with `--space NAME`, the member disks of a pool:

```sh
refs info /dev/sdb2                       # version, clusters, checksums
refs ls /dev/sdb2 --path /src -l          # attributes, size, modified (UTC)
refs ls /dev/sdb2 -R                      # the whole tree
refs stat /dev/sdb2 --path /src/main.c    # times, extents, streams, snapshots, link target
refs cat /dev/sdb2 --path /notes.txt --stream summary
refs cat /dev/sdb2 --path /db.mdf --snapshot nightly > db-nightly.mdf
refs ls --space Data /dev/sdd /dev/sde    # ReFS inside a space
refs check /dev/sdb2                      # consistency, read-only (exit 1 on a problem)
refs mount --space Data /dev/sdd /dev/sde mnt    # options before the disks
mkdir -p mnt && refs mount /dev/sdb2 mnt  # read-only FUSE mount, foreground
refs mount --rw image.img mnt              # for writing (experimental)
fusermount -u mnt                         # (as root: umount mnt)
```

Mounted by root, a block device (a disk, a partition, a space's
`/dev/mapper/ss-...-pN`) becomes a mount of type `fuseblk.refs` on that
device, as ntfs-3g's are, so that udisks2 and `findmnt` count it as the
device's mount. `refs` makes that mount itself, with the device's sector
size as block size (devices with 4 KiB sectors included), without
`fusermount3`; the mount goes away when `refs` ends, also when it is
killed (a small watcher process unmounts it).

The mount shows named streams as extended attributes `user.<name>`
(`getfattr -d`; Linux limits them to 64 KiB, larger streams are read with
`refs cat --stream`), symbolic links and junctions as symbolic links (an
absolute target `C:\path` points into the mount) and hard links as one
inode with its link count.

With `--rw` (one image, disk or partition; not `--space`) the mount
writes through the same code as the writing commands below: creating,
writing, truncating and deleting files (a file opened for writing is
kept in a temporary file without a name that only the mounting user may
read (`O_TMPFILE`, mode 0600; 1.2.2 made it 0644 under a name others could
guess, for a moment), in `$REFS_TMPDIR` or the system's temporary
directory; when it is closed or synced, what was written goes
to the volume as Windows writes it: changed bytes where they are,
appended data in new runs, a shortened file's tail freed, the touched
clusters of an integrity stream copied on write; what lies past a file's
old end reads as zeros where nothing was written there, also after it
was shortened and grown again (1.2.2 let the bytes it held there come
back); a sparse file keeps its holes (bytes written into one take
clusters unless they are zeros; written whole, its clusters of zeros
stay holes; 1.2.2 wrote it dense); a file in its record,
a new or truncated file and a file Windows compressed are written
whole), copies (a whole file copied
into an empty one with copy_file_range, as coreutils 9 `cp` and file
managers copy, becomes a block clone that shares the clusters, as
`Copy-Item` makes on a Dev Drive), directories, renames and moves, hard
links, times (`touch`), the read-only attribute (`chmod a-w`) and named
streams (`setfattr -n user.NAME`). Every change is one transaction with a new checkpoint, so a
crash or a pulled cable leaves the volume as it was after the last
completed change. Symbolic links cannot be made. The mount is served by
one thread; it suits copying files to or from a Dev Drive, not heavy
use. Replacing a file by renaming another over it (`mv -f`, and the way
editors save) is one change too: a crash leaves the file that was there
or the renamed one in its place, never neither (1.2.2 deleted the first
in one change and renamed in the next); so are a block clone to a new
name and a named stream in clusters replaced. A file is created when it
is opened and its data written when it is closed, two changes as on other
file systems: a crash between them leaves it empty. A change `refs`
refuses (a name that is not printable ASCII, for now, or one Windows
programs could not open, see below) changes nothing; one
the volume has no room for fails with ENOSPC (1.2.2: "Operation not
supported"). The mount holds the volume for itself: the writing commands
and another mount are refused while it is mounted ("... is in use by pid N
(refs mount)"), as a mount for writing is while a read-only one is; 1.2.2
let `refs write` change a volume mounted for writing, and the mount's next
change wrote its own view of the volume over that. A file whose last name
goes while it is open (unlinked, or renamed over) reads and takes writes
through its open handles until the last is closed, which writes nothing
back (1.2.2 failed them, and the close, with ENOENT). A directory listed
while names go or come lists every name it had when it was opened, each
once (1.2.2 skipped names when files were deleted during the listing, as
`rm -r` and `find -delete` do). `st_blocks` (`du`) counts what a file has
on the volume: a sparse file less than its size. Names with a lone
surrogate (a UTF-16 unit Windows allows in names and UTF-8 cannot carry)
are shown as WTF-8 bytes, each its own file, and are read, not changed
(1.2.2 showed U+FFFD for each, so two such names were one).
A change whose writes fail on the disk reports the error, and the volume
is read again from the disk (its checkpoint may be down or not); if that
fails too, every later change is refused until the volume is mounted or
opened again. A request that stops on an internal error (a defect of `refs` that
the volume reaches) fails with EIO and the mount goes on, read-only or
not; after one inside a change, the volume in memory may be half changed,
so every later request fails with EIO: unmount it (the volume on disk is
as after the last completed change).

`contrib/mount.refs`, installed as `/sbin/mount.ReFS` (the type blkid
reports) and `/sbin/mount.refs`, lets `mount -t ReFS /dev/sdb2 /mnt`,
`/etc/fstab` entries (`/dev/sdb2 /mnt ReFS ro,nofail 0 0`) and udisks2
(`udisksctl mount -b /dev/sdb2`, desktop file managers) mount ReFS
volumes through `refs mount`, in the background. Mounted by root, every
user may enter the mount; it is read-only unless the options hold both
`rw` and `refs.rw`. Mounted by root, a block device is mounted as type
`fuseblk` (as ntfs-3g mounts are), so udisks2 counts it as the device's
mount: `udisksctl unmount` and the file managers unmount it. Every metadata page is checked against its checksum; a volume that
fails is reported, not guessed at. The data of integrity streams
(`Set-FileIntegrity`, or volumes formatted with integrity streams) is
checked on every read too: damaged data is an error (EIO in the mount),
never returned.

`refs` reads the volume's last checkpoint. A volume that was detached
without being dismounted (the disk image detached, the cable pulled, or
Windows' fast startup) can have newer changes in its log, which Windows
replays when it attaches the volume again; `refs info` says so ("records
past the checkpoint"), and the writing commands refuse such a volume:
attach it to Windows once and take it offline or safely remove it before
writing with `refs`.

Before `refs mount` (and so `mount -t ReFS` and udisks2) mounts a volume,
it runs the quick checks `refs check --quick` prints: the boot sector, the
superblock and its two copies, both checkpoints, and that log. A volume
that is not healthy (such a log makes it suspect) is not mounted unless
asked, read-only:

```sh
refs check --quick /dev/sdb2         # the report, with the evidence
sudo mount -t ReFS -o force /dev/sdb2 /mnt
```

A refused mount says the first failing check and where its report is
(`/run/storage-spaces/reports/refs-<serial>.txt`); udisks2 shows that line.
A volume on a space `spaces attach` attached past its verdict (with
`--degraded` or `--force`) needs `force` too. `refs ls`, `cat`, `stat` and
`info` read a volume that is not healthy and say so.

Changing a file's times or attributes is the first write `refs` does
(experimental; a ReFS volume that is not attached anywhere else; only
with `--yes`, otherwise it prints what would change):

```sh
refs set devdrive.img --path /notes.txt --modified "2024-02-29 12:00:00" --yes
refs set /dev/sdb2 --path /notes.txt --attributes 0x21 --yes   # read-only, archive
refs overwrite devdrive.img --path /data.bin --at 4096 --from patch.bin --yes
refs create devdrive.img --path /notes/todo.txt --from todo.txt --yes
refs rename devdrive.img --path /notes/todo.txt --to done.txt --yes
refs delete devdrive.img --path /notes/done.txt --yes
refs mkdir devdrive.img --path /notes/archive --yes
refs write devdrive.img --path /notes/log.txt --from more.txt --append --yes
refs move devdrive.img --path /notes/log.txt --to /archive/log.txt --yes
refs link devdrive.img --path /archive/log.txt --to /notes/log.txt --yes
refs clone devdrive.img --path /archive/big.iso --to /notes/big.iso --yes
```

`refs overwrite` replaces bytes inside a file (not beyond its end, not in
sparse ranges; the clusters of integrity streams it touches are copied
on write, a compressed file of up to 64 MiB is copied whole) where they
are, as Windows does,
and sets the modification and change times to now. `refs create` makes a
file (up to 1 KiB kept in its record; larger ones in clusters near the
data of the files around it, read from the local file piece by piece,
up to 64 GiB) with a printable ASCII name, in a directory
of any size (its pages split as it grows and merge as it shrinks); it
takes the permissions of the files beside it. Names Windows programs
could not open are refused: a name ending in a dot or a space, and the
device names (CON, PRN, AUX, NUL, COM0 to COM9 and LPT0 to LPT9, also
before an extension, as in `con.txt`). Names are ordered and compared as
the volume's upcase table says, as Windows does (1.2.2 took Unicode's
upper case, and a new name in a directory of non-ASCII names could go
where Windows does not look for it); a path may name its directories in
any case. Writing a file, a named stream, renaming, moving or linking it
sets its archive attribute, as on Windows. `refs rename` renames a
file or directory within its directory and `refs delete` deletes a file
(for a hard-linked file: the one name; also symbolic links, junctions
and files with stream snapshots) or an empty directory; deleting a
file whose clusters other files share (block clones, deduplication)
lowers their reference counts and frees only clusters no other file
has. A file Windows compressed is changed by writing it whole into
ordinary clusters (Windows copies only the clusters it touches); its
compressed clusters lose a reference each, and those no file references
any more are left for Windows' dedup job to reclaim, as Windows leaves
them. `refs mkdir` creates a directory;
`refs write` replaces a file's content (or appends to it with
`--append`), keeping its creation time, attributes and permissions, its
named streams and every other row of its record that is not its data
(1.2.2 dropped the rows it did not know).
New data goes into the volume's data containers; when they are full
`refs` hands out the next free container for data, as Windows does,
and only then uses room in the metadata containers.
`refs move` moves a file or directory into another directory and `refs
link` gives a file another name (a hard link); changing a hard-linked
file through one name changes it for all of them. `refs clone` copies a
file as a block clone, as `Copy-Item` does on a Dev Drive: the copy
shares the file's clusters (counted in the block reference count table)
and takes no room until one of them changes; writing to either later
leaves the other as it was. `refs write --stream
NAME` writes a named stream (an alternate data stream; up to 1 KiB stays
in the file's record) and `refs delete --stream NAME`
deletes one. `refs set --integrity
on` turns integrity streams on for an empty file (as Set-FileIntegrity
does); its data is then written with checksums (CRC32-C per 4 KiB
cluster, CRC-64 per 16 KiB on 64 KiB clusters; up to 2 GiB), and files
created in a directory with
integrity streams have them too.

It writes the way Windows does (copy on write, then a new checkpoint), so
an interruption leaves the volume as it was before; both checkpoints stay
whole, as Windows keeps them: a commit takes none of the clusters of the
pages the commit before it replaced (1.2.2 took them, overwriting the
older checkpoint).

`refs check` reads the whole volume: every page the checkpoint reaches
(its checksum, its rows, its clusters used in its allocator), every table
(no key twice, also across its pages; the counts of rows and pages in its
descriptor those it has), every file (its clusters used; a cluster
referenced several times, by block clones or deduplication, counted so in
the block reference count table, and no count for one referenced once or
not at all), every directory (its names in the order of the volume's
upcase table, where Windows looks them up) and the compressed containers,
and exits 1 on a problem. It
also counts the pages only the older checkpoint references (Windows frees
them with its next checkpoint) and, on ReFS 3.14 volumes, the clusters
used that nothing references; Windows leaves some on every volume (about
150 on a small one), so they are no problem by themselves, but a change by
`refs` should not add to them.

Read: files (resident, in extents, sparse, block-cloned, deduplicated),
directories of any size, named streams, stream snapshots (`refsutil
streamsnapshot`; `refs cat --snapshot`, not in the mount), hard links,
symbolic links and junctions, attributes and times, volumes with 4 KiB
and 64 KiB clusters, CRC-64 or SHA-256 metadata checksums and integrity
streams, and files Windows compressed with LZ4 or ZSTD (`Enable-ReFSDedup
-Type DedupAndCompress`, `refsutil compression`; checked unit by unit)
(verified on ReFS 3.14 volumes made by Windows 11 and on ReFS 3.7, 3.4,
3.1 and 1.2 volumes made by Windows Server 2022, 2019, 2016 and 2012 R2).
Not verified: ReFS 1.1, 3.2, 3.3, 3.5, 3.6 and 3.8 to 3.13. Not read yet:
encrypted files, ReFS 2.x. Only ReFS 3.14 volumes are
written; the writing commands and `refs mount --rw` refuse other
versions. Compressed files are
changed by writing them whole, see above; their named streams, if
compressed, are not changed.

## Hardening

Everything that parses a disk runs as root, and a disk can be anyone's: a
USB stick plugged in, a pool handed over. So the parts that do it are
limited to what they need (checked on Ubuntu 22.04 with `systemd-analyze
security`, scores 9.5 UNSAFE before, 5.1 MEDIUM after):

* `storage-spaces-attach.service` (installed by `contrib/install.sh`):
  no new privileges, the file system read-only except `/run`, a private
  `/tmp`, no namespaces, no network but local sockets and netlink, and
  without 24 capabilities it has no use for. That is a deny-list
  (`CapabilityBoundingSet=~...`): the 17 others stay, CAP_SYS_ADMIN
  (device-mapper, loop devices, mounts) and CAP_SYS_MODULE (the module
  loader) among them, and with them CAP_SYS_RAWIO, CAP_DAC_OVERRIDE,
  CAP_DAC_READ_SEARCH, CAP_CHOWN, CAP_FOWNER, CAP_FSETID, CAP_MKNOD,
  CAP_SETFCAP, CAP_IPC_LOCK, CAP_IPC_OWNER, CAP_LEASE, CAP_SYS_NICE,
  CAP_SYS_RESOURCE, CAP_NET_BIND_SERVICE and CAP_NET_BROADCAST, which it
  has not been shown to need; and no system call filter. (`mount.ReFS`
  starts its unit with an allow-list of 7 instead.) Home
  directories stay readable, as pool disks may be image files. The
  private `/tmp` is an empty file system of its own
  (`TemporaryFileSystem=`), not `PrivateTmp=`: that orders a unit after
  `systemd-tmpfiles-setup.service`, which comes after `local-fs.target`,
  and the unit runs before `local-fs-pre.target` so that fstab can mount
  volumes on spaces. 1.1.0 and 1.2.0 had that ordering cycle, and systemd
  broke it at every boot by dropping jobs: other units' mounts, the setup
  of `/tmp` (then logging in to GNOME failed), or the unit itself. No
  `/var/tmp` (1.2.2 had one): with `/var` a file system of its own, not
  mounted yet, systemd would make the directory on the root file system,
  read-only that early at boot, and the unit would fail.
* The servers `spaces attach` starts (`serve-ublk`, `serve-nbd`,
  `serve-fuse`, as transient `storage-spaces-<guid>.service` units) get
  the same limits from the program itself, but for the FUSE server a
  private view of the file system, whose mount the host could not see.
  A server attached with `--rw` may write the member image files besides
  (`ReadWritePaths=`; 1.2.2's could not, EROFS).
  A server that cannot go on ends with an error, for systemd to show
  (`systemctl status storage-spaces-<guid>`), rather than live on
  uselessly: `serve-ublk`
  after an internal error (a panic in a queue fails its requests and
  removes the device; 1.2.2 exited 0, or hung with a request in flight),
  `serve-nbd` when its listener fails for good (out of descriptors it
  goes on once connections close; 1.2.2 stopped accepting and stayed).
  `serve-fuse` unmounts on SIGINT and SIGTERM (1.2.2 left a dead mount
  that failed the next attach), and a signal that comes before a ublk
  device starts is no longer lost. `spaces detach` deletes the ublk device
  of a server that was killed before it could (1.2.2 left it DEAD).
* `mount.ReFS` (what `mount -t ReFS` and udisks2 call, as root) runs
  `refs mount` as a transient `refs-mount-<pid>-<time>.service` without
  network, new privileges, namespaces or most capabilities, its log in
  `/run/refs-mount/<unit>.log` while it serves the mount. It keeps the
  host's mounts (`PrivateMounts=no`: since systemd 254 a private network
  would otherwise bring a private mount namespace, where the host never
  sees the mount). Without systemd it runs a plain process; where systemd
  runs but the unit cannot be started, it mounts nothing and says why
  (1.2.2 ran `refs` as root without the limits then). `refs`
  mounts the block device itself (mount(2)) rather than through the
  setuid helper `fusermount3`, which only users' mounts need.

The limits need systemd 231 or later for the unit file (238 for the
private `/tmp`) and 247 for the servers' (older ones refuse the unknown
property): then `spaces attach`
says so and starts the server without limits, rather than not at all.
The udev rule starts parsing a disk with the Storage Spaces partition type
without anyone asking (see Installing for turning it off). The code
itself has no `unsafe` (`#![forbid(unsafe_code)]`), is built with integer
overflow checks, and its parsers are fuzzed (`docs/security.md`).

## Troubleshooting

* A pool is not attached at boot, or `spaces attach` says `not attached`:
  the space is not healthy. `spaces status` lists it with its verdict,
  `journalctl -u 'storage-spaces-attach*'` has a line for each failing check,
  and `spaces check` (or `/run/storage-spaces/reports/<space GUID>.txt`)
  the whole report with what to do. `--degraded` or `--force` read it
  anyway, read-only; see "Health checks and forced attach".
* The boot journal says `Found ordering cycle ... after
  storage-spaces-attach.service` and `Job ... deleted to break ordering
  cycle` (1.1.0 and 1.2.0; snaps, `/boot/efi` or `/tmp` may then be
  missing, and logging in may fail): update. A copy of the unit made as a
  workaround in `/etc/systemd/system/storage-spaces-attach.service`
  overrides the package's: remove it and run `systemctl daemon-reload`.
  `systemd-analyze verify default.target 2>&1 | grep 'ordering cycle'`
  shows such cycles without a restart.
* udisks2 says `Unknown error when mounting` a ReFS volume: `mount.ReFS`
  refused it, and udisks2 does not pass the reason on;
  `journalctl -u udisks2 -n 20` shows it (for example `SUSPECT:
  fs.refs.log: ...` for a volume Windows did not dismount) and where the
  report is. `mount -t ReFS -o force` mounts it read-only.
* `no Storage Spaces pool members found`: the disks are not visible
  (`lsblk`), or they are not pool members (Windows 11 pools only).
* `device-mapper device ... already exists`: an interrupted run left a device
  behind; `spaces status` lists those with the UUID of a space and no
  state, and `sudo spaces detach --leftovers` removes them. Others (made by
  1.2, without a UUID): remove them with `dmsetup remove <name>`.
* `backend ublk is not available`: load the module (`modprobe ublk_drv`) or
  use `--backend nbd`.
* `... is in use by device-mapper ss-...` or `... by the kernel`: the disk is
  held by an attached space of its pool, or mounted, or assembled by mdadm or
  LVM; detach or stop that first.
* `... is in use by pid N (spaces serve-ublk --space NAME, unit ...)`:
  another `spaces` process serves or writes the pool (a server of one of its
  spaces, `serve-ublk --rw`, a management command); a writer is refused
  while a server of the pool runs, and a server while a writer does (at
  once: nothing waits).
* `... is in use by pid N (refs mount ...)`: the ReFS volume is mounted
  with `refs mount`; the writing `refs` commands, and a mount for writing,
  are refused at once until it is unmounted (a read-only mount allows other
  read-only mounts).
* `the pool is not in a clean state`: a management command was given a pool
  with a missing disk or out-of-date copies; see
  [Replacing a failed disk](#replacing-a-failed-disk) or attach it to
  Windows first.
* `cannot detach ...: in use: ... is mounted on ...`: unmount the volume
  first; the space stays attached and working meanwhile. (1.2.0 and
  earlier went on and stopped the server under the mount, which then
  gave read errors.)
* Logs of serving processes: `journalctl -u 'storage-spaces-*'`.
* A ReFS volume takes long to mount: the first mount of a large volume on
  hard disks reads its tables (minutes on very large volumes; again after
  a restart, when nothing is cached). `mount -t ReFS` waits for it and
  shows every 10 s how much has been read; udisks2 waits too. Set
  `MOUNT_REFS_TIMEOUT=SECONDS` for a limit.
* AppArmor (Ubuntu) logs `apparmor="DENIED" operation="capable"
  profile="fusermount3" capname="dac_override"` (or `setuid`): other
  programs' FUSE mounts (gvfs, portals) cause these, and they are harmless.
  `refs` does not use `fusermount3` when root mounts a block device.
* `mount.refs: refs mounted the volume, but the mount is not visible here`:
  `refs` ran in another mount namespace (a sandbox around `mount`, an old
  `mount.ReFS` on systemd 254 or later); mount from the host.
