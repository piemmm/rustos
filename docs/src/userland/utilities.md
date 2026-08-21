# Core CLI utilities (`userland/apps` and `userland/shell`)

Stage 6 ships a set of small command-line utilities, each its own crate.
This page documents the ones that have landed (`sysinfo`, `ps`, `man`,
`cat`, `clear`, `reset`, `ls`, `rm`, `cp`, `mv`, `chmod`, `chown`,
`getcap`, `setcap`, `true`, `false`, `yes`, `basename`, and `dirname`)
and is extended as the others (`mount`, …) arrive.

## `sysinfo` — the System Information CLI (`userland/shell/sysinfo`)

`tairix-sysinfo` is the single command-line tool that exposes the System
Information API to the terminal (`AGENTS.md` §16.6). TAIRiX has no
`/proc` and no `/sys`; every piece of live system information is served
by `/System/Services/sysinfod.app/Run` over the typed, versioned, capability-
checked `sysinfo-v1` wire surface defined in `tairix_abi::sysinfo` (see
[System Information API (`sysinfo-v1`)](../abi/sysinfo.md) and the
[System Information service](./sysinfod.md)). `sysinfo` is a *client* of
that API: it does **not** read a virtual filesystem, and there is no
privileged path that bypasses the capability check.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). It
depends only on the audited `tairix-abi` crate and the shared
`tairix-procinfo` client helpers, so it never links a kernel or driver
crate (`AGENTS.md` §17.4).

### Commands

| Command              | Query                 | Capability           |
|----------------------|-----------------------|----------------------|
| `processes`          | `SELF_PROCESS_LIST`   | none                 |
| `processes --all`    | `GLOBAL_PROCESS_LIST` | `CAP_SYSINFO_GLOBAL` |
| `memory`             | `KERNEL_MEMORY_STATS` | `CAP_SYSINFO_KERNEL` |
| `hardware`           | `HARDWARE_TREE`       | `CAP_SYSINFO_HW`     |
| `identity`           | `SYSTEM_IDENTITY`     | none                 |
| `uptime`             | `UPTIME`              | none                 |
| `limits`             | `RESOURCE_LIMITS`     | none (self-scoped)   |
| `seats`              | `SEAT_LIST`           | `CAP_SYSINFO_HW`     |
| `pressure`           | `MEMORY_PRESSURE`     | `CAP_SYSINFO_KERNEL` |
| `reclaim`            | `RECLAIM_STATS`       | `CAP_SYSINFO_KERNEL` |
| `ramzip`             | `RAMZIP_STATS`        | `CAP_SYSINFO_KERNEL` |
| `cpu`                | `CPU_LOAD`            | `CAP_SYSINFO_KERNEL` |
| `help` (the default) | —                     | none                 |

`processes` accepts the `-a`/`--all` flag; the other subcommands take no
arguments and `ps`/`mem`/`hw`/`id`/`rlimits` are accepted as short
aliases. `help` (also `-h`/`-?`/`--help`, and the default with no
arguments) renders the tool's own short help from its bundle's `Help/`
tree through the shared `lib/help` engine (`plans/APPS.md` §4), falling
back to the built-in usage banner when the tree is unavailable.
`pressure`, `reclaim`, `ramzip`, and `cpu` render the kernel-statistics
queries `plans/STRESSTEST.md` ST1 added — the live memory-pressure gauge,
the per-class reclaimable-cache ledger, the compressed tier's counters,
and the per-CPU queue-depth/switch/preemption figures.
`limits` reports the calling process's *own* effective resource
limits and live usage (`AGENTS.md` §24.3) — the read-only counterpart of
the `ulimit` shell builtin that *changes* them. The
capability gate lives in `sysinfod`, not in this tool — `sysinfo` only
ever issues the queries the frozen registry defines, never a free-form
"raw query id".

### A request/render machine, not a data source

`run` turns one parsed `Command` into a typed request and renders the
typed reply, through three steps:

1. Build the `SysinfoRequestHeader` (and, for `processes`, a
   `ProcessListRequest` payload) from the `sysinfo-v1` ABI.
2. Hand the encoded request to the injected `Transport`, which carries
   it to `sysinfod` and returns the reply bytes. The transport owns the
   reply allocation, so the client never guesses a buffer size.
3. Decode the reply with the ABI's fail-closed `from_bytes` decoders and
   write one rendered line per row to the injected `Output`.

`Transport` and `Output` are the only two operations that reach the
outside world. On a running system they are IPC- and console-backed; in
tests they are in-memory fixtures, so every rendering and paging
decision is testable without a kernel — the same seam discipline as
`init` (`Spawner`/`Reaper`) and `login` (`LoginView`).

The `Transport`/`Output` seams, the request framing and capability-aware
call, and the process-list paging and row rendering are shared with `ps`
through the `lib/procinfo` crate. Sibling userland crates may not depend
on one another (`AGENTS.md` §17.4), so the common piece lives in `lib/*`
rather than being copied (`AGENTS.md` §2.2); `sysinfo` adds only the
scalar queries (`memory`/`hardware`/`identity`/`uptime`/`limits`) and its
own command grammar on top.

### Paging

A process list can be longer than a single reply, so `sysinfo` pages it:
it issues `ProcessListRequest`s with an increasing `offset` and a fixed
`limit`, rendering each page, until a page comes back shorter than the
limit. The paging loop lives in the client; the ABI carries only the
`offset`/`limit` fields.

Every shared walk also lets its caller end the paging deliberately: a
per-record sink answers "continue" or "stop", and stopping succeeds
rather than erroring, so a reader that already has its answer (or that
will only hold a bounded number of records) is never obliged to page a
list the service could keep answering indefinitely. `sysinfo` renders
every row, so it always continues; a lookup for one named thing stops at
the match.

### Fail closed

- A capability denial returns from `sysinfod` as
  `Errno::PermissionDenied`, which the CLI renders as a precise "this
  query requires a capability you do not hold" diagnostic
  (`SysinfoError::PermissionDenied`) without inventing a parallel policy
  (`AGENTS.md` §2.2, §16.6).
- An unknown subcommand, an unknown flag, or a stray trailing argument
  is a `SysinfoError::Usage` that issues no query and prints the usage
  banner.
- A reply that does not decode against `sysinfo-v1` — a truncated
  scalar, or a process page whose length is not a whole number of
  records — is a hard `SysinfoError::Service` error, never a
  partially-rendered guess.

`sysinfo hardware` pages the tree in whole through the shared
`tairix_procinfo::hwtree::fetch_tree` walk (the same fetch `lspci` and
`lsusb` render from, `AGENTS.md` §2.2) and summarises it as a node
count; the per-device inventory renderings are those tools' job.

### Advisory output (`stdinfo`, fd 3)

Like `ps`, the default self-scoped `sysinfo processes` listing emits the
`proc.self_scope_only` omission record (`AGENTS.md` §20.1) on the
standard information stream, suggesting `sysinfo processes --all` as the
widening spelling. The record is the one shared `lib/procinfo` definition
(`emit_self_scope_omission`) both tools emit; it is advisory only —
emitted best-effort after the rows, never affecting output, ordering, or
exit status — and nothing is emitted under `--all`, whose listing is
exhaustive, or when the walk fails.

### Tests

`cargo test -p tairix-sysinfo` drives the parser and the request/render
engine against an in-memory `sysinfod` stand-in and a recording output:
the command grammar (every subcommand, alias, and the usage-error
paths), every query's rendering, process-list paging across a page
boundary, self-vs-global query routing, the self-scope advisory record
(present on the default listing, absent under `--all` and on a failed
walk), and the denied, malformed, truncated, and dead-console
fail-closed paths.

## `lspci` — list discovered PCI/PCIe devices (`userland/apps/lspci`)

`tairix-lspci` is the `pciutils` `lspci` over what the TAIRiX model
actually carries (`plans/DEVICES.md` DEVICE1 V2, `AGENTS.md` §16.7): one
line per discovered PCI/PCIe function — a small bus-order listing
number, its class name, and vendor + device names. The inventory is the
hardware tree read
through the `CAP_SYSINFO_HW`-gated `sysinfo-v1` `HARDWARE_TREE` query
(the shared paged `tairix_procinfo::hwtree::fetch_tree` walk — never a
`/proc` and never a kernel bypass), fail-closed whole `HwNode` records
reassembled from a generation-checked snapshot; a refused
query defeats the tool's purpose, so the reason lands on standard error
and nothing is fabricated. Names resolve through `lib/devids` from the
vetted `pci.ids` table the bundle ships as `Resources/pci.ids.bin` (data
on the volume, covered by the signed `AppInfo` content hash — never
`include_bytes!`); an identity the database lacks renders numerically
(`Vendor 8086`, `Device 2922`), with the count advised on fd 3
(`pci.names_unresolved`, `AGENTS.md` §20.1), and a missing or invalid
table degrades the whole listing to numeric ids with the reason on
standard error. `-n`/`-nn` select the numeric modes, `-v` lists the
node's declared resources (the capability-grant requests the tree
records), `-t` renders the bus topology — naming each intermediate bus
by its class and match-key identity, and under `-tv` its declared
resources too — and `-d [<vendor>]:[<device>]`/`-s <node>` filter.
Documented divergences: TAIRiX records no `bus:device.function`
triple, so each listed device is given a small, stable bus-order number
(shown `#<n>`) that `-s` selects — never the internal hardware-tree
node id, which comes from a reserved id space and can be a large,
meaningless value; no subsystem ids, and no `-k` until the
system publishes driver-binding records. Manifest: `CAP_CONSOLE_WRITE` +
`CAP_FS_ACCESS` + `CAP_SYSINFO_HW`. `cargo test -p tairix-lspci` drives
the parser, the naming/fallback/filter/tree/verbose renders, the
fail-closed reply and refusal paths, and the fd-3 record against a
canned tree and a fixture database compiled through the real
`lib/devids` pipeline, plus the thirteen-locale `OPTIONS` pinning.

## `lsusb` — list discovered USB devices (`userland/apps/lsusb`)

`tairix-lsusb` is the `usbutils` `lsusb` over what the TAIRiX model
actually carries (`plans/DEVICES.md` DEVICE1 V3, `AGENTS.md` §16.7): one
`Bus NNN Device NNN: ID vvvv:pppp <vendor> <product>` line per
discovered physical USB device. The inventory records one node per
*interface* (the driver-bind and grant unit), so the engine groups the
interface nodes of one device truthfully by the bus-local device
address the host controller reported on each node (`HwNode::address`,
the device's xHCI slot id) — a composite keyboard+mouse receiver lists
once, while two identical devices (distinct addresses) stay distinct,
and a node whose emitter reported no address is never guessed into a
group. It shares `lspci`'s whole posture: the same
`CAP_SYSINFO_HW`-gated `HARDWARE_TREE` query, the same fail-closed paged
fetch and stable bus order (the shared `tairix_procinfo::hwtree` walk
both tools use, `AGENTS.md` §2.2), and
names resolved through `lib/devids` from the vetted `usb.ids` table the
bundle ships as `Resources/usb.ids.bin`. An identity the database lacks
shows only its `ID vvvv:pppp` (as `usbutils` omits an unknown string),
with the count advised on fd 3 (`usb.names_unresolved`, `AGENTS.md`
§20.1); a missing or invalid table degrades the whole listing to bare
ids with the reason on standard error. `-v` lists each interface's
class / subclass / protocol names from the `usb.ids` class tables (one
triple per interface of the device), `-t` renders the bus → device →
interface topology (the bus line names the controller by its
`compatible` identity), and `-d [<vendor>]:[<product>]` /
`-s [[<bus>]:][<devnum>]` filter. Documented divergence: TAIRiX has no
Linux bus/devnum registry, so the bus and device numbers are 1-based
per-snapshot ordinals in stable bus order — small, dense, and stable
for an unchanged topology, never kernel node ids. Manifest:
`CAP_CONSOLE_WRITE` + `CAP_FS_ACCESS` + `CAP_SYSINFO_HW`. `cargo test
-p tairix-lsusb` drives the parser (including the `usbutils` `-s`
grammar), the naming/bare-id/filter/tree/verbose renders, the
fail-closed reply and refusal paths, and the fd-3 record against a
canned tree and a fixture database compiled through the real
`lib/devids` pipeline, plus the thirteen-locale `OPTIONS` pinning.

## `ps` — list processes (`userland/apps/ps`)

`tairix-ps` is the POSIX-named process lister. Like `sysinfo`, it is a
*client* of the System Information API (`AGENTS.md` §16.6): there is no
`/proc`, so `ps` issues the `sysinfo-v1` process-list queries served by
`/System/Services/sysinfod.app/Run` and has no privileged path that bypasses the
capability check. By default it lists the caller's own processes (the
ungated `SELF_PROCESS_LIST`); `-e`/`-A`/`--all` request every process
(`GLOBAL_PROCESS_LIST`, which the service gates on `CAP_SYSINFO_GLOBAL`).

The crate is `no_std` (with `alloc`, used only by the test fixtures), has
no `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9). It depends only on the audited `tairix-abi` crate and
the shared `tairix-procinfo` client helpers, so it never links a kernel
or driver crate (`AGENTS.md` §17.4).

### Grammar

```
ps [-e | -A | --all] [-h | -?]

  (default)   list your own processes
  -e, -A      list every process (needs CAP_SYSINFO_GLOBAL)
  -h, -?      show this help
```

`ps` takes no file operands. `--` ends option parsing. An unknown option,
an unknown letter inside a cluster, or any positional operand is a
fail-closed `PsError::Usage`. The reserved `-h`/`-?` (and `--help`)
switches render the tool's own short help from its bundle's `Help/` tree
through the shared `lib/help` engine (`plans/APPS.md` §4), falling back
to the built-in usage banner when the tree is unavailable.

### Shared with `sysinfo`

`ps` and `sysinfo` read the same process list, so the request seams
(`Transport`/`Output`), the request framing and capability-aware `call`,
the `offset`/`limit` page walk, the fixed-column row rendering
(`PID PPID UID GID S CPU NAME`, with a single-letter state code), and
the `proc.self_scope_only` advisory emitter
(`emit_self_scope_omission`) live once in the `lib/procinfo` crate
rather than being copied (`AGENTS.md` §2.2). Because sibling userland
crates may not depend on one another (`AGENTS.md` §17.4), that shared
piece is a `lib/*` crate. `ps` supplies only its own argument grammar,
usage banner, widening spelling (`ps -e`), and `PsError`.

### A renderer, not a policy point

`run` pages through the process list via `lib/procinfo` and writes one
rendered row per process to the injected `Output`. The capability gate
lives in `sysinfod`, not here: a denied global listing comes back as
`Errno::PermissionDenied`, which `ps` renders honestly as
`PsError::PermissionDenied` (`AGENTS.md` §5.4 — the service is the policy
point). The two operations that reach the outside world — issuing the
request and writing the terminal — are the injected `Transport` and
`Output` seams; on a running system they are IPC- and console-backed, and
in tests they are in-memory fixtures.

### Fail closed

- An unknown option or a positional operand is a `PsError::Usage` that
  issues no query and prints the usage banner.
- A denied global listing is `PsError::PermissionDenied`; any other
  transport failure or a reply that does not decode against `sysinfo-v1`
  (a process page whose length is not a whole number of records) is a
  hard `PsError::Service`, never a partially-rendered guess.
- A failed terminal write is `PsError::Output`. There is no panic
  (`AGENTS.md` §2.9).

### Advisory output (`stdinfo`, fd 3)

The default self-scope listing emits the `proc.self_scope_only` omission
record (`AGENTS.md` §20.1) on the standard information stream: a terse
human note ("Only your own processes are shown." with the `ps -e`
suggestion) plus structured data for tools (`stdout_is_exhaustive`,
the widening `argv`). The record is the one shared `lib/procinfo`
definition (`emit_self_scope_omission`) that `sysinfo processes` also
emits, parametrised only by each tool's own widening spelling. It is
advisory only — emitted best-effort after the rows, never affecting
output, ordering, or exit status — and nothing is emitted under
`-e`/`-A`/`--all`, whose listing is exhaustive.

### Tests

`cargo test -p tairix-ps` drives the parser and the request/render engine
against an in-memory `sysinfod` stand-in and a recording output: the
command grammar (default self-listing, the `-e`/`-A`/`--all` selectors,
`-h`/`-?`/`--help`, unknown-option and positional-operand rejection), the
Help-document short-help render and its usage-banner fallback, the
self-vs-global query routing, header + rows rendering, the empty listing,
the denied-global capability mapping, the self-scope advisory record
(present by default, absent under `--all`), and the header/row
write-failure paths. The shared page walk and rendering carry their own
unit tests in `lib/procinfo` (`cargo test -p tairix-procinfo`).

## `mount` — list and attach filesystems (`userland/apps/mount`)

`tairix-mount` both reports and changes the mount table, and the two
halves take deliberately different paths. **Listing** the mounted
filesystems is a *read* of live system state, so — like `ps` — it goes
through the System Information API (`AGENTS.md` §16.6): there is no
`/proc` and no mount-table file, so `mount` issues the ungated
`sysinfo-v1` `MOUNT_LIST` query served by `/System/Services/sysinfod.app/Run`.
**Attaching** a filesystem is privileged (it needs `CAP_FS_MOUNT`,
`AGENTS.md` §5.2), and the kernel — not this tool — makes that decision
(`AGENTS.md` §5.4).

The crate is `no_std` (with `alloc`, used only by the test fixtures), has
no `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9). It depends only on the audited `tairix-abi` crate and
the shared `tairix-procinfo` client helpers, so it never links a kernel
or driver crate (`AGENTS.md` §17.4).

### Grammar

```
mount [-r] [-t TYPE] [-o OPTIONS] [--] [SOURCE TARGET]

  (no operands)        list the mounted filesystems
  SOURCE TARGET        mount SOURCE at TARGET (needs CAP_FS_MOUNT)
  -r, --read-only      mount read-only (same as -o ro)
  -t, --types TYPE     filesystem type (probed when omitted)
  -o, --options LIST   comma-separated: ro,rw,nosuid,nodev,noexec
  -h, --help           show the usage banner
```

With no operands `mount` lists the table; with exactly `SOURCE TARGET` it
attaches. Value options accept their value attached (`-text4`,
`--types=ext4`) or as the following argument; `-r` may cluster with other
toggles. `--` ends option parsing. The recognised `-o` names map onto the
frozen `MountFlags` bitmap (`ro`/`rw` plus the `nosuid`/`nodev`/`noexec`
restrictions, `AGENTS.md` §5.3).

### Listing — a client of the mount-list query

A listing pages the `MOUNT_LIST` reply through the same `lib/procinfo`
machinery `ps` uses — the `Transport`/`Output` seams, the request framing,
and the generic `offset`/`limit` page walk — so none of it is copied
(`AGENTS.md` §2.2). The shared renderer prints one familiar
`source on target type fstype (options)` line per mount; the option list
opens with `ro`/`rw` and then names each restriction in force, and a
surprise-removed volume carries a trailing ` [unavailable-dirty]` /
` [unavailable-lost]` marker (`plans/DEVICES.md` D4b), a re-inserted
volume whose non-mutation could not be proven carries
` [recovery-conflict]` (`plans/DEVICES.md` D4c), and a live volume whose
backing device reports itself unwell carries ` [degraded]` or
` [recovering]` from its I/O health (`plans/FIX-IO.md` IO3) — additive, so
a healthy volume's line keeps the classic shape while an unwell, dead, or
conflicted one never looks healthy. The query
is ungated: the mount table is system-wide and secret-free, so any task
may read it (`AGENTS.md` §16.6).

### Attaching — a presenter, not a policy point

For a `SOURCE TARGET` request `mount` parses and validates the arguments
and hands a `MountSpec` to the injected `Mounter` seam; it makes no
permission decision of its own. The kernel is the policy point
(`AGENTS.md` §5.4): a caller lacking `CAP_FS_MOUNT`, an unknown source, a
bad superblock, or an already-mounted target is refused there and
surfaced as `MountError::Mount(errno)`. `mount` writes nothing on a
successful attach. A `None` filesystem type asks the kernel to identify
the volume by probing; `mount` never guesses one (`AGENTS.md` §2.1).

### Fail closed

- An unknown option, a missing option value, or a number of operands
  other than zero or two is a `MountError::Usage`; mount options given
  with no operands are also a usage error (there is nothing to mount).
- An unknown or empty `-o`/`-t` value is a `MountError::BadOption`.
- A listing transport failure or a reply that does not decode against
  `sysinfo-v1` is a hard `MountError::Service`, never a partially-rendered
  guess; a refused or failed attach is `MountError::Mount`; a failed
  terminal write is `MountError::Output`. There is no panic (`AGENTS.md`
  §2.9).

### Tests

`cargo test -p tairix-mount` drives the parser and the engine against an
in-memory `sysinfod` fixture, a recording output, and an in-memory
mounter: the command grammar (list vs mount vs help, every option form,
attached/space values, the read-only shorthand, `--`, and the
usage/bad-option rejections), the mount-table listing and its query
routing, the empty table, the service- and output-failure paths, the
attach request reaching the mounter with the right fields, and the denied
attach mapping to `MountError::Mount`. The shared page walk and the
`source on target type fstype (options)` rendering carry their own unit
tests in `lib/procinfo` (`cargo test -p tairix-procinfo`).

## `unmount` — detach a runtime-attached volume (`userland/apps/unmount`)

`tairix-unmount` is `mount`'s counterpart (`plans/DEVICES.md` D4b):
`unmount NAME` takes the volume mounted under `NAME` out of service.
`NAME` is the volume's catalog name (`usb1`) or its mount-point path
(`/Storage/usb1`), resolved through the same ungated `sysinfo-v1`
`MOUNT_LIST` query the other tools use — whose records carry each
mount's stable 16-byte volume identity and its availability — and the
resolved identity is handed to the kernel's `volume_detach` path
through the injected `Detacher` seam. The kernel is the policy point
(`AGENTS.md` §5.4): it requires `CAP_FS_MOUNT`, flushes the filesystem
and the device, retracts the mount, withdraws the durable `id::` root,
and audits every decision. A successful detach writes nothing, matching
the established `umount` behaviour (`AGENTS.md` §16.7).

### Grammar

```
unmount [-f | --force] [--] NAME

  NAME          the volume to detach (catalog name or mount point)
  -f, --force   force-unmount: discard retained uncommitted data
  -?, --help    show the command's own short help
```

### Force-unmount — the audited exit for an unavailable volume

A surprise-removed volume (`unavailable-dirty`/`unavailable-lost` in
the mount listing) refuses a plain detach: its retained uncommitted
writes are held for the verified re-insert. So does a re-inserted
volume in the `recovery-conflict` state (`plans/DEVICES.md` D4c): it is
mounted read-only while its retained set is still held, and only the
audited force-discard releases it. `--force` is the
deliberate, separately-audited exit — the kernel discards the retained
set, retracts the volume, and logs the data loss with its own event
(`fs.hotplug.force_unmount`, carrying the discarded byte count and the
reason a clean commit was impossible). On a healthy volume `--force`
still commits cleanly; nothing is discarded when the flush succeeds.
A verified re-insert needs no tool at all: a re-attached volume whose
non-mutation is proven has its retained writes replayed by the kernel
and returns to full service (`fs.hotplug.reinsert_replayed`).
When a plain detach is refused because the volume is unavailable, the
tool spells out the `--force` consequence on standard error and emits
an fd-3 `suggestion` record (`fs.volume_unavailable_force_required`,
`safe_to_autorun: false` — `AGENTS.md` §20.1), additive and ignorable.

### Fail closed

- An unknown option or a number of operands other than one is a usage
  error (exit `2`).
- A name matching no mount is `NotFound`; a matched mount with no
  detachable volume identity (the permanent boot volumes, the in-RAM
  view bindings) is `NotDetachable` — the tool never sends the kernel a
  nil identity or guesses a volume.
- A refused detach surfaces the kernel's exact `Errno`; a listing
  failure is a hard `Service` error, never a partially-resolved guess.
  There is no panic (`AGENTS.md` §2.9).

Manifest: `CAP_CONSOLE_WRITE` + `CAP_FS_ACCESS` + `CAP_FS_MOUNT`.
`cargo test -p tairix-unmount` drives the parser, the resolver (catalog
name, mount-point path, unknown name, non-detachable mounts), the
force flag reaching the kernel, the refusal paths with and without the
fd-3 suggestion, the service-failure path, and the thirteen-locale
`OPTIONS` pinning of the bundled `Help/` documents.

## `mdadm` — administer RAID arrays (`userland/apps/mdadm`)

`tairix-mdadm` is the administrator's array tool over the TAIRiX RAID
composer (`plans/FIX-IO.md` IO6), tracking the reference `mdadm`'s
option spelling so a user who knows that tool finds this one familiar.
The inventory is a read: `--detail` and `--examine` call
`tairix_procinfo::raid_arrays` and `tairix_procinfo::raid_members`, the
System Information queries the composer answers at the same
`CAP_SYSINFO_HW` bar the hardware tree is read under. The mutations are
a posted control frame: `--create`, `--add`, `--remove`, and `--stop`
encode a `tairix_abi::raid_admin::RaidControlOp` to
`RAID_CONTROL_ENDPOINT`, and the reply decodes through
`tairix_abi::reply::decode_status_reply` (or
`raid_admin::decode_create_reply`, which carries the identity the
composer minted). The composer is the policy point: it checks the
caller holds `CAP_STORAGE_ADMIN` against the kernel-attested origin, so
the tool never tests authority — it reports the refusal.

### Grammar

```
mdadm --create --level=<L> --raid-devices=<n> [--chunk=<blocks>] <device>...
mdadm --detail [<array>]
mdadm --examine
mdadm --add <array> <device>
mdadm --remove <array> <device>
mdadm --stop <array>

  -C, -D, -E, -a, -r, -S   the mode short forms
  -l, -n, -c               --level, --raid-devices, --chunk
  -h, -?, --help           show the command's own help
  -V, --version            print the version
```

Exactly one mode per invocation: a second, different mode is a usage
error, as is no mode at all, an unknown option, a value option in a
mode that does not take it, and a missing or surplus operand. `--` ends
option parsing, so a `-`-prefixed operand is positional. `--help` and
`--version` win over any mode, in that order.

### Naming a device and an array

There is no `/dev`, so both operand spellings are TAIRiX's own —
documented divergences, refused rather than guessed at:

- A **device** is its hardware-tree node id, spelled `node:<id>` (the
  same name `--detail` and `--examine` print). Any other spelling, and a
  zero id, is `BadDeviceName`.
- An **array** is its 128-bit identity as 32 lower-case hexadecimal
  digits. The full identity resolves, and so does any prefix naming
  exactly one live array; a prefix matching more than one is
  `AmbiguousArray` — never a coin-flip — and one matching none is
  `ArrayNotFound`.

`--create` additionally refuses a device named twice and a member set
larger than `RAID_CREATE_MAX_MEMBERS` before it posts anything, so the
diagnostic names the offending operand. The composed levels are `0`,
`1`, `5`, `6`, `10`, and triple parity (`tp`/`raid-tp`); there is no
RAID4, so `--level=4` is refused with that reason, and `--chunk` is
accepted only for a striped level.

### Reports and advisories

`--detail` prints one `mdadm`-shaped block per array — identity header,
then level, state, raid/active device counts, chunk size for a striped
level, array size, the published `node:<id>`, endpoint, generation, and
a `Rebuild Status` or `Scrub Status` position only while one is running.
`--examine` prints the device table (`Device`, `Array`, `Slot`, `State`,
`Blocks`), listing array members with their slot and disposition and the
unaffiliated blank devices a new array can be created over. Three fd-3
advisories add context the primary output does not carry and never
change it: `raid.redundancy_reduced` (a `summary`, when an array's
health is not optimal), `raid.blank_devices_omitted` (an `omission`, in
the array view that does not list candidates), and `raid.no_arrays` /
`raid.no_devices` (a `context`, on an empty machine, whose report is
correctly empty).

### Fail closed

- A refused read says `reading the array inventory requires
  CAP_SYSINFO_HW`; a refused mutation says `administering arrays
  requires CAP_STORAGE_ADMIN`. Every other transport or decode failure
  is a typed service error and the composer's own refusal keeps its
  `Errno`.
- Every diagnostic goes to standard error with a non-zero exit: `1` for
  a runtime refusal (denied capability, unresolved name, composer
  refusal, output failure), `2` for a command line that did not parse.
  No input panics.

Manifest: `CAP_CONSOLE_WRITE` + `CAP_FS_ACCESS` + `CAP_SYSINFO_HW` +
`CAP_STORAGE_ADMIN`. `cargo test -p tairix-mdadm` drives the parser
(every option and refusal, `--`, help/version precedence), the
resolver (node names, full and partial identities, ambiguity, duplicate
and oversized member sets), the renderers (an optimal array, a degraded
array with an absent slot, a rebuild in progress, an empty machine, a
blank-device listing), and the engine against in-memory reader,
controller, and output fixtures (each mode's request and rendering, the
denied read and mutation, a composer refusal, an unresolved name, and
each advisory record), plus the thirteen-locale `OPTIONS` pinning of
the bundled `Help/` documents.

## `df` — report filesystem space usage (`userland/apps/df`)

`tairix-df` is the GNU coreutils `df` (`plans/APPS.md` §12.1 Stage C,
`AGENTS.md` §16.7): one row per mounted filesystem — the volume's size,
used and available space, use percentage, and mount point — or, with
`file` operands, the filesystem containing each operand (chosen by the
longest mount-point prefix, one row per filesystem). Like `mount`'s
listing, the data is a read of live system state through the ungated
`sysinfo-v1` `MOUNT_LIST` query (the shared
`tairix_procinfo::for_each_mount` walk — never a second query client and
never a `/proc`), whose rows carry each backing volume's `VolumeStats`
as the mounted driver reports its own accounting. The default view hides
capacity-less mounts (the in-RAM view bindings) and further mounts of an
already-listed volume, noting the hidden count on fd 3
(`fs.mounts_omitted`, `AGENTS.md` §20.1); `-a` shows everything.
`-T`/`-t`/`-x` add and filter by filesystem type, `-i` reports inode
counts (a dynamic-inode volume reports the honest zeros), `-P` selects
the POSIX portable wording, `--total` appends a summed row, `-l` accepts
the local-only filter (every TAIRiX mount is local), and
`-k`/`-h`/`-H`/`--si`/`-B <size>` select the scale through the shared
`tairix_util::size` vocabulary `du` uses too (`AGENTS.md` §2.2). Columns
are auto-sized; numbers right-align. A missing or relative operand is
diagnosed on standard error and the report continues (exit `1`; mount
points are absolute and the tool never guesses a resolution); filters
that leave nothing report the GNU `no file systems processed` error.
`--output` and `--sync`/`--no-sync` are not yet available (documented in
the bundle's `Help/`). Manifest: `CAP_CONSOLE_WRITE` + `CAP_FS_ACCESS`,
exactly as `ps`. `cargo test -p tairix-df` drives the parser, the
selection/filter/duplicate rules, every column format, the operand
paths, the fd-3 record, and the failure paths against in-memory
`sysinfod`/probe fixtures, plus the thirteen-locale `OPTIONS` pinning.

## `du` — estimate file space usage (`userland/apps/du`)

`tairix-du` is the GNU coreutils `du` (`plans/APPS.md` §12.1 Stage C,
`AGENTS.md` §16.7): it walks each path operand (default `.`) and
reports, post-order, the storage each directory's tree occupies as
`size<TAB>path` rows. The default measure is each node's **allocated**
on-disk bytes — the `allocated` field the mounted format reports — so
sparse or compressed files count what they really occupy;
`--apparent-size`/`-b` measure apparent lengths. `-a` adds a row per
file, `-s` reports only the operands, `-c` appends a grand total, `-d`
bounds the reported depth (sums are unaffected), `-S` excludes
subdirectories from a directory's own row, `-0` NUL-terminates rows, and
`-k`/`-m`/`-h`/`--si`/`-B <size>` select the scale through the shared
`tairix_util::size` vocabulary (`AGENTS.md` §2.2), later selections
winning as in GNU. The walk is an explicit frame stack (a deep tree can
never exhaust the call stack) over the kernel-authorised `fs_*`
syscalls, and it is I/O-frugal by design: every `fs_readdir` entry
carries the child's kind, sizes, and modification stamp, so a directory
of *n* children
costs one open and one listing — never *n* per-child open/stat/close
round-trips, each a fresh full path resolution on an uncached,
authenticated volume; only operands are stat'ed individually. An
unreachable operand is diagnosed on standard error and the walk
continues (exit `1`), an unreadable directory contributing nothing
rather than a guessed partial sum. `du` does not yet deduplicate a
multiply-named file, so the GNU link-deduplication switches do not exist
and a hard-linked file counts once per name it is reached through; `-x`
awaits device identity (both documented in the bundle's `Help/`). Manifest: `CAP_CONSOLE_WRITE` + `CAP_FS_ACCESS`. `cargo test
-p tairix-du` drives the parser (clusters, values, conflicts), the
post-order accumulation, every option's rendering, the diagnosed-path
paths, and the thirteen-locale `OPTIONS` pinning against an in-memory
tree fixture.

## `cat` — concatenate files to the terminal (`userland/apps/cat`)

`tairix-cat` concatenates files and standard input (`AGENTS.md` §3; a
`plans/APPS.md` command app registered at `/System/Commands/cat.app/Run`,
so the shell resolves the bare word `cat` to it). It reads each of its
sources in order and writes the bytes to the terminal. A source is
a path, standard input — the `-` operand, and the default when
no operand is given — or a typed resource reference (`sys:random`).
The reference support is not cat's own: `tairix_rt::File::open` — the
one open-by-name path every command app uses — applies the shared
`lib/resref` spelling rule and routes a reference to the kernel's
capability-checked `resource_open` resolver rather than the filesystem,
so `cat sys:random` streams the kernel CSPRNG and every other tool that
opens a named operand accepts the same spellings. A malformed reference
in a registered namespace fails closed (never a filename fallback), and
an on-disk name containing `:` stays reachable as `./name`. The option
surface is the GNU `cat` set
(`AGENTS.md` §16.7): numbering (`-n`, non-blank `-b`), blank-line
squeezing (`-s`), and the visibility markers (`-E`, `-T`, `-v`, and the
combinations `-e`, `-t`, `-A`). `-h`/`-?` render the tool's own
short help from its bundled `Help/` tree through the shared `lib/help`
engine (`plans/APPS.md` §4), in the locale the inherited `LANG` variable
names, falling back to the usage banner when the tree is unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-help` engine, so it never links a kernel or driver crate
(`AGENTS.md` §17.4). Its manifest requests `CAP_CONSOLE_WRITE`,
`CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every path per-inode under the
caller's attested identity.

### Grammar

```
cat [-AbeEnstTuv] [--] [file...]
```

| Token            | Meaning                                            |
|------------------|----------------------------------------------------|
| `-A`, `--show-all` | equivalent to `-vET`                             |
| `-b`, `--number-nonblank` | number non-empty output lines; overrides `-n` |
| `-e`             | equivalent to `-vE`                                |
| `-E`, `--show-ends` | print `$` at the end of each line               |
| `-n`, `--number` | number output lines, continuously across sources   |
| `-s`, `--squeeze-blank` | suppress repeated adjacent blank lines       |
| `-t`             | equivalent to `-vT`                                |
| `-T`, `--show-tabs` | print TAB as `^I`                               |
| `-u`             | accepted and ignored (output is unbuffered)        |
| `-v`, `--show-nonprinting` | `^`/`M-` notation for control and non-ASCII bytes |
| `-h`, `-?`, `--help` | show the tool's short help (wins immediately)  |
| `--`             | end option parsing; every later argument is a path |
| `-`              | standard input                                     |
| *path*           | a file to read                                     |

Short options bundle as in the GNU tool (`-nE` is `-n -E`). With no
`path` (or `-`) operand the single source is standard input. Any other
leading-dash argument before `--` is a `CatError::Usage` error, never a
silently ignored token.

### A stream/render machine, not a data source

`run` pulls bytes from each source in fixed-size chunks and writes them
— shaped by the render options — to the terminal. The operations that
reach the outside world are injected seams, the same discipline as
`sysinfo`'s `Transport`/`Output`:

- `FileSource` — read a byte range of a named file, streaming it with an
  advancing offset until a read returns zero (end-of-file).
- `Input` — read the next bytes of standard input until end-of-input.
- `Output` — write rendered bytes to the terminal.
- `tairix_help::HelpSource` — the tool's own bundled `Help/` tree, read
  by the short-help switches; the documents are authored once on disk in
  the bundle, never embedded in the binary (`plans/APPS.md` §6.1).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, streaming, and numbering
decision is testable without a kernel.

### Rendering

`-n` numbers each line once, when its first byte appears; `-b` numbers
only non-empty lines and never numbers a blank one. The line state is
carried across read chunks and across sources, so a line that straddles
a chunk boundary — or a file boundary — is numbered exactly once, and
numbering is continuous across every source. `-s` squeezes a run of
blank lines to one — also across chunk and source boundaries — and a
squeezed line is neither written nor numbered. `-E` prints `$` before
each newline, `-T` renders TAB as `^I`, and `-v` renders other control
bytes as `^X` and non-ASCII bytes in `M-` notation (`M-^@` … `M-^?`),
leaving line feeds and tabs alone.

### Fail closed

- An unrecognised option is a `CatError::Usage` that reads nothing.
- A source that cannot be read surfaces the underlying `Errno` as
  `CatError::Read` and stops before any later source (a missing file
  among several aborts rather than skipping silently).
- A failed terminal write is `CatError::Output`.
- A seam that reports more bytes than the read buffer holds is refused
  (`CatError::Read`) rather than indexed out of bounds — no panic
  (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-cat` drives the parser and the streaming engine
against an in-memory filesystem, a buffered standard input, and a
recording output: the command grammar (every option, `-`/`--`, and the
usage-error path, bundled short flags, and the `-b`-overrides-`-n`
rule), single- and multi-file concatenation, standard-input streaming,
continuous line numbering across files and across a chunk boundary,
non-blank numbering, blank-line squeezing (including across source
boundaries and its interaction with numbering), the `$`/`^I`/`^`/`M-`
marker renderings, a missing trailing newline, an empty numbered file,
chunked streaming of a multi-chunk file, the missing-file and
dead-console fail-closed paths, the short-help render from a Help
document with its usage-banner fallback, and the switch-drift pin that
every locale's `OPTIONS` section documents exactly the parser's switches
(`plans/APPS.md` §3.1).

## `configure` — read and set the boot-time system configuration (`userland/apps/configure`)

The `sysctl`-shaped settings command over the boot-time configuration store
at `/System/Settings/Configuration/system.conf`. With no operand it lists
every setting of the closed registry with its current value; with a key it
shows that setting; with a key and a value it changes it:

```text
configure                          # list every setting
configure os.loginType             # show one setting
configure os.loginType graphical   # set it (boot to the graphical login)
configure cache.all off            # disable every memory cache system-wide
configure cache.filesystem off     # disable only the filesystem cache
```

The store's grammar, closed key registry, fail-closed parse, and canonical
render are the shared `lib/sysconfig` engine (`docs/src/lib/sysconfig.md`)
— the same engine every boot-time consumer reads through, so this writer
and those readers can never diverge. An unknown key, a value outside its
key's closed set, or a store document the engine cannot fully parse is
refused with the reason (and the valid choices) stated, and changes
nothing; a set rewrites the whole document in canonical form, never a
partial patch. The store lives on the encrypted root, so a change takes
effect when its consumer next parses it — `os.loginType` at the next login
prompt, and the `cache.*` caching switches at the next boot's root unlock
(the kernel applies them into its cache-admission control). The `cache.*`
keys are a dedicated caching domain: a master `cache.all` (`on`/`off`)
ceiling over the per-class `cache.filesystem` / `cache.block` /
`cache.transform` / `cache.semantic` (`auto`/`off`) switches — see
`docs/src/lib/sysconfig.md`.

The pure grammar/engine core is host-tested against in-memory seams; the
`Run` binary wires the syscall-backed store file, the shared own-bundle
help source, and the inherited standard output. Manifest:
`CAP_CONSOLE_WRITE` (the listing and short help) and `CAP_FS_ACCESS` (the
store and the bundle's own `Help/` tree) — write authority is the
`/System/Settings` per-inode policy under the caller's attested identity,
so an ordinary account can read settings but a change is refused with its
reason (fail closed, nothing applied). Exit status: `0` success, `1` a
store/output failure, `2` a usage error (unknown key / out-of-set value
included).

## `clear` — clear the terminal screen (`userland/apps/clear`)

`tairix-clear` writes the byte sequence that moves the cursor home and
erases the display — the ncurses `clear` model (a `plans/APPS.md`
command app registered at `/System/Commands/clear.app/Run`, so the shell
resolves the bare word `clear` to it). Which bytes are written is
decided by the inherited `TERM` through the compiled-in `lib/termcap`
capability database, and the sequence is encoded through the one shared
`lib/vt` vocabulary — never a hand-rolled escape string. Fail-closed: an
unknown `TERM` degrades to the dumb baseline, which cannot clear, and
the tool reports that on stderr (exit `1`) instead of printing escape
garbage. `-x` (the GNU "do not clear the scrollback" switch) is accepted
for script compatibility; a TAIRiX console keeps no scrollback, so the
output is identical with and without it — the divergence is documented
in the tool's `Help/` documents. `-h`/`-?` render the tool's own short
help through the shared `lib/help` engine.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline.
`cargo test -p tairix-clear` drives the parser (every switch and the
usage-error path), the per-terminal byte selection (xterm/VT100 clear,
dumb refusal), and the locale switch-drift pin.

## `reset` — restore the terminal to a sane state (`userland/apps/reset`)

`tairix-reset` undoes the state a crashed full-screen program can leave
behind (a `plans/APPS.md` command app registered at
`/System/Commands/reset.app/Run`). It first restores the **cooked** input
discipline through `stream_input_mode` (`tairix_rt::set_input_mode`) — a
crashed viewer may have left the console raw, with neither echo nor
indicator — then writes the restoration sequence for the `TERM`-named
terminal: leave the alternate screen, show the cursor, reset the graphic
rendition and the scroll region, and finally home + erase. Every
operation is a `tairix_vt::Op` the terminal's `lib/termcap` profile
accepts; an operation the terminal lacks is omitted, and the dumb
baseline gets only the discipline restore. `-h`/`-?` render the tool's
own short help through the shared `lib/help` engine.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ` (the discipline restore), and
`CAP_FS_ACCESS` — within the session baseline. `cargo test -p
tairix-reset` drives the parser, the per-terminal restoration sequences
(xterm full set, VT100 subset, dumb empty), and the locale switch-drift
pin.

## `true` / `false` — do nothing, with a fixed status (`userland/apps/true`, `userland/apps/false`)

`tairix-true` and `tairix-false` are the GNU coreutils status tools
(`plans/APPS.md` §12.1 Stage C store bundles): each ignores every
argument and exits `0` (`true`) or `1` (`false`), giving scripts a
command that always succeeds or always fails. Parsing is infallible —
there is no usage error — and only a **first** argument of
`-h`/`-?`/`--help` (the position GNU honours `--help` in) renders the
tool's own short help through the shared `lib/help` engine. One
documented divergence: `false -h` exits `0` (the `plans/APPS.md` §4
short-help convention), where GNU `false --help` exits `1`.

Both crates are `no_std` (no `alloc` in the library), have no `unsafe`,
and no `unwrap`/`expect`/`panic!` in production paths. Each manifest
requests `CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` (the short-help read) —
within the session baseline. `cargo test -p tairix-true -p tairix-false`
drives the ignore-everything and first-argument-help rules and the
locale switch-drift pins.

## `yes` — repeatedly output a line of text (`userland/apps/yes`)

`tairix-yes` is the GNU coreutils repeater (a `plans/APPS.md` §12.1
Stage C store bundle): it writes its operands joined by single spaces —
or `y` when none are given — followed by a newline, until its output
stops accepting bytes or the process is terminated. Option handling
matches GNU: an unrecognised option is a usage error, option scanning
stops at the first operand (`yes a -x` prints `a -x`), and `yes -- -x`
prints `-x`. The line is repeated into a bounded whole-line block (up to
4 KiB) so the endless writer pays one write per block, and a full stream
backing blocks the write kernel-side — the tool never idle-spins.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline.
`cargo test -p tairix-yes` drives the parser (operand/option rules, the
`--` spelling), the block builder (default line, whole-line packing, the
over-long-line floor), the closed-pipe stop condition through an
injected output, and the locale switch-drift pin.

## `seq` — print a sequence of numbers (`userland/apps/seq`)

`tairix-seq` is the GNU coreutils sequence generator (a `plans/APPS.md`
§12.1 Stage C store bundle): print the numbers from FIRST to LAST in
steps of INCREMENT (both defaulting to 1), with the full GNU surface —
`-f`/`--format` (a printf-style floating-point format with one `%`
directive of type `e`/`f`/`g`/`a`, validated with the GNU diagnostics),
`-s`/`--separator`, and `-w`/`--equal-width` (mutually exclusive with
`-f`) — and the GNU output rules. The default precision and the `-w`
width are inferred from the operands' spellings exactly as GNU's
`scan_arg` infers them; plain integer runs (including `1e1`/`0x14`
spellings and an `inf` LAST) are generated in exact decimal string
arithmetic, arbitrarily large; and the floating-point path prints the
value one step past LAST when it renders equal to it (the GNU rounding
rule). Option scanning matches GNU `seq`: no permutation, a leading
negative number is an operand, and the operand count, format, and
`-f`/`-w` conflict are diagnosed in the GNU order. The `-f` renderer
implements C-locale `%e`/`%f`/`%g`/`%a` semantics (flags `-+#0 '`,
width, precision) so a format prints what C's `printf` prints for a
`double`. One deliberate divergence, documented in the crate: GNU
computes the floating-point path in `long double`, TAIRiX in `f64` —
visibly, `%a` prints the C `double` spelling (`0x1.8p+0`) where glibc's
`%La` normalisation spells the same value `0xcp-3`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` (the short-help read) — within
the session baseline. `cargo test -p tairix-seq` drives the number scan
(decimal/hex/inf spellings, the width/precision algebra, single-step
rounding to the subnormal and overflow edges), the format validation and
renderer against glibc-verified outputs, the parser (GNU scan rules and
diagnostic order), the generation engine (fast-path selection, exact
big-integer runs, separators across the flush boundary, the extra-number
rule), and the locale switch-drift pin.

## `printf` — format and print data (`userland/apps/printf`)

`tairix-printf` is the GNU coreutils formatter (a `plans/APPS.md` §12.1
Stage C store bundle): print ARGUMENTs under the control of FORMAT —
literal text, backslash escapes (`\NNN`, `\xHH`, `\uHHHH`/`\UHHHHHHHH`,
and `\c`, which ends all output), and `%` directives (`diouxX` integers,
`eEfFgGaA` floats, `%c`, `%s`, `%b` with its own `\0NNN`-octal escapes,
`%q` shell quoting, `%%`) with the C flags (`-+ #0'`), width, and
precision, both settable to `*`. The FORMAT is reused until every
ARGUMENT is consumed. Argument conversion follows GNU exactly: base-0
integers and `strtod` floats (through the shared `tairix_util::cnum`
scanner), `'x` character constants, silent zero/empty for a missing or
empty argument, and the GNU diagnostics ("expected a numeric value",
"value not completely converted", "Numerical result out of range", the
character-constant warning) with the run continuing and exiting `1`. An
invalid conversion specification — an unknown letter, or a
flag/width/precision on a conversion that rejects it (`%b`/`%q` take
none; the per-conversion validity table is probe-pinned against GNU) —
and a malformed escape are fatal, with output already rendered kept, as
GNU keeps it. Floats render through the shared `tairix_util::cfloat`
engine; `%q` reproduces coreutils `quotearg`'s shell-escape style
(probe-pinned: bare/safe words, `''`, `"it's"` double-quoting, `\'`
splices, `$'\t\n'` control groups, octal-escaped non-ASCII bytes).

Two deliberate divergences, documented in the bundle's help: floats
compute in `f64` rather than `long double` (the `seq` precedent — a
value beyond double's range prints `inf`), and a *first* argument of
`-h`/`-?`/`--help` serves the TAIRiX short-help convention where GNU
would treat it as FORMAT (`printf -- -h...` spells such a format).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` (the short-help read) — within
the session baseline. `cargo test -p tairix-printf` drives the argument
converter (base-0/char-constant/float readings, saturation, wrapping),
the template engine (escapes, every conversion, the validity table, `\c`
halting, `*` width/precision, flush-before-fatal), the `%q` quoter, the
reuse loop with its diagnostics and exit statuses, and the locale
switch-drift pin — all against observed GNU coreutils behaviour.

## `basename` / `dirname` — lexical name surgery (`userland/apps/basename`, `userland/apps/dirname`)

`tairix-basename` and `tairix-dirname` are the POSIX name tools
(`plans/APPS.md` §12.1 Stage C store bundles): purely lexical string
surgery — no operand path is resolved, normalised, or touched on disk.
`basename` prints the final component of each spelling, optionally with
a trailing suffix removed, with the full GNU surface (`NAME [SUFFIX]`,
`-a`/`--multiple`, `-s`/`--suffix` implying `-a`, `-z`/`--zero`,
bundles, permutation); `dirname` prints each spelling with its last
component removed (`-z`/`--zero`, `NAME...`).

One TAIRiX extension, shared by both: a `Name:/` alias root
(`plans/DRIVES.md`) plays the role POSIX gives `/` — it is never
stripped into, so `dirname Home:/tools` is `Home:/` exactly as
`dirname /tools` is `/`. Where the root prefix ends is decided by the
path grammar's own exported rule (`tairix_path::alias_root_len`), so
neither tool carries a second path parser.

Both crates are `no_std` (with `alloc`), have no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths; each manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` (the short-help read) — within
the session baseline. `cargo test -p tairix-basename -p tairix-dirname`
drives the parsers (operand forms, suffix spellings, bundles,
permutation, refusals), the POSIX algorithms (root, slash-run, empty,
suffix, and alias-root cases), and the locale switch-drift pins.

## `mkdir` / `rmdir` — make and remove directories (`userland/apps/mkdir`, `userland/apps/rmdir`)

`tairix-mkdir` and `tairix-rmdir` are the GNU coreutils directory tools
(`plans/APPS.md` §12.1 Stage C store bundles). `mkdir` creates each
operand through `fs_mkdir` (`-p`/`--parents` creates missing ancestors
and tolerates an operand that is already a directory; `-v`/`--verbose`
reports `mkdir: created directory 'dir'`); GNU's `-m`/`--mode` remains
staged — its kernel prerequisite (`fs_set_mode`, syscall 74) now exists,
and the flag lands with its own tests in its own change, never stubbed.
`rmdir` removes each (empty) directory operand through
the **directory-only** `fs_unlink` (`UnlinkFlags::DIRECTORY`): the
filesystem decides the node's kind atomically in the same locked walk
that removes it, so the tool carries no stat/remove race — a file is
refused with the dedicated `Errno::NotADirectory` and a populated
directory with `Errno::NotEmpty`, which `--ignore-fail-on-non-empty`
(and only it) tolerates. `-p` removes ancestors innermost first and
never asks to remove a bare root; `-v` reports the GNU-worded
`rmdir: removing directory, 'dir'` attempt line.

Both tools' `-p` walks spell each ancestor through the shared path
grammar's own rule (`tairix_path::Path::prefix`), so alias-rooted
operands (`Home:/tools/bin`) walk correctly and neither tool carries a
second path parser; an operand the grammar cannot parse is handed to
the kernel whole, which stays the one validator.

Both crates are `no_std` (with `alloc`), have no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths; each manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every operation per-inode.
`cargo test -p tairix-mkdir -p tairix-rmdir` drives the parsers
(switches, clusters, `--`, refusals), the engines over in-memory seams
(ancestor walks, existing-directory tolerance, the tolerated non-empty
refusal, first-failure-stops ordering, GNU `-v` wording), and the
locale switch-drift pins.

## `ln` — create symbolic links (`userland/apps/ln`)

`tairix-ln` is the GNU coreutils `ln` (a `plans/SYMLINKS.md` S4 store
bundle). It creates a symbolic link naming each target through
`fs_symlink`, in every operand shape the GNU tool accepts: one operand
links under the target's own name in the working directory; two make the
second a directory to fill when it is one (or a link to one, unless `-n`)
and the link's name otherwise; three or more require the last to be a
directory; and `-t dir` makes every operand a target.

**Both kinds of link are real.** Without `-s` the link is a hard one
(`fs_link`): a second directory entry for the target's own inode, so both
names reach one file and its storage survives until the last name goes.
`-L` gives the second name to what a symbolic target *names*, `-P` — the
default — links the target as spelled and follows no final link, and
`-d`/`-F` accept a directory operand whose link is still refused
`IsADirectory` (no principal may give a directory a second name). Two
switches stay refused for reasons that are not about hard links:
`-b`/`-S` because this workspace has no backup machinery (`cp`/`mv` omit
them too), and `-r` because computing a target relative to the link's own
directory needs a canonicalising resolution the ABI does not offer, and a
*lexical* one would name a different node the moment a link were involved —
the very collapse the resolver forbids. Every refusal is a usage error
naming the switch; nothing is silently ignored.

The target is stored **verbatim** and never resolved: it may be relative,
carry `..`, and name nothing at all, so `ln -s` may legitimately create a
dangling link. Its *grammar* is still checked kernel-side before it is
stored, so a target no resolver could walk is refused rather than written,
and creating a link grants no authority over what it names — every later
use is authorised component by component under the caller's own identity.

A link name that is already taken is refused unless `-f` or `-i` says to
replace it, and replacing it **removes** that name first: a create or
truncate follows a final link, so leaving one in place would act on
whatever it pointed at. A directory is never replaced. An unanswerable
`-i` question is never consent, and the first failure stops the run before
any later target, exactly as `cp` and `mv` do.

### Grammar

```
ln -s [-finvT] [-t dir] [--] target... [link_name]
```

| Token            | Meaning                                            |
|------------------|----------------------------------------------------|
| `-s`, `--symbolic` | make symbolic links (required — see above)       |
| `-f`, `--force`  | remove an existing link name, then create the link |
| `-i`, `--interactive` | ask before removing an existing link name; later of `-f`/`-i` wins |
| `-n`, `--no-dereference` | treat a link-to-directory destination as the name it also is |
| `-v`, `--verbose` | report each link as `'link' -> 'target'`           |
| `-t dir`, `--target-directory=dir` | create every link in `dir`       |
| `-T`, `--no-target-directory` | the destination is a link name (exactly two operands) |
| `-h`, `-?`, `--help` | show the tool's short help (wins immediately)  |
| `--`             | end option parsing; every later argument is an operand |

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ` (the one line `-i` reads), and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS still
authorises every path per-inode. Exit status is the GNU `ln` shape: `0`
when every link was created, `1` otherwise (there is no separate usage
status). `cargo test -p tairix-ln` drives the parser (every operand shape,
clusters, `--`, the `-t`/`-T` contradiction, refusals) and the engine over
in-memory seams (the four destination readings, the taken-name refusal,
`-f`/`-i` replacement removing the name first, the never-replaced
directory, the hard-link refusal touching nothing, and a format that
stores no links reporting its permanent limit).

## `head` — output the first part of files (`userland/apps/head`)

`tairix-head` is the GNU coreutils `head` (`plans/APPS.md` §12.1 Stage C
store bundle): the first 10 lines of each file operand (or standard
input), or the amount `-n`/`--lines` and `-c`/`--bytes` select — a
leading `-` on the count means "everything but the last COUNT", and a
count takes the GNU multiplier suffixes (`b`, `kB`, `K`, `MB`, `M`, …,
and the `iB` forms; a count beyond `u64` saturates, which is observably
identical since no input can exceed it). Multi-file output carries the
`==> file <==` headers (`-q`/`--quiet`/`--silent` and `-v`/`--verbose`
override; a blank line separates parts), `-z`/`--zero-terminated`
switches the line delimiter to NUL, and the obsolete first-argument
`-COUNT[bkm][lqvz]` form is honoured — including GNU's quirk that a
multiplier letter keeps scaling the count after a later `l`.

The streaming engine is constant-memory per source: the head modes stop
reading at the count; `-c -N` retains a circular ring of the last `N`
bytes and emits what ages out; `-n -N` retains a queue of the last `N`
lines whose unterminated final fragment counts as a line, exactly as in
the GNU tool. A file that cannot be opened is diagnosed (no header) and
the run continues; a mid-stream read error is diagnosed after the bytes
already served; the exit status reflects any failure.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths; its manifest requests
the console pair and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every operand per-inode.
`cargo test -p tairix-head` drives the parser (counts, suffixes, the
obsolete form, clusters, `--`, refusals), the engine over in-memory
seams (chunked streams, headers, both elide modes including a
ring-vs-reference chunking matrix, per-file diagnostics), and the
locale switch-drift pins.

## `wc` — newline, word, and byte counts (`userland/apps/wc`)

`tairix-wc` is the GNU coreutils `wc` (`plans/APPS.md` §12.1 Stage C
store bundle): the line/word/byte counts of each file operand (or
standard input), with `-m`/`--chars` (decoded UTF-8 characters — an
encoding-error byte counts as a byte, not a character) and
`-L`/`--max-line-length` (display columns through the one OS-wide
`tairix_vt::char_width` definition, tabs advancing to 8-column stops)
as the further selectors; counts always print in the fixed
lines/words/chars/bytes/max-line order. `--total={auto,always,only,
never}` (GNU `argmatch` prefix matching) controls the `total` row, and
`--files0-from=F` reads the NUL-separated operand list from a file or
standard input, refusing file operands alongside it and validating
each record with its number.

The output width follows the GNU rule exactly: columns are sized from
the decimal width of the summed regular-file operand sizes (probed
through the seam's three-way `SizeProbe`), any standard-input or
non-regular operand forces the 7-column minimum, an unprobeable
operand contributes nothing, and the single-input/single-count,
`--files0-from`, and `--total=only` forms print unpadded. Counting is
constant-memory via an incremental UTF-8 decoder that carries partial
sequences across chunk boundaries. An unopenable input is diagnosed
with no row; a mid-stream read error keeps the partial row (which
still joins the total) and is diagnosed after it, exactly as in the
GNU tool.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths; its manifest requests
the console pair and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every operand per-inode.
`cargo test -p tairix-wc` drives the parser (selectors, `--total`
argmatch, `--files0-from` conflicts), the counter (chunk-boundary
invariance, encoding errors, tab stops, wide characters), the client
(width rules, totals, files0 records, error rows), and the locale
switch-drift pins.

## `tee` — copy standard input to standard output and files (`userland/apps/tee`)

`tairix-tee` is the GNU coreutils `tee` (`plans/APPS.md` §12.1 Stage C
store bundle): it copies standard input to standard output and to each
file operand (created if absent; overwritten, or appended with
`-a`/`--append`), streaming in constant memory — one 4 KiB chunk fanned
out to every still-live output — and stopping once no output remains.
`--output-error[=MODE]` selects how a failed output is treated
(`warn`, `warn-nopipe`, `exit`, `exit-nopipe`, matched with GNU
`argmatch` prefixes; the value arrives only attached with `=`, and a
bare `--output-error` — or `-p` — selects `warn-nopipe`). A failed
output is diagnosed, dropped, and the run continues (or stops, under an
`exit` mode), exactly as GNU `tee.c` nulls a failed descriptor; an
unopenable file is likewise diagnosed and — only under an `exit` mode —
immediately fatal. A `-` operand names a file called `-`, as in GNU.

Two documented divergences follow from TAIRiX having no `SIGPIPE` and
no per-process signal disposition. The "pipe" class of the GNU modes
maps to the standard-output copy — the one output of this tool that can
be a pipe — where a consumer going away surfaces as a write error,
never a signal: without `--output-error` it stops the run with the
reason stated on standard error (the fail-loud analogue of GNU dying of
`SIGPIPE`); under a `-nopipe` mode it is dropped silently without
affecting the exit status. And GNU `tee -i`/`--ignore-interrupts` is
staged, not stubbed: there is no signal disposition to set today, so
the switch is refused as unrecognised and registers in the change that
lands that kernel work (the `mkdir -m` precedent).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths; its manifest requests
the console pair and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every operand per-inode.
`cargo test -p tairix-tee` drives the parser (switches, bundles, the
argmatch modes, `--`, refusals), the engine over in-memory seams
(fan-out, chunking, append vs overwrite, every mode × failure verdict,
the no-output early stop, read errors), and the locale switch-drift
pins.

## `whoami` — print the current user's account name (`userland/apps/whoami`)

`tairix-whoami` is the GNU coreutils identity tool (a `plans/APPS.md`
§12.1 Stage C store bundle): it prints the user name associated with
the caller's identity, followed by a newline, and nothing else. TAIRiX
has no `/etc/passwd`, so the uid comes from the caller's kernel-attested
origin record (the ungated `self_origin` syscall — a pure self-observer)
and the uid → name pairing from the ungated `USER_DIRECTORY` query
`sysinfod` serves, walked through the shared `lib/procinfo`
account-directory helper — the same one `top`'s `USER` column uses,
never a second copy. The tool takes no operands (`extra operand`) and
knows no options beyond the reserved `-h`/`-?`/`--help` short-help
switches; like GNU getopt's permutation, an option is honoured wherever
it sits (`whoami foo --help` serves the help) and the first bad option
is diagnosed before the operand complaint. A uid with no directory
entry is the GNU `cannot find name for user ID` diagnostic; a failed
directory walk is a service error, never misreported as a missing name.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its manifest requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` (the short-help read) — within
the session baseline. `cargo test -p tairix-whoami` drives the parser
(operand/option rules, getopt ordering, the `--` spelling), the lookup
engine against an in-memory directory fixture (found name, missing
name, failed walk, failed identity read, closed terminal), the
short-help fallback, and the locale switch-drift pin.

## `ls` — list directory contents (`userland/apps/ls`)

`tairix-ls` lists directory contents (`AGENTS.md` §3; a `plans/APPS.md`
command app registered at `/System/Commands/ls.app/Run`, so the shell
resolves the bare word `ls` to it). It inspects each of its path
operands in order: a non-directory operand is listed by name, and a
directory operand has its entries listed, sorted by name (by size under
`-S`, by timestamp newest-first under `-t`, by extension under `-X`, by
natural version order under `-v`, or not at all — directory order —
under `-U`/`-f`, chosen by name with `--sort`), unless `-d` names the
directory itself. With no operand it lists the current directory (`.`).
The option surface is the GNU `ls` set (`AGENTS.md` §16.7): `-a`/`-A`
reveal dotfiles, `-l` (and `-n`/`-g`/`-o`) select the long format, `-h`
scales its sizes, `-i` prefixes the node number, `-c`/`-u`/`--time`
select which timestamp the long format shows and `-t` sorts by,
`--time-style`/`--full-time` set its rendering, `-s`
prefixes each entry with its **allocated** size in 1024-byte blocks (the
real on-disk allocation the filesystem reports through `fs_stat`, never
a value derived from the byte length) with a `total` line per directory
listing (printed under `-l` too, as in the GNU tool), `-R` recurses,
`-r` reverses, `--group-directories-first` floats directories to the top
of the sort (even under `-r`), `-f` shows every entry unsorted (enabling
`-a` and disabling `-l`/`-s`), `-F`/`-p` append indicators, `-Q` quotes,
and `-m`/`-1` pick the arrangement. `-?`/`--help` render the tool's own
short help from its bundled `Help/` tree through the shared `lib/help`
engine (`plans/APPS.md` §4), in the locale the inherited `LANG` variable
names, falling back to the usage banner when the tree is unavailable
(`-h` keeps its GNU human-readable meaning, so it is not a help switch
here).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-help`/`tairix-vt` engines, so it never links a kernel or driver
crate (`AGENTS.md` §17.4). The long format's **link-count column** is the
count the filesystem itself records, carried up from the driver and never
derived; a format that keeps none answers `1`. Its manifest requests
`CAP_CONSOLE_WRITE`
plus `CAP_FS_ACCESS` — within the session baseline — and the secured VFS
still authorises every path per-inode under the caller's attested
identity.

### Grammar

```
ls [-aABbCcdFfGgHhikILlmNnopQqrRsSTtUuvXx1] [-w cols] [-I PATTERN] [--block-size=SIZE] [--si] [--format=WORD] [--indicator-style=WORD] [--hide=PATTERN] [--time=WORD] [--time-style=STYLE] [--sort=WORD] [--quoting-style=STYLE] [--full-time] [--author] [--file-type] [--group-directories-first] [--zero] [--color[=WHEN]] [--] [path...]
```

| Token            | Meaning                                            |
|------------------|----------------------------------------------------|
| `-a`, `--all`    | include entries whose name begins with `.`         |
| `-A`, `--almost-all` | like `-a`, but never list `.` or `..`          |
| `-C`             | columns filled top-to-bottom (terminal default)    |
| `-d`, `--directory` | list directory operands themselves              |
| `-F`, `--classify` | append `/` to directories, `*` to executables    |
| `--file-type`    | append `/` to directories, never `*` (`--indicator-style=file-type`) |
| `--indicator-style=WORD` | `none`/`slash` (`-p`)/`file-type`/`classify` (`-F`) |
| `-g`             | long format without the owner column               |
| `-G`, `--no-group` | omit the group column (does not select `-l`)     |
| `--author`       | with `-l`, print the author (owner) column         |
| `-h`, `--human-readable` | human-readable sizes, base 1024 (`1.1K`, `23M`) |
| `--si`           | human-readable sizes, base 1000 (`1.1k`, `23M`)    |
| `-k`, `--kibibytes` | 1024-byte blocks for `-s`/`total` (the default) |
| `--block-size=SIZE` | scale file and `-s` sizes by SIZE (int, or `K`/`KiB`/`KB`…) |
| `-l`             | long format: mode, owner, group, size, then name   |
| `--format=WORD`  | `long`/`verbose`/`single-column`/`vertical`/`across`/`horizontal`/`commas` |
| `-m`             | comma-separated names, wrapped to the width        |
| `-n`, `--numeric-uid-gid` | long format, numeric owner/group (same as `-l`) |
| `-o`             | long format without the group column               |
| `-p`             | append `/` to directories                          |
| `-N`, `--literal` | names verbatim, no quoting (`--quoting-style=literal`) |
| `-Q`, `--quote-name` | C-style quoting (`--quoting-style=c`)          |
| `-b`, `--escape` | like `-Q` without the quotes, spaces escaped (`--quoting-style=escape`) |
| `--quoting-style=WORD` | `literal`/`shell`/`shell-always`/`shell-escape`/`shell-escape-always`/`c`/`escape` |
| `-q`, `--hide-control-chars` | show nongraphic characters as `?` (terminal default) |
| `--show-control-chars` | print nongraphic characters as-is (non-terminal default) |
| `-r`, `--reverse` | reverse the sort order                            |
| `-R`, `--recursive` | list subdirectories recursively                 |
| `-L`, `--dereference` | show what each symbolic link names, not the link |
| `-H`, `--dereference-command-line` | dereference only the command-line operands |
| `--dereference-command-line-symlink-to-dir` | dereference a command-line link *to a directory* only (the default) |
| `-s`, `--size`   | allocated size per entry, in blocks (scaled by `-h`/`--si`/`--block-size`) |
| `-S`             | sort by size, largest first                        |
| `-t`             | sort by the selected timestamp, newest first       |
| `-U`             | do not sort; list entries in directory order       |
| `-X`             | sort by file-name extension, ties by name          |
| `-v`             | natural version sort (`f2` before `f10`)           |
| `-f`             | unsorted, show all; enables `-a`, disables `-l`/`-s` |
| `--sort=WORD`    | sort key: `none`/`size`/`time`/`version`/`extension`/`name` |
| `--group-directories-first` | list directories before other entries; first even under `-r` |
| `-c`             | select/sort by the metadata-change time (ctime)    |
| `-u`             | select/sort by the access time (atime)             |
| `-i`, `--inode`  | print each entry's node number                     |
| `-B`, `--ignore-backups` | do not list names ending in `~` (every mode)   |
| `-I`, `--ignore=PATTERN` | do not list names matching the glob (repeatable; every mode) |
| `--hide=PATTERN` | like `--ignore`, but suppressed by `-a` / `-A`      |
| `--time=WORD`    | timestamp shown/sorted: `atime`/`ctime`/`mtime`/`birth` |
| `--time-style=STYLE` | `locale`/`long-iso`/`full-iso`/`iso` (no `+FORMAT`) |
| `--full-time`    | like `-l --time-style=full-iso`                    |
| `-T`, `--tabsize <cols>` | column tab stop (default 8; `0` = spaces)    |
| `-w`, `--width <cols>` | output width in columns (`0` = unlimited)     |
| `-x`             | columns filled left-to-right                       |
| `-1`             | one name per line (default when not a terminal)    |
| `--zero`         | NUL line terminator; implies single-column, literal, shown control chars |
| `--color[=WHEN]` | colour names by kind: `auto` (default), `always`, `never` (bare = `always`) |
| `-?`, `--help`   | show the tool's short help (wins immediately)      |
| `--`             | end option parsing; every later argument is a path |
| *path*           | a file or directory to list                        |

With no `path` operand `ls` lists the current directory. Short options
may be combined into one argument (e.g. `-la` is `-l -a`); an
unrecognised letter anywhere in such a cluster is a `LsError::Usage`
error. The bare `-` is a path named `-`, not an option.

### Symbolic links

A link renders with the type letter `l` and, in the long format, as
`name -> target` — the target exactly as stored, unresolved, which is what
the link holds. Which links a listing *resolves* is the GNU four-state
dereference posture, and it decides what every row is:

| Posture | Selected by | Operands | Entries inside a listing |
|---|---|---|---|
| never | `-l`, `-d`, `-F` | the link itself | the link itself |
| command-line link to a directory | the default otherwise | resolved only when it names a directory (so `ls linkdir` lists it) | the link itself |
| command-line | `-H` | resolved | the link itself |
| always | `-L` | resolved | resolved |

The posture selects a per-path reading rather than one per listing, which is
exactly what `-H` exists to express. A dangling link therefore lists normally
under `-l`; only a posture that resolves it reports the target as
unreachable.

A path that cannot be inspected or read never ends the listing: the reason
goes to standard error, that path is skipped, and the remaining operands and
entries are still listed — which is what makes `ls -L` over a directory
holding a dangling link useful. A skipped *entry* keeps the type letter the
directory stream gave it and renders `?` for every cell a stat would have
filled, never a fabricated zero. The exit status is the GNU grade: `0` when
everything listed, `1` for a problem inside a listing, `2` for a command-line
operand that could not be reached (or a usage error).

Because `-L` is the only posture under which `-R` can walk into a directory a
link names, the recursive walk carries the node identities of the directories
it came through and reports `not listing already-listed directory` for one it
reaches a second time, rather than following the loop until the path outgrows
the kernel's bound.

With `--color`, a link takes the shared scheme's link role (bold cyan, GNU's
`ln=`) and the long format paints the `-> target` text in the role of what
the target *is*; a target that cannot be reached is left plain, because the
scheme names no orphan-link role and inventing a second colour vocabulary
would be worse than an uncoloured target.

### A render machine, not a data source

`run` asks the injected filesystem seam for the metadata of each operand
and the entries of each directory, then writes the sorted, formatted
listing to the terminal in a single write. The operations that reach the
outside world are injected seams, the same discipline as `cat`'s
`FileSource`/`Output` and `man`'s `BundleStore`:

- `Listing` — stat a path in the `stat` or `lstat` reading the posture
  selects *per path* (`FinalLink`), read a symbolic link's stored target,
  and read a directory's whole listing in one call, mirroring the kernel's
  one-shot `fs_readdir` contract. An entry's kind is the VFS's own
  `FileKind` — no parallel kind enum to drift. The per-entry stat behind
  the long format's columns, the `-S` size sort, `-F`'s execute-bit check,
  and `-L`'s resolution is paid only when one of them asks for it, and a
  link's target is read only by the format that prints it.
- `Output` — write the rendered listing to the terminal, a skipped path's
  reason to the error stream, and advisory records to the standard
  information stream (fd 3), best-effort.
- `tairix_help::HelpSource` — the tool's own `Help/` tree, read by the
  short-help switches.

On a running system these are syscall-backed (`fs_open`/`fs_stat`/
`fs_readdir` and the inherited standard streams); in tests they are
in-memory fixtures, so every parsing, filtering, sorting, and formatting
decision is testable without a kernel.

### Layout

When several operands are given, non-directory operands are listed first
(sorted by name), then each directory operand has its entries listed,
preceded by a `path:` header and separated from the previous block by a
blank line — the POSIX model. A single directory operand is listed
without a header; under `-R` every directory block is headered and
subdirectories follow depth-first in rendered order. When output is a
terminal the short format lays entries in columns sized to the attested
terminal width, filled top-to-bottom (`-C`, the default); when output
is a pipe or a file it prints one name per line. `-x` fills the columns
left-to-right, `-m` joins the names with `, ` wrapped to the width, `-1`
forces one per line, and `-w`/`--width` sets the width explicitly (the
attested width otherwise, or 80 when it cannot be determined — the
width is read only from the kernel's fail-closed geometry attestation,
never guessed). Cell widths are measured through the shared
`tairix_vt::str_width` table, so a double-width glyph never shifts a
column. The long format
prints the ten-character mode string (`d` for a directory, `-`
otherwise, followed by the nine `rwx` permission bits), the numeric
owner and group (omitted under `-g` / `-o`; account-name resolution
would demand the capability-gated user database, so the GNU numeric
fallback is the output), an optional `--author` column (the owning user
repeated, since there is no separate author), the size right-aligned
across the block, a timestamp, then the name. `-G` / `--no-group` drops
the group column without selecting the long format (unlike `-o`). The
timestamp is the
modified time by default; `-c`, `-u`, and `--time` choose which of the
four `NodeTimes` stamps is shown (and, with `-t`, sorted by), and
`--time-style` / `--full-time` set its rendering — `locale` (the GNU
default: a time-of-day for a stamp within the last six months, a year
otherwise, decided against the wall clock), `long-iso`, `full-iso`, or
`iso`; a custom `+FORMAT` is refused (fail closed) until a shared
`strftime` engine lands with `date`. The stamp is decomposed through the
one shared civil-date breakdown (`tairix_fsmeta::calendar::CivilTime`).

Names are quoted through a faithful port of GNU `quotearg`, so awkward
characters are visible and safe to paste back into a shell. `-N`
(`--quoting-style=literal`) prints names verbatim, `-Q`
(`--quoting-style=c`) uses C string quoting, `-b`
(`--quoting-style=escape`) is the same without the surrounding quotes
(spaces escaped), and `--quoting-style=WORD` selects the full GNU set —
`literal`, `shell`, `shell-always`, `shell-escape`,
`shell-escape-always`, `c`, or `escape`. The `shell` family single-quotes
a name only when a shell metacharacter or a space forces it (falling back
to C double quotes when only a single quote is awkward), and the
`shell-escape` variants splice nongraphic characters in as `$'…'` ANSI-C
escapes. The locale-dependent `locale` and `clocale` styles are refused
(fail closed) — the same stance the tool takes on a custom
`--time-style=+FORMAT` — since TAIRiX has no locale-quotation-mark
infrastructure. The default is resolved against the attested console:
`shell-escape` at a terminal (with control characters hidden, `-q`) and
`literal` otherwise (control characters shown, `--show-control-chars`);
`-q` / `--show-control-chars` toggle whether the non-escaping styles show
nongraphic characters as `?`. `-p` / `-F` / `--file-type` /
`--indicator-style` append the indicator suffix after any closing quote
(with the VFS's two kinds only directories take `/`; `-F` also stars an
executable, `--file-type` never does).

Sizes follow GNU's two independent scalings — one for the long-format
file-size column, one for the `-s` allocation cells and the `total`
line. `-h` (base 1024) and `--si` (base 1000) autoscale both, rounding
up with the GNU letters (`K`/`M`/… vs the lowercase `k` kilo);
`--block-size=SIZE` scales both by SIZE (a plain integer of bytes, or a
`K`/`KiB`/`KB`/`M`/… unit, optionally with an integer coefficient — a
bare unit prints its suffix, a coefficient suppresses it, and a
malformed SIZE fails closed); `-k` / `--kibibytes` forces 1024-byte
blocks for `-s`/`total` only (already the default — TAIRiX has no
`BLOCK_SIZE` environment — so it confirms rather than changes, and any
size option overrides it). The `total` scales the *summed* allocation,
not the sum of the rounded per-entry cells, matching GNU. `-l` and
`-1`/`-C`/`-x`/`-m` (and the `--format=WORD` words) are one last-wins
arrangement state, with GNU's rule that `-1` has no effect after `-l`.
`-T` / `--tabsize` sets the column tab stop (default 8; `0` pads with
spaces only), advancing between columns with tabs by a faithful port of
GNU's `indent`. `--zero` ends every entry line and the `total` with NUL
instead of a newline (headers and the inter-block separator keep the
newline), and defaults to a single column, literal quoting, and shown
control characters.

At a colour terminal names are coloured by kind through the standard
`tairix_vt::scheme` palette — directories in the directory role
(bold blue), executable regular files in the executable role (bold
green), plain files uncoloured. `--color[=WHEN]` chooses when: `auto`
(the default) colours only when standard output is a kernel-attested
terminal, `always` colours even an unattested console (a serial or
remote session) at a 16-colour floor, and `never` is plain; a bare
`--color` means `always`. The depth is resolved from `TERM` through
`lib/termcap`'s shared `resolve_color`, and a scheme colour the terminal
cannot show is degraded through `lib/curses`'s one `downgrade`. Colour is
presentation only: it wraps just the name text (the indicator suffix
stays uncoloured), all column widths are computed on the *plain* text, so
a piped or redirected listing — which fails the attestation and renders
plain under `auto` — is byte-for-byte identical to the coloured one apart
from the SGR escape sequences, never shifting a column, the ordering, or
the exit status. When colour is active every entry is stat'd (the kind
and execute bit decide its colour), exactly as the GNU tool does.

The **link-count column** sits between the mode string and the owner, as
in the GNU tool, and reports the count the filesystem records — never one
derived here. A row whose stat was refused renders it `?` like every
other stat-derived cell. `-i` prefixes each entry (short and long) with
its stable node number, right-aligned.

Entries can be filtered out by name, following GNU's `file_ignored`
order. `-B` / `--ignore-backups` drops names ending in `~` in every
dotfile mode (a direct suffix test — a backup is hidden even under `-a`,
and no leading-`.` glob special case is needed). `-I` /
`--ignore=PATTERN` (repeatable) drops names matching the glob in every
mode, while `--hide=PATTERN` (repeatable) drops matches only when neither
`-a` nor `-A` is given — an explicit "show hidden" wins over `--hide`, the
GNU rule. Both patterns compile through the shared `lib/glob` matcher (in
which `*` / `?` also match a leading `.`, a documented divergence from
GNU's `fnmatch(FNM_PERIOD)`); a malformed pattern is a `LsError::Usage`
error, never a filter that silently matches nothing. These filters are
applied silently — unlike the default dotfile filter, an explicitly
requested one emits no omission record.

### Advisory output (`stdinfo`, fd 3)

When the default dotfile filter hides entries, `ls` emits the canonical
`fs.hidden_entries_omitted` omission record (`AGENTS.md` §20.1) on the
standard information stream: a terse human note ("4 hidden files not
shown." with the `ls -a` suggestion) plus structured data for tools
(`omitted_count`, `stdout_is_exhaustive`). It is advisory only — emitted
best-effort after the listing, never affecting output, ordering, or exit
status — and nothing is emitted under `-a` or when nothing was hidden.

### Fail closed

- An unrecognised option is a `LsError::Usage` that inspects nothing.
- An operand (or a directory entry, when a per-entry stat is needed)
  that cannot be stat'd surfaces the underlying `Errno` as
  `LsError::Stat` and stops before any later operand (a missing operand
  among several aborts rather than skipping silently).
- A directory that cannot be read is `LsError::Read`; a directory
  stream carrying a non-UTF-8 name (an ABI-contract violation) is
  refused whole rather than silently thinned.
- A failed terminal write is `LsError::Output`.
- A missing own-help tree degrades `-?` to the usage banner — never a
  fabricated page, never a failure.
- Recursion never follows `.` or `..`, so a listing always terminates.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-ls` drives the parser and the listing engine
against an in-memory tree, an in-memory help tree, and a recording
output: the command grammar (every option, clustered short flags,
`-`/`-?`/`--`, the `-h`-is-human-readable rule, the retired `--long`
spelling, and the usage-error path), sorted directory listing, the
hidden-file filter with and without `-a`/`-A` (including the advisory
record's content, its singular/plural message, the across-directories
count, and its absence when nothing was hidden), a non-directory
operand, the long format's mode string, owner/group columns (and their
`-g`/`-o` omission), the `--author` column, right-aligned plain and
human-readable sizes, the `--si` / `--block-size` scalings and the
`total`-scales-the-sum rule, the long-format date column, the `-t` time
sort (newest first, ties by name)
and its reverse, the `-c`/`-u`/`--time` time-field selection, the
`locale`/`long-iso`/`full-iso`/`iso` `--time-style` renders (recent vs
old), the `-i` node-number column, the `-B`/`-I`/`--hide` name filters
(backups dropped in every mode, `-I` applied under `-a`, `--hide`
yielding to `-a`, and a malformed pattern as a usage error),
the per-entry stat under a slash-terminated operand, single- and
multi-operand layout (files first, then directory headers), recursive
depth-first traversal with headers, reverse and size sorts, the comma
arrangement, every quoting style (literal, C, escape, and the shell
family incl. `$'…'` control-char splicing) with the tty-resolved default
and `-q` masking, the `/` and `*` indicators and `--file-type` (dir
only, never the executable star), the tab-vs-space column padding under
`-T8`/`-T0`, the `--zero` NUL terminator, the `--color` colouring by kind
(`always`/`never`/`auto` gated on the attestation and `TERM`, the name
coloured but not the indicator suffix, and the coloured grid stripped of
its SGR being byte-identical to the plain grid), the
human-size rounding table, an empty directory, the short-help render and
its usage-banner fallback, and the missing-operand, unreadable-directory,
and dead-console fail-closed paths. `ls`'s help is authored on disk in
the bundle's own `Help/` tree and read at runtime through the injected
seam — never embedded in the binary — and a crate test proves every
locale's document records exactly the parser's switches; the
`tairix-syshelp` discovery crate's tests prove every shipped locale
parses under the engine's bounds and the required locale set is complete.
The aarch64 session-ceiling QEMU vertical types
`ls /System/Commands` in a real session and sees `man.app` in the listing —
a store read only the mounted read-only `/System` volume produces.

## `rm` — remove files and directories (`userland/apps/rm`)

`tairix-rm` removes its operands in order (`AGENTS.md` §3). A
non-directory operand — a regular file, a symbolic link (removed, never
followed), a device node — is unlinked. A directory operand is removed
only with `-r`, which removes its contents depth-first and then the
directory itself; naming a directory without `-r` is an error. With `-f`
an operand that does not exist is skipped rather than reported. This is
the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
rm [-dfiIrRv] [--] file...
```

| Token                   | Meaning                                            |
|-------------------------|----------------------------------------------------|
| `-r`, `-R`, `--recursive` | remove directories and their contents            |
| `-f`, `--force`         | ignore operands that do not exist; never prompt    |
| `-d`, `--dir`           | remove empty directories without `-r`              |
| `-i`, `--interactive`   | prompt before every removal                        |
| `-I`                    | prompt once before removing more than three operands, or before a recursive removal |
| `-v`, `--verbose`       | report each removal                                |
| `--preserve-root`       | refuse to remove `/` (the default)                 |
| `--no-preserve-root`    | allow removing `/`                                 |
| `-h`, `--help`          | print the usage banner (wins immediately)          |
| `--`                    | end option parsing; every later argument is a path |
| *file*                  | a file or directory to remove                      |

At least one file operand is required unless `-f` is given (an empty
`rm -f` removes nothing and succeeds). Short options may be combined into
one argument (e.g. `-rf` is `-r -f`); an unrecognised letter anywhere in
such a cluster is a `RmError::Usage` error. The bare `-` is a path named
`-`, not an option. As in the GNU tool, the later of `-f` / `-i` / `-I`
wins: `-f` cancels prompting and a prompt flag cancels `-f`.

### A removal machine, not a data source

`run` asks the injected filesystem seam what each operand is, walks each
directory `-r` must remove, and unlinks every reachable object. The
operations that reach the outside world are injected seams, the same
discipline as `ls`'s `Listing`/`Output`:

- `Removal` — learn a path's kind, read a directory's entries by index,
  and remove a file or an emptied directory.
- `Prompt` — ask the `-i`/`-I` confirmation questions; a declined
  question skips the object (or the whole run for `-I`) without error,
  and an unanswerable one fails closed — never treated as consent.
- `Output` — write the usage banner and the `-v` `removed '…'` /
  `removed directory '…'` reports to the terminal (`rm` is otherwise
  silent on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, recursion, prompting, and
force decision is testable without a kernel. `--preserve-root` (the
default) refuses the operand `/` outright; `-d` removes an empty
directory without `-r`, surfacing the filesystem's own refusal of a full
one.

### Recursion order

`rm -r` removes a directory's contents depth-first — files and nested
directories before the directory that holds them — so a parent is never
unlinked while it still has children. Entries are read into a list before
removal begins, so the walk does not depend on directory indices staying
stable as objects disappear.

### Fail closed

- An unknown option, or no operand without `-f`, is a `RmError::Usage`
  that removes nothing.
- A directory named without `-r` is a `RmError::IsDirectory`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `RmError::Stat`, and the run stops before any later operand. `-f` makes
  a `NotFound` a silent no-op, but still surfaces any other errno (e.g.
  `PermissionDenied`).
- A directory that cannot be enumerated is `RmError::Read`.
- A failed unlink is `RmError::Remove`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-rm` drives the parser and the removal engine
against an in-memory tree and a recording output: the command grammar
(every option, clustered short flags, `-`/`--`, the no-operand and
usage-error paths), a single file, several files in order, the
directory-without-`-r` refusal, recursive depth-first removal of a nested
tree (asserting contents are unlinked before their directory), an empty
directory, the missing-operand fail-closed path and the `-f` skip, the
`-f`-does-not-mask-permission guarantee, a failure stopping before a
later operand, the unreadable-directory and failed-unlink paths, and the
trailing-slash path join.

## `cp` — copy files and directories (`userland/apps/cp`)

`tairix-cp` copies each of its source operands to a destination
(`AGENTS.md` §3). With a single source and a destination that is not a
directory, the source is copied to that exact path. When the destination
is an existing directory — and always when there is more than one source
— each source is copied *into* it under its own base name. A directory
source is copied only with `-r`, which reproduces the whole subtree;
naming a directory without `-r` is an error. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
cp [-finrRvT] [-t dir] [--] source... dest
```

| Token                     | Meaning                                            |
|---------------------------|----------------------------------------------------|
| `-r`, `-R`, `--recursive` | copy directories and their contents                |
| `-f`, `--force`           | remove an unwritable destination and retry         |
| `-i`, `--interactive`     | ask before overwriting an existing file            |
| `-n`, `--no-clobber`      | never overwrite an existing file                   |
| `-v`, `--verbose`         | report each copy                                   |
| `-t dir`, `--target-directory=dir` | copy every source into `dir`              |
| `-T`, `--no-target-directory` | treat dest as a normal file (one source)      |
| `-h`, `--help`            | print the usage banner (wins immediately)          |
| `--`                      | end option parsing; every later argument is a path |
| *source*                  | a file or directory to copy                        |
| *dest*                    | the destination path (the last operand)            |

At least one source and a destination are required (fewer than two path
operands is a `CpError::Usage`). Without `-t` the last path operand is
the destination and the rest are the sources; with `-t` every operand is
a source and the `-t` directory must exist. With more than one source
the destination must be a directory (`-T` refuses more than one source).
Short options may be combined into one argument (e.g. `-rf` is `-r -f`);
an unrecognised letter anywhere in such a cluster is a `CpError::Usage`.
The bare `-` is a path named `-`, not an option. As in the GNU tool, the
later of `-i` / `-n` wins, `-t` takes its directory attached (`-tdir`)
or as the next argument, and `-t` with `-T` is a usage error.

### A copy machine, not a data source

`run` asks the injected filesystem seam what each source is, streams a
regular file's bytes from source to destination, and walks each directory
`-r` must reproduce. The operations that reach the outside world are
injected seams, the same discipline as `rm`'s `Removal`/`Output`:

- `FileSystem` — learn a path's kind, read a file's bytes and a
  directory's entries, and create directories, files, and bytes (plus
  remove a destination file for `-f`).
- `Prompt` — ask the `-i` overwrite question; a declined question skips
  that copy without error, and an unanswerable one fails closed — never
  treated as consent.
- `Output` — write the usage banner and the `-v` `'src' -> 'dst'`
  reports to the terminal (`cp` is otherwise silent on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, recursion, clobber, and force
decision is testable without a kernel. `-n` silently skips an existing
destination file (a new one still copies).

### Streaming and recursion

A regular file is streamed in fixed-size chunks (matching `cat`'s
granularity), so an arbitrarily large file copies with a bounded buffer.
A directory is reproduced by creating its destination — or merging into
an existing one — then copying each entry, files and nested directories
alike, under the destination. Entries are read into a list before the
copy descends, so the walk does not depend on directory indices staying
stable. The destination directory is created before its contents, so a
parent always exists before a child is copied into it.

### Force

`-f` covers a destination that cannot be created (for example, an
existing read-only file): the destination is removed and the create is
retried exactly once. Without `-f`, a create failure surfaces as a
`CpError::Create` and stops the run.

### Fail closed

- An unknown option, fewer than two operands, or more than one source
  aimed at a non-directory destination is a `CpError::Usage` that copies
  nothing.
- A directory source named without `-r` is a `CpError::IsDirectory`; a
  directory source whose destination already exists as a non-directory is
  a `CpError::NotADirectory`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `CpError::Stat`, and the run stops before any later operand.
- An unreadable source is `CpError::Read`; an uncreatable destination is
  `CpError::Create`; a failed write is `CpError::Write`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-cp` drives the parser and the copy engine against
an in-memory tree and a recording output: the command grammar (every
option, clustered short flags, `-`/`--`, the too-few-operands and
unknown-option paths), a single file to a new path, a file copied across
the streaming-chunk boundary, an empty file, a file copied into a
directory under its base name, several files into a directory, the
several-sources-to-a-non-directory `Usage` refusal, the
directory-without-`-r` refusal, recursive reproduction of a nested tree,
a recursive merge into an existing directory, the recursive-onto-a-file
refusal, the missing-source fail-closed path, a failure stopping before a
later source, the unreadable-source / uncreatable-destination /
failed-write paths, the `-f` remove-and-retry recovery, and the
trailing-slash base-name join.

## `mv` — move (rename) files and directories (`userland/apps/mv`)

`tairix-mv` relocates each of its source operands to a destination
(`AGENTS.md` §3). With a single source and a destination that is not a
directory, the source is moved to that exact path. When the destination
is an existing directory — and always when there is more than one
source — each source is moved *into* it under its base name. Unlike
`cp`, a directory needs no flag: a directory is moved like any other
operand. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
mv [-finvT] [-t dir] [--] source... dest

  -f, --force                remove a blocking destination and retry the
                             rename; never prompt
  -i, --interactive          ask before overwriting an existing destination
  -n, --no-clobber           never overwrite an existing destination
  -v, --verbose              report each move (renamed 'src' -> 'dst')
  -t dir, --target-directory=dir
                             move every source into dir
  -T, --no-target-directory  treat dest as a normal file (one source)
  -h, --help                 show the usage banner
```

At least one source and a destination are required. Short options may be
combined (e.g. `-fn`). `--` ends option parsing: every later argument is
a path. With more than one source the destination must be a directory
(`-T` refuses more than one source; `-t`'s directory must exist).
`-h`/`--help` wins immediately. As in the GNU tool, the last of `-f` /
`-i` / `-n` wins; `-i` asks through the injected `Prompt` seam before
replacing an existing destination — a declined question skips that move
without error and an unanswerable one fails closed, never treated as
consent.

### A move machine, not a data source

`run` asks the injected filesystem seam what each source is, then asks it
to `rename` the source onto its destination. A rename within one
filesystem is atomic and is the whole operation. The operations that
reach the outside world are injected seams, mirroring the other userland
crates (`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s
`FileSystem`):

- `FileSystem` — learn a path's kind, rename a path, read a file's bytes
  and a directory's entries, create directories/files/bytes, and remove
  files and directories (for the cross-device relocation and for `-f`).
- `Output` — write the usage banner to the terminal (`mv` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every routing and fallback decision is
testable without a kernel.

### Cross-device relocation

A rename cannot be atomic when its source and destination live on
different filesystems. Rather than overload an `Errno`, the `rename` seam
reports that case as an explicit `RenameOutcome::CrossDevice` outcome
(`AGENTS.md` §2.11). The engine then performs the POSIX relocation: it
copies the source to the destination — streaming a regular file in
fixed-size chunks (matching `cat`'s and `cp`'s granularity) and
reproducing a directory subtree depth-first — and only then removes the
source, depth-first, so a directory is unlinked after its contents. A
failure during the copy leaves the source in place.

### No-clobber and force

`-n` never overwrites: a source whose destination already exists is
skipped silently. `-f` covers a destination that blocks the rename (for
example, an existing read-only file): the destination is removed and the
rename is retried exactly once. Without either flag an existing
destination is overwritten, the default POSIX behaviour.

### Fail closed

- An unknown option, fewer than two operands, or more than one source
  aimed at a non-directory destination is an `MvError::Usage` that moves
  nothing.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `MvError::Stat`, and the run stops before any later operand.
- A rename that fails for a reason other than crossing a filesystem
  boundary is `MvError::Rename`.
- During a cross-device relocation an unreadable source is
  `MvError::Read`, an uncreatable destination is `MvError::Create`, a
  failed write is `MvError::Write`, and a source that cannot be removed
  after a successful copy is `MvError::Remove`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-mv` drives the parser and the move engine against
an in-memory tree and a recording output: the command grammar (every
option, clustered short flags, `-`/`--`, the too-few-operands and
unknown-option paths), a file renamed to a new path, a directory renamed,
a file moved into a directory under its base name, several files into a
directory, the several-sources-to-a-non-directory `Usage` refusal, the
missing-source fail-closed path, a failure stopping before a later
source, `-n` skipping an existing destination, the default overwrite, the
failed-rename path, the blocking-destination refusal and its `-f`
remove-and-retry recovery, the cross-device file / large-file /
directory relocations, the cross-device read/write/remove fail-closed
paths, and the trailing-slash base-name join.

## `chmod` — change file mode bits (`userland/apps/chmod`)

`tairix-chmod` applies a mode to each of its file operands (`AGENTS.md`
§3). The mode is either an absolute octal value (`644`, `0755`, …) that
replaces the permission bits outright, or a comma-separated list of
symbolic clauses (`[ugoa]*[-+=][rwxXst]*`, e.g. `g+w`, `o-rx`, `a=rx`,
`u+s`) that transform the file's current bits. With `-R` a directory
operand is changed and then its contents are changed recursively. This is
the POSIX model, and it is a building block of the §5.3 filesystem
permission model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependencies are the audited `tairix-abi` crate and the shared
`lib/help` engine, so it never links a kernel or driver crate
(`AGENTS.md` §17.4). The `Run` binary (`src/run.rs`, the store bundle's
entry point) wires the production seams: a resolve-only `fs_open` +
`fs_stat` learns each operand's kind and current bits, `fs_set_mode`
(syscall 74) applies the change — the kernel enforces the owner-only
rule, the mount flags, and every per-inode check — and the one shared
grow-on-`BufferTooSmall` `fs_readdir` walk feeds `-R`.

### Grammar

```
chmod [-cfRv] [--] MODE file...

  -R, --recursive       change files and directories recursively
  -c, --changes         report only files whose mode actually changed
  -v, --verbose         report every file processed
  -f, --silent, --quiet suppress most error messages
  -h, -?, --help        show this command's own short help
```

A mode and at least one file are required. `--` ends option parsing:
every later argument is an operand. POSIX `chmod` spells recursive `-R`;
a bare `-r` is not an option. To set a mode that begins with `-`, write
it without the dash (`a-w`) or end option parsing first
(`chmod -- -w file`). `-h`/`-?`/`--help` wins immediately. The later of `-c`
/ `-v` wins; the reports use the GNU wording (`mode of 'f' changed from
0644 (rw-r--r--) to 0664 (rw-rw-r--)`, `mode of 'f' retained as …`).
`-f` suppresses each failing operand's diagnostic and keeps going, then
fails the whole run with the message-less `ChmodError::Silenced` — the
exit status still reflects the failure.

### The mode grammar

- **Octal**: one to four octal digits set the low twelve permission bits
  (the `rwx` triples plus the setuid/setgid/sticky bits) outright; the
  current mode is irrelevant.
- **Symbolic**: comma-separated clauses, each `[ugoa]*[-+=][rwxXst]*`.
  `u`/`g`/`o` select the owner/group/other field and `a` (or an omitted
  who) selects all. `+` turns the bits on, `-` off, and `=` sets the
  selected fields to exactly those bits. Permissions are `r`, `w`, `x`,
  `X` (execute only for a directory or a file that already carries an
  execute bit), `s` (setuid/setgid), and `t` (sticky). A clause may chain
  several operator sections that share its who (e.g. `u+x-w`). An omitted
  who is treated as `a` (TAIRiX has no per-process umask seam to honour,
  so the `a` interpretation is exact, not umask-masked).

### A mode-changing machine, not a data source

`run` asks the injected filesystem seam for each operand's kind and
current mode, computes the new mode, applies it, and walks each directory
`-R` must descend (changing the directory before its contents). The
operations that reach the outside world are injected seams, mirroring the
other userland crates (`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s
`Removal`, `cp`'s and `mv`'s `FileSystem`):

- `FileSystem` — learn a path's kind and current mode, set its mode, and
  read a directory's entries (for `-R`).
- `Output` — write the short help and the `-v`/`-c` reports to the
  terminal (`chmod` is otherwise silent on success).
- `HelpSource` (from `lib/help`) — the bundle's own `Help/` tree, rendered
  by the `-h`/`-?`/`--help` switches through the one shared engine; the
  usage banner is only the fallback when no document can be served.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, mode-algebra, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `ChmodError::Usage` that
  changes nothing.
- A mode operand that is neither octal nor symbolic is a
  `ChmodError::BadMode`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `ChmodError::Stat`, and the run stops before any later operand.
- A mode that cannot be applied is `ChmodError::Apply`; a directory whose
  entries cannot be read during a recursive descent is `ChmodError::Read`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-chmod` drives the parser, the symbolic-mode
algebra, and the move engine against an in-memory tree and a recording
output: the command grammar (octal and symbolic modes, the recursive
flag, the `-r`-is-not-recursive and unknown-option refusals, `--`,
too-few-operands and bad-mode paths), the full mode algebra (`+`/`-`/`=`,
omitted-who, conditional `X`, setuid/setgid/sticky, left-to-right clause
application, empty-perm no-ops), an octal change, a symbolic change,
several files, a non-recursive directory change leaving its contents
alone, a recursive change touching the directory before its contents,
per-node `X` resolution under recursion, the missing-operand / stat /
apply / read-during-recursion fail-closed paths, and the short-help
switches (the rendered own-document path and the usage-banner fallback).

## `chown` — change file owner and group (`userland/apps/chown`)

`tairix-chown` applies an ownership change to each of its file operands
(`AGENTS.md` §3). The owner operand is `OWNER`, `OWNER:GROUP`, or
`:GROUP`, where `OWNER` and `GROUP` are **decimal** user/group ids:
`OWNER` changes only the owning user, `:GROUP` only the owning group, and
`OWNER:GROUP` both. With `-R` a directory operand is changed and then its
contents are changed recursively. This is the POSIX model, restricted to
numeric ids, and it is a building block of the §5.3 filesystem permission
model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
chown [-cfRv] [--] OWNER[:GROUP] file...

  -R, --recursive       change files and directories recursively
  -c, --changes         report only files whose ownership actually changed
  -v, --verbose         report every file processed
  -f, --silent, --quiet suppress most error messages
  -h, --help            show the usage banner
```

An owner spec and at least one file are required. `--` ends option
parsing: every later argument is an operand. POSIX `chown` spells
recursive `-R`; a bare `-r` is not an option. `-h`/`--help` wins
immediately. The later of `-c` / `-v` wins; the reports use the GNU
wording shaped by the owner spec (`changed ownership of 'f' from
1000:100 to 0:0`, `changed group of 'f' from …`, `… retained as …`),
reading each node's current owner through the seam's `Metadata` stat.
`-f` suppresses each failing operand's diagnostic and keeps going, then
fails the whole run with the message-less `ChownError::Silenced` — the
exit status still reflects the failure.

### The owner grammar

`OWNER` and `GROUP` are decimal ids, in one of three forms:

- `OWNER` — change only the owning user, leaving the group.
- `OWNER:GROUP` — change both.
- `:GROUP` — change only the owning group.

A name (rather than a numeric id) is not accepted: TAIRiX has no
name-to-id seam in this tool, so resolving names would be interface creep
(`AGENTS.md` §2.4). An empty spec, a bare `:`, and a trailing-colon
`OWNER:` (which on POSIX systems means "the user's login group", and has
no meaning without a name database) are all rejected rather than guessed
(`AGENTS.md` §2.1).

### An ownership-changing machine, not a data source

`run` asks the injected filesystem seam for each operand's kind, applies
the new owner, and walks each directory `-R` must descend (changing the
directory before its contents, and reusing the kind carried in each
directory entry so it re-inspects nothing). The operations that reach the
outside world are injected seams, mirroring the other userland crates
(`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s and
`mv`'s `FileSystem`, `chmod`'s `FileSystem`):

- `FileSystem` — learn a path's kind, set its owner, and read a
  directory's entries (for `-R`).
- `Output` — write the usage banner to the terminal (`chown` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, owner-spec, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `ChownError::Usage` that
  changes nothing.
- An owner operand that is not a valid `OWNER`/`OWNER:GROUP`/`:GROUP`
  spec is a `ChownError::BadOwner`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `ChownError::Stat`, and the run stops before any later operand.
- An owner that cannot be applied is `ChownError::Apply`; a directory
  whose entries cannot be read during a recursive descent is
  `ChownError::Read`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-chown` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar (every owner
form, the recursive flag, the `-r`-is-not-recursive and unknown-option
refusals, `--`, the too-few-operands and bad-owner paths), the owner-spec
parser (the three valid forms, the empty/`:`/trailing-colon refusals, and
the non-decimal / overflow / multi-colon refusals), an owner-only change
leaving the group, an owner:group change, a group-only change leaving the
user, several files, a non-recursive directory change leaving its
contents alone, a recursive change touching the directory before its
contents, and the missing-operand / stat / apply / read-during-recursion
fail-closed paths.

## `getcap` — report a file's capability gate (`userland/apps/getcap`)

`tairix-getcap` reports the **optional capability requirement** an inode
may carry: a capability the caller must hold to reach the node at all, on
top of the mode/ACL checks (`AGENTS.md` §5.3). For each file operand it
prints one line — `path CAP_NAME` — when the file carries a gate, and
prints nothing for a file that has none, so a clean tree is silent. With
`-R` a directory operand is reported and then its contents recursively.
It is the read-only companion of [`setcap`](#setcap--set-or-clear-a-files-capability-gate-userlandappssetcap).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
getcap [-R] [--] file...

  -R, --recursive  report files and directories recursively
  -h, --help       show the usage banner
```

At least one file is required. `--` ends option parsing: every later
argument is an operand. `getcap` spells recursive `-R`; a bare `-r` is
not an option. `-h`/`--help` wins immediately.

### Capability names

A gate renders by its canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`),
resolved through `tairix_abi::CapabilityId::name` — the single,
frozen `abi-v1` source of truth shared with `setcap` (`AGENTS.md` §2.2,
§5.2). A node that stored an in-range identifier the running ABI has not
yet named renders as `CAP_<id>` rather than being silently dropped, so a
gate is never hidden (`AGENTS.md` §2.1).

### A reporter, not a policy point

`run` asks the injected filesystem seam for each operand's kind and
capability gate, renders the gated files, and walks each directory `-R`
must descend (reporting the directory before its contents). The driver
only *reports* the stored gate; `getcap` makes no permission decision
(`AGENTS.md` §5.4 — the VFS is the policy point). The operations that
reach the outside world are injected seams, mirroring the other userland
crates:

- `FileSystem` — learn a path's kind, read its capability gate, and read
  a directory's entries (for `-R`).
- `Output` — write the report and the usage banner to the terminal.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, rendering, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `GetcapError::Usage` that
  reports nothing.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `GetcapError::Stat`, and the run stops before any later operand.
- A gate that cannot be read is `GetcapError::Query`; a directory whose
  entries cannot be read during a recursive descent is `GetcapError::Read`;
  a failed write is `GetcapError::Output`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-getcap` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar (the recursive
flag, the `-r`-is-not-recursive and unknown-option refusals, `--`, and
the no-operand path), a gated file reported by name, an ungated file
producing no output, an unnamed in-range gate rendered numerically,
several files reporting only the gated ones in order, a non-recursive
directory report leaving its contents alone, a recursive report touching
the directory before its contents, and the missing-operand / stat / query
/ read-during-recursion fail-closed paths.

## `setcap` — set or clear a file's capability gate (`userland/apps/setcap`)

`tairix-setcap` changes the **optional capability requirement** of each
of its file operands (`AGENTS.md` §5.3). The capability operand is either
a canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`), which installs that gate,
or the literal `-`, which clears the gate so the node has none. With `-R`
a directory operand is changed and then its contents recursively. It is
the policy-*writing* companion of
[`getcap`](#getcap--report-a-files-capability-gate-userlandappsgetcap) and
a building block of the §5.3 filesystem permission model.

`setcap` stores the gate but makes no permission decision itself
(`AGENTS.md` §5.4 — the VFS is the policy point). Setting a gate is itself
a privileged operation; the filesystem seam refuses an attempt the caller
is not authorised to make (it surfaces as `SetcapError::Apply`).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
setcap [-R] [--] CAP file...

  -R, --recursive  change files and directories recursively
  -h, --help       show the usage banner
```

A capability spec and at least one file are required. `--` ends option
parsing: every later argument is an operand. `setcap` spells recursive
`-R`; a bare `-r` is not an option. `-h`/`--help` wins immediately.

### The capability grammar

The capability spec is one of:

- a canonical `CAP_*` name (`CAP_FS_MOUNT`, `CAP_AUDIT_READ`, …) — install
  that gate; the name is resolved through
  `tairix_abi::CapabilityId::from_name`, the same frozen `abi-v1` table
  `getcap` renders with (`AGENTS.md` §2.2);
- the literal `-` — clear the gate.

The name match is exact and case-sensitive (`AGENTS.md` §2.1 — no
guessing): an unknown, mis-cased, or bare-numeric value is rejected as a
`SetcapError::BadCapability` rather than coerced.

### A gate-setting machine, not a data source

`run` asks the injected filesystem seam for each operand's kind, applies
the new gate, and walks each directory `-R` must descend (changing the
directory before its contents, and reusing the kind carried in each
directory entry so it re-inspects nothing). The operations that reach the
outside world are injected seams, mirroring `chmod`'s and `chown`'s
`FileSystem`:

- `FileSystem` — learn a path's kind, set its capability gate, and read a
  directory's entries (for `-R`).
- `Output` — write the usage banner to the terminal (`setcap` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, cap-spec, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `SetcapError::Usage` that
  changes nothing.
- A capability operand that is neither a known `CAP_*` name nor `-` is a
  `SetcapError::BadCapability`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `SetcapError::Stat`, and the run stops before any later operand.
- A gate that cannot be applied is `SetcapError::Apply`; a directory whose
  entries cannot be read during a recursive descent is `SetcapError::Read`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-setcap` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar (a named
capability and the clearing `-`, the recursive flag, the
`-r`-is-not-recursive and unknown-option refusals, `--`, the
too-few-operands and bad-capability paths), the cap-spec parser (the
named and `-` forms, and the unknown / mis-cased / numeric refusals), a
named-capability install, a `-` clear, several files, a non-recursive
directory change leaving its contents alone, a recursive change touching
the directory before its contents, and the missing-operand / stat / apply
/ read-during-recursion fail-closed paths.

## `useradd` — create a user account (`userland/apps/useradd`)

`tairix-useradd` is a `plans/APPS.md` command app registered at
`/System/Commands/useradd.app/Run`. It adds a single account to the user
database that persists under `/System/Security/Users` (`AGENTS.md` §5.1,
§16). It names the new account and its numeric identity — a login name,
an optional user id (auto-allocated when omitted), a **required** primary
group id, an optional supplementary-group set, and the textual comment
and home directory — and hands that record to the database through an
injected seam. Group and user references are **decimal** ids, the same
choice `chown` makes. `-h`/`-?` render the tool's own short help from its
bundled thirteen-locale `Help/` tree through the shared `lib/help` engine
(`plans/APPS.md` §4), falling back to the usage banner when the tree is
unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary, the shared
`tairix-help` engine, and the `tairix-users` account policy, so it never
links a kernel or driver crate (`AGENTS.md` §17.4). Its manifest requests
`CAP_CONSOLE_WRITE`, `CAP_USER_ADMIN`, and `CAP_FS_ACCESS`.

### Grammar

```
useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME

  -u, --uid UID       numeric user id (auto-allocated if omitted)
  -g, --gid GID       numeric primary group id (required)
  -G, --groups LIST   comma-separated numeric supplementary group ids
  -c, --comment TEXT  account comment / full name
  -d, --home PATH     home directory
  -h, -?, --help      show this command's own short help
```

Exactly one name operand is required, and `-g` is mandatory. Each
value-taking option accepts its value attached (`-u0`, `--uid=0`) or as
the following argument (`-u 0`, `--uid 0`). `--` ends option parsing:
every later argument is an operand. `-h`/`-?`/`--help` wins immediately.

### The account grammar

`UID`, `GID`, and the `-G` list entries are decimal ids. A group name
(rather than a numeric id) is not accepted: TAIRiX has no name-to-id seam
in this tool, so resolving names would be interface creep (`AGENTS.md`
§2.4). The login name must match `[a-z_][a-z0-9_-]*` within the length
bound — the portable Unix shape, which admits no name that could be
confused for a numeric id or an option.

`-g` is required rather than defaulted: there is no default-group policy
to invent (`AGENTS.md` §2.1). A missing `-u` is allocated by the shared
`tairix_users::next_id` policy (interactive-user range, `1000..`: one
above the highest taken id in the band) and a
missing `-d` is the shared `tairix_users::default_home` layout (the §16
`/Users/<name>` shape) — both applied by the production database client,
never guessed in the parser.

### The created account has no usable password

GNU `useradd` creates an account that cannot authenticate until an
administrator sets a password. The TAIRiX database requires a well-formed
password record on creation, so the production client submits one derived
from a throwaway 256-bit random secret it immediately discards: no
password matches it, the honest equivalent of the `!` field. The
administrator then sets a real password with the `users` tool's `passwd`
command. The created account starts `tairix_users::DEFAULT_SHELL` and the
`tairix_users::SESSION_BASELINE` capability ceiling.

### An account-spec parser, not a policy point

`run` asks the injected database whether the name is already taken, then
writes the new record. Creating an account is privileged — it needs
`CAP_USER_ADMIN` (`AGENTS.md` §5.2) — but the **database** makes that
decision, not this tool (`AGENTS.md` §5.4): an unauthorised attempt is
refused by the seam and surfaced as `UseraddError::Create`. The database
is likewise the authority on uid collisions, group existence, and the
supplementary-group bound. The operations that reach the outside world
are injected seams, mirroring `setcap`'s `FileSystem`, `login`'s
`Authenticator`, and `init`'s `Spawner`/`Reaper`:

- `UserDb` — learn whether a login name is in use and create the account
  record. The production implementation, `db::UsersAdminDb`, is the
  `users_admin` client over its own injected `db::AdminChannel` (the
  syscall) and `db::Entropy` (the kernel CSPRNG through `sys:random`)
  seams, so the whole client policy is host-tested.
- `tairix_help::HelpSource` — the tool's own bundled `Help/` tree, read
  by the short-help switches.
- `Output` — write the short help to the terminal (`useradd` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing and validation decision is
testable without a kernel.

### Fail closed

- An unknown option, a missing `-g`, or anything other than exactly one
  name operand is a `UseraddError::Usage` that creates nothing.
- A login name outside `[a-z_][a-z0-9_-]*` is a `UseraddError::BadName`; a
  `-u`/`-g`/`-G` value that is not a decimal id is a `UseraddError::BadId`.
- A name already present is a `UseraddError::Exists`; a database that
  cannot be consulted surfaces the underlying `Errno` as
  `UseraddError::Lookup`, and a refused or failed creation as
  `UseraddError::Create`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-useradd` drives the parser, the engine, and the
production client against in-memory fixtures: the command grammar (the
minimal name+group form, every option, long `--opt value`/`--opt=value`
and attached short `-u0` spellings, `-h`/`-?`/`--help`, the missing-group,
wrong-operand-count, unknown-option, and missing-value usage refusals,
`--`, and the bad-id / bad-name refusals), the login-name validator
(accepted and rejected shapes, including the length bound), the
creation engine (a minimal account, every field reaching the database,
the already-exists refusal, and the lookup / create / unknown-group /
help-write fail-closed paths), the short-help render from a Help document
with its usage-banner fallback, the `users_admin` client (uid allocation
and pass-through, the shared defaults, the unusable password record
verifying against no candidate, hostile and overlong replies failing
closed, a refused entropy draw creating nothing), and the switch-drift
pin that every locale's `OPTIONS` section documents exactly the parser's
switches (`plans/APPS.md` §3.1).

## `groupadd` — create a group (`userland/apps/groupadd`)

`tairix-groupadd` is a `plans/APPS.md` command app registered at
`/System/Commands/groupadd.app/Run`. It adds a single group to the group
database that persists under `/System/Security/Groups` (`AGENTS.md` §5.1,
§16). It names the new group and an optional numeric id (auto-allocated
when omitted), and hands that record to the database through an injected
seam. The group id is a **decimal** value, the same choice `chown` and
`useradd` make. It is the natural sibling of `useradd`: the same
parser/seam/error discipline, narrowed to the two fields a group record
carries. `-h`/`-?` render the tool's own short help from its bundled
thirteen-locale `Help/` tree through the shared `lib/help` engine
(`plans/APPS.md` §4), falling back to the usage banner when the tree is
unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary, the shared
`tairix-help` engine, and the `tairix-users` account policy, so it never
links a kernel or driver crate (`AGENTS.md` §17.4). Its manifest requests
`CAP_CONSOLE_WRITE`, `CAP_USER_ADMIN`, and `CAP_FS_ACCESS`.

### Grammar

```
groupadd [-g GID] [--] NAME

  -g, --gid GID   numeric group id (auto-allocated if omitted)
  -h, -?, --help  show this command's own short help
```

Exactly one name operand is required. `-g` accepts its value attached
(`-g0`, `--gid=0`) or as the following argument (`-g 0`). `--` ends
option parsing: every later argument is an operand. `-h`/`-?`/`--help`
wins immediately.

### The group grammar

`GID` is a decimal id. A name (rather than a numeric id) is not accepted:
TAIRiX has no name-to-id seam in this tool, so resolving names would be
interface creep (`AGENTS.md` §2.4). The group name must match
`[a-z_][a-z0-9_-]*` within the length bound — the portable Unix shape,
which admits no name that could be confused for a numeric id or an
option.

A missing `-g` is allocated by the shared `tairix_users::next_id` policy
(interactive-user range, `1000..`: one above the highest taken id in the
band) in the production database client, never guessed in the parser
(`AGENTS.md` §2.1).

### A group-spec parser, not a policy point

`run` asks the injected database whether the name is already taken, then
writes the new record. Creating a group is privileged — it needs
`CAP_USER_ADMIN` (`AGENTS.md` §5.2) — but the **database** makes that
decision, not this tool (`AGENTS.md` §5.4): an unauthorised attempt is
refused by the seam and surfaced as `GroupaddError::Create`. The database
is likewise the authority on gid collisions. The operations that reach
the outside world are injected seams, mirroring `useradd`'s `UserDb`,
`setcap`'s `FileSystem`, `login`'s `Authenticator`, and `init`'s
`Spawner`/`Reaper`:

- `GroupDb` — learn whether a group name is in use and create the group
  record. The production implementation, `db::GroupsAdminDb`, is the
  `users_admin` client over its injected `db::AdminChannel` transport,
  so the whole client policy is host-tested.
- `tairix_help::HelpSource` — the tool's own bundled `Help/` tree, read
  by the short-help switches.
- `Output` — write the short help to the terminal (`groupadd` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing and validation decision is
testable without a kernel.

### Fail closed

- An unknown option or anything other than exactly one name operand is a
  `GroupaddError::Usage` that creates nothing.
- A group name outside `[a-z_][a-z0-9_-]*` is a `GroupaddError::BadName`;
  a `-g` value that is not a decimal id is a `GroupaddError::BadId`.
- A name already present is a `GroupaddError::Exists`; a database that
  cannot be consulted surfaces the underlying `Errno` as
  `GroupaddError::Lookup`, and a refused or failed creation as
  `GroupaddError::Create`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p tairix-groupadd` drives the parser, the engine, and the
production client against in-memory fixtures: the command grammar (the
bare-name and name+gid forms, long `--gid value`/`--gid=value` and
attached short `-g0` spellings, `-h`/`-?`/`--help`, the
wrong-operand-count, unknown-option, and missing-value usage refusals,
`--`, and the bad-id / bad-name refusals), the group-name validator
(accepted and rejected shapes, including the length bound), the creation
engine (a minimal group, a requested gid reaching the database, the
already-exists refusal, and the lookup / create / taken-gid / help-write
fail-closed paths), the short-help render from a Help document with its
usage-banner fallback, the `users_admin` client (gid allocation and
pass-through, hostile and overlong replies failing closed), and the
switch-drift pin that every locale's `OPTIONS` section documents exactly
the parser's switches (`plans/APPS.md` §3.1).

## `users` — interactive account administration (`userland/shell/users`)

`tairix-users-cli` (`/System/Commands/users.app/Run`) is the first holder of
the `CAP_USER_ADMIN`-gated `users_admin` syscall
(`plans/CAPABILITY_USE.md` CU4): an interactive session that lists,
creates, modifies, locks/unlocks, and deletes accounts, edits their
capability ceilings, replaces passwords, and manages groups. It is
interactive (a `users>` prompt over the inherited standard streams);
the one-shot `useradd`/`groupadd` command apps above are thin frontends
over the same syscall — the operation authority lives in exactly one
place, the kernel engine.

Every rule is enforced kernel-side under the caller's attested identity:
the dispatch gate, never-widen grant editing, the last-administrator
guard, the `users-v1` format validation, crash-safe persistence, and the
next-spawn/next-login binding (`docs/src/security/capabilities.md`).
Passwords are read echo-off and hashed client-side into salted PBKDF2
records (salt from `sys:random`); the listing responses are secret-free.

The tool's manifest requests the console pair, `CAP_USER_ADMIN` —
deliberately above the session baseline, so the `manifest ∩ ceiling`
intersection arms it only for an administrator account and leaves it
inert for everyone else — and `CAP_FS_ACCESS`, held solely so the
reserved `-h`/`-?` short-help switches (`plans/APPS.md` §4) can read the
bundle's own `Help/` tree through the secured VFS; accounts themselves
are edited only through the gated syscall, never the filesystem. Any
other command-line argument is a fail-closed usage error — the tool is
administered from inside the session.

### Tests

`cargo test -p tairix-users-cli` drives scripted sessions through the
`ToolIo`/`AdminChannel`/`SaltSource` seams: the command grammar and its
usage refusals, the exact typed requests submitted (decoded and asserted
field by field), the password-record round trip and the
mismatched-password refusal, the grant merge/removal flow against a
served listing, the listing renderers, and the terse errno reporting.

## `man` — show a command's help document (`userland/apps/man`)

`tairix-man` (`/System/Commands/man.app/Run`) renders the help document a
command's application bundle ships (`plans/APPS.md` §7). TAIRiX has no
troff/roff man pages and no `/usr/share/man`: a bundle's single
internationalised `Help/` tree is the one documentation source, and `man`
is its terminal reader.

### Grammar

```
man [-h | -?] <command> [topic]
```

`-h`/`-?` render `man`'s own short help (through the same engine); `--`
ends option parsing; a trailing `.app` names the bundle directly. Exit
codes: `0` page shown, `1` command/document not found or delivery failed,
`2` usage error.

### One resolution, one engine

`man <cmd>` walks `tairix_cmdres::bundle_candidates` — the same
fixed-prefix-then-`PATH` order the shell launches by (`/System/Commands`,
then `/System/Applications`, then the caller's own `<home>/Commands` and
`<home>/Applications`, then each `PATH` entry) — and stops at the first
bundle directory that exists (`NotFound` moves on; any other refusal is
final, mirroring the shell's launch rule), so the page shown always
documents the program the shell would run for the same word. When no
ordered candidate matches a bare word, `man` falls back to a **recursive
bundle search** of the app stores — the machine-wide `/Apps`, then the
user's own `<HOME>/Commands` and `<HOME>/Applications`
(`tairix_cmdres::search_roots`) — walked breadth-first over sorted
listings so the shallowest match wins deterministically. The walk never
descends into another bundle's `.app` directory (a bundle is a sealed
unit), is bounded in depth and by a whole-invocation directory budget (an
exhausted budget is reported as a truncated search, never silently as
"not found"), and a missing root simply lists nothing. `man moose`
therefore finds `/Apps/somefolder/anotherfolder/moose.app`'s help wherever
the bundle was filed; launching stays the shell's fixed-prefix-then-`PATH`
rule, unchanged. The document is located, locale-selected, parsed, and
rendered by `lib/help`, the one shared engine; `man` owns only its
argument grammar, the bundle probe, and the pager.

### Locale

The requested locale is the `LANG` environment variable (a BCP-47 tag the
session/shell exports once, `plans/APPS.md` §5). Fallback is the engine's
deterministic chain (exact → same language → the canonical `en-US/`),
resolved by scanning the bundle's own `Help/` tree for the locales it
actually ships — never a compiled-in language list — so a third-party
bundle carrying only `en-US/` (or any other subset) still serves help; a missing
or malformed `LANG` degrades to the canonical documents. A page served in
a locale other than the requested one is noted with a `context` record
(code `help.locale_fallback`) on `stdinfo` (fd 3) — advisory only, never
affecting output or exit status.

### Paging

Where the kernel attests the console's geometry (`terminal_size`), the
page is shown a screenful at a time — space for the next screenful,
return for one line, `q` to stop — with local echo suppressed while the
pager can prompt. A serial line, pipe, or redirection streams the whole
page.

### Fail closed

An unresolved word, a bundle with no document, an oversized or malformed
document (the `lib/help` bounds), and a refused store probe are all typed
errors reported on standard error — never a panic, never fabricated help
text. The tool holds no ambient authority: its manifest requests the
console pair plus `CAP_FS_ACCESS`, and the secured VFS still authorises
every `Help/` read per-inode under the caller's attested identity.

### Tests

`cargo test -p tairix-man` drives the engine against in-memory
`BundleStore`/`Console` fixtures: the grammar and its refusals, the
fixed-prefix-shadows-`PATH` order, the final-refusal rule,
`.app`/explicit-path words, the recursive app-store search (nested finds,
`/Apps`-before-home order, shallowest-match determinism, the sealed-`.app`
rule, and the reported budget truncation), topics, locale exact/fallback
plus the fd-3 advisory, the pager's key handling, and the `-h` fallback.
`man`'s own `Help/` tree is authored
on disk in the bundle and read at runtime through the `BundleStore` seam,
never embedded in the binary; `tools/syshelp` discovers it from that
source and `tools/mkimage` and the QEMU image fixture plant it on the
read-only `/System` volume, where the `session_ceiling` QEMU vertical
types `man man` end to end.

## `edit` — full-screen text editor (`userland/apps/edit`)

A curses text editor in the spirit of the classic QuickBasic / MS-DOS
editor: a menu bar (`File`, `Search`) across the top, the text area below
it (white on blue on a colour terminal, degrading honestly on shallower
ones), and a status line with the file name, cursor position, and key
hints. It edits one buffer at a time and draws exclusively through
`lib/curses` — no private escape emission.

### Grammar

`edit [file] [-h | -?]` — at most one operand. A named file that does not
exist opens as an empty buffer created on the first save; an unnamed
buffer asks for a name when first saved. `-h`/`-?` render the bundle's own
`Help/` document through the shared `lib/help` engine.

### Keys and menus

Typing inserts (`Insert` toggles overwrite), `Enter` splits, `Backspace`/
`Delete` join at line ends, `Tab` inserts spaces to the next eight-column
stop, and the arrows/`Home`/`End`/`PageUp`/`PageDown` move a cursor the
view follows, horizontally too. `F1` shows the key summary, `F2` saves,
`F3` repeats the last find, and `F10` — or `Alt` plus a title's
accelerator letter, highlighted on the bar (`Alt-F`, `Alt-S`) — opens
the menu (`File`: `New`, `Open...`, `Save`, `Save As...`, `Exit`;
`Search`: `Find...`, `Repeat Last Find`); `Esc` (or `F10`) closes it.
An action that would discard unsaved changes asks first (`y` save /
`n` discard / `c` or `Esc` cancel). The Alt chords arrive as the
"meta sends escape" `ESC`-prefix form (`tairix_vt::Op::Meta`), decoded
to `Event::Alt` by the shared `lib/curses` input decoder.

### Honest file handling

Input must be UTF-8 text within a 16 MiB validation bound; a binary file,
a lone carriage return, or an over-large file is refused with the reason
stated, never opened as garbage. Tab expansion and CRLF→LF conversion are
announced on the status line, never silent, and the file's final-newline
presence is preserved so an untouched buffer round-trips byte for byte. A
failed initial load aborts before the screen is taken over; a failed load
or save inside the session posts a notice and keeps the buffer — the
session never dies over a refused file.

### A seam-injected, host-testable core

The `TextBuffer` (decode/edit primitives) and the `Model` (edit/menu/
prompt/confirm state machine) are pure; file I/O goes through the `Fs`
seam (in production the kernel-authorised `fs_*` syscalls via
`tairix-rt`; in tests an in-memory map) and the display through the
curses `Tty` seam. The blocking input read is parked by the kernel —
never a poll loop.

### Tests

`cargo test -p tairix-edit` drives the parser, the buffer's round-trip/
refusal/editing behaviour, the full keystroke state machine (saving,
save-as, confirm flows, open, wrap-around find, notices) against the
in-memory filesystem, and the renderer against an in-memory tty,
including horizontal scrolling over double-width glyphs and a tiny
screen. The per-locale `OPTIONS` pin keeps the thirteen-locale `Help/` tree
aligned with the parser.
