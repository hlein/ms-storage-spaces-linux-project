# Changelog

## Unreleased

### Fixes

* A parity space whose disk Windows replaced under some of its extent runs
  read wrong data from the rows of those runs, healthy, and nothing failed.
  A thin space has a run of extents per allocation; Windows moves only the
  runs on the replaced disk and numbers what it moved copy 1. The parity
  rotation counts from the first row of each run, and 1.3.0 (as 1.2.2) saw
  only the runs of column 0's first copy: it rotated a moved run from the
  rows of another. A write into a moved run was refused. (Scrub saw nothing:
  a stripe's units agree with single parity whatever the rotation.) A
  column's runs are now those of all its current copies (the Windows pool
  `replacedrun` reads as written, healthy and with any disk missing).
* A space whose pool database lacks extents Windows always writes was called
  healthy (audit 2026-10-21, SPA-01): a row of a thin space with some of its
  columns' extents and not the others' (Windows allocates a row's columns
  together), and a mirror row with fewer current copies than its policy
  keeps (a copy being rebuilt, or none recorded). The first is failed now
  and the second degraded, and the writer refuses both: 1.3.0 wrote into
  such a thin row by allocating it again, its pool database then holding two
  extents for one column on every disk, and the pool no longer opened.
* A space whose hidden structures or tiers do not hold what the records
  name was called healthy (audit 2026-10-21, SPB-02): a write-back
  cache whose header puts its chunks where the cache space has no extent
  (1.3.0 read the data the cache held as zeros), beyond that space or over
  its slot area, a parity journal whose areas lie so, and a fixed tiered
  space one of whose tiers lost all its extent records, or whose tiers end
  before it does (the rest read as zeros). Such spaces fail now: they are
  not read or written, and `health()` does not know the state of the tiered
  ones. `health()` also laid every tier out from the start of the space
  rather than its own, so that a tier starting off a whole row of its own
  layout made the space's state unknown (the auditor's spa_05); it is laid
  out as it is read.
* The dirty region log's runs were not compared where they could have been,
  or were said to be compared where they were not (audit 2026-10-21, SPG-01,
  SPB-05): the listed runs of a tiered space's mirror tier were never
  compared (healthy, their reads failing where the copies differed); runs
  with one copy at hand were counted as compared ("compared, the copies
  agree"), and unallocated runs of a thin space used up the read budget
  before the runs that hold data; the copies of a header page of the log
  that list different runs in the same generation (which Windows does not
  write) were a note, one copy's runs read and the space healthy; and a log
  whose range had no room for its two header pages was read from its first
  page only. The mirror tier's runs are compared, runs with one copy at hand
  counted apart and unallocated ones passed over, such copies of a page
  make the space suspect, and such a log fails it. `spaces pool scrub` and
  `spaces check --deep` compare the tiers of a tiered space, which they
  skipped.
* One copy of the header of a write-back cache or parity journal that did
  not check out by itself, the other copy sound, failed the space: the
  cache header of another space, a journal header of a slot geometry no
  journal has, and (since the checks above) one naming areas its space does
  not hold. Such a copy now gives way to the other, as a damaged one does:
  the space is degraded and reads as before (the auditor's spb_06).
* The device-mapper mapping (`spaces attach --backend dm`, `spaces
  dm-table`) mapped a row whose only copy is out of date as zeros, though
  reads of it fail, and a run the dirty region log of a mirror lists to its
  first copy where the copies differed, though reads of it fail (audit
  2026-10-21, SPB-06). It refuses the first; it compares the copies of the
  listed runs first, as `spaces check` does, and refuses the mirror where
  they differ or there are more than 1 GiB of them.
* The slot area of a write-back cache or parity journal whose mirror copies
  a disk replacement had numbered column by column (Windows numbers a moved
  copy beyond the others in its column: `replacedrun`'s cache has copies 0
  and 1 in one column, 0 and 2 in the other) was read from one copy only.
  One damaged slot there ended the log: chunks destaged since came back as
  cached, read from blocks that no longer held them, and the space was
  called healthy (found by the oracle run of this release on `replacedrun`).
  Every copy of each unit is read now.
* The writers of the write-back cache and the parity journal (`--rw`) took
  memory and time by what a small log names (audit 2026-10-21, SPB-03,
  SPW-02): the cache writer kept a flag per sector of every cached chunk
  (a checkpoint of 4 KiB naming 256 whole chunks of 64 MiB took 32 MiB),
  the journal writer one per stripe of a run, each looked up among the runs
  of the entry it continued (an entry of 10 000 runs did not finish), and a
  journal's entries were decoded without a bound. A cache entry cut a run
  of more than 0x7fff sectors to 15 bits, and a journal entry too long for
  its slot was cut off with its length left as it was; the journal then no
  longer loaded. A parity space whose journal header counts extent runs of
  another size than the space's was written, after which it did not open,
  and journal checkpoint areas of no bytes were taken, a checkpoint written
  over the header. Both writers keep runs now, the journal's entries take
  the cache index's budget, long runs take several words, an entry that
  does not fit its slot is an error, such a space is refused for writing,
  such checkpoint areas count as none, and run words of no stripes, which
  Windows does not write, are refused.
* Management plans trusted numbers of the pool database they compute
  writes from (audit 2026-10-21, SPW-01, SPW-02, and the auditor's spa_03
  and spa_07): a space numbered beyond the 64 the metadata space holds
  overflowed adding or retiring a disk (a panic), or put its database past
  the slab; a disk claiming more data than its partition holds got new
  slabs there, past the partition; a resize of a space whose allocation
  unit is half a slab wrote extents of no slabs, one whose size is short of
  the groups its extents map wrote over them, and the pool database no
  longer opened; a tier template of an unknown resiliency, or a tiered
  space of one tier, was a panic, and tiers larger than a space together
  were accepted; removing a disk from a metadata space of no copies
  underflowed. Such pools and requests are refused now, a resize follows
  the groups the extents map, and a free slot of the database that a cut
  write left a wrong number in is formatted again. The writer stops after
  any failure once it has written (a write cut short by a full pool let
  the next ones on), and a thin space's new row goes to the disk with the
  most free slabs, counted whole (1.3.0 ranked disks by their first free
  slab).
* Smaller findings of the audit of 2026-10-21 (SPB-04, SPB-06, SPA-02 and
  the auditor's spa_06, spa_08, spg05, spg06, scb_04, scb_06): local
  reconstruction groups of more than 15 data units were rebuilt with a
  coefficient GF(16) does not have (wrong data); a member disk whose
  primary GPT header was lost was no member, and the guard took the
  entries of a primary that does not check out (none at all) over a sound
  backup; MBR partitions were numbered by order, not slot; an NTFS volume
  of no sectors was called sound; two spaces of one GUID, or a name holding
  a NUL (which then failed `nbd-client` at attach), passed as healthy;
  `read_uncached_at` read past the space's end; `spaces dump` listed as
  the pool's the first copy of the newest sequence, not the one the pool
  reads; GPT entries of sizes UEFI does not have were read. Each now does
  what Windows does or refuses.
* The checks of a pool of many spaces (`spaces attach`, `spaces check`)
  took about three times as long as in 1.2.2: 1.3.0's check for unmapped
  fixed spaces looked up each space's hidden spaces among all the pool's
  spaces again, for the space and for the pool's health (the reports of
  10 000 spaces with 40 000 hidden ones: 34 s, 1.2.2 13 s). It takes them
  from what its caller looked up; the library's `guard::pool_reports` looks
  them up once for the whole pool (0.5 s).

## 1.3.0 (2026-10-22)

The rest of the code audit of 2026-10-16: the spaces of a pool attached at
once; verdicts that called damaged spaces healthy (a lost first copy of the
partition table, rows without extents, mirror copies the dirty region log
lists that differ, a deep check that could not run); parity spaces written
on recycled slabs; attach and detach that survive being cut off and refuse
what is in use, upgrades and shutdowns that keep spaces and their volumes in
order; the ReFS writer's changes of several steps in one commit each, its
data and structure as Windows has them, and `refs mount --rw` holding the
volume alone; ReFS at scale; the writes of the management commands; a child
process per pool for the checks of `spaces attach` and `spaces check`, with
a memory limit and a deadline; the licence notices of what ships, in static
binaries that name nothing of the machine they were built on. Every fuzz
target ran 24 hours and the oracles over every corpus pool (docs/security.md
says what ran; the fuzzing found two faults of the ReFS writer, fixed here).
Checked on Ubuntu 26.10 and 22.04 (`tools/guard-check.sh`,
`tools/mount-vm-check.sh`, `tools/package-vm-check.sh` with an upgrade from
1.2.2 on 26.10, `tools/multi-vm-check.sh`, `tools/attach-crash-check.sh`,
`tools/server-check.sh`, `tools/refs-mount-check.sh --rw`, the management
scripts `tools/mgmt-*.sh`), and across two restarts on 26.10
(`tools/boot-vm-check.sh`).

### Fixes

* A pool's spaces can be attached at once. Every server held the pool's
  disks exclusively, also read-only, so that a pool's second space a server
  serves (mirror, parity, tiered or cached spaces, or any beside a simple
  space attached through device-mapper) failed to attach, "in use by another
  process". Read-only servers and `spaces export` now share the disks; a
  `--rw` server and the management commands still need them alone, and
  their refusal names what holds them (a device-mapper device, or the
  `spaces` process and its unit).
* An extent record whose count of slabs is 0 (which Windows never writes:
  no pool of the corpus has one) left its space healthy, reading zeros
  where the extent belonged: the plausible values of 1.2.2 bound the count
  from above only. One changed byte of a pool database kept once can make
  such a record (10 of the oracle's mutants did). Such a space now fails
  (`space.layout`), as for the other implausible numbers.

* A space whose partition table lost its first copy was called healthy:
  with its MBR and primary GPT header overwritten, "no partition table"
  (the whole space checked for a file system); with the MBR left, the
  protective entry was taken for an MBR partition. Now a lost primary
  header with a valid backup is suspect and the partitions are read from
  the backup, so that their file systems are checked; a protective MBR
  without a GPT is suspect. `spaces attach` makes partition devices for,
  and `refs` looks for volumes in, the partitions the checks read.
* A fixed space whose rows are not all mapped (extent records missing) was
  called healthy and read zeros there; it now fails (`space.layout`), and
  `spaces info` no longer says a space without extents survives
  18446744073709551615 more disk failures.
* `space.drl` compares the copies of the extent runs the dirty region log
  lists (up to the read budget; what the write-back cache holds is read
  from it): where they differ, after a crash or while a returned disk
  waits for its repair, reads fail, and the space is now suspect instead of
  healthy.
* An NTFS volume smaller than its partition was suspect: its boot sector
  copy was looked for in the partition's last sector, not after the volume,
  where Windows writes it.
* `--space` names a space the same way for every command: its GUID, its
  numeric id or its name. A key that names several spaces of a pool (a name
  two spaces share, a name that is another space's GUID or id) picked one
  of them, the first for most commands and the last for `spaces check`, so
  that a space named like another's GUID was checked or read in its place,
  and `spaces space delete`, `rename` and `resize` could change it in the
  other's place; it is refused now, naming the spaces. `space create` and
  `space rename` refuse a new name that is the name, GUID or id of a space
  already there. `--pool` likewise: a name two pools share is refused,
  naming their GUIDs (`attach` took both).
* `spaces check --deep` passed a space whose deep check could not run (a
  pool the scrub refuses: a warning, a record not understood): it is
  suspect now. A pool database that names no disk keeping a copy of it,
  read from devices that keep one, is suspect, not skipped.
* `spaces detach NAME` naming no attached space exited 0; it fails now. A
  name that spaces of two pools share (pools of one name) is refused,
  naming them; 1.2.2 detached both.
* A parity space written on slabs that held another space's data (one
  deleted before) was suspect after the first session that wrote it, and
  could not be written again: the stripes of an extent run never written
  hold that data, which their parity does not match, and the parity
  journal lists them as not consistent once a write reaches the run. The
  writer now makes them match their parity before it first lists a run
  (reading the run once). Writers also read as `--unclean-parity` says;
  1.2.2's refused whatever it said.
* An attach that was cut off (killed, a crash, a timeout) left a running
  server and devices that nothing recorded: the next attach failed (or
  attached the space without its partitions) and `spaces detach` refused.
  The state is now written before anything is set up, marked attaching,
  and after each step; `attach` and `detach` undo such an attach first.
  Every device-mapper device carries the UUID
  `STORAGESPACES-<pool guid>-<space guid>[-p<n>]`, only devices with the
  space's UUID are removed for it, and `spaces detach --leftovers` removes
  those no state lists. Two spaces of one pool whose names give one device
  name (`Фото` and `Игры`) get apart by their GUID's digits; 1.2.2 failed
  to attach the second and removed the first one's device.
* `spaces detach` removed the devices of a space it could and stopped at a
  busy one, when what used the space was not a mount in its own mount
  namespace or a device-mapper holder: a mount in another mount namespace,
  a swap, a process holding a device open, a loop device. It now finds
  those too (and what it cannot name by the kernel's count of openers) and
  removes nothing while there are any. A step that fails (a device that
  stays, a server that does not stop, nbd-client or losetup) keeps the
  state with what is left; 1.2.2 removed the state anyway.
* Upgrades: `spaces detach --outdated` (the package scripts run it) left
  the spaces it detached to the attach unit, which attaches none attached
  by hand (image files, `--backend nbd`); it now attaches them again with
  the arguments their state records. The deb's postinst enabled the attach
  unit again on every upgrade; it keeps a unit the admin disabled or masked
  (deb-systemd-helper), and leaves alone a unit of that name in `/etc`.
  Removing the deb while a space it serves is in use failed silently and
  left the server running with no program to detach it; the removal now
  stops, saying what to do. `contrib/uninstall.sh` refuses likewise; the
  Arch package, which pacman cannot stop, warns.
* At shutdown the servers of spaces stopped at the same time as the
  volumes on them were unmounted (a ublk device could go under a mounted
  volume); they are now ordered before `local-fs-pre.target`, which stops
  after every local mount. A pool member that appeared while the attach
  unit ran was missed (udev's start only joined that run): the rule now
  starts `storage-spaces-attach@MAJOR:MINOR.service`, a run of its own per
  member, which masking `storage-spaces-attach.service` turns off too. A
  run of the units now ends within their start timeout
  (`SPACES_ATTACH_TIMEOUT=80` bounds every wait of `spaces attach`, which
  names what it did not get to). The units and the servers no longer mount
  a private `/var/tmp`, which with a `/var` of its own made systemd create
  a directory on a root file system still read-only at boot.
* Servers: `serve-nbd` stopped accepting connections at the first error of
  accept(2), out of descriptors with many connections, and lived on deaf;
  it now goes on once some close, and ends with an error when the listener
  fails for good. `serve-fuse` died on SIGTERM and left a dead mount that
  failed the next attach; it unmounts first now, and attach removes such a
  mount. A panic in a `serve-ublk` queue was only reported by libublk and
  the server exited 0 (or hung, its request in flight); the request fails,
  the device is removed and the server ends with an error. A SIGTERM before
  the ublk device started was lost. `spaces detach` deletes the ublk device
  of a server that was killed (1.2.2 left it DEAD).
* ReFS writes: renaming a file over another on `refs mount --rw` (`mv -f`,
  the way editors save) deleted the target in one change and renamed in
  the next, so a crash between them lost the target. It is one change now
  (`Volume::rename_replacing`), and so are a block clone to a new name
  (an empty file was made first) and a named stream in clusters replaced
  (deleted first): several steps, one checkpoint
  (`Volume::in_one_commit`).
* ReFS writes: a file shortened and then grown again (truncate, then write
  past the end, in place or through `refs mount --rw`) read the bytes its
  last cluster still held between the two ends; they read as zeros now. A
  change of a sparse file wrote it anew, dense (200 MB for Windows' file of
  100 MB) and without its sparse attribute; it keeps its holes now, and
  written whole its clusters of zeros stay holes. A change in place read
  the whole changed range into memory; a MiB at a time now.
* ReFS writes, structure: a new directory took its parent's count of pages
  below its root; clusters copied into the live level of a file with
  snapshots were joined into runs across cluster 64 or 256 (Windows splits
  runs there and bugchecked appending to such a run), and clusters of its
  own that other files share were written in place; a page in a wholly used
  allocator row could not be freed while committing (the change was
  refused); the commit after a commit took the clusters of the pages that
  one had replaced, overwriting the older checkpoint before its own was
  down (both checkpoints stay whole now, as Windows keeps them); and after
  a commit that failed the volume went on with the checkpoint before (it is
  read again from the disk now, and refuses changes if it cannot be).
* ReFS writes: the extent map in a file's record was packed tight (a node
  of 0x48 bytes for one record where Windows keeps 0x70, room for two):
  Windows writing into a hole of a sparse file `refs` had written bugchecked
  (0x149), as it adds a record only in the room the node has. Maps have
  Windows' room now.
* `refs check` finds a key in two leaves of a table, descriptor counts of
  rows and pages that are not the table's, and reference counts that are
  not the references the files make (a cluster counted for fewer, or more,
  or named by no file), and counts the clusters used that nothing
  references (on ReFS 3.14; Windows leaves some on every volume, so they
  are not problems).
* ReFS on a damaged volume: a page whose node header put its rows past its
  key index panicked the writer when it compacted the page to add a row
  (found by the 24-hour fuzzing of the writer). The writer now refuses a
  page whose rows do not lie between its node header and its key index (no
  page Windows wrote in the corpus has them elsewhere); `refs check` names
  such a page. A table whose descriptor counted no rows (or 2^63 - 1)
  panicked the writer deleting (adding) a row, and a page whose key index
  names more bytes of rows than the page holds panicked `refs check`: the
  count wraps now, as the count of pages did, and the check names both.
* `refs mount --rw`: a `refs write` (or any writing command) during the
  mount was lost when the mount wrote its view of the volume over it. The
  writing commands and the mount for writing take the device or image
  exclusively, a read-only mount shared, and the others are refused, naming
  the holder. The buffers of files open for writing were 0644 under a name
  others could guess; they have no name and mode 0600 now. A volume without
  room said "Operation not supported"; ENOSPC now. A file whose last name
  went while open (unlinked, renamed over) failed its reads, fstat and
  close with ENOENT; it reads and takes writes until its last handle
  closes. A directory listed while names went skipped names (`rm -r`,
  `find -delete`); it lists every name it had when it was opened. Two
  names that differ only in a lone surrogate were one (one file
  unreachable, listed twice); they are shown as WTF-8, each its own, and
  read, not changed. `st_blocks` counted the size; it counts what the file
  has (a sparse file less).
* ReFS at scale: on a volume of many containers (10 TiB: 160 000) deleting
  or rewriting a large file searched every container for every cluster,
  about an hour for 100 GiB; containers are found by their physical start,
  and clusters are freed run by run (allocator rows, reference counts),
  their list kept as ranges. A commit no longer reads the container tables,
  or the object table, again when it left them as they were. `refs mount`
  trimmed its inodes at every new one once the kernel held more than it
  keeps (`ls -l` of a large directory), and never kept the listing of a
  directory of more than 200 000 names, so every name looked up there read
  all of it again; it trims when the idle ones are more than it keeps, and
  keeps such a listing alone.
* ReFS at scale, block clones: cloning a large file walked the whole block
  reference count table twice for every block of 1024 clusters it shares,
  and each step down the table went over every page of the change, so that
  the time grew with the square of the file (280 s for 32 GiB; found by the
  24-hour fuzzing of the writer, a clone of a file patched to 15 GB that ran
  past its 5-minute limit); deleting a copy of such a file did the same for
  every block of a row of one count (139 s). Each block's row is found by
  its key now (0.3 s for either). A row of one count reaching past cluster
  2^64, and a row whose total of counts was at 2^32 - 1, panicked the writer
  counting a block of it; the first is an error now, the second keeps its
  total.
* Storage Spaces writers and management (audit 2026-10-16): a copy of the
  pool database was written header first, in one write with its entries,
  so that a write torn by a crash made it the newest copy and could make a
  space vanish; each copy is written entries first, the header last (a copy
  torn so, which the others outvote, is written over when a space is
  opened for writing, as stale copies are), and must fit its pool partition (a partition table that made the partition
  shorter than its database got the copy written past it). The model the
  updates are made on read the database only up to its last page
  formatted whole: where a page in use was formatted only in part (17 of
  the fixtures) an update lost that page's records; it reads every slot
  the header counts. `space resize` of a space of local groups (dual parity
  with local reconstruction codes) counted the columns less the
  redundancy and placed the new rows where no row starts: the space no
  longer opened. `pool scrub --repair` of a parity space whose units are
  larger than 1 MiB recomputed whole units from the damaged MiB on, past
  the end of the slab into other data. A write-back cache entry could
  outgrow its slot (about the thousandth write apart from the others into
  a chunk of 1 MiB in 512-byte sectors) and was cut off, and the log no
  longer loaded; the cache is destaged first. The dirty region log stopped
  listing a run before the data written there was flushed. After a write
  or flush that failed the writer went on; it writes nothing more until
  the space is opened again. Tiny disks were computed with wrapping
  arithmetic (a disk of no bytes passed for a large one; debug builds
  panicked), `columns * copies` could overflow, a write-back cache whose
  chunks are not the space's stripes made destaging panic, a dirty region
  log smaller than its two headers wrapped an offset, and records that do
  not assemble made a thin space's new row land on slab 0, over the
  metadata space (and the update made every copy of the database
  unreadable): each is an error now. In `spaces`: `disk add --wipe` wiped
  nothing; it clears the start and end of the disk as `pool create --wipe`
  does. Plans were carried out on the paths opened again, which by then
  could name other disks; they are carried out on the devices they were
  made with. A path under `/dev` was taken for a block device and any
  other for an image; the file's type decides. GUIDs panicked when
  /dev/urandom could not be read.
* Dual parity on 4096-byte sectors read wrong data with two disks
  missing: Q works on chunks of a logical sector (four packets of 1024
  bytes there), not of 512 bytes, as Windows' Q units show (pool
  `dual7k4`). A parity space whose column Windows rebuilt on another disk
  (copy 1 then, pool `replacedpar`) could not be read with a disk missing,
  nor written: columns are taken at their current copy, whatever its
  number (audit 2026-10-16, S9 and M13).
* ReFS 3.1's log was not checked (the guard skipped it; its region of
  256 MiB was not among the sizes tried), and a log away from the two
  starts seen and from container 7 was not found (ReFS 3.4 starts its log
  at container 6): the control page is also looked for at the first
  cluster of the first 64 containers of the container table, by `refs`
  and by the guard's quick checks (audit 2026-10-16, M11).
* `mount.ReFS` ran `refs` as root without its sandbox when `systemd-run`
  failed (a unit of the same name left over, a systemd that refuses a
  property): it mounts nothing then, and says why. The unit's name is its
  own (process and time), and its log stays in `/run/refs-mount` while it
  serves the mount (it was removed at once, and the unit wrote on into a
  deleted file). `refs` made the report directory under the unit's
  `UMask=0077`, 0700, so that only root could read the reports of the
  volumes it refused: the directories are 0755 now, as `spaces` makes
  them (one report writer for both; audit 2026-10-16, M24).
* The packaged `spaces` carried test hooks: a hidden `write-pattern`
  command, which wrote the members without a plan, `--yes` or the guard,
  and `SPACES_STOP_AFTER_STEP`, which stopped a management plan after a
  step as a crash would. The pattern writer is a separate binary,
  `spaces-pattern`, which the packages do not install; the step hook is in
  builds with the feature `test-hooks` only. `refs write` and the other
  commands that replace a file's content dropped the rows of its record
  the writer does not know (it kept named streams and their sets alone):
  every row but the data's stays (audit 2026-10-16, M25).
* `spaces export` wrote the image with the umask's mode (0644 for most:
  everyone could read the space's data), left what it had written when a
  read or write failed, and did not name the file in its errors. The image
  is 0600 now, written beside its name, made durable and renamed into
  place; a failure leaves nothing and names the file. The NBD socket of
  `spaces serve-nbd` was made 0600 after `bind`, which listens at once: it
  is bound in a directory of its own and moved into place once it is 0600
  (audit 2026-10-16, Low "spaces CLI").
* The ReFS writer ordered and compared names by Unicode's upper case;
  Windows orders a directory's names by the volume's upcase table
  (objects 0x7 and 0x8), which differs at 424 units (pool-less sample
  `r314fold`, made by Windows 11): a new name in a directory of non-ASCII
  names could go where Windows does not look for it. The table decides
  now, and `refs check` finds names out of its order. `refs check` also
  reads the table's pages, which it had left out and counted among the
  clusters nothing references (68 on a new volume, not 156).
* The ReFS writer made names Windows programs cannot open: ending in a
  dot or a space, or a device name (`CON`, `con.txt`, `COM1.log`); they are
  refused. A parent path in another case than its directory's name
  failed after the work ("row in table Object(1536)"). Writing, renaming,
  moving and linking a file now set its archive attribute, as on Windows.
  A cluster a file maps at two places lost one reference when the file
  went and stayed allocated with a count of 0: a file's clusters are let
  go of once per reference, as clones count them (audit 2026-10-16, Low
  "ReFS writer").
* `spaces status` listed the refused spaces of disks unplugged since
  until the next boot: a report names its devices now, and one whose
  devices are all gone is left out. An attach or detach waited for ever
  for another one's lock, and for every program it ran (a `dmsetup` or
  `nbd-client` hung on a device held every later run): the lock is waited
  for until the run's time is up (10 minutes by hand), naming who holds
  it, and a program is stopped after 120 s. `spaces attach --rw` of a pool
  on image files failed, "Read-only file system": its server's view of the
  system is read-only, and now lets it write the member images. The ready
  files of the servers are written whole and renamed into place (attach
  could read one empty). A `df` of a `refs mount` walked the medium
  allocator every time; its count is kept until the allocator changes
  (audit 2026-10-16, Low "Misc").
* `spaces attach` and `spaces check` checked every pool in one process: a
  pool whose checks allocated without bound or hung took the whole run,
  and every other pool, with it. Each pool is checked in a child process
  of its own now, all at once, with a memory limit (2048 MiB,
  `SPACES_POOL_MEMORY`) and a deadline (120 s, `SPACES_POOL_TIMEOUT`, within
  the attach units' time; none for `spaces check --deep` unless the
  variable sets one): such a pool fails alone, with a `space.checks`
  report naming the limit (audit 2026-10-16, E9). The devices of several
  pools can be given together; each pool is checked on its own.
* One reader of mountinfo fields for `spaces` and `refs` (the one of
  `spaces` overflowed on `\4xx`, which the kernel does not write) and one
  of block device attributes (audit 2026-10-16, "Duplicates that
  drifted").
* The packages and `contrib/install.sh` no longer list `ublk_drv` and `nbd`
  in `modules-load.d`: `systemd-modules-load.service` failed at every boot
  on a kernel without `ublk_drv`, and `spaces attach` loads a module when
  it needs one. `install.sh` removes the list an earlier version wrote.
  The Arch package names x86_64 only (aarch64 was never built). The user
  guide says what has been tried, and what to expect of SELinux, WSL2 and
  kernels before 6.0 (audit 2026-10-16, "Install story").

### Packaging

* The binaries, the deb and the Arch package ship the licence notices of
  everything the binaries link (`THIRD-PARTY-NOTICES.txt`: the Rust crates,
  the Rust standard library, musl; `tools/third-party-notices.py`). The deb's
  `copyright` is a DEP-5 file naming the copyright holder (it was the bare
  GPL text), its maintainer a project address (it was a placeholder), with
  `Installed-Size`, xz members that every dpkg reads (the zstd of 1.2.2
  needed dpkg 1.21.18) and a description that says what refs does now (it
  said read-only).
* `tools/build-static.sh` builds the static binaries the releases ship:
  without the build machine's paths (the source tree, the crates, the
  target directory and the toolchain's own sources, which the standard
  library's code names; 1.2.2's binaries named the crates and the
  toolchain's sources in the home directory), symbols stripped (the release
  profile); two builds of one tree in different directories give identical
  binaries, and none names the machine (`tools/repro-check.sh`).
  `SHA256SUMS` stays unsigned, and the user guide says so.
* CI: read-only permissions, actions pinned by commit, `cargo install`
  pinned and cached, pushes of `main` and tags only, shellcheck over
  `tools/` and `contrib/`, the tests of the feature `test-hooks`, a smoke
  run of the static binaries, and a deb build whose contents are checked.

### Testing

* The harnesses fail where something fails: the remote parts of the VM
  scripts run with `set -e` and an ERR trap that names the command (they
  went on past it), so every `dpkg -i` is checked; cases that could pass
  vacuously no longer do (`cycle` first sees a cycle planted into a copy of
  the unit, `readable` needs files to read, `nbd` all 40 idle connections,
  the boot check's `cycles` a journal to look in, `nbd` of the install case
  is skipped where the module cannot be unloaded first); `q()` of the
  management scripts and the round-trip runners fail with the command or
  the Windows run that failed (their summary said FAILED and they exited
  0); `tools/fuzz-long.sh` exits non-zero when a target does not run its
  time out. `tools/attach-crash-check.sh` keeps fwupd from running while it
  attaches (it held a ublk partition open and made a detach refuse).
* `tools/attach-crash-check.sh` case `children` (one attach of two pools,
  the second's checks made to allocate without bound, then to hang:
  test builds), `tools/rw-attach-check.sh` with `IMAGES=1` (a pool on image
  files attached read-write), the fuzz target `report_json` (the reports
  read back from a pool's child), and tests with the feature `test-hooks`
  (`cargo test -p spaces-cli --features test-hooks`).
* `tools/mount-vm-check.sh` cases `nosandbox` (`systemd-run` failing),
  `unitlog` (the unit's log after a mount) and `reportmode` (the report
  directory's mode). The write tests' scripts write their pattern with
  `spaces-pattern` (`SPACES_PATTERN` names it; `tools/guard-check.sh`
  copies it to the VM), `tools/mgmt-crash.sh` needs a build with
  `--features test-hooks` and says so.
* New corpus pools made by Windows (`tools/gen-corpus.sh`): `replacedpar`
  (a parity column rebuilt on a new disk), `dual7k4` (dual parity on
  4096-byte sectors), `tierodd` (an SSD tier over an HDD tier of three
  columns: Windows rounds the SSD tier to whole HDD rows), with fixtures
  of each; and ReFS volumes Windows grew (`r314extend`, left with a log
  record past its checkpoint, and `r314extendc`, the copy Windows replayed
  and took offline cleanly, also a fixture; `New-RefsVolume.ps1
  -ExtendMB`), and `r314fold` (`-Scenario casefold`: names Windows' upcase
  table and Unicode's upper case order differently, names folded to them,
  reserved names and names with a last dot or space, what Windows made of
  each in its manifest; also a fixture). The ReFS fixtures carry the
  upcase table's pages.

* `tools/refs-mount-check.sh --rw IMAGE` checks what writing through `refs
  mount --rw` does (the hold, listings while names go, open files whose name
  goes, the buffers' mode) on a copy of the image; the comparison with
  Windows' listings mounts with `--force`, so that the two volumes of the
  corpus taken with log records past their checkpoint, suspect since 1.2.1,
  are compared again. `tools/linux-vm.sh -sync` keeps a `testdata` link on
  the VM.
* The boundary values of the plausibility limits are tried in every field
  on every fixture (79, not 8 as in 1.2.2) and in the slot, checkpoint and
  data offsets of write-back cache and parity journal headers, their CRC
  sealed again, each case within 5 s (`crates/storage-spaces/tests/bounds.rs`).
* `zeroed_headers_of_hidden_structures_never_make_them_vanish` asserts
  what each zeroed copy of the header of a write-back cache, parity
  journal or dirty region log must give: never a skipped check; with
  another copy, a degraded space reading as before; without one, a failed
  space (suspect with a copy on a missing disk, a note when the header is
  zero everywhere and nothing follows it); and writing refused. Over the
  81 corpus pools: 216 copies, 207 degraded, 4 failed, 5 notes.
* The oracles compare each mutant's report with the undamaged pool's: a
  skipped check of a structure the database lists fails them, other
  skips are counted. A mutant over 3 s is timed again at once and is
  slow only if it is over 3 s again. `ORACLE_MODES` runs one mode,
  `ORACLE_ONLY` one mutant.
* Tests for the isolation of the gates of the servers, `export` and
  `refs` (devices that panic) and for `SPACES_CHECK_TIMEOUT`.
* The oracles ran again over the 81 corpus pools, 30,000 mutants per pool
  and mode: no panic; no wrong data under a healthy verdict where the pool
  database has three or more copies; no skipped check of a structure the
  database lists; no pool without a disk healthy. 67 mutants took over 3 s
  under the load of the run (reads waiting on the disk), and alone at most
  0.19 s; the runs that had them, again with less load beside them, one
  other, 0.1 s alone; none was slow twice. A third run of the whole
  campaign at 12 processes: the same results, and no mutant over 3 s at
  all (docs/security.md says what ran).

### Documentation

* refs(1) and `refs --help`: the names the writing commands refuse (a last
  dot or space, device names such as CON or COM1), the archive attribute
  they set, what `refs check` checks since 1.3 (keys twice, descriptor
  counts, reference counts, the upcase table's order of names), `clone`
  among the writing commands, `mount.ReFS`'s sandbox and its exit status 32
  when the sandbox cannot start, ENVIRONMENT (`REFS_TMPDIR`,
  `MOUNT_REFS_TIMEOUT`) and FILES (the unit's log in `/run/refs-mount`).
* spaces(8): `check` of several pools, each in its child with its memory
  limit and deadline, the lock and program time limits, `attach --rw` on
  image files, `disk add --wipe`, the servers (`serve-ublk`, `serve-nbd`,
  `serve-fuse`).
* The user guide and docs/security.md: the deadlines of `--deep` and
  `--bundle`, writers refused at once by a holder (not waiting), `--rw`
  servers writing member images, and the install table without the
  module list 1.3 no longer installs.
* The 1.1.1 section says it shipped with 1.2.0 and has no tag of its own.

## 1.2.2 (2026-10-17)

Fixes from a code audit of 1.2.1 (2026-10-16): checks that a hostile or
damaged disk could make panic, loop or exhaust memory as root at attach
time, verdicts that called damaged spaces healthy, names that reached
terminals raw, ways `refs` lost data, and documentation that said more
than the code does. Checked on Ubuntu 26.10 and 22.04
(`tools/guard-check.sh`, now with a hostile space beside a healthy pool;
`tools/mount-vm-check.sh`; `tools/package-vm-check.sh`, with an upgrade
from 1.2.1 and a space of 200 partitions), and across two restarts on
26.10 (`tools/boot-vm-check.sh`). The 24-hour fuzz run of every target was
not repeated for this hotfix: every target ran 4 hours (docs/security.md
says what ran).

### Fixes

* `spaces attach` checks every pool of the system in one process, as
  root, the moment a disk appears. A panic in the checks of one pool (on a
  damaged or hostile disk) ended the process, and the pools after it
  stayed detached; a loop kept them all waiting. Opening each pool and
  checking each space now run on a thread of their own: a panic fails
  that space (check `space.checks`), checks that take longer than 40 s
  make it suspect (`SPACES_CHECK_TIMEOUT` sets the limit in seconds, 0
  none), and the other pools are attached. `spaces check`, `spaces info`,
  the servers, `export` and `refs` refuse such a space or volume instead
  of panicking.
* Partition tables and boot sectors in a space whose numbers lie beyond
  any disk made the checks of attach overflow (a panic): a GPT entry of
  0xff bytes, a GPT header naming its entries at LBA 2^56, an NTFS boot
  sector in a partition smaller than its sector, a ReFS boot sector
  claiming 2^55 sectors, a ReFS superblock listing checkpoints at cluster
  2^52. They are suspect now, with where. A ReFS volume larger than its
  partition (or the rest of its device), or smaller than ReFS can be, is
  suspect too.
* Numbers in a pool's records far beyond anything Windows writes (a space
  of 2^31 columns or copies, an extent copy number of 2^64 - 1, a size of
  2^62 bytes) made the checks of attach loop for minutes or overflow, and
  the device-mapper table of a space was worked out row by row (2^32 rows
  for a space of 1 EiB). Such a space is refused now (`space.layout` failed,
  with the value; the limits are in docs/storage-spaces-format.md,
  "Plausible values"), and the table follows the extent runs.
* Work that grew with what a pool's metadata claims instead of what it
  holds: a parity journal can list a run of 2^30 stripes, which the check
  walked one by one (days); the health of the pool was worked out again
  for each of its spaces (1600 spaces: minutes); hidden spaces and
  out-of-date extents were searched again for every row and space. The
  check now takes the journal's ranges, the health is worked out once per
  pool, and the rest is looked up: a pool of 10 000 spaces with 40 000
  hidden ones is checked in seconds.
* Memory taken by what a disk claims: a write-back cache whose slot area
  (64 MiB at most) is full of entries of one-sector runs took 700 MiB to
  load and walked every run of a chunk for every read; its runs are kept
  as on disk now (244 MiB, and a read finds its run in log time), and the
  index of a cache may take 256 MiB at most. Five members with differing
  copies of a 64 MiB pool database took 370 MiB: only the copy used is
  kept now (124 MiB).
* ReFS: when the record before the checkpoint's log sequence number took
  more than one page, the checks read the volume's whole log (512 MiB to
  1 GiB) to find the newest record; they look back 64 pages for it now. A
  log the checks would have to read whole is read up to 2 GiB (a hostile
  control page may name 16 GiB); beyond that it is not read, a warning.
* `spaces attach` makes no devices for the partitions of a space with more
  than 128 of them (a table of 4096 entries cost two programs and a wait
  for udev each), waits for udev once for all partitions of a space, and
  waits for udev for 60 s at most in one run, so that the attach unit
  finishes in its time.
* The headers of a space's write-back cache, parity journal and dirty
  region log were read from the first copy of their mirror only, and one
  that did not check out was taken for no structure at all: zeroing one
  sector of a disk made a cache vanish (and the newer data only it held,
  15 corpus pools read other data), the verdict healthy, and `--rw` would
  have written past it. Every copy at hand is read now, and a valid one
  used: a damaged copy makes the space degraded, copies that disagree
  suspect, no valid copy (or slots without their header) failed, a
  header zero on every copy is a structure never written (as Windows
  leaves a new space's), and writing refuses all of these. The dirty
  region log says when one of its two header pages is lost. The guard no
  longer calls a parity space without a journal "not a parity space".
* Copies of the pool database of one sequence that differ were a warning,
  with advice saying Windows rewrites them (it does not): with one sector of
  the first copy of a two-disk pool zeroed, the copy read laid the space out
  on one disk instead of two, and the space was healthy. Copies are
  compared by what they say now: different meaning is suspect unless most
  copies hold the version read; a pool with one copy has nothing to compare
  it with (a note). An extent on a disk the database does not list (a read
  error later), one disk given twice or a clone of it beside it (healthy,
  reading the first; `spaces scan` kept only the first by name), and two
  records with one id are suspect, as is a slab mapped twice through two
  disk records of one device.
* Names from a disk (of pools, spaces, disks, files and streams of ReFS)
  reached terminals, report files (readable by every user) and the journal
  as they were: escape sequences could clear the screen or set a title,
  carriage returns and line breaks forge report lines, bidirectional
  overrides turn text around. Reports for people, refusals, plans, `spaces
  pool health` and `refs ls`, `stat`, `map` and `check` escape control and
  format characters as `\u{..}` now, and cut names at 1024 characters;
  `--json` keeps them, JSON-escaped.
* ReFS volumes (read by `refs` and its mounts, checked by `spaces
  attach`): a container table row naming a start near 2^64 made opening
  the volume panic; a name row with a key of 2 or 3 bytes, or resident
  data claiming a size near 2^64, made listing the directory or opening
  the file panic; an index node naming one page again and again had it
  read once per row (a few such levels: millions of reads), and `refs
  check` walked pages with no limit to depth or repetition and panicked on
  allocator or reference count rows whose clusters pass 2^64. Containers
  must now be of one size, inside the volume and apart; a tree's pages are
  read once per walk, each a level below the node naming it; such keys,
  sizes and rows are errors or problems of `refs check`.
* `refs mount`: a panic while serving a request ended the thread serving
  it, the mount with its last (after four, with `--rw` after the first),
  and the data of open files with it. The request fails with EIO now and
  the mount goes on; after a panic inside a change (`--rw`) every later
  request fails so, as the volume in memory may be half changed.
* ReFS writing: replacing a file's data (`refs write`, a file written
  whole through `refs mount --rw`) reset its record's counter of stream
  set ids, so the next named stream written in clusters (over 1 KiB) took
  the stream set of one the file had: both then read the newer one's
  bytes, and deleting either removed the other's rows, after which the
  file could not be opened. The counter is kept now, a new set takes an id
  past every set of the record, and `refs check` reports a key twice in a
  page or record and a stream whose set has no rows (it saw nothing of
  this before, nor did the fuzzing of the writer, which now writes and
  deletes named streams too).
* `refs mount --rw`: renaming a file over another to a name `refs` does
  not write yet (anything but printable ASCII: `mv -f tmp кириллица.txt`,
  the way editors save) deleted the other file, then failed; the rename is
  checked first now, and a refused one changes nothing. Closing a file
  opened for reading while it was open for writing made the writer's next
  write fail (EBADF) and could lose what it had written. A write-back that
  failed (a write error) was forgotten, and the next flush and the close
  succeeded; now it is kept, and every flush tries again and reports the
  error until it is written.
* ReFS `fs.refs.log`: a log whose control page was not found was a
  warning, so healthy; it is suspect now when the checkpoint shows records
  written to the log, a warning when it shows none, and the summary says
  where it looked. A read error at one place where the control page may be
  no longer ends the search. With two checkpoints of one clock, the checks
  took the second and the reader the first: both take the first.
* `spaces check` exited 0 when `--space` named no space or no pool was
  found, so that a script took it for healthy, and `--json` printed a
  sentence then: it exits 1 now, `--json` printing `[]`. `spaces attach
  --pool` or `--space` naming none of the spaces found exits 1 too
  (without them, as at boot, finding no pool is no error).
* `export` and the servers took the numeric id of a hidden space (a
  cache, a journal: `spaces info --all` lists them) and panicked; they take
  user spaces only. `spaces extents`, `info` and `check --bundle` (the
  bundle a bug report asks for) panicked on extents whose numbers lie
  beyond any disk (a physical slab of 2^36); they show them.
* Renaming a pool or a space to a name of 65 535 characters wrapped the
  name's length and made every copy of the pool database unreadable.
  Names are 1 to 256 characters (as `pool create` asked already), and every
  record a command writes must decode before it is written. Block devices
  are opened exclusively with the system's `O_EXCL` (a fixed 0o200 before,
  wrong on some architectures).
* A space whose layout is not understood made every space of its pool
  failed (`space.state`), since 1.2.0 worked out the health of a pool as a
  whole: the others are judged on their own again, as 1.1.0 did.
* `ruzstd` 0.9.1: with 0.9.0 a tiny ZSTD frame could exhaust memory
  (RUSTSEC-2026-0344) when `refs` read a compressed file of a hostile
  volume.

### Testing

* Two fuzz targets: `guard_userdata` (the guard on whatever a space
  holds: its first and last 64 KiB, with the NTFS and ReFS checks of
  attach) and `policy_fields` (record fields at the edges of their types,
  then health, opening, the journal check, the device-mapper table, a read
  and the guard, under a watchdog); seeds and dictionaries from the
  audit's cases (`fuzz/seeds.py`, `fuzz/dicts`, which `tools/fuzz-long.sh`
  passes on).
* `crates/refs/tests/oracle.rs`: the audit's oracles over the corpus,
  in a small budget (more with `ORACLE_MUTANTS` and `ORACLE_POOLS=all`):
  damage where the library reads a pool (or what the guard reads of its
  spaces) changes nothing the guard calls healthy, and a pool without one
  or two of its disks is never healthy and reads as whole.

### Documentation

* Turning off attaching at plug-in with the packages: their udev rule is
  in `/usr/lib/udev/rules.d`, where removing it does not last; override it
  with a link to `/dev/null` in `/etc/udev/rules.d`, or mask the unit
  (user guide, spaces(8) FILES, which lists both rule locations and the
  report and udev directories). spaces(8) and the user guide list every
  check id; `attach --degraded` and `--force` say what 1.2.0 made them;
  refs(1) shows `mount --rw`.
* The user guide says that a pool's spaces served by processes attach one
  at a time until 1.3, that the attach unit drops capabilities by a
  deny-list (CAP_SYS_ADMIN and others stay), that `--unclean-parity data`
  is for reading, and that `disk add --wipe` clears nothing beyond the
  partition table it writes.
* docs/security.md: `refs` writes when asked and runs as root for
  udisks2, ReFS volumes root mounts are open to every user, what is
  bounded and what is not, and the fuzz runs as they were.

## 1.2.1 (2026-10-11)

Fixes from a user's report of 1.2.0 on Ubuntu 26.10: the attach unit
disturbed every boot, the log of ReFS volumes of several TiB in spaces was
not found, detaching a space in use broke its mounts, and upgrades left
spaces on the old program. Checked on Ubuntu 26.10 and 22.04
(`tools/package-vm-check.sh`), and across a restart on 26.10
(`tools/boot-vm-check.sh`, which fails on 1.2.0).

A copy of the attach unit made as a workaround in
`/etc/systemd/system/storage-spaces-attach.service` overrides the
package's: remove it, then run `systemctl daemon-reload`.

### Fixes

* Boot: the attach unit of 1.1.0 and 1.2.0 made an ordering cycle at
  every boot, which systemd broke by dropping jobs at random: mounts of
  snaps or `/boot/efi`, the setup of `/tmp` (after which logging in to
  GNOME failed), or the attach itself. `PrivateTmp=yes` orders a unit
  after `systemd-tmpfiles-setup.service`, which comes after
  `local-fs.target`, while the unit runs before `local-fs-pre.target`.
  The unit and the servers it starts get a `/tmp` of their own through
  `TemporaryFileSystem=` now (systemd 238); the servers needed it too, or
  at boot they would have waited for the attach that waits for them.
  Running that early, attach could hang in `udevadm trigger --settle`
  while udev worked through the coldplug, until the unit timed out: it
  runs after the coldplug now and waits for udev no longer than 30 s.
* ReFS: on volumes of several TiB the log is 1 GiB from page 0x8000, its
  control page right after it, where `refs` did not look: the checks said
  `fs.refs.log: the log cannot be read`, changes past the checkpoint would
  have gone unnoticed there, and writing failed with "no log control
  page". The log is now found after regions of 512 MiB to 2 GiB from
  container 7 (or from where the volumes seen so far start it), of its
  control page's two copies the newer is read, and writing that cannot
  read the log says so. Such volumes that Windows left with changes in
  the log (fast startup, hibernation) are suspect now, as others are.
* ReFS: checking the log reads the record before the checkpoint and the
  pages after it, not the whole log (512 MiB or 1 GiB): a record's page
  follows from its sequence number. A few pages instead of up to 64 reads
  of 16 MiB for `refs check --quick`, `spaces check`, each mount and
  attach, and `refs info`.
* `spaces detach` of a space whose volume was mounted stopped its server
  anyway (so did removing the package): the mount then gave read errors.
  It refuses now, naming the mount, and changes nothing.
* Upgrading left the spaces attached before on servers running the old
  program until they were detached and attached again (from 1.1.0 that
  kept its cache bug). After an upgrade the packages and
  `contrib/install.sh` detach those nothing uses (`spaces detach
  --outdated`) and attach them with the new program; the others keep the
  old server, as the upgrade says. The Arch package attached nothing
  after an upgrade.
* The attach unit hit systemd's start limit at boot, as udev starts it
  for every member disk that appears: a disk that came a little later
  would not have been attached. It has no start limit now, and the udev
  rule no longer starts attach for the devices of attached spaces.

## 1.2.0 (2026-10-07)

A health guard: before a space is attached (also at boot) or a ReFS
volume is mounted (also by `mount -t ReFS` and udisks2), it is checked,
and only what is healthy is attached and mounted without being asked. A
degraded space is read only with `--degraded`, a suspect one only with
`--force` (a ReFS volume with `-o force`), read-only either way, a failed
one not at all, and writing needs a healthy verdict. Every problem comes
with a report: each check with its evidence (where, what was expected and
what was found), where the truth comes from and what to do. Checked
against the whole corpus (no space Windows left healthy gets another
verdict), with damaged copies of each kind, and on Ubuntu 26.10 and 22.04
(`tools/guard-check.sh`).

### Health guard

* `spaces check` checks the spaces of every pool found (or of the pool
  whose devices are given) and prints a report per space: each check with
  its status, the evidence (the device and offset, LBA, slab, disk and
  physical offset; what was expected and what was found, in hex), where
  the truth comes from and what to do, and the verdict (healthy,
  degraded, suspect, failed). `--json` for programs, `--deep` for the
  checks that read whole spaces, `--bundle FILE` for a bug report (the
  metadata, no file data). It checks the pool's quorum, disks and
  database copies, Windows' state of the space, its slab map, write-back
  cache, parity journal and dirty region log, both copies of its
  partition table, and the file systems in it: the NTFS boot sector and
  its copy, the ReFS boot sector, superblock copies, checkpoints and log.
* `spaces attach` checks every space first: only healthy spaces are
  attached without being asked (also by the udev rule and the attach
  unit), degraded ones (redundancy reduced) with `--degraded` and suspect
  ones (our checks disagree) with `--force`, both read-only; failed ones
  (data lost, metadata not understood) not at all; `--rw` only healthy
  ones. The report of each space goes to
  `/run/storage-spaces/reports/<space guid>.txt` and `.json`, and for a
  refused space to standard error (under the attach unit a line for the
  space and one per failing check, into the journal). `spaces status`
  shows the verdicts and the spaces not attached, `spaces info` the
  verdict of each space. The servers (`serve-ublk`, `serve-nbd`,
  `serve-fuse`) and `export` follow the same rule. Changed: a pool with
  missing disks whose data is partly lost is no longer attached with
  `--degraded`; a pool with no more than half of its database copies
  (Windows takes it read-only) needs `--force` now, also with half of
  its disks.
* udev knows the devices of attached spaces: attach writes their
  properties (SS_SPACE, SS_POOL, SS_VERDICT, SS_FORCED) for the rule to
  import, and the rule hides from udisks2 the devices of a space attached
  past its verdict (no desktop mounts them unasked), and the ublk, nbd or
  loop device a space is served through with its kernel partitions
  (`ublkb0p2` and the like), which showed in file managers next to the
  space's own `ss-*-pN` devices.
* `refs mount` runs the quick checks of the volume first (`refs check
  --quick`: boot sector, superblock and its copies, checkpoints, log) and
  mounts a volume that is not healthy only with `--force`, read-only
  (`mount -t ReFS -o force`); a refused mount says the first failing
  check and where its report is (udisks2 shows "Unknown error" for any
  refusal of a mount helper and keeps that line in its journal). A
  volume on a space `spaces attach` attached gets the space's verdict
  too. `refs ls`, `cat`, `stat` and `info` read such a volume with a
  warning; the writing commands refuse it.
* Opening a space refuses metadata that breaks what the format implies
  (a slab mapped twice, extents or log entries outside the space, journal
  entries that do not cover their run), naming the values.

### Fixes

* Metadata whose spaces' parent links form a circle made `spaces pool
  health` (and the checks) walk on until memory ran out; each hidden
  space is visited once now (found by fuzzing the checks).
* Reading the log of a ReFS volume (`refs info`, the writing commands,
  the checks) takes about 40 reads instead of over 600: on a slow device
  the number of reads counts.

## 1.1.1 (2026-10-07)

Not released on its own: these fixes shipped with 1.2.0, and there is no
tag v1.1.1 (the release notes of 1.2.0 carry this section).

Fixes from a user's report of 1.1.0 on Ubuntu 26.10 (systemd 261, AppArmor
confining fusermount3, three pools of SSDs and hard disks whose spaces have
4 KiB sectors): reading such spaces when Windows left data in their
write-back cache, mounting ReFS volumes with `mount -t ReFS` and udisks2 on
current distributions, and attaching right after installing. Each fix was
checked on Ubuntu 26.10 and 22.04 (`tools/mount-vm-check.sh`,
`tools/package-vm-check.sh`).

### Fixes

* Spaces with 4 KiB logical sectors and a write-back cache (Windows uses
  one for spaces on SSDs) read stale data where the cache held only part
  of a chunk: the cache counts its runs of valid sectors in the space's
  logical sectors, and 1.1.0 took them as 512-byte sectors. It showed as
  "Alternate GPT is invalid" when such a space was attached, and data
  written last in Windows could read as older data. Every byte of such a
  space now reads as Windows reads it (pool `wc4k`, checked against
  Windows' own reads). Writing to such spaces made the same mistake; a
  cache 1.1.0 wrote that way is now refused instead of misread.
* `mount -t ReFS`, fstab and udisks2 mounts of ReFS volumes did not show
  on systemd 254 and later (`mount.ReFS` gave up after 10 s, udisks2 said
  "Unknown error"): there, the private network of the unit `refs` runs in
  brought a private mount namespace along. The unit keeps the host's
  mounts now (`PrivateMounts=no`).
* `mount.ReFS` gave up after 10 s, without a word, on volumes that take
  longer to open (a 10.5 TiB volume on two hard disks after a restart).
  `refs mount` now reports its progress and the outcome in a status file,
  and `mount.ReFS` waits for that instead of a fixed time: it returns once
  the mount is in place, or says why it is not, and shows on a terminal
  how much has been read. `MOUNT_REFS_TIMEOUT` sets a limit.
* Mounting a ReFS volume on a device with 4 KiB logical sectors as root
  (`mount -t ReFS`, udisks2, `refs mount` of a block device; every ReFS
  volume in a space with 4 KiB sectors) failed with "Invalid argument":
  the mount's block size was FUSE's default of 512 bytes, smaller than
  the device's sectors. It is the device's sector size now.
* On Ubuntu releases that confine `fusermount3` with AppArmor (25.10 and
  later), root's mounts of ReFS volumes were refused ("Permission
  denied"): the profile knows neither the mount type `fuseblk.refs` nor
  udisks2's mount points under `/run/media`. As root, `refs` now makes the
  mount itself (mount(2), with the /dev/fuse connection handed to the FUSE
  session) and no longer runs `fusermount3`; the mount goes away when
  `refs` ends, also when it is killed (a watcher process unmounts it).
* Installing the package or running `contrib/install.sh` left the pools
  whose disks were present unattached until a restart: the udev rule only
  acts on disks that appear. Both start the attach once now, and the
  package loads `nbd` as well.
* Removing the package detached every attached space, also those another
  installation's `spaces` serves (`contrib/install.sh`'s); it detaches
  only those its own `spaces` serves now.
* Without root, `spaces status` said "no spaces attached" when spaces were
  attached (the attach unit's umask made `/run/storage-spaces` 0700), and
  `spaces scan` said "no Storage Spaces pool members found" when it could
  not open the disks. The state directory is readable by everyone now
  (0755, its files 0644; the NBD sockets stay 0600), an unreadable one is
  an error that says to run as root, and `scan` says how many devices it
  could not open.
* Two pools of the same name (Windows calls every new pool "Storage
  pool") with spaces of the same name could not be attached together:
  the second failed with "device-mapper device ... already exists
  (left over from an interrupted run?)". Its devices get the first eight
  hex digits of the pool's GUID after the name now
  (`ss-<pool>-<space>-<guid>`); `spaces detach <space>` finds them too.
* The Arch package attaches present pools after installing, loads `nbd`,
  and on removal detaches only what its own `spaces` serves, as the
  Debian package now does.
* The attach unit's `Documentation=` pointed to an old address of the
  project; it names `man:spaces(8)` and this repository now. The user
  guide notes that building needs only the libclang library with
  `LIBCLANG_PATH`, not `libclang-dev`.

## 1.1.0 (2026-10-05)

ReFS: `refs` reads, mounts, checks and (3.14) writes ReFS volumes, checked
against what Windows makes. Storage Spaces itself is unchanged apart from
the hardening below.

### ReFS

* `refs` (new command, `man refs`): `ls`, `cat`, `stat`, `mount` (FUSE,
  read-only or `--rw`), `check`, `info`, and the writing commands `set`,
  `overwrite`, `write`, `create`, `rename`, `move`, `link`, `clone`,
  `delete`, `mkdir` (each only with `--yes`). It works on volumes on disks,
  images and spaces of pools (`--space`).
* Reads: files (resident, in extents, sparse, block-cloned, deduplicated,
  compressed with LZ4 or ZSTD), directories of any size, named streams,
  stream snapshots, hard links, symbolic links and junctions, attributes
  and times; integrity streams checked on every read; 4 KiB and 64 KiB
  clusters, CRC-64 and SHA-256 metadata checksums.
* Versions, each verified against volumes the Windows that makes them
  created (every file as Windows lists it, `refs check` clean, fixtures in
  CI): 3.14 (Windows 11), 3.7 (Server 2022), 3.4 (Server 2019), 3.1
  (Server 2016), 1.2 (Server 2012 R2). Not verified, for lack of Windows
  to make them: 1.1, 2.x, 3.2, 3.3, 3.5, 3.6, 3.8 to 3.13.
* Writes, 3.14 only (every change checked by Windows: `refsutil leak` and
  `triage`, and Windows reading and changing the result): files up to 64
  GiB changed in place as Windows does, created, renamed, moved, linked,
  deleted (block-cloned and deduplicated files too), directories, times,
  attributes, named streams, integrity streams, files with stream
  snapshots, block clones (also whole-file copies through the mount),
  compressed files (written whole into ordinary clusters). Other versions
  are refused.
* `refs mount`, also for udisks2 and `mount -t ReFS` through
  `contrib/mount.refs`; `refs check` checks pages, allocators and shared
  clusters (not the allocators of 1.x).
* The format is described in `docs/refs-format.md`, as far as it was found.

### Hardening (code audit, `docs/security.md`)

* The attach unit, the servers it starts and `mount.ReFS` run with limits
  (no new privileges, read-only file system, no namespaces or network,
  fewer capabilities): `systemd-analyze security` 9.5 UNSAFE down to 5.1
  MEDIUM; on a systemd that does not know a property the servers start
  without limits, with a warning.
* Hostile volumes: bounds on ReFS 1.x run rows and cluster sizes, on the
  unit size of compressed containers, on what `refs write --append` holds
  in memory and on the chunk size of a pool's write-back cache; the FUSE
  mount forgets inodes and bounds its caches, and survives a panic in one
  request.
* Release builds check integer overflow (no measurable cost); the four
  crates forbid `unsafe` code; CI runs `cargo deny` and `cargo audit` on
  every push and every week.
* New tests: damaged volumes of every ReFS version, random bytes to the
  parsers. Fuzzing: 4-hour runs of the ReFS targets after each change, all
  clean; the 24 h run of every target was not repeated for this release
  (the Storage Spaces parsers changed in two lines).

### Installing

* `contrib/uninstall.sh`; the Installing section of the user guide says
  what is needed and what each way of installing gives.
* The Debian package and `contrib/install.sh` include `refs`,
  `mount.ReFS` and the man pages; the package recommends `fuse3`.
* Assets: static `spaces` and `refs` (x86_64, musl), the `.deb`, and
  `SHA256SUMS`.

## 1.0.0 (2026-10-02)

Pool management, the way Windows 11 24H2 does it: Windows takes pools
created and changed on Linux as healthy, repairs and optimizes them.

### Management

* `spaces pool create`: a pool (version 28) of blank disks (`--wipe` for
  others), 512-byte or 4 KiB logical sectors, 512e and 4Kn disks; the
  partition tables, disk headers and pool database Windows would write.
* `spaces space create`: simple, two- and three-way mirror and single
  parity spaces, fixed and thin, with Windows' defaults (columns,
  interleave, the parity space's write-back cache and journal, the mirror's
  dirty region log); `space delete`, `rename`, `resize` (growing).
* `spaces disk add` (also to a pool missing a disk), `disk set` (media
  type, usage), `disk retire` (everything moved off the disk), `disk
  remove` (a retired disk, or a missing one after a repair).
* `spaces pool rename`, `pool remove`, `pool repair` (copies on missing or
  out-of-date disks rebuilt elsewhere: mirror copies copied, parity columns
  recomputed), `pool optimize` (extents spread over the disks), `pool
  scrub` (mirror copies and parity compared, `--repair` makes them agree),
  `pool health` (HealthStatus and OperationalStatus as Windows would show
  them).
* Every command prints its plan and writes only with `--yes`, in steps
  that each end with a flush; data is copied before the metadata points at
  it, so a crash at any point leaves a pool that opens (replayed after
  every step in the tests; Windows took pools cut short this way as
  healthy).
* Member disks are opened exclusively for writing: O_EXCL on block devices
  (also through symlinks), an exclusive lock on image files.
* Checked byte for byte against what Windows writes for every operation it
  exposes, by Windows round trips of pools created and changed on Linux,
  and by fio and NTFS write checks on spaces created on Linux.
* `spaces tier create` (tier templates, `New-StorageTier`) and `spaces space
  create --tier`: tiered spaces of an SSD mirror over an HDD simple or
  parity tier (mirror-accelerated parity), with their cache, dirty region
  log and parity journal on the SSD disks.
* Not supported: creating dual parity spaces; writing tiered spaces; new
  spaces in pools of version 29 (Insider builds).

### Known issue in Windows

* Windows 11 24H2 bugchecks (0x50 in spaceport.sys) when a pool of four
  disks with simple, mirror and parity spaces arrives with one disk absent,
  whether Linux or Windows created the pool.

## 0.2.0 (2026-09-30)

Writing, for spaces whose state is fully understood.

### Writing

* `spaces attach --rw` (and `serve-ublk --rw`, `serve-nbd --rw`): simple,
  two- and three-way mirror and single parity spaces, written the way
  Windows writes them, so that Windows reads the result back (healthy,
  chkdsk clean, every file intact in the round trips of the test suite).
  Refused, with the reason: pools that are not clean (missing disks, stale
  copies, diverging metadata), degraded spaces, dual parity, storage tiers,
  write-back caches whose copies disagree.
* Mirror spaces: the dirty region log lists an extent run, durably, before
  its first write.
* Single parity spaces: writes go through the write-back cache as Windows'
  own do (whole stripes the parity journal records as not consistent go to
  the space directly); destaging rewrites whole stripes under the journal,
  so a stripe always matches its parity or is held whole by the cache: the
  write hole stays closed.
* Thin spaces (256 MiB allocation units) allocate rows as Windows does:
  one pool database update per row, on every member before the data;
  parity rows when the cache destages them. The pool database grows by
  pages of 64 slots when full.
* Discards (TRIM) give rows of thin simple and mirror spaces that they
  cover whole back to the pool, as Windows does (ublk and NBD).
* A pool database copy left stale by an interrupted update is brought up
  to the newest copy when a space is opened for writing, as Windows does.
* The cache and journal logs wrap behind checkpoints as Windows expects;
  a cache holding data when a space is opened for writing is destaged first.
* Writes are durable after a flush (sync, FUA) or a clean detach; the
  serving processes flush when stopped.
* Checked by crash replays of every flush point and of unordered writes,
  fio with verification through ublk, NBD and device-mapper, and NTFS
  written on Linux and verified by Windows.
* The write path is fuzzed (`pool_write`): whatever the metadata, a space
  is written only if it is understood, and writes stay inside the pool
  partitions of its members.

### Reading

* The write-back cache and parity journal are read as Windows reads them:
  from the newest checkpoint on (a wrapped log is no longer read in full).
* `spaces dump` shows checkpoints.

## 0.1.0 (2026-09-28)

First release: read-only access to Microsoft Storage Spaces pools created by
Windows 11 (24H2, pool version 28, and Insider builds, pool version 29).

### Reading

* Simple, two- and three-way mirror, single parity, dual parity (Reed-Solomon
  over GF(16) for 7 to 10 columns, local reconstruction code for 11 and more
  columns), storage tiers and mirror-accelerated parity.
* Fixed and thin provisioning, several spaces per pool, fragmented and
  extended spaces, 512-byte and 4 KiB logical sectors, 512e and 4Kn member
  disks, interleave 16 KiB to 1 MiB.
* The per-space write-back cache, including data not yet moved to the space.
* Missing or failing disks as far as the redundancy allows (any two for dual
  parity); out-of-date mirror copies are never read; quorum check.
* Unclean shutdowns: parity stripes the journal does not mark consistent
  are checked, and so are the copies of mirror extent runs the dirty region
  log lists (those written since the space was last disconnected); stripes
  whose parity does not match and rows whose copies differ are refused
  unless `--unclean-parity data` is given.

### Block devices

* `spaces scan`, `attach`, `detach`, `status`, with device-mapper, ublk, NBD
  and FUSE backends; `/dev/mapper/ss-<pool>-<space>` and one device per
  partition.
* udev rule and systemd unit: pools are attached at boot and when their disks
  appear.
* `spaces info`, `extents`, `export`, `dm-table` for inspecting pools without
  attaching them; `spaces dump` and `diff` print the metadata one fact per
  line and compare two states of a pool.

### Packaging

* Debian package script, Arch Linux PKGBUILD, `contrib/install.sh`, static
  musl build in CI, manual page `spaces(8)`.

### Verification

* Every statement of `docs/storage-spaces-format.md` marked verified is
  checked by tests against pools created by Windows: metadata equal to
  `Get-PhysicalExtent`, a verification pattern read back through every
  backend on Linux, NTFS pools with file checksums recorded on Windows, and
  crash experiments compared with the content Windows shows after recovery.
* Metadata and data fixtures from those pools run in CI, with cargo-fuzz
  targets for the parsers.

### Not supported yet

* Writing, pool management (stages 2 and 3 of `docs/plan.md`).
* Pools created by Windows 8, 10 or Windows Server.
* ReFS.
