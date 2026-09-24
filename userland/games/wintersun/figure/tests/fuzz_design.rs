//! Deterministic fuzz harness for the figure designer.
//!
//! Every record a player authors passes through the designer before a server
//! re-validates it, so a designer that could ever hold a record the decoder
//! refuses is one whose work could not be saved. The invariants:
//!
//! * from any admitted record, any sequence of edits — admissible or not —
//!   leaves the designer holding a record the decoder admits and reads back;
//! * an edit that is taken shows exactly the value it set and changes no
//!   other field, unless it reshapes the record (a species, going bald), and
//!   an edit that is refused changes nothing;
//! * an interaction settles into at most one write, and only when it
//!   changed the record;
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
    EarForm, EyeShape, FaceShape, HairStyle, HornForm, Identity, Setting, Spec, TailForm,
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

/// `spec` with `edit`'s field set, or `None` for an edit that reshapes the
/// record's other fields as well: the oracle a taken edit is held to.
fn written(edit: Edit, spec: Spec) -> Option<Spec> {
    let mut spec = spec;
    match edit {
        Edit::Species(_) | Edit::Hair(_) => return None,
        Edit::Height(setting) => spec.build.height = setting,
        Edit::Girth(setting) => spec.build.girth = setting,
        Edit::Taper(setting) => spec.build.taper = setting,
        Edit::Limbs(setting) => spec.build.limbs = setting,
        Edit::Head(setting) => spec.build.head = setting,
        Edit::Face(face) => spec.features.face = face,
        Edit::EyeShape(eyes) => spec.features.eyes = eyes,
        Edit::Ears(ears) => spec.features.ears = ears,
        Edit::Horns(horns) => spec.features.horns = horns,
        Edit::Tail(tail) => spec.features.tail = tail,
        Edit::Volume(volume) => spec.features.volume = volume,
        Edit::Skin(swatch) => spec.palette.skin = swatch,
        Edit::HairColour(swatch) => spec.palette.hair = swatch,
        Edit::EyeColour(swatch) => spec.palette.eyes = swatch,
        Edit::Markings(swatch) => spec.palette.markings = swatch,
        Edit::Accent(swatch) => spec.palette.accent = swatch,
    }
    Some(spec)
}

/// Make `edit` and hold the designer to every invariant above.
fn exercise(designer: &mut Designer, edit: Edit) {
    let before = *designer;
    if designer.edit(edit).is_ok() {
        let live = designer.live();
        assert_eq!(Identity::decode(&live.encode()), Ok(live));
        if let Some(expected) = written(edit, before.live().spec()) {
            assert_eq!(live.spec(), expected, "{edit:?} did not show as set");
        }
    } else {
        assert!(
            !matches!(edit, Edit::Species(_) | Edit::Hair(_)),
            "{edit:?} was refused"
        );
        assert_eq!(*designer, before, "a refused {edit:?} changed the designer");
    }
}

/// Every species' round trip from where the designer stands gives back the
/// record it left.
fn round_trips(designer: &Designer) {
    let home = designer.live().species();
    for other in Species::ALL {
        let mut probe = *designer;
        probe.edit(Edit::Species(other)).expect("any species");
        probe.edit(Edit::Species(home)).expect("any species");
        assert_eq!(probe.live(), designer.live(), "a trip through {other:?}");
    }
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
        let mut designer = Designer::open(*rng.pick(&records));
        let mut settled = designer.live();
        for _ in 0..EDITS {
            exercise(&mut designer, edit(&mut rng));
            if rng.below(6) == 0 {
                let changed = designer.live() != settled;
                let write = designer.settle();
                assert_eq!(write.is_some(), changed);
                assert_eq!(designer.settle(), None, "one interaction wrote twice");
                settled = designer.live();
            }
        }
        round_trips(&designer);

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
