//! Deterministic fuzz harness for the `lib/help` document parser and
//! renderers (its untrusted-input surface).
//!
//! [`tairix_help::HelpDoc::parse`] consumes a help document read from an
//! installed bundle — signed, but parsed as hostile input. The harness's
//! invariants are:
//!
//! * parsing any byte string never panics — it returns a `HelpDoc` or a
//!   typed `HelpError` (fail closed), in bounded time;
//! * any document that parses renders through both surfaces
//!   ([`tairix_help::render_short`], [`tairix_help::render_full`]) without
//!   panicking, and the emitted operations print no control character (the
//!   parser's control-byte rejection holds through to the output).
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG mutates a
//! real help document, splices structural Markdown tokens into random
//! blobs, and feeds pure noise. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed;
//! `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop
//! to a wall-clock budget.

use tairix_help::{render_full, render_short, HelpDoc};
use tairix_vt::Op;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary byte blob fed to the parser.
const MAX_NOISE: usize = 2_048;

/// A real, fully-featured help document the harness mutates: every block
/// kind (paragraph, sub-heading, both lists with continuation, fence,
/// table) and every inline marker.
const TEMPLATE: &str = "## NAME\n\ntop — display *running* tasks\n\n\
## SYNOPSIS\n\n```\ntop [-d seconds]\n```\n\n\
## DESCRIPTION\n\nFirst line\nsecond line.\n\n### Refresh\n\nMore **detail** here.\n\n\
## OPTIONS\n\n- `-d, --delay <seconds>` — refresh delay\n  continued description\n- `-h, -?` — short help\n\n\
## EXAMPLES\n\n1. run it\n2. read it\n\n\
## EXIT STATUS\n\n| Code | Meaning |\n|-----:|:-------:|\n| 0 | ok |\n\n\
## ENVIRONMENT\n\n`LANG` — locale\n\n\
## SEE ALSO\n\n`ps` \\| `ls`\n";

/// Structural tokens spliced into hostile documents, exercising the
/// heading, fence, list, table, and inline parsers.
const TOKENS: &[&str] = &[
    "## NAME\n",
    "## SYNOPSIS\n",
    "## DESCRIPTION\n",
    "## WRONG\n",
    "### sub\n",
    "```\n",
    "```rust\n",
    "- item\n",
    "1. item\n",
    "999. item\n",
    "  continuation\n",
    "| a | b |\n",
    "|---|---:|\n",
    "|:---:|\n",
    "`code`",
    "**strong**",
    "*em*",
    "\\*",
    "\\",
    "`",
    "**",
    "*",
    "|",
    "#",
    "\n",
    "\n\n",
    "é—口",
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

/// Parse `input` (must not panic); when it parses, render both surfaces
/// (must not panic) and check the printed characters stay control-free.
fn exercise(input: &[u8]) {
    let Ok(doc) = HelpDoc::parse(input) else {
        return;
    };
    for ops in [render_short(&doc), render_full(&doc)] {
        for op in ops {
            if let Op::Print(ch) = op {
                assert!(!ch.is_control(), "rendered control character {ch:?}");
            }
        }
    }
}

#[test]
fn parse_and_render_never_panic_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `tairix_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut state: u64 = tairix_fuzzseed::start(
        "parse_and_render_never_panic_for_any_input",
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
        // 1. The real template with a handful of bytes flipped at random.
        let mut mutated: Vec<u8> = TEMPLATE.as_bytes().to_vec();
        let flips = bounded(next(), 8);
        for _ in 0..flips {
            let pos = bounded(next(), mutated.len() - 1);
            if let Some(byte) = mutated.get_mut(pos) {
                *byte ^= low_byte(next() >> 17);
            }
        }
        exercise(&mutated);

        // 2. A structured-but-hostile document: structural tokens spliced
        //    together with random letters.
        let pieces = bounded(next(), 64);
        let mut spliced = String::new();
        for _ in 0..pieces {
            let pick = bounded(next(), TOKENS.len());
            match TOKENS.get(pick) {
                Some(token) => spliced.push_str(token),
                None => spliced.push(char::from(b'a' + low_byte(next() >> 29) % 26)),
            }
        }
        exercise(spliced.as_bytes());

        // 3. Pure byte noise.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 23)).collect();
        exercise(&noise);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
