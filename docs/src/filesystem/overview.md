# Filesystem overview

RustOS separates **filesystem policy** from **filesystem I/O**:

- **Policy** — path resolution, the mount table, the on-disk layout rules
  of `AGENTS.md` §16, and the §5.3 permission model — lives in
  `kernel/core::fs` (the VFS). It is architecture-neutral and depends only
  on `lib/abi`, `lib/caps`, and `kernel/sec`.
- **I/O** — reading and writing blocks and parsing an on-disk format —
  lives in the `drivers/filesystem/*` crates (`arxfs`, `ext4`, `fat32`)
  behind the [`Filesystem`] trait in `lib/abi`. The VFS never duplicates a
  driver's block I/O. The frozen `Filesystem` trait is mount/unmount only;
  path I/O delegates to a driver through the separate versioned
  `FilesystemRead` and `FilesystemWrite` traits (`AGENTS.md` §2.4 / §9).
  The first block-backed driver is the read/write [FAT32 driver](./fat32.md);
  the native [arxfs driver](./arxfs.md) adds a copy-on-write
  filesystem that stores per-inode ACLs and capability gates; the
  read/write [ext4 driver](./ext4.md) reads ext2/ext3/ext4 volumes,
  mutates the unchecksummed feature set, and surfaces each inode's stored
  owner/mode through `FilesystemSecurity`.

This page describes the VFS. The on-disk layout it enforces is in
[Layout](./layout.md); the permission model is in
[Permissions](./permissions.md). The native [arxfs](./arxfs.md) format —
copy-on-write today, growing always-encrypted, checksummed, compressed, and
deduplicating storage in stages behind these same traits — is the one
native format; there is no separate `v1`.

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
decision (`AGENTS.md` §5.4). The §5.3 check runs against a node's
[`Metadata`] before each read, before each mutation (which also requires
**write** permission on the parent and refuses a `READ_ONLY` mount), and on
every directory descended into. An unrecoverable driver fault, or a
directory entry whose on-disk name is not valid UTF-8, surfaces as
`VfsError::Io`.

Where that `Metadata` comes from is the one place the two policies differ:

- The `*_via` methods apply the **mount point's** `Metadata` as a uniform
  template to every node — the natural model for a filesystem such as FAT
  that stores no per-file owner.
- The `*_via_secured` counterparts (`read_via_secured`,
  `list_via_secured`, `stat_via_secured`, `create_via_secured`,
  `mkdir_via_secured`, `write_via_secured`, `truncate_via_secured`,
  `remove_via_secured`) instead read **each node's own stored §5.3
  record** through the driver's `FilesystemSecurity` surface and translate
  it (`Metadata::from_node_security`). The kernel host calls these for a
  driver such as [arxfs](./arxfs.md) that stores full per-inode owner,
  mode, ACL, and capability gate — so a file marked owner-only or gated on
  a capability is enforced as stored regardless of the mount template — or
  the [ext4 driver](./ext4.md), which reports each inode's stored owner and
  mode (its ACLs live in xattr blocks not yet decoded) and likewise
  serves the mutating `*_via` operations on writable volumes.

Both routes feed the *same* `Metadata::authorize` decision, so the policy
is single-sourced; only the metadata's origin changes.

A driver-backed mount normally sits *below* the root (`/Storage/usb0`,
…), but the **root mount itself** can also be given a backing driver
(`MountTable::back_root`, exactly once — a second root volume is refused)
— the shape of a real installation, whose root volume carries the whole
`AGENTS.md` §16 tree from its own root directory. The kernel's boot-time
users-database load (`rustos_kernel_core::users::load_users_db`, see the
[kernel page](../architecture/kernel.md)) reads
`/System/Security/Users` through exactly this path.

The whole driver-backed read **and** write path is exercised end-to-end
under QEMU against a real (emulated) virtio-blk device by three verticals:
the `fat32_virtio_blk_pci_x86_64` vertical mounts a planted FAT32 image
through the FAT32 driver (see [FAT32](./fat32.md)), the
`arxfs_virtio_blk_pci_x86_64` vertical mounts a planted arxfs volume —
one the arxfs driver itself authored — through the arxfs driver (see
[arxfs](./arxfs.md)), and the `users_db_qemu_aarch64` vertical mounts a
planted users-root arxfs volume on the aarch64 `virt` board and drives
the kernel's users-database load against it. The first two round-trip a
read and a write through the shared, transport-generic device tail.

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
