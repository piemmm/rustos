# Display drivers

Display drivers present a single linear pixel surface to the
compositor (`userland/gui/wm`). They implement
[`rustos_abi::driver::display::Display`](../abi/driver_traits.md) and
are loaded as user-space drivers; compositing, damage tracking, and
GPU acceleration live above this trait, not inside it.

## Class trait

`Display` is intentionally minimal — two methods:

| Method      | Purpose                                  | Capability gate          |
|-------------|------------------------------------------|--------------------------|
| `mode_info` | report `DisplayMode { width_px, height_px, stride_bytes, format }` | `DriverHandle` ownership |
| `present`   | copy a fully-rendered frame to the surface | `DriverHandle` ownership |

Pixel encodings are `DisplayFormat::Rgba8888` and
`DisplayFormat::Bgra8888` (4 bytes per pixel). Per `AGENTS.md` §2.9 the
trait never panics: a frame shorter than `stride_bytes * height_px`
maps to `DriverError::BufferTooSmall`.

## Shipped drivers

| Driver       | Crate                                | Surface source                            | Stage 4 status        |
|--------------|--------------------------------------|-------------------------------------------|------------------------|
| framebuffer  | `rustos-drv-display-framebuffer`     | firmware linear framebuffer (GOP / Pi mailbox / `ramfb`) | host-side tests + riscv64 `ramfb` QEMU vertical |
| vesa         | `rustos-drv-display-vesa`            | x86_64 VBE linear framebuffer (`ModeInfoBlock`) | host-side tests only |

The two display drivers are deliberate siblings (`AGENTS.md` §2.2
carve-out), not duplicates: `vesa` owns the VBE-specific decode, while
`framebuffer` consumes an already-parsed geometry record.

### `rustos-drv-display-framebuffer`

The framebuffer driver copies a frame into a firmware-provided linear
surface. It does not program a display controller; the boot capability
discovers the surface and hands the driver a `FramebufferConfig`
(physical base, width, height, stride, format).

`Framebuffer::open` validates the geometry and maps exactly
`stride_bytes * height_px` bytes through the host's `MmioMapper`,
which enforces `CAP_MMIO_MAP`. The framebuffer is therefore reached
only through a kernel-installed mapping, never through a pointer the
driver synthesises (`AGENTS.md` §4 — no ambient authority). `present`
is byte-preserving: it copies the caller's frame verbatim into the
mapped window, bounds-checked at every write.

Lifecycle: `register` clears `CAP_DRV_LOAD`; `Framebuffer::open` maps
the surface; dropping the `Framebuffer` releases the window (unload);
calling `open` again reloads.

#### QEMU integration vertical

`tests/integration/framebuffer_display_qemu_riscv64`
(`rustos-test-framebuffer-display-qemu-riscv64`, enrolled in `cargo
xtask test --qemu`) drives the driver against a **real** emulated
framebuffer on the riscv64 `virt` board, closing the `load → use →
unload → reload` loop for a display driver.

The boot hand-off is synthesised the way QEMU exposes one: the test
harness attaches a QEMU `ramfb` device (`-device ramfb`) and programs
a static, page-aligned guest-RAM scan-out surface into it over the
`fw_cfg` MMIO DMA interface (find `etc/ramfb` in the file directory,
DMA-write the big-endian `RAMFBCfg`). The resulting geometry is the
`FramebufferConfig` boot hand-off. The harness then loads the signed
framebuffer `.rxe` through `rustos_drvhost::Host` (the §8 load gate)
and, for the "use" step, maps the surface through the
capability-gated `rustos_kernel_virtio::KernelMmioMapper` — the same
real kernel MMIO-map facility the bus drivers use — and calls
`present`. A second window mapped over the same physical range reads
the pixels back to confirm they reached the scan-out memory QEMU
consumes, before and after the reload. The `ramfb`/`fw_cfg` bring-up
is test-harness code, mirroring how the virtio verticals own their
PLIC/trap bring-up rather than placing it in the production kernel.

### `rustos-drv-display-vesa`

The VESA driver presents the linear framebuffer a VESA BIOS Extensions
(VBE) mode exposes on a legacy x86_64 PC. Because the kernel cannot
re-enter real mode to issue VBE BIOS calls, mode selection happens in
the bootloader; the boot stub captures the 256-byte VBE `ModeInfoBlock`
(VBE function `0x4F01`) for a 32-bpp direct-colour linear-framebuffer
mode and hands it to the driver host as a boot capability.

`VbeModeInfo::parse` decodes and validates that block, accepting only a
supported mode whose linear-framebuffer attribute is set
(`ModeAttributes` bits 0 and 7), the direct-colour memory model
(`MemoryModel == 6`), 32 bits per pixel with 8-bit channel masks, and a
channel layout that is either `Bgra8888` (red at bit 16) or `Rgba8888`
(red at bit 0). A zero `PhysBasePtr`, a stride too small for one
scanline, or any other layout fails closed (`DeviceFault` /
`LengthOutOfRange` / `Unsupported`), so the byte-preserving `present`
copy never mis-renders.

`VesaFramebuffer::open` then maps exactly `stride_bytes * height_px`
bytes at the reported `PhysBasePtr` through the host's `MmioMapper`
(which enforces `CAP_MMIO_MAP`); the framebuffer is reached only through
a kernel-installed mapping, never through a pointer the driver
synthesises from the block (`AGENTS.md` §4). Lifecycle and `present`
semantics match the framebuffer driver: `register` clears
`CAP_DRV_LOAD`, dropping the `VesaFramebuffer` releases the window
(unload), and calling `open` again reloads.

A QEMU integration vertical depends on the kernel publishing the
boot-captured `ModeInfoBlock` as a capability and exposing a
`MmioMapper` over the linear framebuffer — the same boot hand-off
prerequisite the framebuffer and block drivers' QEMU verticals waited
on.
