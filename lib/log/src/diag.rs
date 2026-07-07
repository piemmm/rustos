//! The shared boot-console **diagnostic** line format.
//!
//! Every architecture port's serial/console [`Sink`](crate::Sink) renders
//! the same canonical diagnostic line, so the format lives here once and the
//! ports only supply the byte transport:
//!
//! ```text
//! [<secs>.<millis>] [<LEVEL>] id=<id> <message> <key>=<value> ...
//! ```
//!
//! * The leading `[<secs>.<millis>]` is the port's monotonic uptime,
//!   rendered as seconds with a three-digit millisecond fraction (the same
//!   shape as the journal renderer, [`crate::render_line`]). A port with no
//!   uptime source passes [`None`] and the stamp is omitted.
//! * `[<LEVEL>]` is the compact uppercase tag ([`level_tag`]), wrapped in an
//!   ANSI SGR colour when the sink's transport renders escape sequences
//!   (`colored`): a terminal or serial capture colours the tag, a host
//!   console that prints escapes literally (the wasm32 host) stays plain.
//! * Consumers (the QEMU scrapers) match on the `id=<id>` token, never the
//!   line start, so neither the stamp nor the colour is load-bearing.
//! * Event text is escaped exactly as the journal boot renderer escapes it
//!   (SYSLOG §8.2, [`crate::render_line`]): control characters and
//!   backslashes in the message and in every field key and string value are
//!   neutralised, so logged text — even a user-influenced path in a field —
//!   can never move the cursor, change colour, or forge a line. The only
//!   escape bytes on the wire are the system-applied level-tag colours.
//!
//! Write errors are ignored: the sinks are infallible MMIO/RAM writes and
//! the logging path must not panic.

use core::fmt::Write;

use crate::render::{write_escaped, write_value};
use crate::{Event, Level};

/// The compact uppercase tag of a level, as the diagnostic consoles print
/// it. Deliberately distinct from the journal renderer's lowercase level
/// words ([`crate::render_line`]); each has exactly one definition.
#[must_use]
pub const fn level_tag(level: Level) -> &'static str {
    match level {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
        Level::Critical => "CRIT",
    }
}

/// The ANSI SGR sequence that colours a level's tag: severity reads at a
/// glance — calm colours for routine levels, red for the failing ones.
const fn level_color(level: Level) -> &'static str {
    match level {
        Level::Trace => "\x1b[90m",
        Level::Debug => "\x1b[36m",
        Level::Info => "\x1b[32m",
        Level::Warn => "\x1b[33m",
        Level::Error => "\x1b[31m",
        Level::Critical => "\x1b[1;31m",
    }
}

/// The SGR reset that closes a coloured tag.
const SGR_RESET: &str = "\x1b[0m";

/// Format one diagnostic event into `w` in the canonical line shape,
/// with a trailing newline.
///
/// `uptime_ms` is the port's monotonic uptime in milliseconds ([`None`]
/// omits the stamp); `colored` selects the ANSI-coloured level tag.
pub fn write_diag_line<W: Write + ?Sized>(
    w: &mut W,
    uptime_ms: Option<u64>,
    colored: bool,
    event: &Event<'_>,
) {
    if let Some(ms) = uptime_ms {
        let secs = ms / 1_000;
        let millis = ms % 1_000;
        let _ = write!(w, "[{secs:>3}.{millis:03}] ");
    }
    let tag = level_tag(event.level);
    if colored {
        let _ = write!(w, "[{}{}{}]", level_color(event.level), tag, SGR_RESET);
    } else {
        let _ = write!(w, "[{tag}]");
    }
    // One uniform escaping rule for every rendered string (the same one the
    // journal boot renderer applies): logged text never reaches the console
    // raw, so it cannot inject terminal escapes or forge a line.
    let _ = write!(w, " id={} ", event.id.0);
    let _ = write_escaped(w, event.message);
    for field in event.fields {
        let _ = w.write_str(" ");
        let _ = write_escaped(w, field.key);
        let _ = w.write_str("=");
        let _ = write_value(w, &field.value);
    }
    let _ = writeln!(w);
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::string::String;

    use rustos_abi::FieldValue;

    use super::{level_tag, write_diag_line};
    use crate::{Event, EventId, Field, Level};

    fn event(level: Level) -> Event<'static> {
        Event {
            level,
            id: EventId(4242),
            message: "unit started",
            fields: &[],
        }
    }

    fn render(uptime_ms: Option<u64>, colored: bool, event: &Event<'_>) -> String {
        let mut out = String::new();
        write_diag_line(&mut out, uptime_ms, colored, event);
        out
    }

    #[test]
    fn renders_the_uptime_as_seconds_with_millisecond_fraction() {
        let line = render(Some(12_345), false, &event(Level::Info));
        assert_eq!(line, "[ 12.345] [INFO] id=4242 unit started\n");
    }

    #[test]
    fn omits_the_stamp_without_an_uptime_source() {
        let line = render(None, false, &event(Level::Info));
        assert_eq!(line, "[INFO] id=4242 unit started\n");
    }

    #[test]
    fn level_tags_are_the_compact_uppercase_set() {
        // `WARN`, never `WARNING`; `CRIT`, never `CRITICAL`.
        assert_eq!(level_tag(Level::Trace), "TRACE");
        assert_eq!(level_tag(Level::Debug), "DEBUG");
        assert_eq!(level_tag(Level::Info), "INFO");
        assert_eq!(level_tag(Level::Warn), "WARN");
        assert_eq!(level_tag(Level::Error), "ERROR");
        assert_eq!(level_tag(Level::Critical), "CRIT");
    }

    #[test]
    fn a_colored_tag_is_wrapped_in_sgr_and_reset() {
        let line = render(Some(0), true, &event(Level::Warn));
        assert_eq!(
            line,
            "[  0.000] [\x1b[33mWARN\x1b[0m] id=4242 unit started\n"
        );
    }

    #[test]
    fn each_level_gets_its_own_color() {
        let mut seen = alloc::vec::Vec::new();
        for level in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
            Level::Critical,
        ] {
            let line = render(None, true, &event(level));
            let color = line
                .split_once("[\x1b[")
                .map_or("", |(_, rest)| rest.split('m').next().unwrap_or(""));
            assert!(!color.is_empty(), "SGR colour present: {line:?}");
            assert!(!seen.contains(&String::from(color)), "distinct: {line:?}");
            seen.push(String::from(color));
        }
    }

    #[test]
    fn fields_render_after_the_message() {
        let fields = [Field {
            key: "cpu",
            value: FieldValue::UnsignedInt(2),
        }];
        let ev = Event {
            level: Level::Debug,
            id: EventId(7),
            message: "tick",
            fields: &fields,
        };
        let line = render(None, false, &ev);
        assert_eq!(line, "[DEBUG] id=7 tick cpu=2\n");
    }

    #[test]
    fn control_characters_in_the_message_are_neutralised() {
        // Terminal-escape injection, a newline, and a backslash in the
        // message must all be rendered inertly, never raw.
        let ev = Event {
            level: Level::Error,
            id: EventId(9),
            message: "red\u{1b}[31m\nfake\\x",
            fields: &[],
        };
        let line = render(None, false, &ev);
        for b in line.bytes() {
            assert!(
                b == b'\n' && line.ends_with('\n') || b >= 0x20 && b != 0x7f,
                "raw control byte {b:#04x} in {line:?}"
            );
        }
        assert_eq!(line, "[ERROR] id=9 red\\x1b[31m\\x0afake\\\\x\n");
    }

    #[test]
    fn control_characters_in_a_string_field_value_are_neutralised() {
        let fields = [Field {
            key: "path",
            value: FieldValue::Str("/a\u{1b}[2Jb"),
        }];
        let ev = Event {
            level: Level::Warn,
            id: EventId(3),
            message: "skipped",
            fields: &fields,
        };
        let line = render(None, true, &ev);
        // The only raw ESC bytes are the system-applied SGR colour around
        // the level tag; the field's ESC renders as the inert `\x1b` text.
        assert_eq!(
            line,
            "[\x1b[33mWARN\x1b[0m] id=3 skipped path=/a\\x1b[2Jb\n"
        );
    }

    #[test]
    fn the_stamp_matches_the_journal_renderer_column_shape() {
        // Sub-second and >999-second stamps keep the three-digit fraction.
        assert!(render(Some(64), false, &event(Level::Info)).starts_with("[  0.064] "));
        assert!(render(Some(1_234_567), false, &event(Level::Info)).starts_with("[1234.567] "));
    }
}
