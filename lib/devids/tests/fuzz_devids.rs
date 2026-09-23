//! Deterministic fuzz harness for the `lib/devids` vetting parser and table
//! decoder — the two untrusted-input surfaces of the ID-database pipeline.
//!
//! [`tairix_devids::textdb::parse`] judges a raw upstream download (untrusted
//! bytes whose strings end up on users' terminals);
//! [`tairix_devids::DevIds::parse`] decodes a compiled table that is still
//! treated as data. The harness's invariants:
//!
//! * parsing any byte string never panics — it returns a `ParsedDb` or a
//!   typed `ParseError` (fail closed);
//! * whatever the parser accepts, the encoder emits and the decoder accepts
//!   back, and lookups over the round-tripped table never panic;
//! * decoding any byte string — noise or a bit-flipped valid table — never
//!   panics, and an accepted table serves lookups without panicking.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` mutates
//! real snapshot templates, splices structured hostile lines, and draws pure
//! noise. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from
//! a fresh, logged seed; `cargo xtask fuzz` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use tairix_devids::{textdb, DbKind, DevIds};
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest arbitrary byte string fed to either parser.
const MAX_NOISE: usize = 1024;

/// Real snapshot templates the harness mutates: each exercises a different
/// grammar path (vendors, devices, subsystems, class tables, auxiliary
/// sections, comments, and deliberately malformed forms).
const TEMPLATES: &[&str] = &[
    "# comment\n8086  Intel\n\t1237  PMC\n\t\t8086 1237  Board\nC 01  Storage\n\t01  IDE\n\t\t00  ISA\n",
    "1d6b  Linux Foundation\n\t0002  2.0 root hub\nC 03  HID\n\t01  Boot\n\t\t01  Keyboard\n",
    "HUT 01  Desktop\n\t002  Mouse\nL 0409  English\n\t01  US\nBIAS 1  Right Hand\n",
    "0001  V\n0001  W\n",
    "\t0001  Orphan\n",
    "C 001  BadWidth\n",
    "0001   Extra Space \n",
    "0001  Tab\tin name\n",
];

/// Line fragments the structured mutator splices together.
const FRAGMENTS: &[&str] = &[
    "8086  Vendor",
    "\t1237  Device",
    "\t\t8086 1237  Subsystem",
    "C 03  Class",
    "HUT 01  Page",
    "# comment",
    "",
    "\t\t\t00  deep",
    "ffff  \u{1b}[2J",
    "AT 0100  Terminal",
];

/// Parse `bytes` as both databases (must not panic); when a parse accepts,
/// round-trip the encoding through the decoder and exercise lookups.
fn exercise_text(bytes: &[u8]) {
    for kind in [DbKind::Pci, DbKind::Usb] {
        if let Ok(db) = textdb::parse(kind, bytes) {
            let encoded = db.encode();
            let ids = DevIds::parse(kind, &encoded)
                .expect("whatever the vetting parser accepts must decode");
            exercise_lookups(&ids);
        }
    }
}

/// Decode `bytes` as both databases (must not panic); exercise lookups on
/// an accepted table.
fn exercise_decode(bytes: &[u8]) {
    for kind in [DbKind::Pci, DbKind::Usb] {
        if let Ok(ids) = DevIds::parse(kind, bytes) {
            exercise_lookups(&ids);
        }
    }
}

/// Probe every lookup surface, including boundary ids.
fn exercise_lookups(ids: &DevIds<'_>) {
    for id in [0u16, 1, 0x8086, 0xffff] {
        let _ = ids.vendor(id);
        let _ = ids.device(id, id);
    }
    for c in [0u8, 1, 3, 0xff] {
        let _ = ids.class(c);
        let _ = ids.subclass(c, c);
        let _ = ids.prog_if(c, c, c);
    }
}

#[test]
fn parse_encode_decode_never_panic_for_any_input() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);

    // The seed is drawn and logged by `tairix_fuzzseed::start`: fresh per
    // run, reproducible from the logged value via `TAIRIX_FUZZ_SEED`.
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "parse_encode_decode_never_panic_for_any_input",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. A real template with a handful of bytes flipped at random.
        let template = *rng.pick(TEMPLATES);
        let mut mutated: Vec<u8> = template.as_bytes().to_vec();
        let flips = rng.at_most(6);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = rng.below(mutated.len());
            mutated[pos] ^= rng.next_u8();
        }
        exercise_text(&mutated);

        // 2. Structured hostile lines: grammar fragments spliced in a random
        //    order with random indentation damage.
        let lines = rng.at_most(12);
        let mut spliced = Vec::new();
        for _ in 0..lines {
            let fragment = *rng.pick(FRAGMENTS);
            let indent = rng.at_most(2);
            spliced.resize(spliced.len() + indent, b'\t');
            spliced.extend_from_slice(fragment.as_bytes());
            spliced.push(b'\n');
        }
        exercise_text(&spliced);

        // 3. Pure noise against the text parser.
        let nlen = rng.at_most(MAX_NOISE);
        let mut noise = vec![0u8; nlen];
        rng.fill(&mut noise);
        exercise_text(&noise);

        // 4. The decoder: noise, and a valid encoded table with bytes
        //    flipped (header, records, and strings damage alike).
        exercise_decode(&noise);
        if let Ok(db) = textdb::parse(DbKind::Pci, TEMPLATES[0].as_bytes()) {
            let mut table = db.encode();
            for _ in 0..rng.at_most(4) {
                if table.is_empty() {
                    break;
                }
                let pos = rng.below(table.len());
                table[pos] ^= rng.next_u8();
            }
            exercise_decode(&table);
        }

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
