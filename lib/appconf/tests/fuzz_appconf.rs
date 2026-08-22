//! Deterministic fuzz harness for the per-app configuration engine.
//!
//! Invariants, for any UTF-8 text an untrusted settings file may carry:
//!
//! 1. [`Document::parse`] never panics, and either refuses the document with
//!    a typed bound error or returns one it honours (never more than
//!    [`MAX_LINES`] lines or [`MAX_SETTINGS`] settings).
//! 2. **Preservation is exact.** A document that parses renders back
//!    byte-for-byte — the property the whole line model exists for, and the
//!    one a subtle tokenisation bug would break silently.
//! 3. A rendered document re-parses, and the re-parse agrees on every
//!    setting: parse/render is a fixed point, not merely idempotent once.
//! 4. **A write touches one key and no other.** After a `set`, the written
//!    key reads back exactly, every other key is unchanged, and every line
//!    the grammar refused is still there.
//! 5. Every value the engine accepts round-trips through its rendered form,
//!    including the ones that need quoting and escaping.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use std::collections::BTreeMap;

use tairix_appconf::{ConfError, Document, MAX_LINES, MAX_SETTINGS};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Lehmer-style LCG — deterministic, matches the sibling harnesses so a
/// failure reproduces one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        let index = usize::try_from(self.next_u64() % choices.len() as u64).expect("index fits");
        choices[index]
    }
}

/// Keys and values mixed legal with illegal, so a generated document walks
/// the accept/reject boundary rather than mostly failing at the first byte.
const KEYS: &[&str] = &[
    "scheme",
    "font.size",
    "effects.blur",
    "recent.0",
    "a-b_c.d0",
    "Bad.Key",
    "trailing.",
    ".leading",
    "has space",
    "",
    "a.b.c.d.e.f.g.h.i",
];
const VALUES: &[&str] = &[
    "dark",
    "14",
    "1000",
    "1001",
    "true",
    "off",
    "maybe",
    "-1",
    "",
    "  padded  ",
    "\"quoted\"",
    "\"unterminated",
    "\"bad \\z escape\"",
    "\"a # b\"",
    "\"closed\" junk",
    "bare # comment",
    "/Users/ada/notes.txt",
];
const SEPARATORS: &[&str] = &["=", " = ", "   =", "=   ", " ="];

fn structured_line(rng: &mut Lcg) -> String {
    match rng.next_u64() % 10 {
        0 => return String::from("# a comment"),
        1 => return String::new(),
        2 => return String::from("   "),
        3 => return String::from("no separator here"),
        _ => {}
    }
    format!(
        "{}{}{}",
        rng.pick(KEYS),
        rng.pick(SEPARATORS),
        rng.pick(VALUES)
    )
}

fn build_document(rng: &mut Lcg) -> String {
    let lines = (rng.next_u64() % 40) as usize;
    let mut doc = String::new();
    for index in 0..lines {
        doc.push_str(&structured_line(rng));
        // Sometimes leave the last line without a newline, so the harness
        // covers a file a hand-edit left unterminated.
        if index + 1 < lines || rng.next_u64().is_multiple_of(2) {
            doc.push('\n');
        }
    }
    doc
}

/// Snapshot every setting a document answers for, so a write can be checked
/// to have touched exactly one key.
fn snapshot(doc: &Document) -> BTreeMap<String, String> {
    doc.settings()
        .map(|setting| (setting.key.to_string(), setting.value.to_string()))
        // A later duplicate wins, matching `Document::get`.
        .collect()
}

fn check(text: &str, rng: &mut Lcg) {
    // (1) parse never panics and honours its bounds.
    let doc = match Document::parse(text) {
        Ok(doc) => doc,
        Err(ConfError::DocumentTooLarge | ConfError::TooManyLines | ConfError::TooManySettings) => {
            return
        }
        Err(other) => panic!("parse may only refuse on a bound, got {other}"),
    };
    assert!(doc.settings().count() <= MAX_SETTINGS);
    assert!(doc.unparsed().count() <= MAX_LINES);

    // (2) preservation is exact.
    let rendered = doc.render();
    assert_eq!(rendered, text, "a parsed document must render back exactly");

    // (3) parse/render is a fixed point.
    let reparsed = Document::parse(&rendered).expect("a rendered document parses");
    assert_eq!(snapshot(&reparsed), snapshot(&doc));
    assert_eq!(reparsed.render(), rendered);

    // Every value the engine accepted survives its own rendering.
    for setting in doc.settings() {
        let mut one = Document::new();
        one.set(setting.key, setting.value)
            .expect("an accepted key and value are writable");
        let round = Document::parse(&one.render()).expect("a rendered setting parses");
        assert_eq!(round.get(setting.key), Some(setting.value));
    }

    // (4) a write touches one key and nothing else.
    let key = rng.pick(KEYS);
    let value = rng.pick(VALUES);
    let before = snapshot(&doc);
    let refused_before: Vec<String> = doc.unparsed().map(|line| line.text.to_string()).collect();
    let mut written = Document::parse(text).expect("the same text parses again");
    if written.set(key, value).is_err() {
        // A refused write leaves the document exactly as it was.
        assert_eq!(written.render(), text);
    } else {
        assert_eq!(written.get(key), Some(value));
        let after = snapshot(&written);
        for (other, expected) in &before {
            if other == key {
                continue;
            }
            assert_eq!(
                after.get(other),
                Some(expected),
                "`{other}` must be untouched by a write to `{key}`"
            );
        }
        let refused_after: Vec<String> = written
            .unparsed()
            .map(|line| line.text.to_string())
            .collect();
        assert_eq!(
            refused_after, refused_before,
            "a write must not disturb the lines the grammar refused"
        );
    }
}

#[test]
fn structured_documents_hold_every_invariant() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "structured_documents_hold_every_invariant",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let doc = build_document(&mut rng);
            check(&doc, &mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn arbitrary_ascii_never_panics() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_ascii_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut buf = String::new();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            buf.clear();
            let len = (rng.next_u64() % 256) as usize;
            for _ in 0..len {
                // ASCII (incl. control characters, quotes and backslashes) is
                // always valid UTF-8, so the parser sees a legal `&str`.
                let byte = (rng.next_u64() % 128) as u8;
                buf.push(byte as char);
            }
            check(&buf, &mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
