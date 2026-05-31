# FAT32 driver

`drivers/filesystem/fat32` (`rustos-drv-fs-fat32`) is the read/write
FAT32 driver. It is the first block-backed `drivers/filesystem/*`
crate, chosen first because FAT32 backs the EFI system partition and SD
cards (`AGENTS.md` §11).

It reads and writes a FAT32 volume sitting behind any
[`Block`](../abi/driver_traits.md#block) device and exposes it through
the versioned [`FilesystemRead`](../abi/driver_traits.md#filesystem) and
`FilesystemWrite` traits. The frozen `Filesystem` trait carries only
`mount`/`unmount` and a `DriverHandle` and cannot perform I/O, so each
I/O surface is a new versioned trait rather than a widening of the
shipped one (`AGENTS.md` §2.4 / §9).

## What the driver does

| Operation             | FAT32 mechanism                                   |
| --------------------- | ------------------------------------------------- |
| `open`                | Validate the boot sector (BPB) and compute layout.|
| `root`                | The root directory at the BPB's root cluster.     |
| `lookup`              | Scan a directory's entries for a name.            |
| `node_info`           | Report `{ kind, size }` from the node token.      |
| `read_at`             | Walk the FAT cluster chain and copy file bytes.   |
| `read_dir`            | Yield a directory's entries in on-disk order.     |
| `create`              | Write a new LFN set + 8.3 entry; alloc a dir cluster for a directory. |
| `write_at`            | Extend/chain clusters, zero-fill gaps, write bytes, update the entry. |
| `truncate`            | Free the tail chain (shrink) or zero-extend (grow); update the entry. |
| `remove`              | Free the cluster chain and mark the entry + its LFN run deleted. |
| `flush`               | No-op: every mutation writes straight through to the device. |

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

## Writing

Writes address their target as a `(dir, name)` pair, because a FAT file's
length and starting cluster live in its **parent directory entry**, not
in a self-describing `NodeId` (`AGENTS.md` §2.4 — the symmetric counter
to `FilesystemRead`). Each created entry is written as a VFAT long-name
set bound to a generated, directory-unique `~N` 8.3 short alias, so an
arbitrary, case-preserving name round-trips through a later read. Free
clusters are found by scanning the FAT; directories grow by one zeroed
cluster at a time when their entry slots are exhausted; and every FAT
mutation is mirrored across all FAT copies. Sub-block writes are
read-modified-written so neighbouring bytes are preserved.

## End-to-end QEMU vertical

`tests/integration/fat32_virtio_blk_pci_x86_64` exercises the driver
against a **real (emulated) virtio-blk-pci device** under QEMU. It boots
the production kernel pipeline, brings the block device online through
the same shared bring-up the virtio-blk vertical uses, then mounts a
planted FAT32 volume through `Fat32::open`, verifies the planted file
reads back its known contents, and creates + writes + reads back a fresh
file before signalling success.

The on-disk image is built by the shared `rustos-test-fat32-image`
fixture (a 1 MiB volume, two mirrored FATs, one-sector clusters). The
host harness (`cargo xtask test --qemu`) plants exactly that image on the
backing disk, and the freestanding guest tail names the same planted and
to-be-written files through the fixture's constants, so the two sides
share one source of truth (`AGENTS.md` §2.2). The device tail
(`fat32_round_trip`) is generic over the virtio transport, so a riscv64
MMIO sibling runs identical code.

## Permissions

FAT32 stores no owner, mode, ACL, or capability gate. Those live in the
VFS metadata layer ([Permissions](./permissions.md), `AGENTS.md` §5.3);
the driver makes no permission decisions (§5.4 — the VFS is the policy
point, the driver is raw structural I/O). Case-folding and Unicode
normalisation policy likewise belong to the VFS.

## Limitations

- The driver writes through to the block device synchronously; there is
  no in-memory write-back cache or journal (FAT has no journal). A
  generated short alias uses a `~N` numeric tail rather than the legacy
  6-character-plus-hash scheme; collisions are still resolved against the
  live directory, so on-disk uniqueness holds.
