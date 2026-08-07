//! Framebuffer boot console: kernel log output on the attached display.
//!
//! Boot (and later) console messages default to the **video display**;
//! the UART is the fallback when no display exists (the user-facing output is the screen, the serial line is a debug
//! last resort). On the Raspberry Pi the display pipeline is owned by
//! the `VideoCore` firmware, so this module asks it for a scan-out
//! surface over the shared mailbox property-channel client
//! (`tairix_vcmailbox`) and renders the kernel log into that surface
//! with the shared, architecture-neutral framebuffer text-console engine
//! (`tairix_fbcon` — one terminal definition across every arch port).
//!
//! Bring-up runs **before the MMU is enabled** (`configure_from_fdt`
//! is an early-returning, `ranges`-aware walk like the console/GIC
//! discoveries): with the data caches still off, the CPU↔firmware
//! property exchange is coherent by construction, so no cache
//! maintenance is needed during discovery. After the MMU and caches
//! come on, every framebuffer write is followed by a data-cache clean
//! to the point of coherency (`clean_dcache_range`) so the HVS
//! scan-out (which reads physical SDRAM) sees the rendered pixels.
//!
//! On QEMU's `virt` board there is no firmware mailbox; when the tree
//! instead carries a `qemu,fw-cfg-mmio` node **and** QEMU was started
//! with `-device ramfb`, the console programs the `ramfb` scan-out
//! (over the shared `tairix_fwcfg` client) to a statically-reserved
//! guest-RAM surface and renders into that — the same renderer, glyph
//! atlas, and publication discipline as the mailbox path, only the
//! surface source differs.
//!
//! Fail closed: no mailbox node and no ramfb device, a detached
//! display (`0×0` size), or any failed/malformed firmware answer
//! leaves the video console unconfigured and the UART keeps the
//! console (`crate::serial` routes through `write_bytes` only when
//! `is_active` reports a configured surface).

use core::sync::atomic::{AtomicBool, Ordering};

use tairix_abi::driver::display::DisplayFormat;
use tairix_abi::hwtree::FramebufferMemory;
/// The framebuffer console's character cell, re-exported so the boot caller
/// can size and blank the grid buffers it leaks into [`attach_console`]
/// without naming `tairix_fbcon` directly.
pub use tairix_fbcon::Cell;
use tairix_fbcon::Geometry;
use tairix_fdt::Fdt;
use tairix_vcmailbox::{
    discover_framebuffer, query_display_size, FramebufferRequest, MailboxTransport,
};

/// Compatible string of the BCM283x/BCM2711 firmware mailbox doorbell.
const MAILBOX_COMPATIBLE: &[u8] = b"brcm,bcm2835-mbox";

/// A firmware mailbox doorbell located in a flattened device tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DiscoveredMailbox {
    /// CPU-physical MMIO base of the doorbell register block (the
    /// node's first `reg` entry, decoded with its parent bus's cell
    /// counts and translated through the ancestor buses' `ranges`).
    pub base: u64,
    /// Length in bytes of the register window.
    pub len: u64,
}

/// Find the `VideoCore` firmware mailbox doorbell in `fdt`.
///
/// The walk early-returns at the matched node
/// ([`crate::fdt::scan_translated`]), so it stays safe with the MMU
/// off. Returns `None` when the tree carries no mailbox (e.g. QEMU
/// `virt`) or the node's `reg` cannot be decoded/translated — the
/// caller then leaves the video console unconfigured (fail closed).
#[must_use]
pub fn find_mailbox(fdt: &Fdt<'_>) -> Option<DiscoveredMailbox> {
    crate::fdt::scan_translated(fdt, |node, levels, depth| {
        let compatible = node.property("compatible")?;
        if !compatible.iter_strings().any(|s| s == MAILBOX_COMPATIBLE) {
            return None;
        }
        let (base, len) = crate::fdt::translated_reg(node, depth, levels, 0)?;
        Some(DiscoveredMailbox { base, len })
    })
}

// --- Text geometry ---------------------------------------------------------
//
// The shared framebuffer text-console engine — the glyph atlas, palette,
// scrolling, and the `Geometry` / `TextConsole` / `DirtyBand` types — lives in
// `tairix_fbcon` so every arch port renders through one definition. This module
// keeps only the board-specific surface discovery below and threads its
// firmware-confirmed extents into `Geometry::for_display`.

/// Fixed scan-out width of the QEMU `virt` ramfb boot console — the
/// shared `tairix_fwcfg` console geometry, so the port programming the
/// device and every other consumer read one definition.
pub const RAMFB_WIDTH_PX: u32 = tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX;

/// Fixed scan-out height of the QEMU `virt` ramfb boot console.
pub const RAMFB_HEIGHT_PX: u32 = tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX;

/// Text geometry of the fixed-size ramfb surface.
///
/// The surface is tightly packed (stride == width), so this is a pure
/// function of the two constants above; host-testable next to
/// [`Geometry::for_display`].
#[must_use]
pub fn ramfb_geometry() -> Option<Geometry> {
    Geometry::for_display(RAMFB_WIDTH_PX, RAMFB_HEIGHT_PX, RAMFB_WIDTH_PX * 4)
}

// --- Firmware bring-up ------------------------------------------------------

/// A firmware-allocated scan-out surface ready to host the console.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ConfiguredFramebuffer {
    /// ARM-physical base of the surface (page-aligned, in SDRAM).
    pub phys_base: u64,
    /// Allocated surface length in bytes.
    pub len_bytes: u32,
    /// Validated text geometry for the surface.
    pub geometry: Geometry,
    /// Pixel encoding the firmware programmed the surface with — the
    /// format the framebuffer request asked for, confirmed by the
    /// firmware's acceptance of the allocation.
    pub format: DisplayFormat,
}

/// Probe the attached display and allocate a matching scan-out surface
/// over `transport`.
///
/// Asks the firmware for the display's native (EDID-derived) size and
/// requests a 32-bit surface at exactly that size, so the console is
/// pixel-for-pixel on whatever monitor is plugged in. Returns `None` —
/// leaving the UART as the console — when no display is attached
/// (`0×0`), or when any firmware answer fails validation (fail closed; the firmware is an external input).
pub fn bring_up(transport: &mut dyn MailboxTransport) -> Option<ConfiguredFramebuffer> {
    let size = query_display_size(transport).ok()?;
    if !size.is_attached() {
        return None;
    }
    let request = FramebufferRequest {
        width_px: size.width_px,
        height_px: size.height_px,
        format: DisplayFormat::Bgra8888,
    };
    let firmware = discover_framebuffer(transport, &request).ok()?;
    let phys_base = firmware.arm_physical_base().ok()?;
    let geometry =
        Geometry::for_display(firmware.width_px, firmware.height_px, firmware.pitch_bytes)?;
    Some(ConfiguredFramebuffer {
        phys_base,
        len_bytes: firmware.size_bytes,
        geometry,
        format: request.format,
    })
}

// --- Global console state ---------------------------------------------------

/// Whether a video console is configured and rendering.
///
/// Written once (release) by the boot CPU after `configure_from_fdt`
/// succeeds; every console write checks it (acquire) before taking the
/// render lock, so UART-only boards pay one load on the fast path.
static VIDEO_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether console output is routed to the video console.
///
/// `crate::serial` writes to the screen when this is `true` and falls
/// back to the UART when it is `false` (video first, serial last
/// resort). The log/debug line path additionally echoes to the UART in
/// debug builds even when this is `true` (`crate::serial::ConsoleWriter`).
#[must_use]
pub fn is_active() -> bool {
    VIDEO_ACTIVE.load(Ordering::Acquire)
}

/// What the boot audit line records about the video console bring-up.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DiscoveredVideo {
    /// CPU-physical base of the mailbox doorbell the exchange used —
    /// an MMIO fact the identity map's Device gigapage mask must cover.
    pub doorbell_base: u64,
    /// CPU-physical base of the firmware-allocated scan-out surface —
    /// RAM the renderer writes, so the identity map's RAM gigapage mask
    /// must cover it.
    pub fb_base: u64,
    /// Byte length of the firmware-allocated scan-out surface.
    pub fb_len_bytes: u64,
    /// Confirmed surface width in pixels.
    pub width_px: u32,
    /// Confirmed surface height in pixels.
    pub height_px: u32,
    /// Distance in bytes between the start of consecutive scanlines.
    pub stride_bytes: u32,
    /// Pixel encoding the surface was programmed with.
    pub format: DisplayFormat,
    /// CPU mapping policy the surface backing requires when a user-space
    /// display driver maps it. A framebuffer boot-console scan-out is
    /// always CPU-written and read back by a display engine that does not
    /// snoop the CPU caches (the Pi's `VideoCore` HVS, QEMU's `ramfb`
    /// scan-out), so it needs write-combining (Normal non-cacheable)
    /// memory to stay coherent without per-frame cache maintenance —
    /// never write-back cacheable, which strands the CPU's writes in the
    /// data cache and shows the display a stale, fragmented surface.
    pub memory: FramebufferMemory,
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use metal::{
    active_framebuffer_extent, attach_console, configure_from_fdt, reclaim_surface, set_visible,
    text_cell_count, text_grid, write_bytes, write_output_bytes,
};

/// Host stand-in for the freestanding writer: rendering needs the
/// firmware surface, so on the host this is inert (the renderer itself
/// is host-tested directly through [`tairix_fbcon::TextConsole`]).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn write_bytes(_bytes: &[u8]) {}

/// Host stand-in for the freestanding program-output writer: no firmware
/// surface exists, so rendering is inert on the host.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn write_output_bytes(_bytes: &[u8]) {}

/// Host stand-in for the freestanding `text_grid`: no firmware surface exists
/// on the host, so no video console is active and the grid is unknown.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
#[must_use]
pub fn text_grid() -> Option<tairix_abi::TerminalSize> {
    None
}

/// Host stand-in for the freestanding `text_cell_count`: no surface is
/// discovered on the host, so there is no grid to size.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
#[must_use]
pub fn text_cell_count() -> Option<usize> {
    None
}

/// Host stand-in for the freestanding `active_framebuffer_extent`: no surface
/// is discovered on the host, so there is no scan-out extent to report.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
#[must_use]
pub fn active_framebuffer_extent() -> Option<(u64, u64)> {
    None
}

/// Host stand-in for the freestanding `attach_console`: no surface exists on
/// the host, so attaching the cell grids is inert.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn attach_console(
    _main: &'static mut [tairix_fbcon::Cell],
    _alt: &'static mut [tairix_fbcon::Cell],
) {
}

/// Host stand-in for the freestanding `set_visible`: no surface exists on the
/// host, so there is nothing to hand over (the handover itself is host-tested
/// through [`tairix_fbcon::TextConsole`] and the kernel seat registry).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn set_visible(_visible: bool) {}

/// Host stand-in for the freestanding `reclaim_surface`: no surface exists on
/// the host, so a panic has nothing to take back.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn reclaim_surface() {}

/// The freestanding half: the firmware exchange, the identity-mapped
/// surface, and the cache maintenance. Target-only — every routine here
/// either touches MMIO/SDRAM through boot-identity addresses or issues
/// `aarch64` system instructions.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod metal {
    use core::arch::asm;
    use core::cell::UnsafeCell;
    use core::ptr::NonNull;
    use core::sync::atomic::Ordering;

    use tairix_abi::RegisterWindow;
    use tairix_fbcon::{Cell, TextConsole};
    use tairix_fdt::Fdt;
    use tairix_fwcfg::{FwCfg, MmioDma, RamfbConfig, DRM_FORMAT_XRGB8888};
    use tairix_sync::IrqSafeSpinLock;
    use tairix_vcmailbox::{
        arm_physical_to_bus, MmioMailbox, DEFAULT_BUS_ALIAS, DEFAULT_POLL_BUDGET,
        MAILBOX_REGS_LEN_BYTES, PROPERTY_LEN_BYTES,
    };

    use crate::irqmask::PortIrqControl;

    use super::{
        bring_up, find_mailbox, ramfb_geometry, DiscoveredMailbox, DiscoveredVideo, Geometry,
        VIDEO_ACTIVE,
    };

    /// The discovered surface and, once attached post-MMU, the renderer.
    ///
    /// The console is `None` between the pre-MMU discovery (which records the
    /// surface and its geometry) and [`attach_console`] (which, once the heap
    /// is usable, builds the [`TextConsole`] over the leaked cell grids and
    /// publishes [`VIDEO_ACTIVE`]). `geometry` is known from discovery so the
    /// caller can size those grids before attaching.
    struct VideoState {
        /// Identity-mapped base of the firmware surface.
        fb_base: usize,
        /// Surface length in pixels (`stride × height`).
        pixel_count: usize,
        /// The validated text geometry of the surface.
        geometry: Geometry,
        /// The renderer (geometry + cursor + cell grids), once attached.
        console: Option<TextConsole<'static>>,
    }

    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    enum WriteMode {
        Verbatim,
        ProgramOutput,
    }

    /// The video-console slot.
    ///
    /// A plain cell, not a lock, because the pre-MMU boot path must not
    /// execute atomic read-modify-write instructions (UNPREDICTABLE on
    /// MMU-off Device-typed memory — the constraint that orders the
    /// whole aarch64 boot, `plans/PI.md` P6c-2). Mutation discipline:
    /// the boot CPU writes it once, single-threaded, before publishing
    /// [`VIDEO_ACTIVE`] with a release store; afterwards every access
    /// holds [`RENDER_LOCK`] (post-MMU, where the lock's CAS is sound).
    struct VideoSlot(UnsafeCell<Option<VideoState>>);

    // SAFETY: cross-thread access is serialised by the discipline above
    // (single-threaded pre-publication writes; lock-held access after).
    unsafe impl Sync for VideoSlot {}

    static VIDEO: VideoSlot = VideoSlot(UnsafeCell::new(None));

    /// The single render lock every console write serialises on.
    ///
    /// Masking this CPU for the hold is what makes it safe against its own
    /// interrupt handlers: a handler that logs to the screen while the CPU it
    /// fired on was mid-render would otherwise spin on a lock its own
    /// interrupted mainline holds. It guards `()` rather than the state
    /// itself because that state is written pre-MMU, where an atomic
    /// read-modify-write is architecturally UNPREDICTABLE, so the slot cannot
    /// live inside a lock; the discipline on [`VideoSlot`] covers that window.
    static RENDER_LOCK: IrqSafeSpinLock<(), PortIrqControl> = IrqSafeSpinLock::new(());

    /// The DMA-visible mailbox property message, 16-byte aligned as the
    /// doorbell protocol requires.
    #[repr(align(16))]
    struct PropertyBuffer(UnsafeCell<[u8; PROPERTY_LEN_BYTES]>);

    // SAFETY: written only by the single-threaded boot CPU inside
    // `configure_from_fdt` (before SMP bring-up and before any other
    // user exists); never touched again.
    unsafe impl Sync for PropertyBuffer {}

    static PROPERTY_BUFFER: PropertyBuffer =
        PropertyBuffer(UnsafeCell::new([0; PROPERTY_LEN_BYTES]));

    /// Discover the board's display path in `fdt` and bring the
    /// framebuffer console up: the `VideoCore` firmware mailbox where
    /// the tree carries one (the Pi), else the QEMU `virt` `fw_cfg` /
    /// `ramfb` fallback.
    ///
    /// **Boot-CPU, pre-MMU only**: it must run before
    /// `enable_mmu_and_vectors` (with the data caches off the
    /// CPU↔firmware property exchange is coherent without cache
    /// maintenance, and the cell writes below need the single-threaded
    /// boot CPU) and before SMP bring-up. On success the console output
    /// switches to the screen ([`super::is_active`]); on any failure —
    /// no mailbox and no ramfb device, no attached display, a rejected
    /// or malformed firmware answer — the UART keeps the console (fail
    /// closed).
    pub fn configure_from_fdt(fdt: &Fdt<'_>) -> Option<DiscoveredVideo> {
        match find_mailbox(fdt) {
            Some(mailbox) => configure_mailbox(mailbox),
            None => configure_ramfb(fdt),
        }
    }

    /// Bring the Pi's mailbox-allocated framebuffer console up: probe
    /// the attached display over the firmware property channel and
    /// publish the firmware-allocated surface.
    fn configure_mailbox(mailbox: DiscoveredMailbox) -> Option<DiscoveredVideo> {
        if mailbox.len < MAILBOX_REGS_LEN_BYTES as u64 {
            return None;
        }
        // SAFETY: single-threaded boot CPU, pre-publication (see
        // `PropertyBuffer`): no other reference to the buffer exists.
        let buffer_ptr = unsafe { NonNull::new_unchecked(PROPERTY_BUFFER.0.get().cast::<u8>()) };
        let buffer_phys = buffer_ptr.as_ptr() as u64;
        let buffer_bus = arm_physical_to_bus(buffer_phys, DEFAULT_BUS_ALIAS).ok()?;
        let doorbell_ptr = NonNull::new(usize::try_from(mailbox.base).ok()? as *mut u8)?;
        // SAFETY: `mailbox.base` is the FDT-discovered, `ranges`-translated
        // CPU-physical doorbell window (boot runs identity-addressed), at
        // least `MAILBOX_REGS_LEN_BYTES` long (checked above); the buffer
        // pointer covers exactly `PROPERTY_LEN_BYTES` of the static above.
        // Both windows are accessed only through `RegisterWindow`'s checked
        // 32-bit accessors, and neither outlives this call.
        let regs = unsafe {
            RegisterWindow::from_mapping(mailbox.base, doorbell_ptr, MAILBOX_REGS_LEN_BYTES)
        };
        let buffer =
            unsafe { RegisterWindow::from_mapping(buffer_phys, buffer_ptr, PROPERTY_LEN_BYTES) };
        let mut transport = MmioMailbox::new(regs, buffer, buffer_bus, DEFAULT_POLL_BUDGET).ok()?;
        let configured = bring_up(&mut transport)?;

        let fb_base = usize::try_from(configured.phys_base).ok()?;
        // The firmware allocated `[fb_base, fb_base + len_bytes)`
        // page-aligned inside the validated `VideoCore` SDRAM aperture
        // (`bring_up` → `arm_physical_base`); `publish_console` checks
        // the pixel extent fits before touching it.
        publish_console(
            fb_base,
            u64::from(configured.len_bytes),
            configured.geometry,
            mailbox.base,
            configured.format,
        )
    }

    /// Pixels in the statically-reserved ramfb scan-out surface.
    const RAMFB_PIXEL_COUNT: usize =
        super::RAMFB_WIDTH_PX as usize * super::RAMFB_HEIGHT_PX as usize;

    /// The QEMU `virt` ramfb scan-out surface: `ramfb` scans guest RAM
    /// directly, so the kernel supplies the surface itself. A static
    /// keeps the pre-heap bring-up allocation-free; it is kernel BSS
    /// (zero-filled at load, not stored in the image), untouched on a
    /// board whose tree carries a firmware mailbox instead. Mutation
    /// discipline is `VideoSlot`'s: the boot CPU points the console at
    /// it once, pre-publication, and every later access holds the
    /// render lock.
    struct RamfbSurface(UnsafeCell<[u32; RAMFB_PIXEL_COUNT]>);

    // SAFETY: cross-thread access is serialised by the `VideoSlot`
    // discipline above (single-threaded pre-publication writes; render
    // lock afterwards).
    unsafe impl Sync for RamfbSurface {}

    static RAMFB_SURFACE: RamfbSurface = RamfbSurface(UnsafeCell::new([0; RAMFB_PIXEL_COUNT]));

    /// Bring the QEMU `virt` ramfb boot console up over `fw_cfg`.
    ///
    /// The fallback when the tree carries no firmware mailbox: locate
    /// the `qemu,fw-cfg-mmio` node, and — only if the `etc/ramfb` item
    /// exists (QEMU was started with `-device ramfb`) — point the
    /// device's scan-out at the statically-reserved surface and publish
    /// the console. Fail closed on any miss (no fw_cfg node, no ramfb
    /// device, a failed transfer): the UART keeps the console.
    fn configure_ramfb(fdt: &Fdt<'_>) -> Option<DiscoveredVideo> {
        let dma = MmioDma::from_dtb(fdt).ok()?;
        let doorbell_base = dma.base();
        let geometry = ramfb_geometry()?;
        let fb_base = RAMFB_SURFACE.0.get() as usize;
        let fb_len_bytes = (RAMFB_PIXEL_COUNT * 4) as u64;
        let fwcfg = FwCfg::new(dma);
        fwcfg
            .program_ramfb(&RamfbConfig {
                phys_base: fb_base as u64,
                drm_format: DRM_FORMAT_XRGB8888,
                flags: 0,
                width: geometry.width_px,
                height: geometry.height_px,
                stride: geometry.stride_px * 4,
            })
            .ok()?;
        // `DRM_FORMAT_XRGB8888` is little-endian packed `0xXXRRGGBB`, so
        // the in-memory byte order is B, G, R, X — the `Bgra8888` wire
        // format with the alpha byte ignored by the scan-out.
        publish_console(
            fb_base,
            fb_len_bytes,
            geometry,
            doorbell_base,
            super::DisplayFormat::Bgra8888,
        )
    }

    /// Opaque black, the background the surface is cleared to before the cell
    /// grids are attached (matches the renderer's default background so the
    /// pre-attach clear and the post-attach repaint agree).
    const FB_CLEAR_PIXEL: u32 = 0xFF00_0000;

    /// Validate the surface extent, clear it to a clean background, and record
    /// the discovered surface (the shared tail of both bring-up paths).
    ///
    /// The renderer is **not** built here: the cell grids it needs are leaked
    /// from the kernel heap, which is only usable once the identity MMU is on
    /// (atomic read-modify-write is UNPREDICTABLE on the MMU-off Device-typed
    /// memory the boot CPU runs, `plans/PI.md` P6c-2). The post-MMU
    /// [`attach_console`] builds the console and publishes [`VIDEO_ACTIVE`];
    /// until then the surface shows a clean background rather than firmware
    /// garbage.
    ///
    /// **Boot-CPU, pre-publication only** (`VideoSlot` discipline). The
    /// caller guarantees `[fb_base, fb_base + fb_len_bytes)` is
    /// identity-addressed RAM it exclusively owns for scan-out.
    fn publish_console(
        fb_base: usize,
        fb_len_bytes: u64,
        geometry: Geometry,
        doorbell_base: u64,
        format: super::DisplayFormat,
    ) -> Option<DiscoveredVideo> {
        let pixel_count = geometry.pixel_count();
        if u64::try_from(pixel_count.checked_mul(4)?).ok()? > fb_len_bytes {
            return None;
        }
        // SAFETY: the caller owns `[fb_base, fb_base + fb_len_bytes)` as
        // identity-addressed scan-out RAM, `pixel_count * 4 ≤ fb_len_bytes`
        // (checked above), and no other Rust reference aliases the surface
        // (the cell below is the only owner and is not yet published). The
        // caches are off pre-MMU, so the fill is coherent without a clean.
        let pixels = unsafe { core::slice::from_raw_parts_mut(fb_base as *mut u32, pixel_count) };
        pixels.fill(FB_CLEAR_PIXEL);
        // SAFETY: single-threaded boot CPU, pre-publication (see
        // `VideoSlot`): no concurrent access can exist yet.
        unsafe {
            *VIDEO.0.get() = Some(VideoState {
                fb_base,
                pixel_count,
                geometry,
                console: None,
            });
        }
        Some(DiscoveredVideo {
            doorbell_base,
            fb_base: fb_base as u64,
            fb_len_bytes,
            width_px: geometry.width_px,
            height_px: geometry.height_px,
            stride_bytes: geometry.stride_px * 4,
            format,
            // Both bring-up paths (the Pi `VideoCore` mailbox surface and
            // the QEMU `ramfb` surface) are linear scan-out RAM the CPU
            // writes and a non-snooping display engine reads, so both
            // require write-combining to stay coherent without per-frame
            // cache maintenance.
            memory: super::FramebufferMemory::WriteCombine,
        })
    }

    /// The cell-grid length (`columns × rows`) the discovered surface needs,
    /// so the post-MMU caller can size the `main`/`alt` buffers it leaks into
    /// [`attach_console`]. `None` when no surface was discovered (UART-only).
    ///
    /// Post-MMU only (the render lock's atomic CAS requires it).
    pub fn text_cell_count() -> Option<usize> {
        let _guard = RENDER_LOCK.lock();
        // SAFETY: post-MMU, render lock held; `VIDEO` was written pre-MMU by
        // the single-threaded boot CPU (program order makes it visible on the
        // same CPU). A shared borrow suffices; geometry is read, not mutated.
        let state = (unsafe { (*VIDEO.0.get()).as_ref() })?;
        Some(state.geometry.cell_count())
    }

    /// The physical extent `(base, len_bytes)` of the active scan-out
    /// surface, or `None` when no video console was discovered (UART-only).
    ///
    /// The boot path identity-maps the surface (`virtual == physical`), so
    /// `fb_base` is already the physical base; the length is the whole
    /// `stride × height` pixel span in bytes. The pre-boot Supervisor's
    /// `memtest` takeover reads it to keep the live surface out of the
    /// destructive whole-RAM sweep, so the progress display survives — the
    /// Pi's mailbox framebuffer sits in ordinary usable DRAM the sweep would
    /// otherwise overwrite.
    ///
    /// Post-MMU only (the render lock's atomic CAS requires it), which the
    /// takeover always is.
    pub fn active_framebuffer_extent() -> Option<(u64, u64)> {
        let _guard = RENDER_LOCK.lock();
        // SAFETY: post-MMU, render lock held; `VIDEO` was written pre-MMU by
        // the single-threaded boot CPU (visible in program order on the same
        // CPU). A shared borrow suffices; the surface facts are read only.
        let state = (unsafe { (*VIDEO.0.get()).as_ref() })?;
        let len = (state.pixel_count as u64).saturating_mul(4);
        Some((state.fb_base as u64, len))
    }

    /// Attach the borrowed cell grids to the discovered surface and activate
    /// the console: build the [`TextConsole`], clear the surface through it,
    /// and publish [`VIDEO_ACTIVE`] so console output switches to the screen.
    ///
    /// **Post-MMU only** (the render lock's atomic CAS requires it, and the
    /// caller leaks `main`/`alt` from the heap, unusable pre-MMU). The caller
    /// sizes each grid to [`text_cell_count`]. A call with no discovered
    /// surface is a no-op (UART keeps the console, fail closed).
    pub fn attach_console(main: &'static mut [Cell], alt: &'static mut [Cell]) {
        let _guard = RENDER_LOCK.lock();
        // SAFETY: post-MMU, render lock held; `VIDEO` was written pre-MMU by
        // the single-threaded boot CPU and is not yet published active.
        let Some(state) = (unsafe { (*VIDEO.0.get()).as_mut() }) else {
            return;
        };
        let (fb_base, pixel_count, geometry) = (state.fb_base, state.pixel_count, state.geometry);
        let mut console = TextConsole::new(geometry, main, alt);
        // SAFETY: `fb_base`/`pixel_count` describe the surface validated in
        // `publish_console`, identity-mapped RAM; the render lock makes this
        // the only live reference.
        let pixels = unsafe { core::slice::from_raw_parts_mut(fb_base as *mut u32, pixel_count) };
        let dirty = console.clear(pixels);
        if let Some((row_start, row_end)) = dirty {
            let stride_bytes = geometry.stride_px as usize * 4;
            clean_dcache_range(
                fb_base + row_start as usize * stride_bytes,
                (row_end - row_start) as usize * stride_bytes,
            );
        }
        state.console = Some(console);
        VIDEO_ACTIVE.store(true, Ordering::Release);
    }

    /// Render `bytes` onto the configured surface and clean the touched
    /// scanlines to the point of coherency so the scan-out engine sees
    /// them.
    ///
    /// Post-MMU only (callers reach it through `crate::serial`, which
    /// first logs after the MMU is on): the render lock's atomic CAS
    /// requires it. A call with no configured console is a no-op.
    pub fn write_bytes(bytes: &[u8]) {
        render_bytes(bytes, WriteMode::Verbatim);
    }

    /// Render program-output `bytes` onto the configured surface with the
    /// console line discipline's `LF` → `CR LF` translation.
    ///
    /// The full payload is parsed under one render-lock hold and repainted
    /// once, so a scrolling burst never turns each line into a framebuffer
    /// repaint. A call with no configured console is a no-op.
    pub fn write_output_bytes(bytes: &[u8]) {
        render_bytes(bytes, WriteMode::ProgramOutput);
    }

    /// Hand the scan-out surface between the text console and the graphical
    /// session holding the boot seat: `visible == false` gives it up (the
    /// console keeps interpreting output into its retained screen and paints
    /// nothing), `visible == true` takes it back and repaints the whole
    /// screen from that screen — including everything written while it was
    /// away.
    ///
    /// Waits for a concurrent write to finish, because the handover must
    /// *happen*: a skipped hide would leave the console painting over the
    /// session's frame. The wait is bounded by one console write.
    ///
    /// Idempotent in both directions, and a call with no configured console
    /// (a UART-only board) does nothing. Post-MMU only, like every other
    /// render entry point.
    pub fn set_visible(visible: bool) {
        let _guard = RENDER_LOCK.lock();
        apply_visibility(visible);
    }

    /// Take the surface back for a panic report, giving up if it is
    /// contended.
    ///
    /// Best-effort because the panicking CPU may itself hold the render lock
    /// — it panicked inside the console — and blocking there would hang the
    /// machine with no report at all, including on a debug build whose report
    /// would otherwise have reached the serial console untouched. Losing the
    /// repaint is the lesser failure.
    pub fn reclaim_surface() {
        let Some(_guard) = RENDER_LOCK.try_lock() else {
            return;
        };
        apply_visibility(true);
    }

    /// Apply a visibility change, repainting the screen when the surface is
    /// being taken back. The render lock **must** be held.
    fn apply_visibility(visible: bool) {
        // SAFETY: post-MMU, render lock held by the caller; `VIDEO` was
        // written pre-MMU by the single-threaded boot CPU (visible in program
        // order) and the held lock serialises this mutable access (see
        // `VideoSlot`).
        let Some(state) = (unsafe { (*VIDEO.0.get()).as_mut() }) else {
            return;
        };
        let (fb_base, pixel_count) = (state.fb_base, state.pixel_count);
        let Some(console) = state.console.as_mut() else {
            return;
        };
        if !visible {
            console.hide();
            return;
        }
        // SAFETY: `fb_base`/`pixel_count` describe the firmware surface
        // validated at configure time, identity-mapped RAM; the render lock
        // makes this the only live reference.
        let pixels = unsafe { core::slice::from_raw_parts_mut(fb_base as *mut u32, pixel_count) };
        if let Some((row_start, row_end)) = console.show(pixels) {
            let stride_bytes = console.geometry().stride_px as usize * 4;
            clean_dcache_range(
                fb_base + row_start as usize * stride_bytes,
                (row_end - row_start) as usize * stride_bytes,
            );
        }
    }

    fn render_bytes(bytes: &[u8], mode: WriteMode) {
        if !super::is_active() {
            return;
        }
        let _guard = RENDER_LOCK.lock();
        // SAFETY: `VIDEO_ACTIVE` was observed `true` (acquire), so the
        // boot CPU's release-published initialisation is visible, and
        // the held render lock serialises this mutable access (see
        // `VideoSlot`).
        let Some(state) = (unsafe { (*VIDEO.0.get()).as_mut() }) else {
            return;
        };
        let (fb_base, pixel_count) = (state.fb_base, state.pixel_count);
        let Some(console) = state.console.as_mut() else {
            return;
        };
        // A hidden console paints nothing, so no reference into the surface
        // is formed while a graphical session is writing it; the engine
        // still advances its retained screen, and `set_visible` paints the
        // result when the surface comes back.
        let pixels: &mut [u32] = if console.is_visible() {
            // SAFETY: `fb_base`/`pixel_count` describe the firmware surface
            // validated at configure time, identity-mapped RAM; the render
            // lock makes this the only live reference.
            unsafe { core::slice::from_raw_parts_mut(fb_base as *mut u32, pixel_count) }
        } else {
            &mut []
        };
        let dirty = match mode {
            WriteMode::Verbatim => console.write_bytes(pixels, bytes),
            WriteMode::ProgramOutput => console.write_output_bytes(pixels, bytes),
        };
        if let Some((row_start, row_end)) = dirty {
            let stride_bytes = console.geometry().stride_px as usize * 4;
            clean_dcache_range(
                fb_base + row_start as usize * stride_bytes,
                (row_end - row_start) as usize * stride_bytes,
            );
        }
    }

    /// The active framebuffer console's character-cell grid, when one is
    /// configured (`terminal_size` — P-C).
    ///
    /// Post-MMU only (the render lock's atomic CAS requires it). Returns
    /// [`None`] when no video console is active (a UART-only board), so the
    /// caller reports no size and the client applies its fallback. A grid so
    /// large a dimension overflows the `u16` wire field also yields [`None`]
    /// (fail closed) rather than a truncated size.
    pub fn text_grid() -> Option<tairix_abi::TerminalSize> {
        if !super::is_active() {
            return None;
        }
        let _guard = RENDER_LOCK.lock();
        // SAFETY: `VIDEO_ACTIVE` was observed `true` (acquire), so the boot
        // CPU's release-published initialisation is visible, and the held
        // render lock serialises this access (see `VideoSlot`). A shared
        // borrow suffices; the geometry is read, not mutated.
        let state = (unsafe { (*VIDEO.0.get()).as_ref() })?;
        let rows = u16::try_from(state.geometry.rows()).ok()?;
        let cols = u16::try_from(state.geometry.columns()).ok()?;
        tairix_abi::TerminalSize::new(rows, cols).ok()
    }

    /// Clean `[start, start + len)` from the data cache to the point of
    /// coherency, so a DMA reader (the HVS scan-out) observes the CPU's
    /// writes.
    fn clean_dcache_range(start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let ctr: u64;
        // SAFETY: reading the cache-type register is always permitted at
        // EL1 and has no side effects.
        unsafe {
            asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags));
        }
        // CTR_EL0.DminLine (bits 19:16): log2 of the smallest data-cache
        // line in 4-byte words.
        let line = 4usize << ((ctr >> 16) & 0xF);
        let end = start.saturating_add(len);
        let mut addr = start & !(line - 1);
        while addr < end {
            // SAFETY: `dc cvac` cleans the line containing `addr` to the
            // point of coherency; it faults on no address the kernel can
            // form and modifies no memory contents.
            unsafe {
                asm!("dc cvac, {0}", in(reg) addr, options(nostack, preserves_flags));
            }
            addr += line;
        }
        // SAFETY: a data synchronisation barrier completing the cleans
        // before the function returns.
        unsafe {
            asm!("dsb sy", options(nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use tairix_fdt::fixture::raspi_like_arm;
    use tairix_fdt::Fdt;
    use tairix_vcmailbox::mock::MockFirmware;

    use super::*;

    // --- Mailbox discovery -------------------------------------------

    #[test]
    fn finds_the_mailbox_in_a_raspi_tree() {
        // The fixture mirrors the real Pi 4 tree: the mailbox sits under
        // `/soc` at bus address `0x7E00_B880`, remapped by `ranges` to
        // CPU-physical `0xFE00_B880`.
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mailbox = find_mailbox(&fdt).expect("mailbox present");
        assert_eq!(mailbox.base, 0xfe00_b880);
        assert_eq!(mailbox.len, 0x40);
    }

    #[test]
    fn no_mailbox_in_a_mailboxless_tree_is_none() {
        // A virt-like tree (no `brcm,bcm2835-mbox` node) yields no
        // mailbox, so the video console stays unconfigured and the UART
        // keeps the console.
        let mut builder = tairix_fdt::fixture::DtbBuilder::new();
        builder.begin_node("");
        builder.begin_node("pl011@9000000");
        builder.prop_str("compatible", "arm,pl011");
        builder.end_node();
        builder.end_node();
        let blob = builder.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert!(find_mailbox(&fdt).is_none());
    }

    // --- Firmware bring-up -------------------------------------------

    /// A mock firmware whose answers are mutually consistent for a
    /// 1920×1080 display: pitch one scanline, surface one full frame.
    fn full_hd_firmware() -> MockFirmware {
        let mut firmware = MockFirmware::healthy();
        firmware.fb_pitch = 1920 * 4;
        firmware.fb_size = 1920 * 4 * 1080;
        firmware
    }

    #[test]
    fn bring_up_allocates_the_displays_native_mode() {
        let mut firmware = full_hd_firmware();
        let configured = bring_up(&mut firmware).expect("bring-up");
        assert_eq!(configured.phys_base, 0x1000_0000);
        assert_eq!(configured.len_bytes, 1920 * 4 * 1080);
        let geometry = configured.geometry;
        assert_eq!((geometry.width_px, geometry.height_px), (1920, 1080));
        assert_eq!(geometry.stride_px, 1920);
        assert_eq!(
            geometry.scale, 1,
            "1080p selects 1× glyphs of the 26-px cell"
        );
    }

    #[test]
    fn bring_up_fails_closed_with_no_display_attached() {
        let mut firmware = full_hd_firmware();
        (firmware.display_w, firmware.display_h) = (0, 0);
        assert!(bring_up(&mut firmware).is_none());
    }

    #[test]
    fn bring_up_fails_closed_on_an_inconsistent_answer() {
        // The display reports 1920×1080 but the allocate answer carries
        // the healthy mock's 640×480 pitch — narrower than a scanline.
        let mut firmware = MockFirmware::healthy();
        assert!(bring_up(&mut firmware).is_none());
    }

    // --- Geometry policy ---------------------------------------------

    #[test]
    fn ramfb_geometry_is_always_renderable() {
        let geometry = ramfb_geometry().expect("the fixed ramfb mode is renderable");
        assert_eq!(geometry.width_px, RAMFB_WIDTH_PX);
        assert_eq!(geometry.height_px, RAMFB_HEIGHT_PX);
        assert_eq!(geometry.stride_px, RAMFB_WIDTH_PX, "tightly packed");
        assert!(geometry.columns() > 0);
        assert!(geometry.rows() > 0);
        assert_eq!(
            geometry.scale, 1,
            "768 rows select 1× glyphs of the 26-px cell"
        );
    }
}
