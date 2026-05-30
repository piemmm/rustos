# `rustos-drv-display-vesa` — VESA (VBE) linear-framebuffer driver

Stage 4 deliverable. Implements `rustos_abi::driver::display::Display`
over the linear framebuffer a **VESA BIOS Extensions (VBE)** mode
exposes on a legacy x86_64 PC. Mode selection happens in the bootloader
(the kernel cannot re-enter real mode to issue VBE BIOS calls), so the
boot stub captures the 256-byte VBE `ModeInfoBlock` for a 32-bpp
direct-colour linear-framebuffer mode and hands it to the driver host as
a boot capability. This driver parses and validates that block, maps the
linear framebuffer through the capability-gated `MmioMapper`, and
presents fully-rendered frames into it.

## Supported hardware

| Platform | Surface source                          | Stage 4 status       |
|----------|-----------------------------------------|----------------------|
| x86_64   | VBE 2.0+ linear framebuffer (BIOS/VBE `ModeInfoBlock`) | mock-host tests only |

The driver does **not** program the display controller or issue VBE BIOS
calls; it consumes the `ModeInfoBlock` the bootloader already captured.
Mode-setting on programmable controllers is a separate driver class
(`gpu_virtio`); the post-firmware generic surface path is the sibling
`framebuffer` driver. The two display drivers are deliberate siblings
(`AGENTS.md` §2.2 carve-out): VESA owns the VBE-specific decode,
`framebuffer` consumes an already-parsed geometry record.

### Accepted modes

A `ModeInfoBlock` is accepted only when it describes:

- a **supported** mode whose **linear framebuffer** is available
  (`ModeAttributes` bits 0 and 7),
- the **direct-colour** memory model (`MemoryModel == 6`),
- **32 bits per pixel** with **8-bit** red/green/blue masks, and
- a channel layout that is either `Bgra8888` (red at bit 16, green 8,
  blue 0 — the common PC layout) or `Rgba8888` (red at bit 0, green 8,
  blue 16),
- a non-zero `PhysBasePtr` and a stride wide enough for one scanline.

Anything else fails closed (`Unsupported` / `LengthOutOfRange` /
`DeviceFault`) so the byte-preserving `present` copy never mis-renders.
Indexed/packed-pixel and sub-32-bpp modes are out of scope for `abi-v1`
(the `DisplayFormat` surface covers only the two 32-bpp byte orders).

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` at `VesaFramebuffer::open` time — the linear
  framebuffer is device-visible memory and is reached only through the
  capability-gated `MmioMapper`, never through a pointer the driver
  synthesises from the block's `PhysBasePtr` (`AGENTS.md` §4).
- The `Display` methods (`mode_info`, `present`) are gated by ownership
  of the `DriverHandle` returned from `register`.

The driver runs in user space; it does **not** request `CAP_DRV_KERNEL`.

## Lifecycle

`register` clears the load-time gate; `VesaFramebuffer::open` parses the
block and maps the surface; dropping the `VesaFramebuffer` releases the
window (the unload step — the kernel reclaims the mapping). Reloading is
calling `open` again, which the `unload_then_reload_presents_again` test
exercises.

## Test surface

`cargo test -p rustos-drv-display-vesa` exercises, against an in-process
mock `MmioMapper`:

- `register` capability gate.
- `VbeModeInfo::parse`: `Bgra8888` + `Rgba8888` decode and every
  rejection path (short block, unsupported/non-linear attributes,
  non-direct-colour model, non-32-bpp, non-8-bit masks, unknown channel
  layout, zero `PhysBasePtr`, degenerate geometry).
- `open` reports the decoded `DisplayMode`.
- `present` byte-fidelity into the mapped surface, including a surface
  whose length is not a multiple of four (the `u32` bulk path plus the
  byte tail).
- Short-frame rejection (`BufferTooSmall`) and oversized-frame
  truncation.
- `open` capability gates (`CAP_MMIO_MAP` on the host and at the
  mapper), absent mapper (`Unsupported`), a region the platform cannot
  map (`Unsupported`), and a parse failure surfacing before any mapping.
- Unload → reload round-trip.

23/23 host-side tests pass. A QEMU integration vertical depends on the
kernel publishing the boot-captured VBE `ModeInfoBlock` as a capability
and exposing a `MmioMapper` over the linear framebuffer; that boot
hand-off is not yet in the tree, exactly as the framebuffer, ps2, and
virtio-blk QEMU verticals waited on their kernel prerequisites.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`VbeModeInfo` and `VesaFramebuffer` types (and `VbeModeInfo::parse`,
`VesaFramebuffer::open` / `from_mode_info`) are re-exported so the driver
host can decode a boot-supplied block and construct an instance; the host
reaches it only through the `Display` trait afterwards.
