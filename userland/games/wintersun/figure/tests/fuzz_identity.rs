//! Deterministic fuzz harness for the figure record's decoder.
//!
//! A character's record arrives from a client assumed hostile and is stored
//! one per character, so `Identity::decode` is the figure crate's one
//! attacker-reachable entry point. The invariants are:
//!
//! * decoding any byte string never panics — it yields a record or a typed
//!   refusal;
//! * a record that decodes re-encodes to the *same bytes* and re-validates
//!   equal, so there is exactly one spelling of any figure;
//! * a record that decodes stands inside its species' documented ranges;
//! * a record that decodes **builds and places a figure** — so the server's
//!   check and the renderer can never disagree about what is drawable, and
//!   no admitted record is one the builder then refuses.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates
//! real records and draws pseudo-random bytes. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a
//! wall-clock budget.

use tairix_fuzzseed::Prng;
use tairix_wintersun_figure::humanoid::{self, BODY_PARTS, MOST_PARTS};
use tairix_wintersun_figure::identity::{Identity, RECORD_LEN, RECORD_VERSION};
use tairix_wintersun_figure::pose::Pose;
use tairix_wintersun_figure::reference::Reference;
use tairix_wintersun_figure::rig::{Placement, Stance};
use tairix_wintersun_net::value::Facing;

mod corpus;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Decode `bytes` (must not panic) and, when it decodes, hold the record to
/// every invariant above.
fn exercise(bytes: &[u8], placement: &mut Placement) {
    let Ok(identity) = Identity::decode(bytes) else {
        return;
    };
    assert_eq!(
        identity.encode().as_slice(),
        bytes,
        "re-encoding must reproduce the input"
    );
    assert_eq!(Identity::new(identity.spec()), Ok(identity));

    let ranges = identity.species().ranges();
    let made = identity.proportions();
    for (value, bounds) in [
        (made.height, ranges.height),
        (made.girth, ranges.girth),
        (made.taper, ranges.taper),
        (made.limbs, ranges.limbs),
        (made.head, ranges.head),
    ] {
        assert!(value >= bounds.low() && value <= bounds.high());
    }

    let rig = humanoid::rig(&identity).expect("an admitted record builds");
    assert!((BODY_PARTS..=MOST_PARTS).contains(&rig.parts().len()));
    let rigging = humanoid::rigging(&rig).expect("it binds");
    let light = Reference::light().expect("a real light");
    let stance = Stance::new(Facing(0x6000), 1.0, (40.0, 90.0), light).expect("a real stance");
    rigging
        .posture(&Pose::REST)
        .expect("rest is posturable")
        .place(&stance, &[], placement)
        .expect("an admitted record places");
    assert_eq!(placement.len(), rig.parts().len());
}

/// One round of mutation against the corpus.
fn mutate_round(rng: &mut Prng, templates: &[Vec<u8>], placement: &mut Placement) {
    let template = rng.pick(templates);

    // A real record with a handful of bytes changed: hammers every field's
    // range, the species' admissible sets and the canonical rules.
    let mut mutated = template.clone();
    for _ in 0..rng.at_most(4) {
        let at = rng.below(mutated.len());
        mutated[at] = rng.next_u8();
    }
    exercise(&mutated, placement);

    // A real record with one byte stepped to a neighbouring value, which is
    // where an off-by-one in a bound lives.
    let mut stepped = template.clone();
    let at = rng.below(stepped.len());
    let step = 1 + u8::try_from(rng.at_most(1)).unwrap_or(0);
    stepped[at] = if rng.at_most(1) == 0 {
        stepped[at].wrapping_add(step)
    } else {
        stepped[at].wrapping_sub(step)
    };
    exercise(&stepped, placement);

    // A truncation, and trailing bytes no encoder produces.
    exercise(&template[..rng.at_most(template.len())], placement);
    let mut extended = template.clone();
    let extra = 1 + rng.at_most(8);
    extended.extend(corpus::blob(rng, extra));
    exercise(&extended, placement);
}

/// A record of the right length and version whose body is noise: the shape
/// that drives every field check at once.
fn forged_round(rng: &mut Prng, placement: &mut Placement) {
    let mut forged = corpus::blob(rng, RECORD_LEN);
    forged[0] = RECORD_VERSION;
    exercise(&forged, placement);

    let noise_len = rng.at_most(2 * RECORD_LEN);
    let noise = corpus::blob(rng, noise_len);
    exercise(&noise, placement);
}

#[test]
fn decoding_any_bytes_never_panics_and_every_admitted_record_builds() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    // The seed is drawn and logged by `tairix_fuzzseed`: fresh per run,
    // reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut rng =
        corpus::seeded("decoding_any_bytes_never_panics_and_every_admitted_record_builds");
    let mut placement = Placement::new();
    let templates = corpus::records();

    // Every committed corpus record is replayed first, so a crash once
    // found is re-checked on the bytes it was filed under before any new
    // input.
    for record in &templates {
        exercise(record, &mut placement);
    }

    let mut iteration: u64 = 0;
    loop {
        mutate_round(&mut rng, &templates, &mut placement);
        forged_round(&mut rng, &mut placement);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
