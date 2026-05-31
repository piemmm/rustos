# Filesystem overview

RustOS separates **filesystem policy** from **filesystem I/O**:

- **Policy** — path resolution, the mount table, the on-disk layout rules
  of `AGENTS.md` §16, and the §5.3 permission model — lives in
  `kernel/core::fs` (the VFS). It is architecture-neutral and depends only
  on `lib/abi`, `lib/caps`, and `kernel/sec`.
- **I/O** — reading and writing blocks and parsing an on-disk format —
  lives in the `drivers/filesystem/*` crates (`rustfs`, `ext4`, `fat32`)
  behind the [`Filesystem`] trait in `lib/abi`. The VFS never duplicates a
  driver's block I/O. The frozen `Filesystem` trait is mount/unmount only;
  path I/O delegates to a driver through the separate versioned
  `FilesystemRead` and `FilesystemWrite` traits (`AGENTS.md` §2.4 / §9).
  The first block-backed driver is the read/write [FAT32 driver](./fat32.md);
  the native [rustfs driver](./rustfs.md) adds a journaled, copy-on-write
  filesystem that stores per-inode ACLs and capability gates.

This page describes the VFS. The on-disk layout it enforces is in
[Layout](./layout.md); the permission model is in
[Permissions](./permissions.md).

## The VFS tree

The VFS is a tree of inodes. Each inode is either a **directory** (a map
from name to child inode) or a **file** (a byte payload), and carries the
access-control [`Metadata`] described in [Permissions](./permissions.md).

Before a block-backed driver mounts, the tree is held in RAM — the natural
shape of the boot-time root filesystem. The structure and every policy
check are identical regardless of what eventually backs a subtree, so a
later block-backed mount changes *where the bytes live*, not *how access is
decided*.

## Operations

All operations take the caller's [`Credentials`] (uid, primary gid,
supplementary gids, and capability set) and an absolute [`Path`]:

| Operation             | Effect                                            |
| --------------------- | ------------------------------------------------- |
| `metadata`            | Stat an inode (search permission on the path).    |
| `mkdir`               | Create a directory.                               |
| `create_file`         | Create a regular file.                            |
| `read` / `write`      | Read or replace a file's contents.                |
| `list`                | List a directory's entry names.                   |
| `remove`              | Remove an empty directory or a file.              |
| `set_required_cap`    | Set the per-inode capability gate (owner only).   |

Every operation resolves the path component by component, enforcing
**search (execute) permission** on each directory it descends through, and
then applies the access check appropriate to the operation. All failures
are reported as a [`VfsError`]; `VfsError::to_errno` maps them to the
stable user/kernel `Errno` at the syscall boundary.

## Driver delegation

A subtree can be backed by a `drivers/filesystem/*` driver instead of the
in-RAM arena. The mount records the driver's `DriverHandle`, and the VFS
exposes delegating operations that take the live driver the kernel host
maps from that handle — the read ones a `&mut dyn FilesystemRead`, the
mutating ones a driver implementing both `FilesystemRead` and
`FilesystemWrite`:

| Operation      | Effect                                               |
| -------------- | ---------------------------------------------------- |
| `read_via`     | Read a file under a driver-backed mount.             |
| `list_via`     | List a directory under a driver-backed mount.        |
| `stat_via`     | Stat a node under a driver-backed mount.             |
| `create_via`   | Create a file under a driver-backed mount.           |
| `mkdir_via`    | Create a directory under a driver-backed mount.      |
| `write_via`    | Write a file under a driver-backed mount.            |
| `truncate_via` | Set a file's length under a driver-backed mount.     |
| `remove_via`   | Unlink a child under a driver-backed mount.          |

Each walks the in-RAM tree to the mount point — authorising **search
permission on every ancestor** — then hands the remaining path components
to the driver through `DelegatedFs`. The driver returns *structural* I/O
only (node kind, size, children, bytes); it makes **no** permission
decision (`AGENTS.md` §5.4). Every delegated node inherits the mount
point's [`Metadata`] as a uniform permission template — the natural model
for a filesystem such as FAT that stores no per-file owner — and the §5.3
check runs against it before each read, before each mutation (which also
requires **write** permission on the parent and refuses a `READ_ONLY`
mount), and on every directory descended into. An unrecoverable driver
fault, or a directory entry whose on-disk name is not valid UTF-8,
surfaces as `VfsError::Io`.

The whole driver-backed read **and** write path is exercised end-to-end
under QEMU against a real (emulated) virtio-blk device: the
`fat32_virtio_blk_pci_x86_64` vertical mounts a planted FAT32 image
through the FAT32 driver and round-trips a read and a write (see
[FAT32](./fat32.md)).

## Path resolution

A [`Path`] is absolute and normalised at parse time: relative paths, empty
or over-long components, `.`/`..` traversal tokens, and embedded NUL bytes
are rejected with `VfsError::InvalidPath`. Because the path type cannot
represent a `..`, resolution can never escape the tree — there is no
traversal logic to get wrong.

[`Filesystem`]: ../abi/driver_traits.md
[`Metadata`]: ./permissions.md
[`Credentials`]: ./permissions.md
[`Path`]: ./overview.md
[`VfsError`]: ./overview.md
