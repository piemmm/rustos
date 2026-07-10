# `rustos-drv-display-framebuffer` — framebuffer display service

The user-space framebuffer display-service process (`plans/DISPLAY.md`
D7b). The crate is **only** a binary: the `Run` entry point of the
display-service bundle, installed under `/System/Drivers/` and
autoloaded by `devmgr` when a display node carrying a
`HwResourceKind::Framebuffer` resource is discovered. It is the display
half of the zero-copy, lease-gated present path: a desktop session
composes frames into one `shm_grant`ed region and presents by index
over the reserved `DISPLAY_ENDPOINT`; this process blits the presented
frame to the scan-out surface.

The crate contains **no device logic of its own**: the linear-surface
blit engine is `rustos_display::Framebuffer` and the protocol engine is
`rustos_display::DisplayServer` — both `lib/display`, the one shared
definition the framebuffer QEMU verticals also drive. `main` only wires
the real seams:

- `RtDriverHost::from_grants_query` (`lib/drvrt`): the kernel-issued
  device-resource grants, mapped through the capability-gated
  `mmio_map` trap.
- `sole_framebuffer` (`lib/abi`): the surface's `(phys_base, mode)`
  resolved fail-closed from the delivered grants — a missing,
  ambiguous, or malformed surface grant exits rather than scanning out
  a guessed geometry.
- `RtSeatCheck` over `call_peer_seat`: the kernel-attested, per-request
  live-lease check on the in-flight caller — never a claimed lease.
- `RtShmMapper` over `shm_map`/`shm_unmap`: a `Configure` maps the
  client's granted frame region once, sized from the kernel's own
  record of the region length (never the client's claimed geometry);
  the present hot path only indexes the mapped bytes.
- The reserved `DISPLAY_ENDPOINT` bind (`call_create`) and a wait-set
  serve loop: the service parks between requests (endpoint readiness is
  a non-consuming peek drained by `call_recv`), so an idle display
  service costs no CPU.

## Supported hardware

Any platform whose discovery publishes a linear scan-out surface as a
`Framebuffer` hardware-tree resource (the FDT `simple-framebuffer`
model, QEMU `ramfb`, a UEFI GOP hand-off normalised by the x86_64
port). The service does not enumerate or program a display controller;
mode-setting on programmable controllers is a separate driver class
(see `gpu_virtio`, `rpi_hvs`).

## Required capabilities

- `CAP_MMIO_MAP` — mapping the scan-out window; the surface is reached
  only through the capability-gated `mmio_map` trap, never a pointer
  the service synthesises itself.
- `CAP_SHM` — mapping the client's granted frame region at `Configure`.
- `CAP_IPC_BIND_PRIVILEGED` — binding the reserved `DISPLAY_ENDPOINT`
  rendezvous, so a squatter cannot intercept presents.

The service runs in user space; it does **not** request
`CAP_DRV_KERNEL`. Every present — `Query` included — is gated on the
caller's live seat lease through the kernel's `call_peer_seat`; a
revoked client receives the distinct `SeatRevoked` and its configured
frames are dropped, never scanned out under another lease.

## Failure behaviour

Bring-up failures exit fail-loud with reserved codes (no host: 80, no
surface grant: 81, surface map failed: 82, endpoint bind failed: 83,
wait-set failed: 84), leaving the seat without a display service rather
than wedged or busy-polling; the spawning supervisor decides whether to
relaunch.

## Test surface

The engine logic is host-tested where it lives:

- `lib/display/tests/framebuffer.rs` — the surface engine against a
  mock `MmioMapper` (blit fidelity, damage-region blits, fail-closed
  geometry/capability/short-frame refusals, seat-gated presents,
  unload → reload).
- `lib/display/src/tests.rs` — the `DisplayServer`/`DisplayClient`
  protocol semantics over mock seams and a loopback transport.

The three framebuffer QEMU verticals
(`tests/integration/framebuffer_display_qemu_aarch64`,
`framebuffer_display_qemu_riscv64`, `framebuffer_display_wasm32`) drive
the same shared engine against a real emulated scan-out surface,
including the seat-lease and multi-seat phases. The end-to-end
service-process vertical (display node → autoloaded `Run` → session
present) is `plans/DISPLAY.md` D7d.
