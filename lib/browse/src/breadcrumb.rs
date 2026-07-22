//! Breadcrumb path-bar *placement* (`plans/NEW-FILEMANAGER.md` `FM4b`).
//!
//! The pure geometry that turns the [`chrome::breadcrumbs`](crate::chrome)
//! crumb list into the drawn, clickable path bar: given each crumb's rendered
//! label width, [`layout`] places the crumbs left-to-right, separated by a
//! fixed gap, and **right-anchors** the whole strip so the current directory
//! (the terminal crumb) is always fully visible at the right edge. When the
//! crumbs are wider than the bar the leading ancestors simply scroll off the
//! left and are clipped — the deepest, most relevant crumbs stay on screen,
//! and no ancestor is dropped from the placement, so the hit-test can still
//! resolve a click on any visible part of a partially-clipped crumb.
//!
//! Both the painter and the pointer hit-test consume this one placement, so a
//! click always lands on exactly the crumb the user saw (§2.2). The module is
//! font-agnostic — it works in pixel widths a caller measures — so it is
//! host-tested with synthetic widths and never needs a rasteriser.

use alloc::vec::Vec;

/// The text drawn between two adjacent crumbs. The leading and trailing spaces
/// give the clickable labels breathing room; the separator itself is inert
/// (the hit-test resolves only the crumb labels, never the gap).
pub const SEPARATOR: &str = " > ";

/// One crumb placed within the path bar.
///
/// [`index`](Self::index) is the crumb's position in the input slice (so the
/// caller recovers its label, depth, and current-flag); [`x`](Self::x) is the
/// crumb label's left edge in path-bar pixels — it may be negative when the
/// strip is right-anchored and this crumb has scrolled off the left, in which
/// case the painter clips it against the surface and the hit-test ignores the
/// off-screen part. [`width`](Self::width) is the label's rendered width.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PlacedCrumb {
    /// Position of this crumb in the slice passed to [`layout`].
    pub index: usize,
    /// Left edge of the crumb label, in path-bar pixels (may be negative).
    pub x: i32,
    /// Rendered width of the crumb label, in pixels.
    pub width: u32,
}

impl PlacedCrumb {
    /// Whether path-bar pixel column `px` falls on this crumb's on-screen
    /// label. A column left of `0` cannot hit anything, so a crumb clipped off
    /// the left edge only answers for the part still visible.
    #[must_use]
    fn contains(&self, px: i32, bar_width: u32) -> bool {
        if px < 0 || i64::from(px) >= i64::from(bar_width) {
            return false;
        }
        let end = i64::from(self.x).saturating_add(i64::from(self.width));
        i64::from(px) >= i64::from(self.x) && i64::from(px) < end
    }
}

/// Place the crumbs whose label widths are `widths` (root-first) within a path
/// bar `bar_width` pixels wide, insetting the strip by `pad` at each end and
/// leaving `sep_width` pixels between adjacent crumbs.
///
/// The strip is laid out left-to-right and then shifted so its right edge sits
/// at `bar_width - pad`: when everything fits it starts at `pad`; when it does
/// not, the start goes negative and the leading crumbs scroll off the left
/// (clipped by the caller), keeping the current directory visible. Every crumb
/// is placed regardless of visibility, so the hit-test can resolve any crumb
/// that is even partially on screen. An empty slice yields no placements.
#[must_use]
pub fn layout(widths: &[u32], bar_width: u32, pad: u32, sep_width: u32) -> Vec<PlacedCrumb> {
    if widths.is_empty() {
        return Vec::new();
    }
    let usable = i64::from(bar_width).saturating_sub(i64::from(pad).saturating_mul(2));
    // Total width of the whole strip: every label plus one separator between
    // each adjacent pair.
    let mut full: i64 = 0;
    for (i, &w) in widths.iter().enumerate() {
        if i > 0 {
            full = full.saturating_add(i64::from(sep_width));
        }
        full = full.saturating_add(i64::from(w));
    }
    // Right-anchor: when the strip overflows, `start` is < pad (often
    // negative) so the strip slides left and its tail (the current directory)
    // stays flush with the right inset.
    let start = if full <= usable.max(0) {
        i64::from(pad)
    } else {
        i64::from(pad).saturating_add(usable).saturating_sub(full)
    };
    let mut placed = Vec::with_capacity(widths.len());
    let mut offset: i64 = 0;
    for (index, &width) in widths.iter().enumerate() {
        if index > 0 {
            offset = offset.saturating_add(i64::from(sep_width));
        }
        placed.push(PlacedCrumb {
            index,
            x: i64_to_i32(start.saturating_add(offset)),
            width,
        });
        offset = offset.saturating_add(i64::from(width));
    }
    placed
}

/// The index (into the `widths`/crumb slice) of the crumb whose on-screen
/// label contains path-bar pixel column `px`, or `None` for a separator gap, a
/// column past the strip, or a crumb clipped off the left. `bar_width` bounds
/// the visible path bar so an off-screen coordinate never resolves.
#[must_use]
pub fn crumb_at(placed: &[PlacedCrumb], px: i32, bar_width: u32) -> Option<usize> {
    placed
        .iter()
        .find(|crumb| crumb.contains(px, bar_width))
        .map(|crumb| crumb.index)
}

/// Saturating `i64` → `i32`.
fn i64_to_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}
