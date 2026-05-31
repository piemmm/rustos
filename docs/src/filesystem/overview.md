# Filesystem overview

RustOS separates **filesystem policy** from **filesystem I/O**:

- **Policy** — path resolution, the mount table, the on-disk layout rules
  of `AGENTS.md` §16, and the §5.3 permission model — lives in
  `kernel/core::fs` (the VFS). It is architecture-neutral and depends only
  on `lib/abi`, `lib/caps`, and `kernel/sec`.
- **I/O** — reading and writing blocks and parsing an on-disk format —
  lives in the `drivers/filesystem/*` crates (`rustfs`, `ext4`, `fat32`)
  behind the [`Filesystem`] trait in `lib/abi`. The VFS never duplicates a
  driver's block I/O.

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
