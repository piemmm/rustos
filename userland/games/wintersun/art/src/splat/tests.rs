use alloc::vec;

use tairix_raster::color::Pixel;
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::biome::{Material, BLEND_SLOTS};

use super::{
    cutoff_below, mean, splat, Geometry, SpanPlan, SpanTiles, Warp, BLEND_DEPTH, HEIGHT_SHIFT,
    MAX_BLEND_TOTAL, MAX_HEIGHT_SCORE,
};
use crate::material::{self, MaterialTile, Mip, Quality};
use crate::weight::{WeightField, TOTAL};

const SEED: u64 = 0x5748_4954_4553_554E;

/// An unwarped span, so a test of the blend is not also a test of the
/// warp.
fn flat_geometry() -> Geometry {
    Geometry {
        origin: WorldPoint { x: 0, y: 0 },
        step: 16,
        warp_near: (0, 0),
        warp_far: (0, 0),
    }
}

fn no_tiles<'a>() -> SpanTiles<'a> {
    [None; BLEND_SLOTS]
}

#[test]
fn a_plan_over_one_material_is_that_material_at_full_weight() {
    let field = WeightField::solid(Material::Rock);
    let plan = SpanPlan::new(&field, &field);
    assert_eq!(plan.len(), 1);
    assert!(!plan.is_empty());
    assert_eq!(plan.materials().next(), Some(Material::Rock));
}

#[test]
fn a_plan_unions_the_two_ends() {
    let left = WeightField::solid(Material::Sand);
    let right = WeightField::solid(Material::Water);
    let plan = SpanPlan::new(&left, &right);
    let materials: alloc::vec::Vec<_> = plan.materials().collect();
    assert_eq!(materials.len(), 2);
    assert!(materials.contains(&Material::Sand));
    assert!(materials.contains(&Material::Water));
}

#[test]
fn a_plan_never_exceeds_the_slot_count() {
    let mut left = WeightField::solid(Material::Rock);
    left.cover(Material::Gravel, 180);
    left.cover(Material::Sand, 100);
    left.cover(Material::Moor, 50);
    let mut right = WeightField::solid(Material::Glacier);
    right.cover(Material::Snowfield, 180);
    right.cover(Material::Tundra, 100);
    right.cover(Material::Water, 50);

    let plan = SpanPlan::new(&left, &right);
    assert_eq!(plan.len(), BLEND_SLOTS);
    assert_eq!(plan.materials().count(), BLEND_SLOTS);
}

#[test]
fn a_span_with_no_tiles_draws_the_flat_tones() {
    let field = WeightField::solid(Material::Moor);
    let plan = SpanPlan::new(&field, &field);
    let mut row = vec![Pixel::TRANSPARENT; 8];
    splat(&mut row, &plan, &no_tiles(), &flat_geometry());

    let flat = material::params(Material::Moor).flat();
    for pixel in &row {
        assert_eq!(pixel.a, u8::MAX, "ground is opaque");
        assert_eq!((pixel.r, pixel.g, pixel.b), (flat.r, flat.g, flat.b));
    }
}

#[test]
fn a_span_is_always_opaque() {
    let mut left = WeightField::solid(Material::BorealForest);
    left.cover(Material::Rock, 120);
    let right = WeightField::solid(Material::Snowfield);
    let plan = SpanPlan::new(&left, &right);

    let tile = MaterialTile::synthesise(Material::Rock, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let mut tiles = no_tiles();
    // Rock is not the heaviest end material, so find where the plan put it.
    let index = plan
        .materials()
        .position(|m| m == Material::Rock)
        .expect("rock is in the plan");
    tiles[index] = Some(&tile);

    let mut row = vec![Pixel::TRANSPARENT; 64];
    splat(&mut row, &plan, &tiles, &flat_geometry());
    assert!(row.iter().all(|p| p.a == u8::MAX));
}

#[test]
fn a_mismatched_tile_is_ignored_rather_than_drawn() {
    // A caller that pairs its lists up wrongly must get that material's
    // flat tone, never another material's pixels.
    let field = WeightField::solid(Material::Sand);
    let plan = SpanPlan::new(&field, &field);
    let wrong = MaterialTile::synthesise(Material::Rock, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let mut tiles = no_tiles();
    tiles[0] = Some(&wrong);

    let mut with = vec![Pixel::TRANSPARENT; 8];
    splat(&mut with, &plan, &tiles, &flat_geometry());
    let mut without = vec![Pixel::TRANSPARENT; 8];
    splat(&mut without, &plan, &no_tiles(), &flat_geometry());
    assert_eq!(with, without);
}

#[test]
fn an_empty_destination_draws_nothing_and_does_not_panic() {
    let field = WeightField::solid(Material::Water);
    let plan = SpanPlan::new(&field, &field);
    let mut row: alloc::vec::Vec<Pixel> = vec![];
    splat(&mut row, &plan, &no_tiles(), &flat_geometry());
    assert!(row.is_empty());
}

#[test]
fn a_one_pixel_span_draws_its_near_end() {
    let left = WeightField::solid(Material::Sand);
    let right = WeightField::solid(Material::Water);
    let plan = SpanPlan::new(&left, &right);
    let mut one = vec![Pixel::TRANSPARENT; 1];
    splat(&mut one, &plan, &no_tiles(), &flat_geometry());

    let mut many = vec![Pixel::TRANSPARENT; 16];
    splat(&mut many, &plan, &no_tiles(), &flat_geometry());
    assert_eq!(one[0], many[0]);
}

#[test]
fn a_span_walks_from_one_material_to_the_other() {
    // The property a terrain boundary is: the near end reads as the near
    // material and the far end as the far one, with no jump between.
    let left = WeightField::solid(Material::Snowfield);
    let right = WeightField::solid(Material::Ashland);
    let plan = SpanPlan::new(&left, &right);
    let mut row = vec![Pixel::TRANSPARENT; 96];
    splat(&mut row, &plan, &no_tiles(), &flat_geometry());

    let snow = material::params(Material::Snowfield).flat();
    let ash = material::params(Material::Ashland).flat();
    assert_eq!((row[0].r, row[0].g, row[0].b), (snow.r, snow.g, snow.b));
    let last = row[row.len() - 1];
    assert_eq!((last.r, last.g, last.b), (ash.r, ash.g, ash.b));

    // Snow stands far above ash, so the height offset makes the crossing
    // sharp — but never discontinuous.
    let mut biggest = 0i32;
    for pair in row.windows(2) {
        biggest = biggest.max((i32::from(pair[1].r) - i32::from(pair[0].r)).abs());
    }
    assert!(biggest < 120, "the boundary jumps by {biggest}");
}

#[test]
fn the_height_offset_makes_a_standing_material_win_early() {
    // Against a linear blend, the midpoint of a snow/moor span sits
    // toward snow, because snow stands far higher than peat.
    let left = WeightField::solid(Material::Snowfield);
    let right = WeightField::solid(Material::Moor);
    let plan = SpanPlan::new(&left, &right);
    let mut row = vec![Pixel::TRANSPARENT; 65];
    splat(&mut row, &plan, &no_tiles(), &flat_geometry());

    let snow = material::params(Material::Snowfield).flat();
    let moor = material::params(Material::Moor).flat();
    let middle = row[32];
    let to_snow = i32::from(middle.r) - i32::from(moor.r);
    let to_moor = i32::from(snow.r) - i32::from(middle.r);
    assert!(
        to_snow > to_moor,
        "the midpoint sits at snow {to_moor} / moor {to_snow}, which is a linear blend",
    );
}

#[test]
fn the_warp_displaces_and_stays_inside_its_amplitude() {
    let warp = Warp::new(SEED);
    let mut moved = 0;
    for step in 0..64 {
        let at = WorldPoint {
            x: step * 4096,
            y: step * 2048,
        };
        let (dx, dy) = warp.at(at);
        assert!(dx.abs() <= super::WARP_AMPLITUDE);
        assert!(dy.abs() <= super::WARP_AMPLITUDE);
        if dx != 0 || dy != 0 {
            moved += 1;
        }
    }
    assert!(moved > 32, "the warp barely displaces anything");
}

#[test]
fn the_warp_is_smooth() {
    // A warp that jumped would put a visible crease across the ground.
    let warp = Warp::new(SEED);
    let step = 256;
    let mut previous = warp.at(WorldPoint { x: 0, y: 0 });
    for i in 1..200 {
        let next = warp.at(WorldPoint { x: i * step, y: 0 });
        let jump = (next.0 - previous.0).abs().max((next.1 - previous.1).abs());
        assert!(jump < super::WARP_AMPLITUDE / 8, "warp jumps {jump} at {i}");
        previous = next;
    }
}

#[test]
fn two_realms_warp_differently() {
    let a = Warp::new(SEED);
    let b = Warp::new(SEED ^ 0xFFFF);
    let at = WorldPoint {
        x: 12_345,
        y: -6_789,
    };
    assert_ne!(a.at(at), b.at(at));
}

#[test]
fn geometry_takes_the_warp_at_both_ends() {
    let warp = Warp::new(SEED);
    let origin = WorldPoint {
        x: 100_000,
        y: -50_000,
    };
    let geometry = Geometry::new(&warp, origin, 64, 128);
    assert_eq!(geometry.origin, origin);
    assert_eq!(geometry.step, 64);
    assert_eq!(geometry.warp_near, warp.at(origin));
    assert_eq!(
        geometry.warp_far,
        warp.at(WorldPoint {
            x: origin.x + 64 * 127,
            y: origin.y
        }),
    );
}

#[test]
fn geometry_survives_an_absurd_span() {
    // A caller asking for a span that would overflow the world gets a
    // clamped one rather than a panic under overflow checks.
    let warp = Warp::new(SEED);
    let geometry = Geometry::new(
        &warp,
        WorldPoint {
            x: i32::MAX - 1,
            y: 0,
        },
        i32::MAX,
        u32::MAX,
    );
    assert_eq!(geometry.origin.x, i32::MAX - 1);
}

#[test]
fn the_warp_breaks_a_tile_period() {
    // Without the warp, a span stepping exactly one tile period would
    // read identical texels. With it, it must not.
    let field = WeightField::solid(Material::Gravel);
    let plan = SpanPlan::new(&field, &field);
    let tile = MaterialTile::synthesise(Material::Gravel, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let mut tiles = no_tiles();
    tiles[0] = Some(&tile);

    let shift = material::params(Material::Gravel).grain_shift;
    let period = i32::try_from(tile.side() << shift).expect("a tile's world extent fits");
    let warp = Warp::new(SEED);

    let draw = |x: i32| {
        let origin = WorldPoint { x, y: 4_096 };
        let geometry = Geometry::new(&warp, origin, 1 << shift, 16);
        let mut row = vec![Pixel::TRANSPARENT; 16];
        splat(&mut row, &plan, &tiles, &geometry);
        row
    };
    assert_ne!(
        draw(0),
        draw(period),
        "one tile period apart draws identically, so the tile repeats",
    );
}

#[test]
fn a_span_is_reproducible() {
    let field = WeightField::solid(Material::Rock);
    let plan = SpanPlan::new(&field, &field);
    let tile = MaterialTile::synthesise(Material::Rock, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let mut tiles = no_tiles();
    tiles[0] = Some(&tile);
    let warp = Warp::new(SEED);
    let geometry = Geometry::new(&warp, WorldPoint { x: 999, y: -777 }, 24, 32);

    let mut first = vec![Pixel::TRANSPARENT; 32];
    splat(&mut first, &plan, &tiles, &geometry);
    let mut second = vec![Pixel::TRANSPARENT; 32];
    splat(&mut second, &plan, &tiles, &geometry);
    assert_eq!(first, second);
}

#[test]
fn a_plan_over_a_full_field_keeps_both_ends_normalised() {
    let mut left = WeightField::solid(Material::Rock);
    left.cover(Material::Gravel, 180);
    left.cover(Material::Sand, 90);
    let mut right = WeightField::solid(Material::Water);
    right.cover(Material::Saltmarsh, 140);

    let plan = SpanPlan::new(&left, &right);
    let near: u16 = plan.slots[..plan.used].iter().map(|s| s.near).sum();
    let far: u16 = plan.slots[..plan.used].iter().map(|s| s.far).sum();
    assert_eq!(near, TOTAL);
    assert_eq!(far, TOTAL);
}

#[test]
fn negative_world_coordinates_draw() {
    // Half the world is west and north of the origin, and a tile read
    // there must wrap rather than fault or clamp.
    let field = WeightField::solid(Material::Sand);
    let plan = SpanPlan::new(&field, &field);
    let tile = MaterialTile::synthesise(Material::Sand, Mip::BASE, Quality::FULL)
        .expect("a tile fits in test memory");
    let mut tiles = no_tiles();
    tiles[0] = Some(&tile);
    let warp = Warp::new(SEED);
    let geometry = Geometry::new(
        &warp,
        WorldPoint {
            x: -1_000_000,
            y: -2_000_000,
        },
        32,
        48,
    );
    let mut row = vec![Pixel::TRANSPARENT; 48];
    splat(&mut row, &plan, &tiles, &geometry);
    assert!(row.iter().all(|p| p.a == u8::MAX));
    assert!(row.iter().any(|p| *p != row[0]), "the row is flat");
}

#[test]
fn an_end_never_gains_a_material_its_own_field_lacks() {
    // Slots are ordered by *combined* prominence, so slot zero can hold
    // nothing at one end. Handing that end's rounding remainder to it
    // would put snow into a shoreline that has none.
    let left = WeightField::solid(Material::Snowfield);
    let mut right = WeightField::solid(Material::Water);
    right.cover(Material::Saltmarsh, 100);
    right.cover(Material::Moor, 60);
    right.cover(Material::Sand, 40);
    assert_eq!(right.weight_of(Material::Snowfield), 0);

    let plan = SpanPlan::new(&left, &right);
    let snow = plan.slots[..plan.used]
        .iter()
        .find(|s| s.material == Material::Snowfield)
        .expect("snow is the whole near end");
    assert_eq!(snow.far, 0, "snow leaked into the far end");

    let near: u16 = plan.slots[..plan.used].iter().map(|s| s.near).sum();
    let far: u16 = plan.slots[..plan.used].iter().map(|s| s.far).sum();
    assert_eq!((near, far), (TOTAL, TOTAL));
}

/// The reciprocal table floors exactly as the division it replaced, over
/// every divisor a blend can produce and every channel sum that divisor
/// admits.
///
/// Exhaustive rather than sampled: the domain is small, and the whole
/// value of replacing a division with a multiply is that the two agree
/// everywhere — a table that is right for most inputs is a wrong pixel
/// nobody would find by looking.
#[test]
fn reciprocals_are_exact() {
    for total in 1..=u32::try_from(MAX_BLEND_TOTAL).expect("the bound fits") {
        // A channel sum is a sum of `u8` texels weighted by the shares
        // that make up `total`, so it cannot exceed `255 * total`.
        for sum in 0..=255 * total {
            assert_eq!(
                mean(sum, total),
                u8::try_from(sum / total).unwrap_or(u8::MAX)
            );
        }
    }
}

/// A divisor outside the table's domain is clamped into it rather than
/// read past the end, so a later change to the blend's depth or slot
/// count cannot turn into an out-of-bounds read.
#[test]
fn a_divisor_past_the_domain_is_clamped() {
    let last = u32::try_from(MAX_BLEND_TOTAL).expect("the bound fits");
    assert_eq!(mean(255, 0), mean(255, 1), "zero takes the unit divisor");
    assert_eq!(mean(255, last + 1), mean(255, last));
}

/// The bound the table rests on: no slot can contribute more than the
/// blend depth, so the divisor stays inside the domain.
#[test]
fn the_blend_cannot_produce_a_divisor_past_the_domain() {
    assert_eq!(MAX_BLEND_TOTAL, BLEND_SLOTS * usize::from(BLEND_DEPTH));
}

/// A slot the blend skips could never have contributed anything.
///
/// The skip is the one place the kernel decides *not* to read a texel, so
/// it is checked against the definition it claims to follow rather than
/// against a rendered picture: over every weight pair and every relief a
/// texel can carry, a skipped slot's share must be zero.
#[test]
fn a_skipped_slot_could_not_have_reached_the_floor() {
    for heaviest in 0..=TOTAL {
        let cutoff = cutoff_below(heaviest);
        for weight in 0..cutoff {
            for height in 0..=u8::MAX {
                // The winning score is at least the heaviest weight, so
                // the floor is at least this.
                let floor = heaviest.saturating_sub(BLEND_DEPTH);
                let score = weight.saturating_add(u16::from(height) >> HEIGHT_SHIFT);
                assert_eq!(
                    score.saturating_sub(floor),
                    0,
                    "weight {weight} with relief {height} was skipped under {heaviest}"
                );
            }
        }
    }
}

/// The relief bound the skip rests on is the whole height range, so no
/// texel can lift a slot further than the cutoff assumed.
#[test]
fn relief_cannot_exceed_its_bound() {
    assert_eq!(MAX_HEIGHT_SCORE, u16::from(u8::MAX) >> HEIGHT_SHIFT);
    for height in 0..=u8::MAX {
        assert!(u16::from(height) >> HEIGHT_SHIFT <= MAX_HEIGHT_SCORE);
    }
}

/// Nothing is skipped when no slot is far enough ahead, so a genuinely
/// blended pixel still reads every texel.
#[test]
fn a_blended_pixel_skips_nothing() {
    assert_eq!(cutoff_below(0), 0);
    assert_eq!(cutoff_below(BLEND_DEPTH + MAX_HEIGHT_SCORE), 0);
    assert!(cutoff_below(TOTAL) > 0, "a dominant material does skip");
}
