//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is the rendered content of one window (or of the
//! compositor's back buffer). It is defined once in the shared
//! [`tairix_raster`] crate (`lib/raster`) so the taskbar — which may not
//! depend on the window manager — paints into the same
//! type without duplication. This module re-exports it so the
//! rest of the compositor keeps referring to `crate::surface::Surface`.

use tairix_raster::{blend_solid_span, blend_span, DitherRow};

use crate::color::Pixel;

pub use tairix_raster::surface::Surface;

/// Blend `src`, whose first pixel is screen column `src_x`, over the part of
/// `dst` it reaches, `dst` beginning at screen column `dst_x`; report how many
/// columns overlapped.
///
/// This is the one place a composited layer meets the shared span blend: the
/// desktop layer, a window's client run, and each furniture strip are all a
/// slice at a screen column, and clipping one to the segment being composed is
/// the only thing that differs between them. The count is the overlap, not the
/// pixels the blend chose to touch, because a transparent source still *is* a
/// contribution the frame composed.
pub(crate) fn blend_run(
    dst: &mut [Pixel],
    dst_x: i32,
    src: &[Pixel],
    src_x: i32,
    factor: u8,
    dither: DitherRow,
) -> u64 {
    let Some(Overlap {
        dst,
        taken,
        column,
        len,
    }) = overlap(dst, dst_x, src_x, src.len())
    else {
        return 0;
    };
    let Some(src) = src.get(taken..taken.saturating_add(len)) else {
        return 0;
    };
    blend_span(dst, src, factor, dither, column);
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Blend the single colour `src` over the `cols` columns beginning at screen
/// column `src_x`, clipped to `dst`, which begins at screen column `dst_x`;
/// report how many columns overlapped.
///
/// [`blend_run`] for a run that has a colour behind it rather than a slice:
/// the backdrop plate a decorated window lays under its client wherever the
/// client's own pixels do not reach.
pub(crate) fn fill_run(
    dst: &mut [Pixel],
    dst_x: i32,
    src: Pixel,
    src_x: i32,
    cols: usize,
    factor: u8,
    dither: DitherRow,
) -> u64 {
    let Some(Overlap {
        dst, column, len, ..
    }) = overlap(dst, dst_x, src_x, cols)
    else {
        return 0;
    };
    blend_solid_span(dst, src, factor, dither, column);
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// The part of `dst` — beginning at screen column `dst_x` — that a run of
/// `cols` columns beginning at screen column `src_x` reaches.
struct Overlap<'a> {
    /// The destination pixels the run covers.
    dst: &'a mut [Pixel],
    /// How far into the run the overlap starts.
    taken: usize,
    /// The screen column the overlap starts at.
    column: u32,
    /// How many columns overlap.
    len: usize,
}

/// Clip a run of `cols` columns at screen column `src_x` to `dst`, which
/// begins at screen column `dst_x`; `None` when they do not meet.
fn overlap(dst: &mut [Pixel], dst_x: i32, src_x: i32, cols: usize) -> Option<Overlap<'_>> {
    let (Ok(dst_len), Ok(src_len)) = (i64::try_from(dst.len()), i64::try_from(cols)) else {
        return None;
    };
    let (dst_x, src_x) = (i64::from(dst_x), i64::from(src_x));
    let from = dst_x.max(src_x);
    let until = (dst_x + dst_len).min(src_x + src_len);
    let (Ok(len), Ok(into), Ok(taken), Ok(column)) = (
        usize::try_from(until - from),
        usize::try_from(from - dst_x),
        usize::try_from(from - src_x),
        u32::try_from(from),
    ) else {
        return None;
    };
    let dst = dst.get_mut(into..into.saturating_add(len))?;
    Some(Overlap {
        dst,
        taken,
        column,
        len,
    })
}

/// Row `y` of `surface` left to right, or an empty slice when the row is out
/// of bounds — a row that does not exist simply draws nothing.
///
/// [`Surface`] exposes a mutable row accessor but not a read-only one, so
/// both the plain-content row sampler and the decorated-window furniture
/// sampler (`crate::window`, `crate::chrome`) share this one row-major index
/// computation rather than each re-deriving it.
pub(crate) fn row(surface: &Surface, y: u32) -> &[Pixel] {
    let width = usize::try_from(surface.width()).unwrap_or(0);
    let Some(start) = usize::try_from(y).ok().and_then(|y| y.checked_mul(width)) else {
        return &[];
    };
    let Some(end) = start.checked_add(width) else {
        return &[];
    };
    surface.pixels().get(start..end).unwrap_or(&[])
}
