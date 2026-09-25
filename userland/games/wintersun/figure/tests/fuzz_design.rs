//! Deterministic fuzz harness for the figure designer.
//!
//! Every record a player authors passes through the designer before a server
//! re-validates it, so a designer that could ever hold a record the decoder
//! refuses is one whose work could not be saved. The invariants:
//!
//! * from any admitted record, any sequence of edits — admissible or not —
//!   leaves the designer holding a record the decoder admits and reads back;
//! * an edit that is taken shows exactly the value it set and moves no field
//!   it does not reshape (a species its forms and swatches, going bald the
//!   hair's colour and volume), and an edit that is refused changes nothing;
//! * an interaction settles into at most one write, of the record shown, and
//!   only when the store does not hold it already;
//! * an answer or a refusal landing during the next drag leaves the fields
//!   that drag set as they are, and puts every other field where the store's
//!   record has it; a settle made meanwhile is owed, and the answer hands it
//!   out;
//! * a round trip through another species gives back the record it left;
//! * a plausible figure drawn from any seed is a record that builds.
//!
//! As `fuzz_identity`, a plain `cargo test` runs [`SMOKE_ITERATIONS`] from a
//! fresh, logged seed, and `cargo xtask fuzz` extends the loop to its
//! wall-clock budget.

use tairix_fuzzseed::Prng;
use tairix_rng::NonCryptoRng;
use tairix_wintersun_figure::design::{Designer, Edit};
use tairix_wintersun_figure::humanoid;
use tairix_wintersun_figure::identity::{
    EarForm, EyeShape, FaceShape, Field, HairStyle, HornForm, Identity, Setting, TailForm,
};
use tairix_wintersun_figure::plausible;
use tairix_wintersun_figure::species::Species;

mod corpus;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 4_000;

/// How many edits one round makes before it settles.
const EDITS: usize = 24;

/// A swatch byte: usually inside the widest table, so most are admitted,
/// and otherwise anything at all.
fn swatch(rng: &mut Prng) -> u8 {
    if rng.below(4) == 0 {
        rng.next_u8()
    } else {
        u8::try_from(rng.below(16)).unwrap_or(0)
    }
}

fn maybe<T: Copy>(rng: &mut Prng, forms: &[T]) -> Option<T> {
    (rng.below(4) != 0).then(|| *rng.pick(forms))
}

/// Any edit a designer can be asked for.
fn edit(rng: &mut Prng) -> Edit {
    let setting = Setting(rng.next_u8());
    match rng.below(18) {
        0 => Edit::Species(*rng.pick(&Species::ALL)),
        1 => Edit::Height(setting),
        2 => Edit::Girth(setting),
        3 => Edit::Taper(setting),
        4 => Edit::Limbs(setting),
        5 => Edit::Head(setting),
        6 => Edit::Face(*rng.pick(FaceShape::ALL)),
        7 => Edit::EyeShape(*rng.pick(EyeShape::ALL)),
        8 => Edit::Ears(*rng.pick(EarForm::ALL)),
        9 => Edit::Horns(maybe(rng, HornForm::ALL)),
        10 => Edit::Tail(maybe(rng, TailForm::ALL)),
        11 => Edit::Hair(maybe(rng, HairStyle::ALL)),
        12 => Edit::Volume(setting),
        13 => Edit::Skin(swatch(rng)),
        14 => Edit::HairColour(swatch(rng)),
        15 => Edit::EyeColour(swatch(rng)),
        16 => Edit::Markings(swatch(rng)),
        _ => Edit::Accent(swatch(rng)),
    }
}

/// A build setting, which no record refuses and nothing re-derives: the
/// drag an answer lands in the middle of.
fn build(rng: &mut Prng) -> Edit {
    let setting = Setting(rng.next_u8());
    match rng.below(5) {
        0 => Edit::Height(setting),
        1 => Edit::Girth(setting),
        2 => Edit::Taper(setting),
        3 => Edit::Limbs(setting),
        _ => Edit::Head(setting),
    }
}

/// Whether `edit` re-derives `field` around itself.
fn reshaped(edit: Edit, field: Field) -> bool {
    match edit {
        Edit::Species(_) => matches!(
            field,
            Field::Ears
                | Field::Horns
                | Field::Tail
                | Field::SkinColour
                | Field::EyeColour
                | Field::MarkingsColour
        ),
        Edit::Hair(_) => matches!(field, Field::Volume | Field::HairColour),
        _ => false,
    }
}

/// Make `edit` and hold the designer to every invariant above.
fn exercise(designer: &mut Designer, edit: Edit) {
    let before = *designer;
    let was = before.live().spec();
    if designer.edit(edit).is_ok() {
        let live = designer.live();
        assert_eq!(Identity::decode(&live.encode()), Ok(live));
        let now = live.spec();
        assert_eq!(
            Edit::of(edit.field(), now),
            edit,
            "{edit:?} did not show as set"
        );
        for field in Field::ALL {
            if field != edit.field() && !reshaped(edit, field) {
                assert_eq!(
                    Edit::of(field, now),
                    Edit::of(field, was),
                    "{edit:?} moved {field:?}"
                );
            }
        }
    } else {
        assert!(
            !matches!(edit, Edit::Species(_) | Edit::Hair(_)),
            "{edit:?} was refused"
        );
        assert_eq!(*designer, before, "a refused {edit:?} changed the designer");
    }
}

/// Settle, drag the build while the write is out, and answer it, holding the
/// answer to the invariants above; `stored` is the record the store holds.
fn settle_and_answer(designer: &mut Designer, stored: &mut Identity, rng: &mut Prng) {
    let changed = designer.live() != *stored;
    let Some(written) = designer.settle() else {
        assert!(!changed, "a changed record settled into no write");
        return;
    };
    assert!(changed, "a record the store holds was written again");
    assert_eq!(
        written,
        designer.live(),
        "the record written is not the one shown"
    );

    let mut dragged = Vec::new();
    for _ in 0..rng.below(4) {
        let edit = build(rng);
        designer
            .edit(edit)
            .expect("a build setting is never refused");
        if !dragged.contains(&edit.field()) {
            dragged.push(edit.field());
        }
    }
    let owed = rng.below(2) == 0;
    if owed {
        assert_eq!(
            designer.settle(),
            None,
            "a second write went out while one was"
        );
    }

    let held = designer.live().spec();
    let answer = if rng.below(2) == 0 {
        *stored = written;
        designer.landed(written)
    } else {
        designer.refused()
    };
    let now = designer.live();
    for field in Field::ALL {
        let expected = if dragged.contains(&field) {
            Edit::of(field, held)
        } else {
            Edit::of(field, stored.spec())
        };
        assert_eq!(
            Edit::of(field, now.spec()),
            expected,
            "the answer moved {field:?}"
        );
    }
    let wanted = (owed && now != *stored).then_some(now);
    assert_eq!(
        answer, wanted,
        "the owed write was not what the answer handed out"
    );
    if let Some(owed) = answer {
        assert_eq!(designer.landed(owed), None, "an owed write owed another");
        *stored = owed;
    }
}

/// Write every field the record holds at zero with the zero it holds, which
/// must leave the choices beneath it alone.
fn rewrite_what_is_fixed(probe: &mut Designer) {
    for field in [Field::Volume, Field::HairColour, Field::MarkingsColour] {
        let spec = probe.live().spec();
        if spec.fixed(field) {
            probe
                .edit(Edit::of(field, spec))
                .expect("the zero a record holds is admitted");
        }
    }
}

/// Every round trip from where the designer stands — through each species,
/// and through going bald — gives back the record it left, even with every
/// field the far end holds at zero written on the way.
fn round_trips(designer: &Designer) {
    let home = designer.live().spec();
    for other in Species::ALL {
        let mut probe = *designer;
        probe.edit(Edit::Species(other)).expect("any species");
        rewrite_what_is_fixed(&mut probe);
        probe
            .edit(Edit::Species(home.species))
            .expect("any species");
        assert_eq!(probe.live(), designer.live(), "a trip through {other:?}");
    }
    let mut probe = *designer;
    probe.edit(Edit::Hair(None)).expect("anyone may be bald");
    rewrite_what_is_fixed(&mut probe);
    probe
        .edit(Edit::Hair(home.features.hair))
        .expect("anyone may have hair");
    assert_eq!(probe.live(), designer.live(), "a trip through going bald");
}

#[test]
fn any_edits_keep_a_record_and_settle_into_at_most_one_write() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = corpus::seeded("any_edits_keep_a_record_and_settle_into_at_most_one_write");
    let records: Vec<Identity> = corpus::records()
        .iter()
        .map(|bytes| Identity::decode(bytes).expect("the corpus holds records"))
        .collect();

    let mut iteration: u64 = 0;
    loop {
        let mut stored = *rng.pick(&records);
        let mut designer = Designer::open(stored);
        for _ in 0..EDITS {
            exercise(&mut designer, edit(&mut rng));
            if rng.below(6) == 0 {
                settle_and_answer(&mut designer, &mut stored, &mut rng);
            }
        }
        round_trips(&designer);

        let untouched = designer;
        assert_eq!(designer.landed(stored), None, "an answer with no write out");
        assert_eq!(designer.refused(), None, "a refusal with no write out");
        assert_eq!(
            designer, untouched,
            "an answer to nothing changed the designer"
        );

        let species = *rng.pick(&Species::ALL);
        let drawn = plausible::figure(species, &mut NonCryptoRng::seed_from_u64(rng.next_u64()))
            .expect("every draw is a record");
        assert_eq!(drawn.species(), species);
        assert_eq!(Identity::decode(&drawn.encode()), Ok(drawn));
        humanoid::rig(&drawn).expect("every draw builds");
        designer.apply(drawn);
        assert_eq!(designer.live(), drawn);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
