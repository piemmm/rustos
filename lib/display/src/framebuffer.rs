//! The generic linear-framebuffer surface engine.
//!
//! Implements [`Display`] over a single firmware-provided linear pixel
//! surface. The engine is platform-neutral: it copies a fully-rendered
//! frame into a linear framebuffer whose physical base, geometry, and
//! pixel encoding the hosting process discovered and handed in — the
//! `drivers/display/framebuffer` `Run` binary resolves them from its
//! kernel-issued device-resource grants (`sole_framebuffer`), and the
//! framebuffer QEMU verticals hand-assemble them from the board they
//! synthesise. The same engine serves every platform whose firmware
//! exposes such a surface (the aarch64 Raspberry Pi mailbox framebuffer,
//! the riscv64 `virt` board's `ramfb`, any UEFI GOP linear frame
//! buffer); the wasm32 canvas target presents the same surface shape
//! through the browser host.
//!
//! Compositing, damage tracking, and GPU acceleration live above this
//! engine in `userland/gui/wm`; the engine itself only owns the final
//! scan-out copy (`lib/abi/src/driver/display.rs`).
//!
//! # Capabilities
//!
//! Mapping the surface requires
//! [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP): the
//! framebuffer is device-visible memory and is reached only through the
//! capability-gated [`MmioMapper`], never through a pointer the engine
//! synthesises itself (no ambient authority).

use tairix_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode, SeatGate};
use tairix_abi::driver::mmio::{MmioMapError, WindowError};
use tairix_abi::{CapabilityId, DriverError, DriverHost, MmioMapper, RegisterWindow};

/// Discovered description of a linear framebuffer.
///
/// The hosting process fills this in from the platform's framebuffer
/// hand-off (the kernel-granted
/// [`Framebuffer`](tairix_abi::hwtree::HwResourceKind::Framebuffer)
/// resource, UEFI GOP, the Pi mailbox, `ramfb`) and passes it to
/// [`Framebuffer::open`]. The engine never invents these values; it only
/// validates them and maps the region they describe.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FramebufferConfig {
    /// Device-visible physical base address of scanline 0, pixel 0.
    pub phys_base: u64,
    /// Surface width in pixels.
    pub width_px: u32,
    /// Surface height in pixels.
    pub height_px: u32,
    /// Distance in bytes between the start of consecutive scanlines.
    /// May exceed `width_px * format.bytes_per_pixel()` when the
    /// firmware pads scanlines.
    pub stride_bytes: u32,
    /// Pixel encoding the firmware programmed the surface for.
    pub format: DisplayFormat,
}

impl FramebufferConfig {
    /// Number of bytes the surface occupies
    /// (`stride_bytes * height_px`), or [`DriverError::LengthOutOfRange`]
    /// if the product overflows the host's address width.
    fn surface_len(&self) -> Result<usize, DriverError> {
        let bytes = u64::from(self.stride_bytes)
            .checked_mul(u64::from(self.height_px))
            .ok_or(DriverError::LengthOutOfRange)?;
        usize::try_from(bytes).map_err(|_| DriverError::LengthOutOfRange)
    }

    /// Validate the geometry and return the surface length in bytes.
    ///
    /// Rejects a zero-sized surface and a stride that cannot hold one
    /// scanline of `width_px` pixels in `format`. Failing closed here
    /// keeps every later access inside the mapped window.
    fn validate(&self) -> Result<usize, DriverError> {
        if self.width_px == 0 || self.height_px == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        let min_stride = self
            .width_px
            .checked_mul(self.format.bytes_per_pixel())
            .ok_or(DriverError::LengthOutOfRange)?;
        if self.stride_bytes < min_stride {
            return Err(DriverError::LengthOutOfRange);
        }
        let len = self.surface_len()?;
        if len == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(len)
    }
}

/// A linear framebuffer surface mapped through the host's
/// [`MmioMapper`].
///
/// The engine owns the [`RegisterWindow`] for the whole load: dropping
/// the [`Framebuffer`] drops the window, which is the hosting process's
/// quiesce step (the kernel reclaims the mapping on unload).
/// Reloading is simply calling [`Framebuffer::open`] again.
///
/// `'h` borrows the opening [`DriverHost`]: the engine captures the
/// host's [`SeatGate`], so every present re-checks the presenting
/// client's live seat lease before touching the surface. A host with no
/// gate wired (headless bring-up, or a service that gates presents
/// upstream through the display-protocol lease check) presents ungated
/// here.
pub struct Framebuffer<'h> {
    window: RegisterWindow,
    mode: DisplayMode,
    surface_len: usize,
    seat: Option<&'h dyn SeatGate>,
}

impl<'h> Framebuffer<'h> {
    /// Map the firmware framebuffer described by `config` and bring the
    /// surface online.
    ///
    /// Obtains the host's [`MmioMapper`], maps exactly
    /// `stride_bytes * height_px` bytes at `config.phys_base`, and
    /// returns an engine ready to [`present`](Display::present).
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `config` describes a
    ///   zero-sized surface or a stride too small for one scanline.
    /// * [`DriverError::PermissionDenied`] if the host did not grant
    ///   [`CapabilityId::MMIO_MAP`].
    /// * [`DriverError::Unsupported`] if the host exposes no
    ///   [`MmioMapper`], or the platform cannot map the region.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::MMIO_MAP`].
    pub fn open(host: &'h dyn DriverHost, config: FramebufferConfig) -> Result<Self, DriverError> {
        let surface_len = config.validate()?;
        if !host.has_capability(CapabilityId::MMIO_MAP) {
            return Err(DriverError::PermissionDenied);
        }
        let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
        let window = mapper
            .map_window(config.phys_base, surface_len)
            .map_err(MmioMapError::as_driver_error)?;
        Ok(Self {
            window,
            mode: DisplayMode {
                width_px: config.width_px,
                height_px: config.height_px,
                stride_bytes: config.stride_bytes,
                format: config.format,
            },
            surface_len,
            seat: host.seat_gate(),
        })
    }

    /// Copy `frame[..surface_len]` into the mapped window.
    fn blit(&self, frame: &[u8]) -> Result<(), DriverError> {
        self.blit_span(frame, 0, self.surface_len)
    }

    /// Copy `frame[offset..offset + len]` into the window at `offset` —
    /// the one write path both the full and the region blit share.
    ///
    /// A scan-out surface is bulk pixel memory (the kernel maps it
    /// non-cacheable Normal, not Device registers), so the span is copied in
    /// one bounds-checked bulk write rather than a register at a time: filling
    /// a whole frame one word at a time through per-access checked volatile
    /// writes is pathologically slow (tens of seconds per frame under
    /// emulation). A miscomputed offset still fails closed — the single bounds
    /// check rejects a span that would escape the mapping.
    fn blit_span(&self, frame: &[u8], offset: usize, len: usize) -> Result<(), DriverError> {
        let end = offset.checked_add(len).ok_or(DriverError::BufferTooSmall)?;
        let src = frame.get(offset..end).ok_or(DriverError::BufferTooSmall)?;
        self.window
            .write_bytes(offset, src)
            .map_err(WindowError::as_driver_error)
    }
}

impl Display for Framebuffer<'_> {
    fn mode_info(&self) -> Result<DisplayMode, DriverError> {
        Ok(self.mode)
    }

    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        // The present right is derived from the live seat lease, before any
        // other validation or surface access: a client whose lease was
        // revoked cannot scan out even though its mapping persists. A host
        // with no seat wired (headless, boot bring-up) exposes no gate and
        // the present proceeds ungated.
        if let Some(gate) = self.seat {
            gate.check_present()?;
        }
        if frame.len() < self.surface_len {
            return Err(DriverError::BufferTooSmall);
        }
        self.blit(frame)
    }

    fn present_rects(&mut self, frame: &[u8], damage: &[DamageRect]) -> Result<(), DriverError> {
        // Same order as the full present: the lease first, then every
        // bound of the whole list, then the surface — so a bad rectangle
        // late in the list refuses the present instead of leaving the ones
        // before it on screen.
        if let Some(gate) = self.seat {
            gate.check_present()?;
        }
        DamageRect::validate_list(damage, &self.mode)?;
        if frame.len() < self.surface_len {
            return Err(DriverError::BufferTooSmall);
        }
        let stride = self.mode.stride_bytes as usize;
        let bpp = self.mode.format.bytes_per_pixel() as usize;
        for rect in damage {
            let x0 = rect.x as usize * bpp;
            let span = rect.width_px as usize * bpp;
            for row in 0..rect.height_px as usize {
                let offset = (rect.y as usize + row) * stride + x0;
                self.blit_span(frame, offset, span)?;
            }
        }
        Ok(())
    }
}
