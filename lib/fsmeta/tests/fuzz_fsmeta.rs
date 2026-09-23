//! Deterministic fuzz harness for the `lib/fsmeta` untrusted-input paths:
//! the namespaced-key grammar parser and the attribute-set decoder.
//!
//! Attribute keys and encoded attribute sets arrive from outside TAIRiX's
//! trust boundary — copied in from a foreign filesystem, an archive, or a raw
//! block that may be corrupt or hostile. A malformed key, a truncated or
//! over-count encoding, an out-of-bounds length field, a duplicate key, or an
//! unknown flag bit must all be **rejected** fail-closed, never trusted and
//! never a panic. The single invariant driven here:
//!
//! * feeding any byte image to [`tairix_fsmeta::AttrKey::parse`] and
//!   [`tairix_fsmeta::AttrSet::decode`] never panics and never reads out of
//!   bounds — each returns a validated value or a
//!   [`tairix_fsmeta::MetadataError`]. Any value the decoder *accepts* must
//!   re-encode and re-decode to the identical set (round-trip stability). The
//!   run aborting *is* the failure.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates a
//! valid seed encoding and feeds pure noise. A plain `cargo test` runs the
//! fixed smoke sweep; `cargo xtask fuzz` extends the loop to a wall-clock
//! budget.

use tairix_fsmeta::{AttrFlags, AttrKey, AttrSet};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// A well-formed encoded attribute set as a mutation seed.
fn seed_encoding() -> Vec<u8> {
    let mut set = AttrSet::new();
    set.set(b"user.comment", AttrFlags::NO_BACKUP, b"hello world")
        .expect("seed set");
    set.set(b"acorn.filetype", AttrFlags::empty(), b"fff")
        .expect("seed set");
    set.set(b"mac.type", AttrFlags::empty(), b"TEXT")
        .expect("seed set");
    set.encode()
}

/// Decode `bytes`; if it is accepted, the re-encode must decode to the same
/// set. Must never panic, whatever the image.
fn exercise_decode(bytes: &[u8]) {
    if let Ok(set) = AttrSet::decode(bytes) {
        let round = AttrSet::decode(&set.encode()).expect("re-decode of accepted set");
        assert_eq!(round, set, "attribute-set decode is not round-trip stable");
    }
}

#[test]
fn parsing_any_metadata_never_panics() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let seed = seed_encoding();

    let mut rng = Prng::new(tairix_fuzzseed::start(
        "parsing_any_metadata_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. The valid encoding with a handful of bytes flipped at random.
        let mut mutated = seed.clone();
        let flips = rng.at_most(24);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = rng.below(mutated.len());
            mutated[pos] ^= rng.next_u8();
        }
        exercise_decode(&mutated);

        // 2. A truncation of the valid encoding, driving the bounds checks.
        let keep = rng.at_most(seed.len());
        exercise_decode(&seed[..keep]);

        // 3. Pure noise of an arbitrary length through the decoder.
        let nlen = rng.at_most(5000);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        exercise_decode(&noise);

        // 4. Arbitrary bytes as a key, driving the grammar validator.
        let klen = rng.at_most(300);
        let mut key = vec![0u8; klen];
        rng.fill(&mut key);
        let _ = AttrKey::parse(&key);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
