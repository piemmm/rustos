# `rustos-drv-fs-ext4` — ext4 filesystem driver (read-only)

Stage 5 deliverable. Reads an ext2/ext3/ext4 volume behind any
`rustos_abi::driver::block::Block` device and exposes it through the
versioned `rustos_abi::driver::filesystem::FilesystemRead` trait.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so the read surface is a **new
versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9), exactly as for the FAT32 and rustfs drivers.

## Supported volumes

| On-disk feature                          | Support                       |
|------------------------------------------|-------------------------------|
| Block sizes 1024 / 2048 / 4096           | yes                           |
| 128- or 256-byte inodes                  | yes                           |
| 32- or 64-byte group descriptors (64bit) | yes                           |
| Extent-mapped inodes (default ext4)      | yes, incl. multi-level trees  |
| Classic block map (ext2/ext3)            | yes (direct + 1/2/3 indirect) |
| Linear directory blocks                  | yes                           |
| Hash-indexed (`htree`) directories       | linear leaf view only         |

The on-disk superblock magic (`0xEF53`, offset `0x38`) is validated at
`open`; a bad magic or a structurally invalid geometry is rejected with
`DriverError::BadMagic`, and a volume whose block size is outside the
supported range is `DriverError::Unsupported`.

The device logical-block size and the filesystem block size are
decoupled: all I/O is staged one device block at a time, so a 1024-byte
filesystem over a 512-byte block device works without a special case.

## Mapping

A `NodeId` is the on-disk inode number, so there is no in-memory inode
table. `lookup`, `node_info`, `read_at`, and `read_dir` read the inode
on demand. Logical-to-physical block mapping handles both layouts:

- **Extent-mapped** inodes (the `i_flags` extents bit) walk the extent
  tree from the inline root in `i_block`, descending interior index
  nodes; an uninitialised extent and a sparse hole both read as zeros.
- **Block-mapped** inodes resolve the 12 direct pointers and the single,
  double, and triple indirect blocks; a zero pointer is a sparse hole.

## Directories

Directory blocks are walked linearly, honouring each entry's `rec_len`
and skipping unused (`inode == 0`) slots and the `.`/`..` self-links
(the VFS resolves those itself, §16). A child's kind comes from the
entry's `file_type` byte when the `filetype` feature is set, and
otherwise from the child inode's mode. The root block of a hash-indexed
directory is read through its linear `.`/`..` view; deeply indexed
interior directory nodes are not traversed.

## Limitations

- Read-only. Write support (`FilesystemWrite`) is a later deliverable.
- Hash-tree (`htree`) interior nodes are not traversed; only the linear
  leaf layout is read (sufficient for small and moderate directories).
- Inline-data and special files (symlinks, devices, FIFOs) are reported
  as `NotFound` by `node_info` rather than surfaced as a node kind the
  `abi-v1` read surface does not model.

## Security

ext4 stores a per-inode owner, mode, and ACL, but this driver makes
**no** permission decisions: the VFS metadata layer (`AGENTS.md` §5.3)
that mounts the driver is the policy point (§5.4 — this is raw
structural I/O). Case-folding and Unicode normalisation policy likewise
belong to the VFS, not the driver.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The `FilesystemRead` methods are reached only through the
  `DriverHandle` the host minted at load time. The driver runs in user
  space; it does not request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p rustos-drv-fs-ext4` builds a specification-shaped,
allocation-free in-memory ext4 image (block size 1024, one block group,
128-byte inodes, `filetype` on) and exercises:

- Superblock validation (`open`), including bad-magic rejection.
- `node_info` for the directory root and regular files.
- Ordered root listing (`read_dir`), `.`/`..` suppression, and
  end-of-directory; `BufferTooSmall` on an undersized name buffer.
- `lookup` (incl. subdirectory traversal) and the missing-child
  `NotFound` path.
- Reading an **extent-mapped** file (full, mid-offset, EOF).
- Reading a **classic block-mapped** file across sparse holes and the
  direct/single-indirect boundary.
- `Unsupported` on directory-vs-file mismatches and the `NodeId::NONE`
  guard.
- The `register` capability gate and `into_block` round-trip.

14/14 host-side tests pass. A `pjdfstest`-equivalent POSIX suite and an
end-to-end QEMU vertical are tracked in `.junie/next-session-prompt.md`.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `Ext4`
type is exported so the driver host can construct an instance with
`Ext4::open`; the host reaches into it only through the `FilesystemRead`
trait.
