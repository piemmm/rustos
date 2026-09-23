//! What a record admits, what it refuses and why, and that it has one
//! spelling.

use alloc::string::ToString;

use super::{
    Build, EarForm, EyeShape, FaceShape, Features, Field, HairStyle, HornForm, Identity,
    IdentityError, Palette, Setting, Spec, TailForm, RECORD_LEN, RECORD_VERSION,
};
use crate::reference::{self, FIGURES};
use crate::species::{Bounds, Species, DYES, EYES, HAIR, LEATHER, TROUSERS};
use crate::tint::Tint;

/// Where each field sits in a record.
const SPECIES: usize = 1;
const FACE: usize = 7;
const EYE_SHAPE: usize = 8;
const EARS: usize = 9;
const HORNS: usize = 10;
const TAIL: usize = 11;
const HAIR_STYLE: usize = 12;
const VOLUME: usize = 13;
const SKIN: usize = 14;
const HAIR_COLOUR: usize = 15;
const EYE_COLOUR: usize = 16;
const MARKINGS: usize = 17;
const ACCENT: usize = 18;

fn human() -> Spec {
    reference::spec(Species::Human)
}

fn refused(spec: Spec) -> IdentityError {
    Identity::new(spec).expect_err("this record must be refused")
}

/// `record` with byte `at` set to `value`.
fn with(record: [u8; RECORD_LEN], at: usize, value: u8) -> [u8; RECORD_LEN] {
    let mut out = record;
    out[at] = value;
    out
}

#[test]
fn every_grid_figure_round_trips_exactly() {
    for figure in &FIGURES {
        let identity = figure.identity().expect("a real record");
        let bytes = identity.encode();
        assert_eq!(bytes.len(), RECORD_LEN);
        assert_eq!(bytes[0], RECORD_VERSION);
        let back = Identity::decode(&bytes).expect("its own record decodes");
        assert_eq!(back, identity);
        assert_eq!(
            back.spec(),
            figure.spec,
            "{} read back differently",
            figure.name
        );
        assert_eq!(back.encode(), bytes);
    }
}

#[test]
fn a_record_of_any_other_length_is_refused_by_how_it_is_wrong() {
    let record = Identity::new(human()).expect("real").encode();
    for len in 0..RECORD_LEN {
        assert_eq!(
            Identity::decode(&record[..len]),
            Err(IdentityError::Truncated)
        );
    }
    let mut long = [0u8; RECORD_LEN + 8];
    long[..RECORD_LEN].copy_from_slice(&record);
    for len in RECORD_LEN + 1..=long.len() {
        assert_eq!(
            Identity::decode(&long[..len]),
            Err(IdentityError::TrailingBytes)
        );
    }
}

#[test]
fn a_record_in_another_format_is_refused_rather_than_misread() {
    let record = Identity::new(human()).expect("real").encode();
    for version in (0..=u8::MAX).filter(|v| *v != RECORD_VERSION) {
        assert_eq!(
            Identity::decode(&with(record, 0, version)),
            Err(IdentityError::UnknownVersion)
        );
    }
}

#[test]
fn a_byte_naming_nothing_is_refused_with_its_field() {
    let record = Identity::new(human()).expect("real").encode();
    let cases: [(usize, u8, Field); 7] = [
        (SPECIES, 5, Field::Species),
        (FACE, 5, Field::Face),
        (EYE_SHAPE, 3, Field::Eyes),
        (EARS, 6, Field::Ears),
        (HORNS, 5, Field::Horns),
        (TAIL, 4, Field::Tail),
        (HAIR_STYLE, 5, Field::Hair),
    ];
    for (at, first_unknown, field) in cases {
        for value in first_unknown..=u8::MAX {
            assert_eq!(
                Identity::decode(&with(record, at, value)),
                Err(IdentityError::Unknown(field)),
                "byte {at} = {value}"
            );
        }
    }
}

#[test]
fn every_form_spells_itself_and_nothing_past_the_last_is_a_form() {
    fn check<T: Copy + PartialEq + core::fmt::Debug>(
        all: &[T],
        byte: fn(T) -> u8,
        from: fn(u8) -> Option<T>,
    ) {
        for (position, form) in all.iter().enumerate() {
            assert_eq!(
                usize::from(byte(*form)),
                position,
                "{form:?} is out of order"
            );
            assert_eq!(from(byte(*form)), Some(*form));
        }
        let past = u8::try_from(all.len()).expect("a small set");
        assert!((past..=u8::MAX).all(|value| from(value).is_none()));
    }
    check(FaceShape::ALL, FaceShape::byte, FaceShape::from_byte);
    check(EyeShape::ALL, EyeShape::byte, EyeShape::from_byte);
    check(EarForm::ALL, EarForm::byte, EarForm::from_byte);
    check(HornForm::ALL, HornForm::byte, HornForm::from_byte);
    check(TailForm::ALL, TailForm::byte, TailForm::from_byte);
    check(HairStyle::ALL, HairStyle::byte, HairStyle::from_byte);
}

#[test]
fn a_feature_the_species_does_not_carry_is_refused() {
    let mut tailed = human();
    tailed.features.tail = Some(TailForm::Brush);
    assert_eq!(refused(tailed), IdentityError::NotOfSpecies(Field::Tail));

    let mut horned = human();
    horned.features.horns = Some(HornForm::Spire);
    assert_eq!(refused(horned), IdentityError::NotOfSpecies(Field::Horns));

    let mut eared = human();
    eared.features.ears = EarForm::Long;
    assert_eq!(refused(eared), IdentityError::NotOfSpecies(Field::Ears));

    // Horns are what a dragonkin is; one without them is not a dragonkin.
    let mut hornless = reference::spec(Species::Dragonkin);
    hornless.features.horns = None;
    assert_eq!(refused(hornless), IdentityError::NotOfSpecies(Field::Horns));

    // A dragon's red eyes are not a human's.
    let mut red = human();
    red.palette.eyes = 9;
    assert_eq!(refused(red), IdentityError::NotOfSpecies(Field::EyeColour));
}

#[test]
fn a_swatch_past_its_table_is_refused_with_its_slot() {
    let spec = human();
    let past = |len: usize| u8::try_from(len).expect("a small table");
    let mut skin = spec;
    skin.palette.skin = past(Species::Human.covering().len());
    assert_eq!(refused(skin), IdentityError::Unknown(Field::SkinColour));
    let mut hair = spec;
    hair.palette.hair = past(HAIR.len());
    assert_eq!(refused(hair), IdentityError::Unknown(Field::HairColour));
    let mut eyes = spec;
    eyes.palette.eyes = past(EYES.len());
    assert_eq!(refused(eyes), IdentityError::Unknown(Field::EyeColour));
    let mut accent = spec;
    accent.palette.accent = past(DYES.len());
    assert_eq!(refused(accent), IdentityError::Unknown(Field::AccentColour));
    let mut markings = reference::spec(Species::Beastkin);
    markings.palette.markings = past(Species::Beastkin.markings().len());
    assert_eq!(
        refused(markings),
        IdentityError::Unknown(Field::MarkingsColour)
    );
}

#[test]
fn a_second_spelling_of_a_figure_is_refused() {
    // A bald figure has no hair to colour or fill, so the one spelling of
    // each is zero.
    let mut bald = human();
    bald.features.hair = None;
    bald.features.volume = Setting::LOW;
    bald.palette.hair = 0;
    assert!(Identity::new(bald).is_ok());
    let mut full = bald;
    full.features.volume = Setting(1);
    assert_eq!(refused(full), IdentityError::NonCanonical(Field::Volume));
    let mut coloured = bald;
    coloured.palette.hair = 1;
    assert_eq!(
        refused(coloured),
        IdentityError::NonCanonical(Field::HairColour)
    );

    // A human has no markings to colour.
    let mut marked = human();
    marked.palette.markings = 1;
    assert_eq!(
        refused(marked),
        IdentityError::NonCanonical(Field::MarkingsColour)
    );
}

/// Every single-byte change to every grid figure's record either decodes to
/// a record whose own bytes are exactly the changed ones, or is refused —
/// so no byte has two meanings and none is read past its field.
#[test]
fn every_single_byte_change_is_a_different_record_or_a_refusal() {
    for figure in &FIGURES {
        let original = figure.identity().expect("a real record");
        let record = original.encode();
        for at in 0..RECORD_LEN {
            for value in 0..=u8::MAX {
                let changed = with(record, at, value);
                if let Ok(identity) = Identity::decode(&changed) {
                    assert_eq!(identity.encode(), changed, "{} byte {at}", figure.name);
                    assert_eq!(identity == original, value == record[at]);
                }
            }
        }
    }
}

#[test]
fn a_setting_spans_its_interval_end_to_end() {
    assert!(Setting::LOW.fraction().abs() < f64::EPSILON);
    assert!((Setting::HIGH.fraction() - 1.0).abs() < f64::EPSILON);
    let mut last = -1.0;
    for value in 0..=u8::MAX {
        let fraction = Setting(value).fraction();
        assert!(fraction > last && (0.0..=1.0).contains(&fraction));
        last = fraction;
    }
}

#[test]
fn proportions_are_where_the_settings_stand_in_the_species_ranges() {
    for species in Species::ALL {
        let ranges = species.ranges();
        for (setting, pick) in [
            (Setting::LOW, Bounds::low as fn(Bounds) -> f64),
            (Setting::HIGH, Bounds::high),
        ] {
            let mut spec = reference::spec(species);
            spec.build = Build {
                height: setting,
                girth: setting,
                taper: setting,
                limbs: setting,
                head: setting,
            };
            let made = Identity::new(spec)
                .expect("a setting is never out of range")
                .proportions();
            assert_eq!(made.height.to_bits(), pick(ranges.height).to_bits());
            assert_eq!(made.girth.to_bits(), pick(ranges.girth).to_bits());
            assert_eq!(made.taper.to_bits(), pick(ranges.taper).to_bits());
            assert_eq!(made.limbs.to_bits(), pick(ranges.limbs).to_bits());
            assert_eq!(made.head.to_bits(), pick(ranges.head).to_bits());
        }
    }
}

#[test]
fn a_figure_resolves_each_slot_to_the_swatch_it_names() {
    let identity = Identity::new(reference::spec(Species::Beastkin)).expect("real");
    let spec = identity.spec();
    let tints = identity.tints();
    assert_eq!(
        tints.get(Tint::Skin),
        Species::Beastkin.covering()[usize::from(spec.palette.skin)]
    );
    assert_eq!(tints.get(Tint::Hair), HAIR[usize::from(spec.palette.hair)]);
    assert_eq!(tints.get(Tint::Eyes), EYES[usize::from(spec.palette.eyes)]);
    assert_eq!(
        tints.get(Tint::Markings),
        Species::Beastkin.markings()[usize::from(spec.palette.markings)]
    );
    assert_eq!(
        tints.get(Tint::Accent),
        DYES[usize::from(spec.palette.accent)]
    );
    assert_eq!(tints.get(Tint::Trousers), TROUSERS);
    assert_eq!(tints.get(Tint::Leather), LEATHER);

    // A species with no markings carries its skin in that slot rather than a
    // colour nobody chose, and draws nothing in it.
    let plain = Identity::new(human()).expect("real");
    assert_eq!(
        plain.tints().get(Tint::Markings),
        plain.tints().get(Tint::Skin)
    );
}

#[test]
fn every_refusal_says_what_was_wrong() {
    let every = [
        IdentityError::Truncated,
        IdentityError::TrailingBytes,
        IdentityError::UnknownVersion,
        IdentityError::Unknown(Field::Species),
        IdentityError::NotOfSpecies(Field::Tail),
        IdentityError::NonCanonical(Field::Volume),
    ];
    for refusal in every {
        let text = refusal.to_string();
        assert!(!text.is_empty() && text.contains("figure record"), "{text}");
    }
    assert!(IdentityError::NotOfSpecies(Field::EyeColour)
        .to_string()
        .contains("eye colour"));
}

#[test]
fn a_record_names_its_spec_and_species() {
    let spec = Spec {
        species: Species::Elf,
        build: Build::default(),
        features: Features {
            face: FaceShape::Heart,
            eyes: EyeShape::Narrow,
            ears: EarForm::Pointed,
            horns: None,
            tail: None,
            hair: Some(HairStyle::Cropped),
            volume: Setting::HIGH,
        },
        palette: Palette {
            skin: 3,
            hair: 11,
            eyes: 7,
            markings: 0,
            accent: 5,
        },
    };
    let identity = Identity::new(spec).expect("a real elf");
    assert_eq!(identity.spec(), spec);
    assert_eq!(identity.species(), Species::Elf);
    let bytes = identity.encode();
    assert_eq!(bytes[SKIN], 3);
    assert_eq!(bytes[HAIR_COLOUR], 11);
    assert_eq!(bytes[EYE_COLOUR], 7);
    assert_eq!(bytes[MARKINGS], 0);
    assert_eq!(bytes[ACCENT], 5);
    assert_eq!(bytes[VOLUME], u8::MAX);
    assert_eq!(
        bytes[HAIR_STYLE],
        HairStyle::Cropped.byte() + 1,
        "zero is no hair"
    );
}
