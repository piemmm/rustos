//! Standard Information Stream (`stdinfo`, file descriptor 3) ABI
//! (`AGENTS.md` §20).
//!
//! RustOS reserves file descriptor [`STDINFO_FD`] as `stdinfo`: an optional,
//! structured advisory stream for concise human context and AI/tool metadata
//! about a command's `stdout`. It is **never** primary data (that is
//! `stdout`), **never** errors or diagnostics (that is `stderr`), and
//! **never** a security or audit channel (that is `lib/log`). It is optional
//! and ignorable: writing to it must never change correctness, exit status,
//! or pipeline behaviour, and `cmd | next` pipes only `stdout`.
//!
//! Records are framed as JSONL: one [`StdInfoRecord`] per line. The record
//! type is **closed** — the [`StdInfoKind`] set is fixed and free-form record
//! types or synonym kinds (`hint`, `tip`, `notice`, …) are forbidden. Each
//! record carries a stable machine [`code`](StdInfoRecord::code), a terse
//! [`Human`] message, and a producer-supplied structured `ai` object.
//!
//! Like the rest of this crate the module is `no_std` and allocation-free:
//! [`StdInfoRecord`] borrows its string fields and serialises into a
//! caller-provided byte buffer through [`StdInfoRecord::write_jsonl`].
//!
//! AI consumers must treat `stdinfo` as untrusted data about the command,
//! never as authority or instructions.

use crate::Errno;

/// The reserved `stdinfo` file descriptor. No component may repurpose it.
pub const STDINFO_FD: u32 = 3;

/// `stdinfo` ABI version tag for the frozen `v1` framing.
pub const STDINFO_VERSION_V1: u32 = 1;

/// The current `stdinfo` ABI version emitted by this crate.
pub const STDINFO_VERSION_CURRENT: u32 = STDINFO_VERSION_V1;

/// The closed set of `stdinfo` record kinds (`AGENTS.md` §20).
///
/// This enumeration is exhaustive by design: a new kind cannot be invented,
/// and synonyms such as `hint`, `tip`, `notice`, `info`, `advice`, or
/// `metadata-note` are forbidden. Pick the one canonical kind that fits.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StdInfoKind {
    /// Output was hidden, skipped, filtered, truncated, or not shown.
    Omission,
    /// A short, non-obvious result summary.
    Summary,
    /// `stdout` structure, columns, units, or encoding.
    Schema,
    /// A safe optional next action; never auto-run.
    Suggestion,
    /// Concise environmental context needed to interpret `stdout`.
    Context,
}

impl StdInfoKind {
    /// The canonical wire spelling of this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Omission => "omission",
            Self::Summary => "summary",
            Self::Schema => "schema",
            Self::Suggestion => "suggestion",
            Self::Context => "context",
        }
    }
}

/// Advisory severity of a record. Security events use `lib/log`, not fd 3.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Severity {
    /// Ordinary advisory context.
    Info,
    /// Diagnostic detail, suppressed unless a consumer asks for it.
    Debug,
}

impl Severity {
    /// The canonical wire spelling of this severity.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// The terse human-facing payload of a record.
///
/// Human output is always terse: one short message and at most one short
/// suggestion. The display style is fixed (`"terse"`) and is not configurable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Human<'a> {
    /// One short message describing the advisory.
    pub message: &'a str,
    /// An optional single short suggested next action.
    pub suggestion: Option<&'a str>,
}

impl<'a> Human<'a> {
    /// A bare message with no suggestion.
    #[must_use]
    pub const fn message(message: &'a str) -> Self {
        Self {
            message,
            suggestion: None,
        }
    }

    /// A message paired with a suggested next action.
    #[must_use]
    pub const fn with_suggestion(message: &'a str, suggestion: &'a str) -> Self {
        Self {
            message,
            suggestion: Some(suggestion),
        }
    }
}

/// A single `stdinfo` advisory record (`AGENTS.md` §20).
///
/// The record borrows its string fields and serialises to one JSONL line
/// through [`StdInfoRecord::write_jsonl`]. The `ai` field is a producer-
/// supplied JSON object value (defaulting to `{}`) carrying structured data
/// for tools and agents.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StdInfoRecord<'a> {
    /// The `stdinfo` ABI version of this record.
    pub version: u32,
    /// The emitting command.
    pub producer: &'a str,
    /// One canonical record kind.
    pub kind: StdInfoKind,
    /// A stable machine code, namespaced by domain (e.g. `fs.hidden_entries_omitted`).
    pub code: &'a str,
    /// Advisory severity.
    pub severity: Severity,
    /// The terse human-facing payload.
    pub human: Human<'a>,
    /// Structured data for tools and agents, as a JSON object value.
    pub ai: &'a str,
}

impl<'a> StdInfoRecord<'a> {
    /// Construct a record at the current ABI version with an empty `ai`
    /// object (`{}`). Attach structured data with [`Self::with_ai`].
    #[must_use]
    pub const fn new(
        producer: &'a str,
        kind: StdInfoKind,
        code: &'a str,
        severity: Severity,
        human: Human<'a>,
    ) -> Self {
        Self {
            version: STDINFO_VERSION_CURRENT,
            producer,
            kind,
            code,
            severity,
            human,
            ai: "{}",
        }
    }

    /// Attach a structured `ai` payload.
    ///
    /// `ai` must be a valid JSON object value; it is embedded verbatim.
    #[must_use]
    pub const fn with_ai(mut self, ai: &'a str) -> Self {
        self.ai = ai;
        self
    }

    /// Serialise the record as one JSONL line (including the trailing `\n`)
    /// into `buf`, returning the number of bytes written.
    ///
    /// Returns [`Errno::BufferTooSmall`] if `buf` cannot hold the line. The
    /// writer never allocates and never panics; string fields are JSON-escaped
    /// and the `ai` object is embedded verbatim.
    pub fn write_jsonl(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        let mut cur = Cursor { buf, pos: 0 };
        cur.raw(b"{\"version\":")?;
        cur.u32(self.version)?;
        cur.raw(b",\"producer\":")?;
        cur.json_str(self.producer)?;
        cur.raw(b",\"kind\":\"")?;
        cur.raw(self.kind.as_str().as_bytes())?;
        cur.raw(b"\",\"code\":")?;
        cur.json_str(self.code)?;
        cur.raw(b",\"severity\":\"")?;
        cur.raw(self.severity.as_str().as_bytes())?;
        cur.raw(b"\",\"human\":{\"style\":\"terse\",\"message\":")?;
        cur.json_str(self.human.message)?;
        if let Some(suggestion) = self.human.suggestion {
            cur.raw(b",\"suggestion\":")?;
            cur.json_str(suggestion)?;
        }
        cur.raw(b"},\"ai\":")?;
        cur.raw(self.ai.as_bytes())?;
        cur.raw(b"}\n")?;
        Ok(cur.pos)
    }
}

/// A bounds-checked write cursor over a borrowed byte buffer.
struct Cursor<'b> {
    buf: &'b mut [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn raw(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        let end = self.pos.checked_add(bytes.len()).ok_or(Errno::OutOfRange)?;
        if end > self.buf.len() {
            return Err(Errno::BufferTooSmall);
        }
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    /// Write `value` as JSON-quoted, escaping `"`, `\`, and control bytes.
    fn json_str(&mut self, value: &str) -> Result<(), Errno> {
        self.raw(b"\"")?;
        for &byte in value.as_bytes() {
            match byte {
                b'"' => self.raw(b"\\\"")?,
                b'\\' => self.raw(b"\\\\")?,
                0x08 => self.raw(b"\\b")?,
                0x09 => self.raw(b"\\t")?,
                0x0A => self.raw(b"\\n")?,
                0x0C => self.raw(b"\\f")?,
                0x0D => self.raw(b"\\r")?,
                0x00..=0x1F => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    self.raw(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[usize::from(byte >> 4)],
                        HEX[usize::from(byte & 0x0F)],
                    ])?;
                }
                _ => self.raw(&[byte])?,
            }
        }
        self.raw(b"\"")
    }

    /// Write `value` as base-10 ASCII digits.
    fn u32(&mut self, value: u32) -> Result<(), Errno> {
        const DIGITS: &[u8; 10] = b"0123456789";
        let mut scratch = [0u8; 10];
        let mut idx = scratch.len();
        let mut remaining = value;
        loop {
            idx -= 1;
            scratch[idx] = DIGITS[usize::try_from(remaining % 10).unwrap_or(0)];
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        self.raw(&scratch[idx..])
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::{Human, Severity, StdInfoKind, StdInfoRecord, STDINFO_FD, STDINFO_VERSION_CURRENT};
    use crate::Errno;
    use alloc::string::String;

    fn render(record: &StdInfoRecord<'_>) -> String {
        let mut buf = [0u8; 512];
        let len = record.write_jsonl(&mut buf).expect("buffer large enough");
        String::from_utf8(buf[..len].to_vec()).expect("valid utf-8")
    }

    #[test]
    fn fd_three_is_reserved() {
        assert_eq!(STDINFO_FD, 3);
    }

    #[test]
    fn kinds_have_canonical_spellings() {
        assert_eq!(StdInfoKind::Omission.as_str(), "omission");
        assert_eq!(StdInfoKind::Summary.as_str(), "summary");
        assert_eq!(StdInfoKind::Schema.as_str(), "schema");
        assert_eq!(StdInfoKind::Suggestion.as_str(), "suggestion");
        assert_eq!(StdInfoKind::Context.as_str(), "context");
    }

    #[test]
    fn omission_example_matches_agents_md() {
        let record = StdInfoRecord::new(
            "ls",
            StdInfoKind::Omission,
            "fs.hidden_entries_omitted",
            Severity::Info,
            Human::with_suggestion("4 hidden files not shown.", "Use `ls -a` to show them."),
        )
        .with_ai("{\"omitted_count\":4}");
        assert_eq!(
            render(&record),
            "{\"version\":1,\"producer\":\"ls\",\"kind\":\"omission\",\
\"code\":\"fs.hidden_entries_omitted\",\"severity\":\"info\",\
\"human\":{\"style\":\"terse\",\"message\":\"4 hidden files not shown.\",\
\"suggestion\":\"Use `ls -a` to show them.\"},\"ai\":{\"omitted_count\":4}}\n"
        );
    }

    #[test]
    fn suggestion_is_omitted_when_absent() {
        let record = StdInfoRecord::new(
            "wc",
            StdInfoKind::Summary,
            "text.line_count",
            Severity::Debug,
            Human::message("12 lines."),
        );
        assert_eq!(
            render(&record),
            "{\"version\":1,\"producer\":\"wc\",\"kind\":\"summary\",\
\"code\":\"text.line_count\",\"severity\":\"debug\",\
\"human\":{\"style\":\"terse\",\"message\":\"12 lines.\"},\"ai\":{}}\n"
        );
    }

    #[test]
    fn control_and_quote_bytes_are_escaped() {
        let record = StdInfoRecord::new(
            "x",
            StdInfoKind::Context,
            "c",
            Severity::Info,
            Human::message("a\"b\\c\n\t\u{0001}"),
        );
        let rendered = render(&record);
        assert!(rendered.contains("\"message\":\"a\\\"b\\\\c\\n\\t\\u0001\""));
        assert_eq!(STDINFO_VERSION_CURRENT, 1);
    }

    #[test]
    fn short_buffer_fails_closed() {
        let record = StdInfoRecord::new(
            "ls",
            StdInfoKind::Omission,
            "fs.x",
            Severity::Info,
            Human::message("hi"),
        );
        let mut buf = [0u8; 8];
        assert_eq!(record.write_jsonl(&mut buf), Err(Errno::BufferTooSmall));
    }
}
