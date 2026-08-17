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
//! [`ChannelOrder::encode`] — a whole run of it through
//! [`ChannelOrder::encode_run`], which is that same encoder in bulk — and
//! neither pretends to be the other.

use tairix_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode, MAX_DAMAGE_RECTS};
use tairix_geometry::{Rect, Region};
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

    /// Encode a run of premultiplied pixels into `out`, returning the number
    /// of whole pixels written.
    ///
    /// The bulk form of [`encode`](Self::encode) — `out` is walked in whole
    /// four-byte groups and each is handed to that one encoder, so a run and
    /// a single pixel can never disagree about a channel. There is no
    /// bulk-copy special case for the order that already matches the pixel in
    /// memory: [`Pixel`] carries no layout guarantee to copy through, and it
    /// would buy nothing if it did, since a matching order is already a
    /// four-byte move per pixel with no shuffle left for the optimiser to
    /// remove.
    ///
    /// Writes `min(pixels.len(), out.len() / 4)` pixels and reports that
    /// count. A short `out` truncates rather than refusing or overrunning, a
    /// trailing group of fewer than four bytes is left alone rather than
    /// half-written, and a longer `out` keeps its tail untouched. The caller
    /// — the window manager, passing one row-span slice of its back buffer
    /// and the frame bytes for that same span — compares the count against
    /// `pixels.len()` to see whether the span it asked for is the span that
    /// reached the frame.
    #[must_use]
    pub fn encode_run(self, pixels: &[Pixel], out: &mut [u8]) -> usize {
        let (groups, _trailing) = out.as_chunks_mut::<4>();
        let mut written = 0;
        for (slot, pixel) in groups.iter_mut().zip(pixels) {
            *slot = self.encode(*pixel);
            written += 1;
        }
        written
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
    let damage = as_damage(bounds)?;
    if damage.covers(mode) {
        return None;
    }
    Some(damage)
}

/// One screen rectangle as the wire's damage rectangle, or `None` when it is
/// empty or has an edge the wire's unsigned fields cannot express.
fn as_damage(rect: &Rect) -> Option<DamageRect> {
    if rect.is_empty() {
        return None;
    }
    Some(DamageRect {
        x: u32::try_from(rect.left()).ok()?,
        y: u32::try_from(rect.top()).ok()?,
        width_px: rect.width,
        height_px: rect.height,
    })
}

/// Fill `out` with the damage list a present carries for `region`, and
/// return the filled prefix — or `None` to present the whole frame instead.
///
/// `region` is the frame's *disjoint* damage, so each rectangle in the list
/// is blitted once and no pixel twice. There are three answers, and this is
/// the one place that chooses between them:
///
/// * the region's own rectangles, whenever they fit the list — the point of
///   the exercise, so two far-apart updates stay two small blits;
/// * its bounding rectangle, when there are more rectangles than the list
///   holds (or, defensively, one cannot be expressed) — over-covering costs
///   pixels, dropping a rectangle would leave stale ones on screen;
/// * `None`, when the damage genuinely covers the surface.
///
/// **Covering and spanning are not the same thing.** Two opposite corners of
/// the screen have a bounding box the size of the screen while changing
/// eight pixels; naming them is exactly what the list is for, so the
/// whole-frame answer is reserved for damage that really is the surface.
///
/// Call it with a non-empty `region`; an empty one names nothing to present.
#[must_use]
pub fn damage_list<'a>(
    region: &Region,
    mode: &DisplayMode,
    out: &'a mut [DamageRect; MAX_DAMAGE_RECTS],
) -> Option<&'a [DamageRect]> {
    let mut count = 0;
    if region.rects().len() <= out.len() {
        for (slot, rect) in out.iter_mut().zip(region.rects()) {
            let Some(damage) = as_damage(rect) else {
                count = 0;
                break;
            };
            *slot = damage;
            count += 1;
        }
    }
    if count == 1 && out[0].covers(mode) {
        return None;
    }
    if count == 0 {
        out[0] = sub_screen_damage(&region.bounds(), mode)?;
        count = 1;
    }
    Some(&out[..count])
}

#[cfg(test)]
#[path = "scanout_tests.rs"]
mod tests;
