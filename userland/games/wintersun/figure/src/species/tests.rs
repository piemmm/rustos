//! What each species may be, held to its own consistency.

use tairix_raster::Color;

use super::{
    Bounds, Species, DYES, EYES, FUR, FUR_MARKINGS, HAIR, HORN, LEATHER, SCALE, SKIN, TROUSERS,
    VOLUME,
};
use crate::identity::Setting;

#[test]
fn every_species_spells_itself_and_nothing_past_the_last_is_one() {
    for (position, species) in Species::ALL.into_iter().enumerate() {
        assert_eq!(usize::from(species.byte()), position);
        assert_eq!(Species::from_byte(species.byte()), Some(species));
    }
    let past = u8::try_from(Species::ALL.len()).expect("a small set");
    assert!((past..=u8::MAX).all(|byte| Species::from_byte(byte).is_none()));
}

#[test]
fn every_species_has_its_own_name() {
    for (index, species) in Species::ALL.iter().enumerate() {
        assert!(!species.name().is_empty());
        assert!(Species::ALL[index + 1..]
            .iter()
            .all(|other| other.name() != species.name()));
    }
}

/// An interval is its two ends and every straight step between them.
#[test]
fn a_setting_stands_where_its_interval_puts_it() {
    let bounds = Bounds::new(0.8, 1.2);
    assert!((bounds.at(Setting::LOW) - 0.8).abs() < 1e-12);
    assert!((bounds.at(Setting::HIGH) - 1.2).abs() < 1e-12);
    let mut last = f64::MIN;
    for value in 0..=u8::MAX {
        let here = bounds.at(Setting(value));
        assert!(here > last, "an interval never steps backward");
        last = here;
    }
}

/// Every build interval runs low to high, and every factor on the
/// reference's own proportion stays a real, positive one at both ends.
#[test]
fn every_build_interval_is_a_real_one() {
    for species in Species::ALL {
        let ranges = species.ranges();
        for (name, bounds, positive) in [
            ("height", ranges.height, true),
            ("girth", ranges.girth, true),
            ("taper", ranges.taper, false),
            ("limbs", ranges.limbs, true),
            ("head", ranges.head, true),
        ] {
            assert!(
                bounds.low() < bounds.high(),
                "{species:?} {name} does not run low to high"
            );
            if positive {
                assert!(bounds.low() > 0.0, "{species:?} {name} reaches zero");
            }
        }
        // A taper past a whole of either breadth would turn a figure inside
        // out at one end.
        assert!(ranges.taper.low() > -1.0 && ranges.taper.high() < 1.0);
    }
    assert!(VOLUME.low() > 0.0 && VOLUME.low() < VOLUME.high());
}

/// The reference the others are measured against stands its own height at
/// the middle of its range, and every species is smaller or larger than it
/// the way its description says.
#[test]
fn the_species_are_the_sizes_they_are_described_as() {
    let height = |species: Species| species.ranges().height;
    assert!(height(Species::Human).low() < 1.0 && height(Species::Human).high() > 1.0);
    assert!(height(Species::Dwarf).high() < height(Species::Human).low());
    assert!(height(Species::Elf).low() >= 1.0);
    assert!(
        Species::Dwarf.ranges().girth.low() > 1.0,
        "a dwarf is broad"
    );
    assert!(Species::Elf.ranges().girth.high() < Species::Human.ranges().girth.high());
}

#[test]
fn every_species_admits_at_least_one_of_every_feature_and_none_twice() {
    fn distinct<T: PartialEq + core::fmt::Debug>(forms: &[T]) {
        assert!(!forms.is_empty());
        for (index, form) in forms.iter().enumerate() {
            assert!(
                !forms[index + 1..].contains(form),
                "{form:?} is listed twice"
            );
        }
    }
    for species in Species::ALL {
        distinct(species.ears());
        distinct(species.horns());
        distinct(species.tails());
        distinct(species.eyes());
        assert!(species
            .eyes()
            .iter()
            .all(|index| usize::from(*index) < EYES.len()));
        assert!(!species.covering().is_empty());
    }
}

/// Every swatch in a table is a colour of its own, so choosing one is never
/// choosing another by accident.
#[test]
fn every_swatch_table_holds_distinct_colours() {
    let tables: [&[Color]; 8] = [
        &SKIN,
        &FUR,
        &SCALE,
        &HAIR,
        &EYES,
        &FUR_MARKINGS,
        &HORN,
        &DYES,
    ];
    for table in tables {
        for (index, colour) in table.iter().enumerate() {
            assert_eq!(colour.a, u8::MAX, "a swatch is opaque");
            assert!(
                !table[index + 1..].contains(colour),
                "{colour:?} is listed twice"
            );
        }
    }
    assert_eq!(TROUSERS.a, u8::MAX);
    assert_eq!(LEATHER.a, u8::MAX);
    assert_ne!(TROUSERS, LEATHER);
}

/// A species' markings are what its marked surfaces are drawn in: the
/// species that draw some have a table, and the ones that draw none have
/// none to choose from.
#[test]
fn only_a_species_with_markings_has_swatches_for_them() {
    for species in Species::ALL {
        let marked = species.horns().iter().any(Option::is_some)
            || species.tails().iter().any(Option::is_some);
        assert_eq!(!species.markings().is_empty(), marked, "{species:?}");
    }
}

/// Both ends of every interval are exactly its documented ends. The
/// straight step across `1.08 - 0.92` rounds, so without its clamp the
/// highest setting stood a rounding past the bound it is documented to
/// stay inside — which is what the record decoder's fuzz harness found.
#[test]
fn every_interval_ends_exactly_at_its_documented_ends() {
    let mut every = alloc::vec![VOLUME];
    for species in Species::ALL {
        let ranges = species.ranges();
        every.extend([
            ranges.height,
            ranges.girth,
            ranges.taper,
            ranges.limbs,
            ranges.head,
        ]);
    }
    for bounds in every {
        assert_eq!(bounds.at(Setting::LOW).to_bits(), bounds.low().to_bits());
        assert_eq!(bounds.at(Setting::HIGH).to_bits(), bounds.high().to_bits());
        for value in 0..=u8::MAX {
            let here = bounds.at(Setting(value));
            assert!(
                here >= bounds.low() && here <= bounds.high(),
                "{bounds:?} at {value}"
            );
        }
    }
}
