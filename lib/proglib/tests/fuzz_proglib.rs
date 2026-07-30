//! Deterministic fuzz harness for the program-library catalog store.
//!
//! Invariants, for any bytes an untrusted `library.conf` may carry:
//!
//! 1. [`parse`] never panics on any UTF-8 input, and an accepted document
//!    never yields more than [`MAX_ENTRIES`] records.
//! 2. [`render`] and [`parse`] are inverses: an accepted catalog re-renders
//!    to a document that parses back equal, and the rendered text is itself
//!    within [`MAX_CATALOG_LEN`], so a writer can never emit a store the
//!    reader would refuse as too long.
//! 3. [`merge`] never panics for any pair of accepted catalogs, resolves
//!    every patch, stays within the record bound, and resolves visibility
//!    with the overlay's verdict last: an identifier survives exactly when
//!    the last word on it — the user's patch, else the machine's, else its
//!    own declaration — shows it.
//!
//! The generator emits *whole records* and mutates them at a low rate, so
//! most documents are accepted and the round-trip and merge invariants are
//! genuinely exercised rather than being refused at the first line. The
//! third test then hammers the parser with arbitrary ASCII, where nothing
//! but "does not panic" is expected.
//!
//! The fixed sweep runs under plain `cargo test`; under `cargo xtask fuzz`
//! the same seeded stream keeps being drawn until the budget elapses.

use tairix_proglib::{merge, parse, render, EntryPatch, Record, MAX_CATALOG_LEN, MAX_ENTRIES};

/// Fixed-iteration sweep run when no budget is set.
const SMOKE_ITERATIONS: u64 = 5_000;

/// Deterministic LCG, matching the sibling harnesses so a reported seed
/// reproduces a failure exactly one way.
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

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Whether a one-in-`n` event fires.
    fn chance(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        choices[usize::try_from(self.below(choices.len() as u64)).expect("index fits")]
    }
}

/// Identifiers a record may be filed under. Drawn without replacement per
/// document so a duplicate key is a *mutation*, not the common case.
const IDS: &[&str] = &[
    "editor",
    "com.example.editor",
    "term-1",
    "a",
    "chess",
    "net.tairix.files",
];
const NAMES: &[&str] = &["Text Editor", "Chess", "F", "Files & Folders"];
const BUNDLES: &[&str] = &[
    "/Apps/Editor.app",
    "/Apps/games/Chess.app",
    "/Users/ada/Apps/Editor.app",
    "/System/Apps/ls.app",
];
/// Folder identifiers spelled exactly as the closed taxonomy renders them:
/// the decode is case-sensitive, so only these spellings are accepted.
const CATEGORIES: &[&str] = &[
    "Accessories",
    "Graphics",
    "Internet",
    "Multimedia",
    "Office",
    "Programming",
    "Games",
    "SystemTools",
    "Utilities",
    "Other",
];
const ICONS: &[&str] = &["icon.svg", "art/icon.svg", "chess.svg"];
/// Tokens that should be refused wherever they appear.
const BAD_TOKENS: &[&str] = &[
    "",
    " ",
    "has space",
    ".leading",
    "trailing.",
    "utilities",
    "Development",
    "/Apps/Editor",
    "Apps/Editor.app",
    "/Apps//Editor.app",
    "../escape.svg",
    "/absolute.svg",
    "yes",
    "Name",
    "bogus",
];

/// One `<id>.<field> <value>` line, with `value` mutated to a refused token
/// at a low rate so the accept/reject boundary is walked from both sides.
///
/// The rates are per *line*, and a document carries several lines across
/// several records, so they compound: they are kept low enough that the
/// whole document is accepted far more often than not, which is what keeps
/// the round-trip and merge invariants genuinely exercised.
fn line(rng: &mut Lcg, id: &str, field: &str, value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let field = if rng.chance(64) {
        rng.pick(BAD_TOKENS)
    } else {
        field
    };
    let value = if rng.chance(48) {
        rng.pick(BAD_TOKENS)
    } else {
        value
    };
    let _ = write!(out, "{id}.{field} {value}");
    out
}

/// One record: a declared entry (`name` + `bundle`) or a patch (no
/// `bundle`, optionally `hidden`), with the odd stray comment or blank line
/// and the occasional malformed identifier.
fn record(rng: &mut Lcg, id: &str, out: &mut String) {
    let id = if rng.chance(64) {
        rng.pick(BAD_TOKENS)
    } else {
        id
    };
    let declared = !rng.chance(3);

    if rng.chance(6) {
        out.push_str("# a comment\n");
    }
    let name = rng.pick(NAMES);
    out.push_str(&line(rng, id, "name", name));
    out.push('\n');
    if declared {
        let bundle = rng.pick(BUNDLES);
        out.push_str(&line(rng, id, "bundle", bundle));
        out.push('\n');
    }
    if rng.chance(2) {
        let category = rng.pick(CATEGORIES);
        out.push_str(&line(rng, id, "category", category));
        out.push('\n');
    }
    if rng.chance(3) {
        let icon = rng.pick(ICONS);
        out.push_str(&line(rng, id, "icon", icon));
        out.push('\n');
    }
    // `hidden` is legal on any record — a declaration may suppress itself,
    // a patch carries the overlay's verdict — so emit it on both, walking
    // the flag grammar's accept/reject boundary with the odd bad spelling.
    if rng.chance(3) {
        let flag = if rng.chance(8) {
            "yes"
        } else if rng.chance(2) {
            "false"
        } else {
            "true"
        };
        out.push_str(&line(rng, id, "hidden", flag));
        out.push('\n');
    }
    if rng.chance(8) {
        out.push('\n');
    }
}

fn document(rng: &mut Lcg) -> String {
    let mut out = String::new();
    let mut pool: Vec<&str> = IDS.to_vec();
    let records = rng.below(IDS.len() as u64 + 1);
    for _ in 0..records {
        // Draw without replacement: a repeated identifier would be a
        // duplicate-key refusal in almost every document otherwise.
        let index = usize::try_from(rng.below(pool.len().max(1) as u64)).expect("index fits");
        let Some(id) = pool.get(index).copied() else {
            break;
        };
        pool.swap_remove(index);
        record(rng, id, &mut out);
    }
    out
}

/// The round-trip invariant. Returns whether the document was accepted, so
/// the corpus-quality test can measure the generator.
fn check_round_trip(doc: &str) -> bool {
    let Ok(catalog) = parse(doc) else {
        return false;
    };
    assert!(catalog.len() <= MAX_ENTRIES);

    let rendered = render(&catalog);
    assert!(
        rendered.len() <= MAX_CATALOG_LEN,
        "a rendered catalog exceeded the document bound"
    );
    let reparsed = parse(&rendered).expect("a rendered catalog re-parses");
    assert_eq!(catalog, reparsed, "render/parse is not a round trip");
    true
}

fn check_merge(machine: &str, overlay: &str) {
    let (Ok(machine), Ok(overlay)) = (parse(machine), parse(overlay)) else {
        return;
    };

    let resolved = merge(&machine, &overlay);
    assert!(resolved.len() <= MAX_ENTRIES);
    for (_, record) in resolved.records() {
        assert!(
            matches!(record, Record::Entry(_)),
            "merge left a patch unresolved"
        );
    }

    // The visibility oracle: an identifier resolves to an entry exactly
    // when some document declares it and the last verdict on it — the
    // overlay's patch, else the machine's, else the declaration's own
    // flag — shows it.
    for (id, _) in machine.records().chain(overlay.records()) {
        let declared = overlay.entry(id).or_else(|| machine.entry(id));
        let shown = declared.map(|entry| {
            !overlay
                .entry_patch(id)
                .and_then(EntryPatch::hidden)
                .or_else(|| machine.entry_patch(id).and_then(EntryPatch::hidden))
                .unwrap_or(entry.hidden())
        });
        assert_eq!(
            resolved.entry(id).is_some(),
            shown.unwrap_or(false),
            "merge and the visibility oracle disagree for {id}"
        );
    }

    let rendered = render(&resolved);
    let reparsed = parse(&rendered).expect("a rendered resolved catalog re-parses");
    assert_eq!(resolved, reparsed);
}

#[test]
fn generated_documents_round_trip_through_render() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "generated_documents_round_trip_through_render",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            check_round_trip(&document(&mut rng));
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn merging_resolves_every_patch_and_honours_every_hide() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "merging_resolves_every_patch_and_honours_every_hide",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let machine = document(&mut rng);
            let overlay = document(&mut rng);
            check_merge(&machine, &overlay);
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
            let len = rng.below(256);
            for _ in 0..len {
                // ASCII — control characters, `.`, `/`, digits, letters —
                // is always valid UTF-8, so `parse` sees a legal `&str`.
                buf.push(char::from(u8::try_from(rng.below(128)).expect("byte fits")));
            }
            let _ = parse(&buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

/// A generator that almost never produced an accepted document would leave
/// the round-trip and merge invariants untested while still passing, so the
/// corpus itself is asserted on.
#[test]
fn the_generator_produces_accepted_documents() {
    const DRAWS: u64 = 2_000;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_generator_produces_accepted_documents",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let accepted = (0..DRAWS)
        .filter(|_| check_round_trip(&document(&mut rng)))
        .count();
    assert!(
        u64::try_from(accepted).expect("count fits") * 4 >= DRAWS,
        "only {accepted} of {DRAWS} generated documents parsed; the corpus is degenerate"
    );
}
