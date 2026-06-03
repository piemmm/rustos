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

12/12 host-side tests pass.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The `RpiHvs`
type and its config types (`HvsConfig`, `ScanoutConfig`, `PlaneConfig`)
are re-exported so the driver host can construct an instance; the host
never reaches into the type beyond the `Display` / `AcceleratedDisplay`
trait surface.

## Tier

`experimental`.
