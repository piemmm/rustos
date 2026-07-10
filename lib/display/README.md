# rustos-display

Stability tier: **experimental**.

The display-service protocol engine (`plans/DISPLAY.md` D7b): the one
definition of the zero-copy, lease-gated frame-presentation semantics
shared by both ends of the `DISPLAY_ENDPOINT` rendezvous, so the server
and the client can never drift apart.

- **Server** (`DisplayServer`): the engine a display driver's `Run`
  binary hosts. It decodes each fixed-width `DisplayRequest`, gates it —
  `Query` included — on the caller's **live seat lease** through the
  injected `SeatCheck` seam (the kernel's `call_peer_seat`, an
  oracle-free fact about the in-flight caller), maps the client's
  endpoint-directed `shm_grant` region **once** at `Configure` through
  the injected `ShmMapper` seam, and scans out on `Present` by frame
  index through the `Display` trait — damage-aware via
  `Display::present_region`, with no per-present mapping, allocation, or
  copy of its own. A `Configure` binds the mapped frames to the granting
  lease's generation; a `Present` under any other lease (a revoked or
  re-acquired seat) is refused fail-closed until the new owner
  reconfigures, so one owner's frames can never be scanned out under
  another's lease.
- **Client** (`DisplayClient` / `RemoteDisplay`): the session-side half
  over the injected `DisplayTransport` seam. `RemoteDisplay` implements
  the existing `rustos_abi` `Display` trait over the client's own
  mapping of the shared frame region, so `Compositor::present` is
  unchanged: a present copies only the changed pixels (the damage
  unioned with the target frame's stale region — the double-buffer
  bookkeeping that keeps every frame current) into the back frame and
  sends the index + damage, never the pixels.

Every input is validated fail-closed on both halves; the error
vocabulary crosses the wire as typed `Errno` status frames, converted
through the one shared `DriverError::as_errno` mapping on the server
and the client-side inverse `driver_error_from_errno` here.

`no_std`; unit-tested on the host against mock seams. Consumed by the
framebuffer driver's `Run` binary (server) and the desktop session
(client).
