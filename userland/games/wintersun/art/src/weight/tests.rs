use super::{WeightField, TOTAL};
use tairix_wintersun_world::biome::{Blend, Material, BLEND_SLOTS};

#[test]
fn a_solid_field_is_one_material_at_full_weight() {
    let field = WeightField::solid(Material::Rock);
    assert_eq!(field.slots().len(), 1);
    assert_eq!(field.dominant(), Material::Rock);
    assert_eq!(field.total(), TOTAL);
    assert_eq!(field.weight_of(Material::Rock), TOTAL);
    assert_eq!(field.weight_of(Material::Sand), 0);
}

#[test]
fn a_generated_blend_round_trips() {
    let blend = Blend::solid(Material::BorealForest);
    let field = WeightField::from_blend(&blend);
    assert_eq!(field.slots().len(), 1);
    assert_eq!(field.dominant(), Material::BorealForest);
    assert_eq!(field.total(), TOTAL);
}

#[test]
fn covering_reaches_at_least_the_coverage_asked_for() {
    for coverage in [1u16, 17, 64, 128, 200, 254, 255] {
        let mut field = WeightField::solid(Material::TemperateForest);
        assert!(field.cover(Material::Gravel, coverage));
        assert!(
            field.weight_of(Material::Gravel) >= coverage,
            "asked {coverage}, got {}",
            field.weight_of(Material::Gravel),
        );
        assert_eq!(field.total(), TOTAL);
    }
}

#[test]
fn covering_twice_at_the_same_coverage_is_a_no_op() {
    let mut field = WeightField::solid(Material::ColdSteppe);
    assert!(field.cover(Material::Gravel, 180));
    let once = field;
    assert!(!field.cover(Material::Gravel, 180));
    assert_eq!(field, once);
}

#[test]
fn covering_is_order_independent_for_one_material() {
    // Two roads crossing: stamped either way round, the junction is the
    // same road.
    let mut a = WeightField::solid(Material::Moor);
    a.cover(Material::Gravel, 120);
    a.cover(Material::Gravel, 200);

    let mut b = WeightField::solid(Material::Moor);
    b.cover(Material::Gravel, 200);
    b.cover(Material::Gravel, 120);

    assert_eq!(a, b);
    assert!(a.weight_of(Material::Gravel) >= 200);
}

#[test]
fn covering_fully_replaces_the_field() {
    let mut field = WeightField::solid(Material::Saltmarsh);
    assert!(field.cover(Material::Water, TOTAL));
    assert_eq!(field.slots().len(), 1);
    assert_eq!(field.dominant(), Material::Water);
    assert_eq!(field.total(), TOTAL);
}

#[test]
fn covering_below_the_current_share_changes_nothing() {
    let mut field = WeightField::solid(Material::Sand);
    assert!(!field.cover(Material::Sand, 200));
    assert_eq!(field.weight_of(Material::Sand), TOTAL);
}

#[test]
fn a_faint_stamp_on_a_full_field_is_refused() {
    let mut field = WeightField::solid(Material::Rock);
    for (material, coverage) in [
        (Material::Gravel, 200u16),
        (Material::Sand, 120),
        (Material::Moor, 60),
    ] {
        assert!(field.cover(material, coverage));
    }
    assert_eq!(field.slots().len(), BLEND_SLOTS);
    let lightest = field.slots()[BLEND_SLOTS - 1].weight;
    let before = field;
    assert!(!field.cover(Material::Ashland, lightest));
    assert_eq!(
        field, before,
        "a stamp lighter than the field displaced one"
    );
}

#[test]
fn a_heavy_stamp_on_a_full_field_displaces_the_lightest() {
    let mut field = WeightField::solid(Material::Rock);
    for (material, coverage) in [
        (Material::Gravel, 200u16),
        (Material::Sand, 120),
        (Material::Moor, 60),
    ] {
        assert!(field.cover(material, coverage));
    }
    let displaced = field.slots()[BLEND_SLOTS - 1].material;
    assert!(field.cover(Material::Snowfield, 250));
    assert_eq!(field.slots().len(), BLEND_SLOTS);
    assert_eq!(field.weight_of(displaced), 0);
    assert!(field.weight_of(Material::Snowfield) >= 250);
    assert_eq!(field.total(), TOTAL);
}

#[test]
fn covering_never_exceeds_four_materials() {
    let mut field = WeightField::solid(Material::Tundra);
    for (i, material) in Material::ALL.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation, reason = "fifteen materials")]
        field.cover(*material, 40 + (i as u16) * 13);
        assert!(field.slots().len() <= BLEND_SLOTS);
        assert_eq!(field.total(), TOTAL);
    }
}

#[test]
fn slots_are_ordered_heaviest_first() {
    let mut field = WeightField::solid(Material::FellHeath);
    field.cover(Material::Gravel, 90);
    field.cover(Material::Snowfield, 150);
    field.cover(Material::Sand, 30);
    let weights: alloc::vec::Vec<u16> = field.slots().iter().map(|s| s.weight).collect();
    let mut sorted = weights.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(weights, sorted);
}

#[test]
fn lerp_reproduces_its_endpoints() {
    let a = WeightField::solid(Material::Glacier);
    let mut b = WeightField::solid(Material::Ashland);
    b.cover(Material::RiftWaste, 100);

    assert_eq!(a.lerp(&b, 0), a);
    assert_eq!(a.lerp(&b, 255), b);
}

#[test]
fn lerp_fades_a_material_in_rather_than_switching_to_it() {
    let a = WeightField::solid(Material::Snowfield);
    let b = WeightField::solid(Material::Rock);
    let mut previous = 0;
    for t in [0u8, 32, 64, 96, 128, 160, 192, 224, 255] {
        let mixed = a.lerp(&b, t);
        let rock = mixed.weight_of(Material::Rock);
        assert!(rock >= previous, "rock went backwards at t={t}");
        assert_eq!(mixed.total(), TOTAL);
        previous = rock;
    }
    assert_eq!(a.lerp(&b, 128).weight_of(Material::Rock), 128);
}

#[test]
fn lerp_stays_normalised_over_disjoint_material_sets() {
    let mut a = WeightField::solid(Material::Water);
    a.cover(Material::Saltmarsh, 100);
    let mut b = WeightField::solid(Material::Glacier);
    b.cover(Material::Snowfield, 100);

    for t in 0..=u8::MAX {
        let mixed = a.lerp(&b, t);
        assert_eq!(mixed.total(), TOTAL, "unnormalised at t={t}");
        assert!(!mixed.slots().is_empty());
        assert!(mixed.slots().iter().all(|s| s.weight > 0));
    }
}

#[test]
fn equality_ignores_the_unused_tail() {
    // `solid` fills every slot; `cover` back to solid leaves the tail at
    // zero. Both are one material at full weight and must compare equal.
    let direct = WeightField::solid(Material::Gravel);
    let mut covered = WeightField::solid(Material::Moor);
    covered.cover(Material::Gravel, TOTAL);
    assert_eq!(direct, covered);
}
