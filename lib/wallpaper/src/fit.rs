//! The one wallpaper placement geometry: pure, total, and shared by the
//! desktop renderer and the chooser's preview.
//!
//! [`place`] answers, for a source image of a given pixel size and a screen
//! of a given pixel size, exactly how a [`WallpaperFit`] draws it: the
//! destination rectangle on screen, the source rectangle sampled into it,
//! and whether the source repeats. Nothing here decodes, resamples, or
//! touches a pixel — it is arithmetic only, so a preview can never disagree
//! with the desktop about what a fit does.

use tairix_geometry::Rect;

use crate::settings::WallpaperFit;

/// How a source image is drawn to fill a screen under a [`WallpaperFit`].
///
/// The three fields are jointly sufficient to draw every fit: blit
/// [`Self::source`] (sampled at whatever scale maps it onto
/// [`Self::destination`]) into [`Self::destination`], repeating it across
/// the screen when [`Self::tiled`] is set. There is no separate "scale"
/// field — the scale is implied by the two rectangles' relative sizes —
/// and no fit can be expressed outside these three fields, so a consumer
/// cannot mis-draw a placement it did not itself invent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    destination: Rect,
    source: Rect,
    tiled: bool,
}

impl Placement {
    /// Where on the screen the (possibly scaled) source is drawn.
    ///
    /// For every fit but [`WallpaperFit::Fit`] and [`WallpaperFit::Centre`]
    /// with a source smaller than the screen, this is the whole screen.
    #[must_use]
    pub fn destination(&self) -> Rect {
        self.destination
    }

    /// The source-image rectangle, in source pixels, sampled into
    /// [`Self::destination`] (or, when [`Self::tiled`] is set, the one tile
    /// repeated across it).
    #[must_use]
    pub fn source(&self) -> Rect {
        self.source
    }

    /// Whether [`Self::source`] is repeated (at 1:1) across
    /// [`Self::destination`], rather than drawn once (at whatever scale
    /// maps it onto the destination).
    #[must_use]
    pub fn tiled(&self) -> bool {
        self.tiled
    }
}

/// Place a `source`-sized image onto a `screen`-sized surface under `fit`.
///
/// Returns `None` only when `source` or `screen` has a zero width or
/// height — there is no placement of nothing, and no placement onto
/// nothing. Every dimension up to `u32::MAX` in either input is handled:
/// all arithmetic is carried in `u64` and every result is clamped back into
/// range, so this never panics and never divides by zero, however extreme
/// the aspect ratios involved. A crop or letterbox that mathematically
/// wants less than one source pixel is clamped up to one — the discrete
/// arithmetic's only sane answer at an aspect ratio a screen and a source
/// cannot both satisfy exactly — rather than degenerating to an empty
/// rectangle nothing could draw.
#[must_use]
pub fn place(source: (u32, u32), screen: (u32, u32), fit: WallpaperFit) -> Option<Placement> {
    let (src_w, src_h) = source;
    let (screen_w, screen_h) = screen;
    if src_w == 0 || src_h == 0 || screen_w == 0 || screen_h == 0 {
        return None;
    }

    let screen_rect = Rect::new(0, 0, screen_w, screen_h);
    let source_rect = Rect::new(0, 0, src_w, src_h);

    let placement = match fit {
        WallpaperFit::Stretch => Placement {
            destination: screen_rect,
            source: source_rect,
            tiled: false,
        },
        WallpaperFit::Tile => Placement {
            destination: screen_rect,
            source: source_rect,
            tiled: true,
        },
        WallpaperFit::Fill => Placement {
            destination: screen_rect,
            source: cover_crop(src_w, src_h, screen_w, screen_h),
            tiled: false,
        },
        WallpaperFit::Fit => Placement {
            destination: contain_rect(src_w, src_h, screen_w, screen_h),
            source: source_rect,
            tiled: false,
        },
        WallpaperFit::Centre => {
            let rect_w = src_w.min(screen_w);
            let rect_h = src_h.min(screen_h);
            Placement {
                destination: centred(rect_w, rect_h, screen_w, screen_h),
                source: centred(rect_w, rect_h, src_w, src_h),
                tiled: false,
            }
        }
    };
    Some(placement)
}

/// The source pixel box a decoder need only produce to satisfy `fit`,
/// drawing a source of native size `source` onto `screen`.
///
/// Deliberately never more than [`Placement::source`]'s own size (a fit
/// never samples more source detail than it crops in) nor more than
/// [`Placement::destination`]'s size (drawing never needs more source
/// pixels than the screen positions they land on), except under
/// [`WallpaperFit::Tile`], which draws every source pixel at 1:1 and so
/// needs the source at its full native size regardless of the screen.
/// Returns `None` exactly when [`place`] would.
#[must_use]
pub fn decode_target(
    source: (u32, u32),
    screen: (u32, u32),
    fit: WallpaperFit,
) -> Option<(u32, u32)> {
    let placement = place(source, screen, fit)?;
    if placement.tiled() {
        return Some(source);
    }
    let src = placement.source();
    let dst = placement.destination();
    Some((src.width.min(dst.width), src.height.min(dst.height)))
}

/// Clamp `value` into `1..=max`, converting down from `u64` to `u32`.
///
/// `value` is always non-negative and, by the caller's own construction,
/// never exceeds what `max` already bounds; the explicit clamp exists so a
/// degenerate rounding-to-zero at an extreme aspect ratio (or, in
/// principle, a conversion failure) can never produce a zero-sized or
/// out-of-range rectangle.
fn clamp_dimension(value: u64, max: u32) -> u32 {
    let capped = value.min(u64::from(max));
    u32::try_from(capped).unwrap_or(max).max(1)
}

/// The midpoint offset centring a `size`-wide span inside a `bound`-wide
/// one, as a screen coordinate. `size` is always at most `bound` by
/// construction, so the subtraction never underflows; the conversion to
/// `i32` saturates to `0` rather than panicking at the extreme upper end of
/// `u32`'s range.
fn centring_offset(size: u32, bound: u32) -> i32 {
    i32::try_from((bound - size) / 2).unwrap_or(0)
}

/// Whether `src` is relatively wider than `screen`: `src`'s aspect ratio
/// exceeds `screen`'s, compared by cross-multiplication so no floating
/// point or division is needed. `u64` products of two `u32` values never
/// overflow.
fn source_is_relatively_wider(
    src_w: u32,
    src_h: u32,
    screen_w: u32,
    screen_h: u32,
) -> core::cmp::Ordering {
    (u64::from(src_w) * u64::from(screen_h)).cmp(&(u64::from(src_h) * u64::from(screen_w)))
}

/// The cropped source rectangle [`WallpaperFit::Fill`] samples: centred,
/// matching `screen`'s aspect ratio exactly, keeping the full extent of
/// whichever source dimension is not the tighter constraint.
fn cover_crop(src_w: u32, src_h: u32, screen_w: u32, screen_h: u32) -> Rect {
    match source_is_relatively_wider(src_w, src_h, screen_w, screen_h) {
        core::cmp::Ordering::Greater => {
            // Source is relatively wider: crop width, keep full height.
            let cropped_w = clamp_dimension(
                u64::from(src_h) * u64::from(screen_w) / u64::from(screen_h),
                src_w,
            );
            Rect::new(centring_offset(cropped_w, src_w), 0, cropped_w, src_h)
        }
        core::cmp::Ordering::Less => {
            // Source is relatively taller: crop height, keep full width.
            let cropped_h = clamp_dimension(
                u64::from(src_w) * u64::from(screen_h) / u64::from(screen_w),
                src_h,
            );
            Rect::new(0, centring_offset(cropped_h, src_h), src_w, cropped_h)
        }
        core::cmp::Ordering::Equal => Rect::new(0, 0, src_w, src_h),
    }
}

/// The letterboxed destination rectangle [`WallpaperFit::Fit`] draws into:
/// centred on `screen`, the whole source visible, scaled by whichever of
/// the two candidate scales is the tighter (smaller) constraint.
fn contain_rect(src_w: u32, src_h: u32, screen_w: u32, screen_h: u32) -> Rect {
    let (dst_w, dst_h) = match source_is_relatively_wider(src_w, src_h, screen_w, screen_h) {
        core::cmp::Ordering::Greater => {
            // Width is the tighter constraint: full screen width, scaled height.
            let dst_h = clamp_dimension(
                u64::from(src_h) * u64::from(screen_w) / u64::from(src_w),
                screen_h,
            );
            (screen_w, dst_h)
        }
        core::cmp::Ordering::Less => {
            // Height is the tighter constraint: full screen height, scaled width.
            let dst_w = clamp_dimension(
                u64::from(src_w) * u64::from(screen_h) / u64::from(src_h),
                screen_w,
            );
            (dst_w, screen_h)
        }
        core::cmp::Ordering::Equal => (screen_w, screen_h),
    };
    Rect::new(
        centring_offset(dst_w, screen_w),
        centring_offset(dst_h, screen_h),
        dst_w,
        dst_h,
    )
}

/// A `size_w`x`size_h` rectangle centred inside a `bound_w`x`bound_h` one.
/// `size_w <= bound_w` and `size_h <= bound_h` by the caller's own
/// construction.
fn centred(size_w: u32, size_h: u32, bound_w: u32, bound_h: u32) -> Rect {
    Rect::new(
        centring_offset(size_w, bound_w),
        centring_offset(size_h, bound_h),
        size_w,
        size_h,
    )
}

#[cfg(test)]
#[path = "fit_tests.rs"]
mod tests;
