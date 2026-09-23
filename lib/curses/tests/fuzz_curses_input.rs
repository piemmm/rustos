//! Deterministic fuzz harness for the `lib/curses` input decoder
//! (the curses application's untrusted-input path).
//!
//! [`tairix_curses::Input`] turns terminal bytes — local keystrokes and, in the
//! remote stages of `plans/CURSES.md`, a foreign host's reported keys, mouse
//! events, and pastes — into typed [`tairix_curses::Event`]s, over the one
//! shared `lib/vt` parser. Per that decode path is driven by a fuzz
//! harness whose single invariant is:
//!
//! * feeding any byte stream never panics and never reads out of bounds — the
//!   decoder emits well-formed events or silently drops what it cannot
//!   interpret (fail closed), and a never-terminated
//!   bracketed paste cannot make it misbehave.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng`
//! draws pseudo-random byte strings and mutates real key/mouse/paste templates.
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask
//! fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock
//! budget.

use tairix_curses::Input;
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string fed straight to the decoder.
const MAX_NOISE: usize = 4096;

/// Real input-sequence templates the harness mutates: each exercises a
/// different decode path (function key, editing key, SGR mouse report,
/// bracketed paste, arrow key).
const TEMPLATES: &[&[u8]] = &[
    b"\x1bOP\x1bOQ\x1bOR\x1bOS",
    b"\x1b[15~\x1b[24~\x1b[3~\x1b[6~",
    b"\x1b[<0;10;5M\x1b[<2;1;1m\x1b[<64;3;4M",
    b"\x1b[200~pasted\ttext\nwith controls\x1b[201~",
    b"\x1b[A\x1b[B\x1b[C\x1b[D normal text",
    b"\x1b[200~unterminated paste that never ends",
];

/// Feed arbitrary bytes through a fresh decoder: must never panic, whatever it
/// emits. Draining the events also exercises the [`tairix_curses::Event`]
/// payloads.
fn feed_never_panics(bytes: &[u8]) {
    let mut input = Input::new();
    let mut count = 0u64;
    input.feed(bytes, |_event| {
        count = count.wrapping_add(1);
    });
    let _ = count;
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

        // 2. A structured-but-hostile mouse report: the SGR introducer, a
        //    random parameter blob, and a final byte.
        let blob_len = rng.at_most(64);
        let mut spliced = Vec::new();
        spliced.extend_from_slice(b"\x1b[<");
        for _ in 0..blob_len {
            spliced.push(rng.next_u8());
        }
        spliced.push(b'M');
        feed_never_panics(&spliced);

        // 3. Pure noise straight into the decoder.
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
