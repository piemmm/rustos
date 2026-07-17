# `tairix-drv-fs-fat32` — FAT32 filesystem driver (read/write)

Stage 5 deliverable. Reads and writes a FAT32 volume behind any
`tairix_abi::driver::block::Block` device and exposes it through the
versioned `tairix_abi::driver::filesystem::FilesystemRead` and
`FilesystemWrite` traits.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so each I/O surface is a
**new versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9).

## Supported volumes

| Format | Mode       | Names                                    |
|--------|------------|------------------------------------------|
| FAT32  | read/write | long names (VFAT) + 8.3 short names      |

FAT type is identified by the FAT32 boot-sector shape — a zero 16-bit
FAT size (offset 22) and a zero root-entry count (offset 17). A
FAT12/FAT16 volume has non-zero values there and is rejected with
`DriverError::BadMagic`, so the distinction is exact for the volumes
this driver accepts.

The device logical-block size and the FAT bytes-per-sector are
decoupled: all I/O is staged one logical block at a time, so a 4096-byte
FAT sector over a 512-byte block device (or the reverse) works without a
special case.

## Names

Each directory entry exposes a single name as **UTF-8**: the
reconstructed long name when a valid long-name set precedes the 8.3
short entry (contiguous sequence, `0x40` last-logical fragment present,
short-name checksum matches), and otherwise the 8.3 short name. UTF-16LE
code units — including surrogate pairs — are decoded, and the driver
falls back to the short name on any malformed set (unpaired surrogate,
invalid scalar value, checksum mismatch) rather than surfacing a partial
name. When a long name is present the internal 8.3 alias is *not*
separately resolvable.

## Writing

`FilesystemWrite` addresses a target as a `(dir, name)` pair, because a
FAT file's length and starting cluster live in its parent directory
entry, not in a self-describing `NodeId`. `create`/`mkdir` write a VFAT
long-name set bound to a generated, directory-unique `~N` 8.3 short alias
(so arbitrary, case-preserving names round-trip); `write_at` allocates
and chains clusters, zero-fills sparse gaps and updates the entry;
`truncate` frees the tail chain (shrink) or zero-extends (grow); and
`remove` frees the chain and marks the entry plus its long-name run
deleted. Free clusters are found by scanning the FAT, directories grow a
zeroed cluster at a time, and every FAT mutation is mirrored across all
FAT copies.

## Limitations

- Writes go straight through to the block device; there is no
  write-back cache or journal (FAT has no journal).
- A generated short alias uses a `~N` numeric tail (collision-resolved
  against the live directory) rather than the legacy hashed scheme.

## Security

FAT32 has no per-inode owner, mode, ACL, or capability gate. Those live
in the VFS metadata layer (`AGENTS.md` §5.3) that mounts this driver;
the driver makes **no** permission decisions (§5.4 — the VFS is the
policy point, this is raw structural I/O). Case-folding and Unicode
normalisation policy likewise belong to the VFS, not the driver.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The `FilesystemRead`/`FilesystemWrite` methods are reached only through
  the `DriverHandle` the host minted at load time, and the VFS only
  delegates a write to a non-`READ_ONLY` mount. The driver runs in user
  space; it does not request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p tairix-drv-fs-fat32` builds a specification-shaped,
allocation-free in-memory FAT32 image and exercises:

- Boot-sector validation (`open`), including bad-signature and
  non-FAT32 rejection.
- Ordered root-directory listing (`read_dir`) and end-of-directory.
- Case-insensitive `lookup`, including subdirectory traversal.
- File reads across the FAT cluster chain, including a window that
  straddles a cluster boundary, plus offset/EOF behaviour.
- Long-name (VFAT) reconstruction: multi-fragment listing and lookup,
  the short name superseded by its long name, checksum-mismatch
  fall-back, and the UTF-16LE→UTF-8 decoder (surrogate pairs, unpaired
  surrogates, terminator, output overflow).
- `Unsupported` on directory-vs-file mismatches and `BufferTooSmall`
  on an undersized name buffer.
- The `register` capability gate.
- Write round-trips: create + write + read-back (short and long names),
  writes that extend across a cluster boundary, sparse zero-fill,
  `truncate` shrink and grow, `remove` + name reuse, `mkdir` with a
  nested file, and the `Busy`/`Unsupported`/`NotFound` guards.

38/38 host-side tests pass.

An **end-to-end QEMU vertical** drives the driver against a real
(emulated) virtio-blk-pci device:
`tests/integration/fat32_virtio_blk_pci_x86_64` boots the production
kernel, brings the block device online, mounts a planted FAT32 image
through `Fat32::open`, verifies the planted file, and creates + writes +
reads back a fresh file. The image is built by the shared
`tairix-test-fat32-image` fixture and planted by `cargo xtask test
--qemu`; the guest tail names the same files through that fixture, so
both sides share one source of truth (`AGENTS.md` §2.2). A
`pjdfstest`-equivalent POSIX suite is still tracked in
`.junie/next-session-prompt.md`.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `Fat32`
type is exported so the driver host can construct an instance with
`Fat32::open`; the host reaches into it only through the
`FilesystemRead`/`FilesystemWrite` traits.
