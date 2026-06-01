//! Damage tracking: the set of screen rectangles that changed.
//!
//! The compositor only has to recompute the pixels that changed since
//! the last frame. A [`DamageRegion`] accumulates the dirty rectangles
//! (a window moved, a surface was repainted) so [`composite`] can skip
//! the untouched majority of the screen.
//!
//! The representation is deliberately simple — a list of rectangles
//! plus their cached bounding box. It never merges overlapping
//! rectangles (that would cost more than the pixels saved at this
//! scale); a pixel covered by two damage rectangles is simply visited
//! by composition under each, which is idempotent.
//!
//! [`composite`]: crate::Compositor::composite

use alloc::vec::Vec;

use crate::geometry::{Point, Rect};

/// An accumulating set of dirty screen rectangles.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DamageRegion {
    rects: Vec<Rect>,
    bounds: Rect,
}

impl DamageRegion {
    /// An empty damage region.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rects: Vec::new(),
            bounds: Rect::EMPTY,
        }
    }

    /// `true` when nothing is marked dirty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Mark `rect` dirty. Empty rectangles are ignored.
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        self.bounds = self.bounds.union(&rect);
        self.rects.push(rect);
    }

    /// The dirty rectangles, in the order they were added.
    #[must_use]
    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// The bounding box of every dirty rectangle, or [`Rect::EMPTY`].
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// `true` when `point` lies inside any dirty rectangle.
    #[must_use]
    pub fn covers(&self, point: Point) -> bool {
        self.rects.iter().any(|r| r.contains(point))
    }

    /// Forget all damage (called after a frame is presented).
    pub fn clear(&mut self) {
        self.rects.clear();
        self.bounds = Rect::EMPTY;
    }
}
