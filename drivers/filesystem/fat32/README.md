# `rustos-drv-fs-fat32` — FAT32 filesystem driver (read-only)

Stage 5 deliverable. Reads a FAT32 volume behind any
`rustos_abi::driver::block::Block` device and exposes it through the
versioned `rustos_abi::driver::filesystem::FilesystemRead` trait.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so the read surface is a
**new versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9). A future `FilesystemWrite` trait will add the
mutating surface.

## Supported volumes

| Format | Mode      | Names                                    |
|--------|-----------|------------------------------------------|
| FAT32  | read-only | long names (VFAT) + 8.3 short names      |

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

## Limitations

- **Read-only.** Writing is out of scope until `FilesystemWrite` lands
  (tracked in `PLAN.md`).

## Security

FAT32 has no per-inode owner, mode, ACL, or capability gate. Those live
in the VFS metadata layer (`AGENTS.md` §5.3) that mounts this driver;
the driver makes **no** permission decisions (§5.4 — the VFS is the
policy point, this is raw structural I/O). Case-folding and Unicode
normalisation policy likewise belong to the VFS, not the driver.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The `FilesystemRead` methods are reached only through the
  `DriverHandle` the host minted at load time. The driver runs in user
  space; it does not request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p rustos-drv-fs-fat32` builds a specification-shaped,
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

25/25 host-side tests pass. A QEMU `pjdfstest`-equivalent integration
suite over `virtio_blk` is tracked in `.junie/next-session-prompt.md`.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `Fat32`
type is exported so the driver host can construct an instance with
`Fat32::open`; the host reaches into it only through the
`FilesystemRead` trait.
