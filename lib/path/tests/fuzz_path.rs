//! Deterministic fuzz harness for the `lib/path` parser (its untrusted
//! path-string decoder).
//!
//! [`tairix_path::parse`] turns a string supplied by a user, a script, or a
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
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` draws
//! pseudo-random path strings and mutates real path templates. A plain
//! `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged
//! seed; `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the
//! PRNG loop to a wall-clock budget.

use tairix_fuzzseed::Prng;
use tairix_path::{parse, Root, MAX_ALIAS_LEN, MAX_COMPONENTS, MAX_COMPONENT_LEN};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string turned into a path string.
const MAX_NOISE: usize = 512;

/// Real path templates the harness mutates: each exercises a different parse
/// path (view, alias shorthand, expanded alias, relative, `.`/`..`
/// normalisation, unsupported resolvers, resource-reference shapes, and
/// deliberately malformed forms).
const TEMPLATES: &[&str] = &[
    "/System/Kernel/tairix.rxe",
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
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The seed is drawn and logged by `tairix_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "parse_never_panics_and_round_trips_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. A real template with a handful of bytes flipped at random.
        let template = *rng.pick(TEMPLATES);
        let mut mutated: Vec<u8> = template.as_bytes().to_vec();
        let flips = rng.at_most(6);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = rng.below(mutated.len());
            mutated[pos] ^= rng.next_u8();
        }
        exercise(&String::from_utf8_lossy(&mutated));

        // 2. A structured-but-hostile string: delimiters spliced with random
        //    bytes, exercising the root-delimiter and component splitter.
        let blob_len = rng.at_most(48);
        let mut spliced = String::new();
        for _ in 0..blob_len {
            let pick = rng.at_most(6);
            match pick {
                0 => spliced.push('/'),
                1 => spliced.push(':'),
                2 => spliced.push('.'),
                3 => spliced.push_str(".."),
                4 => spliced.push_str("alias::"),
                _ => spliced.push(char::from(b'a' + rng.next_u8() % 26)),
            }
        }
        exercise(&spliced);

        // 3. Pure noise (lossy UTF-8).
        let nlen = rng.at_most(MAX_NOISE);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        exercise(&String::from_utf8_lossy(&noise));

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
