//! RustOS VESA (VBE) linear-framebuffer display driver.
//!
//! Implements [`rustos_abi::driver::display::Display`] over the linear
//! framebuffer a VESA BIOS Extensions (VBE) mode exposes on a legacy
//! x86_64 PC. On those machines the kernel cannot re-enter real mode to
//! issue VBE BIOS calls itself, so mode selection happens in the
//! bootloader: the boot stub asks the firmware for a 32-bit direct-colour
//! linear-framebuffer mode and hands the resulting **VBE `ModeInfoBlock`**
//! (the 256-byte record VBE function `0x4F01` fills in) to the driver host
//! as a boot capability. This driver parses and validates that block,
//! derives the surface geometry and pixel encoding from it, maps the
//! linear framebuffer through the capability-gated [`MmioMapper`], and
//! presents fully-rendered frames into it.
//!
//! This is what distinguishes the VESA driver from the generic
//! linear-surface engine (`rustos_display::Framebuffer`, hosted by the
//! `drivers/display/framebuffer` service process): that engine consumes
//! an already-parsed geometry record from firmware that hands off a
//! surface directly (UEFI GOP, the Pi mailbox,
//! `ramfb`), whereas this driver owns the VBE-specific decode — the
//! linear-framebuffer attribute bit, the direct-colour memory model, the
//! per-channel mask sizes and field positions — that a VBE `ModeInfoBlock`
//! requires before a surface can be trusted. The two are deliberate
//! sibling display drivers (carve-out), not duplicates.
//!
//! Compositing, damage tracking, and GPU acceleration live above this
//! driver in `userland/gui/wm`; the driver itself only owns the final
//! scan-out copy (`lib/abi/src/driver/display.rs`).
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`VbeModeInfo`] and [`VesaFramebuffer`] are public *types* re-exported
//! so the driver host can decode a boot-supplied block and instantiate the
//! surface through [`VesaFramebuffer::open`]; the host never reaches into
//! the type beyond the [`Display`] trait afterwards.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]. Mapping the surface
//! additionally requires [`CapabilityId::MMIO_MAP`]: the linear
//! framebuffer is device-visible memory and is reached only through the
//! capability-gated [`MmioMapper`], never through a pointer the driver
//! synthesises from the `ModeInfoBlock`'s `PhysBasePtr` itself
//! (no ambient authority). The driver runs in user space;
//! it does not request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::display::{Display, DisplayFormat, DisplayMode, SeatGate};
use rustos_abi::driver::mmio::{MmioMapError, WindowError};
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, MmioMapper, RegisterWindow};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention the framebuffer and bus drivers use: the host
/// re-issues a host-local handle when binding the driver into its load
/// table; this constant is the on-the-wire signal that every load-time
/// gate cleared. The low bytes spell `"VESA"`.
const REGISTER_HANDLE_MARKER: u64 = 0x5645_5341_0000_0001;

/// Wire length in bytes of a VBE `ModeInfoBlock` (VBE 3.0 §4.3.2).
pub const VBE_MODE_INFO_BLOCK_LEN: usize = 256;

/// `ModeAttributes` bit 0 — the mode is supported by the hardware.
const MODE_ATTR_SUPPORTED: u16 = 1 << 0;
/// `ModeAttributes` bit 7 — a linear (flat) framebuffer is available.
const MODE_ATTR_LINEAR_FRAMEBUFFER: u16 = 1 << 7;
/// `MemoryModel` value 6 — direct colour (the only model abi-v1 presents).
const MEMORY_MODEL_DIRECT_COLOUR: u8 = 6;
/// The only bit depth the abi-v1 [`DisplayFormat`] surface covers.
const SUPPORTED_BITS_PER_PIXEL: u8 = 32;
/// Channel mask width every supported 32-bpp direct-colour mode uses.
const SUPPORTED_CHANNEL_MASK_BITS: u8 = 8;

/// Byte offsets of the `ModeInfoBlock` fields this driver reads
/// (VBE 3.0 §4.3.2). Fields the driver does not consume are skipped.
mod field {
    /// `ModeAttributes` (u16).
    pub const MODE_ATTRIBUTES: usize = 0x00;
    /// `BytesPerScanLine` (u16) — the surface stride.
    pub const BYTES_PER_SCAN_LINE: usize = 0x10;
    /// `XResolution` (u16) — surface width in pixels.
    pub const X_RESOLUTION: usize = 0x12;
    /// `YResolution` (u16) — surface height in pixels.
    pub const Y_RESOLUTION: usize = 0x14;
    /// `BitsPerPixel` (u8).
    pub const BITS_PER_PIXEL: usize = 0x19;
    /// `MemoryModel` (u8).
    pub const MEMORY_MODEL: usize = 0x1B;
    /// `RedMaskSize` (u8).
    pub const RED_MASK_SIZE: usize = 0x1F;
    /// `RedFieldPosition` (u8).
    pub const RED_FIELD_POSITION: usize = 0x20;
    /// `GreenMaskSize` (u8).
    pub const GREEN_MASK_SIZE: usize = 0x21;
    /// `GreenFieldPosition` (u8).
    pub const GREEN_FIELD_POSITION: usize = 0x22;
    /// `BlueMaskSize` (u8).
    pub const BLUE_MASK_SIZE: usize = 0x23;
    /// `BlueFieldPosition` (u8).
    pub const BLUE_FIELD_POSITION: usize = 0x24;
    /// `PhysBasePtr` (u32) — physical base of the linear framebuffer
    /// (VBE 2.0+).
    pub const PHYS_BASE_PTR: usize = 0x28;
}

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

/// A validated VBE linear-framebuffer mode decoded from a 256-byte
/// `ModeInfoBlock`.
///
/// Construct one with [`VbeModeInfo::parse`]; the fields are read-only
/// projections of the bytes the firmware reported, after every invariant
/// the driver relies on has been checked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VbeModeInfo {
    /// Physical base address of the linear framebuffer (scanline 0,
    /// pixel 0). Always non-zero after a successful parse.
    pub phys_base: u64,
    /// Surface width in pixels.
    pub width_px: u32,
    /// Surface height in pixels.
    pub height_px: u32,
    /// Distance in bytes between consecutive scanlines
    /// (`BytesPerScanLine`). May exceed `width_px * 4` when the mode
    /// pads scanlines.
    pub stride_bytes: u32,
    /// Pixel encoding derived from the per-channel field positions.
    pub format: DisplayFormat,
}

/// Read a little-endian `u16` field. The caller proved `off + 2` is in
/// bounds by validating the block length first.
fn read_u16(block: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([block[off], block[off + 1]])
}

/// Read a little-endian `u32` field. The caller proved `off + 4` is in
/// bounds by validating the block length first.
fn read_u32(block: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]])
}

impl VbeModeInfo {
    /// Decode and validate a VBE `ModeInfoBlock`.
    ///
    /// Accepts the 256-byte record the bootloader captured from VBE
    /// function `0x4F01`. The parse fails closed on anything the abi-v1
    /// [`Display`] surface cannot honour, so a successful result is a
    /// surface every later access can trust.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `block` is shorter than
    ///   [`VBE_MODE_INFO_BLOCK_LEN`].
    /// * [`DriverError::Unsupported`] if the mode is not marked
    ///   supported, exposes no linear framebuffer, is not direct-colour,
    ///   is not 32 bits per pixel, does not use 8-bit channel masks, or
    ///   uses a channel layout that is neither `Rgba8888` nor `Bgra8888`.
    /// * [`DriverError::LengthOutOfRange`] if the geometry is degenerate
    ///   (zero width/height, or a stride too small for one scanline, or
    ///   a surface size that overflows the host address width).
    /// * [`DriverError::DeviceFault`] if the mode advertises a linear
    ///   framebuffer but reports a zero physical base.
    pub fn parse(block: &[u8]) -> Result<Self, DriverError> {
        if block.len() < VBE_MODE_INFO_BLOCK_LEN {
            return Err(DriverError::BufferTooSmall);
        }

        let attributes = read_u16(block, field::MODE_ATTRIBUTES);
        if attributes & MODE_ATTR_SUPPORTED == 0 {
            return Err(DriverError::Unsupported);
        }
        if attributes & MODE_ATTR_LINEAR_FRAMEBUFFER == 0 {
            return Err(DriverError::Unsupported);
        }
        if block[field::MEMORY_MODEL] != MEMORY_MODEL_DIRECT_COLOUR {
            return Err(DriverError::Unsupported);
        }
        if block[field::BITS_PER_PIXEL] != SUPPORTED_BITS_PER_PIXEL {
            return Err(DriverError::Unsupported);
        }

        let format = parse_format(block)?;

        let width_px = u32::from(read_u16(block, field::X_RESOLUTION));
        let height_px = u32::from(read_u16(block, field::Y_RESOLUTION));
        let stride_bytes = u32::from(read_u16(block, field::BYTES_PER_SCAN_LINE));
        let phys_base = u64::from(read_u32(block, field::PHYS_BASE_PTR));

        if phys_base == 0 {
            return Err(DriverError::DeviceFault);
        }
        validate_geometry(width_px, height_px, stride_bytes, format)?;

        Ok(Self {
            phys_base,
            width_px,
            height_px,
            stride_bytes,
            format,
        })
    }

    /// Number of bytes the surface occupies (`stride_bytes * height_px`).
    ///
    /// Cannot overflow: [`VbeModeInfo::parse`] validated the product
    /// against the host address width.
    fn surface_len(&self) -> Result<usize, DriverError> {
        let bytes = u64::from(self.stride_bytes)
            .checked_mul(u64::from(self.height_px))
            .ok_or(DriverError::LengthOutOfRange)?;
        usize::try_from(bytes).map_err(|_| DriverError::LengthOutOfRange)
    }

    /// The abi-v1 [`DisplayMode`] this mode presents.
    fn display_mode(&self) -> DisplayMode {
        DisplayMode {
            width_px: self.width_px,
            height_px: self.height_px,
            stride_bytes: self.stride_bytes,
            format: self.format,
        }
    }
}

/// Derive the abi-v1 [`DisplayFormat`] from the block's direct-colour
/// channel layout.
///
/// abi-v1 covers the two 32-bpp byte orders VBE direct-colour modes use
/// in practice: blue at byte 0 ([`DisplayFormat::Bgra8888`], the common
/// PC linear-framebuffer layout) and red at byte 0
/// ([`DisplayFormat::Rgba8888`]). Any other layout, or a non-8-bit mask,
/// is rejected so the byte-preserving [`present`](Display::present) copy
/// never silently mis-renders.
fn parse_format(block: &[u8]) -> Result<DisplayFormat, DriverError> {
    let masks_are_8bit = block[field::RED_MASK_SIZE] == SUPPORTED_CHANNEL_MASK_BITS
        && block[field::GREEN_MASK_SIZE] == SUPPORTED_CHANNEL_MASK_BITS
        && block[field::BLUE_MASK_SIZE] == SUPPORTED_CHANNEL_MASK_BITS;
    if !masks_are_8bit {
        return Err(DriverError::Unsupported);
    }

    let red = block[field::RED_FIELD_POSITION];
    let green = block[field::GREEN_FIELD_POSITION];
    let blue = block[field::BLUE_FIELD_POSITION];

    match (red, green, blue) {
        (16, 8, 0) => Ok(DisplayFormat::Bgra8888),
        (0, 8, 16) => Ok(DisplayFormat::Rgba8888),
        _ => Err(DriverError::Unsupported),
    }
}

/// Reject degenerate geometry and confirm the stride holds one scanline.
fn validate_geometry(
    width_px: u32,
    height_px: u32,
    stride_bytes: u32,
    format: DisplayFormat,
) -> Result<(), DriverError> {
    if width_px == 0 || height_px == 0 {
        return Err(DriverError::LengthOutOfRange);
    }
    let min_stride = width_px
        .checked_mul(format.bytes_per_pixel())
        .ok_or(DriverError::LengthOutOfRange)?;
    if stride_bytes < min_stride {
        return Err(DriverError::LengthOutOfRange);
    }
    Ok(())
}

/// A VESA linear framebuffer surface mapped through the host's
/// [`MmioMapper`].
///
/// The driver owns the [`RegisterWindow`] for the whole load: dropping
/// the [`VesaFramebuffer`] drops the window, which is the driver's
/// quiesce step (the kernel reclaims the mapping on unload). Reloading is calling [`VesaFramebuffer::open`] again.
///
/// `'h` borrows the opening [`DriverHost`]: the driver captures the
/// host's [`SeatGate`], so every present re-checks the presenting
/// client's live seat lease before touching the surface.
pub struct VesaFramebuffer<'h> {
    window: RegisterWindow,
    mode: DisplayMode,
    surface_len: usize,
    seat: Option<&'h dyn SeatGate>,
}

impl<'h> VesaFramebuffer<'h> {
    /// Decode the boot-supplied VBE `ModeInfoBlock` and bring its linear
    /// framebuffer online.
    ///
    /// Parses `mode_info_block` with [`VbeModeInfo::parse`], maps exactly
    /// `stride_bytes * height_px` bytes at the reported `PhysBasePtr`
    /// through the host's [`MmioMapper`], and returns a driver ready to
    /// [`present`](Display::present).
    ///
    /// # Errors
    ///
    /// * Any error [`VbeModeInfo::parse`] returns for a malformed or
    ///   unsupported block.
    /// * [`DriverError::PermissionDenied`] if the host did not grant
    ///   [`CapabilityId::MMIO_MAP`].
    /// * [`DriverError::Unsupported`] if the host exposes no
    ///   [`MmioMapper`], or the platform cannot map the region.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
    /// [`CapabilityId::DRV_LOAD`] [`register`] checked.
    pub fn open(host: &'h dyn DriverHost, mode_info_block: &[u8]) -> Result<Self, DriverError> {
        let info = VbeModeInfo::parse(mode_info_block)?;
        Self::from_mode_info(host, info)
    }

    /// Map the linear framebuffer described by an already-parsed
    /// [`VbeModeInfo`].
    ///
    /// # Errors
    ///
    /// As [`VesaFramebuffer::open`], minus the parse failures.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::MMIO_MAP`].
    pub fn from_mode_info(
        host: &'h dyn DriverHost,
        info: VbeModeInfo,
    ) -> Result<Self, DriverError> {
        let surface_len = info.surface_len()?;
        if surface_len == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        if !host.has_capability(CapabilityId::MMIO_MAP) {
            return Err(DriverError::PermissionDenied);
        }
        let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
        let window = mapper
            .map_window(info.phys_base, surface_len)
            .map_err(MmioMapError::as_driver_error)?;
        Ok(Self {
            window,
            mode: info.display_mode(),
            surface_len,
            seat: host.seat_gate(),
        })
    }

    /// Copy `frame[..surface_len]` into the mapped window.
    ///
    /// The bulk of the surface is written as naturally-aligned `u32`
    /// scan-out words; any trailing bytes (a surface whose length is not
    /// a multiple of four) are written individually. Every write is
    /// bounds-checked by the window, so a miscomputed offset fails closed
    /// instead of escaping the mapping.
    fn blit(&self, frame: &[u8]) -> Result<(), DriverError> {
        let whole_words = self.surface_len / 4;
        for word in 0..whole_words {
            let off = word * 4;
            let value =
                u32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            self.window
                .write_u32(off, value)
                .map_err(WindowError::as_driver_error)?;
        }
        let tail_start = whole_words * 4;
        for (i, &byte) in frame[tail_start..self.surface_len].iter().enumerate() {
            self.window
                .write_u8(tail_start + i, byte)
                .map_err(WindowError::as_driver_error)?;
        }
        Ok(())
    }
}

impl Display for VesaFramebuffer<'_> {
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
}
