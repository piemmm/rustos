# `tairix-drv-fs-ext4` — ext4 filesystem driver (read/write)

Stage 5 deliverable. Attaches an ext2/ext3/ext4 volume behind any
`tairix_abi::driver::block::Block` device and exposes it through the
versioned `tairix_abi::driver::filesystem::FilesystemRead` and
`FilesystemWrite` traits, and surfaces each inode's §5.3 owner/mode
through the versioned `FilesystemSecurity` trait.

The frozen `Filesystem` trait carries only `mount`/`unmount` and a
`DriverHandle` — it cannot perform I/O — so each surface is a **new
versioned trait** rather than a widening of the shipped one
(`AGENTS.md` §2.4 / §9), exactly as for the FAT32 and arxfs drivers.

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

## Writing

The `FilesystemWrite` surface (`create`, `write_at`, `truncate`,
`remove`, `flush`) mutates a mounted volume; like the read surface it
makes **no** permission decision (`AGENTS.md` §5.4 — the VFS authorises
first). New files/directories use the classic block map; data blocks
are taken from the block bitmap and inodes from the inode bitmap, with
the group-descriptor and superblock free counts kept in step. Directory
entries are inserted by splitting an existing record's slack (growing
the directory by a block when none fits) and removed by merging the
freed slot into its predecessor. `truncate` frees the tail of the
classic map and the extent map (the inline depth-0 root or a depth-1
tree) and zeroes the retained partial block.

A pre-existing extent-mapped file grows in place: `write_at` extends the
last extent when contiguous, otherwise appends a fresh extent, and once
the four inline `i_block` slots are exhausted it converts the root into
a **depth-1 tree** (extents move into a new leaf block; the root becomes
an index node) and attaches further leaves via new ascending-ordered
root index entries. `truncate`/`remove` free a depth-1 tree's emptied
leaves, drop their index entries, and collapse the root back to an empty
depth-0 node when none survive.

### Checksums and wide descriptors

The write path maintains every on-disk checksum a volume carries, so a
default `mkfs.ext4` image (`metadata_csum`, `extent`, `64bit`) is
mutated in place. All checksum primitives are **first-party** (a storage
checksum is not a cryptographic primitive, so §2.12's "never roll your
own" does not apply):

- **`metadata_csum`** (crc32c, reversed polynomial `0x82F6_3B78`,
  seeded with `crc32c(~0, s_uuid)`): the superblock `s_checksum`; each
  group descriptor `bg_checksum` (low 16 bits); the block- and
  inode-bitmap checksums (`bg_*_bitmap_csum_lo/hi`); each inode
  (`i_checksum_lo`/`i_checksum_hi`, seeded per inode by number and
  generation); each directory leaf's `ext4_dir_entry_tail`; and each
  allocated extent block's `ext4_extent_tail`.
- **`gdt_csum`/`uninit_bg`** (crc16, reversed polynomial `0xA001`): the
  legacy group-descriptor `bg_checksum`.
- **`64bit`**: the 64-byte group descriptor's high-half checksum and
  `bg_itable_unused` fields.

`bg_itable_unused` is lowered as inodes are allocated, and `remove`
marks the freed inode deleted (`i_links_count = 0`, `i_dtime`, zeroed
size/blocks) so a consistency check sees a freed — not orphaned — inode.

Mutation still **fails closed** (`DriverError::Unsupported`) on a
volume whose feature set the write path cannot maintain — anything
outside the supported `incompat`/`ro_compat` allow-list, e.g.
`bigalloc`, `meta_bg`, `inline_data`, or an explicit `checksum_seed`
(which would invalidate the uuid-derived seed) — and on a block group
whose bitmaps are not materialised (`BLOCK_UNINIT`/`INODE_UNINIT`).
Growing an extent tree beyond a single index level (depth ≥ 2), and
`remove`/`truncate` of a file whose mapping is neither the classic map
nor a depth-0/depth-1 extent tree, are likewise refused rather than
half-built or orphaning blocks (`AGENTS.md` §2.1 / §5.4). Such volumes
stay fully **readable**.

## Formatting (mkfs)

`Ext4::format(block, inode_count)` lays a fresh, empty volume onto a
blank `Block` device and returns it mounted (no `mkfs` shell-out —
`AGENTS.md` §12/§2.12). It is the write-side counterpart of `open()`,
which stays the single source of truth for the on-disk layout: after
`format()` the bytes are handed straight to `open()`.

The formatter writes a deliberately conservative on-disk shape the
read/write path fully supports:

- `filetype` + `extent` (`s_feature_incompat`); no read-only-compat
  features, **no** checksum (`metadata_csum`/`gdt_csum`), and **no**
  `64bit` feature, so no checksum maintenance is needed;
- 128-byte inodes, 32-byte group descriptors;
- block size 4096 bytes for volumes ≥ 128 MiB, else 1024 bytes;
  `blocks_per_group = 8 * block_size` (bitmap-maximal). Only whole
  groups are used, so the reader's group count is exact and no
  degenerate tail group appears;
- **every** block group fully materialised — no lazy/`UNINIT` groups —
  so the volume can be filled to exhaustion;
- the reserved inodes 1..=10 plus an extent-mapped empty root directory
  (inode 2); `inode_count` is the minimum total inode budget, rounded up
  to a whole number per group (≥ 16 per group).

Allocation past the volume's free data blocks or inode budget fails with
`DriverError::NoSpace` (POSIX `ENOSPC`), distinct from `DeviceFault`
(`AGENTS.md` §5.4 / §2.9). A device too small for one group plus a data
region, or a zero `inode_count`, is refused with `OutOfRange`.

## Limitations

- Mutation is gated to the feature allow-list above; an unsupported
  feature leaves the volume read-only (fail closed).
- `format()` writes the no-checksum, no-`64bit` subset above; it does
  not emit `metadata_csum`/`gdt_csum`, backup superblocks, or a journal.
  Files created on the volume use the classic block map, so a single
  file reaches at most 12 direct + one single-indirect block.
- Extent-tree growth is gated to a single index level (depth ≤ 1); a
  deeper tree is refused, never half-built (the read path still maps
  any depth).
- Hash-tree (`htree`) interior nodes are not traversed; only the linear
  leaf layout is read (sufficient for small and moderate directories).
- Devices, FIFOs, and sockets are reported as `NotFound` by `node_info`
  rather than surfaced as a node kind the `abi-v1` read surface does not
  model.
- Symbolic links are **read** in both on-disk spellings — fast (target
  inline in `i_block`) and slow (block-backed) — but not **authored**:
  `create_link` answers `Unsupported`, which the VFS surfaces as the
  permanent `NotSupported` limit rather than a substituted regular file.
  An inline-data link keeps its target in the inode's own extended-attribute
  area, which this driver decodes nowhere, so it answers `Unsupported` too.

## Security

ext4 stores a per-inode owner, mode, and ACL, but this driver makes
**no** permission decisions: the VFS metadata layer (`AGENTS.md` §5.3)
that mounts the driver is the policy point (§5.4 — this is raw
structural I/O). Case-folding and Unicode normalisation policy likewise
belong to the VFS, not the driver.

The driver implements `FilesystemSecurity`: `security(node)` reports a
`NodeSecurity` carrying the inode's POSIX mode (the low 12 bits, with
the type bits stripped), owner uid, and owner gid. The uid and gid each
recombine their low half (`i_uid`/`i_gid`) with the osd2 high half
(`l_i_uid_high`/`l_i_gid_high`). ext4 has no inline capability gate, so
the record carries no `required_cap`.

Named-user / named-group POSIX ACL grants are decoded from the inode's
`system.posix_acl_access` extended attribute into the record's inline
ACL entries. Both storage forms are read: the **inline** region in an
enlarged inode's tail (after `i_extra_isize`, value offsets relative to
the first entry) and the **external block** named by `i_file_acl` (value
offsets relative to the block start). `ACL_USER`/`ACL_GROUP` entries
become one grant-only `SecurityAcl` each; the owner/owning-group/other/
mask entries are already expressed by the mode bits and are skipped. An
absent or malformed attribute simply contributes no grants (the mode
bits still apply — §5.4, fail closed).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- The `FilesystemRead` methods are reached only through the
  `DriverHandle` the host minted at load time. The driver runs in user
  space; it does not request `CAP_DRV_KERNEL`.

## Test surface

`cargo test -p tairix-drv-fs-ext4` builds a specification-shaped,
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
- `FilesystemSecurity::security` for a regular file (mode, and a uid/gid
  that span both the low and osd2 high halves) and a directory, plus the
  `NotFound` guard.
- **POSIX ACLs**: standalone `decode`/`find` units (both value-base
  conventions, bad version, the inline-budget cap, unrelated
  attributes), and end-to-end `security` reads of an external xattr
  block, a garbage block, and an inline ACL in a 256-byte-inode volume.
- **Writing**: create + multi-block `write_at` round-tripping across a
  remount; duplicate / invalid-name / non-directory `create` rejection;
  sparse extension past EOF; `truncate` shrink-then-grow; directory
  creation (`.`/`..`) and removal (empty vs. `Busy`); inode reuse after
  `remove`; `write_at`/`truncate` directory and not-found guards;
  free-inode exhaustion; the fail-closed refusal of mutation on an
  unsupported (`checksum_seed`) feature set; and the depth-0 → depth-1
  extent-tree growth (sparse writes overflowing the inline root,
  read-back + remount persistence, a sparse hole between extents, and
  depth-1 `truncate`-to-zero and `remove` freeing and reusing the tree).
- The `register` capability gate and `into_block` round-trip.

44/44 in-tree unit tests pass. A separate integration suite
(`tests/checksummed.rs`, 5 tests) mutates **real `mke2fs 1.47.0`**
`metadata_csum` and `gdt_csum` fixtures (committed under
`tests/fixtures/`) and re-verifies every on-disk checksum with an
*independent* crc implementation — both on the pristine image (proving
the reference crc matches `mke2fs`) and after a
`create`/`write`/`truncate`/`mkdir`/`remove` cycle (proving the driver
wrote correct checksums). The mutated images also pass `e2fsck -f`
cleanly. A `pjdfstest`-equivalent POSIX suite is tracked in
`.junie/next-session-prompt.md`.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `Ext4`
type is exported so the driver host can construct an instance with
`Ext4::open`; the host reaches into it only through the `FilesystemRead`,
`FilesystemWrite`, and `FilesystemSecurity` traits.
