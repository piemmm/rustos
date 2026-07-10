//! RustOS generic linear framebuffer display driver.
//!
//! Implements [`rustos_abi::driver::display::Display`] over a single
//! firmware-provided linear pixel surface. The driver is
//! platform-neutral: it copies a fully-rendered frame into a linear
//! framebuffer whose physical base, geometry, and pixel encoding the
//! boot capability discovered and handed to the driver host. The same
//! source serves every platform whose firmware exposes such a surface
//! (the aarch64 Raspberry Pi mailbox framebuffer, the riscv64 `virt`
//! board's `ramfb`, and any UEFI GOP linear frame buffer); the
//! wasm32 canvas target presents the same surface shape through the
//! browser host.
//!
//! Compositing, damage tracking, and GPU acceleration live above this
//! driver in `userland/gui/wm`; the driver itself only owns the final
//! scan-out copy (`lib/abi/src/driver/display.rs`).
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`Framebuffer`] is a public *type* re-exported so the driver host
//! can instantiate it through [`Framebuffer::open`]; the host never
//! reaches into the type beyond the [`Display`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]. Mapping the surface
//! additionally requires [`CapabilityId::MMIO_MAP`]: the framebuffer
//! is device-visible memory and is reached only through the
//! capability-gated [`MmioMapper`], never through a pointer the driver
//! synthesises itself (no ambient authority). The
//! driver runs in user space; it does not request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::display::{DamageRect, Display, DisplayFormat, DisplayMode, SeatGate};
use rustos_abi::driver::mmio::{MmioMapError, WindowError};
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, MmioMapper, RegisterWindow};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention the bus and storage drivers use: the host
/// re-issues a host-local handle when binding the driver into its load
/// table; this constant is the on-the-wire signal that every load-time
/// gate cleared. The bytes spell `"FBUF"`.
const REGISTER_HANDLE_MARKER: u64 = 0x4642_5546_0000_0001;

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// Firmware-discovered description of a linear framebuffer.
///
/// The boot capability fills this in from the platform's framebuffer
/// hand-off (UEFI GOP, the Pi mailbox, `ramfb`) and the driver host
/// passes it to [`Framebuffer::open`]. The driver never invents these
/// values; it only validates them and maps the region they describe.
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
/// The driver owns the [`RegisterWindow`] for the whole load: dropping
/// the [`Framebuffer`] drops the window, which is the driver's quiesce
/// step (the kernel reclaims the mapping on unload).
/// Reloading is simply calling [`Framebuffer::open`] again.
///
/// `'h` borrows the opening [`DriverHost`]: the driver captures the
/// host's [`SeatGate`], so every present re-checks the presenting
/// client's live seat lease before touching the surface.
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
    /// returns a driver ready to [`present`](Display::present).
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
    /// Requires [`CapabilityId::MMIO_MAP`] in addition to the
    /// load-time [`CapabilityId::DRV_LOAD`] [`register`] checked.
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
    /// The bulk of the span is written as naturally-aligned `u32`
    /// scan-out words; unaligned head and tail bytes are written
    /// individually. Every write is bounds-checked by the window, so a
    /// miscomputed offset fails closed instead of escaping the mapping.
    fn blit_span(&self, frame: &[u8], offset: usize, len: usize) -> Result<(), DriverError> {
        let end = offset + len;
        let head_end = end.min(offset.next_multiple_of(4));
        for (off, &byte) in frame.iter().enumerate().take(head_end).skip(offset) {
            self.window
                .write_u8(off, byte)
                .map_err(WindowError::as_driver_error)?;
        }
        let mut off = head_end;
        while off + 4 <= end {
            let value =
                u32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            self.window
                .write_u32(off, value)
                .map_err(WindowError::as_driver_error)?;
            off += 4;
        }
        for (tail, &byte) in frame.iter().enumerate().take(end).skip(off) {
            self.window
                .write_u8(tail, byte)
                .map_err(WindowError::as_driver_error)?;
        }
        Ok(())
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

    fn present_region(&mut self, frame: &[u8], damage: DamageRect) -> Result<(), DriverError> {
        // Same order as the full present: the lease first, then every
        // bound, then the surface.
        if let Some(gate) = self.seat {
            gate.check_present()?;
        }
        damage.validate_in(&self.mode)?;
        if frame.len() < self.surface_len {
            return Err(DriverError::BufferTooSmall);
        }
        let stride = self.mode.stride_bytes as usize;
        let bpp = self.mode.format.bytes_per_pixel() as usize;
        let x0 = damage.x as usize * bpp;
        let span = damage.width_px as usize * bpp;
        for row in 0..damage.height_px as usize {
            let offset = (damage.y as usize + row) * stride + x0;
            self.blit_span(frame, offset, span)?;
        }
        Ok(())
    }
}
