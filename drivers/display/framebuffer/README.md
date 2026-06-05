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
| aarch64 `virt`        | QEMU `ramfb`                    | mock-host + QEMU vertical   |
| riscv64 `virt`        | QEMU `ramfb`                    | mock-host + QEMU vertical   |
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

12/12 host-side tests pass.

### QEMU integration vertical

`tests/integration/framebuffer_display_qemu_riscv64`
(`rustos-test-framebuffer-display-qemu-riscv64`, enrolled in `cargo
xtask test --qemu`) drives the driver against a **real** emulated
framebuffer on the riscv64 `virt` board. The test harness synthesises
the device with QEMU `ramfb`: it programs a static guest-RAM scan-out
surface into the device over the `fw_cfg` MMIO DMA interface, then
publishes the geometry as the `FramebufferConfig` boot hand-off. It
loads the signed framebuffer `.rxe` through `rustos_drvhost::Host`
(the §8 load gate) and drives `load → use → unload → reload`, where
"use" maps the surface through the capability-gated
`rustos_kernel_virtio::KernelMmioMapper` and `present`s a frame; a
second independently-mapped window reads the pixels back to confirm
they reached the scan-out memory QEMU consumes. See
`docs/src/drivers/display.md`.

`tests/integration/framebuffer_display_qemu_aarch64`
(`rustos-test-framebuffer-display-qemu-aarch64`, also enrolled) is the
aarch64 `virt`-board sibling: it drives the same driver over the
EL1/GICv2 path, reusing the shared aarch64 bring-up and the **same**
shared `fw_cfg` MMIO transport the riscv64 vertical uses (the two `virt`
boards expose `fw_cfg` identically — one transport, not two, `AGENTS.md`
§2.2), embedding the canonical `virt` device tree because QEMU's aarch64
`-kernel <ELF>` path passes no DTB pointer.

## Public surface

`AGENTS.md` §8 — the only public *function* is `register`. The
`Framebuffer` type and `FramebufferConfig` are re-exported so the
driver host can construct an instance; the host never reaches into the
type beyond the `Display` trait surface.
