# ext4 driver

`drivers/filesystem/ext4` (`rustos-drv-fs-ext4`) is a **read-only**
driver for ext2/ext3/ext4 volumes behind any
[`Block`](../abi/driver_traits.md) device. It implements the versioned
`FilesystemRead` trait; the frozen `Filesystem` trait remains
mount/unmount only, so the read surface is a separate trait rather than
a widening of the shipped one (`AGENTS.md` §2.4 / §9), exactly as for the
[FAT32](./fat32.md) and [rustfs](./rustfs.md) drivers.

## Why read-only first

ext4 is the dominant Linux on-disk format, so reading existing volumes
(installation media, foreign disks) is the first need. Write support is
a later deliverable and, as with FAT32, will arrive as the separate
`FilesystemWrite` trait rather than by changing the read surface.

## On-disk support

| Feature                                  | Support                       |
| ---------------------------------------- | ----------------------------- |
| Block sizes 1024 / 2048 / 4096           | yes                           |
| 128- or 256-byte inodes                  | yes                           |
| 32- or 64-byte group descriptors (64bit) | yes                           |
| Extent-mapped inodes (default ext4)      | yes, incl. multi-level trees  |
| Classic block map (ext2/ext3)            | yes (direct + 1/2/3 indirect) |
| Linear directory blocks                  | yes                           |
| Hash-indexed (`htree`) directories       | linear leaf view only         |

`Ext4::open` validates the superblock magic (`0xEF53` at byte `0x38` of
the superblock, which itself starts at the fixed byte offset 1024) and
re-derives the geometry — block size, inodes/blocks per group, inode
size, descriptor size, and the group-descriptor table offset — rejecting
a bad magic or an inconsistent geometry with `DriverError::BadMagic` and
an out-of-range block size with `DriverError::Unsupported`. The device
logical-block size and the filesystem block size are independent: all
I/O is staged one device block at a time.

## Nodes and mapping

A `NodeId` is the on-disk inode number, so there is no in-memory inode
table; each operation reads the inode on demand. Logical-to-physical
block mapping covers both on-disk layouts:

- **Extent-mapped** inodes walk the extent tree from the inline root in
  `i_block`, descending interior index nodes up to a bounded depth. An
  uninitialised extent and a missing extent both read as zeros.
- **Block-mapped** inodes resolve the 12 direct pointers and the single,
  double, and triple indirect blocks. A zero pointer is a sparse hole
  and reads as zeros.

## Directories

Directory blocks are scanned linearly, honouring each entry's record
length and skipping unused slots and the `.`/`..` self-links (the VFS
resolves those itself, §16). A child's kind comes from the directory
entry's `file_type` byte when the `filetype` feature is set, otherwise
from the child inode's mode. The root block of a hash-indexed directory
is read through its linear `.`/`..` view; deeply indexed interior nodes
are not traversed.

## Security

ext4 stores a per-inode owner, mode, and ACL, but the driver makes **no**
permission decision: the VFS metadata layer that mounts it is the policy
point (`AGENTS.md` §5.4). Surfacing the stored per-inode §5.3 record to
the VFS through `FilesystemSecurity` (as [rustfs](./rustfs.md) does) is a
later step; until then a mounted ext4 subtree is governed by the uniform
mount-point template.

## Capabilities

Loading requires `CAP_DRV_LOAD`; the `FilesystemRead` methods are reached
only through the `DriverHandle` the host minted at load time. The driver
runs in user space and does not request `CAP_DRV_KERNEL`.

## Tests

`cargo test -p rustos-drv-fs-ext4` builds an allocation-free in-memory
ext4 image (block size 1024, one block group, 128-byte inodes,
`filetype` on) holding an extent-mapped root, an extent-mapped file, a
subdirectory with a nested file, and a classic block-mapped file that
combines direct pointers, sparse holes, and a single-indirect block. The
14 host-side tests cover superblock validation, `node_info`, ordered
listing with `.`/`..` suppression and end-of-directory, `lookup` and
subdirectory traversal, extent and classic reads (including across holes
and the direct/indirect boundary), the `Unsupported`/`BufferTooSmall`/
`NotFound` guards, and the `register` capability gate. A
`pjdfstest`-equivalent POSIX suite and an end-to-end QEMU vertical remain
future work.
