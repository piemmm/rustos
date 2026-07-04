# `rustos-ps` — list processes

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `ps` lists running
processes through the typed, versioned, capability-checked System
Information API (`sysinfo-v1`) served by `/System/Services/sysinfod.app/Run`
(`AGENTS.md` §16.6). RustOS has no `/proc` and no `/sys`: `ps` issues the
API's process-list queries and has no privileged path that bypasses the
capability check. By default it lists the caller's own processes; `-e`/`-A`
request every process system-wide, which the service gates on
`CAP_SYSINFO_GLOBAL`.

The crate is `no_std` (with `alloc`, used only by the test fixtures), has no
`unsafe`, and no `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md`
§2.9). Its only dependencies are the audited `rustos-abi` crate and the
shared `rustos-procinfo` client helpers, so it never links a kernel or
driver crate (`AGENTS.md` §17.4).

## Usage

```
ps [-e | -A | --all]

  (default)   list your own processes
  -e, -A      list every process (needs CAP_SYSINFO_GLOBAL)
  -h, --help  show the usage banner
```

`ps` takes no file operands. `--` ends option parsing. An unknown option or
any positional operand is a fail-closed `PsError::Usage`.

## Shared with the `sysinfo` CLI

`ps` and the `sysinfo` umbrella command both read the same process list, so
the request framing, the `offset`/`limit` page walk, the fixed-column row
rendering (`PID PPID UID GID S CPU NAME`), and the `Transport`/`Output`
seams live once in `lib/procinfo` rather than being copied (`AGENTS.md`
§2.2). Sibling userland crates may not depend on one another (`AGENTS.md`
§17.4), so the shared piece is a `lib/*` crate. `ps` owns only its own
argument grammar, usage banner, and `PsError`.

## A renderer, not a policy point

`run` pages through the process list via `lib/procinfo` and renders one row
per process. The capability gate lives in `sysinfod`, not here: a denied
global listing comes back as `Errno::PermissionDenied`, which `ps` renders
honestly as `PsError::PermissionDenied` (`AGENTS.md` §5.4 — the service is
the policy point). The two operations that reach the outside world — issuing
the request and writing the terminal — are the injected `Transport` and
`Output` seams. On a running system these are IPC- and console-backed; in
tests they are in-memory fixtures, so every parsing and rendering decision
is testable without a kernel.

## The `Run` binary

The crate is both this request/render library and the `Run` entry-point
binary (`rustos-ps-run`, `src/run.rs`) a shell spawns. Built for a Tier-1
target it is a freestanding pure-Rust program: it links the `rustos-rt`
runtime, collects its inherited arguments, parses them, and runs the command
against the production seams shared through `lib/procinfo`
(`IpcTransport` over the `sysinfo` IPC endpoint, `RtOutput` over fd 1). It is
registered at `/System/Apps/ps.app/Run` and holds only `CAP_CONSOLE_WRITE`; every
per-query scope is enforced by `sysinfod` against the caller's kernel-attested
origin. On the host it is an inert stub, so the library stays fully testable.

## Fail closed

An unknown option or a positional operand is a `PsError::Usage`. A denied
global listing is `PsError::PermissionDenied`; any other transport failure
or an undecodable reply is `PsError::Service`; a failed terminal write is
`PsError::Output`. There is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-ps` drives the parser and the engine against an
in-memory `sysinfod` fixture and a recording output: the command grammar
(default self-listing, the `-e`/`-A`/`--all` selectors, `-h`/`--help`,
unknown-option and positional-operand rejection), the self/global query
routing, header + rows rendering, the empty listing, the denied-global
capability mapping, and the header/row write-failure paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
