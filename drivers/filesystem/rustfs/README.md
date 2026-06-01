# `rustos-drv-fs-rustfs` — native RustOS filesystem driver

Stage 5 deliverable. `rustfs` is the **native RustOS filesystem**: a
block-backed, journaled, copy-on-write filesystem that stores full POSIX
metadata plus an inline access-control list and an optional capability
gate **per inode** (`AGENTS.md` §5.3). It sits behind any
`rustos_abi::driver::block::Block` device and is exposed through the
versioned `rustos_abi::driver::filesystem::FilesystemRead`,
`FilesystemWrite`, and `FilesystemSecurity` traits.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so each I/O surface is a **new
versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9).

## On-disk format

Fixed-size blocks (the device logical block size, 512–4096 bytes, a power
of two). The regions tile the device in order — superblock, inode table
(256-byte records, index 1 = root), data-block bitmap, journal (one header
block + a redo-log data area), then data blocks. Each inode has 16 direct
block pointers plus one single-indirect block. Directories are
block-addressed payloads of 64-byte slots; `.`/`..` are stored on disk and
hidden from `read_dir`.

## Crash consistency

- **File data is copy-on-write**: a write goes to a freshly allocated
  block, the inode is re-pointed, and the old block is freed — a crash
  never exposes a torn data block.
- **Metadata is journaled** (physical redo log): a transaction's modified
  bitmap / inode / directory / indirect blocks are staged into the journal,
  a checksummed commit record is written, then the images are checkpointed
  to their home blocks. A mount replays a committed-but-un-checkpointed
  transaction and discards an uncommitted one.

The staged block images live in the on-disk journal; only the home block
numbers are held in RAM (no large in-memory staging buffer). No
`unwrap`/`expect`/`panic!` and no `unsafe`.

## Security

`rustfs` **stores** each inode's owner, mode, ACL, and capability gate. It
reports the record through the versioned `FilesystemSecurity` trait
(`security(node) -> NodeSecurity`) and accepts an updated one through
`RustFs::set_security`, but makes **no** permission decision itself: the
VFS is the policy point (`AGENTS.md` §5.4). Because the driver implements
`FilesystemSecurity`, the VFS delegates through its `*_via_secured`
operations and judges each node against its own stored §5.3 record rather
than a uniform mount-point template.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The `FilesystemRead`/`FilesystemWrite` methods are reached only through
  the `DriverHandle` the host minted at load time, and the VFS only
  delegates a write to a non-`READ_ONLY` mount. The driver runs in user
  space; it does not request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p rustos-drv-fs-rustfs` runs 17 host-side tests over an
in-memory device: format/open (and unformatted-device rejection),
create/lookup/listing (buffer-size guard, `.`/`..` skip), read/write across
block boundaries and sparse zero-fill, single-indirect large files across a
remount, `truncate` shrink/grow, `remove` + name reuse, the non-empty
directory `Busy` guard, the per-inode security record round-tripping across
a remount, copy-on-write overwrite persistence, the `register` capability
gate, and a crash-consistency sweep that faults the device after every
possible write count during a journalled overwrite and asserts the result
is always fully-old or fully-new — never torn.

The native journal crash-consistency soak and the `pjdfstest`-equivalent
POSIX suite are tracked in `.junie/next-session-prompt.md`.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `RustFs`
type is exported so the driver host can construct an instance with
`RustFs::format` / `RustFs::open`; the host reaches into it through the
`FilesystemRead`/`FilesystemWrite`/`FilesystemSecurity` traits and the
`set_security` accessor.
