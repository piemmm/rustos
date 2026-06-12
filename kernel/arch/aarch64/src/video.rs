//! Framebuffer boot console: kernel log output on the attached display.
//!
//! Boot (and later) console messages default to the **video display**;
//! the UART is the fallback when no display exists (`AGENTS.md` §10 —
//! the user-facing output is the screen, the serial line is a debug
//! last resort). On the Raspberry Pi the display pipeline is owned by
//! the `VideoCore` firmware, so this module asks it for a scan-out
//! surface over the shared mailbox property-channel client
//! (`rustos_vcmailbox`, `AGENTS.md` §2.2) and renders the kernel log
//! into that surface with the shared 5×7 glyph atlas
//! (`rustos_font::glyphs` — one font definition, §2.2).
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
//! Fail closed (`AGENTS.md` §2.9): no mailbox node, a detached display
//! (`0×0` size), or any failed/malformed firmware answer leaves the
//! video console unconfigured and the UART keeps the console
//! (`crate::serial` routes through `write_bytes` only when
//! `is_active` reports a configured surface). QEMU's `virt` board has
//! no mailbox node, so the existing UART-backed verticals are
//! unaffected.

use core::sync::atomic::{AtomicBool, Ordering};

use rustos_abi::driver::display::DisplayFormat;
use rustos_fdt::Fdt;
use rustos_font::glyphs;
use rustos_vcmailbox::{
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
/// caller then leaves the video console unconfigured (fail closed,
/// `AGENTS.md` §2.9).
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

// --- Text geometry and renderer -------------------------------------------

/// Glyph cell width in pixels at scale 1: the atlas glyph plus one
/// column of inter-character spacing.
const CELL_WIDTH: u32 = glyphs::GLYPH_WIDTH + 1;

/// Glyph cell height in pixels at scale 1: the atlas glyph plus one row
/// of inter-line spacing.
const CELL_HEIGHT: u32 = glyphs::GLYPH_HEIGHT + 1;

/// Foreground (text) colour: light grey, opaque (grey is symmetric in
/// both 32-bit channel orders, so the rendered text is correct whether
/// the firmware honoured BGRA or RGBA).
const FOREGROUND: u32 = 0xFFD8_D8D8;

/// Background colour: opaque black.
const BACKGROUND: u32 = 0xFF00_0000;

/// Largest glyph scale the policy selects.
///
/// Beyond 4× the 5×7 atlas looks blocky without gaining legibility, so
/// the policy caps there even on very tall displays.
const MAX_SCALE: u32 = 4;

/// Pixel rows of display height per unit of glyph scale.
///
/// `height / 360` keeps roughly 45 text rows on screen at every common
/// mode (480p → 1×, 720p → 2×, 1080p → 3×, 2160p → 4×): enough boot log
/// to read, large enough to read it on a TV across a room.
const ROWS_PER_SCALE: u32 = 360;

/// Validated framebuffer text geometry: the scan-out extents plus the
/// glyph scale the policy chose for them.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Geometry {
    /// Visible width in pixels.
    pub width_px: u32,
    /// Visible height in pixels.
    pub height_px: u32,
    /// Pixels (not bytes) between consecutive scanlines.
    pub stride_px: u32,
    /// Integer glyph scale (`1..=MAX_SCALE`).
    pub scale: u32,
}

impl Geometry {
    /// Derive the text geometry for a firmware-confirmed surface.
    ///
    /// Returns `None` when the surface cannot host even one glyph cell,
    /// the pitch is not whole pixels, or the pitch is narrower than a
    /// scanline — the caller leaves the video console unconfigured
    /// rather than rendering out of bounds (fail closed, `AGENTS.md`
    /// §2.9 / §5.4: the geometry is firmware input).
    #[must_use]
    pub fn for_display(width_px: u32, height_px: u32, pitch_bytes: u32) -> Option<Self> {
        if pitch_bytes % 4 != 0 {
            return None;
        }
        let stride_px = pitch_bytes / 4;
        if width_px == 0 || height_px == 0 || stride_px < width_px {
            return None;
        }
        let scale = (height_px / ROWS_PER_SCALE).clamp(1, MAX_SCALE);
        let geometry = Self {
            width_px,
            height_px,
            stride_px,
            scale,
        };
        (geometry.columns() != 0 && geometry.rows() != 0).then_some(geometry)
    }

    /// Text columns the surface holds.
    #[must_use]
    pub const fn columns(&self) -> u32 {
        self.width_px / (CELL_WIDTH * self.scale)
    }

    /// Text rows the surface holds.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.height_px / (CELL_HEIGHT * self.scale)
    }

    /// Pixel rows one text row occupies.
    const fn cell_height_px(&self) -> u32 {
        CELL_HEIGHT * self.scale
    }

    /// Pixel columns one text column occupies.
    const fn cell_width_px(&self) -> u32 {
        CELL_WIDTH * self.scale
    }

    /// Pixel count of the rendered band (`stride × height`), the slice
    /// length the renderer draws into.
    #[must_use]
    pub const fn pixel_count(&self) -> usize {
        self.stride_px as usize * self.height_px as usize
    }
}

/// The pixel-row band `[start, end)` a rendering call touched, so the
/// freestanding writer can clean exactly those scanlines to the point
/// of coherency.
type DirtyBand = (u32, u32);

/// Merge two optional dirty bands into their union.
fn merge_bands(a: Option<DirtyBand>, b: Option<DirtyBand>) -> Option<DirtyBand> {
    match (a, b) {
        (Some((a0, a1)), Some((b0, b1))) => Some((a0.min(b0), a1.max(b1))),
        (band, None) | (None, band) => band,
    }
}

/// Pixel height of one boot-progress beacon band (the freestanding
/// `boot_beacon_band`).
pub const BEACON_BAND_PX: u32 = 16;

/// Pixel-row range `[start, end)` of boot-progress beacon band `index`.
///
/// Bands stack **upward from the bottom edge** of the surface (band 0 is
/// the bottom-most), so they never collide with the boot-log text the
/// console renders from the top. Returns `None` — the beacon is skipped,
/// never drawn out of bounds — when the band does not fit the surface
/// (`AGENTS.md` §2.9).
#[must_use]
pub fn beacon_band_rows(geometry: &Geometry, index: u32) -> Option<(u32, u32)> {
    let end = geometry
        .height_px
        .checked_sub(index.checked_mul(BEACON_BAND_PX)?)?;
    let start = end.checked_sub(BEACON_BAND_PX)?;
    Some((start, end))
}

/// A fixed-grid text console rendering the shared 5×7 atlas
/// ([`rustos_font::glyphs`]) into a borrowed row-major `u32` pixel
/// buffer.
///
/// The grid is a **ring**: reaching the bottom row wraps the cursor to
/// the top and clears that row, rather than copying the whole surface
/// up one line — a scroll would re-write (and re-clean) megabytes per
/// log line on the boot path (`AGENTS.md` §2.16).
///
/// Pure CPU pixel arithmetic over a borrowed slice, so the renderer is
/// host-testable; the freestanding side wraps the firmware surface in a
/// slice and adds the cache maintenance.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct TextConsole {
    geometry: Geometry,
    column: u32,
    row: u32,
}

impl TextConsole {
    /// A console at the top-left of a `geometry`-sized surface.
    #[must_use]
    pub const fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            column: 0,
            row: 0,
        }
    }

    /// The validated geometry this console renders into.
    #[must_use]
    pub const fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// Clear the whole surface to the background and home the cursor,
    /// returning the dirty band (the full surface height).
    pub fn clear(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        for pixel in pixels.iter_mut() {
            *pixel = BACKGROUND;
        }
        self.column = 0;
        self.row = 0;
        Some((0, self.geometry.height_px))
    }

    /// Render one byte, returning the pixel-row band it touched.
    ///
    /// `\n` advances to the next (cleared) row, `\r` returns to column
    /// zero, and any byte outside the printable-ASCII atlas renders the
    /// `?` glyph rather than being silently dropped.
    pub fn write_byte(&mut self, pixels: &mut [u32], byte: u8) -> Option<DirtyBand> {
        match byte {
            b'\n' => Some(self.next_row(pixels)),
            b'\r' => {
                self.column = 0;
                None
            }
            _ => {
                let printable = if (0x20..=0x7E).contains(&byte) {
                    byte
                } else {
                    b'?'
                };
                let mut dirty = Some(self.blit_glyph(pixels, printable));
                self.column += 1;
                if self.column == self.geometry.columns() {
                    dirty = merge_bands(dirty, Some(self.next_row(pixels)));
                }
                dirty
            }
        }
    }

    /// Advance to the next text row, wrapping ring-style and clearing
    /// the row the cursor lands on.
    fn next_row(&mut self, pixels: &mut [u32]) -> DirtyBand {
        self.column = 0;
        self.row = (self.row + 1) % self.geometry.rows();
        self.clear_text_row(pixels, self.row)
    }

    /// Fill one text row with the background colour.
    fn clear_text_row(&self, pixels: &mut [u32], row: u32) -> DirtyBand {
        let geometry = &self.geometry;
        let y0 = row * geometry.cell_height_px();
        let y1 = y0 + geometry.cell_height_px();
        for y in y0..y1 {
            let start = y as usize * geometry.stride_px as usize;
            let end = start + geometry.width_px as usize;
            if let Some(span) = pixels.get_mut(start..end) {
                for pixel in span {
                    *pixel = BACKGROUND;
                }
            }
        }
        (y0, y1)
    }

    /// Blit one printable-ASCII glyph cell at the cursor.
    fn blit_glyph(&self, pixels: &mut [u32], byte: u8) -> DirtyBand {
        let geometry = &self.geometry;
        let glyph = &glyphs::GLYPHS[(byte - glyphs::FIRST_CHAR as u8) as usize];
        let x0 = self.column * geometry.cell_width_px();
        let y0 = self.row * geometry.cell_height_px();
        for cell_y in 0..CELL_HEIGHT {
            let bits = if cell_y < glyphs::GLYPH_HEIGHT {
                glyph[cell_y as usize]
            } else {
                0
            };
            for cell_x in 0..CELL_WIDTH {
                let lit = cell_x < glyphs::GLYPH_WIDTH
                    && bits & (1 << (glyphs::GLYPH_WIDTH - 1 - cell_x)) != 0;
                let colour = if lit { FOREGROUND } else { BACKGROUND };
                for sub_y in 0..geometry.scale {
                    let y = (y0 + cell_y * geometry.scale + sub_y) as usize;
                    let x = (x0 + cell_x * geometry.scale) as usize;
                    let start = y * geometry.stride_px as usize + x;
                    if let Some(span) = pixels.get_mut(start..start + geometry.scale as usize) {
                        for pixel in span {
                            *pixel = colour;
                        }
                    }
                }
            }
        }
        (y0, y0 + geometry.cell_height_px())
    }
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
}

/// Probe the attached display and allocate a matching scan-out surface
/// over `transport`.
///
/// Asks the firmware for the display's native (EDID-derived) size and
/// requests a 32-bit surface at exactly that size, so the console is
/// pixel-for-pixel on whatever monitor is plugged in. Returns `None` —
/// leaving the UART as the console — when no display is attached
/// (`0×0`), or when any firmware answer fails validation (fail closed,
/// `AGENTS.md` §2.9; the firmware is an external input, §5.4).
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
/// resort).
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
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub use metal::{boot_beacon_band, configure_from_fdt, write_bytes};

/// Host stand-in for the freestanding writer: rendering needs the
/// firmware surface, so on the host this is inert (the renderer itself
/// is host-tested directly through [`TextConsole`]).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn write_bytes(_bytes: &[u8]) {}

/// The freestanding half: the firmware exchange, the identity-mapped
/// surface, and the cache maintenance. Target-only — every routine here
/// either touches MMIO/SDRAM through boot-identity addresses or issues
/// `aarch64` system instructions.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod metal {
    use core::arch::asm;
    use core::cell::UnsafeCell;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicBool, Ordering};

    use rustos_abi::RegisterWindow;
    use rustos_fdt::Fdt;
    use rustos_vcmailbox::{
        arm_physical_to_bus, MmioMailbox, DEFAULT_BUS_ALIAS, DEFAULT_POLL_BUDGET,
        MAILBOX_REGS_LEN_BYTES, PROPERTY_LEN_BYTES,
    };

    use super::{
        bring_up, find_mailbox, merge_bands, DirtyBand, DiscoveredVideo, TextConsole, VIDEO_ACTIVE,
    };

    /// Serialises post-MMU rendering (cursor + surface writes) across
    /// CPUs, masking IRQ+FIQ for the critical section so a log write
    /// from an interrupt handler on the holding CPU cannot deadlock
    /// (`AGENTS.md` §23.2).
    ///
    /// A minimal DAIF-masking spinlock private to this module —
    /// deliberately not `rustos_sync::IrqSafeSpinLock`, a documented
    /// §2.2 carve-out: the minimal aarch64 QEMU test binaries link no
    /// global allocator, and cargo feature unification across the
    /// single `--target aarch64-unknown-none` build of the test matrix
    /// compiles `lib/sync`'s alloc-backed `epoch` module into every
    /// graph naming that crate, which would force an allocator into
    /// those binaries.
    struct RenderLock {
        locked: AtomicBool,
    }

    /// Holds [`RenderLock`]; releasing restores the saved DAIF state.
    struct RenderGuard<'a> {
        lock: &'a RenderLock,
        daif: u64,
    }

    impl RenderLock {
        const fn new() -> Self {
            Self {
                locked: AtomicBool::new(false),
            }
        }

        fn lock(&self) -> RenderGuard<'_> {
            let daif: u64;
            // SAFETY: reading DAIF and setting its I/F mask bits
            // (`msr daifset, #3`) is always permitted at EL1 and
            // touches no memory; masking *before* acquiring makes the
            // hold-time interrupt-free, and an already-masked state
            // round-trips unchanged (reentrant).
            unsafe {
                asm!(
                    "mrs {0}, daif",
                    "msr daifset, #3",
                    out(reg) daif,
                    options(nomem, nostack, preserves_flags)
                );
            }
            while self.locked.swap(true, Ordering::Acquire) {
                core::hint::spin_loop();
            }
            RenderGuard { lock: self, daif }
        }
    }

    impl Drop for RenderGuard<'_> {
        fn drop(&mut self) {
            self.lock.locked.store(false, Ordering::Release);
            // SAFETY: writes back the exact DAIF value captured by
            // `lock` on this CPU, restoring the prior interrupt mask.
            unsafe {
                asm!(
                    "msr daif, {0}",
                    in(reg) self.daif,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
    }

    /// The configured surface and cursor.
    struct VideoState {
        /// Identity-mapped base of the firmware surface.
        fb_base: usize,
        /// Surface length in pixels (`stride × height`).
        pixel_count: usize,
        /// The renderer (geometry + cursor).
        console: TextConsole,
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
    static RENDER_LOCK: RenderLock = RenderLock::new();

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

    /// Discover the firmware mailbox in `fdt`, probe the display, and
    /// bring the framebuffer console up.
    ///
    /// **Boot-CPU, pre-MMU only**: it must run before
    /// `enable_mmu_and_vectors` (with the data caches off the
    /// CPU↔firmware property exchange is coherent without cache
    /// maintenance, and the cell writes below need the single-threaded
    /// boot CPU) and before SMP bring-up. On success the console output
    /// switches to the screen ([`super::is_active`]); on any failure —
    /// no mailbox node (QEMU `virt`), no attached display, a rejected
    /// or malformed firmware answer — the UART keeps the console (fail
    /// closed, `AGENTS.md` §2.9).
    pub fn configure_from_fdt(fdt: &Fdt<'_>) -> Option<DiscoveredVideo> {
        let mailbox = find_mailbox(fdt)?;
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
        let pixel_count = configured.geometry.pixel_count();
        if pixel_count.checked_mul(4)? > configured.len_bytes as usize {
            return None;
        }
        let mut console = TextConsole::new(configured.geometry);
        // SAFETY: the firmware allocated `[fb_base, fb_base + len_bytes)`
        // page-aligned inside the validated `VideoCore` SDRAM aperture
        // (`bring_up` → `arm_physical_base`), `pixel_count * 4 ≤
        // len_bytes` (checked above), and boot runs identity-addressed;
        // no other Rust reference aliases the surface (the cell below is
        // the only owner and is not yet published).
        let pixels = unsafe { core::slice::from_raw_parts_mut(fb_base as *mut u32, pixel_count) };
        console.clear(pixels);
        // SAFETY: single-threaded boot CPU, pre-publication (see
        // `VideoSlot`): no concurrent access can exist yet.
        unsafe {
            *VIDEO.0.get() = Some(VideoState {
                fb_base,
                pixel_count,
                console,
            });
        }
        VIDEO_ACTIVE.store(true, Ordering::Release);
        Some(DiscoveredVideo {
            doorbell_base: mailbox.base,
            fb_base: configured.phys_base,
            fb_len_bytes: u64::from(configured.len_bytes),
            width_px: configured.geometry.width_px,
            height_px: configured.geometry.height_px,
        })
    }

    /// Render `bytes` onto the configured surface and clean the touched
    /// scanlines to the point of coherency so the scan-out engine sees
    /// them.
    ///
    /// Post-MMU only (callers reach it through `crate::serial`, which
    /// first logs after the MMU is on): the render lock's atomic CAS
    /// requires it. A call with no configured console is a no-op.
    pub fn write_bytes(bytes: &[u8]) {
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
        // SAFETY: `fb_base`/`pixel_count` describe the firmware surface
        // validated at configure time, identity-mapped RAM; the render
        // lock makes this the only live reference.
        let pixels = unsafe {
            core::slice::from_raw_parts_mut(state.fb_base as *mut u32, state.pixel_count)
        };
        let mut dirty: Option<DirtyBand> = None;
        for &byte in bytes {
            dirty = merge_bands(dirty, state.console.write_byte(pixels, byte));
        }
        if let Some((row_start, row_end)) = dirty {
            let stride_bytes = state.console.geometry().stride_px as usize * 4;
            clean_dcache_range(
                state.fb_base + row_start as usize * stride_bytes,
                (row_end - row_start) as usize * stride_bytes,
            );
        }
    }

    /// Paint boot-progress beacon band `index` in `colour` and clean the
    /// touched scanlines to the point of coherency, so the band is
    /// scan-out-visible whether the data cache is on yet or not (pre-MMU
    /// the writes are uncached and the cleans find no lines — harmless).
    ///
    /// Bands stack upward from the bottom edge
    /// ([`super::beacon_band_rows`]); the count of bands on screen *is*
    /// the boot progress (`docs/src/platform/aarch64.md`, "Boot progress
    /// beacon"). A call with no configured console, or a band that does
    /// not fit the surface, is a no-op (fail closed, `AGENTS.md` §2.9).
    ///
    /// # Safety
    ///
    /// Boot-CPU, single-threaded, pre-SMP only: it renders **without**
    /// taking [`RENDER_LOCK`], because the beacon must also work on the
    /// MMU-off boot path where the lock's atomic read-modify-write is
    /// UNPREDICTABLE (the constraint that orders the whole aarch64 boot,
    /// `plans/PI.md` P6c-2). The caller must guarantee no other CPU or
    /// interrupt handler touches the surface concurrently — true for the
    /// boot pipeline before SMP bring-up with interrupts masked.
    pub unsafe fn boot_beacon_band(index: u32, colour: u32) {
        if !super::is_active() {
            return;
        }
        // SAFETY: `VIDEO_ACTIVE` was observed `true` (acquire), so the
        // boot CPU's release-published initialisation is visible, and
        // the caller's pre-SMP single-threaded contract stands in for
        // the render lock (see the function's safety contract).
        let Some(state) = (unsafe { (*VIDEO.0.get()).as_mut() }) else {
            return;
        };
        let geometry = *state.console.geometry();
        let Some((row_start, row_end)) = super::beacon_band_rows(&geometry, index) else {
            return;
        };
        // SAFETY: `fb_base`/`pixel_count` describe the firmware surface
        // validated at configure time, identity-mapped RAM; the caller's
        // contract makes this the only live reference.
        let pixels = unsafe {
            core::slice::from_raw_parts_mut(state.fb_base as *mut u32, state.pixel_count)
        };
        let stride = geometry.stride_px as usize;
        for row in row_start..row_end {
            let offset = row as usize * stride;
            pixels[offset..offset + geometry.width_px as usize].fill(colour);
        }
        clean_dcache_range(
            state.fb_base + row_start as usize * stride * 4,
            (row_end - row_start) as usize * stride * 4,
        );
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
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    use rustos_fdt::fixture::raspi_like_arm;
    use rustos_fdt::Fdt;
    use rustos_vcmailbox::mock::MockFirmware;

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
        let mut builder = rustos_fdt::fixture::DtbBuilder::new();
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
        assert_eq!(geometry.scale, 3, "1080p selects 3× glyphs");
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
    fn geometry_scale_policy_tracks_display_height() {
        for (height, scale) in [(480, 1), (720, 2), (1080, 3), (2160, 4), (4320, 4)] {
            let geometry = Geometry::for_display(1920, height, 1920 * 4).expect("geometry");
            assert_eq!(geometry.scale, scale, "height {height}");
        }
    }

    #[test]
    fn geometry_rejects_unusable_surfaces() {
        // Pitch not whole pixels.
        assert!(Geometry::for_display(640, 480, 640 * 4 + 2).is_none());
        // Pitch narrower than a scanline.
        assert!(Geometry::for_display(640, 480, 639 * 4).is_none());
        // Degenerate extents.
        assert!(Geometry::for_display(0, 480, 640 * 4).is_none());
        assert!(Geometry::for_display(640, 0, 640 * 4).is_none());
        // Too small for one glyph cell.
        assert!(Geometry::for_display(4, 4, 4 * 4).is_none());
    }

    // --- Renderer ------------------------------------------------------

    /// A 2-column × 2-row scale-1 test surface (12×16 px, stride 14 to
    /// exercise stride ≠ width).
    fn small_console() -> (TextConsole, Vec<u32>) {
        let geometry = Geometry {
            width_px: 12,
            height_px: 16,
            stride_px: 14,
            scale: 1,
        };
        assert_eq!((geometry.columns(), geometry.rows()), (2, 2));
        let mut pixels = vec![0u32; geometry.pixel_count()];
        let mut console = TextConsole::new(geometry);
        console.clear(&mut pixels);
        (console, pixels)
    }

    /// The pixels of one glyph cell, row-major.
    fn cell(pixels: &[u32], geometry: &Geometry, column: u32, row: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for y in 0..CELL_HEIGHT * geometry.scale {
            for x in 0..CELL_WIDTH * geometry.scale {
                let index = (row * CELL_HEIGHT * geometry.scale + y) as usize
                    * geometry.stride_px as usize
                    + (column * CELL_WIDTH * geometry.scale + x) as usize;
                out.push(pixels[index]);
            }
        }
        out
    }

    #[test]
    fn clear_paints_the_background_and_homes_the_cursor() {
        let (_, pixels) = small_console();
        assert!(pixels.iter().all(|&p| p == BACKGROUND));
    }

    #[test]
    fn a_glyph_renders_its_atlas_rows() {
        let (mut console, mut pixels) = small_console();
        let dirty = console.write_byte(&mut pixels, b'!');
        assert_eq!(dirty, Some((0, 8)), "dirty band covers the cell");
        let rendered = cell(&pixels, console.geometry(), 0, 0);
        // '!' lights the centre column (atlas row pattern 0b00100) on
        // rows 0..=4 and 6; row 5 and the padding row/column stay dark.
        let glyph = &glyphs::GLYPHS[(b'!' - b' ') as usize];
        for (y, &bits) in glyph.iter().enumerate() {
            for x in 0..CELL_WIDTH as usize {
                let lit = x < glyphs::GLYPH_WIDTH as usize
                    && bits & (1 << (glyphs::GLYPH_WIDTH as usize - 1 - x)) != 0;
                let expected = if lit { FOREGROUND } else { BACKGROUND };
                assert_eq!(rendered[y * CELL_WIDTH as usize + x], expected, "({x},{y})");
            }
        }
        // The inter-line padding row is background.
        for x in 0..CELL_WIDTH as usize {
            let y = glyphs::GLYPH_HEIGHT as usize;
            assert_eq!(rendered[y * CELL_WIDTH as usize + x], BACKGROUND);
        }
    }

    #[test]
    fn unprintable_bytes_render_the_question_mark_fallback() {
        let (mut console, mut pixels) = small_console();
        console.write_byte(&mut pixels, 0x01);
        let fallback = cell(&pixels, console.geometry(), 0, 0);
        let (mut reference_console, mut reference) = small_console();
        reference_console.write_byte(&mut reference, b'?');
        assert_eq!(
            fallback,
            cell(&reference, reference_console.geometry(), 0, 0)
        );
    }

    #[test]
    fn newline_and_carriage_return_move_the_cursor() {
        let (mut console, mut pixels) = small_console();
        console.write_byte(&mut pixels, b'A');
        console.write_byte(&mut pixels, b'\n');
        console.write_byte(&mut pixels, b'B');
        let geometry = *console.geometry();
        assert_ne!(
            cell(&pixels, &geometry, 0, 1)
                .iter()
                .filter(|&&p| p == FOREGROUND)
                .count(),
            0,
            "B rendered on row 1"
        );
        // `\r` returns to column 0: the next glyph overwrites `B`.
        console.write_byte(&mut pixels, b'\r');
        console.write_byte(&mut pixels, b' ');
        assert!(cell(&pixels, &geometry, 0, 1)
            .iter()
            .all(|&p| p == BACKGROUND));
    }

    #[test]
    fn the_grid_wraps_columns_and_rings_rows() {
        let (mut console, mut pixels) = small_console();
        // Fill row 0 (2 columns): the cursor wraps to row 1.
        console.write_byte(&mut pixels, b'A');
        console.write_byte(&mut pixels, b'A');
        // Fill row 1: the ring wraps back to row 0 and clears it.
        console.write_byte(&mut pixels, b'B');
        console.write_byte(&mut pixels, b'B');
        let geometry = *console.geometry();
        assert!(
            cell(&pixels, &geometry, 0, 0)
                .iter()
                .all(|&p| p == BACKGROUND),
            "ring wrap cleared the top row"
        );
        assert_ne!(
            cell(&pixels, &geometry, 0, 1)
                .iter()
                .filter(|&&p| p == FOREGROUND)
                .count(),
            0,
            "row 1 still holds its glyphs"
        );
        // The next glyph lands on the cleared top row.
        let dirty = console.write_byte(&mut pixels, b'C');
        assert_eq!(dirty, Some((0, 8)));
    }

    // --- Boot-progress beacon bands ----------------------------------

    #[test]
    fn beacon_bands_stack_up_from_the_bottom_edge() {
        let geometry = Geometry::for_display(640, 480, 2560).expect("usable surface");
        assert_eq!(beacon_band_rows(&geometry, 0), Some((464, 480)));
        assert_eq!(beacon_band_rows(&geometry, 1), Some((448, 464)));
        assert_eq!(beacon_band_rows(&geometry, 4), Some((400, 416)));
    }

    #[test]
    fn a_beacon_band_that_does_not_fit_is_rejected() {
        let geometry = Geometry::for_display(640, 480, 2560).expect("usable surface");
        // The last whole band fits; one past it does not, and an index
        // whose offset arithmetic overflows is rejected, not wrapped.
        assert_eq!(
            beacon_band_rows(&geometry, 480 / BEACON_BAND_PX - 1),
            Some((0, 16))
        );
        assert_eq!(beacon_band_rows(&geometry, 480 / BEACON_BAND_PX), None);
        assert_eq!(beacon_band_rows(&geometry, u32::MAX), None);
    }

    #[test]
    fn dirty_bands_merge_to_their_union() {
        assert_eq!(merge_bands(None, None), None);
        assert_eq!(merge_bands(Some((8, 16)), None), Some((8, 16)));
        assert_eq!(merge_bands(Some((8, 16)), Some((0, 8))), Some((0, 16)));
    }
}
