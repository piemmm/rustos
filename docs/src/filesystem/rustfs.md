# rustfs driver

`rustfs` (`drivers/filesystem/rustfs`, crate `rustos-drv-fs-rustfs`) is the
**native RustOS filesystem**: a block-backed, journaled, copy-on-write
filesystem that stores full POSIX metadata plus an inline access-control
list and an optional capability gate **per inode** (`AGENTS.md` §5.3). It
sits behind any `rustos_abi::driver::block::Block` device and is exposed
through the versioned `FilesystemRead` and `FilesystemWrite` traits — never
by widening the frozen mount/unmount `Filesystem` trait (`AGENTS.md` §2.4 /
§9).

The driver **stores** each inode's owner, mode, ACL, and capability gate
but makes **no** permission decision itself: the VFS is the policy point
(`AGENTS.md` §5.4). The stored record is read back through the versioned
`FilesystemSecurity` trait (`security(node) -> NodeSecurity`) and written
through `RustFs::set_security`. Because `rustfs` implements
`FilesystemSecurity`, the kernel host delegates to it through the VFS's
`*_via_secured` operations, which judge each node against its **own**
stored §5.3 record (`Metadata::from_node_security`) rather than a uniform
mount-point template — so an owner-only or capability-gated file is
enforced as stored. See [Driver delegation](./overview.md) and the
[driver-trait reference](../abi/driver_traits.md).

## On-disk layout

A volume is a sequence of fixed-size blocks (the device's logical block
size, between 512 and 4096 bytes, a power of two). The regions tile the
device in order; `RustFs::open` re-derives and validates the geometry from
the superblock:

| Region        | Contents                                                    |
| ------------- | ----------------------------------------------------------- |
| Superblock    | Block 0: magic, version (2), geometry, region offsets, root.|
| Inode table   | Fixed 256-byte inode records; index 1 is the root.          |
| Data bitmap   | One bit per data block.                                     |
| Journal       | One header block plus a fixed-size redo-log data area.      |
| Data          | File and directory data blocks, and indirect-pointer blocks.|

Each inode holds 12 direct block pointers plus one single-indirect block,
so a file spans up to `12 + block_size/8` blocks. Directories are ordinary
block-addressed payloads of 64-byte slots (`inode`, `name_len`, name);
`.` and `..` are stored on disk and hidden from `read_dir`.

The inode record also stores the four §21 timestamps; format version 2
reduced the direct-pointer count from 16 to 12 to make room for them
without growing the fixed 256-byte record. A version-1 volume is refused
rather than misread.

## Timestamps (§21)

Every inode stores four 64-bit-native `Time64` timestamps —
`created`, `modified`, `accessed`, and `changed` — so absolute time is
never a seconds-only scalar and the full pre-1970 / post-2038 range
round-trips without truncation (`AGENTS.md` §21). They are surfaced
through the versioned `FilesystemTimestamps` trait
(`times(node) -> NodeTimes`), a separate `abi-v1` extension alongside
`FilesystemSecurity` — never a widening of `FilesystemRead` /
`FilesystemWrite` (`AGENTS.md` §2.4 / §9).

The driver stamps them from a clock seam installed with
`RustFs::with_clock(clock: fn() -> Time64)`; without it every stamp is
the Unix epoch, so a board with no wall clock yet keeps deterministic,
in-range timestamps rather than panicking or inventing a time
(`AGENTS.md` §2.9). The stamping follows the POSIX model:

- **create** sets all four to the creation instant and bumps the parent
  directory's `modified`/`changed`;
- **write** advances `modified`/`accessed`/`changed`;
- **truncate** advances `modified`/`changed`;
- **set_security** advances only `changed` (a metadata change);
- **remove** bumps the parent directory's `modified`/`changed`.

`created` is set once and never changed. Installing a different clock
never rewrites timestamps already on disk.

## Copy-on-write and journaling

`rustfs` keeps metadata and data consistent across a crash without
`fsck` (`AGENTS.md` §2.5):

- **File data is copy-on-write.** A write allocates a *fresh* data block,
  writes the new contents there, re-points the inode, and frees the old
  block. A crash before commit leaves the old block intact, so a reader
  never observes a torn block.
- **Metadata is journaled.** The data-block bitmap, inode-table blocks,
  directory blocks, and indirect blocks of a single operation are staged
  into the journal's redo-log area. Commit writes a checksummed record,
  then checkpoints each staged image to its home block. A mount **replays**
  a committed-but-un-checkpointed transaction and **discards** an
  uncommitted or checksum-mismatched one, so the metadata is always at a
  transaction boundary.

The staged block images live in the on-disk journal, not in RAM; only the
small list of home block numbers is held in memory.

## Operations

`FilesystemRead` provides `root`/`node_info`/`lookup`/`read_at`/`read_dir`;
`FilesystemWrite` provides `create`/`write_at`/`truncate`/`remove`/`flush`,
addressing a target as a `(dir, name)` pair. `write_at` extends files
(zero-filling sparse gaps), `truncate` shrinks (freeing the tail and
copy-on-write zeroing the partial last block) or grows, and `remove`
refuses a non-empty directory with `Busy`. A `NodeId` is the inode index;
node identity is stable across a remount.

## End-to-end QEMU vertical

`tests/integration/rustfs_virtio_blk_pci_x86_64` exercises the driver
against a **real (emulated) virtio-blk-pci device** under QEMU. It boots
the production kernel pipeline, brings the block device online through
the same shared bring-up the virtio-blk and FAT32 verticals use, then
mounts a planted rustfs volume through `RustFs::open`, verifies the
planted file reads back its known contents, and creates + writes + reads
back a fresh file before signalling success.

The on-disk image is built by the shared `rustos-test-rustfs-image`
fixture (a 1 MiB, 512-byte-block, 64-inode volume). Unlike the
hand-encoded FAT32 fixture, the rustfs image is authored by the **real
rustfs driver itself** — the fixture formats an in-memory volume through
`RustFs::format` and plants the file through the driver's own write path
— so the fixture and the driver can never disagree about the on-disk
format (`AGENTS.md` §2.2). The host harness (`cargo xtask test --qemu`)
plants exactly that image on the backing disk, and the freestanding guest
tail names the same planted and to-be-written files through the fixture's
constants. The device tail (`rustfs_round_trip`) is generic over the
virtio transport, so a riscv64 MMIO sibling runs identical code.

## Capabilities

Loading requires `CAP_DRV_LOAD` at `register` time. The driver runs in
user space; it does not request `CAP_DRV_KERNEL`. The read/write methods
are reached only through the `DriverHandle` the host minted at load time,
and the VFS only delegates a write to a non-`READ_ONLY` mount.

## Test surface

`cargo test -p rustos-drv-fs-rustfs` formats an in-memory volume and
exercises: `format`/`open` round-trip and rejection of an unformatted
device; create/lookup/listing (including the buffer-size guard and the
`.`/`..` skip); read/write with block-boundary straddling and sparse
zero-fill; single-indirect large files across a remount; `truncate` shrink
and grow; `remove` and name reuse; the non-empty-directory `Busy` guard;
the per-inode security record (mode, owner, ACL, capability gate) round-
tripping across a remount; the four §21 `Time64` timestamps defaulting to
the epoch without a clock, being stamped per the POSIX model, tracking
directory create/remove, persisting across a remount, and round-tripping
pre-1970 and post-2038 instants without truncation; copy-on-write
overwrite persistence; the `register` capability gate; a
**crash-consistency sweep** that faults the
device after every possible write count during a journalled overwrite and
asserts the result is always either fully the old or fully the new
contents — never torn — with both outcomes observed; and a **journal
soak** that drives a deterministic, seeded stream of
`create`/`write`/`truncate`/`remove` operations and crash-tests *every*
operation at *every* device-write count, asserting the recovered whole-
tree snapshot equals the volume either exactly before or exactly after the
operation (never an intermediate) and remains mountable, with rollbacks
and replays both observed across the run.

The `pjdfstest`-equivalent POSIX suite remains tracked in
`.junie/next-session-prompt.md`.
