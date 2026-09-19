use super::{
    lerp, Ramp, ASHLAND, BOREAL_FOREST, COLD_STEPPE, DUST, EMBER, FELL_HEATH, GLACIER, GRAVEL,
    LEAF, MOOR, RAIN, RIFT_WASTE, ROCK, SALTMARSH, SAND, SMOKE, SNOWFALL, SNOWFIELD, SPLASH,
    TEMPERATE_FOREST, TUNDRA, WATER,
};
use tairix_raster::color::Color;

/// Every ramp the palette publishes, so a sweep covers the set rather than
/// a sample of it.
const ALL: &[Ramp] = &[
    WATER,
    GLACIER,
    SNOWFIELD,
    TUNDRA,
    FELL_HEATH,
    COLD_STEPPE,
    BOREAL_FOREST,
    TEMPERATE_FOREST,
    MOOR,
    SALTMARSH,
    ASHLAND,
    RIFT_WASTE,
    ROCK,
    GRAVEL,
    SAND,
    RAIN,
    SNOWFALL,
    EMBER,
    SMOKE,
    DUST,
    SPLASH,
    LEAF,
];

/// Perceived brightness, weighted as the eye weighs the channels.
fn luma(c: Color) -> u32 {
    u32::from(c.r) * 2 + u32::from(c.g) * 5 + u32::from(c.b)
}

#[test]
fn every_ramp_is_ordered_dark_to_light() {
    for ramp in ALL {
        assert!(
            luma(ramp.shadow) < luma(ramp.mid),
            "shadow is not darker than mid in {ramp:?}"
        );
        assert!(
            luma(ramp.mid) < luma(ramp.light),
            "mid is not darker than light in {ramp:?}"
        );
    }
}

#[test]
fn every_ramp_is_opaque() {
    for ramp in ALL {
        assert_eq!((ramp.shadow.a, ramp.mid.a, ramp.light.a), (255, 255, 255));
    }
}

#[test]
fn sample_hits_all_three_stops_exactly() {
    for ramp in ALL {
        assert_eq!(ramp.sample(0), ramp.shadow);
        assert_eq!(ramp.sample(128), ramp.mid);
        assert_eq!(ramp.sample(255), ramp.light);
    }
}

#[test]
fn sample_is_monotone_across_the_whole_axis() {
    for ramp in ALL {
        let mut previous = luma(ramp.sample(0));
        for t in 1..=u8::MAX {
            let next = luma(ramp.sample(t));
            assert!(next >= previous, "{ramp:?} dips at t={t}");
            previous = next;
        }
    }
}

#[test]
fn lerp_hits_both_ends_and_rounds_the_middle() {
    let a = Color::rgb(0, 10, 200);
    let b = Color::rgb(255, 20, 100);
    assert_eq!(lerp(a, b, 0), a);
    assert_eq!(lerp(a, b, 255), b);
    // 128/255 of the way, rounded to nearest: 128, 15, 150.
    assert_eq!(lerp(a, b, 128), Color::rgb(128, 15, 150));
}

#[test]
fn lerp_carries_alpha() {
    let a = Color::rgba(0, 0, 0, 0);
    let b = Color::rgba(0, 0, 0, 255);
    assert_eq!(lerp(a, b, 255).a, 255);
    assert_eq!(lerp(a, b, 0).a, 0);
}

#[test]
fn the_ground_set_is_cold_biased() {
    // A realm called WinterSun has no warm ground but the two the art
    // direction names: the ash of a volcano and the light end of sand.
    let warm = ALL
        .iter()
        .take(15)
        .filter(|r| u32::from(r.mid.r) > u32::from(r.mid.b) + 40)
        .count();
    assert!(warm <= 4, "{warm} ground ramps run warm");
}
