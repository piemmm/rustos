# `rustos-drv-fs-rustfs` — native RustOS filesystem driver

`rustfs` is the **native RustOS filesystem**: a block-backed, copy-on-write
filesystem that stores full POSIX metadata plus an inline access-control
list and an optional capability gate **per inode** (`AGENTS.md` §5.3). It
sits behind any `rustos_abi::driver::block::Block` device and is exposed
through the versioned `rustos_abi::driver::filesystem::FilesystemRead`,
`FilesystemWrite`, `FilesystemSecurity`, and `FilesystemTimestamps` traits.

There is exactly **one** on-disk version. `rustfs` is built up internally
in the stages of `docs/src/filesystem/rustfs-spec.md`, but the driver and its format are a
single shipping thing — not a `v1`/`v2` pair. This crate is the only
implementation.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so each I/O surface is a **new
versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9).

## On-disk format

Fixed-size blocks (the device logical block size, 512–4096 bytes, a power
of two). The device opens at a **superblock ring** of four slots in the
first four blocks; everything else — the transaction root, the
copy-on-write inode-tree nodes, the per-file extent-tree nodes, directory
blocks, and raw file-data blocks — is allocated copy-on-write from the pool
that follows.

Every **metadata** block is self-identifying (`AGENTS.md` §8 block
identity): its first 96 bytes carry a magic, block type, format version,
the volume UUID, an owner object, a generation, its logical and physical
address, and a fast checksum over identity + payload. Decoding verifies all
of that against the address the reader expected, so a stale, misdirected,
wrong-type, or torn block is rejected at decode time and the mount fails
closed (`AGENTS.md` §5.4). Raw file-data blocks carry no header.

Inodes are 256-byte records (inode 1 = root, the four §21 `Time64`
timestamps inline) held in a copy-on-write **inode tree** keyed by inode
number, so metadata scales past any fixed inode count. Each inode names the
root of its own copy-on-write **extent tree** mapping a logical block offset
to a physical run `(start, length)`, so a file can span the whole volume and
a contiguous write stays a single record. Both are the one generic B-tree in
`src/btree.rs` (`AGENTS.md` §2.2). Directories are block-addressed payloads of
64-byte slots reached through the extent map; `.`/`..` are stored on disk and
hidden from `read_dir`.

## Crash consistency (copy-on-write + superblock ring)

Every operation is a transaction. A block reachable from the last
committed transaction root is **never overwritten in place**: modified
metadata and data are written copy-on-write to freshly allocated blocks,
and superseded blocks are deferred-freed (reusable only after the
transaction commits). The commit order (`docs/src/filesystem/rustfs-spec.md` §14) is: write
the copy-on-write blocks, write the new transaction root carrying its
inline commit record, then publish the next superblock-ring slot pointing
at it. `RustFs::open` scans the ring and selects the highest-generation
slot whose root and commit record validate — so a crash leaves the mount
on a whole transaction boundary, never a torn one.

The free-block bitmap is rebuilt in memory at mount by walking the trees
from the selected root — every inode-tree node, then each inode's extent-tree
nodes and the runs they map — so the authoritative free set is always derived
from live metadata (the crate uses `alloc` for these). Block allocation draws
data upward and metadata downward from the pool with a small metadata reserve,
so a delete can copy-on-write itself even on a full volume. No
`unwrap`/`expect`/`panic!` and no `unsafe`.

> **Staged build.** A volume is a complete, mountable copy-on-write
> filesystem with B-tree metadata today, but encryption, compression, and
> dedupe are later stages of the
> [specification](../../../docs/src/filesystem/rustfs-spec.md): a volume is
> not yet encrypted at rest, and the metadata checksum is the fast physical
> checksum, not yet the keyed authenticator (which will arrive via
> `lib/crypto`).

## Security

`rustfs` **stores** each inode's owner, mode, ACL, and capability gate. It
reports the record through `FilesystemSecurity` (`security(node)`) and
accepts an updated one through `RustFs::set_security`, but makes **no**
permission decision itself: the VFS is the policy point (`AGENTS.md` §5.4).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The read/write methods are reached only through the `DriverHandle` the
  host minted at load time, and the VFS only delegates a write to a
  non-`READ_ONLY` mount. The driver runs in user space; it does not
  request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p rustos-drv-fs-rustfs` runs the block-header decode-rejection
tests (wrong magic / type / address / UUID / flipped checksum) and host
tests over an in-memory device: format/open (and unformatted-device
rejection), nested create/lookup/listing, read/write across block
boundaries, extent-backed large files across a remount, inode-tree
split/borrow/merge across many inodes, a many-extent file that splits its
extent tree, a contiguous write collapsing to one extent, the free-space
rebuild matching the authoritative live set, `truncate` prefix survival,
`remove` reclaiming space after `NoSpace`, the fail-closed
extremes (`Busy`/`LengthOutOfRange`/`NotFound`), the per-inode security
record and four §21 timestamps (incl. pre-1970 / far-future) round-tripping
across a remount, superblock-ring generation selection, and a
**crash-replay sweep** that faults the device after every write count
during a committing transaction and asserts the re-opened volume always
mounts with the in-flight write either fully applied or fully absent —
never torn.

The 1 GiB filesystem soak (`cargo xtask fssoak --target rustfs`) drives the
shared cross-filesystem exerciser, and a `cargo xtask fuzz` harness
(`fuzz_mount`) fuzzes the mount / metadata-decode path (`AGENTS.md` §19.6).

## End-to-end QEMU vertical

`tests/integration/rustfs_virtio_blk_pci_x86_64` mounts a planted rustfs
volume over a real (emulated) virtio-blk-pci device under QEMU and
round-trips a read and a write (`cargo xtask test --qemu`). The backing
image comes from the `tests/integration/rustfs_image` fixture, which the
real rustfs driver itself authors, so the fixture and the driver share one
source of truth for the on-disk format (`AGENTS.md` §2.2).

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `RustFs`
type is exported so the driver host can construct an instance with
`RustFs::format` / `RustFs::open`; the host reaches into it through the
filesystem traits and the `set_security` accessor.
