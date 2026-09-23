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
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates a
//! real help document, splices structural Markdown tokens into random
//! blobs, and feeds pure noise. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed;
//! `cargo xtask fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop
//! to a wall-clock budget.

use tairix_fuzzseed::Prng;
use tairix_help::{render_full, render_short, HelpDoc, Locale, RenderCtx, Styling};
use tairix_vt::Op;

/// The served-locale tags the harness renders under, including malformed
/// spellings that must degrade to the canonical locale, never crash.
const FUZZ_LOCALES: &[&str] = &[
    "en-US",
    "fr-FR",
    "de-DE",
    "uk-UA",
    "zh-CN",
    "ja-JP",
    "ar-SA",
    "he-IL",
    "",
    "not a tag",
];

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

/// Parse `input` (must not panic); when it parses, render both surfaces
/// (must not panic) and check the printed characters stay control-free.
///
/// The styling level and served locale are drawn from `rng`, so every render
/// path (plain / monochrome / colour, translated / default headings) is fuzzed.
fn exercise(input: &[u8], rng: &mut Prng) {
    let Ok(doc) = HelpDoc::parse(input) else {
        return;
    };
    let styling = match rng.below(3) {
        0 => Styling::Plain,
        1 => Styling::Monochrome,
        _ => Styling::Colour,
    };
    let tag = *rng.pick(FUZZ_LOCALES);
    let locale = Locale::parse(tag).unwrap_or_default();
    let ctx = RenderCtx::new(&locale, styling);
    for ops in [render_short(&doc, &ctx), render_full(&doc, &ctx)] {
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

    // The seed is drawn and logged by `tairix_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "parse_and_render_never_panic_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. The real template with a handful of bytes flipped at random.
        let mut mutated: Vec<u8> = TEMPLATE.as_bytes().to_vec();
        let flips = rng.at_most(8);
        for _ in 0..flips {
            let pos = rng.below(mutated.len());
            if let Some(byte) = mutated.get_mut(pos) {
                *byte ^= rng.next_u8();
            }
        }
        exercise(&mutated, &mut rng);

        // 2. A structured-but-hostile document: structural tokens spliced
        //    together with random letters.
        let pieces = rng.at_most(64);
        let mut spliced = String::new();
        for _ in 0..pieces {
            let pick = rng.at_most(TOKENS.len());
            match TOKENS.get(pick) {
                Some(token) => spliced.push_str(token),
                None => spliced.push(char::from(b'a' + rng.next_u8() % 26)),
            }
        }
        exercise(spliced.as_bytes(), &mut rng);

        // 3. Pure byte noise.
        let nlen = rng.at_most(MAX_NOISE);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        exercise(&noise, &mut rng);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
