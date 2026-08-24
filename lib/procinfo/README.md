# tairix-procinfo

Shared `no_std` client helpers for the TAIRiX System Information API
(`sysinfo-v1`, `AGENTS.md` §16.6): the request/response seams, the
capability-aware call mapping, a generic paged-list walk, and the
process-list and mount-list row rendering used by the terminal tools that
read live system state.

TAIRiX has no `/proc` and no `/sys`; the `sysinfo` umbrella command, the
POSIX-named `ps`, and the `mount` listing all speak the typed API served by
`/System/Services/sysinfod.app/Run`. They would otherwise duplicate the request
envelope, the page walk, and the row render, so that shape lives here in one
place (`AGENTS.md` §2.2). Sibling userland crates may not depend on each
other (`AGENTS.md` §17.4); this `lib/*` crate is the permitted shared home.

The crate provides:

- `Transport` and `Output` — the object-safe seams a tool injects, so its
  request/render logic runs against in-memory fixtures with no kernel.
- `encode_request` / `call` / `CallError` — framing a `sysinfo-v1` request
  and mapping a capability denial onto a distinguished error.
- `ListError` — the shared error type for the paged-list walks, returned by
  both `for_each_process` and `for_each_mount`.
- `walk_pages` / `WalkStep` — the generic paging loop every `for_each_*` walk
  is built on, public so a consumer with its own bound or cadence policy (the
  Switchboard sampler, which caps how many records one reading may
  accumulate) drives it directly rather than re-implementing it.
- `WalkStep` is how *every* walk is bounded on the caller's side: each
  `for_each_*` sink answers `WalkStep::Continue` for the next record or
  `WalkStep::Stop` to end the walk there, and stopping returns `Ok`, so a
  deliberate truncation is never mistaken for a failed service and no
  caller is left paging a list only an offset overflow would end. A sink
  that wants everything simply always continues; one with an answer already
  (a single-uid lookup, a named-volume lookup) stops at the match.
- `for_each_cpu_time` / `CpuTotals` — the paged CPU-time walk and its busy/idle
  delta arithmetic.
- `for_each_process` / `PROCESS_HEADER` / `render_process` / `state_char` —
  the paged process-list walk and its fixed-column rendering.
- `for_each_mount` / `render_mount` / `render_options` — the paged
  mount-table walk and its `source on target type fstype (options)`
  rendering.
- `pressure::refresh_into` — reading the published memory-pressure band and
  publishing it to a `tairix_reclaim::ReportedPressure` gauge, the one
  definition every caching program keeps its band current through.
- `resolve` / `ResolveInfoError` — the userspace `info:`/`state:`/`stats:`
  resolver, mapping a parsed `resref` reference onto a registry-defined query.
  Its `Display` is the one wording of a refusal and its `to_errno` the one
  stable code, so `sysinfo show`, the shell, and a tool reading an operand all
  report the same refusal identically.
- `read_value` / `MAX_VALUE_LEN` — that resolver's value rendered as the bytes
  a reader consumes, bounded so a caller can size a pipe write against it.

Each consuming tool keeps its own argument grammar, usage banner, and error
enum; this crate owns only the parts they share.

## The `program` feature (production seams)

The default-off `program` feature adds the concrete client implementations
the `sysinfo` and `ps` `Run` binaries link (and the `top` TUI when its `Run`
binary lands):

- `IpcTransport` — a `Transport` that carries a framed `sysinfo-v1` request to
  `/System/Services/sysinfod.app/Run` over the well-known `SYSINFO_ENDPOINT` IPC call
  (`tairix_rt::ipc_call`) and unwraps the reply frame (`decode_reply`),
  surfacing a per-query refusal as the exact `Errno`.
- `RtOutput` — an `Output` that writes each rendered line to the inherited
  standard output (fd 1) through `tairix-rt`.
- `NamedSource` / `OpenError` — the one open-by-name path for a readable
  source a tool was given: a path or stream reference through the kernel, an
  `info:`/`state:`/`stats:` value through the broker (which no kernel backing
  can serve). `cat` reads its operands through it; a refusal keeps the
  resolver's typed reason so the caller can name the capability a denial
  wanted.
- `args` / `write_stderr_line` — the shared argument-vector walk and the
  standard-error diagnostic sink the tool `Run` binaries use, written once
  here rather than pasted into each (`AGENTS.md` §2.2).
- `pressure::watch` / `pressure::refresh` — arming the edge-triggered
  `WaitSourceKind::MemoryPressure` wake against this process's gauge, and
  draining it. `watch` also primes the gauge, because the wake reports only
  *changes* and the gauge admits nothing until it is told a band: a program
  that skips this does not cache imperfectly, it caches nothing at all and
  rebuilds every value on every use (`plans/SMARTRAM.md` SMART5).

The feature pulls the freestanding userland runtime `tairix-rt` and is enabled
only for a bare-metal (`target_os = "none"`) program build; the host tooling
and the pure library never link the runtime.

See the crate-level rustdoc for the full surface.

## Stability tier

`experimental` — the surface tracks the `sysinfo-v1` ABI in `lib/abi` and is
consumed by `userland/shell/sysinfo`, `userland/apps/ps`,
`userland/apps/mount`, and every program that keeps a reclaimable cache. It is
`no_std` (with `alloc`) and depends only on the audited `lib/abi` crate, the
shared reference parser `lib/resref`, and the shared reclaim model
`lib/reclaim` (`AGENTS.md` §17.4). No `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).
