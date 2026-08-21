# Filesystem overview

TAIRiX separates **filesystem policy** from **filesystem I/O**:

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
| `readlink_via` | Read a symbolic link's stored target.                |
| `symlink_via`  | Create a symbolic link with a stored target.         |
| `link_via`     | Add a second name for an existing node (hard link).  |
| `realpath_via` | Canonicalise a path: every link followed, every `..` applied. |

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
  `remove_via_secured`, `readlink_via_secured`, `symlink_via_secured`,
  `link_via_secured`, `realpath_via_secured`) instead read **each node's own stored §5.3
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
users-database load (`tairix_kernel_core::users::load_users_db`, see the
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
are rejected with `VfsError::InvalidPath`. A **caller-supplied** path
therefore cannot spell a `..` at all, so nothing a caller types can escape
the tree.

The one looser grammar is a *symbolic link's stored target* (below), which
is on-disk data rather than a caller's spelling; it is parsed by
`parse_link_target`, never by `Path::parse`, so widening it did not widen
the caller boundary.

## Symbolic links

A node may be a `NodeKind::Symlink`, whose content is a **path** rather
than bytes. Resolution follows one per component, under two fixed
fail-closed bounds — at most `SYMLINK_HOP_MAX` (40) hops and
`MAX_RESOLVE_STEPS` steps in one resolution — so a cycle or an over-long
chain is refused with `VfsError::LinkLoop` instead of being walked. These
are security bounds on untrusted on-disk structure, not capacities
(`AGENTS.md` §24.4). The full design, including the decisions this page
only summarises, is `plans/SYMLINKS.md`.

- **A link in an interior position is always followed**: such a component
  is being *used* as a directory, so what matters is what it names. Only
  the **final** component's treatment varies, selected by `FinalLink`:
  `Follow` is POSIX `stat`, `Keep` is POSIX `lstat`. `Keep` is what an
  `OpenFlags::NO_FOLLOW` descriptor carries, and it is re-derived from the
  handle (`FinalLink::for_open`) by every operation served for that
  descriptor — so a stat or a listing can never contradict the open that
  produced it. A dangling link is describable only under `Keep`;
  `Follow` reports it as `VfsError::NotFound`.
- **`..` is resolved physically, never lexically.** The walk keeps a stack
  of the real nodes it passed through and `..` pops that stack, so it names
  the directory the resolution actually came through rather than one a
  link's spelling suggests.
- **A link cannot resolve outside what its mount projects.** The stack
  starts at the root the covering mount projects and never pops past it, so
  an absolute target on a foreign volume resolves against *that mount's*
  root — a USB stick's link cannot name `/System/Security`. For a
  **sub-mount** (a subtree of a larger volume bound at a mount point, such
  as `/System/Logs`) the floor is the subtree's own root rather than the
  driver's, so a link stored inside one cannot reach the rest of the volume
  either, and every node a walk reaches has a path under the mount point.
  That totality is what makes canonicalisation (below) able to name every
  result.
- **Following a link never bypasses a permission check.** A spliced
  target's components are authorised exactly as typed ones are: search
  permission is required on every directory the resolution traverses,
  whoever supplied its name.
- **A link is not byte-readable.** Its target is reached only through
  `readlink_via`; asking an open for byte access to something that really
  is a link is refused with `Errno::LinkLoop`, and `FilesystemWrite::create`
  refuses `NodeKind::Symlink` — a link is created only by `create_link`,
  which carries the target.
- **A target is stored verbatim and never resolved at creation.** It is
  data, so creating a link authorises only the right to add a name in the
  link's own parent and grants no authority over what it names; authority
  is decided at each later use, per component. Its *grammar* is still
  checked before it is stored, so a target this resolver could never walk
  is refused rather than written.
- **A format with no link object type refuses rather than approximating
  one.** `FilesystemRead::read_link` and `FilesystemWrite::create_link`
  default to `DriverError::Unsupported`, which surfaces as
  `VfsError::NotSupported` — never a regular file whose contents merely
  look like a path.

### Which operations follow a final link

The posture is a property of the operation, not a default, so each one names
it. A walk reports the **place** its final name occupies — the directory
holding the name and the name itself — because the driver mutation surface is
keyed `(dir, name)` rather than by node; under `Follow` that is the *target's*
place, which is what makes a write reach the target rather than the link.

| Operation | Final link | Why |
|---|---|---|
| `read`, `write`, `truncate`, truncate-on-open, append | followed | POSIX acts on the file the link names. The write permission this VFS asks for on a write's parent therefore applies to the directory the target lives in, not the one the link sits in. |
| `stat`, `readdir`, `open` | per the descriptor | `FinalLink::for_open` fixes it once from `OpenFlags::NO_FOLLOW`; every operation served for that handle re-derives it. |
| `open` with `CREATE` | followed | Creating through a *dangling* link creates the file the link names, as POSIX specifies, rather than reporting the link's own name as taken. |
| `mkdir` | kept | POSIX `mkdir` over a link is `AlreadyExists`, live target or dangling. |
| `symlink` | kept | A new link never replaces an existing name. |
| `unlink`, `rmdir`, `rename` (both ends) | kept | The call is about the name as typed; removing or moving a link never touches what it names. |
| `readlink` | kept | The call is about the link. |
| `link` (existing name) | per the call | An empty `LinkFlags` keeps it, so the *link* gains the second name and one planted on the way cannot redirect it onto an object the caller never spelled (POSIX `link()`); `LinkFlags::FOLLOW` follows it, so its target does instead (`linkat(AT_SYMLINK_FOLLOW)`, what `ln -L` asks for). |
| `link` (new name) | kept | A name being created; a create never replaces an existing name. |
| `chmod`, `chown`, extended attributes | followed | POSIX applies these to the target. |


## Canonicalisation

`realpath_via{,_secured}` answers the **one** path that names what a path
resolves to: every symbolic link followed, every `..` applied to the nodes
the walk really traversed, and the answer spelled in the caller's own
namespace — the covering mount point followed by the canonical remainder.
It is the same walk every other operation uses, so a caller cannot obtain a
path the kernel would resolve elsewhere. `fs_realpath` (116) exposes it, and
`readlink -f`/`-e`/`-m` and `ln -r` are its userland consumers; a tool must
not canonicalise for itself, because a copy that disagreed by one rule would
print a path the kernel resolves differently.

`RealpathMode` chooses how much of the path must exist, and nothing else:

| Mode | Requirement | GNU switch |
| --- | --- | --- |
| `Existing` | every component exists | `readlink -e` |
| `Final` | every component but the last | `readlink -f` |
| `Missing` | none need exist | `readlink -m` |

A component that does not exist is carried into the answer unchanged, and a
`..` below the deepest existing node pops that carried tail — the only
reading available where no node exists to ascend from. Nothing above the
deepest existing node is looked up, so `Missing` never leaks existence past
a permission check: search permission on each traversed directory is proven
*before* the child is looked up, exactly as for any other walk.

The answer is always a path this VFS would accept back — at most
`MAX_PATH_COMPONENTS` components and `FS_PATH_MAX` bytes, else
`VfsError::InvalidPath` — so a caller can feed it straight into another
call without being refused for spelling.

## Per-format link support

| Format | Reads a symbolic link | Creates a symbolic link | Hard links |
|---|---|---|---|
| ARXFS  | yes — inode kind `3`, target stored as node data (`arxfs-spec.md` §20) | yes | creates them; `nlink` maintained per inode and storage freed only at zero (`arxfs-spec.md` §20.5) |
| ext4   | yes — both the fast (inline `i_block`) and slow (block-backed) spellings | no; this driver reads foreign links but does not author them | reports `i_links_count` and honours it on unlink; authors none |
| FAT32  | no — the format has no link object type | no | no — a directory entry *is* the file's identity |
| ADFS   | no — the format has no link object type | no | no — the format has no such object |

A "no" is `VfsError::NotSupported`: the permanent format (or driver) limit a
caller can tell apart from a structural refusal, never a substituted file.

## Hard links

A node may carry more than one name. Each is an ordinary directory entry
reaching one inode, so a write through either is visible through the other,
and the node's storage is released only when the **last** name is unlinked —
the count that decides it is the format's own (`NodeInfo::nlink`), carried up
to `FileStat` and printed by `ls -l`, never derived by the VFS, which could
not count names without walking every directory on the volume.

- **A directory is refused, in the VFS rather than per format.** The tree
  staying a tree is what makes physical `..` resolution well-defined, so
  `VfsError::IsADirectory` answers a directory operand on every backing.
- **A pair that crosses a mount is refused.** A directory entry addresses an
  inode in its own backing, so both paths must resolve under one mounted
  volume; `VfsError::CrossVolume` is the same refusal a rename gives, for the
  same reason, through the same check.
- **The count is a fixed format bound, not a capacity.** A create that would
  overflow it fails closed with `VfsError::TooManyLinks` rather than wrapping
  a count whose zero would free storage a live name still reaches.
- **A second name confers no authority.** It is authorised exactly as a
  create in its own parent (search plus write); nothing further is required
  of the caller against the node, and nothing further is granted.
- **A format that keeps no count answers `1`.** Such a format also has no
  second-name object, so one name is the whole truth rather than a floor
  (`NodeInfo::SINGLE_NAME`, the one definition every such driver reads).

[`Filesystem`]: ../abi/driver_traits.md
[`Metadata`]: ./permissions.md
[`Credentials`]: ./permissions.md
[`Path`]: ./overview.md
[`VfsError`]: ./overview.md
