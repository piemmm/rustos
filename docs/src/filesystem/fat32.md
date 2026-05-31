# FAT32 driver

`drivers/filesystem/fat32` (`rustos-drv-fs-fat32`) is the read-only
FAT32 driver. It is the first block-backed `drivers/filesystem/*`
crate, chosen first because FAT32 backs the EFI system partition and SD
cards (`AGENTS.md` §11).

It reads a FAT32 volume sitting behind any
[`Block`](../abi/driver_traits.md#block) device and exposes it through
the versioned [`FilesystemRead`](../abi/driver_traits.md#filesystem)
trait. The frozen `Filesystem` trait carries only `mount`/`unmount` and
a `DriverHandle` and cannot perform I/O, so the read surface is a new
versioned trait rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9).

## What the driver does

| Operation             | FAT32 mechanism                                   |
| --------------------- | ------------------------------------------------- |
| `open`                | Validate the boot sector (BPB) and compute layout.|
| `root`                | The root directory at the BPB's root cluster.     |
| `lookup`              | Scan a directory's entries for a name.            |
| `node_info`           | Report `{ kind, size }` from the node token.      |
| `read_at`             | Walk the FAT cluster chain and copy file bytes.   |
| `read_dir`            | Yield a directory's entries in on-disk order.     |

## Long file names (VFAT)

Each directory entry exposes a **single** name. When a valid long-name
set precedes the 8.3 short entry — its sequence is contiguous, its
`0x40` last-logical fragment is present, and its checksum matches the
short name — the driver reconstructs that long name; otherwise it uses
the 8.3 short name (so a volume written without long names stays fully
readable). When a long name is present the internal 8.3 alias is *not*
separately resolvable.

Names are returned as **UTF-8**: the long name's UTF-16LE code units
(including surrogate pairs) are decoded, and the driver falls back to
the short name on any malformed set — an unpaired surrogate, an invalid
scalar value, or a checksum mismatch — rather than surfacing a partial
name.

A `NodeId` is self-describing: it packs the entry's first cluster, a
directory flag, and (for files) the size, so `node_info` needs no extra
I/O and no in-memory inode table.

## FAT type detection

The driver identifies FAT32 by the boot-sector shape — a zero 16-bit
FAT size and a zero root-entry count — rather than by re-deriving the
cluster-count threshold. A FAT12/FAT16 volume has non-zero values in
those fields and is rejected with `DriverError::BadMagic`, so the
distinction is exact for the volumes this driver accepts.

The device logical-block size and the FAT bytes-per-sector are
decoupled: every access is staged one logical block at a time, so the
two sizes need not match.

## Permissions

FAT32 stores no owner, mode, ACL, or capability gate. Those live in the
VFS metadata layer ([Permissions](./permissions.md), `AGENTS.md` §5.3);
the driver makes no permission decisions (§5.4 — the VFS is the policy
point, the driver is raw structural I/O). Case-folding and Unicode
normalisation policy likewise belong to the VFS.

## Limitations

- **Read-only.** Writing waits on a `FilesystemWrite` trait, tracked in
  `PLAN.md` Stage 5; the per-driver crate `README.md` records the same
  caveat next to the code.
