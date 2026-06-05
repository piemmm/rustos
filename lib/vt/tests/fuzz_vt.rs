//! Deterministic fuzz harness for the `lib/vt` streaming parser
//! (`AGENTS.md` §19.5 / §19.6 — the terminal's untrusted-input decoder).
//!
//! [`rustos_vt::Parser`] consumes bytes a terminal did not produce: local shell
//! output and, in the remote stages of `plans/CURSES.md`, a foreign host's
//! output. Per §19.6 that decode path is driven by a fuzz harness whose single
//! invariant is:
//!
//! * feeding any byte stream never panics and never reads out of bounds — the
//!   parser either emits well-formed [`rustos_vt::Op`] events or silently drops
//!   the bytes it cannot interpret (fail closed, `AGENTS.md` §2.9).
//!
//! RustOS pulls in no external fuzz runner (`AGENTS.md` §2.12): a fixed-seed LCG
//! draws pseudo-random byte strings, mutates real escape-sequence templates, and
//! splices structured-but-hostile sequences together. A plain `cargo test` runs
//! the fixed [`SMOKE_ITERATIONS`] sweep; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock budget.

use rustos_vt::{Op, Parser};

/// Fixed-iteration sweep run by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string fed straight to the parser.
const MAX_NOISE: usize = 4096;

/// Real escape-sequence templates the harness mutates: each exercises a
/// different decode path (SGR colour, cursor motion, private mode, OSC title).
const TEMPLATES: &[&[u8]] = &[
    b"\x1b[1;31;4mhello\x1b[0m",
    b"\x1b[38;2;16;112;240m\x1b[48;5;231mx\x1b[m",
    b"\x1b[?1049h\x1b[?25l\x1b[10;20H\x1b[2J\x1b[?25h\x1b[?1049l",
    b"\x1b]0;a window title\x07",
    b"\x1bP1;2pignored device control\x1b\\done",
    b"\x1b[999999999999A\xff\xfe\xc3\x28text\xe2\x98\x83",
];

/// Deadline for the current run, or `None` for the fixed smoke sweep.
fn budget() -> Option<std::time::Instant> {
    let secs: u64 = std::env::var("RUSTOS_FUZZ_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(std::time::Instant::now() + std::time::Duration::from_secs(secs))
}

fn within_budget(deadline: Option<std::time::Instant>) -> bool {
    matches!(deadline, Some(end) if std::time::Instant::now() < end)
}

/// Initial PRNG seed for this harness. `cargo xtask fuzz` exports
/// `RUSTOS_FUZZ_SEED` so each soak run explores fresh inputs (`AGENTS.md`
/// §19.6 / §2.1); a plain `cargo test` leaves it unset and replays the fixed
/// `salt` for a reproducible smoke sweep. `salt` distinguishes independent
/// PRNG streams within one harness.
fn seed(salt: u64) -> u64 {
    match std::env::var("RUSTOS_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(env) => env ^ salt,
        None => salt,
    }
}

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Feed arbitrary bytes through a fresh parser: must never panic, whatever it
/// emits. Draining the events also exercises the [`Op`] payloads.
fn feed_never_panics(bytes: &[u8]) {
    let mut parser = Parser::new();
    let mut glyphs = 0u64;
    parser.feed(bytes, |op| {
        if let Op::Print(_) = op {
            glyphs = glyphs.wrapping_add(1);
        }
    });
    let _ = glyphs;
}

#[test]
fn feed_never_panics_for_any_input() {
    let deadline = budget();

    // The LCG seed comes from `seed()`: fixed under a plain `cargo test`
    // (reproducible), fresh per soak run under `cargo xtask fuzz`.
    let mut state: u64 = seed(0x9E37_79B9_7F4A_7C15);
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
        let mut mutated = template.to_vec();
        let flips = bounded(next(), 8);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        feed_never_panics(&mutated);

        // 2. A structured-but-hostile stream: a valid CSI introducer, a random
        //    blob, and a final byte, exercising the parameter scanner.
        let blob_len = bounded(next(), 64);
        let mut spliced = Vec::new();
        spliced.extend_from_slice(b"\x1b[");
        for _ in 0..blob_len {
            spliced.push(low_byte(next() >> 23));
        }
        spliced.push(b'm');
        feed_never_panics(&spliced);

        // 3. Pure noise straight into the parser.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        feed_never_panics(&noise);

        iteration += 1;
        if !within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
