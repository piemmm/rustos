# ext4 driver

`drivers/filesystem/ext4` (`rustos-drv-fs-ext4`) is a **read/write**
driver for ext2/ext3/ext4 volumes behind any
[`Block`](../abi/driver_traits.md) device. It implements the versioned
`FilesystemRead`, `FilesystemWrite`, and `FilesystemSecurity` traits; the
frozen `Filesystem` trait remains mount/unmount only, so each surface is
a separate trait rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9), exactly as for the [FAT32](./fat32.md) and
[rustfs](./rustfs.md) drivers.

ext4 is the dominant Linux on-disk format, so reading existing volumes
(installation media, foreign disks) was the first need; mutation arrived
later behind the separate `FilesystemWrite` trait, and now maintains the
on-disk checksums (`metadata_csum`/`gdt_csum`) and wide 64-bit
descriptors a real `mkfs.ext4` image carries.

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
point (`AGENTS.md` §5.4).

The driver implements the versioned `FilesystemSecurity` trait (as
[rustfs](./rustfs.md) does), so the VFS can drive authorization from the
stored per-inode §5.3 record instead of the uniform mount-point
template. `security(node)` reports a `NodeSecurity` carrying the inode's
POSIX mode (the low 12 bits, with the type bits stripped) and its owner
uid/gid; each id recombines its low half (`i_uid`/`i_gid`) with the osd2
high half (`l_i_uid_high`/`l_i_gid_high`). ext4 has no inline capability
gate, so the record carries no `required_cap`.

### POSIX ACLs

Named-user and named-group POSIX ACL grants are decoded from the inode's
`system.posix_acl_access` extended attribute into the record's inline
ACL entries. ext4 keeps that attribute in two places, both of which the
driver reads:

- an **inline** region in the tail of an enlarged (`inode_size > 128`)
  inode record, immediately after `i_extra_isize`, whose entry value
  offsets are measured from the first entry; and
- an **external block** named by `i_file_acl`, whose entry value offsets
  are measured from the start of the block.

Both share the `ext4_xattr_entry` encoding (magic `0xEA020000`,
`e_name_index = 2` for the access ACL). The `ACL_USER` / `ACL_GROUP`
entries map to one grant-only `SecurityAcl` each (`SecuritySubject::User`
/ `Group` with the POSIX `rwx` triad); the owner/owning-group/other/mask
entries are already expressed by the mode bits, so they are skipped. A
volume may carry the attribute inline, in the external block, in both, or
in neither; an absent or malformed region simply contributes no grants
(the mode bits still apply — `AGENTS.md` §5.4, fail closed). The record's
inline ACL budget bounds the number of named grants surfaced.

## Writing

The `FilesystemWrite` surface (`create`, `write_at`, `truncate`,
`remove`, `flush`) mutates a mounted volume. Like the read surface it
makes **no** permission decision — the VFS authorises every write before
delegating (`AGENTS.md` §5.4). New files and directories are created with
the classic block map (12 direct pointers + the single indirect block);
file data is allocated from the block bitmap, inodes from the inode
bitmap, and the group-descriptor and superblock free counts are kept in
step. Directory entries are inserted by splitting an existing record's
slack (growing the directory by a block when none fits) and removed by
merging the freed slot into its predecessor. `truncate` frees the tail
blocks of the classic map and the extent map (the inline depth-0 root or
a depth-1 tree) and zeroes the retained partial block so a later
extension reads as zeros.

A pre-existing extent-mapped file (those a foreign `mkfs` created) grows
in place: `write_at` extends the last extent when the new block is
contiguous and otherwise appends a fresh extent. When the four inline
`i_block` extent slots are exhausted, the inline root is converted into
a **depth-1 tree** — its extents move into a freshly allocated leaf
block and the root becomes an index node — after which further leaves
are attached through new (ascending-ordered) root index entries.
`truncate`/`remove` free a depth-1 tree's emptied leaf blocks and drop
their index entries, collapsing the root back to an empty depth-0 node
when no leaf survives. A tree that would need a second index level
(depth ≥ 2) is refused (see below); the driver never builds one, and the
read path still maps any depth on disk.

### Checksums and wide descriptors

The write path maintains every on-disk checksum a volume carries, so a
default `mkfs.ext4` image (`metadata_csum`, `extent`, `64bit`) is mutated
in place. The checksum primitives are **first-party** — a storage
checksum is not a cryptographic primitive, so `AGENTS.md` §2.12's "never
roll your own" does not apply:

- **`metadata_csum`** uses crc32c (reversed polynomial `0x82F6_3B78`,
  seeded with `crc32c(~0, s_uuid)`) for the superblock `s_checksum`,
  each group descriptor `bg_checksum`, the block/inode-bitmap checksums,
  every inode (`i_checksum_lo`/`hi`, seeded per inode by number and
  generation), each directory leaf's `ext4_dir_entry_tail`, and each
  allocated extent block's `ext4_extent_tail`.
- **`gdt_csum`/`uninit_bg`** uses crc16 (reversed polynomial `0xA001`)
  for the legacy group-descriptor checksum.
- **`64bit`** descriptors carry the high halves of the bitmap checksums
  and `bg_itable_unused`, which the allocator also maintains.

Mutation still **fails closed** (`DriverError::Unsupported`) on a feature
the write path cannot maintain — anything outside the supported
`incompat`/`ro_compat` allow-list, e.g. `bigalloc`, `meta_bg`,
`inline_data`, or an explicit `checksum_seed` (which would invalidate the
uuid-derived seed) — and on an uninitialised block group
(`BLOCK_UNINIT`/`INODE_UNINIT`). `remove`/`truncate` of a file whose
mapping is neither the classic map nor a depth-0/depth-1 extent tree,
and growing an extent tree beyond a single index level (depth ≥ 2), are
refused rather than orphaning blocks or half-building a tree. Such
volumes stay fully **readable**. This is a deliberate, documented
boundary (`AGENTS.md` §2.1 / §5.4), not a silent best-effort.

## Formatting (mkfs)

`Ext4::format(block, inode_count)` lays a fresh, empty volume onto a
blank `Block` device and returns it mounted — the write-side counterpart
of `open()`, with no `mkfs` shell-out (`AGENTS.md` §12/§2.12). After
`format()` the bytes are handed straight to `open()`, which remains the
single source of truth for the on-disk layout.

The formatter writes a deliberately conservative shape the read/write
path fully supports: `filetype` + `extent` only (no `metadata_csum`,
`gdt_csum`, or `64bit`, so no checksum maintenance is needed), 128-byte
inodes, 32-byte group descriptors, block size 4096 bytes for volumes
≥ 128 MiB and 1024 bytes otherwise, and `blocks_per_group = 8 *
block_size`. Only whole groups are laid down — so the reader's group
count is exact — and **every** group is fully materialised (no
lazy/`UNINIT` groups), so the volume can be filled to exhaustion. The
reserved inodes 1..=10 and an extent-mapped empty root directory
(inode 2) are written; `inode_count` is the minimum total inode budget,
rounded up to a whole number of inodes per group (≥ 16 per group).

Allocation past the free data blocks or the inode budget fails with
`DriverError::NoSpace` (POSIX `ENOSPC`), distinct from `DeviceFault`
(`AGENTS.md` §5.4 / §2.9); a device too small for one group plus a data
region, or a zero `inode_count`, is refused with `OutOfRange`. Files
created on a formatted volume use the classic block map, so a single
file reaches at most 12 direct + one single-indirect block.

## Capabilities

Loading requires `CAP_DRV_LOAD`; the `FilesystemRead` methods are reached
only through the `DriverHandle` the host minted at load time. The driver
runs in user space and does not request `CAP_DRV_KERNEL`.

## Tests

`cargo test -p rustos-drv-fs-ext4` builds an in-memory ext4 image (block
size 1024, one block group, 128-byte inodes, `filetype` on, with block
and inode bitmaps and free space) holding an extent-mapped root, an
extent-mapped file, a subdirectory with a nested file, and a classic
block-mapped file that combines direct pointers, sparse holes, and a
single-indirect block. The 44 host-side tests cover superblock
validation, `node_info`, ordered listing with `.`/`..` suppression and
end-of-directory, `lookup` and subdirectory traversal, extent and
classic reads (including across holes and the direct/indirect boundary),
the `Unsupported`/`BufferTooSmall`/`NotFound` guards, the `register`
capability gate, the `FilesystemSecurity::security` record for a file
and a directory, and the write surface: create + multi-block write
round-trips (persisting across a remount), duplicate / invalid-name /
non-directory rejection, sparse extension, `truncate` shrink-then-grow,
directory creation with `.`/`..` and removal (empty vs. `Busy`), inode
reuse after `remove`, the `write_at`/`truncate` directory and
not-found guards, free-inode exhaustion, the fail-closed refusal of
mutation on an unsupported (`checksum_seed`) feature set, the depth-0 →
depth-1 extent-tree growth (sparse writes that overflow the inline root,
with read-back and remount persistence, a sparse hole between extents,
and depth-1 `truncate`-to-zero and `remove` freeing and reusing the
tree's blocks), and the POSIX-ACL decode —
standalone `decode`/`find` units (both value-base conventions, bad
version, the inline budget cap, unrelated attributes) plus end-to-end
`security` reads of an external xattr block, a garbage block, and an
inline ACL in a 256-byte-inode volume.

A separate integration suite (`tests/checksummed.rs`) validates the
checksum maintenance against **real `mke2fs 1.47.0`** images committed
under `tests/fixtures/` — one `metadata_csum,extent,64bit` volume and one
legacy `gdt_csum` volume. It recomputes every on-disk checksum with an
*independent* crc implementation, both on the pristine fixture (proving
the reference crc matches `mke2fs`) and after a
`create`/`write`/`truncate`/`mkdir`/`remove` cycle (proving the driver
wrote correct checksums); the mutated images also pass `e2fsck -f`
cleanly. A `pjdfstest`-equivalent POSIX suite and an end-to-end QEMU
vertical remain future work.
