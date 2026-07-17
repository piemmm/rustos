//! Deterministic fuzz-style integration test for the segment-local string
//! dictionary codec.
//!
//! A segment's records are attacker-influenced (a compromised journal, a
//! tampered or torn file, a volume lifted from another machine), so the
//! reader-side [`tairix_log::DictionaryView`] must refuse malformed
//! dictionary-coded bytes cleanly and never panic. This harness drives it two
//! ways:
//!
//! * on pseudo-random bytes, decoding a stream of coded strings until the
//!   first refusal — the contract is "must not panic"; and
//! * as a builder → view round-trip over random sequences drawn from a tiny
//!   alphabet (so strings repeat and the promote-on-repeat / handle-reference
//!   paths are actually reached), asserting every string decodes back to what
//!   was encoded and the reader consumes exactly the writer's bytes.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `tairix_fuzzseed` seam (one definition).

use tairix_log::{DictionaryBuilder, DictionaryView};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

/// A generous per-field bound; the codec's own `MAX_DICT_STRING` cap is the
/// interning limit under test.
const MAX: usize = 256;

/// Decode coded strings from arbitrary bytes until refusal; must never panic.
fn exercise_random(bytes: &[u8]) {
    let mut view = DictionaryView::new();
    let mut pos = 0usize;
    // Bound the loop by the input length: every successful decode consumes at
    // least one byte, so this always terminates.
    for _ in 0..=bytes.len() {
        match view.decode_str(bytes, &mut pos, MAX) {
            Ok(s) => assert!(s.len() <= MAX),
            Err(_) => break,
        }
    }
}

/// Encode a random sequence of small-alphabet strings, then decode it back.
fn exercise_round_trip(rng: &mut tairix_fuzzseed::Lcg) {
    let count = (rng.next_u64() % 40) as usize;
    let mut strings: Vec<String> = Vec::with_capacity(count);
    for _ in 0..count {
        // Length 0..8, drawn from a 4-symbol alphabet so repeats are frequent.
        let len = (rng.next_u64() % 9) as usize;
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            let sym = b"abc."[(rng.next_u64() % 4) as usize];
            s.push(sym as char);
        }
        strings.push(s);
    }

    let mut buf = [0u8; 8192];
    let mut builder = DictionaryBuilder::new();
    let mut wpos = 0usize;
    for s in &strings {
        builder
            .encode_str(&mut buf, &mut wpos, s, MAX)
            .expect("small strings always fit the buffer");
    }

    let mut view = DictionaryView::new();
    let mut rpos = 0usize;
    for s in &strings {
        let got = view
            .decode_str(&buf[..wpos], &mut rpos, MAX)
            .expect("what the builder wrote must decode");
        assert_eq!(got, s.as_str(), "round-trip mismatch");
    }
    assert_eq!(rpos, wpos, "reader consumes exactly the writer's bytes");
}

#[test]
fn random_and_round_trip_dictionary_never_panic() {
    let mut rng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "random_and_round_trip_dictionary_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut buf = [0u8; 512];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for i in 0..SMOKE_ITERATIONS {
            if i % 2 == 0 {
                let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                exercise_random(&buf[..size]);
            } else {
                exercise_round_trip(&mut rng);
            }
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
