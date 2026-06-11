# `rustos-drv-display-rpi-hvs` — Raspberry Pi HVS display driver

Stage 7 deliverable. The first **GPU-accelerated** display driver: it
exposes the Raspberry Pi VideoCore Hardware Video Scaler (HVS) as a
hardware **layer compositor** through the
`rustos_abi::driver::display::AcceleratedDisplay` seam, so the desktop
compositor (`userland/gui/wm`) can hand it the visible windows as
layers and let the HVS composite and scan them out — the host never
blends the whole screen in software.

The driver also implements the plain `Display` trait, so the software
full-frame path remains the **mandatory fallback** (`AGENTS.md` §10):
when the window stack exceeds the hardware plane budget the compositor
composites in software and presents one finished frame.

## How acceleration works

The HVS composites by walking a *display list* (DLIST) of plane entries
held in a small dedicated RAM. On each accelerated present the driver:

1. Uploads each layer's premultiplied pixels into a per-plane,
   GPU-visible source buffer (a capability-mapped MMIO region).
2. Builds one unity-scaled DLIST plane entry per layer (control word,
   position, size, source bus pointer, pitch, alpha), modelled on the
   VC4 HVS plane format — six 32-bit words per plane plus an end marker.
3. Writes the DLIST into the HVS display-list RAM and arms the display
   channel through its control register.

Plane pointers are **bus addresses**: a plane's physical address is
translated through the configured VideoCore bus alias (default
`0xC000_0000`) and bounds-checked against the 30-bit aperture, failing
closed rather than driving the engine off a bad address.

## Supported hardware

| Platform               | Engine                         | Status                |
|------------------------|--------------------------------|-----------------------|
| aarch64 (Raspberry Pi) | VideoCore VC4 HVS              | mock-host tests       |

The driver consumes an `HvsConfig` the firmware/boot capability already
produced (scan-out geometry, the HVS DLIST RAM, the display-channel
control register, and the per-plane source buffers). It does not
enumerate the SoC itself.

## Firmware framebuffer discovery (`mailbox`)

The `mailbox` module (the `plans/PI.md` P7 protocol layer) produces the
scan-out half of that `HvsConfig` by speaking the BCM2711 **mailbox
property channel** to the GPU firmware:

- `FramebufferRequest::encode` frames the request (set physical/virtual
  size, depth 32, pixel order from the `DisplayFormat`, allocate at page
  alignment, get pitch; end tag).
- `decode_framebuffer_response` validates the in-place answer
  fail-closed: header code, per-tag response bits and lengths, exact
  geometry echoes, and pitch/size consistency.
- `bus_to_arm_physical` strips the 2-bit VideoCore bus alias and rejects
  a zero, unaligned, or out-of-aperture buffer; the result becomes the
  `ScanoutConfig` (and the firmware's alias feeds `HvsConfig.bus_alias`).
- The doorbell sits behind the `MailboxTransport` seam. `MmioMailbox`
  drives the real register block (`0xFE00_B880`, reached as a discovered
  `hwtree` resource under `CAP_MMIO_MAP`) with a budget-bounded poll
  that fails closed with `Timeout` rather than spinning forever.

QEMU models neither the VideoCore firmware nor an in-aperture RAM
window (`virt` RAM begins at `0x4000_0000`, beyond the 30-bit
aperture), so emulation proves the full host-side chain against a
protocol-faithful mock firmware; the real scan-out is the `plans/PI.md`
P7 metal acceptance item.

## Driver-host wiring (`wiring`)

The `wiring` module brings the driver up from the hardware tree: the
aarch64 `FdtDiscovery` emits the mailbox node (`brcm,bcm2835-mbox`)
with the doorbell MMIO window and a `Dma` request for a one-page,
aperture-bounded property-buffer carve. `wiring::open_discovered`
checks `CAP_MMIO_MAP`, maps both, translates the carve to a bus address
(`arm_physical_to_bus`), rings `MmioMailbox`, and delegates to
`wiring::open_with_transport`, which assembles the full `HvsConfig`
(firmware scan-out + the host's `HvsRegions`: DLIST RAM, control
window, plane carves) for `RpiHvs::open`.

### Pixel formats

`DisplayFormat::Rgba8888` and `DisplayFormat::Bgra8888`. Layer pixels
are **premultiplied alpha** in the active format, matching the
compositor's pixel model (`AGENTS.md` §10) so no extra straight-alpha
conversion is needed.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` at `RpiHvs::open` time — every region (scan-out, DLIST
  RAM, control register, per-plane buffers) is device-visible memory
  reached only through the capability-gated `MmioMapper`, never through
  a pointer the driver synthesises itself (`AGENTS.md` §4).
- The `Display` / `AcceleratedDisplay` methods are gated by ownership of
  the `DriverHandle` returned from `register`.

The driver runs in user space; it does **not** request `CAP_DRV_KERNEL`.

## Lifecycle

`register` clears the load-time gate; `RpiHvs::open` maps every region;
dropping the `RpiHvs` releases the windows (the unload step — the kernel
reclaims the mappings). Reloading is calling `RpiHvs::open` again.

## Test surface

`cargo test -p rustos-drv-display-rpi-hvs` exercises, against an
in-process multi-region mock `MmioMapper`:

- `register` capability gate.
- `open` reports the configured `DisplayMode` and `accel_caps`.
- Software `present` byte-fidelity into the scan-out surface and
  short-frame rejection (`BufferTooSmall`).
- `present_layers` uploads each layer into its plane buffer **and**
  encodes a correct DLIST (valid bit, plane pointers as bus addresses,
  pitch, alpha, packed position, end marker), bumping the control
  register's present generation.
- Fail-closed paths: more layers than planes, a layer larger than the
  screen, and a layer whose pixels are shorter than its geometry.
- `open` capability gates (`CAP_MMIO_MAP`, absent mapper).
- Config validation (zero planes, a too-small DLIST) and bus-address
  aperture bounds.

The `mailbox` module adds, against a protocol-faithful mock firmware
and RAM-backed doorbell windows:

- Request framing (tag order, lengths, pixel-order mapping) and
  degenerate-geometry rejection.
- Response validation fail-closed paths: firmware error and unknown
  header codes, bad header length, missing response bit, short and
  oversized tag responses, substituted geometry echoes, missing tag,
  inconsistent pitch/size, bad buffer aperture.
- Bus↔physical translation in both directions
  (`bus_to_arm_physical` / `arm_physical_to_bus`) across all four
  aliases and their aperture fail-closed cases.
- `MmioMailbox` construction validation, the stage→ring→read-back
  doorbell path, three timeout modes, and rejection of a foreign
  property completion.
- The full discovery chain: mock firmware →
  `wiring::open_with_transport` → `ScanoutConfig` → `RpiHvs::open` →
  `present` into the discovered surface.
- The `wiring` fail-closed paths: missing `CAP_MMIO_MAP`, missing
  mapper, an out-of-aperture property-buffer carve, and a silent
  firmware timing the exchange out after `open_discovered` staged the
  request and rang the doorbell.

38/38 host-side tests pass.

## Public surface

`AGENTS.md` §8 — the only public *function* on the driver itself is
`register`. The `RpiHvs` type and its config types (`HvsConfig`,
`ScanoutConfig`, `PlaneConfig`) are re-exported so the driver host can
construct an instance; the host never reaches into the type beyond the
`Display` / `AcceleratedDisplay` trait surface. The `mailbox` module is
the discovery half of that same host hand-off: the host calls
`discover_framebuffer` over a `MailboxTransport` to *produce* the
`HvsConfig` it then passes to `RpiHvs::open` — it is not device control
surface, and nothing in it bypasses the driver's capability gates. The
`wiring` module composes the two for the host —
`open_discovered`/`open_with_transport` over the hardware-tree mailbox
node — and is gated by the same `CAP_MMIO_MAP` check.

## Tier

`experimental`.
