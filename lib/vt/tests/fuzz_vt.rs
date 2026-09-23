//! Deterministic fuzz harness for the `lib/vt` streaming parser
//! (the terminal's untrusted-input decoder).
//!
//! [`tairix_vt::Parser`] consumes bytes a terminal did not produce: local shell
//! output and, in the remote stages of `plans/CURSES.md`, a foreign host's
//! output. Per that decode path is driven by a fuzz harness whose single
//! invariant is:
//!
//! * feeding any byte stream never panics and never reads out of bounds — the
//!   parser either emits well-formed [`tairix_vt::Op`] events or silently drops
//!   the bytes it cannot interpret (fail closed).
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng`
//! draws pseudo-random byte strings, mutates real escape-sequence templates, and
//! splices structured-but-hostile sequences together. A plain `cargo test` runs
//! the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz --soak` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock budget.

use tairix_fuzzseed::Prng;
use tairix_vt::{Op, Parser};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
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
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The seed is drawn and logged by `tairix_fuzzseed::start`: fresh
    // per run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "feed_never_panics_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. A real template with a handful of bytes flipped at random.
        let template = *rng.pick(TEMPLATES);
        let mut mutated = template.to_vec();
        let flips = rng.at_most(8);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = rng.below(mutated.len());
            mutated[pos] ^= rng.next_u8();
        }
        feed_never_panics(&mutated);

        // 2. A structured-but-hostile stream: a valid CSI introducer, a random
        //    blob, and a final byte, exercising the parameter scanner.
        let blob_len = rng.at_most(64);
        let mut spliced = Vec::new();
        spliced.extend_from_slice(b"\x1b[");
        for _ in 0..blob_len {
            spliced.push(rng.next_u8());
        }
        spliced.push(b'm');
        feed_never_panics(&spliced);

        // 3. Pure noise straight into the parser.
        let nlen = rng.at_most(MAX_NOISE);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        feed_never_panics(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
