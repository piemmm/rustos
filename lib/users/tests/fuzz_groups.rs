//! Deterministic fuzz harness for the `lib/users` group-database parser
//! (a parser of on-disk, untrusted input).
//!
//! `/System/Security/Groups` is read by the kernel before any session
//! exists, so its bytes are outside the trust boundary of the reader: a
//! hostile or corrupted database must be **rejected**, never trusted (fail
//! closed). The decode path is driven here against arbitrary text, with two
//! invariants:
//!
//! * feeding any string to [`rustos_users::GroupsDb::parse`] never panics
//!   and never reads out of bounds — it returns a database or a
//!   [`rustos_users::ParseError`];
//! * any database that parses re-serialises to text that parses back to an
//!   equal database (the format has one meaning).
//!
//! Like the `fuzz_users` harness it pulls in no external fuzz runner: a
//! per-run-seeded LCG mutates real databases built through the public
//! constructors, splices hostile record lines under a valid header, and
//! feeds pure noise. A plain `cargo test` runs the fixed
//! [`SMOKE_ITERATIONS`] sweep; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use rustos_users::{Gid, GroupRecord, GroupsDb, GROUPS_FORMAT_HEADER};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary string fed straight to the parser.
const MAX_NOISE: usize = 2048;

/// Bytes the noise generator draws from: the format's own alphabet, so the
/// mutations reach past the first charset check instead of bouncing off it.
const ALPHABET: &[u8] = b"abcdefxyz0123456789:,#/_-. \nrustos-groups-v1";

/// Build the corpus of real, well-formed databases through the public
/// constructors, so this harness encodes no second copy of the format.
fn templates() -> Vec<String> {
    let record = |name: &str, gid: u32| GroupRecord::new(name, Gid(gid)).expect("fixture is valid");

    let single = GroupsDb::new(vec![record("wheel", 0)]).expect("valid");
    let multi = GroupsDb::new(vec![
        record("wheel", 0),
        record("staff", 50),
        record("ada", 1000),
    ])
    .expect("valid");
    let empty = GroupsDb::new(Vec::new()).expect("valid");

    vec![
        single.serialise(),
        multi.serialise(),
        empty.serialise(),
        format!("{GROUPS_FORMAT_HEADER}\n# only a comment\n\n"),
    ]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Parse `text`; on success, the round-trip invariant must hold. Must never
/// panic, whatever the input.
fn exercise_never_panics(text: &str) {
    let Ok(db) = GroupsDb::parse(text) else {
        return;
    };
    let reparsed = GroupsDb::parse(&db.serialise()).expect("serialised database parses back");
    assert_eq!(reparsed, db, "round trip changed the database");
    for record in db.records() {
        let _ = db.lookup(record.name());
        let _ = db.lookup_gid(record.gid());
    }
}

#[test]
fn parsing_any_group_database_never_panics() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    let corpus = templates();

    // The LCG seed is drawn and logged by `rustos_fuzzseed::start`: fresh
    // per run, reproducible from the logged value via `RUSTOS_FUZZ_SEED`.
    let mut state: u64 = rustos_fuzzseed::start(
        "parsing_any_group_database_never_panics",
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
        // 1. A real database with a handful of bytes swapped for alphabet
        //    bytes, hammering the header, field separators, and encodings.
        let template = &corpus[bounded(next(), corpus.len() - 1)];
        let mut mutated = template.clone().into_bytes();
        for _ in 0..bounded(next(), 12) {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] = ALPHABET[bounded(next() >> 17, ALPHABET.len() - 1)];
        }
        if let Ok(text) = core::str::from_utf8(&mutated) {
            exercise_never_panics(text);
        }

        // 2. A truncation of a real database, driving the field-count and
        //    record-shape checks.
        let keep = bounded(next(), template.len());
        if let Some(prefix) = template.get(..keep) {
            exercise_never_panics(prefix);
        }

        // 3. A valid header over hostile record lines built from the
        //    format's own alphabet.
        let mut spliced = String::from(GROUPS_FORMAT_HEADER);
        spliced.push('\n');
        for _ in 0..bounded(next(), MAX_NOISE) {
            spliced.push(char::from(
                ALPHABET[bounded(next() >> 23, ALPHABET.len() - 1)],
            ));
        }
        exercise_never_panics(&spliced);

        // 4. Pure alphabet noise straight into the parser.
        let mut noise = String::new();
        for _ in 0..bounded(next(), MAX_NOISE) {
            noise.push(char::from(
                ALPHABET[bounded(next() >> 29, ALPHABET.len() - 1)],
            ));
        }
        exercise_never_panics(&noise);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
