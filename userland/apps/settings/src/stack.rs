//! Where a column of stacked plates is drawn, and the gap between them.
//!
//! Both bodies that fill the pane column stack plates down it — a form its
//! groups, the storage pane its volume cards — and both scroll by whole
//! plates rather than by pixels, because a plate is *placed* on the surface
//! rather than clipped to it: one given a negative top draws nothing and
//! hit-tests as nothing, so sliding the column up by pixels would make the
//! plate above the fold vanish instead of scroll.
//!
//! That placement is therefore one definition, read by the measurement, the
//! paint and the hit test of both.

use alloc::vec::Vec;

use tairix_geometry::{to_i32, Rect, Scale};
use tairix_theme::Theme;

/// The gap between stacked plates, and between them and the column's own
/// edges: the theme's control gap, so a pane breathes at whatever density
/// the desktop is drawn at.
pub(crate) fn gap(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().control_gap).max(1)
}

/// The width a plate takes in a column `width` pixels wide: the column less
/// the gap either side of it.
///
/// One definition, read by the placement below and by whatever measures a
/// plate's height — a group's rows wrap into that width, so measuring
/// against a different one would reserve the wrong height.
pub(crate) fn plate_width(width: u32, scale: Scale, theme: &Theme) -> u32 {
    width.saturating_sub(gap(scale, theme).saturating_mul(2))
}

/// Where each plate from `first` is drawn down `bounds`, given `count`
/// plates whose heights `height` answers.
///
/// Only the plates that fit whole are placed: one half off the bottom would
/// draw its content over the window's edge, and a reader cannot press a row
/// they cannot see. The first always draws, however short the column —
/// seating nothing at all would be a blank window.
pub(crate) fn place(
    bounds: Rect,
    first: usize,
    count: usize,
    scale: Scale,
    theme: &Theme,
    height: impl Fn(usize) -> u32,
) -> Vec<(usize, Rect)> {
    let gap = gap(scale, theme);
    let width = plate_width(bounds.width, scale, theme);
    let limit = bounds.bottom();
    let mut top = bounds.top().saturating_add(to_i32(gap));
    let mut placed = Vec::with_capacity(count.saturating_sub(first));
    for index in first..count {
        let plate = height(index);
        let rect = Rect::new(bounds.left().saturating_add(to_i32(gap)), top, width, plate);
        if rect.bottom() > limit && index > first {
            break;
        }
        placed.push((index, rect));
        top = top.saturating_add(to_i32(plate.saturating_add(gap)));
    }
    placed
}

/// The first plate to draw from so that plate `index` is seated in `bounds`.
///
/// A plate above the window becomes the first drawn; one below it becomes
/// the last. Each step may seat a different number of plates, so the count
/// is re-asked rather than assumed uniform.
pub(crate) fn reveal_from(
    first: usize,
    index: usize,
    bounds: Rect,
    count: usize,
    scale: Scale,
    theme: &Theme,
    height: impl Fn(usize) -> u32 + Copy,
) -> usize {
    if index < first {
        return index;
    }
    let mut want = first;
    while want < index
        && !place(bounds, want, count, scale, theme, height)
            .iter()
            .any(|(seated, _)| *seated == index)
    {
        want = want.saturating_add(1);
    }
    want
}

/// A plate count as a scroll extent.
///
/// A column of more plates than a [`u64`] can count is not one this surface
/// could draw, so the saturation is unreachable rather than lossy.
pub(crate) fn as_extent(plates: usize) -> u64 {
    u64::try_from(plates).unwrap_or(u64::MAX)
}
