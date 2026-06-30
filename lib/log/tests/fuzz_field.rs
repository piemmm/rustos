//! Deterministic fuzz-style integration test for the log value decoder.
//!
//! [`rustos_log::FieldValue::decode`] accepts an arbitrary byte slice (a log
//! segment a verifier or renderer reads is attacker-influenced once the
//! journal ingests untrusted caller fields), so the right way to drive it is a
//! fuzz harness. This file is the smoke harness that runs in `cargo test`: a
//! deterministic PRNG generates short pseudo-random inputs and asserts the
//! decoder refuses them cleanly without panicking, and that any value it does
//! accept re-encodes to exactly the bytes it consumed and decodes back equal.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition). A plain `cargo test`
//! runs the fixed [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed;
//! `cargo xtask fuzz --soak` sets the budget and keeps drawing from the same
//! continuing stream until the deadline.

use rustos_log::FieldValue;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Drive the value decoder on `bytes`.
///
/// The contract is "must not panic for any input"; a value the decoder accepts
/// must additionally re-encode to exactly the bytes it consumed, decode back
/// to an equal value, and (for a list) iterate exactly its reported length.
fn exercise(bytes: &[u8]) {
    let Ok((value, consumed)) = FieldValue::decode(bytes) else {
        return;
    };
    assert!(
        consumed <= bytes.len(),
        "a decode cannot consume past its input"
    );

    let mut buf = [0u8; 512];
    let written = value
        .encode(&mut buf)
        .expect("an accepted value must re-encode");
    assert_eq!(
        written, consumed,
        "the canonical re-encoding must match what decode consumed"
    );
    let (redecoded, reconsumed) =
        FieldValue::decode(&buf[..written]).expect("round-trip of an accepted value must succeed");
    assert_eq!(reconsumed, written);
    assert_eq!(
        redecoded, value,
        "an accepted value must survive a round trip"
    );

    if let FieldValue::List(list) = value {
        let counted = list.iter().count();
        assert_eq!(counted, list.len(), "iteration must yield every element");
    }
}

#[test]
fn random_short_inputs_never_panic() {
    let mut rng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "random_short_inputs_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 256];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0xFFFF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
