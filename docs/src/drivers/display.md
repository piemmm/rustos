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
| framebuffer  | `rustos-drv-display-framebuffer`     | firmware linear framebuffer (GOP / Pi mailbox / `ramfb`) | host-side tests only |

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

QEMU integration on a real surface depends on the kernel framebuffer
hand-off plumbing (a boot capability that publishes the firmware
`FramebufferConfig` plus a kernel `MmioMapper` over the surface),
which is not yet in the tree — the same prerequisite pattern the
block drivers' QEMU verticals waited on.
