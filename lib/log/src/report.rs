//! Rich structured views over committed log records (SYSLOG §8.3).
//!
//! The boot-console renderer ([`crate::render_line`]) emits one flat readable
//! line. This module adds the structured *report* views the `log` tools render
//! for `log show`, `log report --format md`, and `log export --format json`: a
//! JSON object, a Markdown fragment, and an aligned terminal-table row. All
//! three are **views** — the segment files and anchors remain the authority
//! (SYSLOG §17); editing a rendered report never changes the log.
//!
//! Two properties hold for every view, exactly as for the boot line:
//!
//! * **Provenance is preserved, never obeyed.** Each view separates the
//!   *system-attested* facts the kernel/journal vouch for (stream, sequence,
//!   CPU, monotonic and wall time, effective level, system-derived source, and
//!   the attested [`Origin`](rustos_abi::Origin)) from the *caller-supplied*
//!   content (message,
//!   caller level, component, tag, event id, the stream/source the caller
//!   *requested*, and the `data.*` fields). A caller that requested a
//!   privileged source or stream has that request shown inertly as a caller
//!   claim, never promoted to the real source/stream.
//!
//! * **Caller text can never forge output.** Every caller-controlled string is
//!   escaped: control characters (newline, `ESC`, `DEL`, the C1 range, …)
//!   cannot move a cursor, forge a prefix, or split a line, and the JSON view
//!   additionally escapes `"`/`\` and emits control bytes as `\u00xx`, so its
//!   output is valid JSON and free of raw control bytes.
//!
//! Like the rest of the crate the renderers are `no_std` and allocation-free:
//! they write into any [`core::fmt::Write`] sink.

use core::fmt::{self, Write};

use rustos_abi::{
    BootId, Duration64, FieldValue, TrustDomain, WallTimeState, BOOT_ID_HEX_LEN, PROC_ID_HEX_LEN,
};

use crate::record::{LogRecordRef, RECORD_FORMAT_VERSION};
use crate::render::{level_label, write_escaped, write_value};
use crate::stream::Stream;

/// The container-owned facts a rich renderer pairs with a decoded record body.
///
/// A reader fills these from a segment's [`SegmentHeader`](crate::SegmentHeader)
/// (`stream`, `boot_id`) and the record's [`RecordBlockRef`](crate::RecordBlockRef)
/// (`cpu_id`, `seq`, `monotonic`). They are the system-attested identity the
/// logical record body does not itself carry, so the renderers show a complete
/// record without a side channel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RecordFrame {
    /// The stream the record belongs to.
    pub stream: Stream,
    /// The boot the record was written in.
    pub boot_id: BootId,
    /// The record's originating CPU id.
    pub cpu_id: u32,
    /// The record's append sequence within its stream.
    pub seq: u64,
    /// The record's monotonic ordering time within the boot (SYSLOG §5.1).
    pub monotonic: Duration64,
}

/// The presentation label of a wall-time trust state.
const fn wall_state_label(state: WallTimeState) -> &'static str {
    match state {
        WallTimeState::Unset => "unset",
        WallTimeState::Firmware => "firmware",
        WallTimeState::Trusted => "trusted",
        WallTimeState::Adjusted => "adjusted",
    }
}

/// The presentation label of an attested trust domain.
const fn trust_domain_label(domain: TrustDomain) -> &'static str {
    match domain {
        TrustDomain::Kernel => "kernel",
        TrustDomain::User => "user",
    }
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// A [`Write`] adapter that escapes text for a JSON string body.
///
/// `"` and `\` are backslash-escaped, the short forms `\n`/`\r`/`\t`/`\b`/`\f`
/// are used where they apply, and every other control character (including
/// `DEL` and the C1 range) is emitted as `\u00xx`. All other characters,
/// including printable multi-byte UTF-8, pass through unchanged. The result is
/// always valid inside a JSON string and free of raw control bytes.
struct JsonEscape<'w, W: Write + ?Sized> {
    inner: &'w mut W,
}

impl<W: Write + ?Sized> Write for JsonEscape<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            match c {
                '"' => self.inner.write_str("\\\"")?,
                '\\' => self.inner.write_str("\\\\")?,
                '\n' => self.inner.write_str("\\n")?,
                '\r' => self.inner.write_str("\\r")?,
                '\t' => self.inner.write_str("\\t")?,
                '\u{08}' => self.inner.write_str("\\b")?,
                '\u{0c}' => self.inner.write_str("\\f")?,
                c if c.is_control() => write!(self.inner, "\\u{:04x}", c as u32)?,
                c => self.inner.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// Write `"text"` as a quoted, JSON-escaped string.
fn json_string<W: Write + ?Sized>(out: &mut W, text: &str) -> fmt::Result {
    out.write_str("\"")?;
    JsonEscape { inner: out }.write_str(text)?;
    out.write_str("\"")
}

/// Write a `"key": "value"` member for an optional caller string, prefixed by
/// `,` so it follows the mandatory `message` member.
fn json_opt<W: Write + ?Sized>(out: &mut W, key: &str, value: Option<&str>) -> fmt::Result {
    if let Some(v) = value {
        out.write_str(",")?;
        json_string(out, key)?;
        out.write_str(":")?;
        json_string(out, v)?;
    }
    Ok(())
}

/// Render a `data.*` value as a JSON value.
///
/// The three shapes JSON represents natively — booleans and 64-bit integers —
/// are emitted as JSON literals; every other typed value is rendered as its
/// canonical string form inside a JSON string, so the value is unambiguous and
/// control-free without inventing a nested schema per type.
fn json_value<W: Write + ?Sized>(out: &mut W, value: &FieldValue<'_>) -> fmt::Result {
    match value {
        FieldValue::Null => out.write_str("null"),
        FieldValue::Bool(b) => out.write_str(if *b { "true" } else { "false" }),
        FieldValue::SignedInt(n) => write!(out, "{n}"),
        FieldValue::UnsignedInt(n) => write!(out, "{n}"),
        other => {
            out.write_str("\"")?;
            write!(JsonEscape { inner: out }, "{other}")?;
            out.write_str("\"")
        }
    }
}

/// Write a `{ "secs": S, "nanos": N }` object for a seconds/nanoseconds pair.
fn json_time<W: Write + ?Sized>(out: &mut W, secs: i64, nanos: u32) -> fmt::Result {
    write!(out, "{{\"secs\":{secs},\"nanos\":{nanos}}}")
}

/// Render one committed record as a single-line JSON object, no trailing
/// newline (the caller joins records for a JSONL export).
///
/// The object has three provenance groups: the top-level system-attested
/// fields, a `"caller"` object holding the caller's own content, and a
/// `"data"` object of the typed `data.*` fields. `data.*` keys obey the
/// [`FieldName`](rustos_abi::FieldName) grammar (lowercase, no control
/// characters), so they are
/// written directly; every caller string value is JSON-escaped.
///
/// # Errors
///
/// Propagates the first [`fmt::Error`] the sink returns.
pub fn render_json<W: Write + ?Sized>(
    out: &mut W,
    frame: &RecordFrame,
    record: &LogRecordRef<'_>,
) -> fmt::Result {
    let mut boot_hex = [0u8; BOOT_ID_HEX_LEN];
    let mut proc_hex = [0u8; PROC_ID_HEX_LEN];
    let origin = record.origin();
    let wall = record.wall();

    write!(out, "{{\"version\":{RECORD_FORMAT_VERSION}")?;
    write!(out, ",\"stream\":")?;
    json_string(out, frame.stream.name())?;
    write!(out, ",\"seq\":{}", frame.seq)?;
    write!(out, ",\"cpu_id\":{}", frame.cpu_id)?;
    write!(out, ",\"cpu_seq\":{}", record.cpu_seq())?;
    write!(out, ",\"boot_id\":")?;
    json_string(out, frame.boot_id.write_hex(&mut boot_hex))?;
    write!(out, ",\"monotonic\":")?;
    json_time(out, frame.monotonic.secs(), frame.monotonic.subsec_nanos())?;
    write!(out, ",\"level\":")?;
    json_string(out, level_label(record.effective_level()))?;
    write!(out, ",\"source\":")?;
    json_string(out, record.source_name())?;

    write!(out, ",\"wall\":{{\"time\":")?;
    json_time(out, wall.time().secs(), wall.time().subsec_nanos())?;
    write!(out, ",\"state\":")?;
    json_string(out, wall_state_label(wall.state()))?;
    write!(out, "}}")?;

    write!(out, ",\"origin\":{{\"trust_domain\":")?;
    json_string(out, trust_domain_label(origin.trust_domain()))?;
    write!(out, ",\"uid\":{}", origin.uid())?;
    write!(out, ",\"gid\":{}", origin.gid())?;
    write!(out, ",\"pid\":{}", origin.pid())?;
    write!(out, ",\"proc_id\":")?;
    json_string(out, origin.proc_id().write_hex(&mut proc_hex))?;
    write!(out, "}}")?;

    let caller = record.caller();
    write!(out, ",\"caller\":{{\"message\":")?;
    json_string(out, caller.message)?;
    json_opt(out, "level", caller.level.map(level_label))?;
    json_opt(out, "component", caller.component)?;
    json_opt(out, "tag", caller.tag)?;
    json_opt(out, "event_id", caller.event_id)?;
    json_opt(out, "requested_source", caller.requested_source)?;
    json_opt(
        out,
        "requested_stream",
        caller.requested_stream.map(Stream::name),
    )?;
    write!(out, "}}")?;

    out.write_str(",\"data\":{")?;
    let mut first = true;
    for (name, value) in record.data() {
        if !first {
            out.write_str(",")?;
        }
        first = false;
        json_string(out, name.as_str())?;
        out.write_str(":")?;
        json_value(out, &value)?;
    }
    out.write_str("}}")
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// Write `"text"`, a double-quoted, control-neutralised rendering of a
/// caller-controlled string.
///
/// Newlines and every other control character are escaped by the shared
/// escaper, so caller text cannot forge a new Markdown line or bullet; the
/// surrounding quotes keep an empty message visible. A backtick passes through
/// as an ordinary character (this is quoted text, not an inline-code span), so
/// there is no span to break.
fn md_quoted<W: Write + ?Sized>(out: &mut W, text: &str) -> fmt::Result {
    out.write_str("\"")?;
    write_escaped(out, text)?;
    out.write_str("\"")
}

/// Render one committed record as a Markdown fragment (a bullet block), no
/// trailing newline on the final line.
///
/// The first line is the system-attested header — sequence, monotonic time,
/// effective level, system-derived source, and the stream/cpu context — and
/// the indented sub-bullets carry the caller's own content and `data.*`
/// fields, so the provenance boundary is visible in the rendered report.
///
/// # Errors
///
/// Propagates the first [`fmt::Error`] the sink returns.
pub fn render_markdown<W: Write + ?Sized>(
    out: &mut W,
    frame: &RecordFrame,
    record: &LogRecordRef<'_>,
) -> fmt::Result {
    let secs = frame.monotonic.secs();
    let millis = frame.monotonic.subsec_nanos() / 1_000_000;
    // The source name is system-derived (a checked dotted-label grammar with no
    // backtick), so an inline-code span is safe here; escape it anyway for one
    // uniform rule.
    write!(
        out,
        "- **seq {}** `[{secs:>3}.{millis:03}]` {} `",
        frame.seq,
        level_label(record.effective_level())
    )?;
    write_escaped(out, record.source_name())?;
    write!(
        out,
        "` (stream={}, cpu={}, cpu_seq={})",
        frame.stream.name(),
        frame.cpu_id,
        record.cpu_seq()
    )?;

    out.write_str("\n  - caller message: ")?;
    md_quoted(out, record.caller().message)?;

    let caller = record.caller();
    if let Some(component) = caller.component {
        out.write_str("\n  - component: ")?;
        md_quoted(out, component)?;
    }
    if let Some(requested) = caller.requested_source {
        out.write_str("\n  - requested source (claim): ")?;
        md_quoted(out, requested)?;
    }
    if let Some(requested) = caller.requested_stream {
        write!(out, "\n  - requested stream (claim): {}", requested.name())?;
    }

    if record.data_count() > 0 {
        out.write_str("\n  - data:")?;
        for (name, value) in record.data() {
            out.write_str(" ")?;
            out.write_str("`")?;
            out.write_str(name.as_str())?;
            out.write_str("=")?;
            write_value(out, &value)?;
            out.write_str("`")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Table
// ---------------------------------------------------------------------------

/// Write the aligned-column header line for the table view, no trailing
/// newline.
///
/// The columns match [`render_table_row`]: sequence, monotonic time, effective
/// level, stream, source, then the message.
///
/// # Errors
///
/// Propagates the first [`fmt::Error`] the sink returns.
pub fn render_table_header<W: Write + ?Sized>(out: &mut W) -> fmt::Result {
    // Bound headings (not string literals) so the same alignment specs as
    // `render_table_row` apply without a `write!`-literal lint.
    let (seq, time, level, stream, source) = ("SEQ", "TIME", "LEVEL", "STREAM", "SOURCE");
    write!(
        out,
        "{seq:>8}  {time:>9}  {level:<8}  {stream:<8}  {source:<24}  MESSAGE"
    )
}

/// Render one committed record as one aligned table row, no trailing newline.
///
/// The source and message are escaped, so a row can never inject a terminal
/// escape or a forged column. Over-wide values are not truncated (that would
/// hide provenance); they push the message column right instead.
///
/// # Errors
///
/// Propagates the first [`fmt::Error`] the sink returns.
pub fn render_table_row<W: Write + ?Sized>(
    out: &mut W,
    frame: &RecordFrame,
    record: &LogRecordRef<'_>,
) -> fmt::Result {
    let secs = frame.monotonic.secs();
    let millis = frame.monotonic.subsec_nanos() / 1_000_000;
    write!(
        out,
        "{:>8}  {secs:>5}.{millis:03}  {:<8}  {:<8}  {:<24}  ",
        frame.seq,
        level_label(record.effective_level()),
        frame.stream.name(),
        record.source_name(),
    )?;
    write_escaped(out, record.caller().message)
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use alloc::string::String;

    use rustos_abi::{
        BootId, CapabilitySummary, Duration64, FieldName, FieldValue, Origin, ProcId, Time64,
        TrustDomain, WallClockReading, WallTimeState, BOOT_ID_LEN, ORIGIN_CONSOLE_NONE,
    };

    use super::{render_json, render_markdown, render_table_header, render_table_row, RecordFrame};
    use crate::dict::{DictionaryBuilder, DictionaryView};
    use crate::record::{decode as decode_record, CallerContent, LogRecord, LogRecordRef};
    use crate::stream::Stream;
    use crate::Level;

    fn origin() -> Origin {
        Origin::new(
            TrustDomain::User,
            1000,
            1000,
            42,
            ProcId::from_raw([7u8; 16]),
            CapabilitySummary::from_raw([0u8; 32]),
            ORIGIN_CONSOLE_NONE,
        )
    }

    fn frame() -> RecordFrame {
        RecordFrame {
            stream: Stream::Runtime,
            boot_id: BootId::from_raw([0xAB; BOOT_ID_LEN]),
            cpu_id: 0,
            seq: 12,
            monotonic: Duration64::from_nanos(64_000_000),
        }
    }

    fn base<'a>(source: &'a str, level: Level, message: &'a str) -> LogRecord<'a> {
        LogRecord {
            effective_level: level,
            cpu_seq: 3,
            wall: WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
            origin: origin(),
            source_name: source,
            caller: CallerContent {
                level: None,
                component: None,
                tag: None,
                event_id: None,
                requested_source: None,
                requested_stream: None,
                message,
            },
            data: &[],
        }
    }

    fn with<R>(record: &LogRecord<'_>, f: impl FnOnce(&LogRecordRef<'_>) -> R) -> R {
        let mut buf = [0u8; 4096];
        let len = record
            .encode(&mut buf, &mut DictionaryBuilder::new())
            .expect("encodes");
        let mut view = DictionaryView::new();
        let decoded = decode_record(&buf[..len], &mut view).expect("decodes");
        f(&decoded)
    }

    fn json(record: &LogRecord<'_>) -> String {
        with(record, |r| {
            let mut out = String::new();
            render_json(&mut out, &frame(), r).expect("renders");
            out
        })
    }

    #[test]
    fn json_carries_provenance_groups() {
        let out = json(&base("kernel.mem", Level::Info, "started"));
        assert!(out.contains("\"stream\":\"runtime\""), "{out}");
        assert!(out.contains("\"source\":\"kernel.mem\""), "{out}");
        assert!(out.contains("\"seq\":12"), "{out}");
        assert!(out.contains("\"cpu_seq\":3"), "{out}");
        assert!(out.contains("\"level\":\"info\""), "{out}");
        assert!(out.contains("\"state\":\"trusted\""), "{out}");
        assert!(out.contains("\"uid\":1000"), "{out}");
        assert!(
            out.contains("\"caller\":{\"message\":\"started\"}"),
            "{out}"
        );
        assert!(out.contains("\"data\":{}"), "{out}");
    }

    #[test]
    fn json_shows_a_requested_source_as_an_inert_caller_claim() {
        let mut record = base("user.1000.proc.0707", Level::Warn, "audit disabled");
        record.caller.requested_source = Some("kernel.audit");
        let out = json(&record);
        // The real source is the user source; the privileged request lives
        // only inside the caller object.
        assert!(out.contains("\"source\":\"user.1000.proc.0707\""), "{out}");
        assert!(
            out.contains("\"requested_source\":\"kernel.audit\""),
            "{out}"
        );
    }

    #[test]
    fn json_escapes_control_bytes_and_quotes_in_caller_text() {
        let hostile = "a\"b\\c\nd\u{1b}e";
        let record = base("user.1000.proc.0707", Level::Error, hostile);
        let out = json(&record);
        for b in out.bytes() {
            assert!(b >= 0x20 && b != 0x7f, "raw control byte {b:#04x}: {out}");
        }
        assert!(out.contains("a\\\"b\\\\c\\nd\\u001be"), "{out}");
    }

    #[test]
    fn json_types_scalar_data_values() {
        let data = [
            (FieldName::new("ok").unwrap(), FieldValue::Bool(true)),
            (FieldName::new("n").unwrap(), FieldValue::SignedInt(-7)),
            (FieldName::new("u").unwrap(), FieldValue::UnsignedInt(9)),
            (
                FieldName::new("path").unwrap(),
                FieldValue::Str("/Storage/a"),
            ),
        ];
        let mut record = base("kernel.net", Level::Info, "summary");
        record.data = &data;
        let out = json(&record);
        assert!(out.contains("\"ok\":true"), "{out}");
        assert!(out.contains("\"n\":-7"), "{out}");
        assert!(out.contains("\"u\":9"), "{out}");
        assert!(out.contains("\"path\":\"/Storage/a\""), "{out}");
    }

    #[test]
    fn markdown_separates_attested_header_from_caller_content() {
        let mut record = base("service.backup", Level::Warn, "file skipped");
        record.caller.component = Some("scanner");
        let out = with(&record, |r| {
            let mut s = String::new();
            render_markdown(&mut s, &frame(), r).expect("renders");
            s
        });
        assert!(out.starts_with("- **seq 12** `[  0.064]` warn "), "{out}");
        assert!(out.contains("`service.backup`"), "{out}");
        assert!(out.contains("stream=runtime"), "{out}");
        assert!(
            out.contains("\n  - caller message: \"file skipped\""),
            "{out}"
        );
        assert!(out.contains("\n  - component: \"scanner\""), "{out}");
    }

    #[test]
    fn table_row_is_control_free_even_for_hostile_text() {
        let record = base("user.1000.proc.0707", Level::Error, "x\u{1b}[2J\ny");
        let out = with(&record, |r| {
            let mut s = String::new();
            render_table_row(&mut s, &frame(), r).expect("renders");
            s
        });
        for b in out.bytes() {
            assert!(b >= 0x20 && b != 0x7f, "control byte {b:#04x}: {out:?}");
        }
        assert!(out.contains("runtime"), "{out}");
        assert!(out.contains("user.1000.proc.0707"), "{out}");
    }

    #[test]
    fn table_header_has_the_expected_columns() {
        let mut out = String::new();
        render_table_header(&mut out).expect("renders");
        assert!(out.contains("SEQ"), "{out}");
        assert!(out.contains("LEVEL"), "{out}");
        assert!(out.contains("STREAM"), "{out}");
        assert!(out.contains("SOURCE"), "{out}");
        assert!(out.contains("MESSAGE"), "{out}");
    }
}
