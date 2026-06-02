# rustfs driver

`rustfs` (`drivers/filesystem/rustfs`, crate `rustos-drv-fs-rustfs`) is the
**native RustOS filesystem**: a block-backed, copy-on-write filesystem that
stores full POSIX metadata plus an inline access-control list and an
optional capability gate **per inode** (`AGENTS.md` §5.3). There is exactly
one on-disk version — `rustfs` is built up internally in the stages of
`.junie/RUSTFS.md`, but the driver and its format are a single shipping
thing, not a `v1`/`v2` pair. It
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
size, between 512 and 4096 bytes, a power of two). The device opens at a
**superblock ring** of four slots in the first four blocks; everything
else is allocated copy-on-write from the pool that follows. `RustFs::open`
re-derives and validates the geometry from the selected superblock slot.

| Region          | Contents                                                  |
| --------------- | --------------------------------------------------------- |
| Superblock ring | Blocks 0–3: four slots, each pointing at a committed root.|
| Pool            | Everything else, allocated copy-on-write: the transaction |
|                 | root, the inode map, inode blocks, directory blocks,      |
|                 | indirect-pointer blocks, and raw file-data blocks.        |

Every **metadata** block is self-identifying (`AGENTS.md` §8 block
identity): its first 96 bytes carry a magic, block type, format version,
the volume UUID, an owner object, a generation, its logical and physical
address, and a fast checksum over identity + payload. Decoding verifies
all of that against the address the reader *expected*, so a stale,
misdirected, wrong-type, or torn block is rejected at decode time and the
mount fails closed (`AGENTS.md` §5.4). Raw file-data blocks carry no
header and use the full block.

Inodes are 256-byte records reached through a two-level **copy-on-write
inode map** (an index block pointing at map blocks, which point at the
packed inode blocks); index 1 is the root. Each inode holds 12 direct
block pointers plus one single-indirect block, so a file spans up to
`12 + (block_size - 96)/8` blocks. Directories are block-addressed
payloads of 64-byte slots (`inode`, `name_len`, name); `.` and `..` are
stored on disk and hidden from `read_dir`. The inode record also stores
the four §21 timestamps. A volume written by a different format version is
refused rather than misread.

> **Stage 1 of `.junie/RUSTFS.md`.** The volume is a complete, mountable
> copy-on-write filesystem, but encryption, compression, and dedupe are
> later stages: a Stage-1 volume is not yet encrypted at rest, and the
> metadata checksum is the fast physical checksum, not yet the keyed
> authenticator. The free-block bitmap and inode-allocation bitmap are
> rebuilt in memory at mount by walking the selected root; they are not
> stored on disk.

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

## Copy-on-write and the superblock ring

`rustfs` keeps metadata and data consistent across a crash without
`fsck` (`AGENTS.md` §2.5). Every operation is a transaction, and a block
reachable from the last committed transaction root is **never overwritten
in place**:

- **Copy-on-write everywhere.** A modified metadata or data block is
  written to a freshly allocated block; the block that referenced it is
  itself copy-on-written to point at the new location, up to the inode
  map. Blocks superseded by the transaction are *deferred-freed* — marked
  reusable only after the transaction commits — so the previous committed
  tree stays wholly intact until the new one is durable.
- **Commit order (`.junie/RUSTFS.md` §14).** Write the copy-on-write
  blocks, write the new transaction root carrying its inline commit
  record, then publish the next superblock-ring slot (round-robin)
  pointing at that root. `open` scans the ring and selects the
  highest-generation slot whose root *and* commit record validate. A
  crash before the slot is published leaves the previous committed root
  selected; a crash mid-publish overwrites only the oldest ring slot, so
  the most recent committed root always survives — the mount lands on a
  whole transaction boundary, never a torn one.

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
exercises: the self-identifying block header rejecting a wrong magic,
wrong type, wrong expected address, foreign UUID, and a flipped checksum;
`format`/`open` round-trip and rejection of an unformatted device;
create/lookup/listing across nested directories; read/write with
block-boundary straddling; single-indirect large files across a remount;
`truncate` keeping the surviving prefix; `remove` reclaiming space so a
full volume can allocate again; the fail-closed extremes
(`Busy`/`LengthOutOfRange`/`NotFound`); the per-inode security record and
the four §21 `Time64` timestamps (incl. pre-1970 and far-future)
round-tripping across a remount; superblock-ring selection of the
highest committed generation; and a **crash-replay sweep** that faults the
device after every possible write count during a single committing
transaction and asserts the re-opened volume always mounts, the
pre-existing file is always intact, and the in-flight write is either
fully applied or fully absent — never torn.

The mount / metadata-decode path additionally has a `cargo xtask fuzz`
harness (`fuzz_mount`, `AGENTS.md` §19.6): a per-byte flip sweep over a
valid image plus a fixed-seed PRNG drives `RustFs::open` over arbitrary
bytes, asserting it never panics and fails closed.

The `pjdfstest`-equivalent POSIX suite remains tracked in
`.junie/next-session-prompt.md`.
