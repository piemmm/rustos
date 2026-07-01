//! Deterministic fuzz harness for the `lib/path` parser (its untrusted
//! path-string decoder).
//!
//! [`rustos_path::parse`] turns a string supplied by a user, a script, or a
//! stored record — untrusted input — into a typed `Path`. The harness's
//! invariants are:
//!
//! * parsing any byte string (as UTF-8) never panics — it returns a `Path` or a
//!   typed `PathError` (fail closed);
//! * a `Path` that parses always renders (`Display`) and re-parses to an equal
//!   `Path` (the parser and its canonical spelling round-trip);
//! * a parsed `Path` never exceeds the fixed security bounds, and a rooted
//!   (view/alias) path never retains a `.`/`..` navigation component.
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded LCG draws
//! pseudo-random path strings and mutates real path templates. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged
//! seed; `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS` to extend the
//! PRNG loop to a wall-clock budget.

use rustos_path::{parse, Root, MAX_ALIAS_LEN, MAX_COMPONENTS, MAX_COMPONENT_LEN};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string turned into a path string.
const MAX_NOISE: usize = 512;

/// Real path templates the harness mutates: each exercises a different parse
/// path (view, alias shorthand, expanded alias, relative, `.`/`..`
/// normalisation, unsupported resolvers, resource-reference shapes, and
/// deliberately malformed forms).
const TEMPLATES: &[&str] = &[
    "/System/Kernel/rustos.rxe",
    "Home:/Documents/../Photos/2026",
    "alias::Backup/snapshots/latest",
    "../../notes/todo.txt",
    "a/./b/../c",
    "id::b7f2e4e6-8d7a-4ef8-a13e-d3b84d4e8001/x",
    "sys:random",
    "Home:/a//b",
    ":/orphan",
    "::/resolver",
    "Home:/..",
    "café/фото/文件",
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
    let Ok(path) = parse(input) else {
        return;
    };

    // Bounds hold on the parsed path.
    assert!(path.components().len() <= MAX_COMPONENTS);
    for component in path.components() {
        assert!(component.len() <= MAX_COMPONENT_LEN);
        assert!(!component.is_empty());
    }
    if let Some(alias) = path.alias() {
        assert!(!alias.is_empty());
        assert!(alias.len() <= MAX_ALIAS_LEN);
    }

    // A rooted path has no navigation components left in it.
    if path.is_absolute() {
        for component in path.components() {
            assert_ne!(component, ".");
            assert_ne!(component, "..");
        }
    } else {
        assert_eq!(path.root(), &Root::Relative);
    }

    // The canonical spelling re-parses to an equal path (idempotent Display).
    let rendered = path.to_string();
    let reparsed = parse(&rendered).expect("canonical spelling must re-parse");
    assert_eq!(path, reparsed);
    assert_eq!(rendered, reparsed.to_string());
}

#[test]
fn parse_never_panics_and_round_trips_for_any_input() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `rustos_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `RUSTOS_FUZZ_SEED`.
    let mut state: u64 = rustos_fuzzseed::start(
        "parse_never_panics_and_round_trips_for_any_input",
        rustos_fuzzseed::FUZZ_SEED_ENV,
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
        //    bytes, exercising the root-delimiter and component splitter.
        let blob_len = bounded(next(), 48);
        let mut spliced = String::new();
        for _ in 0..blob_len {
            let pick = bounded(next(), 6);
            match pick {
                0 => spliced.push('/'),
                1 => spliced.push(':'),
                2 => spliced.push('.'),
                3 => spliced.push_str(".."),
                4 => spliced.push_str("alias::"),
                _ => spliced.push(char::from(b'a' + low_byte(next() >> 29) % 26)),
            }
        }
        exercise(&spliced);

        // 3. Pure noise (lossy UTF-8).
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 23)).collect();
        exercise(&String::from_utf8_lossy(&noise));

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
