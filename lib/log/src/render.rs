//! Boot-console rendering of committed log records (SYSLOG §8.2).
//!
//! [`render_line`] turns one decoded [`LogRecordRef`] plus its container-owned
//! monotonic time into a single readable line of the canonical shape:
//!
//! ```text
//! [monotonic] level source[component]: message key=value key=value
//! ```
//!
//! It needs no event templates and no registry: the source name, level, and
//! fields are already on the record. Two properties make it safe to point at a
//! real terminal:
//!
//! * **Caller text can never forge output.** The message, component, and the
//!   caller's *requested* source are attacker-controlled, as are string
//!   [`FieldValue`]s. Every one of them is passed through an escaping writer
//!   that neutralises control characters (newline, carriage return, `ESC`,
//!   `NUL`, `DEL`, the C1 range, …) and the backslash, so caller content cannot
//!   move the cursor, change colour, clear the screen, forge a prefix, or split
//!   itself across lines. The line the renderer emits is therefore free of
//!   control bytes regardless of its input.
//!
//! * **Provenance is preserved, not obeyed.** The line is headed by the
//!   *system-derived* [`source_name`](LogRecordRef::source_name); a caller that
//!   *requested* a privileged source (a spoof attempt the ingress path already
//!   downgraded) has that request shown inertly as `requested_source=…`
//!   evidence, never as the real source. A user record labelled `critical`
//!   still renders under its true user source.
//!
//! The renderer is `no_std` and allocation-free: it writes into any
//! [`core::fmt::Write`] sink (a serial ring, a framebuffer console, a bounded
//! byte buffer), and clean runs of text are written in bulk rather than a byte
//! at a time.

use core::fmt::{self, Write};

use rustos_abi::{Duration64, FieldValue};

use crate::record::LogRecordRef;
use crate::Level;

/// The lowercase canonical name of a level (SYSLOG §4.3).
///
/// This is the boot renderer's presentation of a level. It is deliberately
/// distinct from the compact uppercase abbreviations the architecture serial
/// consoles print for the diagnostic log path; there is only ever one lowercase
/// definition, and this is it.
const fn level_label(level: Level) -> &'static str {
    match level {
        Level::Trace => "trace",
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warn => "warn",
        Level::Error => "error",
        Level::Critical => "critical",
    }
}

/// A [`Write`] adapter that neutralises control characters and the backslash.
///
/// Every character that could let attacker-controlled text escape its field —
/// a C0 control (including newline and `ESC`), `DEL`, or a C1 control — is
/// rendered as the visible escape `\xNN` (two lowercase hex digits; every such
/// code point is `<= 0x9F`). The backslash itself is doubled so the escaping is
/// unambiguous. All other characters, including printable multi-byte UTF-8,
/// pass through unchanged, and unbroken runs of them are forwarded in one call.
struct EscapeWriter<'w, W: Write + ?Sized> {
    inner: &'w mut W,
}

impl<W: Write + ?Sized> Write for EscapeWriter<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut clean = 0usize;
        for (i, c) in s.char_indices() {
            if c == '\\' || c.is_control() {
                if clean < i {
                    self.inner.write_str(&s[clean..i])?;
                }
                if c == '\\' {
                    self.inner.write_str("\\\\")?;
                } else {
                    // `is_control()` code points are all `<= 0x9F`, so two hex
                    // digits always suffice.
                    write!(self.inner, "\\x{:02x}", c as u32)?;
                }
                clean = i + c.len_utf8();
            }
        }
        if clean < s.len() {
            self.inner.write_str(&s[clean..])?;
        }
        Ok(())
    }
}

/// Write `text` to `out` with control characters and backslashes neutralised.
fn write_escaped<W: Write + ?Sized>(out: &mut W, text: &str) -> fmt::Result {
    EscapeWriter { inner: out }.write_str(text)
}

/// Render `value` to `out`, neutralising control characters in any string it
/// contains (a `Str`, or a `Str` inside a `List`); numeric, address, hex, and
/// enumerated renderings are already control-free.
fn write_value<W: Write + ?Sized>(out: &mut W, value: &FieldValue<'_>) -> fmt::Result {
    write!(EscapeWriter { inner: out }, "{value}")
}

/// Render one committed record as a readable boot-console line, without a
/// trailing newline (the caller decides the line terminator for its sink).
///
/// `monotonic` is the record's container-owned ordering time within the boot
/// (SYSLOG §5.1); pair it with the [`LogRecordRef`] decoded from the record
/// body. The emitted line is the canonical
/// `[monotonic] level source[component]: message key=value …`, with a caller's
/// downgraded `requested_source` shown as inert evidence before the colon when
/// present. All caller-controlled text is escaped (see the module docs), so the
/// returned line never contains a control byte.
///
/// # Errors
///
/// Propagates the first [`fmt::Error`] the sink returns (for example a bounded
/// byte buffer that fills). Nothing else can fail.
pub fn render_line<W: Write + ?Sized>(
    out: &mut W,
    monotonic: Duration64,
    record: &LogRecordRef<'_>,
) -> fmt::Result {
    let secs = monotonic.secs();
    let millis = monotonic.subsec_nanos() / 1_000_000;
    write!(out, "[{secs:>3}.{millis:03}] ")?;
    write!(out, "{:<5} ", level_label(record.effective_level()))?;

    // The source name is system-derived (a checked grammar), but escape it too:
    // one uniform rule for every rendered string is cheaper to trust than a
    // per-field judgement about which strings are safe.
    write_escaped(out, record.source_name())?;

    let caller = record.caller();
    if let Some(component) = caller.component {
        out.write_str("[")?;
        write_escaped(out, component)?;
        out.write_str("]")?;
    }
    if let Some(requested) = caller.requested_source {
        out.write_str(" requested_source=")?;
        write_escaped(out, requested)?;
    }

    out.write_str(": ")?;
    write_escaped(out, caller.message)?;

    for (name, value) in record.data() {
        // A `data.*` key obeys the `[a-z][a-z0-9_]{0,63}` grammar, so it carries
        // no control characters and is written directly.
        out.write_str(" ")?;
        out.write_str(name.as_str())?;
        out.write_str("=")?;
        write_value(out, &value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use alloc::string::String;

    use rustos_abi::{
        CapabilitySummary, Duration64, FieldName, FieldValue, IpAddr, Origin, ProcId, TrustDomain,
        WallClockReading,
    };

    use super::render_line;
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
        )
    }

    // Encode `record` and render the decoded view at `monotonic`, returning the
    // rendered line. The encode/decode round-trip mirrors the real path: the
    // renderer only ever sees a validated `LogRecordRef`.
    fn render(record: &LogRecord<'_>, monotonic: Duration64) -> String {
        let mut buf = [0u8; 4096];
        let len = record
            .encode(&mut buf, &mut DictionaryBuilder::new())
            .expect("encodes");
        let mut view = DictionaryView::new();
        let decoded: LogRecordRef<'_> = decode_record(&buf[..len], &mut view).expect("decodes");
        let mut out = String::new();
        render_line(&mut out, monotonic, &decoded).expect("renders");
        out
    }

    fn base<'a>(source: &'a str, level: Level, message: &'a str) -> LogRecord<'a> {
        LogRecord {
            effective_level: level,
            cpu_seq: 0,
            wall: WallClockReading::default(),
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

    #[test]
    fn renders_the_canonical_minimal_line() {
        let record = base("kernel.mem", Level::Info, "started");
        let line = render(&record, Duration64::from_nanos(64_000_000));
        assert_eq!(line, "[  0.064] info  kernel.mem: started");
    }

    #[test]
    fn renders_a_component_and_data_fields() {
        let data = [
            (
                FieldName::new("path").unwrap(),
                FieldValue::Str("/Storage/a"),
            ),
            (
                FieldName::new("errno").unwrap(),
                FieldValue::Error(rustos_abi::Errno::PermissionDenied),
            ),
        ];
        let mut record = base("service.backup", Level::Warn, "file skipped");
        record.caller.component = Some("scanner");
        record.data = &data;
        let line = render(&record, Duration64::from_nanos(12_440_000_000));
        assert!(
            line.starts_with("[ 12.440] warn  service.backup[scanner]: file skipped "),
            "unexpected header: {line}"
        );
        assert!(line.contains(" path=/Storage/a"), "path field: {line}");
        assert!(line.contains(" errno="), "errno field: {line}");
    }

    #[test]
    fn a_requested_source_is_shown_as_inert_evidence_not_the_real_source() {
        // A user process that requested the privileged `kernel.audit` source is
        // rendered under its true user source, with the request preserved as
        // evidence — never as a kernel audit line.
        let mut record = base("user.1000.proc.0707", Level::Warn, "audit disabled");
        record.caller.requested_source = Some("kernel.audit");
        let line = render(&record, Duration64::from_nanos(44_120_000_000));
        assert_eq!(
            line,
            "[ 44.120] warn  user.1000.proc.0707 requested_source=kernel.audit: audit disabled"
        );
    }

    #[test]
    fn control_characters_in_caller_text_are_neutralised() {
        // A message packed with terminal-escape injection, a newline, a
        // carriage return, and a backslash. None may reach the output raw.
        let hostile = "red\u{1b}[31m\nfake line\rroot# \\x";
        let record = base("user.1000.proc.0707", Level::Error, hostile);
        let line = render(&record, Duration64::ZERO);
        for b in line.bytes() {
            assert!(
                b >= 0x20 && b != 0x7f,
                "rendered line contains a raw control byte {b:#04x}: {line:?}"
            );
        }
        assert!(line.contains("\\x1b"), "ESC escaped: {line:?}");
        assert!(line.contains("\\x0a"), "newline escaped: {line:?}");
        assert!(line.contains("\\x0d"), "carriage return escaped: {line:?}");
        assert!(line.contains("\\\\x"), "backslash doubled: {line:?}");
    }

    #[test]
    fn control_characters_inside_a_string_field_value_are_neutralised() {
        let data = [(
            FieldName::new("input").unwrap(),
            FieldValue::Str("a\u{1b}b\nc"),
        )];
        let mut record = base("user.1000.proc.0707", Level::Info, "got input");
        record.data = &data;
        let line = render(&record, Duration64::ZERO);
        for b in line.bytes() {
            assert!(
                b >= 0x20 && b != 0x7f,
                "field value leaked a control byte {b:#04x}: {line:?}"
            );
        }
        assert!(
            line.contains("input=a\\x1bb\\x0ac"),
            "field escaped: {line:?}"
        );
    }

    #[test]
    fn a_critical_user_record_is_not_dressed_up_as_a_system_line() {
        // The caller labelled itself `critical`, but the authoritative source
        // is still the user process; the effective level drives the rendered
        // level word.
        let mut record = base("user.1000.proc.0707", Level::Info, "totally fine");
        record.caller.level = Some(Level::Critical);
        let line = render(&record, Duration64::ZERO);
        assert!(line.starts_with("[  0.000] info  user.1000.proc.0707:"));
        assert!(!line.contains("critical"));
    }

    #[test]
    fn renders_representative_value_types_without_control_bytes() {
        let data = [
            (
                FieldName::new("ip").unwrap(),
                FieldValue::Ip(IpAddr::V4([10, 0, 0, 1])),
            ),
            (FieldName::new("ok").unwrap(), FieldValue::Bool(true)),
            (FieldName::new("n").unwrap(), FieldValue::SignedInt(-7)),
            (
                FieldName::new("raw").unwrap(),
                FieldValue::Bytes(&[0x00, 0x1b, 0xff]),
            ),
        ];
        let mut record = base("kernel.net", Level::Info, "summary");
        record.data = &data;
        let line = render(&record, Duration64::ZERO);
        for b in line.bytes() {
            assert!(b >= 0x20 && b != 0x7f, "control byte in {line:?}");
        }
        assert!(line.contains(" ip=10.0.0.1"), "{line}");
        assert!(line.contains(" ok=true"), "{line}");
        assert!(line.contains(" n=-7"), "{line}");
        // Bytes render as hex (the raw 0x1b never appears as an escape byte).
        assert!(line.contains(" raw=001bff"), "{line}");
    }

    #[test]
    fn a_stream_is_never_named_but_the_requested_stream_does_not_leak() {
        // A record that requested a privileged stream still renders normally;
        // the boot line is about source/level/message, not the stream claim.
        let mut record = base("user.1000.proc.0707", Level::Info, "hi");
        record.caller.requested_stream = Some(Stream::Audit);
        let line = render(&record, Duration64::ZERO);
        assert_eq!(line, "[  0.000] info  user.1000.proc.0707: hi");
    }
}
