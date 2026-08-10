# Display drivers

Display drivers present a single linear pixel surface to the
compositor (`userland/gui/wm`). They implement
[`tairix_abi::driver::display::Display`](../abi/driver_traits.md) and
are loaded as user-space drivers; compositing, damage tracking, and
GPU acceleration live above this trait, not inside it.

## Class trait

`Display` is intentionally minimal — three methods:

| Method           | Purpose                                  | Capability gate          |
|------------------|------------------------------------------|--------------------------|
| `mode_info`      | report `DisplayMode { width_px, height_px, stride_bytes, format }` | `DriverHandle` ownership |
| `present`        | copy a fully-rendered frame to the surface | `DriverHandle` ownership |
| `present_region` | present a full frame of which only a `DamageRect` changed | `DriverHandle` ownership |

Pixel encodings are `DisplayFormat::Rgba8888` and
`DisplayFormat::Bgra8888` (4 bytes per pixel). Per `AGENTS.md` §2.9 the
trait never panics: a frame shorter than `stride_bytes * height_px`
maps to `DriverError::BufferTooSmall`.

`present_region` is the damage-aware present (`plans/DISPLAY.md` D7b):
`frame` still carries the whole surface, and `damage` names the
rectangle that changed since the previous present, validated against
the active mode (`DamageRect::validate_in` — non-empty, wholly
on-surface, overflow-checked) before any pixel access. The trait
supplies a full-blit default so every driver stays correct unchanged;
a driver whose scan-out path is a copy overrides it to blit only the
touched scanline spans (the framebuffer driver does). The WM
compositor threads its composited damage bounds through this method,
so a small screen update costs a small copy end to end.

### The present right follows the live seat lease

Every present/flip path is additionally gated on the presenting
client's *current* seat lease (`plans/DISPLAY.md` D4): at `open` a
driver captures its host's `DriverHost::seat_gate()` — a
`tairix_abi::driver::display::SeatGate` the kernel bound to the
client's `SeatLease` — and consults it at the **top** of `present` (and
`present_layers`), before any validation or surface access. A client
whose lease was revoked is refused with the distinct
`DriverError::SeatRevoked` even though its framebuffer mapping
(`CAP_MMIO_MAP`) still exists; any other dead handle is
`DriverError::PermissionDenied`, and the refused frame never touches
scan-out. A host with no seat wired (headless, boot bring-up, unit
seams) exposes no gate and the present proceeds ungated — there is no
lease to derive the right from. See
[the seat model](../desktop/seat.md) for the ownership side.

### Optional hardware acceleration

A driver whose hardware can composite a stack of planes itself also
implements `AcceleratedDisplay: Display` — the GPU-accelerated path
(`AGENTS.md` §10):

| Method           | Purpose                                            |
|------------------|----------------------------------------------------|
| `accel_caps`     | report `AccelCaps { max_layers, max_width_px, max_height_px, per_layer_opacity }` |
| `present_layers` | composite a back-to-front `AccelLayer` stack and scan it out |

Each `AccelLayer` carries premultiplied-alpha source pixels in the
display's active format plus a destination origin and a per-layer
opacity; the engine blends them and scans the result out, so the host
never composites the whole screen in software. The software
`Display::present` path is always the mandatory fallback: the
compositor (`Compositor::present_accelerated`) drops back to it when the
scene exceeds the reported `AccelCaps` (too many layers, or a layer
larger than the engine can source), so a hardware frame is never
partial (§2.9).

## Shipped drivers

| Driver       | Crate                                | Surface source                            | Stage 4 status        |
|--------------|--------------------------------------|-------------------------------------------|------------------------|
| framebuffer  | `tairix-drv-display-framebuffer` (Run process over `tairix_display::Framebuffer`) | firmware linear framebuffer (GOP / Pi mailbox / `ramfb`) | host-side tests + riscv64 & aarch64 `ramfb` QEMU verticals + wasm32 browser-canvas vertical |
| vesa         | `tairix-drv-display-vesa`            | x86_64 VBE linear framebuffer (`ModeInfoBlock`) | host-side tests + x86_64 `ramfb` QEMU vertical |
| rpi_hvs      | `tairix-drv-display-rpi-hvs`         | Raspberry Pi VideoCore HVS plane compositor (`AcceleratedDisplay`) | host-side tests |

The two display drivers are deliberate siblings (`AGENTS.md` §2.2
carve-out), not duplicates: `vesa` owns the VBE-specific decode, while
the framebuffer path consumes an already-parsed geometry record.

### `tairix-drv-display-framebuffer`

The framebuffer display service copies a presented frame into a
firmware-provided linear surface. Its crate is **bin-only** — the `Run`
entry point of the `/System/Drivers/` bundle `devmgr` autoloads when a
display node carrying a `HwResourceKind::Framebuffer` resource is
discovered — and it holds no device logic of its own: the
linear-surface engine is `tairix_display::Framebuffer` and the
protocol engine is `tairix_display::DisplayServer` (`lib/display`, one
shared definition, `plans/DISPLAY.md` D7b). Neither programs a display
controller; the service resolves its surface's `(phys_base, mode)`
fail-closed from its kernel-issued device-resource grants
(`sole_framebuffer`) and never scans out a guessed geometry.

`Framebuffer::open` validates the geometry and maps exactly
`stride_bytes * height_px` bytes through the host's `MmioMapper`,
which enforces `CAP_MMIO_MAP`. The framebuffer is therefore reached
only through a kernel-installed mapping, never through a pointer the
service synthesises (`AGENTS.md` §4 — no ambient authority). `present`
is byte-preserving: it copies the caller's frame verbatim into the
mapped window, bounds-checked at every write; `present_region` blits
only the validated damage rectangle.

The hardware-tree framebuffer resource also carries its discovered CPU
memory policy. QEMU `ramfb` scans ordinary coherent guest RAM and therefore
stays write-back cacheable. A true CPU-written display aperture requests the
separate write-combining page attribute; a port that cannot encode WC refuses
that mapping rather than silently substituting Device or WB memory. On
aarch64, WC is Normal Non-Cacheable and remains distinct from bidirectional
coherent DMA in the software PTE metadata. The HVS software fallback and its
plane uploads use one bounds-checked bulk transfer rather than millions of
individually checked volatile word stores.

No current backend advertises Linux-style y-wrap. Linux enables y-wrap only
when the driver explicitly reports hardware wrap and the visible and virtual
heights align to text rows. QEMU `ramfb` has no wrap primitive, and the Pi
firmware mailbox exposes virtual panning but does not prove wrap-at-end
semantics; treating either as wrap could scan beyond the allocation. The
retained console grid therefore keeps its correct coalesced damage repaint
until a discovered backend can guarantee bounded pan or true wrap.

The service binds the reserved `DISPLAY_ENDPOINT` (its manifest's
`CAP_IPC_BIND_PRIVILEGED` — a squatter cannot intercept presents) and
serves the `lib/display` protocol from a wait-set park: every request
— `Query` included — is gated on the in-flight caller's live seat
lease through `call_peer_seat`, a `Configure` maps the client's
`shm_grant`ed frame region once (sized from the kernel's own record of
the region length via `shm_map`'s `len_out`, never the client's
claimed geometry), and a `Present` blits by frame index — zero frame
bytes ever cross the IPC.

Lifecycle: the signed-bundle load gate clears `CAP_DRV_LOAD`;
`Framebuffer::open` maps the surface; dropping the `Framebuffer`
releases the window (unload); calling `open` again reloads. The engine
and its unit tests live in `lib/display`
(`lib/display/tests/framebuffer.rs`); the QEMU verticals below drive
that same shared engine.

#### The client side of a present

`lib/display` also owns the three pixel-exact decisions every program
that presents has to get right, in `tairix_display::scanout`: how many
bytes a frame is for a given mode (`scanout_len`, refusing a zero
extent, a stride too short for one scanline, or a size that overflows),
which byte order the mode's format wants (`ChannelOrder::for_format`,
refusing a format it has no software encoding for rather than guessing
and rendering the screen in false colour), and whether a damage
rectangle is a sub-region or the whole frame (`sub_screen_damage`,
falling back to a full present rather than presenting a wrong region).

They live here because getting any of them wrong is visible on the
whole screen, and two programs need them: the compositing window
manager, and the graphical login screen — which may not depend on the
window manager. What is deliberately *not* shared is the composition
itself: a compositor blending many windows through a back buffer and a
login screen blitting one surface are different loops, and both encode
their result through the one `ChannelOrder::encode`.

A composite loop encodes a whole row span at once through
`ChannelOrder::encode_run` — that same per-pixel encoder walked four
output bytes at a time, not a second spelling of the channel order. It
writes `min(pixels.len(), out.len() / 4)` pixels and returns that count,
so a short frame slice truncates rather than panicking or overrunning, a
longer one keeps its tail, and a trailing group of fewer than four bytes
is never half-written; the window manager passes one row span of its
back buffer with the frame bytes for that same span and checks the count
against the pixels it offered. There is no bulk-copy shortcut for the
order that already matches the pixel in memory: `tairix_raster::Pixel`
carries no layout guarantee to copy through, and a matching order is
already a four-byte move per pixel with no shuffle left to remove.

#### QEMU integration vertical

`tests/integration/framebuffer_display_qemu_riscv64`
(`tairix-test-framebuffer-display-qemu-riscv64`, enrolled in `cargo
xtask test --qemu`) drives the driver against a **real** emulated
framebuffer on the riscv64 `virt` board, closing the `load → use →
unload → reload` loop for a display driver.

The boot hand-off is synthesised the way QEMU exposes one: the test
harness attaches a QEMU `ramfb` device (`-device ramfb`) and programs
a static, page-aligned guest-RAM scan-out surface into it over the
`fw_cfg` MMIO DMA interface (find `etc/ramfb` in the file directory,
DMA-write the big-endian `RAMFBCfg`). The resulting geometry is the
`FramebufferConfig` boot hand-off. The harness then loads the signed
framebuffer `.rxe` through `tairix_drvhost::Host` (the §8 load gate)
and, for the "use" step, maps the surface through the
capability-gated `tairix_kernel_virtio::KernelMmioMapper` — the same
real kernel MMIO-map facility the bus drivers use — and calls
`present`. A second window mapped over the same physical range reads
the pixels back to confirm they reached the scan-out memory QEMU
consumes, before and after the reload. The `ramfb`/`fw_cfg` bring-up
is test-harness code, mirroring how the virtio verticals own their
PLIC/trap bring-up rather than placing it in the production kernel.
The `fw_cfg` DMA protocol itself lives once in the shared
`lib/fwcfg` crate (`tairix-fwcfg`, which also serves the aarch64
framebuffer boot console's ramfb path); this vertical supplies only
the riscv64 MMIO transport (`AGENTS.md` §2.2).

`tests/integration/framebuffer_display_qemu_aarch64`
(`tairix-test-framebuffer-display-qemu-aarch64`, enrolled in `cargo
xtask test --qemu`) is the aarch64 `virt`-board sibling of the riscv64
vertical, driving the same driver against a **real** emulated `ramfb`
framebuffer over the EL1/GICv2 path. It reuses the shared aarch64
bring-up (`tairix-test-virtio-qemu-support`'s FP-enable + 2 GiB identity
MMU + EL1 vectors) and the **same** shared `fw_cfg` MMIO transport
(`tairix-fwcfg`'s `MmioDma`) the riscv64 vertical uses — the two
`virt` boards expose `fw_cfg` identically, so there is one transport,
not two (`AGENTS.md` §2.2). Because QEMU's aarch64 `-kernel <ELF>` path
passes no DTB pointer, the vertical embeds the canonical `virt` device
tree (dumped at build time) to discover the `fw_cfg` base. The driver
lifecycle, `ramfb` programming, and pixel read-back are otherwise the
riscv64 scenario's siblings.

`tests/integration/framebuffer_display_wasm32`
(`tairix-test-framebuffer-display-wasm32`, enrolled in `cargo xtask test
--wasm`) is the wasm32 sibling: it drives the **same** framebuffer driver
in a real headless browser, against a static RGBA8888 surface in WASM
linear memory. Because wasm32 has no MMU, the surface is mapped through a
capability-checked `WasmMmioMapper` — a bounds- and `CAP_MMIO_MAP`-gated
view of the one in-memory surface, the MMU-less analogue of
`KernelMmioMapper`. Each presented frame is read back two ways: through a
second independently-mapped window (the bytes reached linear memory) and
through the new `tairix_host_present_framebuffer` host import, which
paints the surface onto an HTML `<canvas>` and returns the count of
pixels that survived the canvas round-trip — so the vertical proves the
pixels reach a genuine display surface, not just memory. The signed
`.rxe` load gate and the `load → use → unload → reload` lifecycle are the
bare-metal verticals' siblings (`AGENTS.md` §2.2); the driver itself is
byte-for-byte the same. See `docs/src/platform/wasm32.md` for the host
loader and harness.

### `tairix-drv-display-vesa`

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

#### QEMU integration vertical

`tests/integration/vesa_display_qemu_x86_64`
(`tairix-test-vesa-qemu-x86-64`, enrolled in `cargo xtask test --qemu`)
is the x86_64 sibling of the framebuffer vertical: it drives the driver
against a **real** emulated framebuffer, closing the `load → use →
unload → reload` loop on x86_64.

The harness attaches a QEMU `ramfb` device (`-device ramfb`) and
programs a static, page-aligned guest-RAM scan-out surface into it over
the `fw_cfg` **IOport** DMA interface (registers `0x514`/`0x518`). It
then synthesises the bootloader-captured VBE `ModeInfoBlock` describing
that surface — the shape a real VBE mode query (`0x4F01`) would produce
— as the boot hand-off, loads the signed vesa `.rxe` through
`tairix_drvhost::Host` (the §8 load gate), and for the "use" step decodes
the block with `VesaFramebuffer::open`, maps the surface through the
capability-gated `tairix_kernel_virtio::KernelMmioMapper`, and calls
`present`. A second window mapped over the same physical range reads the
pixels back to confirm they reached the scan-out memory, before and
after the reload.

The `fw_cfg` DMA protocol lives once in the shared `lib/fwcfg` crate
(`tairix-fwcfg`); this vertical supplies only the x86_64 IOport
transport, the deliberate sibling of the riscv64 MMIO transport
(`AGENTS.md` §2.2).

### `tairix-drv-display-rpi-hvs`

The Raspberry Pi driver is the first to implement `AcceleratedDisplay`.
It exposes the VideoCore Hardware Video Scaler (HVS) — a fixed-function
plane compositor — so the window manager hands it the visible windows
as layers instead of a pre-composited frame.

The HVS composites by walking a *display list* (DLIST) of plane entries
held in a dedicated RAM. On each accelerated present the driver uploads
each layer's premultiplied pixels into a per-plane, GPU-visible source
buffer, builds one unity-scaled DLIST entry per layer (control word,
packed position, size, source **bus** pointer, pitch, alpha — modelled
on the VC4 HVS plane format, six 32-bit words plus an end marker),
writes the list into the HVS DLIST RAM, and arms the display channel
through its control register. Plane pointers are bus addresses: a
physical address is translated through the configured VideoCore bus
alias (default `0xC000_0000`) and bounds-checked against the 30-bit
aperture, failing closed rather than driving the engine off a bad
address.

Every region (scan-out, DLIST RAM, control register, per-plane buffers)
is discovered by the boot capability as an `HvsConfig` and mapped
through the host's `MmioMapper` under `CAP_MMIO_MAP` (`AGENTS.md` §4 —
no ambient authority). The driver also implements the plain `Display`
trait, so the software full-frame path remains the mandatory fallback.
Like the other display drivers it runs in user space and is exercised
by host-side tests against a multi-region mock `MmioMapper` that reads
back both the uploaded plane pixels and the encoded DLIST words.

#### Firmware framebuffer discovery (`tairix-vcmailbox`)

The scan-out surface itself comes from the Pi's GPU firmware, spoken to
over the BCM2711 **mailbox property channel**. That protocol lives once
in the shared `lib/vcmailbox` crate (`AGENTS.md` §2.2 — the aarch64
port's framebuffer boot console speaks it too): `FramebufferRequest::encode`
builds the tag message (set physical/virtual size, depth 32, pixel
order from the `DisplayFormat`, allocate at page alignment, get pitch);
`decode_framebuffer_response` validates the firmware's in-place answer
fail-closed — header code, per-tag response bits and lengths, exact
geometry echoes, pitch/size consistency — and `bus_to_arm_physical`
strips the 2-bit VideoCore bus alias, rejecting a zero, unaligned, or
out-of-aperture buffer. The crate also carries the display-size query
(`query_display_size`, the boot console's "is a display attached, and
at what resolution" probe). The validated answer feeds `RpiHvs::open`
as the `ScanoutConfig` (`ScanoutConfig::from_firmware`, which also
supplies the bus alias the DLIST pointers use).

The doorbell sits behind the crate's `MailboxTransport` seam:
`MmioMailbox` drives the real register block (`0xFE00_B880` on the
BCM2711, reached as a discovered `hwtree` resource through
`CAP_MMIO_MAP`, never a compiled-in constant) with a budget-bounded
poll that fails closed with `Timeout` instead of spinning forever. QEMU
models neither the VideoCore firmware nor an in-aperture RAM window
(the `virt` board's RAM begins at `0x4000_0000`, outside the 30-bit
VideoCore aperture), so the emulation artefact is the host-side full
chain — the crate's protocol-faithful `mock::MockFirmware` answers the
exchange and the decoded `ScanoutConfig` drives `RpiHvs::open` and
`present` (the `wiring_tests` full-chain test) — while the real
scan-out (HVS hardware, HDMI) is the `plans/PI.md` P7 metal acceptance
item.

#### Driver-host wiring (`wiring`)

The `wiring` module is the bring-up seam between the hardware tree and
the driver. The aarch64 `FdtDiscovery` emits the Pi's mailbox node
(`brcm,bcm2835-mbox`) with two resources: the discovered doorbell
window as a capability-gated MMIO resource and a `Dma` resource
requesting a one-page property-buffer carve bounded by the 30-bit
VideoCore aperture (`AGENTS.md` §18.1). The host satisfies the carve
and calls `wiring::open_discovered`, which checks `CAP_MMIO_MAP`, maps
the doorbell and the carve, translates the carve to a bus address
(`arm_physical_to_bus`, the exact fail-closed inverse of
`bus_to_arm_physical`), exchanges the framebuffer request over
`MmioMailbox`, and assembles the full `HvsConfig` — the firmware's
scan-out plus the host's `HvsRegions` (DLIST RAM, display-channel
control, plane carves) — for `RpiHvs::open`. `open_with_transport` is
the host-provable half below the doorbell; the wiring's fail-closed
paths (missing capability, missing mapper, out-of-aperture carve, a
silent firmware timing out after the doorbell was rung) are unit-tested
on the host.
