//! Deterministic fuzz-style integration test for the boot-console renderer.
//!
//! A committed log record carries attacker-controlled text (`caller.message`,
//! `caller.component`, the caller's *requested* source, string `data.*`
//! values). [`rustos_log::render_line`] must turn any such record into a
//! terminal line that a hostile caller cannot use to inject escape sequences,
//! move the cursor, or forge lines: the rendered bytes must be free of C0
//! controls and `DEL`, whatever the input, and rendering must never panic.
//!
//! The harness drives the renderer two ways: on records decoded from
//! pseudo-random bytes (so any body the on-disk decoder accepts is rendered),
//! and on deliberately hostile records built from random control-laden ASCII
//! (so the message/component/source/field escaping paths are actually reached).
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition).

use rustos_abi::{
    CapabilitySummary, Duration64, FieldName, FieldValue, Origin, ProcId, TrustDomain,
    WallClockReading, ORIGIN_CONSOLE_NONE,
};
use rustos_log::{
    decode_record, render_line, CallerContent, DictionaryBuilder, DictionaryView, Level, LogRecord,
    LogRecordRef,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 40_000;

/// The one invariant every rendered line must hold: no C0 control byte and no
/// `DEL`. Printable multi-byte UTF-8 (>= 0x80) is allowed through unchanged.
fn assert_control_free(line: &str) {
    for b in line.bytes() {
        assert!(
            b >= 0x20 && b != 0x7f,
            "renderer emitted a raw control byte {b:#04x} in {line:?}"
        );
    }
}

/// Render whatever the record decoder accepts from `bytes`; the contract is
/// "never panic, and the line is control-free".
fn exercise_decoded(bytes: &[u8], monotonic: Duration64) {
    let mut dict = DictionaryView::new();
    if let Ok(view) = decode_record(bytes, &mut dict) {
        let mut line = String::new();
        render_line(&mut line, monotonic, &view).expect("String sink never fails");
        assert_control_free(&line);
    }
}

/// A byte slice interpreted as a bounded, valid-UTF-8, control-laden string:
/// mask each byte into `0x00..=0x7f` (always valid UTF-8) so control characters
/// are frequent, and cap the length so the record encoder accepts it.
fn ascii<'a>(src: &'a mut [u8], raw: &[u8], max: usize) -> &'a str {
    let n = raw.len().min(max).min(src.len());
    for i in 0..n {
        src[i] = raw[i] & 0x7f;
    }
    // `&0x7f` keeps every byte in the single-byte UTF-8 range, so this is total.
    core::str::from_utf8(&src[..n]).expect("masked bytes are valid UTF-8")
}

/// Build a hostile record from `raw`, render it, and assert the output is
/// control-free. Exercises the message/component/requested-source/field
/// escaping paths that a purely-random decode rarely reaches.
fn exercise_hostile(raw: &[u8], monotonic: Duration64, level: Level) {
    let (mut mbuf, mut cbuf, mut sbuf, mut vbuf) = ([0u8; 64], [0u8; 32], [0u8; 64], [0u8; 64]);
    // Deterministically slice `raw` into four sub-strings (empty slices are fine).
    let q = raw.len() / 4;
    let message = ascii(&mut mbuf, raw.get(..q).unwrap_or(&[]), 60);
    let component = ascii(&mut cbuf, raw.get(q..2 * q).unwrap_or(&[]), 30);
    let requested = ascii(&mut sbuf, raw.get(2 * q..3 * q).unwrap_or(&[]), 60);
    let field_val = ascii(&mut vbuf, raw.get(3 * q..).unwrap_or(&[]), 60);

    let data = [(FieldName::new("input").unwrap(), FieldValue::Str(field_val))];
    let record = LogRecord {
        effective_level: level,
        cpu_seq: 0,
        wall: WallClockReading::default(),
        origin: Origin::new(
            TrustDomain::User,
            1000,
            1000,
            42,
            ProcId::from_raw([7u8; 16]),
            CapabilitySummary::from_raw([0u8; 32]),
            ORIGIN_CONSOLE_NONE,
        ),
        source_name: "user.1000.proc.0707",
        caller: CallerContent {
            level: None,
            component: (!component.is_empty()).then_some(component),
            tag: None,
            event_id: None,
            requested_source: (!requested.is_empty()).then_some(requested),
            requested_stream: None,
            message,
        },
        data: &data,
    };

    let mut buf = [0u8; 512];
    let len = record
        .encode(&mut buf, &mut DictionaryBuilder::new())
        .expect("bounded hostile record encodes");
    let mut view = DictionaryView::new();
    let decoded: LogRecordRef<'_> = decode_record(&buf[..len], &mut view).expect("decodes");
    let mut line = String::new();
    render_line(&mut line, monotonic, &decoded).expect("String sink never fails");
    assert_control_free(&line);
}

#[test]
fn rendered_lines_are_control_free_and_never_panic() {
    let mut rng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "rendered_lines_are_control_free_and_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut buf = [0u8; 512];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for i in 0..SMOKE_ITERATIONS {
            let monotonic = Duration64::from_nanos(rng.next_u64());
            if i % 2 == 0 {
                let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                exercise_decoded(&buf[..size], monotonic);
            } else {
                let size = ((rng.next_u64() & 0xFF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                let level = Level::from_u8((rng.next_u64() % 6) as u8).unwrap_or(Level::Info);
                exercise_hostile(&buf[..size], monotonic, level);
            }
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
