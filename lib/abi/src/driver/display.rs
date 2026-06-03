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

/// What an [`AcceleratedDisplay`] back-end can composite in hardware.
///
/// Reported by [`AcceleratedDisplay::accel_caps`] so the compositor can
/// decide, per frame, whether the hardware path can serve the current
/// window stack or whether it must fall back to the software
/// [`Display::present`] path (`AGENTS.md` §10 — the software path is
/// always the fallback).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AccelCaps {
    /// Maximum number of layers the hardware can composite in a single
    /// [`AcceleratedDisplay::present_layers`] call. A back-end always
    /// supports at least one layer.
    pub max_layers: u32,
    /// Largest layer width, in pixels, the hardware can source.
    pub max_width_px: u32,
    /// Largest layer height, in pixels, the hardware can source.
    pub max_height_px: u32,
    /// Whether the hardware can scale a whole layer's contribution by a
    /// constant [`AccelLayer::opacity`] in addition to its per-pixel
    /// alpha. A back-end that reports `false` requires every layer to
    /// carry `opacity == 255`.
    pub per_layer_opacity: bool,
}

/// One source layer in an [`AcceleratedDisplay::present_layers`] call.
///
/// Layers are composited back-to-front: index `0` is the bottom-most
/// layer and the last layer is the top-most, each placed at its
/// destination origin and blended *over* the layers beneath it.
///
/// `pixels` is **premultiplied-alpha** pixel data in the display's
/// active [`DisplayMode::format`], stored row-major with `stride_bytes`
/// between consecutive scanlines. Premultiplied alpha matches the
/// desktop compositor's pixel model (`AGENTS.md` §10), so a layer can be
/// handed to the hardware without an extra straight-alpha conversion.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AccelLayer<'a> {
    /// Premultiplied-alpha source pixels in the display's active format.
    pub pixels: &'a [u8],
    /// Layer width in pixels.
    pub width_px: u32,
    /// Layer height in pixels.
    pub height_px: u32,
    /// Bytes between consecutive scanlines of `pixels`. Must hold at
    /// least `width_px * format.bytes_per_pixel()` bytes.
    pub stride_bytes: u32,
    /// X position of the layer's top-left pixel in the scan-out surface.
    /// May be negative; the off-screen portion is clipped.
    pub dst_x: i32,
    /// Y position of the layer's top-left pixel in the scan-out surface.
    /// May be negative; the off-screen portion is clipped.
    pub dst_y: i32,
    /// Constant opacity applied to the whole layer, `0` fully
    /// transparent, `255` fully opaque (per-pixel alpha only). Requires
    /// [`AccelCaps::per_layer_opacity`] unless it is `255`.
    pub opacity: u8,
}

impl AccelLayer<'_> {
    /// Number of source bytes the layer's geometry requires
    /// (`stride_bytes * height_px`), or [`DriverError::LengthOutOfRange`]
    /// if the product overflows the host's address width.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if the byte count overflows.
    pub fn required_len(&self, format: DisplayFormat) -> Result<usize, DriverError> {
        let min_stride = self
            .width_px
            .checked_mul(format.bytes_per_pixel())
            .ok_or(DriverError::LengthOutOfRange)?;
        if self.width_px == 0 || self.height_px == 0 || self.stride_bytes < min_stride {
            return Err(DriverError::LengthOutOfRange);
        }
        let bytes = u64::from(self.stride_bytes)
            .checked_mul(u64::from(self.height_px))
            .ok_or(DriverError::LengthOutOfRange)?;
        usize::try_from(bytes).map_err(|_| DriverError::LengthOutOfRange)
    }
}

/// A display back-end that can composite a stack of layers in hardware.
///
/// This is the optional GPU-accelerated path (`AGENTS.md` §10). A driver
/// that exposes it composites the supplied [`AccelLayer`] stack with its
/// own fixed-function or programmable compositor (e.g. the Raspberry Pi
/// `VideoCore` HVS plane engine) and scans the result out itself, so the
/// host never composites the whole screen in software. Every
/// `AcceleratedDisplay` is also a [`Display`]: the software
/// full-frame [`Display::present`] path remains available as the
/// mandatory fallback when the layer stack exceeds [`AccelCaps`].
///
/// # Capabilities
///
/// Every method is gated by ownership of the driver's
/// [`DriverHandle`](crate::driver::DriverHandle), exactly as for
/// [`Display`].
pub trait AcceleratedDisplay: Display {
    /// Report what the hardware compositor can do this frame.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if querying the hardware fails.
    /// * [`DriverError::Unsupported`] if acceleration is unavailable on
    ///   this device even though the trait is implemented.
    fn accel_caps(&self) -> Result<AccelCaps, DriverError>;

    /// Composite `layers` back-to-front and scan the result out.
    ///
    /// Index `0` is the bottom layer; the last entry is on top. The
    /// scan-out surface is cleared to opaque black before the first
    /// layer is blended, so callers need not supply a full-screen base
    /// layer.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if more layers are supplied
    ///   than [`AccelCaps::max_layers`], or a layer's geometry exceeds
    ///   [`AccelCaps::max_width_px`] / [`AccelCaps::max_height_px`].
    /// * [`DriverError::BufferTooSmall`] if a layer's `pixels` is
    ///   shorter than its geometry requires.
    /// * [`DriverError::Unsupported`] if a layer requests a per-layer
    ///   [`AccelLayer::opacity`] the hardware cannot apply.
    /// * [`DriverError::DeviceFault`] if the hardware rejected the
    ///   present.
    /// * [`DriverError::Busy`] if a previous present is still in flight.
    fn present_layers(&mut self, layers: &[AccelLayer<'_>]) -> Result<(), DriverError>;
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

    fn layer(width: u32, height: u32, stride: u32) -> AccelLayer<'static> {
        AccelLayer {
            pixels: &[],
            width_px: width,
            height_px: height,
            stride_bytes: stride,
            dst_x: 0,
            dst_y: 0,
            opacity: 255,
        }
    }

    #[test]
    fn layer_required_len_uses_stride_times_height() {
        // 4-px BGRA scanline needs 16 bytes; stride 20 × 3 rows = 60.
        let l = layer(4, 3, 20);
        assert_eq!(l.required_len(DisplayFormat::Bgra8888), Ok(60));
    }

    #[test]
    fn layer_rejects_degenerate_or_narrow_geometry() {
        assert_eq!(
            layer(0, 3, 16).required_len(DisplayFormat::Rgba8888),
            Err(DriverError::LengthOutOfRange)
        );
        assert_eq!(
            layer(4, 0, 16).required_len(DisplayFormat::Rgba8888),
            Err(DriverError::LengthOutOfRange)
        );
        // Stride too small to hold one 4-px scanline (needs 16).
        assert_eq!(
            layer(4, 3, 12).required_len(DisplayFormat::Rgba8888),
            Err(DriverError::LengthOutOfRange)
        );
    }

    /// A back-end that records the layer stack it was asked to present
    /// and validates each layer against its reported caps.
    struct MockAccel {
        mode: DisplayMode,
        caps: AccelCaps,
        layers_seen: core::cell::Cell<usize>,
    }

    impl Display for MockAccel {
        fn mode_info(&self) -> Result<DisplayMode, DriverError> {
            Ok(self.mode)
        }
        fn present(&mut self, _frame: &[u8]) -> Result<(), DriverError> {
            Ok(())
        }
    }

    impl AcceleratedDisplay for MockAccel {
        fn accel_caps(&self) -> Result<AccelCaps, DriverError> {
            Ok(self.caps)
        }
        fn present_layers(&mut self, layers: &[AccelLayer<'_>]) -> Result<(), DriverError> {
            if layers.len() > self.caps.max_layers as usize {
                return Err(DriverError::LengthOutOfRange);
            }
            for l in layers {
                let need = l.required_len(self.mode.format)?;
                if l.pixels.len() < need {
                    return Err(DriverError::BufferTooSmall);
                }
                if l.opacity != 255 && !self.caps.per_layer_opacity {
                    return Err(DriverError::Unsupported);
                }
            }
            self.layers_seen.set(layers.len());
            Ok(())
        }
    }

    fn mock_accel() -> MockAccel {
        MockAccel {
            mode: DisplayMode {
                width_px: 4,
                height_px: 2,
                stride_bytes: 16,
                format: DisplayFormat::Bgra8888,
            },
            caps: AccelCaps {
                max_layers: 2,
                max_width_px: 64,
                max_height_px: 64,
                per_layer_opacity: false,
            },
            layers_seen: core::cell::Cell::new(0),
        }
    }

    #[test]
    fn accelerated_present_accepts_well_formed_layers() {
        let mut d = mock_accel();
        let pixels = [0u8; 16]; // 4×1 BGRA, stride 16.
        let dyn_ref: &mut dyn AcceleratedDisplay = &mut d;
        assert_eq!(
            dyn_ref.accel_caps().map(|c| c.max_layers),
            Ok(2),
            "caps round-trip"
        );
        let layers = [AccelLayer {
            pixels: &pixels,
            width_px: 4,
            height_px: 1,
            stride_bytes: 16,
            dst_x: 0,
            dst_y: 0,
            opacity: 255,
        }];
        assert!(dyn_ref.present_layers(&layers).is_ok());
        assert_eq!(d.layers_seen.get(), 1);
    }

    #[test]
    fn accelerated_present_fails_closed_on_over_budget_and_short_and_opacity() {
        let mut d = mock_accel();
        let pixels = [0u8; 16];
        let one = AccelLayer {
            pixels: &pixels,
            width_px: 4,
            height_px: 1,
            stride_bytes: 16,
            dst_x: 0,
            dst_y: 0,
            opacity: 255,
        };
        // Three layers exceeds max_layers == 2.
        assert_eq!(
            d.present_layers(&[one, one, one]),
            Err(DriverError::LengthOutOfRange)
        );
        // A layer whose pixels are shorter than its geometry.
        let short = AccelLayer {
            pixels: &pixels[..8],
            ..one
        };
        assert_eq!(d.present_layers(&[short]), Err(DriverError::BufferTooSmall));
        // Per-layer opacity the hardware does not support.
        let faded = AccelLayer {
            opacity: 128,
            ..one
        };
        assert_eq!(d.present_layers(&[faded]), Err(DriverError::Unsupported));
    }
}
