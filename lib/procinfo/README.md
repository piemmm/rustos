# rustos-procinfo

Shared `no_std` client helpers for the RustOS System Information API
(`sysinfo-v1`, `AGENTS.md` §16.6): the request/response seams, the
capability-aware call mapping, and the process-list paging plus row
rendering used by the terminal tools that read live system state.

RustOS has no `/proc` and no `/sys`; the `sysinfo` umbrella command and the
POSIX-named `ps` both speak the typed API served by
`/System/Services/sysinfod`. They would otherwise duplicate the request
envelope, the page walk, and the columnar render, so that shape lives here
in one place (`AGENTS.md` §2.2). Sibling userland crates may not depend on
each other (`AGENTS.md` §17.4); this `lib/*` crate is the permitted shared
home.

The crate provides:

- `Transport` and `Output` — the object-safe seams a tool injects, so its
  request/render logic runs against in-memory fixtures with no kernel.
- `encode_request` / `call` / `CallError` — framing a `sysinfo-v1` request
  and mapping a capability denial onto a distinguished error.
- `for_each_process` / `PROCESS_HEADER` / `render_process` / `state_char` —
  the paged process-list walk and its fixed-column rendering.

Each consuming tool keeps its own argument grammar, usage banner, and error
enum; this crate owns only the parts they share.

See the crate-level rustdoc for the full surface.

## Stability tier

`experimental` — the surface tracks the `sysinfo-v1` ABI in `lib/abi` and is
consumed by `userland/shell/sysinfo` and `userland/apps/ps`. It is `no_std`
(with `alloc`) and depends only on the audited `lib/abi` crate
(`AGENTS.md` §17.4). No `unsafe`, and no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9).
