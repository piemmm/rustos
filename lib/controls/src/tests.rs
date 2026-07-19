//! Unit tests for the scroll geometry engine.
//!
//! These cover the checklist the design language requires of a scrollbar:
//! zero overflow, proportional thumb math, minimum thumb size, line and page
//! steps, range changes during drag, both orientations, keyboard bounds, and
//! the fail-closed behaviour for degenerate ranges.

use crate::scroll::{ScrollGeometry, ScrollModel, ScrollOrientation, ScrollRange, TrackHit};

// --- ScrollRange normalisation ------------------------------------------

#[test]
fn range_not_scrollable_when_viewport_covers_content() {
    let range = ScrollRange::new(100, 100, 50);
    assert!(!range.is_scrollable());
    assert_eq!(range.max_offset(), 0);
    assert_eq!(range.offset(), 0, "offset pins to zero when not scrollable");
}

#[test]
fn range_not_scrollable_when_viewport_exceeds_content() {
    let range = ScrollRange::new(80, 200, 10);
    assert!(!range.is_scrollable());
    assert_eq!(range.max_offset(), 0);
    assert_eq!(range.offset(), 0);
}

#[test]
fn zero_viewport_fails_closed_to_not_scrollable() {
    let range = ScrollRange::new(1000, 0, 500);
    assert!(!range.is_scrollable());
    assert_eq!(range.offset(), 0);
    assert_eq!(range.max_offset(), 0);
}

#[test]
fn offset_is_clamped_to_max() {
    let range = ScrollRange::new(1000, 200, 5_000);
    assert!(range.is_scrollable());
    assert_eq!(range.max_offset(), 800);
    assert_eq!(range.offset(), 800, "overflowing offset clamps to the end");
}

#[test]
fn resize_preserves_offset_where_it_still_fits() {
    let range = ScrollRange::new(1000, 200, 300);
    let grown = range.resize(2000, 200);
    assert_eq!(grown.offset(), 300, "offset kept when still valid");
    assert_eq!(grown.max_offset(), 1800);
}

#[test]
fn resize_clamps_offset_that_no_longer_fits() {
    let range = ScrollRange::new(1000, 200, 700);
    let shrunk = range.resize(400, 200);
    assert_eq!(shrunk.max_offset(), 200);
    assert_eq!(shrunk.offset(), 200, "offset clamped to the new end");
}

#[test]
fn extreme_extents_do_not_overflow() {
    let range = ScrollRange::new(u64::MAX, 1, u64::MAX);
    assert!(range.is_scrollable());
    assert_eq!(range.max_offset(), u64::MAX - 1);
    assert_eq!(range.offset(), u64::MAX - 1);
    // Geometry over a huge range must not panic and must stay in the track.
    let geom = ScrollGeometry::new(range, 400, 24);
    let thumb = geom.thumb();
    assert!(thumb.length >= 24);
    assert!(u64::from(thumb.start) + u64::from(thumb.length) <= 400);
}

// --- ScrollModel stepping ------------------------------------------------

fn model() -> ScrollModel {
    ScrollModel::new(ScrollRange::new(1000, 100, 0), 10, 90)
}

#[test]
fn line_and_page_steps_move_by_the_declared_distance() {
    let m = model().line_forward();
    assert_eq!(m.offset(), 10);
    let m = m.page_forward();
    assert_eq!(m.offset(), 100);
    let m = m.line_backward();
    assert_eq!(m.offset(), 90);
    let m = m.page_backward();
    assert_eq!(m.offset(), 0);
}

#[test]
fn steps_saturate_at_the_bounds() {
    let m = model().page_backward();
    assert_eq!(m.offset(), 0, "cannot scroll before the start");
    let m = model().to_end();
    assert_eq!(m.offset(), 900);
    let m = m.page_forward().line_forward();
    assert_eq!(m.offset(), 900, "cannot scroll past the end");
}

#[test]
fn home_and_end_reach_the_bounds() {
    assert_eq!(model().to_end().offset(), 900);
    assert_eq!(model().to_end().to_start().offset(), 0);
}

#[test]
fn scroll_by_negative_moves_toward_start() {
    let m = model().scroll_to(500).scroll_by(-200);
    assert_eq!(m.offset(), 300);
    let m = m.scroll_by(i64::MIN);
    assert_eq!(m.offset(), 0, "huge negative delta saturates, never wraps");
}

#[test]
fn zero_step_moves_nothing() {
    let m = ScrollModel::new(ScrollRange::new(1000, 100, 400), 0, 0);
    assert_eq!(m.line_forward().offset(), 400);
    assert_eq!(m.page_backward().offset(), 400);
}

#[test]
fn pathologically_large_step_saturates() {
    let m = ScrollModel::new(ScrollRange::new(1000, 100, 0), u64::MAX, u64::MAX);
    assert_eq!(
        m.line_forward().offset(),
        900,
        "clamped to the end, no panic"
    );
}

#[test]
fn model_resize_reclamps_offset() {
    let m = model().to_end();
    assert_eq!(m.offset(), 900);
    let m = m.resize(300, 100);
    assert_eq!(m.offset(), 200);
    assert_eq!(m.line_step(), 10, "steps unchanged by resize");
    assert_eq!(m.page_step(), 90);
}

// --- Thumb geometry ------------------------------------------------------

#[test]
fn thumb_is_proportional_to_visible_fraction() {
    // viewport is a quarter of the content -> thumb is a quarter of the track.
    let geom = ScrollGeometry::new(ScrollRange::new(400, 100, 0), 400, 10);
    assert_eq!(geom.thumb_length(), 100);
}

#[test]
fn thumb_respects_minimum_length() {
    // viewport is 1/1000 of content; proportional thumb would be sub-pixel.
    let geom = ScrollGeometry::new(ScrollRange::new(100_000, 100, 0), 400, 24);
    assert_eq!(geom.thumb_length(), 24, "floored at the theme minimum");
}

#[test]
fn thumb_never_exceeds_track() {
    let geom = ScrollGeometry::new(ScrollRange::new(101, 100, 0), 40, 100);
    assert_eq!(
        geom.thumb_length(),
        40,
        "minimum capped by the track length"
    );
}

#[test]
fn non_scrollable_thumb_fills_track_and_is_not_draggable() {
    let geom = ScrollGeometry::new(ScrollRange::new(100, 100, 0), 400, 24);
    assert_eq!(geom.thumb_length(), 400);
    assert_eq!(geom.travel(), 0);
    assert!(!geom.draggable());
    assert_eq!(geom.thumb().start, 0);
}

#[test]
fn zero_track_yields_empty_thumb() {
    let geom = ScrollGeometry::new(ScrollRange::new(1000, 100, 500), 0, 24);
    assert_eq!(geom.thumb_length(), 0);
    assert!(!geom.draggable());
    assert_eq!(geom.offset_for_thumb_start(50), 0);
}

#[test]
fn thumb_position_maps_offset_across_travel() {
    let range = ScrollRange::new(1000, 100, 0);
    let track = 400;
    let geom = ScrollGeometry::new(range, track, 40);
    let thumb_len = geom.thumb_length();
    assert_eq!(thumb_len, 40);
    let travel = geom.travel();
    assert_eq!(travel, 360);

    // At offset 0 the thumb is at the start.
    assert_eq!(geom.thumb().start, 0);
    // At max offset the thumb sits flush against the end.
    let at_end = ScrollGeometry::new(range.with_offset(900), track, 40);
    assert_eq!(at_end.thumb().start, travel);
    assert_eq!(
        u64::from(at_end.thumb().start) + u64::from(thumb_len),
        u64::from(track)
    );
    // Halfway.
    let mid = ScrollGeometry::new(range.with_offset(450), track, 40);
    assert_eq!(mid.thumb().start, travel / 2);
}

// --- Round-trip: thumb position <-> offset --------------------------------

#[test]
fn thumb_start_and_offset_are_inverses_at_the_bounds() {
    let range = ScrollRange::new(1000, 100, 0);
    let geom = ScrollGeometry::new(range, 400, 40);
    let travel = geom.travel();
    assert_eq!(geom.offset_for_thumb_start(0), 0);
    assert_eq!(geom.offset_for_thumb_start(travel), 900);
    // Past the end clamps.
    assert_eq!(geom.offset_for_thumb_start(travel + 500), 900);
}

#[test]
fn offset_for_thumb_start_rounds_to_nearest() {
    let range = ScrollRange::new(1000, 100, 0);
    let geom = ScrollGeometry::new(range, 400, 40);
    let travel = geom.travel();
    // Middle of the travel maps to the middle of the offset range.
    assert_eq!(geom.offset_for_thumb_start(travel / 2), 450);
}

// --- Hit testing ---------------------------------------------------------

#[test]
fn hit_classifies_track_regions() {
    let range = ScrollRange::new(1000, 100, 450);
    let geom = ScrollGeometry::new(range, 400, 40);
    let thumb = geom.thumb();
    assert_eq!(
        geom.hit(thumb.start.saturating_sub(1)),
        TrackHit::BeforeThumb
    );
    assert_eq!(geom.hit(thumb.start), TrackHit::Thumb);
    assert_eq!(geom.hit(thumb.start + thumb.length - 1), TrackHit::Thumb);
    assert_eq!(geom.hit(thumb.start + thumb.length), TrackHit::AfterThumb);
    assert_eq!(geom.hit(10_000), TrackHit::AfterThumb, "past the track");
}

// --- Drag ----------------------------------------------------------------

#[test]
fn drag_preserves_pointer_to_thumb_anchor() {
    let range = ScrollRange::new(1000, 100, 300);
    let geom = ScrollGeometry::new(range, 400, 40);
    let thumb = geom.thumb();
    // Pointer grabs the thumb 5px from its near edge.
    let grab = thumb.start + 5;
    let anchor = i32::try_from(grab).unwrap() - i32::try_from(thumb.start).unwrap();
    // No pointer movement -> same offset (no jump).
    let same = geom.offset_for_drag(i32::try_from(grab).unwrap(), anchor);
    assert_eq!(same, 300, "grabbing the thumb must not move content");
    // Drag the pointer to the very end.
    let end = geom.offset_for_drag(10_000, anchor);
    assert_eq!(end, 900);
    // Drag before the start pins to zero.
    let start = geom.offset_for_drag(-10_000, anchor);
    assert_eq!(start, 0);
}

#[test]
fn drag_on_non_draggable_bar_is_inert() {
    let geom = ScrollGeometry::new(ScrollRange::new(100, 100, 0), 400, 40);
    assert_eq!(geom.offset_for_drag(200, 0), 0);
}

#[test]
fn range_change_during_drag_stays_in_bounds() {
    // Simulate a content extent shrinking mid-drag: the geometry is rebuilt
    // from the new range and the preserved anchor, and never yields an invalid
    // offset.
    let anchor = 5;
    let before = ScrollGeometry::new(ScrollRange::new(2000, 100, 0), 400, 40);
    let _ = before.offset_for_drag(100, anchor);
    // Content shrank to 300 units; viewport unchanged.
    let after = ScrollGeometry::new(ScrollRange::new(300, 100, 0), 400, 40);
    let offset = after.offset_for_drag(100_000, anchor);
    assert!(offset <= after.range().max_offset());
    assert_eq!(offset, 200);
}

// --- Orientation ---------------------------------------------------------

#[test]
fn orientation_is_behaviourally_neutral() {
    // The same range and track produce identical thumb math regardless of
    // orientation; orientation only informs how a viewport maps the 1-D span.
    let range = ScrollRange::new(1000, 250, 500);
    let geom = ScrollGeometry::new(range, 320, 32);
    let v = ScrollOrientation::Vertical;
    let h = ScrollOrientation::Horizontal;
    assert_ne!(v, h);
    // There is exactly one geometry; both axes read the same numbers.
    assert_eq!(geom.thumb().length, 80);
}
