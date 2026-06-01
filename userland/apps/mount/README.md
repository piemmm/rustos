# `rustos-mount` — list and attach filesystems

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `mount` both reports
and changes the mount table, and the two halves take different paths.
**Listing** the mounted filesystems is a read of live system state, so —
like `ps` — it goes through the typed, versioned, capability-checked System
Information API (`sysinfo-v1`) served by `/System/Services/sysinfod`
(`AGENTS.md` §16.6): RustOS has no `/proc` and no mount-table file, so
`mount` issues the ungated `MOUNT_LIST` query and has no privileged path
that bypasses the capability check. **Attaching** a filesystem is
privileged — it needs `CAP_FS_MOUNT` (`AGENTS.md` §5.2) — and the kernel,
not this tool, makes that decision (`AGENTS.md` §5.4).

The crate is `no_std` (with `alloc`, used only by the test fixtures), has no
`unsafe`, and no `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md`
§2.9). Its only dependencies are the audited `rustos-abi` crate and the
shared `rustos-procinfo` client helpers, so it never links a kernel or
driver crate (`AGENTS.md` §17.4).

## Usage

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
toggles. `--` ends option parsing. The `-o` names map onto the frozen
`MountFlags` bitmap (`AGENTS.md` §5.3).

## Shared with the other `sysinfo` clients

Listing pages the `MOUNT_LIST` reply through the same `lib/procinfo`
machinery `ps` and `sysinfo` use — the `Transport`/`Output` seams, the
request framing, and the generic `offset`/`limit` page walk — so none of it
is copied (`AGENTS.md` §2.2). The shared renderer prints one familiar
`source on target type fstype (options)` line per mount. Because sibling
userland crates may not depend on one another (`AGENTS.md` §17.4), that
shared piece is the `lib/procinfo` crate; `mount` owns only its own argument
grammar, the `Mounter` attach seam, the usage banner, and `MountError`.

## A presenter, not a policy point

For a `SOURCE TARGET` request `run` builds a `MountSpec` and hands it to the
injected `Mounter` seam; it makes no permission decision of its own. The
kernel is the policy point (`AGENTS.md` §5.4): a missing `CAP_FS_MOUNT`, an
unknown source, a bad superblock, or an already-mounted target is refused
there and surfaced as `MountError::Mount(errno)`. On a running system the
`Transport`, `Output`, and `Mounter` seams are IPC-, console-, and
syscall-backed; in tests they are in-memory fixtures, so every parsing,
rendering, and routing decision is testable without a kernel.

## Fail closed

An unknown option, a missing option value, or a number of operands other
than zero or two is a `MountError::Usage`; an unknown or empty `-o`/`-t`
value is a `MountError::BadOption`. A listing transport failure or an
undecodable reply is `MountError::Service`; a refused or failed attach is
`MountError::Mount`; a failed terminal write is `MountError::Output`. There
is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-mount` drives the parser and the engine against an
in-memory `sysinfod` fixture, a recording output, and an in-memory mounter:
the command grammar, the mount-table listing and its query routing, the
empty table, the service/output-failure paths, the attach request reaching
the mounter with the right fields, and the denied-attach mapping.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
