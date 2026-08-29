# tairix-display

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
  `Display::present_rects`, whose whole rectangle list it validates before
  blitting any of it, with no per-present mapping, allocation, or copy of
  its own. A `Configure` binds the mapped frames to the granting
  lease's generation; a `Present` under any other lease (a revoked or
  re-acquired seat) is refused fail-closed until the new owner
  reconfigures, so one owner's frames can never be scanned out under
  another's lease.
- **Surface engine** (`Framebuffer` / `FramebufferConfig`): the generic
  linear-framebuffer scan-out engine the framebuffer service's `Run`
  binary hosts behind the `Display` trait (and the framebuffer QEMU
  verticals drive directly), hoisted here so the surface blit has
  exactly one definition. It validates the discovered geometry
  fail-closed, maps exactly `stride_bytes * height_px` bytes through
  the capability-gated `MmioMapper`, and blits full frames or damage
  spans with every write bounds-checked by the window.
- **Scan-out encode** (`scanout`): the pixel-exact decisions both
  presenting programs — the compositing window manager and the graphical
  login screen — have to get right, so neither carries its own copy: how
  many bytes a frame is for a mode (`scanout_len`), which byte order the
  mode's format wants (`ChannelOrder::for_format`), the channel encode
  itself (`ChannelOrder::encode`, and `encode_run` for a whole row span
  at a time — the bulk form of that same encoder, not a second one), and
  whether a damage rectangle is a sub-region or the whole frame
  (`sub_screen_damage`). Each refuses rather than guesses: an
  unencodable mode or format is `None`, and a run writes whole pixels
  only, stopping at whichever of its two slices ends first and reporting
  the count the caller checks. The three per-pixel codecs are `#[inline]`,
  and load-bearingly so: each is reached once per pixel from another module
  or crate, and the desktop's crates build with many codegen units and no
  link-time optimisation in the profile the debug image uses, so without it
  a four-byte shuffle costs an out-of-line call.
- **Window-frame codec** (`winframe`): the same pixel/byte boundary for the
  frame a *window* channel carries, in both directions — `encode` writes an
  app's premultiplied surface out as the straight-alpha bytes the frame holds,
  and `decode` reads a presented frame into the compositor's own window surface
  and reports the sub-rectangle whose pixels genuinely changed. It lives beside
  the scan-out encode because the channel-order decision is the same one, and no
  program may hold a second opinion about which byte is red. Both directions are
  row-independent and are expressed over `lib/parallel`'s `JobRunner`: the
  desktop hands the decode a real pool because it cannot bound a pass whose size
  the app declares, and an app encodes on its own thread because it *can* — it
  should present only what it changed. Every index is validated before the first
  write, so a hostile geometry refuses the whole conversion rather than leaving a
  window half-converted.
- **Client** (`DisplayClient` / `RemoteDisplay`): the session-side half
  over the injected `DisplayTransport` seam. `RemoteDisplay` implements
  the existing `tairix_abi` `Display` trait over the client's own
  mapping of the shared frame region, so `Compositor::present` is
  unchanged: a present copies only the changed pixels (the damage
  unioned with the target frame's stale region — the double-buffer
  bookkeeping that keeps every frame current) into the back frame and
  sends the index + damage, never the pixels.

Every input is validated fail-closed on both halves; the error
vocabulary crosses the wire as typed `Errno` status frames, converted
through the one shared `DriverError::as_errno` mapping on the server
and the client-side inverse `driver_error_from_errno` here.

`no_std`, `forbid(unsafe_code)`; unit-tested on the host against mock
seams (the protocol halves in `src/tests.rs`; the surface engine in
`tests/framebuffer.rs`, a separate test crate because its mock
`RegisterWindow` needs the `unsafe` constructor the library itself
forbids). Consumed by the framebuffer service's `Run` binary (server +
surface engine), the framebuffer QEMU verticals (surface engine), and
the desktop session (client).
