# rustfs driver

`rustfs` (`drivers/filesystem/rustfs`, crate `rustos-drv-fs-rustfs`) is the
**native RustOS filesystem**: a block-backed, journaled, copy-on-write
filesystem that stores full POSIX metadata plus an inline access-control
list and an optional capability gate **per inode** (`AGENTS.md` §5.3). It
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
size, between 512 and 4096 bytes, a power of two). The regions tile the
device in order; `RustFs::open` re-derives and validates the geometry from
the superblock:

| Region        | Contents                                                    |
| ------------- | ----------------------------------------------------------- |
| Superblock    | Block 0: magic, version, geometry, region offsets, root.    |
| Inode table   | Fixed 256-byte inode records; index 1 is the root.          |
| Data bitmap   | One bit per data block.                                     |
| Journal       | One header block plus a fixed-size redo-log data area.      |
| Data          | File and directory data blocks, and indirect-pointer blocks.|

Each inode holds 16 direct block pointers plus one single-indirect block,
so a file spans up to `16 + block_size/8` blocks. Directories are ordinary
block-addressed payloads of 64-byte slots (`inode`, `name_len`, name);
`.` and `..` are stored on disk and hidden from `read_dir`.

## Copy-on-write and journaling

`rustfs` keeps metadata and data consistent across a crash without
`fsck` (`AGENTS.md` §2.5):

- **File data is copy-on-write.** A write allocates a *fresh* data block,
  writes the new contents there, re-points the inode, and frees the old
  block. A crash before commit leaves the old block intact, so a reader
  never observes a torn block.
- **Metadata is journaled.** The data-block bitmap, inode-table blocks,
  directory blocks, and indirect blocks of a single operation are staged
  into the journal's redo-log area. Commit writes a checksummed record,
  then checkpoints each staged image to its home block. A mount **replays**
  a committed-but-un-checkpointed transaction and **discards** an
  uncommitted or checksum-mismatched one, so the metadata is always at a
  transaction boundary.

The staged block images live in the on-disk journal, not in RAM; only the
small list of home block numbers is held in memory.

## Operations

`FilesystemRead` provides `root`/`node_info`/`lookup`/`read_at`/`read_dir`;
`FilesystemWrite` provides `create`/`write_at`/`truncate`/`remove`/`flush`,
addressing a target as a `(dir, name)` pair. `write_at` extends files
(zero-filling sparse gaps), `truncate` shrinks (freeing the tail and
copy-on-write zeroing the partial last block) or grows, and `remove`
refuses a non-empty directory with `Busy`. A `NodeId` is the inode index;
node identity is stable across a remount.

## Capabilities

Loading requires `CAP_DRV_LOAD` at `register` time. The driver runs in
user space; it does not request `CAP_DRV_KERNEL`. The read/write methods
are reached only through the `DriverHandle` the host minted at load time,
and the VFS only delegates a write to a non-`READ_ONLY` mount.

## Test surface

`cargo test -p rustos-drv-fs-rustfs` formats an in-memory volume and
exercises: `format`/`open` round-trip and rejection of an unformatted
device; create/lookup/listing (including the buffer-size guard and the
`.`/`..` skip); read/write with block-boundary straddling and sparse
zero-fill; single-indirect large files across a remount; `truncate` shrink
and grow; `remove` and name reuse; the non-empty-directory `Busy` guard;
the per-inode security record (mode, owner, ACL, capability gate) round-
tripping across a remount; copy-on-write overwrite persistence; the
`register` capability gate; and a **crash-consistency sweep** that faults
the device after every possible write count during a journalled overwrite
and asserts the result is always either fully the old or fully the new
contents — never torn — with both outcomes observed.

The native journal crash-consistency soak and the `pjdfstest`-equivalent
POSIX suite remain tracked in `.junie/next-session-prompt.md`.
