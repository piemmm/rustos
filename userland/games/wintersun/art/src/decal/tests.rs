use super::{distance_to_segment, fray_offset, Bounds, Decal, Fray};
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::biome::Material;

use crate::weight::{WeightField, TOTAL};

const SEED: u64 = 0x524F_4144_5741_5953;

fn at(x: i32, y: i32) -> WorldPoint {
    WorldPoint { x, y }
}

/// An east-west road through the origin.
const ROAD: [WorldPoint; 2] = [WorldPoint { x: -4000, y: 0 }, WorldPoint { x: 4000, y: 0 }];

fn road() -> Decal<'static> {
    Decal {
        material: Material::Gravel,
        path: &ROAD,
        half_width: 120,
        feather: 90,
        coverage: 220,
    }
}

#[test]
fn distance_to_a_segment_is_exact_where_it_can_be() {
    let a = at(0, 0);
    let b = at(300, 400);
    assert_eq!(distance_to_segment(a, b, a), 0);
    assert_eq!(distance_to_segment(a, b, b), 0);
    // A 3-4-5 triangle: the far end is 500 away from the near end.
    assert_eq!(distance_to_segment(a, a, b), 500);
    // Perpendicular from the midpoint of an axis-aligned segment.
    assert_eq!(distance_to_segment(at(0, 0), at(100, 0), at(50, 70)), 70);
}

#[test]
fn distance_clamps_to_the_segment_rather_than_the_line() {
    // Beyond an end, the nearest point is the end, not the infinite line.
    assert_eq!(distance_to_segment(at(0, 0), at(100, 0), at(200, 0)), 100);
    assert_eq!(distance_to_segment(at(0, 0), at(100, 0), at(-50, 0)), 50);
}

#[test]
fn distance_survives_a_realm_spanning_segment() {
    // The projection is a ratio of dot products; at these magnitudes a
    // 64-bit intermediate would have overflowed.
    let a = at(i32::MIN / 2, i32::MIN / 2);
    let b = at(i32::MAX / 2, i32::MAX / 2);
    let d = distance_to_segment(a, b, at(0, 0));
    assert!(d < 4, "the origin is on that diagonal, got {d}");
}

#[test]
fn a_path_of_fewer_than_two_points_stamps_nothing() {
    let fray = Fray::new(SEED);
    let lone = [at(0, 0)];
    for path in [&[][..], &lone[..]] {
        let decal = Decal { path, ..road() };
        assert!(decal.bounds().is_none());
        assert_eq!(decal.coverage_at(&fray, at(0, 0)), 0);
        let mut field = WeightField::solid(Material::Moor);
        assert!(!decal.stamp(&mut field, &fray, at(0, 0)));
    }
}

#[test]
fn zero_coverage_stamps_nothing() {
    let fray = Fray::new(SEED);
    let decal = Decal {
        coverage: 0,
        ..road()
    };
    assert_eq!(decal.coverage_at(&fray, at(0, 0)), 0);
}

#[test]
fn the_centreline_is_fully_covered() {
    let fray = Fray::new(SEED);
    let decal = road();
    for x in (-3000..3000).step_by(311) {
        assert_eq!(decal.coverage_at(&fray, at(x, 0)), decal.coverage);
    }
}

#[test]
fn coverage_falls_to_nothing_outside_the_reach() {
    let fray = Fray::new(SEED);
    let decal = road();
    let beyond = i32::try_from(decal.reach() + fray_offset(decal.feather)).expect("small");
    for x in (-3000..3000).step_by(211) {
        assert_eq!(decal.coverage_at(&fray, at(x, beyond)), 0);
        assert_eq!(decal.coverage_at(&fray, at(x, -beyond)), 0);
    }
}

#[test]
fn coverage_decreases_away_from_the_centreline_on_average() {
    // The fray perturbs the edge, so the ramp is not monotone point by
    // point — but the band as a whole must still fall away.
    let fray = Fray::new(SEED);
    let decal = road();
    let mean = |y: i32| -> u64 {
        let mut total = 0u64;
        let mut count = 0u64;
        for x in (-3000..3000).step_by(53) {
            total += u64::from(decal.coverage_at(&fray, at(x, y)));
            count += 1;
        }
        total / count
    };
    let inner = mean(130);
    let middle = mean(170);
    let outer = mean(205);
    assert!(inner > middle, "{inner} then {middle}");
    assert!(middle > outer, "{middle} then {outer}");
}

#[test]
fn the_fray_actually_breaks_the_edge() {
    // A ramp that ran true would give one coverage for a whole line at a
    // fixed distance from the centreline.
    let fray = Fray::new(SEED);
    let decal = road();
    let first = decal.coverage_at(&fray, at(-3000, 170));
    let varies = (-3000..3000)
        .step_by(37)
        .any(|x| decal.coverage_at(&fray, at(x, 170)) != first);
    assert!(varies, "the edge runs true");
}

#[test]
fn bounds_contain_every_point_the_stamp_reaches() {
    let fray = Fray::new(SEED);
    let decal = road();
    let bounds = decal.bounds().expect("two points");
    for y in -400..400 {
        for x in (-5000..5000).step_by(97) {
            if decal.coverage_at(&fray, at(x, y)) > 0 {
                assert!(
                    bounds.contains(at(x, y)),
                    "({x}, {y}) is stamped but outside the reported bounds",
                );
            }
        }
    }
}

#[test]
fn bounds_overlap_is_symmetric_and_exclusive() {
    let a = Bounds {
        min_x: 0,
        min_y: 0,
        max_x: 10,
        max_y: 10,
    };
    let b = Bounds {
        min_x: 10,
        min_y: 10,
        max_x: 20,
        max_y: 20,
    };
    let far = Bounds {
        min_x: 11,
        min_y: 11,
        max_x: 20,
        max_y: 20,
    };
    assert!(a.overlaps(&b) && b.overlaps(&a));
    assert!(!a.overlaps(&far) && !far.overlaps(&a));
}

#[test]
fn a_stamp_raises_the_material_in_the_field() {
    let fray = Fray::new(SEED);
    let decal = road();
    let mut field = WeightField::solid(Material::Moor);
    assert!(decal.stamp(&mut field, &fray, at(0, 0)));
    assert!(field.weight_of(Material::Gravel) >= decal.coverage);
    assert_eq!(field.total(), TOTAL);
}

#[test]
fn two_roads_crossing_merge_rather_than_double() {
    let fray = Fray::new(SEED);
    let north_south = [at(0, -4000), at(0, 4000)];
    let across = Decal {
        path: &north_south,
        ..road()
    };

    let mut both = WeightField::solid(Material::ColdSteppe);
    road().stamp(&mut both, &fray, at(0, 0));
    across.stamp(&mut both, &fray, at(0, 0));

    let mut one = WeightField::solid(Material::ColdSteppe);
    road().stamp(&mut one, &fray, at(0, 0));

    assert_eq!(both, one, "a junction is more road than a road");
}

#[test]
fn stamping_is_order_independent() {
    let fray = Fray::new(SEED);
    let river_path = [at(-2000, -800), at(500, 200), at(3000, 900)];
    let river = Decal {
        material: Material::Water,
        path: &river_path,
        half_width: 200,
        feather: 150,
        coverage: 250,
    };

    for point in [at(0, 0), at(400, 150), at(-1500, -600), at(2000, 600)] {
        let mut forward = WeightField::solid(Material::Saltmarsh);
        road().stamp(&mut forward, &fray, point);
        river.stamp(&mut forward, &fray, point);

        let mut backward = WeightField::solid(Material::Saltmarsh);
        river.stamp(&mut backward, &fray, point);
        road().stamp(&mut backward, &fray, point);

        assert_eq!(forward.total(), TOTAL);
        assert_eq!(backward.total(), TOTAL);
        assert_eq!(
            forward.weight_of(Material::Water) > 0,
            backward.weight_of(Material::Water) > 0,
            "the river appears only one way round at {point:?}",
        );
    }
}

#[test]
fn a_stamp_far_from_the_path_changes_nothing() {
    let fray = Fray::new(SEED);
    let decal = road();
    let mut field = WeightField::solid(Material::Tundra);
    let before = field;
    assert!(!decal.stamp(&mut field, &fray, at(0, 100_000)));
    assert_eq!(field, before);
}

#[test]
fn a_decal_with_no_feather_still_stamps() {
    // A hard-edged decal is legal — it is just not what a road wants.
    let fray = Fray::new(SEED);
    let decal = Decal {
        feather: 0,
        ..road()
    };
    assert_eq!(fray_offset(0), 0);
    assert_eq!(decal.coverage_at(&fray, at(0, 0)), decal.coverage);
    assert_eq!(decal.coverage_at(&fray, at(0, 500)), 0);
}

#[test]
fn a_degenerate_segment_is_a_point_not_a_division_by_zero() {
    let fray = Fray::new(SEED);
    let same = [at(100, 100), at(100, 100)];
    let decal = Decal {
        path: &same,
        ..road()
    };
    assert_eq!(decal.coverage_at(&fray, at(100, 100)), decal.coverage);
    assert_eq!(decal.coverage_at(&fray, at(100, 100_000)), 0);
}

#[test]
fn two_realms_fray_differently() {
    let a = Fray::new(SEED);
    let b = Fray::new(SEED ^ 0xABCD);
    let decal = road();
    let differs = (-3000..3000)
        .step_by(29)
        .any(|x| decal.coverage_at(&a, at(x, 170)) != decal.coverage_at(&b, at(x, 170)));
    assert!(differs);
}
