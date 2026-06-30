# On-disk layout enforcement

This page mirrors `AGENTS.md` §16. The VFS in `kernel/core::fs` enforces
it; the installer (Stage 8) lays it out.

## Exactly four top-level directories

RustOS has **exactly four** top-level directories:

```
/
├── System/    # All OS-provided files. Read-only at runtime.
├── Users/     # One subdirectory per user account.
├── Apps/      # Installed application bundles.
└── Storage/   # Mount points for removable / extra volumes.
```

`Vfs::with_default_layout` provides exactly these and, beneath `/System`,
the two writable exceptions `Logs` and `Settings` (see below).

## Reserved legacy names

The following legacy POSIX names are **reserved and forbidden** as
top-level directories:

```
etc  home  usr  var  proc  sys  lib  lib64  bin
sbin opt   root tmp  dev   mnt  media run    boot
```

`Vfs::mkdir` (and `create_file`) refuse to create any of them directly
under the root, returning `VfsError::ReservedPath`. The same refusal
applies on the **driver-backed** create/mkdir/rename path, so once the
writable root volume backs `/` (see below) a delegated operation cannot
lay a reserved name onto the volume either — the ban is a structural
layout rule, not a permission, enforced before any driver write. The
reservation is **top-level only**: `/Users/tmp` is fine; `/tmp` is not.
There is no `/proc` and no `/sys`; live system information is exposed
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
`system_mount`) wires two on-disk `RustFs` volumes into that layout:

- the **encrypted, writable root volume** (`RustFsRoot`) is mounted as
  `/` (`MountTable::back_root`). It is the persistent home of `/`,
  `/Users`, `/Apps`, `/Storage`, and the writable `/System` exceptions;
- the **read-only, well-known-keyed `/System` volume** (`RustFsSystem`)
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
| `/Users/alice/notes`    | `RustFsRoot` (`/`) | yes       |
| `/Apps/Example.app/Run` | `RustFsRoot`       | yes       |
| `/System/Drivers/vesa`  | `RustFsSystem`     | no        |
| `/System/Logs/boot`     | `RustFsRoot`       | yes       |

This is **disjoint sub-mounting**, never a union/overlay "merge" of two
`/System` trees: no path is served by both volumes, so resolution stays
deterministic and fail-closed. Consequently the two volumes carry
**non-overlapping** content — the immutable `/System` subtree lives only
on `RustFsRoot`'s read-only sibling, and the encrypted root authors only
the writable-state subtree (`/System/Logs`, `/System/Settings`,
`/System/Security`) plus the four top-level directories.

Until the encrypted root is unlocked, the writable root driver is not yet
registered and every operation on `/` and its writable subtrees fails
closed (`Errno::NotImplemented`), never a silent fallback to the
read-only `/System`.
