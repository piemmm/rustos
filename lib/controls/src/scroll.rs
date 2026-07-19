//! The orientation-independent scroll geometry engine.
//!
//! A scrollbar exposes one viewport's position within a larger content, and
//! the design language requires exactly one implementation of that behaviour
//! shared by the window-manager root viewport and by nested application
//! content, across both axes. This module is that one implementation: it
//! turns a validated [`ScrollRange`] into a draggable thumb ([`ScrollGeometry`])
//! and maps pointer, wheel, and keyboard input back to a clamped offset
//! ([`ScrollModel`]).
//!
//! # Units
//!
//! [`ScrollRange`] is expressed in the *logical scroll unit* the owning
//! viewport declares — pixels, rows, or application records — and content
//! extent, viewport extent, and offset all use that one unit; they are never
//! mixed implicitly. [`ScrollGeometry`], by contrast, works in the *physical
//! track length* (the pixels the thumb travels along), because thumb size and
//! position are a rendering concern. The two never mix: the range says "where
//! am I in the content", the geometry says "where is the thumb on screen".
//!
//! # Fail-closed
//!
//! Invalid, overflowing, or stale range data normalises to a non-draggable,
//! zero-offset scrollbar rather than producing out-of-bounds geometry. Every
//! division is guarded by a non-zero denominator, so no path panics.

/// Which axis a scrollbar lays out along.
///
/// The behaviour is identical on both axes; orientation only decides how the
/// owning viewport maps the computed one-dimensional [`ThumbSpan`] onto a
/// screen rectangle. Keeping it a single parameter is what lets one engine
/// serve the vertical and horizontal bars without a duplicated recipe.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ScrollOrientation {
    /// Scrolls the viewport's vertical offset; the thumb moves top to bottom.
    Vertical,
    /// Scrolls the viewport's horizontal offset; the thumb moves along the
    /// logical start-to-end axis.
    Horizontal,
}

/// A validated scroll range: how much content there is, how much of it the
/// viewport shows, and where the viewport currently sits.
///
/// The three quantities share the owning viewport's logical scroll unit. A
/// `ScrollRange` is always **normalised**: the offset never exceeds
/// [`max_offset`](ScrollRange::max_offset), and a viewport that covers its
/// content (or a degenerate zero-size viewport) pins the offset to zero. The
/// fields are private precisely so this invariant cannot be violated by
/// constructing an out-of-range value directly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScrollRange {
    content_extent: u64,
    viewport_extent: u64,
    offset: u64,
}

impl ScrollRange {
    /// An empty range: no content, no viewport, no offset. Not scrollable.
    pub const EMPTY: Self = Self {
        content_extent: 0,
        viewport_extent: 0,
        offset: 0,
    };

    /// Build a normalised range from raw extents and a desired offset.
    ///
    /// The offset is clamped into `0..=max_offset`; when the viewport is not
    /// smaller than the content (or is zero), the range is not scrollable and
    /// the offset is forced to zero.
    #[must_use]
    pub fn new(content_extent: u64, viewport_extent: u64, offset: u64) -> Self {
        let mut range = Self {
            content_extent,
            viewport_extent,
            offset: 0,
        };
        range.offset = if range.is_scrollable() {
            offset.min(range.max_offset())
        } else {
            0
        };
        range
    }

    /// The total content extent in the viewport's logical scroll unit.
    #[must_use]
    pub const fn content_extent(&self) -> u64 {
        self.content_extent
    }

    /// The visible viewport extent in the same unit.
    #[must_use]
    pub const fn viewport_extent(&self) -> u64 {
        self.viewport_extent
    }

    /// The current, already-clamped offset.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// `true` when the content is larger than the viewport, so the bar has a
    /// draggable range. A zero-size viewport is never scrollable (fail-closed).
    #[must_use]
    pub const fn is_scrollable(&self) -> bool {
        self.viewport_extent > 0 && self.content_extent > self.viewport_extent
    }

    /// The largest valid offset: `content - viewport` when scrollable, else 0.
    #[must_use]
    pub const fn max_offset(&self) -> u64 {
        if self.is_scrollable() {
            self.content_extent - self.viewport_extent
        } else {
            0
        }
    }

    /// The same range moved to `offset`, re-clamped to stay valid.
    #[must_use]
    pub fn with_offset(self, offset: u64) -> Self {
        Self::new(self.content_extent, self.viewport_extent, offset)
    }

    /// The same viewport position re-expressed against new extents, keeping
    /// the current offset clamped into the new valid range.
    ///
    /// This is what a viewport calls when its content grows or shrinks: the
    /// offset is preserved where it still fits and clamped where it no longer
    /// does, never left dangling past the new end.
    #[must_use]
    pub fn resize(self, content_extent: u64, viewport_extent: u64) -> Self {
        Self::new(content_extent, viewport_extent, self.offset)
    }
}

impl Default for ScrollRange {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// A validated range plus the line-step and page-step distances an input
/// gesture moves, all in the same logical scroll unit.
///
/// The model is the single source of truth for the viewport offset: pointer,
/// wheel, and keyboard input all flow through its step methods, which return a
/// new model with a re-clamped offset. The control never keeps a private
/// offset separate from this.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScrollModel {
    range: ScrollRange,
    line_step: u64,
    page_step: u64,
}

impl ScrollModel {
    /// Build a model from a range and its step distances.
    ///
    /// A zero step is permitted and simply moves nothing, so a viewport that
    /// has not declared a step fails closed to "no movement" rather than to a
    /// guessed distance.
    #[must_use]
    pub fn new(range: ScrollRange, line_step: u64, page_step: u64) -> Self {
        Self {
            range,
            line_step,
            page_step,
        }
    }

    /// The underlying validated range.
    #[must_use]
    pub const fn range(&self) -> ScrollRange {
        self.range
    }

    /// The current clamped offset.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.range.offset()
    }

    /// The line-step distance.
    #[must_use]
    pub const fn line_step(&self) -> u64 {
        self.line_step
    }

    /// The page-step distance.
    #[must_use]
    pub const fn page_step(&self) -> u64 {
        self.page_step
    }

    /// This model with the offset set to `offset`, re-clamped.
    #[must_use]
    pub fn scroll_to(self, offset: u64) -> Self {
        Self {
            range: self.range.with_offset(offset),
            ..self
        }
    }

    /// This model with the offset moved by a signed `delta`, saturating at the
    /// range bounds. A negative delta scrolls toward the start.
    #[must_use]
    pub fn scroll_by(self, delta: i64) -> Self {
        let current = i128::from(self.range.offset());
        let moved = current + i128::from(delta);
        let clamped = moved.clamp(0, i128::from(self.range.max_offset()));
        // `clamped` is within `0..=max_offset`, which fits a `u64`; the
        // fallback can never be reached but keeps the path panic-free.
        self.scroll_to(u64::try_from(clamped).unwrap_or(0))
    }

    /// Move one line toward the start.
    #[must_use]
    pub fn line_backward(self) -> Self {
        self.scroll_by(-signed_step(self.line_step))
    }

    /// Move one line toward the end.
    #[must_use]
    pub fn line_forward(self) -> Self {
        self.scroll_by(signed_step(self.line_step))
    }

    /// Move one page toward the start.
    #[must_use]
    pub fn page_backward(self) -> Self {
        self.scroll_by(-signed_step(self.page_step))
    }

    /// Move one page toward the end.
    #[must_use]
    pub fn page_forward(self) -> Self {
        self.scroll_by(signed_step(self.page_step))
    }

    /// Jump to the start of the content.
    #[must_use]
    pub fn to_start(self) -> Self {
        self.scroll_to(0)
    }

    /// Jump to the end of the content.
    #[must_use]
    pub fn to_end(self) -> Self {
        let end = self.range.max_offset();
        self.scroll_to(end)
    }

    /// Re-express the model against new content and viewport extents, keeping
    /// the current offset clamped and the step distances unchanged.
    #[must_use]
    pub fn resize(self, content_extent: u64, viewport_extent: u64) -> Self {
        Self {
            range: self.range.resize(content_extent, viewport_extent),
            ..self
        }
    }
}

/// A step distance as a non-negative `i64`, saturating a pathologically large
/// distance at `i64::MAX` so negation and addition stay in range.
fn signed_step(step: u64) -> i64 {
    i64::try_from(step).unwrap_or(i64::MAX)
}

/// A one-dimensional thumb: its start and length along the track, in physical
/// track pixels.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ThumbSpan {
    /// Distance from the track start to the thumb's near edge, in pixels.
    pub start: u32,
    /// The thumb's length along the track, in pixels.
    pub length: u32,
}

/// Which part of the track a coordinate falls in.
///
/// End buttons (decrement/increment) are laid out by the owning viewport with
/// its own theme metrics and are *not* part of the track this engine measures;
/// [`ScrollGeometry`] is given only the track span between them.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum TrackHit {
    /// The track region before the thumb — a page step toward the start.
    BeforeThumb,
    /// The thumb itself — the start of a drag.
    Thumb,
    /// The track region after the thumb — a page step toward the end.
    AfterThumb,
}

/// The painted geometry of a scrollbar thumb for a given track length.
///
/// Constructed from a [`ScrollRange`], the physical `track_len` between the end
/// controls, and the theme's minimum thumb length. All results are clamped to
/// the track, and a non-scrollable or zero-travel range is reported as
/// non-[`draggable`](ScrollGeometry::draggable) with a zero-offset thumb.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScrollGeometry {
    range: ScrollRange,
    track_len: u32,
    min_thumb: u32,
}

impl ScrollGeometry {
    /// Build the geometry for a track of `track_len` physical pixels, with a
    /// theme-supplied `min_thumb` minimum thumb length.
    #[must_use]
    pub fn new(range: ScrollRange, track_len: u32, min_thumb: u32) -> Self {
        Self {
            range,
            track_len,
            min_thumb,
        }
    }

    /// The range this geometry was built from.
    #[must_use]
    pub const fn range(&self) -> ScrollRange {
        self.range
    }

    /// The full track length in pixels.
    #[must_use]
    pub const fn track_len(&self) -> u32 {
        self.track_len
    }

    /// The thumb length in pixels: proportional to the fraction of the content
    /// the viewport shows, never smaller than the theme minimum (bounded by the
    /// track), and never longer than the track. A non-scrollable range fills
    /// the whole track with a non-draggable thumb.
    #[must_use]
    pub fn thumb_length(&self) -> u32 {
        if self.track_len == 0 {
            return 0;
        }
        if !self.range.is_scrollable() {
            return self.track_len;
        }
        let track = u128::from(self.track_len);
        // viewport < content (scrollable), so this is strictly less than the
        // track length. `u128` keeps the product from overflowing for any
        // `u64` extents.
        let proportional = track * u128::from(self.range.viewport_extent())
            / u128::from(self.range.content_extent());
        let floor = u128::from(self.min_thumb).min(track).max(1);
        let length = proportional.max(floor).min(track);
        // `length <= track == track_len`, so this always fits.
        u32::try_from(length).unwrap_or(self.track_len)
    }

    /// The distance the thumb can travel: `track_len - thumb_length`.
    #[must_use]
    pub fn travel(&self) -> u32 {
        self.track_len.saturating_sub(self.thumb_length())
    }

    /// `true` when the thumb can be dragged: the range is scrollable and the
    /// thumb does not already fill the whole track.
    #[must_use]
    pub fn draggable(&self) -> bool {
        self.range.is_scrollable() && self.travel() > 0
    }

    /// The thumb's current span along the track, derived from the range offset.
    #[must_use]
    pub fn thumb(&self) -> ThumbSpan {
        let length = self.thumb_length();
        let start = if self.draggable() {
            let travel = u128::from(self.travel());
            let max_offset = u128::from(self.range.max_offset());
            // `draggable()` guarantees both `travel` and `max_offset` are > 0.
            let start = travel * u128::from(self.range.offset()) / max_offset;
            // `start <= travel == travel()`, so this always fits.
            u32::try_from(start).unwrap_or_else(|_| self.travel())
        } else {
            0
        };
        ThumbSpan { start, length }
    }

    /// Classify a track-relative coordinate (0 at the track start).
    #[must_use]
    pub fn hit(&self, pos: u32) -> TrackHit {
        let thumb = self.thumb();
        if pos < thumb.start {
            TrackHit::BeforeThumb
        } else if pos < thumb.start.saturating_add(thumb.length) {
            TrackHit::Thumb
        } else {
            TrackHit::AfterThumb
        }
    }

    /// The content offset a thumb whose near edge sits at `thumb_start` (in
    /// track pixels) represents. The inverse of [`thumb`](ScrollGeometry::thumb),
    /// rounded to the nearest offset and clamped to the valid range.
    #[must_use]
    pub fn offset_for_thumb_start(&self, thumb_start: u32) -> u64 {
        if !self.draggable() {
            return 0;
        }
        let travel = u128::from(self.travel());
        let clamped = u128::from(thumb_start).min(travel);
        let max_offset = u128::from(self.range.max_offset());
        // `travel > 0` (draggable); round to nearest so the extremes map to 0
        // and `max_offset` exactly. `u128` keeps the product in range.
        let offset = (clamped * max_offset + travel / 2) / travel;
        u64::try_from(offset.min(max_offset)).unwrap_or(self.range.max_offset())
    }

    /// The content offset implied by a thumb drag.
    ///
    /// `pointer_pos` is the pointer's current position along the track and
    /// `anchor` is the pointer-to-thumb-start distance captured when the drag
    /// began (`pointer_pos_at_start - thumb_start_at_start`). Preserving the
    /// anchor keeps the content from jumping when the drag begins, and the
    /// implied thumb start is clamped into the track before mapping, so a drag
    /// past either end pins to that end rather than overflowing.
    #[must_use]
    pub fn offset_for_drag(&self, pointer_pos: i32, anchor: i32) -> u64 {
        if !self.draggable() {
            return 0;
        }
        let travel = i64::from(self.travel());
        let thumb_start = i64::from(pointer_pos) - i64::from(anchor);
        let clamped = thumb_start.clamp(0, travel);
        // `clamped` is within `0..=travel == travel()`, so this always fits.
        self.offset_for_thumb_start(u32::try_from(clamped).unwrap_or_else(|_| self.travel()))
    }
}
