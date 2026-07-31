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
  both `for_each_process` and `for_each_mount` (the generic paging loop they
  share is crate-internal).
- `for_each_cpu_time` / `CpuTotals` — the paged CPU-time walk and its busy/idle
  delta arithmetic.
- `for_each_process` / `PROCESS_HEADER` / `render_process` / `state_char` —
  the paged process-list walk and its fixed-column rendering.
- `for_each_mount` / `render_mount` / `render_options` — the paged
  mount-table walk and its `source on target type fstype (options)`
  rendering.

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
- `args` / `write_stderr_line` — the shared argument-vector walk and the
  standard-error diagnostic sink the tool `Run` binaries use, written once
  here rather than pasted into each (`AGENTS.md` §2.2).

The feature pulls the freestanding userland runtime `tairix-rt` and is enabled
only for a bare-metal (`target_os = "none"`) program build; the host tooling
and the pure library never link the runtime.

See the crate-level rustdoc for the full surface.

## Stability tier

`experimental` — the surface tracks the `sysinfo-v1` ABI in `lib/abi` and is
consumed by `userland/shell/sysinfo`, `userland/apps/ps`, and
`userland/apps/mount`. It is `no_std` (with `alloc`) and depends only on the
audited `lib/abi` crate (`AGENTS.md` §17.4). No `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).
