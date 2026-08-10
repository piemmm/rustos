//! Integer screen geometry: points and rectangles (`lib/geometry`).
//!
//! This crate is the single shared definition of the desktop's integer
//! coordinate types. It is consumed by the compositing window manager
//! (`userland/gui/wm`), the taskbar (`userland/gui/taskbar`), and the
//! default graphical apps, none of which may depend on one another; shared code therefore lives in `lib/*`, exactly
//! as `lib/theme` is the shared home for design tokens.
//!
//! Screen coordinates are signed (`i32`): a window may be positioned partly
//! off the top or left edge. Sizes are unsigned (`u32`). All arithmetic is
//! overflow-checked through `i64`/`u32` widening so a pathological
//! coordinate fails closed rather than wrapping.
//!
//! [`Region`] extends the vocabulary from a single rectangle to a *set* of
//! pixels held as disjoint rectangles: which pixels changed, which a control
//! owns, which survive a clip. That is geometry, not rendering — the colour
//! algebra stays in `lib/raster` — so damage tracking and control repaint
//! reporting share one definition instead of a copy each.
//!
//! The crate also owns [`Scale`], the desktop's single DPI / UI scale factor:
//! desktop lengths are authored in *logical* pixels at a reference density
//! and converted to a panel's *physical* pixels through one shared
//! [`Scale::scale_length`], so variable-DPI support is not re-implemented per
//! consumer.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

mod region;
mod scale;

pub use region::Region;
pub use scale::{Scale, REFERENCE_DPI};

/// A point in screen space.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Point {
    /// Horizontal coordinate, pixels from the left edge.
    pub x: i32,
    /// Vertical coordinate, pixels from the top edge.
    pub y: i32,
}

impl Point {
    /// The screen origin (top-left).
    pub const ORIGIN: Self = Self { x: 0, y: 0 };

    /// Construct a point.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle: a top-left [`Point`] and a size.
///
/// A rectangle with a zero `width` or `height` is *empty*; emptiness is
/// the canonical "covers nothing" value used throughout damage tracking
/// and clipping.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    /// Top-left corner.
    pub origin: Point,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// The empty rectangle at the origin.
    pub const EMPTY: Self = Self {
        origin: Point::ORIGIN,
        width: 0,
        height: 0,
    };

    /// Construct a rectangle from a position and size.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            origin: Point::new(x, y),
            width,
            height,
        }
    }

    /// `true` when the rectangle covers no pixels.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Left edge (inclusive).
    #[must_use]
    pub const fn left(&self) -> i32 {
        self.origin.x
    }

    /// Top edge (inclusive).
    #[must_use]
    pub const fn top(&self) -> i32 {
        self.origin.y
    }

    /// Right edge (exclusive).
    #[must_use]
    pub fn right(&self) -> i32 {
        self.origin.x.saturating_add_unsigned(self.width)
    }

    /// Bottom edge (exclusive).
    #[must_use]
    pub fn bottom(&self) -> i32 {
        self.origin.y.saturating_add_unsigned(self.height)
    }

    /// The intersection of two rectangles, or [`Rect::EMPTY`] if they
    /// do not overlap.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        if self.is_empty() || other.is_empty() {
            return Self::EMPTY;
        }
        let left = self.left().max(other.left());
        let top = self.top().max(other.top());
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left || bottom <= top {
            return Self::EMPTY;
        }
        Self::new(left, top, span(left, right), span(top, bottom))
    }

    /// The smallest rectangle covering both operands. The union of an
    /// empty rectangle with `r` is `r`.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let left = self.left().min(other.left());
        let top = self.top().min(other.top());
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(left, top, span(left, right), span(top, bottom))
    }

    /// `true` when `point` lies inside the half-open rectangle.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.left()
            && point.x < self.right()
            && point.y >= self.top()
            && point.y < self.bottom()
    }

    /// This rectangle's size, translated so it lies wholly inside `screen`.
    ///
    /// The size is preserved; only the origin moves — pulled back along each
    /// axis just far enough that the far edge stops at `screen`'s far edge,
    /// then pushed forward so the near edge does not cross `screen`'s near
    /// edge. A rectangle larger than `screen` on an axis pins to `screen`'s
    /// leading edge on that axis rather than centring or overflowing the far
    /// side. This is the one placement rule the taskbar's menus and an app's
    /// popup surface both clamp with, so a surface anchored near a screen
    /// edge is never drawn partly off-screen.
    #[must_use]
    pub fn clamped_onto(&self, screen: Rect) -> Self {
        let x = clamp_axis(self.origin.x, self.width, screen.left(), screen.right());
        let y = clamp_axis(self.origin.y, self.height, screen.top(), screen.bottom());
        Self::new(x, y, self.width, self.height)
    }
}

/// Shift `origin` so `[origin, origin + extent)` stays within `[low, high)`,
/// pinning to `low` when the extent does not fit.
fn clamp_axis(origin: i32, extent: u32, low: i32, high: i32) -> i32 {
    let max = high.saturating_sub(to_i32(extent));
    origin.min(max).max(low)
}

/// The unsigned distance `high - low`, clamped at zero, as `u32`.
pub(crate) fn span(low: i32, high: i32) -> u32 {
    u32::try_from(i64::from(high) - i64::from(low)).unwrap_or(0)
}

/// A `u32` extent as an `i32` coordinate, saturating rather than wrapping.
///
/// Sizes are unsigned while coordinates are signed, so every consumer that
/// turns a width, height, or other extent into a coordinate to add or
/// compare needs the same saturating rule; an extent at or past `i32::MAX`
/// clamps to it rather than wrapping negative, which a downstream bounds
/// check would fail to catch.
#[must_use]
pub fn to_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod region_tests;
#[cfg(test)]
mod tests;
