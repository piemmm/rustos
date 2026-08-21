# DRIVES.md — TAIRiX drive, volume, alias, and path namespace brief

This file is the AI-facing design brief that seeded the TAIRiX storage-namespace
specification. **The binding spec has been produced: `docs/src/filesystem/
drives.md`** (with the `AGENTS.md` §16.1 amendment it required). This brief is
retained only as the source material behind that spec; where the two differ,
the spec in `docs/src/` wins. The remaining P4 work is the descriptor-producing
open-a-path ABI (see `plans/SHELL.md`).

The goal is to replace the Unix habit of making every storage device reachable
only through one persistent root filesystem tree. TAIRiX should keep Unix-like
`/` separators and readable paths, but storage roots must be first-class objects
that survive independently of any one root view.

---

## 1. Prompt to give another AI

Use the following prompt when generating the full spec:

```text
You are writing a TAIRiX design/specification document for drive names,
volume roots, aliases, path parsing, and mount/view semantics.

Read AGENTS.md and this DRIVES.md. Produce a precise spec suitable for
`docs/src/filesystem/drives.md` plus a list of exact AGENTS.md sections that
must be amended. The spec must be consistent with TAIRiX principles: Rust-only
implementation, microkernel-leaning storage services, capability-checked
filesystem operations, no ambient authority, user-space drivers where feasible,
no `/proc` or `/sys` virtual filesystem, no POSIX legacy top-level directories,
no target-specific generic code, no C code, no duplicated parsing logic, and no
single root filesystem dependency for discovering or naming other volumes.

The design direction is:

- Storage roots are a forest, not one global Unix tree.
- A root is named by a resolver and root selector, e.g. `id::<volume-id>/path`,
  `alias::Home/path`, `fs::arxfs/<root>/path`.
- User-facing shorthand uses `Alias:/path`, e.g. `Home:/Documents/spec.md`.
- The first-class user idea is the alias: a named root binding such as
  `System:`, `Users:`, `Home:`, `Apps:`, `Backup:`, or `CameraCard:`.
- `/` remains available only as a synthetic session compatibility view assembled
  from aliases; it must never be the canonical identity of storage.
- `/Storage/<volume>` compatibility remains possible only as a view projection
  of independent roots. It must not be the native model.
- Filesystem-backed namespaces such as `adfs::HardDisc4/path` may exist for
  diagnostics, compatibility, recovery, and filesystem-specific tooling, but
  normal durable paths should use `id::` or aliases, not filesystem-driver names.

The final spec must define syntax, grammar, resolver semantics, alias policy,
capabilities, failure modes, boot/discovery lifecycle, installer defaults,
security invariants, tests, and examples. It must explicitly identify every
current AGENTS.md rule it changes or preserves.
```

---

## 2. Design problem

TAIRiX currently wants a clean filesystem layout, but the existing AGENTS.md
wording is still Unix-shaped in one important respect: the installed system is
described as a single `/` tree containing `/System`, `/Users`, `/Apps`, and
`/Storage`.

That is familiar, but it recreates the central fault-tolerance weakness the new
design is meant to avoid: if access to the root tree fails, every path that
depends on that root view fails with it, even when the underlying volumes are
healthy.

TAIRiX should separate these concepts:

- the identity of a storage root;
- the filesystem driver used to read it;
- the user-facing name for it;
- the synthetic `/` view shown to POSIX-ish tools and simple shells;
- the capabilities that allow a process to resolve, open, mutate, publish, or
  reconfigure it.

The final spec should not merely rename Unix mountpoints. It should define a
storage namespace model where independent roots can be discovered, named,
authorized, and opened without first walking a persistent root filesystem.

---

## 3. Current AGENTS.md constraints to preserve or amend

The final spec must explicitly reconcile these charter facts.

### 3.1 Preserve

TAIRiX remains Rust-only. No C or C++ source, hand-written headers, or C build
glue may be introduced as part of this design.

TAIRiX remains microkernel-leaning. Drivers run in user space wherever feasible.
Storage and filesystem drivers should follow the existing driver model unless a
bootstrap-floor exception is justified.

Capabilities remain mandatory. Mounting, publishing roots, creating aliases,
relaxing mount flags, reading privileged volume metadata, and seeing system-wide
storage inventory all require explicit capability checks.

Filesystem permissions remain mode bits plus ACLs plus optional capability
requirements. Any path resolution API is only a name-to-object step; actual
open/read/write/mutate operations still enforce inode permissions, ACLs,
capability gates, flags, and MAC policy.

There is still no `/proc` and no `/sys`. Runtime storage information must be
available through typed System Information API queries, not a virtual filesystem
made of text files.

The default installed layout remains secure: OS files read-only, user files
`nosuid,nodev`, applications capability-gated, removable or extra storage
`nosuid,nodev,noexec` unless explicitly relaxed by capability.

The existing driver-store model remains: loadable drivers live under the signed
installed driver store and are discovered by the device manager. The bootstrap
floor exists only to reach the store and must stay minimal.

### 3.2 Amend

AGENTS.md §16.1 currently says TAIRiX has exactly four top-level directories:
`/System`, `/Users`, `/Apps`, and `/Storage`. The new spec should replace this
with exactly four default **view entries** in the default session root view,
backed by first-class aliases:

```text
/System   -> System:/
/Users    -> Users:/
/Apps     -> Apps:/
/Storage  -> Storage:/       # synthetic catalog/view, not a storage parent
```

The canonical storage names become aliases and IDs, not `/` paths:

```text
System:/Kernel/tairix.rxe
Users:/ian/Documents/design.md
Apps:/Example.app/Run
Backup:/snapshots/2026-06-20
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/snapshots/2026-06-20
```

`/Storage/<volume>` remains a compatibility projection only:

```text
/Storage/Backup        -> Backup:/
/Storage/CameraCard    -> CameraCard:/
```

A process must be able to open `Backup:/file` even if the synthetic `/` view or
`Storage:/` catalog is absent, corrupt, or deliberately hidden.

---

## 4. Core recommendation

Use a **forest of named roots** with an alias-first user experience.

Human shorthand:

```text
Alias:/path/inside/root
```

Expanded resolver form:

```text
alias::Alias/path/inside/root
```

Stable canonical form:

```text
id::<stable-volume-id>/path/inside/root
```

Administrative filesystem-driver form:

```text
fs::<driver>/<root-selector>/path/inside/root
```

Optional driver shorthand:

```text
arxfs::System/path
fat32::EFI/BOOT/BOOTX64.EFI
adfs::HardDisc4/Apps/Paint
```

The shorthand `Alias:/path` should be the default shell, GUI, and documentation
form. `id::` should be the durable form for persistent machine records,
boot-critical references, recovery logs, and any place where mutable aliases are
not stable enough.

---

## 5. Terminology

The final spec should use these terms consistently.

### 5.1 Storage object

A physical or virtual thing capable of containing bytes or blocks: disk,
partition, block device, network share, disk image, encrypted container, memory
backing store, or host-provided WASM storage object.

A storage object is not necessarily a filesystem. It may require partition
parsing, decryption, decompression, or driver attachment before it yields a
volume root.

### 5.2 Filesystem driver

A driver that interprets an on-disk or remote filesystem format: `arxfs`,
`fat32`, `ext4`, `iso9660`, `adfs`, etc.

The filesystem driver is implementation detail for normal paths. It may be
named in administrative paths, recovery tools, test fixtures, and compatibility
layers.

### 5.3 Volume root

A resolved root directory object produced by attaching a filesystem driver to a
storage object or remote export.

A volume root is independently addressable. It is not born as a directory under
another volume.

### 5.4 Volume ID

A stable, non-human identifier for a volume root. The final spec should choose
an exact representation, probably a UUID-like or content-derived identifier,
carried by `lib/abi` as a fixed-size type.

Example:

```text
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/Kernel/tairix.rxe
```

### 5.5 Alias

A human-facing root binding. An alias maps a short name to a root or view.

Examples:

```text
System:/
Users:/
Home:/
Apps:/
Backup:/
CameraCard:/
```

Aliases are the main user idea. They are closer to Amiga assigns or RISC OS disc
names than to Unix mountpoints. They are not drive letters. They are not stored
as directories under `/`.

### 5.6 Resolver

The namespace component before `::`. It selects how the following root selector
is interpreted.

Examples:

```text
alias::Home/Documents/file.txt
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/Documents/file.txt
fs::arxfs/System/Kernel/tairix.rxe
arxfs::System/Kernel/tairix.rxe
```

`::` names a resolver. Some resolvers are filesystem-backed; most are not.

### 5.7 View

A synthetic directory tree assembled from roots, aliases, and service-provided
entries. `/` is a view. `Storage:/` may also be a view.

A view is a convenience surface. It is never the canonical identity of storage.

### 5.8 Mount

The final spec should avoid using `mount` as the primary abstraction. Use more
precise verbs:

- `discover` a storage object;
- `attach` a filesystem driver to produce a volume root;
- `publish` a root into a namespace;
- `alias` a root under a human name;
- `project` an alias into a view.

If the word `mount` remains for user familiarity, define it as a high-level
operation that may perform attach + publish + alias + view projection, not as
"place this device under a directory in `/`".

---

## 6. Path syntax

### 6.1 Native user syntax

```text
Alias:/path/inside/root
```

Examples:

```text
System:/Kernel/tairix.rxe
System:/Drivers/storage/arxfs/Driver.rxe
Users:/ian/Documents/DRIVES.md
Home:/Documents/DRIVES.md
Apps:/Editor.app/Run
Backup:/snapshots/latest/Users/ian
CameraCard:/DCIM/100MEDIA/IMG_0001.RAW
```

Rules:

- `Alias:/` is absolute from the alias root.
- `Alias:path` without `/` is invalid. Do not copy Windows `D:relative` rules.
- Alias lookup is case-sensitive unless the final spec deliberately defines a
  separate display-only case-folding rule.
- The alias name is not a directory component.
- The first `/` after `:` marks the root boundary.
- `..` cannot escape above the alias root.

### 6.2 Expanded alias syntax

```text
alias::Alias/path/inside/root
```

Examples:

```text
alias::Home/Documents/DRIVES.md
alias::System/Kernel/tairix.rxe
alias::Backup/snapshots/latest
```

This is equivalent to `Alias:/path/inside/root` after parsing.

The final spec should decide whether to permit both spellings everywhere or to
make `alias::` the normalized internal string form and `Alias:/` the shell/UI
form. Recommended: permit both at API boundaries, normalize to a typed root
handle internally, and display `Alias:/path` to humans.

### 6.3 Stable ID syntax

```text
id::<volume-id>/path/inside/root
```

Examples:

```text
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/Kernel/tairix.rxe
id::2f5a2e1c-9a3b-4d6b-9e4e-18d76e21cafe/ian/Documents/DRIVES.md
```

Use this for durable machine references. A volume label or alias may change; the
volume ID should not.

### 6.4 Filesystem-driver syntax

Administrative syntax:

```text
fs::<driver>/<root-selector>/path/inside/root
```

Optional shorthand:

```text
<driver>::<root-selector>/path/inside/root
```

Examples:

```text
fs::arxfs/System/Kernel/tairix.rxe
fs::fat32/EFI/BOOT/BOOTX64.EFI
fs::adfs/HardDisc4/Apps/Paint
arxfs::System/Kernel/tairix.rxe
fat32::EFI/BOOT/BOOTX64.EFI
adfs::HardDisc4/Apps/Paint
```

Filesystem-driver paths are not preferred for durable application state. They
are valuable for:

- recovery shells;
- filesystem check/repair tools;
- import/export tools for foreign media;
- tests of filesystem drivers;
- diagnostics that must select a particular driver;
- compatibility with historical naming conventions.

### 6.5 Device syntax

The final spec may define an administrative `dev::` resolver, but it must be
capability-gated and must not become a general file path namespace.

Example diagnostic use:

```text
dev::block/nvme0n1p2
```

A raw device path should not imply filesystem traversal. Opening raw storage
requires explicit authority. Normal users should see aliases and volume names,
not bus paths.

### 6.6 Network syntax

Network shares should become roots and aliases, not special strings buried in
paths.

Examples:

```text
net::nas.local/projects/TAIRiX/design.md
Projects:/TAIRiX/design.md
```

`net::` may exist as a resolver for explicit remote roots, but ordinary users
should usually interact with published aliases such as `Projects:` or `Team:`.

### 6.7 The synthetic `/` view

`/` may exist for scripts, POSIX-ish tools, file browsers, and users who expect
a tree. It is a view assembled from aliases.

Recommended default view:

```text
/
├── System/   -> System:/
├── Users/    -> Users:/
├── Apps/     -> Apps:/
└── Storage/  -> Storage:/
```

`/` must not be a directory stored on the System volume. It must be generated by
the namespace manager from the process/session namespace.

Native TAIRiX APIs should not require a path to start at `/`. A process current
directory should be `(root_handle, directory_handle)`, not merely a string.

---

## 7. Alias model

### 7.1 Alias classes

The final spec should define at least these alias classes.

#### Machine aliases

Configured by the installer, updater, or administrator. Examples:

```text
System:
Users:
Apps:
Storage:
EFI:
Recovery:
```

Machine aliases are visible system-wide unless a process namespace masks them.

#### User/session aliases

Created at login or by the desktop/session manager. Examples:

```text
Home:       -> Users:/ian
Desktop:    -> Users:/ian/Desktop
Documents:  -> Users:/ian/Documents
Downloads:  -> Users:/ian/Downloads     # only if Downloads is adopted
```

A user/session alias must never widen authority. It is a convenient name for a
root or subroot the user already has permission to reach.

#### Volume aliases

Published for removable disks, extra internal disks, network shares, and
administrator-created roots. Examples:

```text
Backup:
CameraCard:
Projects:
Scratch:
```

A volume alias should be sanitized, unique within its namespace, and auditable.

#### Service aliases

Used by system services where a direct alias is cleaner than a submount under
another alias. Examples:

```text
Logs:
Settings:
Spool:
```

The final spec must decide whether service aliases are user-visible or reserved
for system components. If projected into `System:/Logs` or `/System/Logs`, they
remain view bindings, not canonical subdirectories stored inside `System:`.

### 7.2 Alias target types

An alias may target:

- a full volume root;
- a subroot within a volume;
- a synthetic view;
- a search list of roots for read-only lookup, if the final spec accepts
  multi-target aliases.

Recommended rule for multi-target aliases:

```text
read/search: allowed in defined order
write/create: denied unless exactly one write target is configured
```

This gives Amiga-style assign convenience without ambiguous writes.

### 7.3 Alias scopes

Alias tables should be layered:

```text
kernel bootstrap namespace
machine namespace
login/session namespace
process namespace
sandbox namespace
```

Resolution checks the most specific namespace first, then falls back according
to explicit policy.

A sandbox may receive a namespace containing only one alias, such as:

```text
Input:/
Output:/
```

This allows strong confinement without inventing a fake `/tmp` or `/proc`.

### 7.4 Alias persistence

Alias resolution must not depend solely on one root filesystem.

Recommended persistence sources:

1. Volume self-description: stable volume ID, label, role, filesystem type,
   creation time, and optional signed role metadata.
2. Machine alias policy: signed mapping from alias names to volume IDs, stored
   in a location chosen by the installer and mirrored where practical.
3. Session alias policy: generated at login from user records and capabilities.
4. Rebuild fallback: if machine alias policy is unavailable, scan discovered
   volumes and publish unambiguous safe aliases from volume metadata.

The key invariant:

```text
A healthy non-System volume with a valid ID remains openable through `id::` even
when the System volume, the `/` view, or the machine alias policy is unavailable.
```

### 7.5 Alias conflicts

If two roots claim the same alias or label:

- durable `id::` paths remain valid;
- the ambiguous alias must fail closed or require explicit disambiguation;
- the namespace manager may publish disambiguated display aliases, but must not
  silently choose one for privileged operations.

Example:

```text
CameraCard:        # denied: ambiguous
CameraCard-1:      # generated display alias, if policy allows
CameraCard-2:      # generated display alias, if policy allows
id::<uuid-a>/DCIM
id::<uuid-b>/DCIM
```

---

## 8. Resolver table

The final spec should define a closed resolver table for `abi-v1`. Adding a
resolver later is an ABI-visible change and must follow the ABI discipline.

Recommended initial resolvers:

| Resolver | Purpose | Normal user-facing? | Durable? | Notes |
|---|---|---:|---:|---|
| `alias::` | Resolve a named alias in the current namespace | Yes | Sometimes | Backing for `Alias:/path` |
| `id::` | Resolve a stable volume/root ID | Rarely | Yes | Best durable machine form |
| `fs::` | Explicit filesystem-driver resolver | No | No | Admin/recovery/testing |
| `<driver>::` | Optional shorthand for `fs::<driver>/...` | No | No | Useful for `adfs::...` etc. |
| `net::` | Explicit remote root resolver | Sometimes | Depends | Usually publish as an alias |
| `dev::` | Raw device/storage object resolver | No | No | Capability-gated diagnostics |
| `view::` | Named synthetic view resolver | No | No | For namespace manager/debugging |

Do not create `/proc` or `/sys` equivalents under `proc::` or `sys::` for live
system information. Runtime system data belongs in the typed System Information
API.

---

## 9. Relationship to AGENTS.md §16 layout

The full spec should propose one of these two paths. Recommendation: choose
Option B.

### Option A: keep §16 literally and add aliases on top

`/System`, `/Users`, `/Apps`, and `/Storage` remain canonical directories on a
root volume. Aliases are convenience names pointing into that root.

This is not recommended. It preserves too much of the Unix single-root failure
model.

### Option B: make §16 a default view, not storage identity

The four names become view entries and aliases:

```text
System:/
Users:/
Apps:/
Storage:/
```

The default `/` view projects them as:

```text
/System
/Users
/Apps
/Storage
```

This preserves the clean TAIRiX user layout while removing the root filesystem
as the dependency through which all storage must be reached.

Recommended AGENTS.md amendment shape:

```text
TAIRiX exposes exactly four entries in the default session root view: /System,
/Users, /Apps, and /Storage. These are synthetic view bindings backed by the
first-class aliases System:, Users:, Apps:, and Storage:. The canonical storage
identity is the root ID or alias path, not the / view path. Creating additional
entries in the default root view is forbidden unless AGENTS.md is amended.
```

Then define:

```text
/System/*   view of System:/*
/Users/*    view of Users:/*
/Apps/*     view of Apps:/*
/Storage/*  view/catalog of published non-core aliases
```

---

## 10. Boot, discovery, and publishing lifecycle

The final spec should define a lifecycle like this.

### 10.1 Early boot

1. Architecture-specific discovery creates the hardware tree.
2. The bootstrap floor brings up the minimal bus/storage path needed to read the
   driver store.
3. The partition layer discovers storage object boundaries.
4. Filesystem probes identify candidate volume roots.
5. The volume manager publishes `id::` roots for every valid discovered root.
6. The namespace manager loads signed alias policy if available.
7. The namespace manager publishes machine aliases.
8. The session manager adds user aliases after login.
9. The synthetic `/` view is assembled from the alias namespace.

### 10.2 Required failure property

This must be true:

```text
Failure of the System volume or default / view does not make unrelated healthy
volumes unreachable by their stable IDs.
```

Example recovery shell:

```text
list-volumes
open id::2f5a2e1c-9a3b-4d6b-9e4e-18d76e21cafe/ian/Documents
copy id::2f5a2e1c-9a3b-4d6b-9e4e-18d76e21cafe/ian/Documents/important.txt \
     id::9af31ab4-6e5d-42a0-95b9-b34c80d772ab/recovered/important.txt
```

No path in that example depends on `/System`, `/Storage`, `/mnt`, `/media`, or a
root volume directory.

### 10.3 Hotplug

On hotplug:

1. A hardware tree update announces the storage object.
2. The appropriate bus/storage/filesystem driver stack attaches.
3. A new `id::` root appears.
4. Alias policy decides whether to publish a human alias.
5. The `Storage:` catalog view updates.
6. System Information API queries report the new root if the caller has the
   required capability.

On removal:

1. Existing handles fail according to their object semantics.
2. The alias is marked unavailable or removed according to policy.
3. The event is logged.
4. No unrelated aliases or roots are invalidated.

---

## 11. Security and capability model

### 11.1 Names are not authority

A path string grants nothing. Authority comes from capabilities, file
permissions, ACLs, manifest grants, and delegated handles.

Resolving a path has at least two phases:

1. Resolve the root selector: alias, ID, driver root, network export, device.
2. Walk the path inside that root and open or operate on the object.

Both phases must enforce capability checks before touching protected state.

### 11.2 Suggested capabilities

The final spec may reuse existing `CAP_FS_MOUNT` and
`CAP_FS_MOUNT_RELAX`, but the model is clearer with more precise capabilities.
Suggested names:

```text
CAP_FS_ATTACH          attach a filesystem driver to a storage object
CAP_FS_PUBLISH_ROOT    publish a discovered root into a namespace
CAP_FS_ALIAS_ADMIN     create, delete, or alter persistent aliases
CAP_FS_VIEW_ADMIN      alter default / view projections
CAP_FS_MOUNT_RELAX     relax nosuid/nodev/noexec/ro policy
CAP_FS_RAW_DEVICE      open raw storage through dev::
CAP_FS_RECOVERY        access recovery-only resolver operations
CAP_SYSINFO_STORAGE    query privileged storage inventory through sysinfo
```

The final spec should decide whether these become new capabilities or are folded
into existing ones. Do not use `uid = 0` as an authority shortcut.

### 11.3 Fail-closed rules

A path operation fails closed when:

- an alias is missing;
- an alias is ambiguous;
- a resolver is unknown;
- a root ID is unavailable;
- the caller lacks authority to resolve the root;
- the caller lacks authority to traverse or open a component;
- a view projection would relax a source root's policy;
- a filesystem driver reports lossy semantics the caller did not explicitly
  accept;
- a path attempts to escape a root with `..`;
- a driver-specific resolver would bypass normal policy.

### 11.4 Mount flags become root/view policy

The existing flags remain relevant:

```text
ro
nosuid
nodev
noexec
```

But attach them to root publications and view bindings, not only Unix-style
mountpoints.

A view binding may only preserve or restrict authority. It may not relax flags
from the source root unless the caller has the explicit relax capability and the
action is logged.

### 11.5 File picker and app sandboxing

Apps should not gain broad alias access merely by receiving a path string. A GUI
file picker or shell should pass a one-shot file or directory capability where
possible.

Example:

```text
User chooses: Home:/Documents/report.md
App receives: file handle or delegated FileCap, not ambient access to Home:/
```

Path strings are useful for display, logs, recent-file lists, and explicit shell
operations. They are not capability tokens.

---

## 12. Path parser and normalization requirements

The final spec should require exactly one shared parser in `lib/*`, likely a new
crate or an extension to `lib/abi` plus a no_std helper crate. Do not duplicate
path parsing in kernel, userland, drivers, shell, and GUI.

### 12.1 Parser outputs

The parser should produce typed data, not ad-hoc strings:

```rust
ParsedPath {
    root: PathRoot,
    components: ComponentList,
}

PathRoot::Alias(AliasName)
PathRoot::VolumeId(VolumeId)
PathRoot::Resolver { resolver: ResolverName, selector: RootSelector }
PathRoot::ViewAbsolute
PathRoot::CurrentRootAbsolute
PathRoot::Relative
```

This Rust sketch is illustrative. The final spec should define the exact ABI and
library ownership without inventing unnecessary public surface.

### 12.2 Normalization rules

- Collapse repeated separators inside the path part.
- Interpret `.` as current directory.
- Interpret `..` without allowing escape above the selected root.
- Preserve path component spelling except where a filesystem explicitly declares
  case-insensitive or normalization behavior.
- Reject invalid root names before root lookup.
- Reject NUL in all names.
- Do not silently reinterpret malformed Windows-like or URL-like strings.

### 12.3 Invalid examples

```text
C:foo                  # invalid relative-drive syntax
Home:Documents/file    # missing root slash
alias::::Home/file     # malformed resolver syntax
/../System             # cannot escape view root
Home:/../../System     # cannot escape Home root
```

### 12.4 Valid examples

```text
Home:/Documents/file.txt
alias::Home/Documents/file.txt
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/Documents/file.txt
arxfs::System/Kernel/tairix.rxe
/Users/ian/Documents/file.txt       # valid view path, not canonical storage ID
```

---

## 13. Filesystem-backed `::` resolvers

Yes, TAIRiX should have filesystem-backed resolvers. They are useful and fit the
RISC OS-inspired direction.

Example:

```text
adfs::HardDisc4/Apps/Paint
```

Meaning:

```text
resolver:       adfs
root selector:  HardDisc4
inner path:     Apps/Paint
```

Recommended equivalence:

```text
adfs::HardDisc4/Apps/Paint
fs::adfs/HardDisc4/Apps/Paint
```

However, these are not normal durable application paths. The canonical durable
path for the same object should be:

```text
id::<volume-id>/Apps/Paint
```

and the human path should be:

```text
HardDisc4:/Apps/Paint
```

if the root is published under that alias.

The final spec should define driver-backed resolver registration as an explicit
act. Merely adding a filesystem driver should not automatically create a public
resolver that bypasses namespace policy.

Security invariant:

```text
A filesystem-backed resolver must enforce the same capability, ACL, mount-flag,
MAC, and audit rules as `alias::` and `id::` access to the same object.
```

---

## 14. System roots and suggested aliases

The final spec should define default aliases created by the installer.

### 14.1 Required machine aliases

```text
System:    OS-provided files; read-only except projected writable service roots
Users:     user account roots
Apps:      installed application bundles
Storage:   synthetic catalog/view of non-core storage roots
```

### 14.2 Likely service aliases

```text
Logs:      append-only structured logs, writable by log service capabilities
Settings:  machine-wide settings, writable by settings/update capabilities
Recovery:  recovery tools and recovery image root
EFI:       firmware boot partition, if present and authorized
```

The final spec should decide which of these are mandatory, optional, hidden, or
only present in certain images.

### 14.3 Login/session aliases

For user `ian`:

```text
Home:      -> Users:/ian
Desktop:   -> Users:/ian/Desktop
Documents: -> Users:/ian/Documents
Library:   -> Users:/ian/Library
Settings:  -> Users:/ian/Settings      # if shadowing machine Settings is allowed
```

Be careful with `Settings:` because there may also be a machine settings root.
The final spec should avoid ambiguous names. It may prefer:

```text
UserSettings:
SystemSettings:
```

or make `Settings:` session-local and expose the machine one as
`SystemSettings:`.

---

## 15. The `Storage:` catalog

`Storage:` should not be a disk containing other disks. It should be a synthetic
catalog view listing published non-core roots.

Example:

```text
Storage:/
├── Backup/      -> Backup:/
├── CameraCard/  -> CameraCard:/
└── NAS/         -> NAS:/
```

Important invariant:

```text
Storage:/Backup/file is a view path for Backup:/file. Backup:/file remains
canonical and works without Storage:/.
```

This preserves user familiarity while avoiding the Unix fault-tolerance problem.

---

## 16. Symlinks, hardlinks, and cross-root references

The final spec must define links clearly.

### 16.1 Hardlinks

Hardlinks must not cross volume roots.

### 16.2 Relative symbolic links

Relative links are interpreted relative to the containing directory and remain
inside the same root unless they explicitly use a root-qualified target.

Example:

```text
../assets/logo.svg
```

### 16.3 Same-root absolute symbolic links

A leading `/` inside a stored link should be defined carefully. Recommended:
inside a volume, `/path` means from that same volume root, not from the global
synthetic `/` view.

This prevents a link stored on `Backup:` from silently depending on the session
view root.

### 16.4 Cross-root symbolic links

Cross-root links must be explicit:

```text
alias::Assets/shared/logo.svg
Assets:/shared/logo.svg
id::<volume-id>/shared/logo.svg
```

The final spec should decide whether cross-root links require a capability at
creation time and how they behave when the target alias is unavailable.

---

## 17. Shell and GUI behavior

### 17.1 Shell display

The shell prompt should display alias paths when possible:

```text
Home:/Projects/TAIRiX>
System:/Kernel>
Backup:/snapshots/latest>
```

If no alias maps to the current root, display the stable ID:

```text
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/>
```

### 17.2 `cd` behavior

Valid:

```text
cd Home:/Documents
cd System:/Kernel
cd Backup:/snapshots/latest
cd /Users/ian/Documents       # view path
cd ../src                     # relative path inside current root
```

Invalid:

```text
cd C:Users
cd Home:Documents
```

No per-drive current directories. There is one current directory as a root
handle plus directory handle.

### 17.3 File manager behavior

The file manager should show friendly aliases and volume labels, not raw device
names. A technical details pane may show:

```text
Alias: Backup:
Stable ID: id::<uuid>
Filesystem: arxfs
Device: dev::<diagnostic-id>
Flags: nosuid,nodev,noexec
```

### 17.4 Recent files

Recent-file records should store a stable root ID plus display path:

```text
root_id:    <volume-id>
inner_path: Documents/report.md
display:    Home:/Documents/report.md
```

If the alias changes, the file remains findable by ID.

---

## 18. System Information API integration

Because TAIRiX forbids `/proc` and `/sys`, storage inventory and namespace state
must be queried through typed APIs.

Suggested sysinfo queries:

```text
ListStorageObjects
ListVolumeRoots
ListAliases
ResolveAlias
ListViewBindings
DescribeVolume
DescribeFilesystemLimits
```

Each query declares its required capability. Unprivileged users may see their
own aliases and mounted removable volumes. Privileged callers may see raw device
relationships, filesystem drivers, root IDs, flags, and failure state.

No storage information should require scraping a virtual filesystem.

---

## 19. Installer implications

The installer should create aliases rather than assuming one canonical `/` root.

Recommended default roles:

```text
System:   read-only OS root
Users:    user data root
Apps:     application bundle root
Storage:  synthetic catalog
Home:     session alias generated after first user login
```

Possible layouts:

### 19.1 Single-volume install

All required aliases target subroots of one encrypted ARXFS volume:

```text
System: -> id::<root>/System
Users:  -> id::<root>/Users
Apps:   -> id::<root>/Apps
```

This is permitted, but the namespace model still treats them as aliases.

### 19.2 Multi-volume install

Each major alias may target a separate root:

```text
System: -> id::<system-volume>/
Users:  -> id::<users-volume>/
Apps:   -> id::<apps-volume>/
Logs:   -> id::<logs-volume>/
```

This is the model that delivers the fault-isolation goal.

### 19.3 View projection

The installer/session manager projects aliases into the default `/` view:

```text
/System -> System:/
/Users  -> Users:/
/Apps   -> Apps:/
/Storage -> Storage:/
```

If the view cannot be assembled, the system can still open `id::` and alias
paths where the namespace manager is available.

---

## 20. ARXFS and foreign filesystem implications

ARXFS should be the native filesystem and may store rich metadata:

- stable volume ID;
- volume label;
- role hints;
- Time64 timestamps;
- capability-aware metadata;
- filesystem feature flags;
- namespace policy hints if accepted by the final spec.

Foreign filesystems such as FAT32, ext4, ISO9660, ADFS, or others may have
limited metadata. Their drivers must declare limits through the filesystem
capability API:

- timestamp range and precision;
- case behavior;
- maximum component length;
- forbidden characters;
- support for symlinks/hardlinks;
- support for permissions/ACLs;
- support for stable file IDs;
- whether labels are unique, mutable, missing, or lossy.

TAIRiX must not let a weak foreign filesystem weaken TAIRiX security. If a
foreign filesystem cannot store TAIRiX ACLs or capabilities, the root publication
must apply a policy wrapper and default to restrictive flags.

---

## 21. ABI and library ownership

The final spec should identify shared crates and ABI files precisely.

Likely homes:

```text
lib/abi/src/fs.rs             ABI-visible filesystem/path/root types
lib/abi/src/storage.rs        storage object and volume metadata types
lib/abi/src/sysinfo.rs        storage sysinfo query/response types
lib/path or lib/fsname        no_std path parser and canonicalizer
lib/caps                     capability IDs and checks
userland/system/devmgr        storage driver autoload integration
userland/system/installer     default alias policy creation
userland/shell/elsh          Alias:/ parsing and display
```

Do not duplicate the parser in shell, kernel, filesystem drivers, and GUI. There
must be one shared parser and one shared normalization rule set.

ABI-visible types must use 64-bit time where time is stored or reported.

---

## 22. Error model

The full spec should define typed errors. Suggested error names:

```text
UnknownResolver
MalformedPath
InvalidAliasName
AliasNotFound
AliasAmbiguous
AliasUnavailable
RootIdNotFound
RootUnavailable
PermissionDenied
CapabilityRequired
ViewUnavailable
ViewCycle
PathEscapesRoot
FilesystemDriverUnavailable
FilesystemFeatureUnsupported
ForeignFilesystemLimit
TimestampOutOfRange
DeviceUnavailable
RawDeviceAccessDenied
PolicyWouldRelaxFlags
```

No production path may panic on malformed input, missing alias, absent device,
I/O error, or unsupported filesystem feature.

---

## 23. Audit and logging requirements

Log security-relevant namespace decisions through `lib/log` with stable event
IDs.

Suggested events:

```text
fs.root.discovered
fs.root.attached
fs.root.publish.allow
fs.root.publish.deny
fs.alias.create.allow
fs.alias.create.deny
fs.alias.remove.allow
fs.alias.remove.deny
fs.alias.resolve.deny
fs.view.bind.allow
fs.view.bind.deny
fs.flags.relax.allow
fs.flags.relax.deny
fs.raw_device.open.allow
fs.raw_device.open.deny
fs.hotplug.root_added
fs.hotplug.root_removed
fs.conflict.alias_ambiguous
```

Do not log secrets, keys, capability token values, or private path contents
beyond the policy set by the audit subsystem.

---

## 24. Tests the final spec should require

The final spec should require at least these tests.

### 24.1 Parser tests

- `Home:/Documents/file.txt` parses as alias root `Home` plus components.
- `alias::Home/Documents/file.txt` parses to the same root and components.
- `id::<uuid>/Documents/file.txt` parses as stable root ID.
- `adfs::HardDisc4/Apps/Paint` parses as driver resolver `adfs`.
- `Home:Documents/file.txt` is rejected.
- `C:foo` is rejected.
- `Home:/../../System` is rejected or normalized without escape.
- NUL-containing paths are rejected.

### 24.2 Namespace tests

- Alias lookup succeeds only with required authority.
- Missing aliases fail closed.
- Ambiguous aliases fail closed.
- Per-session alias overrides do not alter the machine alias table.
- A sandboxed process can see only aliases delegated to it.
- Multi-target alias writes fail unless a write target is explicit.

### 24.3 Fault-tolerance tests

- A volume remains openable by `id::` when the `/` view is unavailable.
- A volume remains openable by `id::` when `Storage:/` is unavailable.
- Corruption or absence of `System:` does not hide an unrelated healthy `Backup:`
  from a recovery environment with sufficient authority.
- Rebuilding the alias table from discovered volume metadata produces safe,
  unambiguous aliases only.

### 24.4 Security tests

- Publishing a root requires capability.
- Creating or changing a persistent alias requires capability.
- A view binding cannot relax `ro`, `nosuid`, `nodev`, or `noexec` without the
  explicit relax capability.
- Driver-backed resolvers do not bypass ACLs, capability gates, or mount flags.
- Raw `dev::` access is denied without explicit authority.
- System-wide storage inventory is not available through files under `/proc` or
  `/sys`.

### 24.5 Installer tests

- Default install creates the required aliases.
- Default `/` view contains exactly the allowed view entries.
- Expert mode cannot introduce legacy top-level POSIX names into the default
  root view.
- `/Storage/<name>` view entries are projections of independent aliases, not
  canonical mountpoints.

### 24.6 Hotplug tests

- Inserting a removable volume publishes an `id::` root.
- If policy permits, inserting a removable volume creates a safe alias.
- Removing a volume invalidates only that root and its aliases.
- Duplicate labels do not produce silent alias selection.

---

## 25. Forbidden outcomes

The final spec must reject these outcomes explicitly.

- Storage identity depends on one persistent `/` directory tree.
- Removable or extra volumes are canonical only as `/Storage/<volume>`.
- `/mnt` or `/media` returns under another name.
- Drive letters such as `C:` become the primary model.
- Windows `D:relative` semantics are accepted.
- Filesystem type becomes the normal durable path identity.
- `adfs::`, `fat32::`, or `arxfs::` bypass policy.
- A path string acts as a capability token.
- `uid = 0` can bypass namespace checks.
- `/proc` or `/sys` is recreated as `proc::`, `sys::`, or a hidden virtual tree.
- A second path parser is added for shell, GUI, or a driver.
- The final design requires C code or hand-written generated headers.
- Target-specific path behavior leaks into generic code.
- The default root view can contain arbitrary new top-level entries without
  updating AGENTS.md.

---

## 26. Example final user experience

A normal desktop user sees:

```text
Home:/Documents
Apps:/Editor.app
Storage:/CameraCard
Backup:/snapshots/latest
```

The file manager may show:

```text
System
Home
Apps
Storage
Backup
CameraCard
```

A shell can still use view paths:

```text
/Users/ian/Documents
/Apps/Editor.app
/Storage/CameraCard/DCIM
```

But tools that care about durability or recovery can use:

```text
id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/ian/Documents
```

An administrator can inspect a foreign disk:

```text
adfs::HardDisc4/$.Apps.Paint
```

or, if the final spec normalizes RISC OS paths to Unix-like separators:

```text
adfs::HardDisc4/Apps/Paint
```

The key user-facing property:

```text
Backup:/file.txt does not depend on /Storage/Backup existing.
```

---

## 27. Suggested final spec structure

The generated full spec should use this structure:

```text
# TAIRiX Storage Namespaces, Volume Roots, and Aliases

1. Purpose and non-goals
2. Charter relationship and AGENTS.md amendments
3. Terminology
4. Design invariants
5. Path grammar
6. Resolver table
7. Alias model
8. Volume IDs and durable references
9. Synthetic root view `/`
10. Storage catalog `Storage:`
11. Filesystem-backed resolvers
12. Boot/discovery/publish lifecycle
13. Hotplug and removal
14. Security and capability model
15. Permissions, flags, ACLs, and MAC
16. System Information API integration
17. Installer defaults
18. Shell and GUI behavior
19. Links and cross-root references
20. Foreign filesystem behavior
21. ABI and crate ownership
22. Error model
23. Audit events
24. Required tests
25. Examples
26. Rejected designs
```

---

## 28. Key decision to carry into the final spec

The central decision is:

```text
TAIRiX native storage paths are alias-rooted or ID-rooted. `/` is a generated
view, not the root of storage.
```

The most important syntax is:

```text
Home:/Documents/file.txt
System:/Kernel/tairix.rxe
Backup:/snapshots/latest
id::<volume-id>/snapshots/latest
```

The most important compatibility rule is:

```text
/Storage/Backup/file.txt is only a projection of Backup:/file.txt.
```

The most important filesystem-backed rule is:

```text
adfs::HardDisc4/path is allowed for explicit driver-backed access, but durable
ordinary paths should use aliases or stable IDs.
```

