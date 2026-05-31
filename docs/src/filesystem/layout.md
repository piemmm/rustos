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
under the root, returning `VfsError::ReservedPath`. The reservation is
**top-level only**: `/Users/tmp` is fine; `/tmp` is not. There is no
`/proc` and no `/sys`; live system information is exposed exclusively
through the System Information API (`AGENTS.md` §16.6, Stage 6).

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
