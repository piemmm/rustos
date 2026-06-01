//! Integer screen geometry: points and rectangles (`lib/geometry`).
//!
//! This crate is the single shared definition of the desktop's integer
//! coordinate types. It is consumed by the compositing window manager
//! (`userland/gui/wm`), the taskbar (`userland/gui/taskbar`), and the
//! default graphical apps, none of which may depend on one another
//! (`AGENTS.md` §17.4); shared code therefore lives in `lib/*` (§6), exactly
//! as `lib/theme` is the shared home for design tokens.
//!
//! Screen coordinates are signed (`i32`): a window may be positioned partly
//! off the top or left edge. Sizes are unsigned (`u32`). All arithmetic is
//! overflow-checked through `i64`/`u32` widening so a pathological
//! coordinate fails closed rather than wrapping (`AGENTS.md` §2.9). There is
//! no rendering or compositing arithmetic here — that is the window
//! manager's job — so nothing is duplicated (§2.2).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

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
}

/// The unsigned distance `high - low`, clamped at zero, as `u32`.
fn span(low: i32, high: i32) -> u32 {
    u32::try_from(i64::from(high) - i64::from(low)).unwrap_or(0)
}

#[cfg(test)]
mod tests;
