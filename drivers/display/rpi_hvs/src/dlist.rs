//! Configuration and `VideoCore` HVS display-list (DLIST) encoding.
//!
//! The HVS composites by walking a *display list* of plane entries held
//! in a small dedicated RAM. This module owns the firmware-discovered
//! [`HvsConfig`] the driver host hands to [`RpiHvs::open`](crate::RpiHvs)
//! and the [`DlistBuilder`] that lays one unity-scaled plane entry per
//! source layer into that RAM.
//!
//! The entry layout below is modelled on the `VideoCore` VC4 HVS plane
//! format (six 32-bit words per plane plus an end marker). It is kept
//! self-contained and fully validated so a miscomputed field fails
//! closed rather than driving the scan-out engine off the end of its
//! list.

use rustos_abi::driver::display::{AccelLayer, DisplayFormat, DisplayMode};
use rustos_abi::driver::mmio::WindowError;
use rustos_abi::{DriverError, RegisterWindow};
use rustos_vcmailbox::{FirmwareFramebuffer, MailboxError};

/// Maximum number of hardware planes (and per-plane source buffers) the
/// driver tracks. The VC4 HVS exposes a small fixed plane budget; eight
/// covers the desktop's window stack with the software path picking up
/// any overflow.
pub const MAX_PLANES: usize = 8;

/// Words per plane entry in the display list (control, position, size,
/// pointer, pitch, alpha).
const WORDS_PER_PLANE: usize = 6;

/// Display-list capacity in words: every plane plus the end marker.
const MAX_DLIST_WORDS: usize = MAX_PLANES * WORDS_PER_PLANE + 1;

/// End-of-list marker word the HVS stops on.
const HVS_DLIST_END: u32 = 0x8000_0000;

/// Control word bit: the plane entry is valid.
const CTL_VALID: u32 = 1 << 31;
/// Control word bit: unity scaling (source size equals destination).
const CTL_UNITY: u32 = 1 << 8;
/// Control word bit: source pixels are premultiplied-alpha.
const CTL_PREMULTIPLIED: u32 = 1 << 9;

/// Bytes of the display-channel control window the driver writes.
pub const CONTROL_LEN_BYTES: usize = 8;
/// Control-window offset of the active display-list head (word index).
pub const CONTROL_DLIST_HEAD_OFFSET: usize = 0;
/// Control-window offset of the present generation counter.
pub const CONTROL_GENERATION_OFFSET: usize = 4;

/// Map a [`DisplayFormat`] to its HVS plane format code.
fn format_code(format: DisplayFormat) -> u32 {
    // The low nibble of the control word selects the pixel format; the
    // abi discriminant is reused directly so the mapping is one place.
    u32::from(format.as_u8())
}

/// Firmware-discovered geometry of the scan-out surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScanoutConfig {
    /// Device-visible physical base of scanline 0, pixel 0.
    pub phys_base: u64,
    /// Surface width in pixels.
    pub width_px: u32,
    /// Surface height in pixels.
    pub height_px: u32,
    /// Bytes between consecutive scanlines.
    pub stride_bytes: u32,
    /// Pixel encoding the firmware programmed the surface for.
    pub format: DisplayFormat,
}

impl ScanoutConfig {
    /// Surface length in bytes (`stride_bytes * height_px`).
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] on overflow.
    pub fn surface_len(&self) -> Result<usize, DriverError> {
        let bytes = u64::from(self.stride_bytes)
            .checked_mul(u64::from(self.height_px))
            .ok_or(DriverError::LengthOutOfRange)?;
        usize::try_from(bytes).map_err(|_| DriverError::LengthOutOfRange)
    }

    /// The [`DisplayMode`] this surface presents.
    #[must_use]
    pub const fn mode(&self) -> DisplayMode {
        DisplayMode {
            width_px: self.width_px,
            height_px: self.height_px,
            stride_bytes: self.stride_bytes,
            format: self.format,
        }
    }

    /// Produce the scan-out config from the firmware's validated
    /// framebuffer answer ([`rustos_vcmailbox::discover_framebuffer`]),
    /// translating its `VideoCore` bus address to the ARM physical base
    /// the host maps.
    ///
    /// # Errors
    ///
    /// [`MailboxError::BadAperture`] on a bad buffer address (see
    /// [`FirmwareFramebuffer::arm_physical_base`]).
    pub fn from_firmware(firmware: &FirmwareFramebuffer) -> Result<Self, MailboxError> {
        Ok(Self {
            phys_base: firmware.arm_physical_base()?,
            width_px: firmware.width_px,
            height_px: firmware.height_px,
            stride_bytes: firmware.pitch_bytes,
            format: firmware.format,
        })
    }

    fn validate(&self) -> Result<(), DriverError> {
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
        if self.surface_len()? == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(())
    }
}

/// Firmware-discovered geometry of one per-plane source buffer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PlaneConfig {
    /// Device-visible physical base of the plane's pixel buffer.
    pub phys_base: u64,
    /// Length of the plane's pixel buffer in bytes.
    pub len_bytes: usize,
}

/// Everything the driver host discovers about the HVS the driver binds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HvsConfig {
    /// The scan-out surface (used by the software fallback and as the
    /// composition target geometry).
    pub scanout: ScanoutConfig,
    /// Physical base of the HVS display-list RAM.
    pub dlist_phys_base: u64,
    /// Length of the display-list RAM in bytes (a multiple of four).
    pub dlist_len_bytes: usize,
    /// Physical base of the display-channel control register window.
    pub control_phys_base: u64,
    /// Per-plane source buffers; only the first `plane_count` are used.
    pub planes: [PlaneConfig; MAX_PLANES],
    /// Number of active planes (`1..=MAX_PLANES`).
    pub plane_count: usize,
    /// `VideoCore` bus alias OR-ed into a plane's physical address to form
    /// the bus address the HVS DMAs through.
    pub bus_alias: u32,
}

impl HvsConfig {
    /// The active plane configs.
    #[must_use]
    pub fn planes(&self) -> &[PlaneConfig] {
        &self.planes[..self.plane_count]
    }

    /// Translate a device-visible physical address to the `VideoCore` bus
    /// address the HVS DMAs through.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if `phys` does not fit the
    /// 30-bit `VideoCore` aperture.
    pub fn bus_address(&self, phys: u64) -> Result<u32, DriverError> {
        const APERTURE: u64 = 0x3FFF_FFFF;
        if phys > APERTURE {
            return Err(DriverError::LengthOutOfRange);
        }
        // `phys` is within the 30-bit aperture, so the conversion is exact.
        let base = u32::try_from(phys).map_err(|_| DriverError::LengthOutOfRange)?;
        Ok(base | self.bus_alias)
    }

    /// Validate the whole configuration.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] for a degenerate surface, an
    /// out-of-range plane count, a display list too small to hold the
    /// active planes, a non-word-aligned list, or a zero-length plane
    /// buffer.
    pub fn validate(&self) -> Result<(), DriverError> {
        self.scanout.validate()?;
        if self.plane_count == 0 || self.plane_count > MAX_PLANES {
            return Err(DriverError::LengthOutOfRange);
        }
        if self.dlist_len_bytes % 4 != 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        let needed_words = self.plane_count * WORDS_PER_PLANE + 1;
        if self.dlist_len_bytes / 4 < needed_words {
            return Err(DriverError::LengthOutOfRange);
        }
        for plane in self.planes() {
            if plane.len_bytes == 0 {
                return Err(DriverError::LengthOutOfRange);
            }
            self.bus_address(plane.phys_base)?;
        }
        Ok(())
    }
}

/// Accumulates plane entries into a fixed-capacity display list, then
/// writes them into the HVS display-list RAM.
pub struct DlistBuilder {
    words: [u32; MAX_DLIST_WORDS],
    len: usize,
    capacity_words: usize,
    finished: bool,
}

impl DlistBuilder {
    /// A builder targeting a display-list RAM of `capacity_words` words.
    #[must_use]
    pub fn new(capacity_words: usize) -> Self {
        Self {
            words: [0; MAX_DLIST_WORDS],
            len: 0,
            capacity_words,
            finished: false,
        }
    }

    fn push(&mut self, word: u32) -> Result<(), DriverError> {
        if self.len >= self.words.len() || self.len >= self.capacity_words {
            return Err(DriverError::LengthOutOfRange);
        }
        self.words[self.len] = word;
        self.len += 1;
        Ok(())
    }

    /// Append one unity-scaled plane entry for `layer` sourced from
    /// `bus_addr` in `format`.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if the list would overflow its
    /// RAM.
    pub fn push_plane(
        &mut self,
        bus_addr: u32,
        format: DisplayFormat,
        layer: &AccelLayer<'_>,
    ) -> Result<(), DriverError> {
        let ctl = CTL_VALID | CTL_UNITY | CTL_PREMULTIPLIED | (format_code(format) & 0xFF);
        self.push(ctl)?;
        self.push(pack_signed(layer.dst_x, layer.dst_y))?;
        self.push(pack_dims(layer.width_px, layer.height_px))?;
        self.push(bus_addr)?;
        self.push(layer.stride_bytes)?;
        self.push(u32::from(layer.opacity))?;
        Ok(())
    }

    /// Terminate the list with the end marker.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if there is no room for the
    /// marker.
    pub fn finish(&mut self) -> Result<(), DriverError> {
        self.push(HVS_DLIST_END)?;
        self.finished = true;
        Ok(())
    }

    /// The encoded words (only valid after [`Self::finish`]).
    #[must_use]
    pub fn words(&self) -> &[u32] {
        &self.words[..self.len]
    }

    /// Write the finished list into the display-list RAM `window`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if called before [`Self::finish`].
    /// * Any window error mapped from a bounds-checked write.
    pub fn write_to(&self, window: &RegisterWindow) -> Result<(), DriverError> {
        if !self.finished {
            return Err(DriverError::DeviceFault);
        }
        for (i, &word) in self.words().iter().enumerate() {
            window
                .write_u32(i * 4, word)
                .map_err(WindowError::as_driver_error)?;
        }
        Ok(())
    }
}

/// Pack two signed 16-bit screen coordinates into one word
/// (`x` low, `y` high), saturating an out-of-range coordinate.
fn pack_signed(x: i32, y: i32) -> u32 {
    let lo = u32::from(u16::from_ne_bytes(saturating_i16(x).to_ne_bytes()));
    let hi = u32::from(u16::from_ne_bytes(saturating_i16(y).to_ne_bytes()));
    lo | (hi << 16)
}

/// Pack two unsigned dimensions into one word (`width` low, `height`
/// high), saturating a dimension above 16 bits.
fn pack_dims(width: u32, height: u32) -> u32 {
    let lo = width.min(0xFFFF);
    let hi = height.min(0xFFFF);
    lo | (hi << 16)
}

/// Clamp an `i32` to the `i16` range so a wildly off-screen origin packs
/// to the nearest representable edge rather than wrapping.
fn saturating_i16(value: i32) -> i16 {
    let clamped = value.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
    i16::try_from(clamped).unwrap_or(0)
}
