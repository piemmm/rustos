//! Deterministic fuzz harness for the `lib/users` database parser
//! (`AGENTS.md` §19.5 / §19.6 — a parser of on-disk, untrusted input).
//!
//! `/System/Security/Users` is read by the login path before any session
//! exists, so its bytes are outside the trust boundary of the reader: a
//! hostile or corrupted database must be **rejected**, never trusted
//! (`AGENTS.md` §5.4 — fail closed). Per §19.6 the decode path is driven
//! here against arbitrary text, with two invariants:
//!
//! * feeding any string to [`rustos_users::UsersDb::parse`] never panics
//!   and never reads out of bounds — it returns a database or a
//!   [`rustos_users::ParseError`] (`AGENTS.md` §2.9);
//! * any database that parses re-serialises to text that parses back to an
//!   equal database (the format has one meaning).
//!
//! RustOS pulls in no external fuzz runner (`AGENTS.md` §2.12): a
//! per-run-seeded LCG mutates real databases built through the public
//! constructors, splices hostile record lines under a valid header, and
//! feeds pure noise. A plain `cargo test` runs the fixed
//! [`SMOKE_ITERATIONS`] sweep; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.
//!
//! [`UsersDb::authenticate`] is deliberately *not* driven per iteration:
//! its cost is the PBKDF2 work factor by design, and its input validation
//! is the same parser surface exercised here.

use rustos_abi::CapabilityId;
use rustos_caps::CapabilitySet;
use rustos_users::{
    AccountState, Gid, Identity, Uid, UserRecord, UsersDb, FORMAT_HEADER, MIN_ITERATIONS,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary string fed straight to the parser.
const MAX_NOISE: usize = 2048;

/// Bytes the noise generator draws from: the format's own alphabet, so the
/// mutations reach past the first charset check instead of bouncing off it.
const ALPHABET: &[u8] = b"abcdefxyz0123456789:,$#/_-. \nACTIVELOCKEDpbkdf2sha256rustos-users-v1";

/// Build the corpus of real, well-formed databases through the public
/// constructors, so this harness encodes no second copy of the format
/// (`AGENTS.md` §2.2).
fn templates() -> Vec<String> {
    let record = |username: &str, uid: u32, state: AccountState| {
        let mut capabilities = CapabilitySet::empty();
        capabilities.insert(CapabilityId::PROC_SPAWN);
        capabilities.insert(CapabilityId::USER_ADMIN);
        UserRecord::with_password(
            Identity {
                username,
                uid: Uid(uid),
                primary_gid: Gid(uid),
                supplementary_gids: &[Gid(4), Gid(100)],
                display_name: "Fuzz Fixture",
                home: "/Users/fuzz",
                shell: "/Apps/Shell.app/Run",
                capabilities,
                state,
            },
            b"fixture",
            [0x11; 16],
            MIN_ITERATIONS,
        )
        .expect("fixture record is valid")
    };

    let single = UsersDb::new(vec![record("root", 0, AccountState::Active)]).expect("valid");
    let multi = UsersDb::new(vec![
        record("root", 0, AccountState::Active),
        record("ada", 1000, AccountState::Active),
        record("mallory", 1001, AccountState::Locked),
    ])
    .expect("valid");
    let empty = UsersDb::new(Vec::new()).expect("valid");

    vec![
        single.serialise(),
        multi.serialise(),
        empty.serialise(),
        format!("{FORMAT_HEADER}\n# only a comment\n\n"),
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
    let Ok(db) = UsersDb::parse(text) else {
        return;
    };
    let reparsed = UsersDb::parse(&db.serialise()).expect("serialised database parses back");
    assert_eq!(reparsed, db, "round trip changed the database");
    for record in db.records() {
        let _ = db.lookup(record.username());
        let _ = db.lookup_uid(record.uid());
    }
}

#[test]
fn parsing_any_users_database_never_panics() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    let corpus = templates();

    // The LCG seed is drawn and logged by `rustos_fuzzseed::start`: fresh
    // per run, reproducible from the logged value via `RUSTOS_FUZZ_SEED`.
    let mut state: u64 = rustos_fuzzseed::start(
        "parsing_any_users_database_never_panics",
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
        let mut spliced = String::from(FORMAT_HEADER);
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
