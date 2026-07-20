//! Root-viewport scrollbars: the window-manager furniture that exposes a
//! window's scroll position through the one shared scroll geometry engine.
//!
//! A top-level window may declare a **root viewport** — window-level
//! scrollbars the window manager composes and owns, rather than application
//! controls the client draws inside its own content. The design language
//! requires both the window-level bars here and any nested application bars to
//! use the same range validation, thumb math, and input behaviour, over one
//! implementation rather than separate recipes; this module is the
//! window-manager consumer of that shared engine ([`tairix_controls::scroll`]),
//! and the viewer app is the independent nested consumer.
//!
//! A [`RootViewport`] carries a [`ScrollModel`] per axis (either may be
//! absent), a visibility [`ScrollPolicy`], and the theme-derived scrollbar
//! breadth and minimum thumb length in **physical** pixels. From a window's
//! screen [`Rect`] it derives a [`FurnitureLayout`] — the client rectangle and
//! the track/corner rectangles — and classifies a screen point into a
//! [`FurnitureHit`]. Because the layout reserves a stable gutter under
//! [`ScrollPolicy::ReservedGutter`], the client can never receive a pointer
//! event that lands on the furniture, and the furniture never overlaps the
//! client (the separate furniture hit map the design language mandates).
//!
//! Everything here is pure integer geometry over the shared engine: no
//! rendering, no allocation, and no panic — an axis with no bar, a
//! zero-size window, or a degenerate track simply yields no furniture and no
//! movement (fail closed).

use tairix_controls::{ScrollGeometry, ScrollModel, ScrollOrientation, TrackHit};

use crate::geometry::{Point, Rect};

/// How a root viewport's scrollbars occupy space relative to the client.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ScrollPolicy {
    /// A present bar always reserves a quiet gutter of the theme breadth, so
    /// the client rectangle shrinks by that breadth on the barred edge. The
    /// gutter is stable: it does not appear or vanish as the content grows or
    /// shrinks by a pixel, so content never shifts under an interaction.
    ReservedGutter,
    /// A present bar floats over the client edge; the client keeps the full
    /// window rectangle and the bar overlays its trailing/bottom strip.
    Overlay,
}

/// Which part of a window's furniture a point falls in.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum FurnitureHit {
    /// The client area — the point belongs to the application surface.
    Client,
    /// The vertical scrollbar, and which region of its track.
    Vertical(TrackHit),
    /// The horizontal scrollbar, and which region of its track.
    Horizontal(TrackHit),
    /// The scroll corner at a two-bar junction (a neutral, non-scrolling
    /// cell; a resize grabber replaces it on a resizable window later).
    Corner,
}

/// The rectangles a [`RootViewport`] occupies within a window's bounds.
///
/// Every rectangle is in screen coordinates. The client rectangle is the area
/// the application surface is clipped to; the track and corner rectangles are
/// the window-manager furniture. Under [`ScrollPolicy::ReservedGutter`] the
/// client is shrunk by the barred edges so it never overlaps the furniture;
/// under [`ScrollPolicy::Overlay`] the client keeps the full window rectangle
/// and the bars float over its edge strips.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FurnitureLayout {
    /// The area the application surface is clipped to.
    pub client: Rect,
    /// The vertical scrollbar track, when a vertical bar is present.
    pub vertical_track: Option<Rect>,
    /// The horizontal scrollbar track, when a horizontal bar is present.
    pub horizontal_track: Option<Rect>,
    /// The corner cell at a two-bar junction, when both bars are present.
    pub corner: Option<Rect>,
}

/// A window's window-manager-owned scrollbars over the shared scroll engine.
///
/// Either axis may be absent; a present axis carries the single
/// [`ScrollModel`] that owns its offset (the furniture never keeps a private
/// offset beside it). Breadth and minimum thumb length are the theme metrics
/// already converted to **physical** pixels for this window's output, so the
/// window manager needs no theme dependency to lay the furniture out.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RootViewport {
    vertical: Option<ScrollModel>,
    horizontal: Option<ScrollModel>,
    policy: ScrollPolicy,
    breadth: u32,
    min_thumb: u32,
}

impl RootViewport {
    /// Build a root viewport with no bars yet, the given visibility `policy`,
    /// and the physical scrollbar `breadth` and `min_thumb` length.
    #[must_use]
    pub const fn new(policy: ScrollPolicy, breadth: u32, min_thumb: u32) -> Self {
        Self {
            vertical: None,
            horizontal: None,
            policy,
            breadth,
            min_thumb,
        }
    }

    /// This viewport with a vertical bar driven by `model`.
    #[must_use]
    pub const fn with_vertical(mut self, model: ScrollModel) -> Self {
        self.vertical = Some(model);
        self
    }

    /// This viewport with a horizontal bar driven by `model`.
    #[must_use]
    pub const fn with_horizontal(mut self, model: ScrollModel) -> Self {
        self.horizontal = Some(model);
        self
    }

    /// The vertical bar's scroll model, if a vertical bar is present.
    #[must_use]
    pub const fn vertical(&self) -> Option<ScrollModel> {
        self.vertical
    }

    /// The horizontal bar's scroll model, if a horizontal bar is present.
    #[must_use]
    pub const fn horizontal(&self) -> Option<ScrollModel> {
        self.horizontal
    }

    /// The scroll model for `orientation`, if that bar is present.
    #[must_use]
    pub const fn model(&self, orientation: ScrollOrientation) -> Option<ScrollModel> {
        match orientation {
            ScrollOrientation::Vertical => self.vertical,
            ScrollOrientation::Horizontal => self.horizontal,
        }
    }

    /// The declared visibility policy.
    #[must_use]
    pub const fn policy(&self) -> ScrollPolicy {
        self.policy
    }

    /// The physical scrollbar breadth (the gutter/strip width).
    #[must_use]
    pub const fn breadth(&self) -> u32 {
        self.breadth
    }

    /// The physical minimum thumb length.
    #[must_use]
    pub const fn min_thumb(&self) -> u32 {
        self.min_thumb
    }

    /// Whether `orientation`'s bar reserves a gutter that shrinks the client.
    const fn reserves(&self, orientation: ScrollOrientation) -> bool {
        matches!(self.policy, ScrollPolicy::ReservedGutter) && self.model(orientation).is_some()
    }

    /// The furniture rectangles for a window whose screen rectangle is
    /// `bounds`.
    ///
    /// A bar occupies the trailing (vertical) or bottom (horizontal) edge
    /// strip of the theme breadth; when both bars are present the bottom-right
    /// breadth-square is the corner and neither track runs into it. A zero
    /// breadth or an absent axis contributes no rectangle.
    #[must_use]
    pub fn layout(&self, bounds: Rect) -> FurnitureLayout {
        let show_v = self.vertical.is_some() && self.breadth > 0;
        let show_h = self.horizontal.is_some() && self.breadth > 0;
        let b = self.breadth;

        let gutter_v = if self.reserves(ScrollOrientation::Vertical) {
            b
        } else {
            0
        };
        let gutter_h = if self.reserves(ScrollOrientation::Horizontal) {
            b
        } else {
            0
        };
        let client = Rect::new(
            bounds.left(),
            bounds.top(),
            bounds.width.saturating_sub(gutter_v),
            bounds.height.saturating_sub(gutter_h),
        );

        // A track leaves the corner square free for the other bar when both
        // are present, so the corner never overlaps a thumb.
        let corner_reserve = if show_v && show_h { b } else { 0 };
        let vertical_track = show_v.then(|| {
            Rect::new(
                bounds.right() - i32::try_from(b).unwrap_or(0),
                bounds.top(),
                b,
                bounds.height.saturating_sub(corner_reserve),
            )
        });
        let horizontal_track = show_h.then(|| {
            Rect::new(
                bounds.left(),
                bounds.bottom() - i32::try_from(b).unwrap_or(0),
                bounds.width.saturating_sub(corner_reserve),
                b,
            )
        });
        let corner = (show_v && show_h).then(|| {
            Rect::new(
                bounds.right() - i32::try_from(b).unwrap_or(0),
                bounds.bottom() - i32::try_from(b).unwrap_or(0),
                b,
                b,
            )
        });
        FurnitureLayout {
            client,
            vertical_track,
            horizontal_track,
            corner,
        }
    }

    /// The scroll geometry for `orientation` given this window's `bounds`, or
    /// `None` when that bar is absent or its track is degenerate.
    ///
    /// Returns the track rectangle alongside the geometry so the caller can
    /// map a screen coordinate onto the track without re-deriving the layout.
    #[must_use]
    pub fn track_and_geometry(
        &self,
        bounds: Rect,
        orientation: ScrollOrientation,
    ) -> Option<(Rect, ScrollGeometry)> {
        let model = self.model(orientation)?;
        let layout = self.layout(bounds);
        let track = match orientation {
            ScrollOrientation::Vertical => layout.vertical_track?,
            ScrollOrientation::Horizontal => layout.horizontal_track?,
        };
        let track_len = match orientation {
            ScrollOrientation::Vertical => track.height,
            ScrollOrientation::Horizontal => track.width,
        };
        Some((
            track,
            ScrollGeometry::new(model.range(), track_len, self.min_thumb),
        ))
    }

    /// Classify a screen `point` within a window whose bounds are `bounds`.
    ///
    /// Furniture is tested before the client, so a point on a bar or the
    /// corner is never reported as [`FurnitureHit::Client`] — the furniture
    /// hit map the design language requires, keeping outer-frame input away
    /// from the application surface.
    #[must_use]
    pub fn hit_test(&self, bounds: Rect, point: Point) -> FurnitureHit {
        let layout = self.layout(bounds);
        if let Some(corner) = layout.corner {
            if corner.contains(point) {
                return FurnitureHit::Corner;
            }
        }
        if let Some(track) = layout.vertical_track {
            if track.contains(point) {
                let along = u32::try_from(point.y - track.top()).unwrap_or(0);
                let geometry = ScrollGeometry::new(
                    self.vertical.map(|m| m.range()).unwrap_or_default(),
                    track.height,
                    self.min_thumb,
                );
                return FurnitureHit::Vertical(geometry.hit(along));
            }
        }
        if let Some(track) = layout.horizontal_track {
            if track.contains(point) {
                let along = u32::try_from(point.x - track.left()).unwrap_or(0);
                let geometry = ScrollGeometry::new(
                    self.horizontal.map(|m| m.range()).unwrap_or_default(),
                    track.width,
                    self.min_thumb,
                );
                return FurnitureHit::Horizontal(geometry.hit(along));
            }
        }
        FurnitureHit::Client
    }

    /// Apply wheel `dx`/`dy` ticks (one line step per tick) to the present
    /// bars, returning whether any offset changed.
    pub fn wheel(&mut self, dx: i32, dy: i32) -> bool {
        let mut changed = false;
        if dx != 0 {
            changed |= self.scroll_axis(ScrollOrientation::Horizontal, dx);
        }
        if dy != 0 {
            changed |= self.scroll_axis(ScrollOrientation::Vertical, dy);
        }
        changed
    }

    /// Move `orientation` by one line step in the chosen direction, returning
    /// whether the offset changed.
    pub fn line(&mut self, orientation: ScrollOrientation, forward: bool) -> bool {
        self.update(orientation, |model| {
            if forward {
                model.line_forward()
            } else {
                model.line_backward()
            }
        })
    }

    /// Move `orientation` by one page step in the chosen direction, returning
    /// whether the offset changed.
    pub fn page(&mut self, orientation: ScrollOrientation, forward: bool) -> bool {
        self.update(orientation, |model| {
            if forward {
                model.page_forward()
            } else {
                model.page_backward()
            }
        })
    }

    /// Jump `orientation` to the start of its content, returning whether the
    /// offset changed.
    pub fn to_start(&mut self, orientation: ScrollOrientation) -> bool {
        self.update(orientation, ScrollModel::to_start)
    }

    /// Jump `orientation` to the end of its content, returning whether the
    /// offset changed.
    pub fn to_end(&mut self, orientation: ScrollOrientation) -> bool {
        self.update(orientation, ScrollModel::to_end)
    }

    /// Set `orientation`'s offset directly (re-clamped), returning whether it
    /// changed. Used by a thumb drag mapped through [`Self::track_and_geometry`].
    pub fn set_offset(&mut self, orientation: ScrollOrientation, offset: u64) -> bool {
        self.update(orientation, |model| model.scroll_to(offset))
    }

    /// Re-express `orientation`'s range against new content and viewport
    /// extents (its step distances unchanged), returning whether the offset
    /// changed. This is what a viewport calls when its content or client size
    /// changes; the offset is preserved where it still fits and clamped where
    /// it does not, so a range change mid-drag never yields an invalid offset.
    pub fn resize(
        &mut self,
        orientation: ScrollOrientation,
        content_extent: u64,
        viewport_extent: u64,
    ) -> bool {
        self.update(orientation, |model| {
            model.resize(content_extent, viewport_extent)
        })
    }

    /// Apply one line step per `ticks` to `orientation`.
    fn scroll_axis(&mut self, orientation: ScrollOrientation, ticks: i32) -> bool {
        self.update(orientation, |model| {
            let step = i64::try_from(model.line_step()).unwrap_or(i64::MAX);
            model.scroll_by(i64::from(ticks).saturating_mul(step))
        })
    }

    /// Replace `orientation`'s model with `change(model)` when that bar is
    /// present, returning whether the offset changed.
    fn update(
        &mut self,
        orientation: ScrollOrientation,
        change: impl FnOnce(ScrollModel) -> ScrollModel,
    ) -> bool {
        let slot = match orientation {
            ScrollOrientation::Vertical => &mut self.vertical,
            ScrollOrientation::Horizontal => &mut self.horizontal,
        };
        let Some(model) = *slot else {
            return false;
        };
        let before = model.offset();
        let updated = change(model);
        let changed = updated.offset() != before;
        *slot = Some(updated);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::{FurnitureHit, RootViewport, ScrollPolicy};
    use crate::geometry::{Point, Rect};
    use tairix_controls::{ScrollModel, ScrollOrientation, ScrollRange, TrackHit};

    const BREADTH: u32 = 14;
    const MIN_THUMB: u32 = 24;
    const BOUNDS: Rect = Rect::new(100, 50, 200, 300);

    /// A vertical model: 1000 units of content, a 300-unit viewport at the
    /// top, 10-unit lines and 100-unit pages.
    fn vertical_model() -> ScrollModel {
        ScrollModel::new(ScrollRange::new(1000, 300, 0), 10, 100)
    }

    /// A horizontal model: 800 units of content, a 200-unit viewport.
    fn horizontal_model() -> ScrollModel {
        ScrollModel::new(ScrollRange::new(800, 200, 0), 10, 100)
    }

    fn both_bars() -> RootViewport {
        RootViewport::new(ScrollPolicy::ReservedGutter, BREADTH, MIN_THUMB)
            .with_vertical(vertical_model())
            .with_horizontal(horizontal_model())
    }

    #[test]
    fn reserved_gutter_shrinks_the_client_on_each_barred_edge() {
        let layout = both_bars().layout(BOUNDS);
        assert_eq!(layout.client.width, BOUNDS.width - BREADTH);
        assert_eq!(layout.client.height, BOUNDS.height - BREADTH);
        assert_eq!(layout.client.left(), BOUNDS.left());
        assert_eq!(layout.client.top(), BOUNDS.top());
    }

    #[test]
    fn overlay_policy_keeps_the_full_client() {
        let viewport = RootViewport::new(ScrollPolicy::Overlay, BREADTH, MIN_THUMB)
            .with_vertical(vertical_model())
            .with_horizontal(horizontal_model());
        let layout = viewport.layout(BOUNDS);
        assert_eq!(layout.client.width, BOUNDS.width);
        assert_eq!(layout.client.height, BOUNDS.height);
        // The bars still exist as furniture over the edge strips.
        assert!(layout.vertical_track.is_some());
        assert!(layout.horizontal_track.is_some());
    }

    #[test]
    fn corner_exists_only_with_both_bars_and_overlaps_neither_track() {
        let both = both_bars().layout(BOUNDS);
        let corner = both.corner.expect("both bars present");
        let vtrack = both.vertical_track.expect("vertical present");
        let htrack = both.horizontal_track.expect("horizontal present");
        assert!(corner.intersection(&vtrack).is_empty());
        assert!(corner.intersection(&htrack).is_empty());

        // A single bar leaves no corner and spans the full edge.
        let only_v = RootViewport::new(ScrollPolicy::ReservedGutter, BREADTH, MIN_THUMB)
            .with_vertical(vertical_model())
            .layout(BOUNDS);
        assert!(only_v.corner.is_none());
        assert_eq!(only_v.vertical_track.expect("v").height, BOUNDS.height);
    }

    #[test]
    fn a_point_in_the_gutter_is_furniture_never_client() {
        let viewport = both_bars();
        let layout = viewport.layout(BOUNDS);
        // A point in the vertical gutter, above the corner.
        let in_v_gutter = Point::new(BOUNDS.right() - 1, BOUNDS.top() + 5);
        assert!(!layout.client.contains(in_v_gutter));
        assert!(matches!(
            viewport.hit_test(BOUNDS, in_v_gutter),
            FurnitureHit::Vertical(_)
        ));
        // A point deep in the client is the client's.
        let in_client = Point::new(BOUNDS.left() + 5, BOUNDS.top() + 5);
        assert_eq!(viewport.hit_test(BOUNDS, in_client), FurnitureHit::Client);
        // The junction is the corner, not a track.
        let in_corner = Point::new(BOUNDS.right() - 1, BOUNDS.bottom() - 1);
        assert_eq!(viewport.hit_test(BOUNDS, in_corner), FurnitureHit::Corner);
    }

    #[test]
    fn hit_test_classifies_the_thumb_and_the_page_regions() {
        let viewport = both_bars();
        let (track, geometry) = viewport
            .track_and_geometry(BOUNDS, ScrollOrientation::Vertical)
            .expect("vertical bar");
        let thumb = geometry.thumb();
        // A point over the thumb classifies as the thumb.
        let on_thumb = Point::new(
            track.left() + 1,
            track.top() + i32::try_from(thumb.start).unwrap() + 1,
        );
        assert_eq!(
            viewport.hit_test(BOUNDS, on_thumb),
            FurnitureHit::Vertical(TrackHit::Thumb)
        );
        // A point below the thumb is the after-thumb page region (offset 0).
        let below = Point::new(
            track.left() + 1,
            track.top() + i32::try_from(thumb.start + thumb.length).unwrap() + 1,
        );
        assert_eq!(
            viewport.hit_test(BOUNDS, below),
            FurnitureHit::Vertical(TrackHit::AfterThumb)
        );
    }

    #[test]
    fn wheel_moves_one_line_per_tick_on_each_axis() {
        let mut viewport = both_bars();
        assert!(viewport.wheel(0, 3));
        assert_eq!(viewport.vertical().unwrap().offset(), 30); // 3 * line_step(10)
        assert!(viewport.wheel(2, 0));
        assert_eq!(viewport.horizontal().unwrap().offset(), 20);
        // Scrolling back to the start, then again, no longer changes anything.
        assert!(viewport.wheel(0, -100));
        assert_eq!(viewport.vertical().unwrap().offset(), 0);
        assert!(!viewport.wheel(0, -1));
    }

    #[test]
    fn page_line_and_bounds_drive_the_model() {
        let mut viewport = both_bars();
        assert!(viewport.page(ScrollOrientation::Vertical, true));
        assert_eq!(viewport.vertical().unwrap().offset(), 100);
        assert!(viewport.line(ScrollOrientation::Vertical, true));
        assert_eq!(viewport.vertical().unwrap().offset(), 110);
        assert!(viewport.to_end(ScrollOrientation::Vertical));
        assert_eq!(viewport.vertical().unwrap().offset(), 700); // 1000 - 300
        assert!(viewport.to_start(ScrollOrientation::Vertical));
        assert_eq!(viewport.vertical().unwrap().offset(), 0);
    }

    #[test]
    fn absent_axis_yields_no_geometry_and_no_movement() {
        let mut only_v = RootViewport::new(ScrollPolicy::ReservedGutter, BREADTH, MIN_THUMB)
            .with_vertical(vertical_model());
        assert!(only_v
            .track_and_geometry(BOUNDS, ScrollOrientation::Horizontal)
            .is_none());
        assert!(!only_v.wheel(5, 0)); // no horizontal bar, no change
        assert!(!only_v.page(ScrollOrientation::Horizontal, true));
    }

    #[test]
    fn resize_reclamps_a_dragged_offset_when_content_shrinks() {
        let mut viewport = both_bars();
        viewport.to_end(ScrollOrientation::Vertical);
        assert_eq!(viewport.vertical().unwrap().offset(), 700);
        // Content shrinks so the old offset no longer fits: it re-clamps to the
        // new maximum rather than dangling past the end.
        assert!(viewport.resize(ScrollOrientation::Vertical, 400, 300));
        assert_eq!(viewport.vertical().unwrap().offset(), 100); // 400 - 300
    }

    #[test]
    fn zero_breadth_reserves_nothing_and_shows_no_furniture() {
        let viewport = RootViewport::new(ScrollPolicy::ReservedGutter, 0, MIN_THUMB)
            .with_vertical(vertical_model());
        let layout = viewport.layout(BOUNDS);
        assert_eq!(layout.client.width, BOUNDS.width);
        assert!(layout.vertical_track.is_none());
        assert_eq!(
            viewport.hit_test(BOUNDS, Point::new(BOUNDS.right() - 1, BOUNDS.top() + 1)),
            FurnitureHit::Client
        );
    }
}
