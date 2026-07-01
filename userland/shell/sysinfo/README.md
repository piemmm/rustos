# `rustos-sysinfo` — the System Information CLI

Stage 6 deliverable (`AGENTS.md` §3 `userland/shell/`, §16.6). `sysinfo`
is the single command-line tool that exposes the System Information API
to the terminal. RustOS has no `/proc` and no `/sys`; every piece of
live system information is served by `/System/Services/sysinfod` over the
typed, versioned, capability-checked `sysinfo-v1` API. `sysinfo` is a
*client* of that API — it does **not** read a virtual filesystem, and it
has no privileged path that bypasses the capability check (`AGENTS.md`
§16.6).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` crate and the shared
`rustos-procinfo` client helpers (and, for the `Run` binary only, the
`rustos-rt` userland runtime), so it never links a kernel or driver crate
(`AGENTS.md` §17.4).

## Usage

```
sysinfo <query>

processes [--all]   list processes (--all: every process, needs CAP_SYSINFO_GLOBAL)
memory              kernel memory statistics (needs CAP_SYSINFO_KERNEL)
hardware            detected hardware tree (needs CAP_SYSINFO_HW)
identity            machine identity and OS version
uptime              time since boot and boot wall-clock time
limits              your effective resource limits and live usage
help                show the usage banner
```

`processes` (without `--all`), `identity`/`uptime`, and `limits` require
no capability; the privileged queries are gated by `sysinfod`, not by this
tool. `limits` (alias `rlimits`) is self-scoped — it reports the calling
process's *own* effective resource limits and live usage (`AGENTS.md`
§24.3); the `ulimit` shell builtin is the counterpart that *changes* them.

## A request/render machine, not a data source

`run` decides *which* query to issue, builds the typed
`SysinfoRequestHeader` and request payload from the `sysinfo-v1` ABI,
decodes the typed reply with the ABI's fail-closed `from_bytes`
decoders, and renders human-readable rows. The two operations that reach
the outside world are injected seams, mirroring the other userland
crates:

- `Transport` — carries the encoded request to `sysinfod` and returns
  the reply bytes. The transport owns the reply allocation, so the
  client never guesses a buffer size.
- `Output` — writes one rendered line to the terminal.

On a running system these are IPC- and console-backed; in tests they are
in-memory fixtures, so every rendering and paging decision is testable
without a kernel.

## The `Run` binary

The crate is both this request/render library and the `Run` entry-point
binary (`rustos-sysinfo-run`, `src/run.rs`) a shell spawns. Built for a
Tier-1 target it is a freestanding pure-Rust program: it links `rustos-rt`,
collects its inherited arguments, parses them, and runs the query against the
production seams shared through `lib/procinfo` (`IpcTransport` over the
`sysinfo` IPC endpoint, `RtOutput` over fd 1). It is registered at
`/Apps/Sysinfo.app/Run` and holds only `CAP_CONSOLE_WRITE`; every per-query
scope is enforced by `sysinfod` against the caller's kernel-attested origin.
On the host it is an inert stub, so the library stays fully testable.

## Fail closed

A capability denial comes back from `sysinfod` as
`Errno::PermissionDenied`, which the CLI renders as a precise "this query
requires a capability you do not hold" diagnostic without inventing a
parallel policy (`AGENTS.md` §2.2, §16.6). An unknown subcommand, an
unknown flag, or a stray argument is a usage error that issues no query.
A reply that does not decode against `sysinfo-v1` — a truncated scalar, a
process page whose length is not a whole number of records — is a hard
error, never a partially-rendered guess.

The hardware-tree wire format is owned by `lib/abi` §18 and is not built
yet, so `sysinfo hardware` honestly reports the byte length the service
returned rather than pretending to decode it (`AGENTS.md` §2.1).

## Tests

`cargo test -p rustos-sysinfo` drives the parser and the request/render
engine against an in-memory `sysinfod` stand-in and a recording output:
the command grammar (every subcommand, alias, and the usage-error
paths), every query's rendering, process-list paging across a page
boundary, self-vs-global query routing, and the denied, malformed,
truncated, and dead-console fail-closed paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
