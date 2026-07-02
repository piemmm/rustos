//! Deterministic fuzz-style integration test for the rich record renderers.
//!
//! A committed log record carries attacker-controlled text (`caller.message`,
//! `caller.component`, the caller's *requested* source, string `data.*`
//! values). The JSON, Markdown, and table views
//! ([`rustos_log::render_json`] / [`rustos_log::render_markdown`] /
//! [`rustos_log::render_table_row`]) must turn any such record into output a
//! hostile caller cannot use to inject escape sequences, forge lines, or (for
//! JSON) break the document: the rendered bytes must be free of C0 controls
//! and `DEL`, whatever the input, and rendering must never panic.
//!
//! The harness drives the renderers two ways: on records decoded from
//! pseudo-random bytes (so any body the on-disk decoder accepts is rendered),
//! and on deliberately hostile records built from random control-laden ASCII
//! (so the message/component/source/field escaping paths are actually reached).
//!
//! Seed selection, the start-of-test seed log, and the smoke / soak loop are
//! the shared `rustos_fuzzseed` seam (one definition).

use rustos_abi::{
    BootId, CapabilitySummary, Duration64, FieldName, FieldValue, Origin, ProcId, TrustDomain,
    WallClockReading, BOOT_ID_LEN,
};
use rustos_log::{
    decode_record, render_json, render_markdown, render_table_row, CallerContent,
    DictionaryBuilder, DictionaryView, Level, LogRecord, LogRecordRef, RecordFrame, Stream,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 30_000;

/// Every rendered view must be free of C0 control bytes and `DEL`. Printable
/// multi-byte UTF-8 (>= 0x80) is allowed through unchanged.
///
/// `allow_newline` is set for the Markdown view, whose *own* structure spans
/// several lines (the renderer emits the `\n` separators itself). Caller text
/// can never contribute one: it is escaped to a visible `\x0a`, so a stray
/// newline would still be the renderer's, not an injection. The single-line
/// table row and the single-line JSON object forbid every control byte.
fn assert_control_free(what: &str, out: &str, allow_newline: bool) {
    for b in out.bytes() {
        if allow_newline && b == 0x0a {
            continue;
        }
        assert!(
            b >= 0x20 && b != 0x7f,
            "{what} emitted a raw control byte {b:#04x} in {out:?}"
        );
    }
}

/// A JSON object rendered by [`render_json`] must at least be brace-delimited
/// and control-free; the exact escaping is pinned by the unit tests.
fn assert_json_shape(out: &str) {
    assert!(out.starts_with("{\"version\":"), "json head: {out:?}");
    assert!(out.ends_with('}'), "json tail: {out:?}");
    assert_control_free("render_json", out, false);
}

/// Derive a [`RecordFrame`] from the PRNG so the container-owned facts vary too.
fn frame(rng: &mut rustos_fuzzseed::Lcg) -> RecordFrame {
    let stream = Stream::from_u8((rng.next_u64() % 6) as u8).unwrap_or(Stream::Runtime);
    let mut boot = [0u8; BOOT_ID_LEN];
    rng.fill(&mut boot);
    RecordFrame {
        stream,
        boot_id: BootId::from_raw(boot),
        // Mask before narrowing so the value provably fits (no truncation lint).
        cpu_id: (rng.next_u64() & 0xFFFF_FFFF) as u32,
        seq: rng.next_u64(),
        monotonic: Duration64::from_nanos(rng.next_u64()),
    }
}

/// Render whatever the record decoder accepts from `bytes` through all three
/// views; the contract is "never panic, output control-free, JSON well-shaped".
fn exercise_decoded(bytes: &[u8], frame: &RecordFrame) {
    let mut dict = DictionaryView::new();
    if let Ok(view) = decode_record(bytes, &mut dict) {
        render_all(frame, &view);
    }
}

fn render_all(frame: &RecordFrame, view: &LogRecordRef<'_>) {
    let mut json = String::new();
    render_json(&mut json, frame, view).expect("String sink never fails");
    assert_json_shape(&json);

    let mut md = String::new();
    render_markdown(&mut md, frame, view).expect("String sink never fails");
    assert_control_free("render_markdown", &md, true);

    let mut row = String::new();
    render_table_row(&mut row, frame, view).expect("String sink never fails");
    assert_control_free("render_table_row", &row, false);
}

/// A byte slice interpreted as a bounded, valid-UTF-8, control-laden string:
/// mask each byte into `0x00..=0x7f` (always valid UTF-8) so control characters
/// (and `"`, `\`, backtick) are frequent, and cap the length so the record
/// encoder accepts it.
fn ascii<'a>(src: &'a mut [u8], raw: &[u8], max: usize) -> &'a str {
    let n = raw.len().min(max).min(src.len());
    for i in 0..n {
        src[i] = raw[i] & 0x7f;
    }
    core::str::from_utf8(&src[..n]).expect("masked bytes are valid UTF-8")
}

/// Build a hostile record from `raw`, render it through every view, and assert
/// each output is control-free (and JSON well-shaped). Exercises the
/// message/component/requested-source/field escaping paths a random decode
/// rarely reaches.
fn exercise_hostile(raw: &[u8], frame: &RecordFrame, level: Level) {
    let (mut mbuf, mut cbuf, mut sbuf, mut vbuf) = ([0u8; 64], [0u8; 32], [0u8; 64], [0u8; 64]);
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
    render_all(frame, &decoded);
}

#[test]
fn rendered_views_are_control_free_and_never_panic() {
    let mut rng = rustos_fuzzseed::Lcg::new(rustos_fuzzseed::start(
        "rendered_views_are_control_free_and_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut buf = [0u8; 512];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for i in 0..SMOKE_ITERATIONS {
            let frame = frame(&mut rng);
            if i % 2 == 0 {
                let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                exercise_decoded(&buf[..size], &frame);
            } else {
                let size = ((rng.next_u64() & 0xFF) as usize) % (buf.len() + 1);
                rng.fill(&mut buf[..size]);
                let level = Level::from_u8((rng.next_u64() % 6) as u8).unwrap_or(Level::Info);
                exercise_hostile(&buf[..size], &frame, level);
            }
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
