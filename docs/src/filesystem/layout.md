# On-disk layout enforcement

This page mirrors `AGENTS.md` §16. The VFS in `kernel/core::fs` enforces
it; the installer (Stage 8) lays it out.

## Exactly four default root-view entries

The default session root view (`/`) has **exactly four** entries:

```
/
├── System/    # All OS-provided files. Read-only at runtime.
├── Users/     # One subdirectory per user account.
├── Apps/      # Installed application bundles.
└── Storage/   # Catalog/view of published non-core storage roots.
```

These are synthetic view bindings backed by the first-class aliases
`System:`, `Users:`, `Apps:`, and `Storage:` — the canonical identity of a
storage root is its root ID or alias path, not the `/` view path
(`AGENTS.md` §16.1; the binding model is the storage-namespace spec,
[Storage namespaces, volume roots, and aliases](./drives.md)). The
single-root VFS this page describes is the *current realization* of that
default `/` view; the forest-of-roots model it projects lands with the
resolver / open-a-path stage tracked in that spec.

`Vfs::with_default_layout` provides exactly these four entries and,
beneath `/System`, the two writable exceptions `Logs` and `Settings` (see
below).

## Legacy POSIX names: the OS never authors them

The legacy POSIX top-level names —

```
etc  home  usr  var  proc  sys  lib  lib64  bin
sbin opt   root tmp  dev   mnt  media run    boot
```

— are names the **OS itself never creates**. `Vfs::with_default_layout`
lays out exactly the four permitted directories and nothing else, the
image builder authors only those four (`tools/mkimage`), and the
installer refuses to lay any legacy name out (`AGENTS.md` §11). No
in-tree component hard-codes one of these paths.

This is a rule the OS keeps to, **not** a structural ban the kernel
imposes on userland. The VFS does **not** police a user's own request:
with write permission on the root directory a caller may `mkdir /etc`
like any other directory — ordinary owner/mode/ACL permission on `/`
governs it, exactly as for a non-legacy name, with no separate
capability and no `VfsError` reserved for the name. Because production
`/` is owned by the system user with a restrictive mode, an unprivileged
user cannot create a top-level entry of *any* name; a privileged one
may.

There is no OS-provided `/proc` and no `/sys`: the OS does not create
them and nothing relies on them. Live system information is exposed
exclusively through the System Information API (`AGENTS.md` §16.6,
Stage 6).

## Read-only `/System`

`/System` is mounted read-only at runtime. Its only writable paths are
`/System/Logs` and `/System/Settings`, which are separate child mounts
flagged `nosuid,nodev,noexec`.

The mount table resolves the **longest mount-point prefix** of a path, so
a child mount shadows its parent:

| Path                     | Covering mount   | Writable? |
| ------------------------ | ---------------- | --------- |
| `/System/Drivers/vesa`   | `/System`        | no        |
| `/System/Logs/boot`      | `/System/Logs`   | yes       |
| `/System/Settings/host`  | `/System/Settings` | yes     |

Because creating or removing an entry mutates the *parent* directory, the
writability of a create/remove is governed by the **parent's** covering
mount. This is why removing the `/System/Logs` mount point itself is
refused (its parent `/System` is read-only), while writing a file *inside*
`/System/Logs` is allowed. A write to a read-only location returns
`VfsError::ReadOnly`.

## Default mount policy

`Vfs::with_default_layout` installs the `AGENTS.md` §16.2 / §16.3 mount
flags:

| Mount               | Flags                     |
| ------------------- | ------------------------- |
| `/System`           | `ro`                      |
| `/System/Logs`      | `nosuid,nodev,noexec`     |
| `/System/Settings`  | `nosuid,nodev,noexec`     |
| `/Users`            | `nosuid,nodev`            |
| `/Apps`             | `nosuid,nodev`            |
| `/Storage`          | `nosuid,nodev,noexec`     |

## Boot-time volume layering: writable root, read-only `/System` shadow

`with_default_layout` is the in-RAM shape before any disk is mounted. At
boot the production `fs_*` mount table (`kernel/rustos-kernel`'s
`system_mount`) wires two on-disk `ARXFS` volumes into that layout:

- the **encrypted, writable root volume** (`ARXFSRoot`) is mounted as
  `/` (`MountTable::back_root`). It is the persistent home of `/`,
  `/Users`, `/Apps`, `/Storage`, and the writable `/System` exceptions;
- the **read-only, well-known-keyed `/System` volume** (`ARXFSSystem`)
  is mounted *over* it at `/System`, carrying the immutable kernel image,
  drivers, and libraries.

The writable `/System/Logs` and `/System/Settings`, and the flag-bearing
`/Users` / `/Apps` / `/Storage`, are then attached to the *same* writable
root volume (`MountTable::set_backing`), each rebased onto its own
same-named path on that volume. They exist as their own mounts only to
carry stricter flags than `/` and, under `/System`, to shadow the
read-only volume. Longest-prefix resolution gives every path exactly one
covering volume:

| Path                    | Resolves to        | Writable? |
| ----------------------- | ------------------ | --------- |
| `/Users/alice/notes`    | `ARXFSRoot` (`/`) | yes       |
| `/Apps/Example.app/Run` | `ARXFSRoot`       | yes       |
| `/System/Drivers/vesa`  | `ARXFSSystem`     | no        |
| `/System/Logs/boot`     | `ARXFSRoot`       | yes       |

This is **disjoint sub-mounting**, never a union/overlay "merge" of two
`/System` trees: no path is served by both volumes, so resolution stays
deterministic and fail-closed. Consequently the two volumes carry
**non-overlapping** content — the immutable `/System` subtree lives only
on `ARXFSRoot`'s read-only sibling, and the encrypted root authors only
the writable-state subtree (`/System/Logs`, `/System/Settings`,
`/System/Security`) plus the four top-level directories.

Until the encrypted root is unlocked, the writable root driver is not yet
registered and every operation on `/` and its writable subtrees fails
closed (`Errno::NotImplemented`), never a silent fallback to the
read-only `/System`.
