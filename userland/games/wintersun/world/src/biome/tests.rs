use super::{classify, Blend, Conditions, Material, BLEND_SLOTS, WEIGHT_TOTAL};
use crate::geom::{Moisture, Temperature};

fn conditions(celsius: f64, damp: f64) -> Conditions {
    Conditions {
        temperature: Temperature::from_celsius(celsius),
        moisture: Moisture::from_fraction(damp),
        elevation_units: 120.0,
        slope: 0.2,
        belt: 0.0,
        submerged: false,
        dryness: 1.0,
    }
}

#[test]
fn the_material_table_is_in_discriminant_order() {
    for (index, material) in Material::ALL.iter().enumerate() {
        assert_eq!(usize::from(material.id()), index);
    }
}

#[test]
fn every_blend_is_normalised() {
    for celsius_step in -40..45 {
        for damp_step in 0..=20 {
            for slope_step in 0..8 {
                for dryness_step in 0..4 {
                    let mut site = conditions(f64::from(celsius_step), f64::from(damp_step) / 20.0);
                    site.slope = f64::from(slope_step) * 0.6;
                    site.dryness = f64::from(dryness_step) / 3.0;
                    let blend = classify(site);
                    assert_eq!(
                        blend.total(),
                        WEIGHT_TOTAL,
                        "unnormalised blend at {celsius_step}C"
                    );
                }
            }
        }
    }
}

#[test]
fn a_blend_is_ordered_heaviest_first() {
    for celsius_step in -30..35 {
        for damp_step in 0..=10 {
            let blend = classify(conditions(
                f64::from(celsius_step),
                f64::from(damp_step) / 10.0,
            ));
            let weights = blend.weights();
            for slot in 1..BLEND_SLOTS {
                assert!(
                    weights[slot - 1] >= weights[slot],
                    "slot {slot} outweighs the one above it"
                );
            }
            assert_eq!(blend.dominant(), blend.materials()[0]);
        }
    }
}

#[test]
fn submerged_ground_is_water_and_nothing_else() {
    let mut site = conditions(4.0, 0.5);
    site.submerged = true;
    let blend = classify(site);
    assert_eq!(blend.dominant(), Material::Water);
    assert_eq!(blend.weights()[0], 255);
    assert_eq!(blend.total(), WEIGHT_TOTAL);
}

#[test]
fn a_face_is_rock() {
    let mut site = conditions(6.0, 0.6);
    site.slope = 6.0;
    assert_eq!(classify(site).dominant(), Material::Rock);
}

#[test]
fn the_cold_end_is_ice_and_the_warm_dry_end_is_ash() {
    assert_eq!(
        classify(conditions(-30.0, 0.5)).dominant(),
        Material::Glacier
    );
    let mut arid = conditions(24.0, 0.05);
    arid.dryness = 1.0;
    assert_eq!(classify(arid).dominant(), Material::Ashland);
}

#[test]
fn a_damp_temperate_cell_is_wooded() {
    let blend = classify(conditions(12.0, 0.62));
    assert_eq!(blend.dominant(), Material::TemperateForest);
    let boreal = classify(conditions(3.0, 0.68));
    assert_eq!(boreal.dominant(), Material::BorealForest);
}

#[test]
fn a_shore_is_sand_or_marsh_rather_than_woodland() {
    let mut warm_shore = conditions(14.0, 0.6);
    warm_shore.dryness = 0.0;
    warm_shore.slope = 0.05;
    let warm = classify(warm_shore);
    assert!(warm.materials()[..2].contains(&Material::Sand));

    let mut cold_shore = conditions(-2.0, 0.6);
    cold_shore.dryness = 0.0;
    cold_shore.slope = 0.05;
    let cold = classify(cold_shore);
    assert!(cold.materials()[..2].contains(&Material::Saltmarsh));
}

#[test]
fn torn_low_ground_inside_a_belt_shows_the_rift() {
    let mut site = conditions(2.0, 0.2);
    site.belt = 0.95;
    site.elevation_units = 20.0;
    assert!(classify(site).materials().contains(&Material::RiftWaste));
}

#[test]
fn a_solid_blend_is_normalised_too() {
    for material in Material::ALL {
        let blend = Blend::solid(material);
        assert_eq!(blend.total(), WEIGHT_TOTAL);
        assert_eq!(blend.dominant(), material);
    }
}

#[test]
fn classification_is_a_pure_function() {
    let site = conditions(5.0, 0.4);
    assert_eq!(classify(site), classify(site));
}
