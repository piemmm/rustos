//! Damage tracking: the set of screen rectangles that changed.
//!
//! The compositor only has to recompute the pixels that changed since
//! the last frame. A [`DamageRegion`] accumulates the dirty rectangles
//! (a window moved, a surface was repainted) so [`composite`] can skip
//! the untouched majority of the screen.
//!
//! The representation is a list of rectangles plus their cached
//! bounding box. Overlapping rectangles are coalesced into their union
//! on [`add`](DamageRegion::add): recomposition is per rectangle, so a
//! region marked dirty more than once (the same window rectangle pushed
//! by an open, a focus, and a surface update, or a window's old and new
//! position after a move) must not become the *same pixels composited
//! several times* — at a full window's size that is the difference
//! between one recomposite and four. Disjoint rectangles (a window and
//! the far-away taskbar) stay separate, so coalescing never inflates the
//! work to the whole screen either. The union of two overlapping
//! rectangles can cover a few pixels in neither original; recompositing
//! them is harmless (composition reads the live window state and is
//! idempotent), and it is far cheaper than revisiting the overlap twice.
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
    ///
    /// `rect` is coalesced with every already-dirty rectangle it
    /// overlaps, growing to their union, so the same region is never
    /// recorded — and later recomposited — more than once. A rectangle
    /// disjoint from all current damage is kept as its own entry, so
    /// unrelated updates never merge into one screen-spanning rectangle.
    pub fn add(&mut self, mut rect: Rect) {
        if rect.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.rects.len() {
            if overlaps(&self.rects[i], &rect) {
                rect = rect.union(&self.rects[i]);
                // Order carries no meaning: each rectangle is composited
                // independently, so a swap-remove is safe. Restart the
                // scan because the grown rectangle may now reach earlier
                // rectangles it did not before.
                self.rects.swap_remove(i);
                i = 0;
            } else {
                i += 1;
            }
        }
        self.bounds = self.bounds.union(&rect);
        self.rects.push(rect);
    }

    /// The coalesced dirty rectangles, in no particular order (each is
    /// composited independently, so their order is immaterial).
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

/// `true` when `a` and `b` share at least one pixel. Rectangles that
/// merely touch along an edge do not overlap and are left separate.
fn overlaps(a: &Rect, b: &Rect) -> bool {
    !a.intersection(b).is_empty()
}
