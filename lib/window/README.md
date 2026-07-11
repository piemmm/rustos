# rustos-window

Stability tier: **experimental**.

The window-channel protocol engine (`plans/APPWIN.md` AW2): the one
definition of the zero-copy, owner-keyed app-window semantics shared by
both ends of the `WINDOW_ENDPOINT` rendezvous, so the desktop session's
server and every app's client can never drift apart.

- **Server** (`WindowServer`): the engine the desktop session composes.
  It decodes each fixed-width `WindowRequest`, attests the in-flight
  caller through the injected `CallerIdentity` seam (the kernel's
  `call_peer_origin` — an unforgeable `ProcId`, never a claimed id),
  keys every window to that owner, maps the app's endpoint-directed
  `shm_grant` region **once** at `Create` through the shared
  `rustos_display::ShmMapper` seam, and hands each `Present` to the
  injected `WindowHost` (the session's compositor bridge) as a
  bounds-checked frame slice plus a validated damage rectangle — no
  per-present mapping, allocation, or copy of its own. A `Present` or
  `Close` naming a window the caller does not own is refused `NotFound`
  (no existence oracle); a per-client window cap bounds how much pinned
  memory one app can reserve; a dead client's windows are torn down
  fail-closed via `client_exited`. Routed input is pushed the other way
  with `deliver_event`, which validates the event against the addressed
  window (owner's endpoint, window-local pointer bounds) before it is
  encoded.
- **Client** (`WindowClient` / `WindowEvents`): the app-side half over
  the injected `WindowTransport` seam (the `ipc_call` syscall in
  production). `create` validates and sends the window geometry, grant
  handle, event endpoint, and title, returning the session-minted window
  id; `present` sends a frame index plus damage, never pixels; `close`
  tears the window down. `WindowEvents` wraps the injected `EventSource`
  seam — a **parked** wait on the app's own event endpoint, never a
  poll — and decodes each delivered `WindowEvent` fail-closed.

The wire format itself lives in `rustos_abi::window_ipc`; this crate adds
the behaviour. Both halves are host-proven in `src/tests.rs` against an
in-process loopback (a real `WindowServer` behind the client seams), so
the request semantics have exactly one tested definition. The production
wiring — the session serving the reserved endpoint from its waitset loop
and the app parking on its event endpoint — lands with the session and
app bundles (`plans/APPWIN.md` AW3/AW4).
