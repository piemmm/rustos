//! Deterministic fuzz-style integration test for the logical-record decoder.
//!
//! A logical record body under `/System/Logs` is attacker-influenced (a
//! compromised journal, a tampered or torn file, a volume lifted from another
//! machine), so [`rustos_log::decode_record`] must refuse malformed bytes
//! cleanly and never panic. This harness drives it on both pseudo-random bytes
//! and single-byte mutations of a genuine record (so the version, level,
//! flags, string-length, origin, wall, and `data.*` paths are actually
//! reached), asserting only that it never panics, that a successful decode's
//! `data.*` iterator always terminates, and that a decode's field count agrees
//! with the number of iterated fields.
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition).

use rustos_abi::{
    CapabilitySummary, Duration64, FieldName, FieldValue, Origin, ProcId, Time64, TrustDomain,
    WallClockReading, WallTimeState,
};
use rustos_log::{
    decode_record, CallerContent, DictionaryBuilder, DictionaryView, Level, LogRecord, Stream,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 50_000;

/// Build a genuine, fully-populated record so mutations reach every check.
fn base_record(buf: &mut [u8]) -> usize {
    let origin = Origin::new(
        TrustDomain::User,
        1000,
        1000,
        42,
        ProcId::from_raw([0x7A; 16]),
        CapabilitySummary::from_raw([0u8; 32]),
    );
    let data = [
        (FieldName::new("iface").unwrap(), FieldValue::Str("net0")),
        (
            FieldName::new("elapsed").unwrap(),
            FieldValue::Duration(Duration64::from_secs(10)),
        ),
        (
            FieldName::new("attempt").unwrap(),
            FieldValue::UnsignedInt(3),
        ),
    ];
    let record = LogRecord {
        effective_level: Level::Warn,
        cpu_seq: 0x0102_0304_0506_0708,
        wall: WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
        origin,
        source_name: "service.dhcp",
        caller: CallerContent {
            level: Some(Level::Critical),
            component: Some("dhcp"),
            tag: Some("lease"),
            event_id: Some("dhcp.timeout"),
            requested_source: Some("dhcp"),
            requested_stream: Some(Stream::Runtime),
            message: "dhcp timeout",
        },
        data: &data,
    };
    record
        .encode(buf, &mut DictionaryBuilder::new())
        .expect("base record encodes")
}

/// Drive the record decoder; the contract is "must not panic".
fn exercise(bytes: &[u8]) {
    let mut dict = DictionaryView::new();
    if let Ok(view) = decode_record(bytes, &mut dict) {
        let mut count = 0usize;
        for (name, _value) in view.data() {
            // A validated field name is never empty and within the grammar.
            assert!(!name.as_str().is_empty());
            count += 1;
            // Each field consumes at least the key-length prefix, so a walk of
            // `bytes` can never yield more fields than there are bytes.
            assert!(count <= bytes.len(), "field walk failed to terminate");
        }
        assert_eq!(count, view.data_count(), "iterated count matches header");
    }
}

#[test]
fn random_and_mutated_records_never_panic() {
    let mut rng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "random_and_mutated_records_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut base = [0u8; 512];
    let base_len = base_record(&mut base);

    let mut buf = [0u8; 512];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for i in 0..SMOKE_ITERATIONS {
            if i % 2 == 0 {
                // Pure random bytes of a random length.
                let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                exercise(&buf[..size]);
            } else {
                // A genuine record with a handful of byte flips.
                buf[..base_len].copy_from_slice(&base[..base_len]);
                let flips = (rng.next_u64() % 4) + 1;
                for _ in 0..flips {
                    let pos = ((rng.next_u64() & 0x1FF) as usize) % base_len;
                    buf[pos] ^= rng.next_u64().to_le_bytes()[0] | 1;
                }
                exercise(&buf[..base_len]);
            }
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
