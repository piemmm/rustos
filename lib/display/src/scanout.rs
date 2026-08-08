//! Turning composited pixels into the bytes a scan-out frame holds.
//!
//! Every program that presents through [`crate::RemoteDisplay`] faces the
//! same three questions: how big is the frame for this mode, which byte
//! order does this format want, and is this damage rectangle a sub-region
//! or the whole screen. The answers are pixel-exact — a wrong channel order
//! renders the display in false colour, and a damage rectangle that lies
//! shows stale pixels — so there is one definition of each here, shared by
//! the compositing window manager and the graphical login screen rather
//! than copied into both.
//!
//! What is *not* here is the composition itself. A compositor blending many
//! windows through a back buffer and a program blitting one surface are
//! genuinely different loops; both encode their result through
//! [`ChannelOrder::encode`], and neither pretends to be the other.

use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use tairix_geometry::Rect;
use tairix_raster::Pixel;

/// The byte order a display format wants each pixel written in.
///
/// A format whose byte order this crate does not know is refused at
/// configure time rather than guessed, because guessing renders the whole
/// screen in false colour.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ChannelOrder {
    /// Red, green, blue, alpha.
    Rgba,
    /// Blue, green, red, alpha.
    Bgra,
}

impl ChannelOrder {
    /// The order `format` wants, or `None` for a format with no software
    /// encoding here.
    ///
    /// The format set is open, so a caller resolves this once at configure
    /// time and refuses the mode on `None` rather than reaching a pixel it
    /// does not know how to write.
    #[must_use]
    pub const fn for_format(format: DisplayFormat) -> Option<Self> {
        match format {
            DisplayFormat::Rgba8888 => Some(Self::Rgba),
            DisplayFormat::Bgra8888 => Some(Self::Bgra),
            _ => None,
        }
    }

    /// Encode one premultiplied pixel into its four scan-out bytes.
    ///
    /// The screen is opaque, so a premultiplied channel already equals its
    /// straight-alpha form and no un-premultiply is needed.
    #[must_use]
    pub const fn encode(self, pixel: Pixel) -> [u8; 4] {
        match self {
            Self::Rgba => [pixel.r, pixel.g, pixel.b, pixel.a],
            Self::Bgra => [pixel.b, pixel.g, pixel.r, pixel.a],
        }
    }
}

/// The length in bytes of one scan-out frame for `mode`.
///
/// `None` when the mode cannot describe a frame at all: a zero extent, a
/// stride too small for one scanline, or a size that overflows `usize` —
/// each a mode to refuse rather than a buffer to guess the size of. The
/// caller allocates, so a program reusing a frame across mode changes is
/// not forced through a fresh allocation.
#[must_use]
pub fn scanout_len(mode: &DisplayMode) -> Option<usize> {
    let min_stride = mode.width_px.checked_mul(mode.format.bytes_per_pixel())?;
    if mode.stride_bytes < min_stride || mode.width_px == 0 || mode.height_px == 0 {
        return None;
    }
    let len = u64::from(mode.stride_bytes).checked_mul(u64::from(mode.height_px))?;
    usize::try_from(len).ok()
}

/// Convert already-screen-clipped damage `bounds` into the [`DamageRect`] a
/// region present carries.
///
/// `None` means "present the whole frame instead": the bounds cover the
/// screen, are empty, or — defensively — do not fit the wire fields. A
/// rectangle that cannot be expressed exactly falls back to the full
/// present rather than presenting a wrong region.
#[must_use]
pub fn sub_screen_damage(bounds: &Rect, mode: &DisplayMode) -> Option<DamageRect> {
    if bounds.is_empty() {
        return None;
    }
    let damage = DamageRect {
        x: u32::try_from(bounds.left()).ok()?,
        y: u32::try_from(bounds.top()).ok()?,
        width_px: bounds.width,
        height_px: bounds.height,
    };
    if damage.covers(mode) {
        return None;
    }
    Some(damage)
}

#[cfg(test)]
#[path = "scanout_tests.rs"]
mod tests;
