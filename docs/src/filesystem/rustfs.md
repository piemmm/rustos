# rustfs driver

`rustfs` (`drivers/filesystem/rustfs`, crate `rustos-drv-fs-rustfs`) is the
**native RustOS filesystem**: a block-backed, copy-on-write filesystem that
stores full POSIX metadata plus an inline access-control list and an
optional capability gate **per inode** (`AGENTS.md` §5.3). There is exactly
one on-disk version — `rustfs` is built up internally in the stages of
its [specification](./rustfs-spec.md), but the driver and its format are a
single shipping thing, not a `v1`/`v2` pair. It
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
**superblock ring** of four logical slots, each a **mirrored pair** of
adjacent blocks (eight blocks in all); everything else is allocated
copy-on-write from the pool that follows. `RustFs::open` re-derives and
validates the geometry from the selected superblock slot.

| Region          | Contents                                                  |
| --------------- | --------------------------------------------------------- |
| Superblock ring | Blocks 0–7: four slots, each a mirrored pair of blocks,   |
|                 | each pointing at a committed root.                        |
| Pool            | Everything else, allocated copy-on-write: the transaction |
|                 | root, the inode-tree nodes, the per-file extent-tree      |
|                 | nodes, directory blocks, and raw file-data blocks.        |

Every **metadata** block is self-identifying (`AGENTS.md` §8 block
identity): its first 128 bytes carry a magic, block type, format version,
the volume UUID, an owner object, a generation, its logical and physical
address, and a **keyed authenticator** — an HMAC-SHA256 tag computed
through `lib/crypto` (`AGENTS.md` §2.12) over identity + payload. Decoding
verifies all of that against the address the reader *expected*, so a stale,
misdirected, wrong-type, torn, bit-rotted, or wrong-key block is rejected at
decode time and the mount fails closed (`AGENTS.md` §5.4). Raw file-data
blocks carry no header and use the full block.

Inodes are 256-byte records held in a **copy-on-write inode tree** keyed by
inode number (see the next section); inode 1 is the root directory. Each
inode names the root of its own **extent tree**, which maps a file's logical
block offset to a physical run `(start, length)` — so a file can span the
whole volume and a large contiguous write collapses to a single extent
record. Directories are block-addressed payloads of 64-byte slots (`inode`,
`name_len`, name) reached through the same extent map; `.` and `..` are
stored on disk and hidden from `read_dir`. The inode record also stores the
four §21 timestamps. A volume written by a different format version is
refused rather than misread.

> **Stage 3 of the [specification](./rustfs-spec.md).** The volume is a
> complete, mountable copy-on-write filesystem whose metadata scales through
> B-trees, is **authenticated** with a `lib/crypto` keyed MAC, and is stored
> in **two physical copies** that are repaired from each other (see the next
> section). Encryption, compression, and dedupe are later stages: the volume
> is not yet encrypted at rest, and the authenticator key is, this stage, a
> placeholder derived from the volume UUID through `lib/crypto` rather than a
> real per-volume key hierarchy (`rustfs-spec.md` §15.4). The free-block
> bitmap is rebuilt in memory at mount by walking the trees from the selected
> root; it is not stored on disk.

## Metadata authentication and redundancy (`rustfs-spec.md` §5, §8)

Each metadata block is sealed with a **keyed authenticator** (HMAC-SHA256
through `lib/crypto`, `AGENTS.md` §2.12 — crypto is the standing "don't roll
your own" exception) covering the block's identity *and* its payload, so the
tag detects not only a flipped payload byte but a stale, misdirected,
wrong-type, torn, or wrong-key block. The key is derived from the volume UUID
through `lib/crypto` this stage; the real per-volume key hierarchy arrives
with encryption (`rustfs-spec.md` §15.4).

Every metadata block is stored in **two physical copies** — a primary and a
companion mirror at the adjacent block (`companion = primary + 1`), so
metadata is allocated in adjacent pairs. One read path serves all metadata
— superblock-ring slots, transaction roots, B-tree nodes, and directory
blocks: it reads the primary, and when the primary fails to authenticate it
falls back to the companion and **repairs** the primary from the good copy
(`rustfs-spec.md` §8 — try redundant copies, repair bad from good). If both
copies fail to authenticate the read fails closed; it never trusts corrupt
bytes and never panics (`AGENTS.md` §5.4 / §2.9). A directory's content
blocks are themselves metadata, so they too are mirrored pairs; a regular
file's data blocks are single-copy and carry no header. Because every
metadata block obeys the one `primary + 1` rule, there is a single
redundancy mechanism rather than one per structure (`AGENTS.md` §2.2).

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

## Copy-on-write metadata trees

Both scalable metadata structures are the **same** generic copy-on-write
B-tree (`src/btree.rs`), keyed by `u64` (`AGENTS.md` §2.2 — one
implementation, not two). Each tree node is one self-identifying metadata
block (`BlockType::Btree`); a leaf holds `(key, value)` records in key order
and an internal node holds `(separator, child)` records, where the separator
is the smallest key in the child.

- **Inode tree.** Keyed by inode number, value the 256-byte inode record. It
  supersedes Stage 1's two-level inode map and removes the format-time
  `inode_count` cap — the tree grows as inodes are created. The transaction
  root names the tree's root block and the next inode number to hand out.
- **Extent tree.** One per file, keyed by logical block offset, value a
  `(physical start, run length)` extent. It supersedes the 12-direct +
  single-indirect map; a lookup is a floor query that finds the run covering
  an offset, and a sequential write merges into the adjacent run so the map
  stays compact.

Mutations copy-on-write the touched node to a fresh (or transaction-private)
block and bubble the change up to a new root; nodes split on overflow and
borrow-or-merge on underflow, all `Result`-based and panic-free with no
`unsafe`. Block allocation draws file **data** upward from the low end of the
pool and **metadata** downward from the high end, with a small metadata
reserve so a delete can always copy-on-write itself and commit even on an
otherwise-full volume.

The mount-time **free-space rebuild** walks these trees from the selected
root — every inode-tree node, then each inode's extent-tree nodes and the
physical runs they map — to reconstruct the in-memory free-block bitmap, so
the authoritative free set is always derived from live metadata rather than a
stored bitmap.

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
- **Commit order (`docs/src/filesystem/rustfs-spec.md` §14).** Write the copy-on-write
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
wrong type, wrong expected address, foreign UUID, a flipped payload byte,
and a wrong authenticator key; a metadata bit-flip being **detected and
repaired** from the companion mirror, a one-copy superblock corruption still
mounting via the mirror, and both copies corrupt failing closed;
`format`/`open` round-trip and rejection of an unformatted device;
create/lookup/listing across nested directories; read/write with
block-boundary straddling; extent-backed large files across a remount;
inode-tree growth and shrink (split, borrow, and merge) across many inodes;
a file with many non-contiguous extents that splits its extent tree; a large
contiguous write collapsing to a single extent; the mount-time free-space
rebuild matching the authoritative live set; `truncate` keeping the surviving
prefix; `remove` reclaiming space so a full volume can allocate again; the
fail-closed extremes
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
valid image (which also drives the authenticate-then-fall-back-to-mirror
path), a duplicated-copy sweep that corrupts *both* copies of each block
pair, and a fixed-seed PRNG all drive `RustFs::open` over arbitrary bytes,
asserting it never panics and fails closed.

The `pjdfstest`-equivalent POSIX suite remains tracked in
`.junie/next-session-prompt.md`.
