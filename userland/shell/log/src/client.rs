//! The render/verify engine: read a stream's segments, decode each committed
//! record, and render it — or verify its integrity — one segment at a time.
//!
//! Every command is a **read-only view** over the authoritative segment files
//! (SYSLOG §17): the tool never writes the log. Two properties carry over
//! unchanged from the renderers it drives:
//!
//! * **Provenance is preserved, never obeyed.** The rich renderers separate
//!   the system-attested facts (stream, sequence, CPU, monotonic/wall time,
//!   effective level, the system-derived source, the attested origin) from
//!   caller content, and show a caller's *requested* privileged source/stream
//!   inertly as a claim.
//! * **Caller text can never forge output.** Control characters and quotes in
//!   caller-controlled strings are escaped by the renderers, so a hostile log
//!   record cannot move the cursor, forge a prefix, or break the JSON.

use alloc::string::String;
use core::fmt::Write as _;

use rustos_abi::Errno;
use rustos_log::{
    decode_record, render_json, render_line, render_markdown, render_table_header,
    render_table_row, verify_segment, DictionaryView, LogAttestationKey, RecordBlockRef,
    RecordFrame, SegmentHeader, SegmentReader, Stream,
};

use crate::command::{Command, Format};
use crate::error::LogError;
use crate::io::{Output, SegmentSource};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: log <command> [stream] [--format F]

  show    [stream] [--format line|json|md|table]   render records (default: line)
  report  [stream] [--format md|table]             render a human report (default: md)
  export  [stream] [--format json]                 export structured records (default: json)
  verify  [stream]                                 verify hashes, chains, and seals
  help, -h, --help                                 show this message

A stream operand is one of: boot runtime debug security audit journal.
Omitting it selects every stream. Output uses standard streams: records go to
stdout; verification results and diagnostics to stderr.
";

/// Run one [`Command`], reading segments through `source`, verifying seals
/// with `key` when present, and writing rendered output to `out`.
///
/// `key` is the per-installation log-attestation key (the journal principal's
/// `/System/Security/Keys/LogAttestation`). It is required to verify the
/// `audit` and `security` streams, whose closed segments are sealed with a
/// MAC; without it, `verify` of those streams fails closed
/// ([`LogError::Corrupt`] carrying `SealKeyRequired`) rather than passing a
/// segment it cannot check.
///
/// # Errors
///
/// * [`LogError::Read`] — a segment could not be read.
/// * [`LogError::Corrupt`] — a stored segment did not parse or verify.
/// * [`LogError::Decode`] — a committed record body did not decode.
/// * [`LogError::Output`] — writing the output failed.
pub fn run(
    command: Command,
    source: &dyn SegmentSource,
    key: Option<&LogAttestationKey>,
    out: &dyn Output,
) -> Result<(), LogError> {
    match command {
        Command::Help => write_line(out, USAGE.as_bytes()),
        Command::Show { stream, format } => show(stream, format, source, out),
        Command::Verify { stream } => verify(stream, source, key, out),
    }
}

/// The streams a command touches: the single named one, or every stream.
fn streams(stream: Option<Stream>) -> &'static [Stream] {
    match stream {
        Some(Stream::Boot) => &[Stream::Boot],
        Some(Stream::Runtime) => &[Stream::Runtime],
        Some(Stream::Debug) => &[Stream::Debug],
        Some(Stream::Security) => &[Stream::Security],
        Some(Stream::Audit) => &[Stream::Audit],
        Some(Stream::Journal) => &[Stream::Journal],
        None => &Stream::ALL,
    }
}

/// Render every record of the selected stream(s) in `format`.
fn show(
    stream: Option<Stream>,
    format: Format,
    source: &dyn SegmentSource,
    out: &dyn Output,
) -> Result<(), LogError> {
    if format == Format::Table {
        let mut header = String::new();
        render_to(&mut header, render_table_header)?;
        write_line(out, header.as_bytes())?;
    }
    for &stream in streams(stream) {
        let mut index = 0usize;
        while let Some(bytes) = source.read(stream, index).map_err(LogError::Read)? {
            render_segment(&bytes, format, out)?;
            index += 1;
        }
    }
    Ok(())
}

/// Decode and render every committed record of one segment image.
fn render_segment(bytes: &[u8], format: Format, out: &dyn Output) -> Result<(), LogError> {
    let reader = SegmentReader::open(bytes).map_err(LogError::Corrupt)?;
    // The header is Copy; take it before the reader is consumed as an iterator.
    let header: SegmentHeader = *reader.header();
    // One dictionary view per segment, advanced in append order as records are
    // decoded (the segment-local back-reference codec is only resolvable that
    // way).
    let mut view = DictionaryView::new();
    for block in reader {
        let record = decode_record(block.payload, &mut view).map_err(LogError::Decode)?;
        let frame = record_frame(&header, &block);
        let mut line = String::new();
        match format {
            Format::Line => render_to(&mut line, |w| render_line(w, block.monotonic, &record))?,
            Format::Json => render_to(&mut line, |w| render_json(w, &frame, &record))?,
            Format::Markdown => render_to(&mut line, |w| render_markdown(w, &frame, &record))?,
            Format::Table => render_to(&mut line, |w| render_table_row(w, &frame, &record))?,
        }
        write_line(out, line.as_bytes())?;
    }
    Ok(())
}

/// Verify the integrity of the selected stream(s), one summary line per
/// segment, returning the first failure after reporting it.
fn verify(
    stream: Option<Stream>,
    source: &dyn SegmentSource,
    key: Option<&LogAttestationKey>,
    out: &dyn Output,
) -> Result<(), LogError> {
    let mut failure: Option<LogError> = None;
    for &stream in streams(stream) {
        let mut index = 0usize;
        while let Some(bytes) = source.read(stream, index).map_err(LogError::Read)? {
            let mut line = String::new();
            match verify_segment(&bytes, key) {
                Ok(summary) => render_to(&mut line, |w| {
                    write!(
                        w,
                        "ok    {} segment {} seq {}..{} records {}{}",
                        stream.name(),
                        summary.header.segment_id,
                        summary.first_seq,
                        summary.next_seq,
                        summary.record_count,
                        if summary.sealed { " sealed" } else { "" },
                    )
                })?,
                Err(err) => {
                    render_to(&mut line, |w| {
                        write!(w, "FAIL  {} segment {index}: {err:?}", stream.name())
                    })?;
                    failure.get_or_insert(LogError::Corrupt(err));
                }
            }
            write_line(out, line.as_bytes())?;
            index += 1;
        }
    }
    match failure {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Assemble the container-owned [`RecordFrame`] a rich renderer needs from a
/// segment header and one committed record block.
fn record_frame(header: &SegmentHeader, block: &RecordBlockRef<'_>) -> RecordFrame {
    RecordFrame {
        stream: header.stream,
        boot_id: header.boot_id,
        cpu_id: block.cpu,
        seq: block.seq,
        monotonic: block.monotonic,
    }
}

/// Run a renderer against an in-memory [`String`] sink.
///
/// Writing to a `String` is infallible; the [`core::fmt::Error`] arm is
/// therefore unreachable, but it is mapped fail-closed rather than ignored.
fn render_to<F>(sink: &mut String, render: F) -> Result<(), LogError>
where
    F: FnOnce(&mut String) -> core::fmt::Result,
{
    render(sink).map_err(|_| LogError::Output(Errno::BufferTooSmall))
}

/// Write `bytes` followed by a newline to the terminal.
fn write_line(out: &dyn Output, bytes: &[u8]) -> Result<(), LogError> {
    out.write_all(bytes).map_err(LogError::Output)?;
    out.write_all(b"\n").map_err(LogError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use rustos_abi::{
        BootId, CapabilitySummary, Duration64, Origin, ProcId, Time64, TrustDomain,
        WallClockReading, WallTimeState, BOOT_ID_LEN,
    };
    use rustos_log::{
        machine_id_hash, stream_genesis, CallerContent, DictionaryBuilder, Level,
        LogAttestationKey, LogRecord, SegmentHeader, SegmentWriter, Stream,
    };

    use super::{run, USAGE};
    use crate::command::{Command, Format};
    use crate::error::LogError;
    use crate::io::{Output, SegmentSource};

    const MID: [u8; 16] = [0x11; 16];

    fn boot() -> BootId {
        BootId::from_raw([0x5A; BOOT_ID_LEN])
    }

    fn genesis(stream: Stream) -> [u8; 32] {
        stream_genesis(&machine_id_hash(&MID), stream.genesis_label(), &boot())
    }

    fn origin() -> Origin {
        Origin::new(
            TrustDomain::User,
            1000,
            1000,
            42,
            ProcId::from_raw([7u8; 16]),
            CapabilitySummary::from_raw([0u8; 32]),
            rustos_abi::ORIGIN_CONSOLE_NONE,
        )
    }

    /// A caller record with `message`, optionally requesting a privileged
    /// `source` (a spoof the renderer must show inertly).
    fn record<'a>(message: &'a str, requested_source: Option<&'a str>) -> LogRecord<'a> {
        LogRecord {
            effective_level: Level::Info,
            cpu_seq: 0,
            wall: WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
            origin: origin(),
            source_name: "user.1000.proc.2a",
            caller: CallerContent {
                level: None,
                component: None,
                tag: None,
                event_id: None,
                requested_source,
                requested_stream: None,
                message,
            },
            data: &[],
        }
    }

    /// Build a closed segment of `stream` carrying `messages`, sealing it with
    /// `key` when the stream requires a seal.
    fn segment(
        stream: Stream,
        segment_id: u64,
        messages: &[&str],
        key: Option<&LogAttestationKey>,
    ) -> Vec<u8> {
        let header = SegmentHeader {
            stream,
            segment_id,
            machine_id_hash: machine_id_hash(&MID),
            boot_id: boot(),
            first_seq: 0,
            prev_segment_hash: genesis(stream),
            creation_monotonic: Duration64::from_secs(1),
            creation_wall: WallClockReading::new(
                Time64::from_secs(1_700_000_000),
                WallTimeState::Trusted,
            ),
        };
        let mut buf = vec![0u8; 16384];
        let len = {
            let mut writer = SegmentWriter::begin(&mut buf, &header).expect("begin");
            for (i, message) in messages.iter().enumerate() {
                let mut record_buf = [0u8; 4096];
                let mut dict = DictionaryBuilder::new();
                let encoded = record(message, None)
                    .encode(&mut record_buf, &mut dict)
                    .expect("encode record");
                let cpu = u32::try_from(i).expect("index fits u32");
                let secs = 10 + i64::try_from(i).expect("index fits i64");
                writer
                    .append_record(cpu, Duration64::from_secs(secs), &record_buf[..encoded])
                    .expect("append");
            }
            writer.finish(key).expect("finish").len
        };
        buf.truncate(len);
        buf
    }

    /// A segment source backed by an in-memory map of stream -> segment images.
    struct Store {
        segments: Vec<(Stream, Vec<u8>)>,
    }

    impl Store {
        fn new() -> Self {
            Self {
                segments: Vec::new(),
            }
        }

        fn with(mut self, stream: Stream, bytes: Vec<u8>) -> Self {
            self.segments.push((stream, bytes));
            self
        }
    }

    impl SegmentSource for Store {
        fn read(&self, stream: Stream, index: usize) -> Result<Option<Vec<u8>>, rustos_abi::Errno> {
            Ok(self
                .segments
                .iter()
                .filter(|(s, _)| *s == stream)
                .nth(index)
                .map(|(_, bytes)| bytes.clone()))
        }
    }

    /// An output sink that captures every written byte.
    struct Capture {
        bytes: RefCell<Vec<u8>>,
    }

    impl Capture {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).expect("utf-8 output")
        }
    }

    impl Output for Capture {
        fn write_all(&self, bytes: &[u8]) -> Result<(), rustos_abi::Errno> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    #[test]
    fn help_prints_the_usage_banner() {
        let out = Capture::new();
        assert_eq!(run(Command::Help, &Store::new(), None, &out), Ok(()));
        assert_eq!(out.text(), std::format!("{USAGE}\n"));
    }

    #[test]
    fn show_renders_one_line_per_record() {
        let store = Store::new().with(
            Stream::Runtime,
            segment(
                Stream::Runtime,
                0,
                &["first message", "second message"],
                None,
            ),
        );
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Show {
                    stream: Some(Stream::Runtime),
                    format: Format::Line,
                },
                &store,
                None,
                &out,
            ),
            Ok(())
        );
        let text = out.text();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("first message"));
        assert!(text.contains("second message"));
        assert!(text.contains("user.1000.proc.2a"));
    }

    #[test]
    fn show_all_streams_reads_every_stream() {
        let store = Store::new()
            .with(Stream::Runtime, segment(Stream::Runtime, 0, &["r"], None))
            .with(Stream::Debug, segment(Stream::Debug, 0, &["d"], None));
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Show {
                    stream: None,
                    format: Format::Line,
                },
                &store,
                None,
                &out,
            ),
            Ok(())
        );
        let text = out.text();
        assert!(text.contains(": r"));
        assert!(text.contains(": d"));
    }

    #[test]
    fn export_json_is_one_object_per_record() {
        let store = Store::new().with(Stream::Runtime, segment(Stream::Runtime, 0, &["msg"], None));
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Show {
                    stream: Some(Stream::Runtime),
                    format: Format::Json,
                },
                &store,
                None,
                &out,
            ),
            Ok(())
        );
        let line = out.text();
        assert!(line.trim_end().starts_with('{'));
        assert!(line.trim_end().ends_with('}'));
        assert!(line.contains("\"stream\""));
    }

    #[test]
    fn table_emits_a_header_then_one_row_per_record() {
        let store = Store::new().with(
            Stream::Runtime,
            segment(Stream::Runtime, 0, &["a", "b"], None),
        );
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Show {
                    stream: Some(Stream::Runtime),
                    format: Format::Table,
                },
                &store,
                None,
                &out,
            ),
            Ok(())
        );
        // One header line plus one row per record.
        assert_eq!(out.text().lines().count(), 3);
    }

    #[test]
    fn a_requested_privileged_source_is_shown_inertly() {
        // A record that requested the kernel source is rendered under its real
        // user source, with the spoofed request preserved as evidence only.
        let header = SegmentHeader {
            stream: Stream::Runtime,
            segment_id: 0,
            machine_id_hash: machine_id_hash(&MID),
            boot_id: boot(),
            first_seq: 0,
            prev_segment_hash: genesis(Stream::Runtime),
            creation_monotonic: Duration64::from_secs(1),
            creation_wall: WallClockReading::new(
                Time64::from_secs(1_700_000_000),
                WallTimeState::Trusted,
            ),
        };
        let mut buf = vec![0u8; 8192];
        let len = {
            let mut writer = SegmentWriter::begin(&mut buf, &header).expect("begin");
            let mut record_buf = [0u8; 4096];
            let mut dict = DictionaryBuilder::new();
            let encoded = record("hello", Some("kernel.mem"))
                .encode(&mut record_buf, &mut dict)
                .expect("encode");
            writer
                .append_record(0, Duration64::from_secs(1), &record_buf[..encoded])
                .expect("append");
            writer.finish(None).expect("finish").len
        };
        buf.truncate(len);
        let store = Store::new().with(Stream::Runtime, buf);
        let out = Capture::new();
        run(
            Command::Show {
                stream: Some(Stream::Runtime),
                format: Format::Line,
            },
            &store,
            None,
            &out,
        )
        .expect("renders");
        let text = out.text();
        assert!(text.contains("requested_source=kernel.mem"));
        // The real source heads the line, not the spoofed request.
        assert!(text.contains("user.1000.proc.2a"));
    }

    #[test]
    fn verify_reports_ok_for_an_honest_segment() {
        let store = Store::new().with(
            Stream::Runtime,
            segment(Stream::Runtime, 3, &["a", "b"], None),
        );
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Verify {
                    stream: Some(Stream::Runtime)
                },
                &store,
                None,
                &out
            ),
            Ok(())
        );
        let text = out.text();
        assert!(text.starts_with("ok"));
        assert!(text.contains("records 2"));
    }

    #[test]
    fn verify_fails_closed_on_a_tampered_segment() {
        let mut bytes = segment(Stream::Runtime, 0, &["a", "b"], None);
        // Flip a payload byte well inside the body: the record hash chain and
        // segment hash must catch it.
        let victim = bytes.len() / 2;
        bytes[victim] ^= 0xff;
        let store = Store::new().with(Stream::Runtime, bytes);
        let out = Capture::new();
        let result = run(
            Command::Verify {
                stream: Some(Stream::Runtime),
            },
            &store,
            None,
            &out,
        );
        assert!(matches!(result, Err(LogError::Corrupt(_))));
        assert!(out.text().contains("FAIL"));
    }

    #[test]
    fn verify_of_a_sealed_stream_needs_the_key() {
        let key = LogAttestationKey::from_key([0x24; 32]);
        let sealed = segment(Stream::Audit, 0, &["audited"], Some(&key));
        let store = Store::new().with(Stream::Audit, sealed);

        // Without the key, a sealed segment cannot be verified: fail closed.
        let out = Capture::new();
        assert!(matches!(
            run(
                Command::Verify {
                    stream: Some(Stream::Audit)
                },
                &store,
                None,
                &out
            ),
            Err(LogError::Corrupt(_))
        ));

        // With the key it verifies and reports the seal.
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Verify {
                    stream: Some(Stream::Audit),
                },
                &store,
                Some(&key),
                &out,
            ),
            Ok(())
        );
        assert!(out.text().contains("sealed"));
    }

    #[test]
    fn a_missing_stream_renders_nothing() {
        let out = Capture::new();
        assert_eq!(
            run(
                Command::Show {
                    stream: Some(Stream::Journal),
                    format: Format::Line,
                },
                &Store::new(),
                None,
                &out,
            ),
            Ok(())
        );
        assert!(out.text().is_empty());
    }

    #[test]
    fn a_corrupt_segment_header_fails_closed_on_show() {
        let store = Store::new().with(Stream::Runtime, vec![0u8; 8]);
        let out = Capture::new();
        assert!(matches!(
            run(
                Command::Show {
                    stream: Some(Stream::Runtime),
                    format: Format::Line,
                },
                &store,
                None,
                &out,
            ),
            Err(LogError::Corrupt(_))
        ));
    }
}
