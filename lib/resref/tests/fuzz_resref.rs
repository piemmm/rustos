//! Deterministic fuzz harness for the `lib/resref` parser (its untrusted
//! resource-reference decoder).
//!
//! [`tairix_resref::parse`] turns a string supplied by a user, a script, or a
//! stored record — untrusted input — into a typed `ResourceRef`. The harness's
//! invariants are:
//!
//! * parsing any byte string (as UTF-8) never panics — it returns a
//!   `ResourceRef` or a typed `RefError` (fail closed);
//! * a `ResourceRef` that parses always renders (`Display`) and re-parses to an
//!   equal value (the parser and its canonical spelling round-trip);
//! * a parsed reference never exceeds the fixed security bounds.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG draws
//! pseudo-random reference strings and mutates real templates. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged
//! seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the
//! PRNG loop to a wall-clock budget.

use tairix_resref::{
    parse, MAX_FACET_LEN, MAX_GUARD_LEN, MAX_NAMESPACE_LEN, MAX_PARAMS, MAX_PARAM_KEY_LEN,
    MAX_PARAM_VALUE_LEN, MAX_SEGMENT_LEN, MAX_SELECTOR_SEGMENTS,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string turned into a reference string.
const MAX_NOISE: usize = 512;

/// Real reference templates the harness mutates: each exercises a different
/// parse path (simple, multi-segment, guard, facet, guard+facet, query,
/// query-only, direct-identity shorthand, full-identity, and malformed forms).
const TEMPLATES: &[&str] = &[
    "sys:random",
    "info:cpu/vendor",
    "disk:backup@7K2M::raw",
    "disk:slot/front-usb@P91Q::raw",
    "stats:net/wan/rx.pps?window=1s",
    "disk:?removable=true,size>=16GiB",
    "disk:@7K2M",
    "disk:id/serial/S6XYZ123456789",
    "vol:home",
    "tty:debug",
    "disk:",
    "::raw",
    "?window=1s",
];

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Parse `input` (must not panic) and, when it parses, check the structural
/// invariants and the `Display`/re-parse round-trip.
fn exercise(input: &str) {
    let Ok(reference) = parse(input) else {
        return;
    };

    // Bounds hold on the parsed reference.
    assert!(reference.namespace().as_str().len() <= MAX_NAMESPACE_LEN);
    assert!(reference.selector().len() <= MAX_SELECTOR_SEGMENTS);
    for segment in reference.selector() {
        assert!(!segment.is_empty());
        assert!(segment.len() <= MAX_SEGMENT_LEN);
    }
    if let Some(guard) = reference.guard() {
        assert!(!guard.is_empty());
        assert!(guard.len() <= MAX_GUARD_LEN);
    }
    if let Some(facet) = reference.facet() {
        assert!(!facet.is_empty());
        assert!(facet.len() <= MAX_FACET_LEN);
    }
    assert!(reference.params().len() <= MAX_PARAMS);
    for param in reference.params() {
        assert!(!param.key().is_empty());
        assert!(param.key().len() <= MAX_PARAM_KEY_LEN);
        assert!(param.value().len() <= MAX_PARAM_VALUE_LEN);
    }

    // A parsed reference names something: an empty selector implies a guard or
    // a query stood in for it.
    if reference.selector().is_empty() {
        assert!(reference.guard().is_some() || !reference.params().is_empty());
    }

    // The canonical spelling re-parses to an equal reference (idempotent).
    let rendered = reference.to_string();
    let reparsed = parse(&rendered).expect("canonical spelling must re-parse");
    assert_eq!(reference, reparsed);
    assert_eq!(rendered, reparsed.to_string());
}

#[test]
fn parse_never_panics_and_round_trips_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `tairix_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut state: u64 = tairix_fuzzseed::start(
        "parse_never_panics_and_round_trips_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. A real template with a handful of bytes flipped at random.
        let template = TEMPLATES[bounded(next(), TEMPLATES.len() - 1)];
        let mut mutated: Vec<u8> = template.as_bytes().to_vec();
        let flips = bounded(next(), 6);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        exercise(&String::from_utf8_lossy(&mutated));

        // 2. A structured-but-hostile string: delimiters spliced with random
        //    bytes, exercising the namespace/selector/guard/facet/param split.
        let blob_len = bounded(next(), 48);
        let mut spliced = String::new();
        for _ in 0..blob_len {
            let pick = bounded(next(), 8);
            match pick {
                0 => spliced.push(':'),
                1 => spliced.push('/'),
                2 => spliced.push('@'),
                3 => spliced.push_str("::"),
                4 => spliced.push('?'),
                5 => spliced.push(','),
                6 => spliced.push_str(">="),
                _ => spliced.push(char::from(b'a' + low_byte(next() >> 29) % 26)),
            }
        }
        exercise(&spliced);

        // 3. Pure noise (lossy UTF-8).
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 23)).collect();
        exercise(&String::from_utf8_lossy(&noise));

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
