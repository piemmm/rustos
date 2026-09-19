//! The projection is a bijection on the pixels it covers, and the view
//! never leaves the realm.

use super::*;
use tairix_wintersun_world::params::{RealmParams, RealmSpec};

fn params(extent_chunks: u32) -> RealmParams {
    let spec = RealmSpec {
        extent_chunks,
        ..RealmParams::winter_default(0x5749_4E54_4552).spec()
    };
    RealmParams::new(spec).expect("the spec is within its own stated range")
}

#[test]
fn zoom_stops_are_doublings_within_the_stated_range() {
    assert_eq!(Zoom::NEAREST.sub_units_per_pixel(), 8);
    assert_eq!(Zoom::FURTHEST.sub_units_per_pixel(), 128);
    assert_eq!(Zoom::DEFAULT.pixels_per_cell(), 32);
    assert_eq!(Zoom::new(0), Zoom::NEAREST, "below the range clamps in");
    assert_eq!(Zoom::new(99), Zoom::FURTHEST, "above the range clamps in");
    assert_eq!(Zoom::NEAREST.nearer(), None);
    assert_eq!(Zoom::FURTHEST.further(), None);

    let mut stops = alloc::vec![Zoom::NEAREST];
    while let Some(next) = stops.last().and_then(|z| z.further()) {
        stops.push(next);
    }
    assert_eq!(stops.len(), 5, "five stops, each a doubling");
    for pair in stops.windows(2) {
        assert_eq!(
            pair[1].sub_units_per_pixel(),
            pair[0].sub_units_per_pixel() * 2
        );
    }
}

#[test]
fn a_pixel_round_trips_through_the_projection() {
    let camera = Camera::new(
        WorldPoint {
            x: 12_345,
            y: -6_789,
        },
        Zoom::DEFAULT,
        realm_bounds(params(64)),
    );
    let (w, h) = (640u32, 360u32);
    let (wi, hi) = (
        i32::try_from(w).expect("small"),
        i32::try_from(h).expect("small"),
    );
    for py in [0, 1, hi / 2, hi - 1] {
        for px in [0, 1, wi / 2, wi - 1] {
            let world = camera.world_at(w, h, px, py);
            assert_eq!(
                camera.screen_at(w, h, world),
                (px, py),
                "pixel ({px},{py}) did not survive the round trip"
            );
        }
    }
}

#[test]
fn a_world_point_between_samples_lands_in_the_pixel_that_covers_it() {
    let camera = Camera::new(
        WorldPoint::default(),
        Zoom::DEFAULT,
        realm_bounds(params(64)),
    );
    let (w, h) = (64, 64);
    let span = Zoom::DEFAULT.sub_units_per_pixel();
    let base = camera.world_at(w, h, 10, 10);
    for offset in 0..span {
        let inside = WorldPoint {
            x: base.x + offset,
            y: base.y + offset,
        };
        assert_eq!(
            camera.screen_at(w, h, inside),
            (10, 10),
            "an offset of {offset} inside one pixel left it"
        );
    }
    let next = WorldPoint {
        x: base.x + span,
        y: base.y + span,
    };
    assert_eq!(camera.screen_at(w, h, next), (11, 11));
}

#[test]
fn a_point_west_or_north_of_the_origin_floors_rather_than_truncating() {
    // Truncation toward zero would map the pixel left of the origin and
    // the origin pixel itself to the same column.
    let camera = Camera::new(
        WorldPoint { x: 0, y: 0 },
        Zoom::DEFAULT,
        realm_bounds(params(64)),
    );
    let (w, h) = (2, 2);
    let origin = camera.origin(w, h);
    let just_west = WorldPoint {
        x: origin.x - 1,
        y: origin.y - 1,
    };
    assert_eq!(camera.screen_at(w, h, just_west), (-1, -1));
}

#[test]
fn the_visible_extent_is_exactly_the_pixels_drawn() {
    let camera = Camera::new(
        WorldPoint { x: 4_096, y: 4_096 },
        Zoom::DEFAULT,
        realm_bounds(params(64)),
    );
    let (w, h) = (100, 50);
    let visible = camera.visible(w, h);
    assert_eq!(camera.world_at(w, h, 0, 0).x, visible.min_x);
    assert_eq!(camera.world_at(w, h, 0, 0).y, visible.min_y);
    let last = camera.world_at(
        w,
        h,
        i32::try_from(w).expect("small") - 1,
        i32::try_from(h).expect("small") - 1,
    );
    assert_eq!((last.x, last.y), (visible.max_x, visible.max_y));
    assert!(visible.contains(camera.centre(w, h)));
}

#[test]
fn aiming_never_shows_ground_outside_the_realm() {
    let bounds = realm_bounds(params(8));
    let mut camera = Camera::new(WorldPoint::default(), Zoom::DEFAULT, bounds);
    let (w, h) = (320, 200);
    for target in [
        WorldPoint {
            x: i32::MIN,
            y: i32::MIN,
        },
        WorldPoint {
            x: i32::MAX,
            y: i32::MAX,
        },
        WorldPoint {
            x: bounds.min_x,
            y: bounds.max_y,
        },
        WorldPoint { x: 0, y: 0 },
    ] {
        camera.look_at(target);
        let visible = camera.visible(w, h);
        assert!(
            visible.min_x >= bounds.min_x && visible.max_x <= bounds.max_x,
            "aiming at {target:?} put {visible:?} outside {bounds:?} horizontally"
        );
        assert!(
            visible.min_y >= bounds.min_y && visible.max_y <= bounds.max_y,
            "aiming at {target:?} put {visible:?} outside {bounds:?} vertically"
        );
    }
}

#[test]
fn aiming_tracks_the_target_where_the_realm_has_room() {
    let bounds = realm_bounds(params(64));
    let mut camera = Camera::new(WorldPoint::default(), Zoom::DEFAULT, bounds);
    let target = WorldPoint {
        x: 1_000_000,
        y: -500_000,
    };
    camera.look_at(target);
    assert_eq!(
        camera.centre(320, 200),
        target,
        "an interior target is centred exactly"
    );
}

#[test]
fn a_window_that_grows_after_the_camera_settled_still_shows_only_the_realm() {
    // The clamp is applied where the view is projected, so a camera
    // settled against an edge in a small window does not project past
    // that edge when the window grows. Clamping only on being aimed left
    // exactly this hole, and the property model found it.
    let bounds = realm_bounds(params(8));
    let mut camera = Camera::new(WorldPoint::default(), Zoom::DEFAULT, bounds);
    camera.look_at(WorldPoint {
        x: bounds.max_x,
        y: bounds.max_y,
    });
    let _ = camera.visible(1, 1);
    for (w, h) in [(2u32, 2u32), (320, 200), (4096, 4096)] {
        let visible = camera.visible(w, h);
        assert!(
            visible.min_x >= bounds.min_x
                && visible.max_x <= bounds.max_x
                && visible.min_y >= bounds.min_y
                && visible.max_y <= bounds.max_y,
            "a {w}x{h} window showed {visible:?} outside {bounds:?}"
        );
    }
}

#[test]
fn a_realm_narrower_than_the_view_is_centred_rather_than_pinned() {
    // Four chunks at the furthest stop is 32 768 sub-units across; a
    // 4096-pixel viewport covers sixteen times that.
    let params = params(4);
    let bounds = realm_bounds(params);
    let mut camera = Camera::new(WorldPoint::default(), Zoom::FURTHEST, bounds);
    camera.look_at(WorldPoint {
        x: bounds.max_x,
        y: bounds.max_y,
    });
    assert_eq!(
        camera.centre(4096, 4096),
        WorldPoint {
            x: i32::midpoint(bounds.min_x, bounds.max_x),
            y: i32::midpoint(bounds.min_y, bounds.max_y),
        },
        "a realm smaller than the view is centred in it"
    );
}

#[test]
fn the_projection_does_not_overflow_at_the_coordinate_extremes() {
    // Overflow checks are on in both profiles, so an unguarded multiply
    // here would panic rather than merely answer wrongly.
    let camera = Camera::new(
        WorldPoint {
            x: i32::MAX,
            y: i32::MIN,
        },
        Zoom::FURTHEST,
        realm_bounds(params(64)),
    );
    let (w, h) = (4096, 4096);
    let _ = camera.visible(w, h);
    let _ = camera.world_at(w, h, i32::MAX, i32::MIN);
    let _ = camera.screen_at(
        w,
        h,
        WorldPoint {
            x: i32::MIN,
            y: i32::MAX,
        },
    );
}

#[test]
fn realm_bounds_cover_every_chunk_the_realm_holds() {
    let params = params(16);
    let bounds = realm_bounds(params);
    let cell = i64::from(tairix_wintersun_world::geom::CELL_SUB_UNITS);
    let cells = i64::from(tairix_wintersun_world::geom::CHUNK_CELLS);
    assert_eq!(
        i64::from(bounds.min_x),
        i64::from(params.min_chunk()) * cells * cell
    );
    assert_eq!(
        i64::from(bounds.max_x),
        (i64::from(params.max_chunk()) + 1) * cells * cell
    );
    assert_eq!(bounds.min_x, bounds.min_y, "the realm is square");
}
