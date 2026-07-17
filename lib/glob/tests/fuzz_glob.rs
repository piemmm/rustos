//! Deterministic fuzz harness for the `lib/glob` matcher (its untrusted-pattern
//! decoder and match loop).
//!
//! [`tairix_glob::Pattern::new`] compiles a pattern supplied by a user or a
//! script — untrusted input — and [`tairix_glob::Pattern::matches`] runs the
//! compiled pattern against a candidate. The harness's invariants are:
//!
//! * compiling any byte string (as UTF-8) never panics — it returns a
//!   `Pattern` or a typed `GlobError` (fail closed);
//! * matching any compiled pattern against any candidate never panics and
//!   always terminates (the algorithm is backtracking-free, so a hostile
//!   pattern or candidate cannot trigger runaway work).
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG draws
//! pseudo-random pattern strings, mutates real glob templates, and matches them
//! against mutated candidates. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask fuzz`
//! exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock
//! budget.

use tairix_glob::Pattern;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string turned into a pattern or candidate.
const MAX_NOISE: usize = 512;

/// Real glob templates the harness mutates: each exercises a different
/// compile/match path (stars, question marks, ranges, negation, escaping,
/// literal brackets, and deliberately malformed forms).
const TEMPLATES: &[&str] = &[
    "*.rs",
    "img_[0-9][0-9].???",
    "[!a-z]*[A-Z]",
    r"a\*b\?c",
    "[]-]*[^]]",
    "**/*.tar.gz",
    "[abc",
    "[z-a]",
    r"trailing\",
    "?*?*?*?*?*",
];

/// Candidate strings compiled patterns are matched against.
const CANDIDATES: &[&str] = &[
    "",
    "lib.rs",
    "img_04.png",
    "café/photo_99.jpeg",
    "a/b/c/d.tar.gz",
    "]-]",
    "ZZZ",
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

/// Compile `pattern` (must not panic) and, when it compiles, match it against
/// every fixed candidate plus `extra` (must not panic or hang).
fn exercise(pattern: &str, extra: &str) {
    if let Ok(pat) = Pattern::new(pattern) {
        for cand in CANDIDATES {
            let _ = pat.matches(cand);
        }
        let _ = pat.matches(extra);
    }
}

#[test]
fn compile_and_match_never_panic_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `tairix_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut state: u64 = tairix_fuzzseed::start(
        "compile_and_match_never_panic_for_any_input",
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
        let candidate = CANDIDATES[bounded(next(), CANDIDATES.len() - 1)];
        // Lossy UTF-8 keeps the pattern a `&str` while still feeding it the
        // byte damage the flips introduced.
        exercise(&String::from_utf8_lossy(&mutated), candidate);

        // 2. A structured-but-hostile pattern: metacharacters spliced with a
        //    random blob, exercising the tokeniser and bracket parser.
        let blob_len = bounded(next(), 48);
        let mut spliced = String::new();
        for _ in 0..blob_len {
            let pick = bounded(next(), 7);
            match pick {
                0 => spliced.push('*'),
                1 => spliced.push('?'),
                2 => spliced.push('['),
                3 => spliced.push(']'),
                4 => spliced.push('-'),
                5 => spliced.push('\\'),
                6 => spliced.push('!'),
                _ => spliced.push(char::from(b'a' + low_byte(next() >> 29) % 26)),
            }
        }
        exercise(&spliced, candidate);

        // 3. Pure noise as the pattern (lossy UTF-8), and as the candidate.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 23)).collect();
        let noise_str = String::from_utf8_lossy(&noise);
        exercise(&noise_str, &noise_str);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
