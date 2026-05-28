//! Display driver class (`drivers/display/*`).
//!
//! A display driver presents a single linear pixel surface. The
//! trait is intentionally minimal: querying the active mode and
//! presenting a fully-rendered frame are the only operations the
//! Stage 4 first drivers (`vesa`, `framebuffer`, `gpu_virtio`) need.
//! Compositing, damage tracking, and GPU acceleration live above
//! this trait in `userland/gui/wm`.

use super::DriverError;

/// Pixel encodings supported by the abi-v1 display trait.
///
/// Names follow the byte order of the first pixel in memory; the
/// first letter is the byte at offset 0. New formats must take the
/// next free integer and are added in `abi-v2`.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum DisplayFormat {
    /// 32-bit colour, byte order R, G, B, A.
    Rgba8888 = 1,
    /// 32-bit colour, byte order B, G, R, A.
    Bgra8888 = 2,
}

impl DisplayFormat {
    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Bytes per pixel in this format.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba8888 | Self::Bgra8888 => 4,
        }
    }
}

/// Mode-information record returned by [`Display::mode_info`].
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DisplayMode {
    /// Surface width in pixels.
    pub width_px: u32,
    /// Surface height in pixels.
    pub height_px: u32,
    /// Distance in bytes between the start of consecutive scanlines.
    pub stride_bytes: u32,
    /// Pixel encoding.
    pub format: DisplayFormat,
}

/// Trait every display driver implements.
///
/// # Capabilities
///
/// Every method is gated by ownership of the [`DriverHandle`] returned
/// from the driver's `register` entry point. The load-time grant of
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) is what
/// permits the host to issue that handle; the per-method dispatcher
/// re-verifies the handle on every call.
///
/// [`DriverHandle`]: crate::driver::DriverHandle
pub trait Display {
    /// Report the active mode.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if hardware enumeration fails.
    /// * [`DriverError::Unsupported`] if the driver was loaded into a
    ///   headless host that cannot expose a surface.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn mode_info(&self) -> Result<DisplayMode, DriverError>;

    /// Present a fully-rendered frame.
    ///
    /// `frame` must contain at least
    /// `mode_info()?.stride_bytes * mode_info()?.height_px` bytes laid
    /// out in the format reported by [`Self::mode_info`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `frame` is shorter than
    ///   the active mode requires.
    /// * [`DriverError::DeviceFault`] if the underlying hardware
    ///   rejected the present.
    /// * [`DriverError::Busy`] if a previous present is still in
    ///   flight.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn present(&mut self, frame: &[u8]) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_byte_width() {
        assert_eq!(DisplayFormat::Rgba8888.bytes_per_pixel(), 4);
        assert_eq!(DisplayFormat::Bgra8888.bytes_per_pixel(), 4);
    }

    #[test]
    fn format_discriminants_are_frozen() {
        assert_eq!(DisplayFormat::Rgba8888.as_u8(), 1);
        assert_eq!(DisplayFormat::Bgra8888.as_u8(), 2);
    }

    struct MockDisplay {
        mode: DisplayMode,
        frame_len_seen: core::cell::Cell<usize>,
    }

    impl Display for MockDisplay {
        fn mode_info(&self) -> Result<DisplayMode, DriverError> {
            Ok(self.mode)
        }

        fn present(&mut self, frame: &[u8]) -> Result<(), DriverError> {
            let required = (self.mode.stride_bytes as usize) * (self.mode.height_px as usize);
            if frame.len() < required {
                return Err(DriverError::BufferTooSmall);
            }
            self.frame_len_seen.set(frame.len());
            Ok(())
        }
    }

    #[test]
    fn trait_is_object_safe_and_callable() {
        let mut d = MockDisplay {
            mode: DisplayMode {
                width_px: 4,
                height_px: 2,
                stride_bytes: 16,
                format: DisplayFormat::Rgba8888,
            },
            frame_len_seen: core::cell::Cell::new(0),
        };
        let dyn_ref: &mut dyn Display = &mut d;
        let Ok(info) = dyn_ref.mode_info() else {
            unreachable!("mock always succeeds")
        };
        assert_eq!(info.width_px, 4);
        assert_eq!(dyn_ref.present(&[0u8; 8]), Err(DriverError::BufferTooSmall));
        assert!(dyn_ref.present(&[0u8; 32]).is_ok());
        assert_eq!(d.frame_len_seen.get(), 32);
    }
}
