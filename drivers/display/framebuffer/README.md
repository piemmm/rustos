# `rustos-drv-display-framebuffer` — linear framebuffer driver

Stage 4 deliverable. Implements `rustos_abi::driver::display::Display`
over a single firmware-provided linear pixel surface. The driver is
**platform-neutral**: it copies a fully-rendered frame into a linear
framebuffer whose physical base, geometry, and pixel encoding the boot
capability discovered and handed to the driver host.

## Supported hardware

| Platform              | Surface source                  | Stage 4 status            |
|-----------------------|---------------------------------|----------------------------|
| aarch64 (Raspberry Pi)| VideoCore mailbox framebuffer   | mock-host tests only       |
| riscv64 `virt`        | QEMU `ramfb`                    | mock-host tests only       |
| x86_64 (UEFI)         | GOP linear frame buffer         | mock-host tests only       |
| wasm32                | browser canvas (via host)       | mock-host tests only       |

The driver does not enumerate or program the display controller; it
consumes a `FramebufferConfig` the firmware/boot capability already
produced. Mode-setting on programmable controllers is a separate
driver class (see `gpu_virtio`).

### Pixel formats

`DisplayFormat::Rgba8888` and `DisplayFormat::Bgra8888` (both 4 bytes
per pixel). `present` is byte-preserving: it copies the caller's frame
verbatim, so the caller renders in whatever encoding `mode_info`
reports.

## Required capabilities

- `CAP_DRV_LOAD` at `register` time.
- `CAP_MMIO_MAP` at `Framebuffer::open` time — the framebuffer is
  device-visible memory and is reached only through the
  capability-gated `MmioMapper`, never through a pointer the driver
  synthesises itself (`AGENTS.md` §4).
- The `Display` methods (`mode_info`, `present`) are gated by
  ownership of the `DriverHandle` returned from `register`.

The driver runs in user space; it does **not** request
`CAP_DRV_KERNEL`.

## Lifecycle

`register` clears the load-time gate; `Framebuffer::open` maps the
surface; dropping the `Framebuffer` releases the window (the unload
step — the kernel reclaims the mapping). Reloading is calling
`Framebuffer::open` again, which the `unload_then_reload_presents_again`
test exercises.

## Test surface

`cargo test -p rustos-drv-display-framebuffer` exercises, against an
in-process mock `MmioMapper`:

- `register` capability gate.
- `open` reports the configured `DisplayMode`.
- `present` byte-fidelity into the mapped surface, including a
  surface whose length is not a multiple of four (the `u32` bulk path
  plus the byte tail).
- Short-frame rejection (`BufferTooSmall`) and oversized-frame
  truncation.
- `open` capability gates (`CAP_MMIO_MAP` on the host and at the
  mapper), absent mapper (`Unsupported`), and a region the platform
  cannot map (`Unsupported`).
- Degenerate-geometry rejection (`LengthOutOfRange`).
- Unload → reload round-trip.

12/12 host-side tests pass. A QEMU integration vertical depends on the
kernel framebuffer hand-off plumbing (a boot capability that publishes
the firmware `FramebufferConfig` and a `KernelMmioMapper` over the
surface), which is not yet in the tree; it is tracked as a follow-up
exactly as the virtio-blk QEMU verticals were before their kernel
prerequisites landed.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`Framebuffer` type and `FramebufferConfig` are re-exported so the
driver host can construct an instance; the host never reaches into the
type beyond the `Display` trait surface.
