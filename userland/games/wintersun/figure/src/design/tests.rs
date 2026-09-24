//! What an edit costs, what it refuses, and that the record stays one.

use alloc::vec::Vec;

use tairix_rng::{NonCryptoRng, RandU64};

use super::{apart, canonical, Change, Designer, Edit};
use crate::humanoid;
use crate::identity::{
    EarForm, EyeShape, FaceShape, Field, HairStyle, HornForm, Identity, IdentityError, Setting,
    Spec, TailForm,
};
use crate::reference::{self, FIGURES};
use crate::species::{Species, EYES};

fn open(species: Species) -> Designer {
    Designer::open(reference::identity(species).expect("a real record"))
}

/// A bald human, which the grid's least human is.
fn bald() -> Designer {
    let least = FIGURES
        .iter()
        .find(|figure| figure.name == "human-least")
        .expect("the grid's least human");
    assert_eq!(least.spec.features.hair, None);
    Designer::open(least.identity().expect("a real record"))
}

/// Every edit a designer can ask for, over every value each field takes that
/// is worth distinguishing: every form, every byte of every swatch, and the
/// ends and middle of every setting.
fn every_edit() -> Vec<Edit> {
    let mut edits = Vec::new();
    for setting in [Setting::LOW, Setting(1), Setting(128), Setting::HIGH] {
        edits.extend([
            Edit::Height(setting),
            Edit::Girth(setting),
            Edit::Taper(setting),
            Edit::Limbs(setting),
            Edit::Head(setting),
            Edit::Volume(setting),
        ]);
    }
    edits.extend(Species::ALL.map(Edit::Species));
    edits.extend(FaceShape::ALL.iter().map(|face| Edit::Face(*face)));
    edits.extend(EyeShape::ALL.iter().map(|eyes| Edit::EyeShape(*eyes)));
    edits.extend(EarForm::ALL.iter().map(|ears| Edit::Ears(*ears)));
    edits.extend(optional(HornForm::ALL).map(Edit::Horns));
    edits.extend(optional(TailForm::ALL).map(Edit::Tail));
    edits.extend(optional(HairStyle::ALL).map(Edit::Hair));
    for swatch in 0..=u8::MAX {
        edits.extend([
            Edit::Skin(swatch),
            Edit::HairColour(swatch),
            Edit::EyeColour(swatch),
            Edit::Markings(swatch),
            Edit::Accent(swatch),
        ]);
    }
    edits
}

fn optional<T: Copy>(forms: &'static [T]) -> impl Iterator<Item = Option<T>> {
    forms.iter().map(|form| Some(*form)).chain([None])
}

/// However many samples a drag takes, it writes once, when it settles.
#[test]
fn an_edit_writes_nothing_until_its_interaction_settles() {
    let mut designer = open(Species::Dwarf);
    for step in 0..=u8::MAX {
        designer
            .edit(Edit::Height(Setting(step)))
            .expect("every setting is in range");
    }
    let written = designer.settle();
    assert_eq!(written, Some(designer.live()));
    assert_eq!(
        written.map(|record| record.spec().build.height),
        Some(Setting::HIGH)
    );
    assert_eq!(designer.settle(), None, "one interaction wrote twice");
}

/// A drag that comes back to where it started leaves nothing to write.
#[test]
fn a_drag_that_ends_where_it_began_writes_nothing() {
    let mut designer = open(Species::Elf);
    let start = designer.live();
    for step in [10, 200, 90, start.spec().build.girth.0] {
        designer
            .edit(Edit::Girth(Setting(step)))
            .expect("every setting is in range");
    }
    assert_eq!(designer.live(), start);
    assert_eq!(designer.settle(), None);
}

/// A picked preset or a drawn figure is a whole interaction: one write, and
/// none for picking the record already there.
#[test]
fn a_pick_is_one_write() {
    let mut designer = open(Species::Human);
    let elf = reference::identity(Species::Elf).expect("a real record");
    designer.apply(elf);
    assert_eq!(designer.live(), elf);
    assert_eq!(designer.settle(), Some(elf));
    designer.apply(elf);
    assert_eq!(designer.settle(), None);
}

/// A palette edit never owes more than a re-tint, and the rig it would have
/// rebuilt is the same rig: no joint and no surface moves.
#[test]
fn a_palette_edit_owes_a_retint_and_moves_nothing() {
    for species in Species::ALL {
        let designer = open(species);
        let was = designer.live();
        let before = humanoid::rig(&was).expect("it builds");
        for edit in every_edit() {
            if !matches!(
                edit,
                Edit::Skin(_)
                    | Edit::HairColour(_)
                    | Edit::EyeColour(_)
                    | Edit::Markings(_)
                    | Edit::Accent(_)
            ) {
                continue;
            }
            let mut edited = designer;
            if edited.edit(edit).is_err() {
                continue;
            }
            let now = edited.live();
            let owed = Change::between(&was, &now);
            assert_eq!(
                owed,
                if now == was {
                    Change::Nothing
                } else {
                    Change::Tints
                },
                "{species:?} {edit:?}"
            );
            let after = humanoid::rig(&now).expect("it builds");
            assert_eq!(after.joints(), before.joints(), "{edit:?} moved a joint");
            assert_eq!(after.parts(), before.parts(), "{edit:?} moved a surface");
        }
    }
}

/// Anything that shapes the figure owes a rebuild.
#[test]
fn a_build_or_feature_edit_owes_a_rebuild() {
    let designer = open(Species::Beastkin);
    let was = designer.live();
    for edit in [
        Edit::Height(Setting(3)),
        Edit::Girth(Setting(250)),
        Edit::Taper(Setting(0)),
        Edit::Limbs(Setting(77)),
        Edit::Head(Setting(222)),
        Edit::Face(FaceShape::Broad),
        Edit::EyeShape(EyeShape::Round),
        Edit::Ears(EarForm::Lop),
        Edit::Horns(Some(HornForm::Nubs)),
        Edit::Tail(None),
        Edit::Hair(Some(HairStyle::Shaggy)),
        Edit::Volume(Setting(7)),
        Edit::Species(Species::Human),
    ] {
        let mut edited = designer;
        edited.edit(edit).expect("a beastkin carries all of these");
        assert_eq!(
            Change::between(&was, &edited.live()),
            Change::Rig,
            "{edit:?}"
        );
    }
}

#[test]
fn the_record_already_drawn_owes_nothing() {
    for figure in &FIGURES {
        let identity = figure.identity().expect("a real record");
        assert_eq!(Change::between(&identity, &identity), Change::Nothing);
    }
    assert!(Change::Nothing < Change::Tints && Change::Tints < Change::Rig);
}

/// A value the species cannot carry is refused with the field the record
/// decoder would name, and the designer is left exactly as it was.
#[test]
fn an_edit_the_record_cannot_carry_is_refused_by_name_and_changes_nothing() {
    let cases = [
        (
            open(Species::Human),
            Edit::Ears(EarForm::Lop),
            IdentityError::NotOfSpecies(Field::Ears),
        ),
        (
            open(Species::Human),
            Edit::Horns(Some(HornForm::Nubs)),
            IdentityError::NotOfSpecies(Field::Horns),
        ),
        (
            open(Species::Elf),
            Edit::Tail(Some(TailForm::Brush)),
            IdentityError::NotOfSpecies(Field::Tail),
        ),
        (
            open(Species::Dragonkin),
            Edit::Horns(None),
            IdentityError::NotOfSpecies(Field::Horns),
        ),
        (
            open(Species::Dragonkin),
            Edit::Tail(None),
            IdentityError::NotOfSpecies(Field::Tail),
        ),
        (
            open(Species::Beastkin),
            Edit::Skin(10),
            IdentityError::Unknown(Field::SkinColour),
        ),
        (
            open(Species::Human),
            Edit::Skin(12),
            IdentityError::Unknown(Field::SkinColour),
        ),
        (
            open(Species::Human),
            Edit::Markings(1),
            IdentityError::NonCanonical(Field::MarkingsColour),
        ),
        (
            open(Species::Beastkin),
            Edit::Markings(8),
            IdentityError::Unknown(Field::MarkingsColour),
        ),
        (
            open(Species::Elf),
            Edit::EyeColour(9),
            IdentityError::NotOfSpecies(Field::EyeColour),
        ),
        (
            open(Species::Human),
            Edit::EyeColour(12),
            IdentityError::Unknown(Field::EyeColour),
        ),
        (
            open(Species::Human),
            Edit::HairColour(16),
            IdentityError::Unknown(Field::HairColour),
        ),
        (
            open(Species::Human),
            Edit::Accent(16),
            IdentityError::Unknown(Field::AccentColour),
        ),
        (
            bald(),
            Edit::HairColour(3),
            IdentityError::NonCanonical(Field::HairColour),
        ),
        (
            bald(),
            Edit::Volume(Setting(9)),
            IdentityError::NonCanonical(Field::Volume),
        ),
    ];
    for (designer, edit, refusal) in cases {
        let mut edited = designer;
        assert_eq!(edited.edit(edit), Err(refusal), "{edit:?}");
        assert_eq!(edited, designer, "a refused {edit:?} changed the designer");
    }
}

/// Every edit from every figure of the grid either takes — and then the
/// live record is exactly the old one with that field set, unless the edit
/// is one that reshapes the others — or is refused and changes nothing.
#[test]
fn every_edit_from_every_record_leaves_a_record_that_shows_it() {
    let edits = every_edit();
    for figure in &FIGURES {
        let designer = Designer::open(figure.identity().expect("a real record"));
        for edit in &edits {
            let mut edited = designer;
            match edited.edit(*edit) {
                Ok(()) => {
                    if !edit.reshapes() {
                        assert_eq!(
                            edited.live().spec(),
                            edit.written(designer.live().spec()),
                            "{} {edit:?}",
                            figure.name
                        );
                    }
                }
                Err(refusal) => {
                    assert!(
                        !edit.reshapes(),
                        "{} refused {edit:?}: {refusal:?}",
                        figure.name
                    );
                    assert_eq!(edited, designer, "{} {edit:?}", figure.name);
                }
            }
        }
    }
}

/// Going bald zeroes the hair's colour and volume, and growing it back
/// gives back the hair the figure had.
#[test]
fn going_bald_zeroes_the_hair_and_growing_it_back_restores_it() {
    let mut designer = open(Species::Human);
    let haired = designer.live();
    let style = haired.spec().features.hair;
    assert!(style.is_some());

    designer.edit(Edit::Hair(None)).expect("anyone may be bald");
    let shorn = designer.live().spec();
    assert_eq!(shorn.features.volume, Setting::LOW);
    assert_eq!(shorn.palette.hair, 0);

    designer
        .edit(Edit::Hair(style))
        .expect("anyone may have hair");
    assert_eq!(designer.live(), haired);
}

/// A new species keeps every choice it can carry: a setting is a position
/// within the species, so the middle of one is the middle of the next.
#[test]
fn a_species_change_keeps_every_choice_the_new_species_carries() {
    let mut designer = open(Species::Human);
    let human = designer.live().spec();
    designer
        .edit(Edit::Species(Species::Elf))
        .expect("any species");
    let elf = designer.live().spec();
    assert_eq!(elf.species, Species::Elf);
    assert_eq!(elf.build, human.build);
    assert_eq!(elf.features.face, human.features.face);
    assert_eq!(elf.features.eyes, human.features.eyes);
    assert_eq!(elf.features.hair, human.features.hair);
    assert_eq!(elf.features.volume, human.features.volume);
    assert_eq!(elf.palette.skin, human.palette.skin);
    assert_eq!(elf.palette.hair, human.palette.hair);
    assert_eq!(elf.palette.eyes, human.palette.eyes);
    assert_eq!(elf.palette.accent, human.palette.accent);
}

/// A swatch past a shorter table is clamped to its last, and the choice
/// comes back with the species that has room for it.
#[test]
fn a_species_change_clamps_a_swatch_and_gives_it_back_on_return() {
    let mut designer = open(Species::Human);
    designer.edit(Edit::Skin(11)).expect("the darkest skin");
    designer
        .edit(Edit::Species(Species::Beastkin))
        .expect("any species");
    let last = u8::try_from(Species::Beastkin.covering().len() - 1).expect("a small table");
    assert_eq!(designer.live().spec().palette.skin, last);
    designer
        .edit(Edit::Species(Species::Human))
        .expect("any species");
    assert_eq!(designer.live().spec().palette.skin, 11);
}

/// A form the new species does not carry becomes the first it does, and an
/// eye colour it does not admit becomes the admitted one nearest in colour.
#[test]
fn a_species_change_replaces_what_the_new_species_does_not_carry() {
    let mut designer = open(Species::Beastkin);
    designer
        .edit(Edit::Species(Species::Human))
        .expect("any species");
    let human = designer.live().spec();
    assert_eq!(human.features.ears, EarForm::Round);
    assert_eq!(human.features.tail, None);
    assert_eq!(human.palette.markings, 0);

    let mut designer = open(Species::Human);
    designer
        .edit(Edit::Species(Species::Elf))
        .expect("any species");
    assert_eq!(designer.live().spec().features.ears, Species::Elf.ears()[0]);

    // Violet, which an elf may have and a human may not, reads nearest to
    // blue among the colours a human admits.
    let mut designer = open(Species::Elf);
    designer.edit(Edit::EyeColour(7)).expect("an elf's violet");
    designer
        .edit(Edit::Species(Species::Human))
        .expect("any species");
    assert_eq!(designer.live().spec().palette.eyes, 5);

    for species in Species::ALL {
        for chosen in 0..u8::try_from(EYES.len()).expect("a small table") {
            let mut spec = reference::spec(species);
            spec.palette.eyes = chosen;
            let eyes = canonical(spec).palette.eyes;
            assert!(species.eyes().contains(&eyes));
            let distance = |eye: u8| apart(EYES[usize::from(eye)], EYES[usize::from(chosen)]);
            assert!(
                species
                    .eyes()
                    .iter()
                    .all(|other| distance(*other) >= distance(eyes)),
                "{species:?} replaced eye {chosen} with {eyes}, not the nearest"
            );
        }
    }
}

/// A species that must carry a form is given one.
#[test]
fn a_required_form_is_given() {
    let mut designer = open(Species::Human);
    designer
        .edit(Edit::Species(Species::Dragonkin))
        .expect("any species");
    let features = designer.live().spec().features;
    assert_eq!(features.horns, Species::Dragonkin.horns()[0]);
    assert_eq!(features.tail, Some(TailForm::Scaled));
}

/// A round trip through any other species gives back exactly the figure it
/// left, from every figure of the grid.
#[test]
fn a_round_trip_through_any_species_gives_back_the_figure_it_left() {
    for figure in &FIGURES {
        let identity = figure.identity().expect("a real record");
        for other in Species::ALL {
            let mut designer = Designer::open(identity);
            designer.edit(Edit::Species(other)).expect("any species");
            designer
                .edit(Edit::Species(identity.species()))
                .expect("any species");
            assert_eq!(
                designer.live(),
                identity,
                "{} through {other:?}",
                figure.name
            );
        }
    }
}

/// The projection keeps a record exactly as it is, turns any choices at all
/// into a record, and leaves its own output alone.
#[test]
fn any_choices_come_to_a_record_and_a_record_comes_to_itself() {
    for figure in &FIGURES {
        assert_eq!(canonical(figure.spec), figure.spec, "{}", figure.name);
    }
    let mut rng = NonCryptoRng::seed_from_u64(0x4445_5349_474E);
    for _ in 0..20_000 {
        let chosen = arbitrary(&mut rng);
        let spec = canonical(chosen);
        assert_eq!(
            Identity::new(spec).map(|identity| identity.spec()),
            Ok(spec),
            "{chosen:?}"
        );
        assert_eq!(canonical(spec), spec);
    }
}

/// Choices nothing checked: any species, any form of any feature, any byte
/// in every swatch.
fn arbitrary(rng: &mut NonCryptoRng) -> Spec {
    fn any<T: Copy>(rng: &mut NonCryptoRng, items: &[T]) -> T {
        let len = u64::try_from(items.len()).expect("a small table");
        items[usize::try_from(rng.next_below(len)).expect("a small table")]
    }
    fn maybe<T: Copy>(rng: &mut NonCryptoRng, items: &[T]) -> Option<T> {
        (rng.next_below(4) != 0).then(|| any(rng, items))
    }
    fn byte(rng: &mut NonCryptoRng) -> u8 {
        u8::try_from(rng.next_below(256)).expect("a byte")
    }
    let mut spec = reference::spec(any(rng, &Species::ALL));
    spec.build.height = Setting(byte(rng));
    spec.build.girth = Setting(byte(rng));
    spec.build.taper = Setting(byte(rng));
    spec.build.limbs = Setting(byte(rng));
    spec.build.head = Setting(byte(rng));
    spec.features.face = any(rng, FaceShape::ALL);
    spec.features.eyes = any(rng, EyeShape::ALL);
    spec.features.ears = any(rng, EarForm::ALL);
    spec.features.horns = maybe(rng, HornForm::ALL);
    spec.features.tail = maybe(rng, TailForm::ALL);
    spec.features.hair = maybe(rng, HairStyle::ALL);
    spec.features.volume = Setting(byte(rng));
    spec.palette.skin = byte(rng);
    spec.palette.hair = byte(rng);
    spec.palette.eyes = byte(rng);
    spec.palette.markings = byte(rng);
    spec.palette.accent = byte(rng);
    spec
}

/// Opening again on what a store holds is the whole of undoing a refused
/// write.
#[test]
fn opening_again_on_the_stored_record_undoes_what_was_refused() {
    let stored = reference::identity(Species::Human).expect("a real record");
    let mut designer = Designer::open(stored);
    designer.edit(Edit::Girth(Setting::HIGH)).expect("in range");
    assert!(designer.settle().is_some());
    let mut designer = Designer::open(stored);
    assert_eq!(designer.live(), stored);
    assert_eq!(designer.settle(), None);
}
