//! The streaming engine: emit the last part of each source.

use alloc::format;
use alloc::vec;

use tairix_abi::stdinfo::{Human, Severity, StdInfoKind, StdInfoRecord};
use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};
use tairix_util::tailwindow::{ByteWindow, LineWindow};

use crate::command::{Command, Count, HeaderMode, Job, Source};
use crate::error::TailError;
use crate::io::{FileSource, Info, Input, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `tail`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: tail [-qvz] [-c [+]num[suffix]] [-n [+]num[suffix]] [--] [file...]";

/// `tail`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "tail";

/// Bytes pulled from a source per read call. A fixed chunk bounds the
/// per-call buffer so a source of any size streams through a constant amount
/// of memory (the last-N modes additionally retain the requested window,
/// exactly as much as the format demands).
const READ_CHUNK: usize = 4096;

/// One source's selection state machine, plus the running tally of leading
/// units it dropped (for the fd-3 advisory).
struct Engine {
    mode: Mode,
    /// Leading units (lines or bytes) not shown: aged out of the last-N
    /// window, or skipped by the from-start posture.
    omitted: u64,
}

/// What a source's engine is doing.
enum Mode {
    /// `-c N`: retain the last `N` bytes, emitted at end.
    LastBytes(ByteWindow),
    /// `-n N`: retain the last `N` lines, emitted at end.
    LastLines(LineWindow),
    /// `-c +N`: skip the first `skip` (= N-1) bytes, stream the rest.
    FromByte { skip: u64 },
    /// `-n +N`: skip through the first `skip` (= N-1) delimiters, stream the
    /// rest.
    FromLine { skip: u64, delim: u8 },
}

impl Engine {
    fn new(job: &Job) -> Self {
        let delim = if job.zero { 0 } else { b'\n' };
        let mode = match (job.count, job.from_start) {
            (Count::Bytes(n), false) => Mode::LastBytes(ByteWindow::new(n)),
            (Count::Lines(n), false) => Mode::LastLines(LineWindow::new(n, delim)),
            // `+N` starts at unit N (1-based), so skip the first N-1 units;
            // `+0` and `+1` skip nothing (the whole source).
            (Count::Bytes(n), true) => Mode::FromByte {
                skip: n.saturating_sub(1),
            },
            (Count::Lines(n), true) => Mode::FromLine {
                skip: n.saturating_sub(1),
                delim,
            },
        };
        Self { mode, omitted: 0 }
    }

    /// Feed the next chunk, writing whatever the mode emits now and counting
    /// the leading units it drops.
    fn feed(&mut self, chunk: &[u8], out: &dyn Output) -> Result<(), Errno> {
        let Engine { mode, omitted } = self;
        match mode {
            // The last-N modes drop what ages out of the retained window
            // (that leading content is exactly what `tail` does *not* show)
            // and only tally it for the fd-3 advisory; the window is emitted
            // at `finish`.
            Mode::LastBytes(window) => window.push(chunk, |aged| {
                *omitted += aged.len() as u64;
                Ok(())
            }),
            Mode::LastLines(window) => window.push(chunk, |_aged| {
                *omitted += 1;
                Ok(())
            }),
            Mode::FromByte { skip } => {
                let len = chunk.len() as u64;
                if *skip >= len {
                    *skip -= len;
                    *omitted += len;
                    Ok(())
                } else {
                    // `*skip < len == chunk.len()`, so the conversion cannot
                    // truncate; clamp fail-safe rather than cast bare.
                    let start = usize::try_from(*skip)
                        .unwrap_or(usize::MAX)
                        .min(chunk.len());
                    *omitted += *skip;
                    *skip = 0;
                    out.write_all(&chunk[start..])
                }
            }
            Mode::FromLine { skip, delim } => {
                if *skip == 0 {
                    return out.write_all(chunk);
                }
                let mut index = 0;
                while index < chunk.len() {
                    let byte = chunk[index];
                    index += 1;
                    if byte == *delim {
                        *skip -= 1;
                        *omitted += 1;
                        if *skip == 0 {
                            return out.write_all(&chunk[index..]);
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// The source is exhausted: emit whatever the last-N modes still hold.
    /// The unterminated final fragment counts as a line, as in the GNU tool.
    fn finish(&mut self, out: &dyn Output) -> Result<(), Errno> {
        let Engine { mode, omitted } = self;
        match mode {
            Mode::LastBytes(window) => window.drain(|bytes| out.write_all(bytes)),
            Mode::LastLines(window) => {
                window.finish_partial(|_aged| {
                    *omitted += 1;
                    Ok(())
                })?;
                window.drain(|line| out.write_all(line))
            }
            Mode::FromByte { .. } | Mode::FromLine { .. } => Ok(()),
        }
    }
}

/// Run one [`Command`], reading its sources through `files`/`stdin` and
/// writing the selected bytes to `out`. `locale` is the user's `LANG`
/// preference, if set; `help` is the tool's own `Help/` tree, read by the
/// short-help switches; `err` receives the per-file diagnostics; `info`
/// receives the fd-3 advisory.
///
/// Returns `Ok(true)` when every source was served, `Ok(false)` when at
/// least one source failed (it was diagnosed on `err` and the run moved to
/// the next operand, as in the GNU tool).
///
/// # Errors
///
/// [`TailError::Output`] — writing `out` or `err` failed. Nothing else is
/// fatal; an fd-3 advisory write never fails the run.
#[allow(clippy::too_many_arguments)]
pub fn run(
    command: Command,
    locale: Option<&str>,
    files: &dyn FileSource,
    stdin: &dyn Input,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
    info: &dyn Info,
) -> Result<bool, TailError> {
    let job = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(TailError::Output)?;
            return Ok(true);
        }
        Command::Tail(job) => job,
    };

    let show_headers = match job.headers {
        HeaderMode::Always => true,
        HeaderMode::Never => false,
        HeaderMode::MultipleFiles => job.sources.len() > 1,
    };

    let lines = matches!(job.count, Count::Lines(_));
    let mut all_ok = true;
    let mut header_written = false;
    let mut omitted_total: u64 = 0;
    for source in &job.sources {
        let (ok, omitted) = match source {
            Source::Stdin => {
                if show_headers {
                    write_header(out, "standard input", &mut header_written)
                        .map_err(TailError::Output)?;
                }
                serve_stdin(&job, stdin, out, err)?
            }
            Source::Path(path) => serve_path(
                &job,
                path,
                files,
                out,
                err,
                show_headers,
                &mut header_written,
            )?,
        };
        all_ok &= ok;
        omitted_total = omitted_total.saturating_add(omitted);
    }

    if omitted_total > 0 {
        emit_omission_record(info, omitted_total, lines, job.from_start);
    }
    Ok(all_ok)
}

/// The `==> name <==` banner, preceded by a blank line for every header
/// after the first, exactly as the GNU tool separates multi-file output.
fn write_header(out: &dyn Output, name: &str, header_written: &mut bool) -> Result<(), Errno> {
    let banner = if *header_written {
        format!("\n==> {name} <==\n")
    } else {
        format!("==> {name} <==\n")
    };
    *header_written = true;
    out.write_all(banner.as_bytes())
}

/// Serve standard input. A read failure is diagnosed and ends this source;
/// the caller moves on. Returns `(served-cleanly, leading-units-omitted)`.
fn serve_stdin(
    job: &Job,
    stdin: &dyn Input,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(bool, u64), TailError> {
    let mut engine = Engine::new(job);
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => engine.feed(&buf[..n], out).map_err(TailError::Output)?,
            Err(errno) => {
                diagnose(err, &format!("error reading 'standard input': {errno}"))?;
                return Ok((false, engine.omitted));
            }
        }
    }
    engine.finish(out).map_err(TailError::Output)?;
    Ok((true, engine.omitted))
}

/// Serve one named file. The first read doubles as the open probe: its
/// failure is the GNU "cannot open" diagnostic and suppresses the header,
/// exactly as an unopenable file prints no header there. Returns
/// `(served-cleanly, leading-units-omitted)`.
fn serve_path(
    job: &Job,
    path: &str,
    files: &dyn FileSource,
    out: &dyn Output,
    err: &dyn Output,
    show_headers: bool,
    header_written: &mut bool,
) -> Result<(bool, u64), TailError> {
    let mut buf = vec![0u8; READ_CHUNK];
    let first = match files.read(path, 0, &mut buf) {
        Ok(n) => n,
        Err(errno) => {
            diagnose(err, &format!("cannot open '{path}' for reading: {errno}"))?;
            return Ok((false, 0));
        }
    };
    if show_headers {
        write_header(out, path, header_written).map_err(TailError::Output)?;
    }

    let mut engine = Engine::new(job);
    engine.feed(&buf[..first], out).map_err(TailError::Output)?;
    let mut offset = first as u64;
    if first > 0 {
        loop {
            match files.read(path, offset, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    engine.feed(&buf[..n], out).map_err(TailError::Output)?;
                    offset += n as u64;
                }
                Err(errno) => {
                    diagnose(err, &format!("error reading '{path}': {errno}"))?;
                    return Ok((false, engine.omitted));
                }
            }
        }
    }
    engine.finish(out).map_err(TailError::Output)?;
    Ok((true, engine.omitted))
}

/// Emit the fd-3 advisory that leading content was not shown: a tool or user
/// then knows the output is a tail view and is not exhaustive. Advisory only
/// — never affects the output, ordering, or exit status.
fn emit_omission_record(info: &dyn Info, omitted: u64, lines: bool, from_start: bool) {
    let (unit, units) = if lines {
        ("line", "lines")
    } else {
        ("byte", "bytes")
    };
    let noun = if omitted == 1 { unit } else { units };
    let message = format!("{omitted} earlier {noun} not shown.");
    let reason = if from_start {
        "before_start_offset"
    } else {
        "before_tail"
    };
    let code = if lines {
        "text.leading_lines_omitted"
    } else {
        "text.leading_bytes_omitted"
    };
    let ai = format!(
        "{{\"subject\":\"tail_view\",\
         \"omission\":{{\"reason\":\"{reason}\",\
         \"entry_class\":\"{unit}\",\"omitted_count\":{omitted},\
         \"stdout_is_exhaustive\":false}}}}"
    );
    let record = StdInfoRecord::new(
        OWN_WORD,
        StdInfoKind::Omission,
        code,
        Severity::Info,
        Human::message(&message),
    )
    .with_ai(&ai);
    let mut buf = [0u8; 512];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        info.emit(&buf[..len]);
    }
}

/// One `tail: …` diagnostic line on the error stream. A failure to report is
/// itself fatal — the tool never continues silently.
fn diagnose(err: &dyn Output, message: &str) -> Result<(), TailError> {
    err.write_all(format!("tail: {message}\n").as_bytes())
        .map_err(TailError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};

    use super::{run, USAGE};
    use crate::command::{parse, Command};
    use crate::error::TailError;
    use crate::io::{FileSource, Info, Input, Output};

    /// An in-memory filesystem serving reads in bounded slices so a streamed
    /// file crosses several read calls.
    struct MemFiles {
        files: BTreeMap<String, Vec<u8>>,
        chunk: usize,
        fail_at: Option<(String, u64)>,
    }

    impl MemFiles {
        fn new(entries: &[(&str, &[u8])]) -> Self {
            Self {
                files: entries
                    .iter()
                    .map(|(name, data)| (String::from(*name), data.to_vec()))
                    .collect(),
                chunk: usize::MAX,
                fail_at: None,
            }
        }
    }

    impl FileSource for MemFiles {
        fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            if let Some((name, at)) = &self.fail_at {
                if name == path && offset >= *at {
                    return Err(Errno::NotImplemented);
                }
            }
            let Some(data) = self.files.get(path) else {
                return Err(Errno::NotFound);
            };
            let offset = usize::try_from(offset).unwrap_or(usize::MAX);
            if offset >= data.len() {
                return Ok(0);
            }
            let take = buf.len().min(self.chunk).min(data.len() - offset);
            buf[..take].copy_from_slice(&data[offset..offset + take]);
            Ok(take)
        }
    }

    /// In-memory standard input, likewise serving bounded slices.
    struct MemStdin {
        data: RefCell<Vec<u8>>,
        chunk: usize,
    }

    impl MemStdin {
        fn new(data: &[u8]) -> Self {
            Self {
                data: RefCell::new(data.to_vec()),
                chunk: usize::MAX,
            }
        }
    }

    impl Input for MemStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            let mut data = self.data.borrow_mut();
            let take = buf.len().min(self.chunk).min(data.len());
            buf[..take].copy_from_slice(&data[..take]);
            data.drain(..take);
            Ok(take)
        }
    }

    /// A capturing output stream.
    #[derive(Default)]
    struct Sink(RefCell<Vec<u8>>);

    impl Sink {
        fn bytes(&self) -> Vec<u8> {
            self.0.borrow().clone()
        }
    }

    impl Output for Sink {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    /// An output stream whose every write fails.
    struct BrokenSink;

    impl Output for BrokenSink {
        fn write_all(&self, _bytes: &[u8]) -> Result<(), Errno> {
            Err(Errno::NotImplemented)
        }
    }

    /// A capturing fd-3 sink.
    #[derive(Default)]
    struct InfoSink(RefCell<Vec<Vec<u8>>>);

    impl InfoSink {
        fn records(&self) -> Vec<String> {
            self.0
                .borrow()
                .iter()
                .map(|r| String::from_utf8(r.clone()).expect("record is UTF-8"))
                .collect()
        }
    }

    impl Info for InfoSink {
        fn emit(&self, record: &[u8]) {
            self.0.borrow_mut().push(record.to_vec());
        }
    }

    /// A bundle with no help documents, so the short help falls back to the
    /// usage banner.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    fn command(args: &[&str]) -> Command {
        parse(args).expect("fixture command line parses")
    }

    /// Run `args` against the fixtures, returning (all-ok, stdout, stderr,
    /// fd-3 records).
    fn run_case(
        args: &[&str],
        files: &MemFiles,
        stdin: &MemStdin,
    ) -> (bool, Vec<u8>, String, Vec<String>) {
        let out = Sink::default();
        let err = Sink::default();
        let info = InfoSink::default();
        let ok = run(
            command(args),
            None,
            files,
            stdin,
            &NoHelp,
            &out,
            &err,
            &info,
        )
        .expect("fixture output streams never fail");
        (
            ok,
            out.bytes(),
            String::from_utf8(err.bytes()).expect("stderr is UTF-8"),
            info.records(),
        )
    }

    #[test]
    fn default_is_the_last_ten_lines_of_stdin() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n");
        let (ok, out, err, _) = run_case(&[], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n");
        assert!(err.is_empty());
    }

    #[test]
    fn last_bytes_across_chunked_reads() {
        let mut files = MemFiles::new(&[("f", b"hello world")]);
        files.chunk = 2;
        let stdin = MemStdin::new(b"");
        let (ok, out, _, _) = run_case(&["-c", "5", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"world");
    }

    #[test]
    fn last_lines_across_chunked_reads() {
        let mut files = MemFiles::new(&[("f", b"a\nb\nc\nd\n")]);
        files.chunk = 3;
        let stdin = MemStdin::new(b"");
        let (ok, out, _, _) = run_case(&["-n", "2", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"c\nd\n");
    }

    #[test]
    fn from_line_skips_the_leading_lines() {
        let mut files = MemFiles::new(&[("f", b"l1\nl2\nl3\nl4\nl5\n")]);
        files.chunk = 4;
        let stdin = MemStdin::new(b"");
        let (ok, out, _, _) = run_case(&["-n", "+3", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"l3\nl4\nl5\n");
    }

    #[test]
    fn from_line_plus_one_is_the_whole_source() {
        let files = MemFiles::new(&[("f", b"a\nb\nc\n")]);
        let stdin = MemStdin::new(b"");
        for arg in [&["-n", "+1", "f"][..], &["-n", "+0", "f"][..]] {
            let (ok, out, _, records) = run_case(arg, &files, &stdin);
            assert!(ok);
            assert_eq!(out, b"a\nb\nc\n");
            // Nothing was skipped, so no advisory.
            assert!(records.is_empty(), "unexpected fd-3 record: {records:?}");
        }
    }

    #[test]
    fn from_byte_skips_the_leading_bytes() {
        let mut files = MemFiles::new(&[("f", b"abcdefghij")]);
        files.chunk = 3;
        let stdin = MemStdin::new(b"");
        let (ok, out, _, _) = run_case(&["-c", "+5", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"efghij");
    }

    #[test]
    fn zero_terminated_lines_use_the_nul_delimiter() {
        let files = MemFiles::new(&[("f", b"a\0b\0c\0")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _, _) = run_case(&["-z", "-n", "1", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"c\0");
    }

    #[test]
    fn unterminated_final_fragment_counts_as_a_line() {
        let files = MemFiles::new(&[("f", b"a\nb\nc")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _, _) = run_case(&["-n", "2", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"b\nc");
    }

    #[test]
    fn headers_name_each_of_several_sources() {
        let files = MemFiles::new(&[("a", b"A\n"), ("b", b"B\n")]);
        let stdin = MemStdin::new(b"S\n");
        let (ok, out, _, _) = run_case(&["-n1", "a", "-", "b"], &files, &stdin);
        assert!(ok);
        assert_eq!(
            core::str::from_utf8(&out).expect("utf8"),
            "==> a <==\nA\n\n==> standard input <==\nS\n\n==> b <==\nB\n"
        );
    }

    #[test]
    fn verbose_forces_and_quiet_suppresses_headers() {
        let files = MemFiles::new(&[("a", b"A\n"), ("b", b"B\n")]);
        let stdin = MemStdin::new(b"");
        let (_, out, _, _) = run_case(&["-v", "-n1", "a"], &files, &stdin);
        assert_eq!(out, b"==> a <==\nA\n");
        let (_, out, _, _) = run_case(&["-q", "-n1", "a", "b"], &files, &stdin);
        assert_eq!(out, b"A\nB\n");
    }

    #[test]
    fn a_missing_file_is_diagnosed_and_the_run_continues() {
        let files = MemFiles::new(&[("b", b"B\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err, _) = run_case(&["-n1", "missing", "b"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, b"==> b <==\nB\n");
        assert!(
            err.starts_with("tail: cannot open 'missing' for reading:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn a_mid_stream_read_error_is_diagnosed() {
        let mut files = MemFiles::new(&[("f", b"abcdef")]);
        files.chunk = 2;
        files.fail_at = Some((String::from("f"), 4));
        let stdin = MemStdin::new(b"");
        // A from-start read streams as it goes, so the partial output before
        // the failure is visible and the failure is reported.
        let (ok, out, err, _) = run_case(&["-c", "+1", "f"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, b"abcd");
        assert!(
            err.starts_with("tail: error reading 'f':"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn the_omission_advisory_reports_dropped_leading_lines() {
        let files = MemFiles::new(&[("f", b"1\n2\n3\n4\n5\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _, records) = run_case(&["-n", "2", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"4\n5\n");
        assert_eq!(records.len(), 1);
        assert!(
            records[0].contains("\"code\":\"text.leading_lines_omitted\""),
            "unexpected record: {}",
            records[0]
        );
        assert!(
            records[0].contains("\"omitted_count\":3"),
            "unexpected record: {}",
            records[0]
        );
        assert!(records[0].contains("\"kind\":\"omission\""));
    }

    #[test]
    fn no_advisory_when_nothing_is_dropped() {
        let files = MemFiles::new(&[("f", b"1\n2\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, _, _, records) = run_case(&["-n", "9", "f"], &files, &stdin);
        assert!(ok);
        assert!(records.is_empty(), "unexpected fd-3 record: {records:?}");
    }

    #[test]
    fn from_byte_advisory_reports_skipped_bytes() {
        let files = MemFiles::new(&[("f", b"abcdef")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _, records) = run_case(&["-c", "+3", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"cdef");
        assert_eq!(records.len(), 1);
        assert!(
            records[0].contains("\"code\":\"text.leading_bytes_omitted\"")
                && records[0].contains("\"omitted_count\":2")
                && records[0].contains("\"reason\":\"before_start_offset\""),
            "unexpected record: {}",
            records[0]
        );
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err, _) = run_case(&["-h"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, alloc::format!("{USAGE}\n").into_bytes());
        assert!(err.is_empty());
    }

    #[test]
    fn a_failed_output_write_is_fatal() {
        let files = MemFiles::new(&[("f", b"data\n")]);
        let stdin = MemStdin::new(b"");
        let result = run(
            command(&["f"]),
            None,
            &files,
            &stdin,
            &NoHelp,
            &BrokenSink,
            &Sink::default(),
            &InfoSink::default(),
        );
        assert_eq!(result, Err(TailError::Output(Errno::NotImplemented)));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same keys
    /// as the canonical one. The documents are read from the bundle's own
    /// on-disk `Help/` tree — the single source the image builder plants —
    /// never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/tail.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-c, --bytes <num>`",
                "`-n, --lines <num>`",
                "`-q, --quiet, --silent`",
                "`-v, --verbose`",
                "`-z, --zero-terminated`",
                "`-h, -?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/tail.md must document {switch}"
                );
            }
        }
    }
}
