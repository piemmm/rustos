//! The streaming engine: emit the selected part of each source.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_help::{own_short_help, HelpSource};

use crate::command::{Command, Count, HeaderMode, Job, Source};
use crate::error::HeadError;
use crate::io::{FileSource, Input, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `head`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: head [-qvz] [-c [-]num[suffix]] [-n [-]num[suffix]] [--] [file...]";

/// `head`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "head";

/// Bytes pulled from a source per read call. A fixed chunk bounds the
/// per-call buffer so a source of any size streams through a constant
/// amount of memory (the elide modes additionally retain up to the elided
/// tail, exactly as much as the semantics require).
const READ_CHUNK: usize = 4096;

/// One source's trimming state machine: fed chunks in order, it writes the
/// part of the stream the job selects and retains only what the elide
/// semantics require.
enum Engine {
    /// `-c N`: the first `remaining` bytes.
    FirstBytes { remaining: u64 },
    /// `-n N`: everything through the `remaining`-th delimiter.
    FirstLines { remaining: u64, delim: u8 },
    /// `-c -N`: everything but the last `keep` bytes.
    ElideBytes(TailRing),
    /// `-n -N`: everything but the last `keep` lines.
    ElideLines(TailLines),
}

impl Engine {
    fn new(job: &Job) -> Self {
        let delim = if job.zero { 0 } else { b'\n' };
        match (job.count, job.elide) {
            (Count::Bytes(n), false) => Self::FirstBytes { remaining: n },
            (Count::Lines(n), false) => Self::FirstLines {
                remaining: n,
                delim,
            },
            (Count::Bytes(n), true) => Self::ElideBytes(TailRing::new(n)),
            (Count::Lines(n), true) => Self::ElideLines(TailLines::new(n, delim)),
        }
    }

    /// Whether the engine will emit nothing further, so the caller can stop
    /// reading. Only the head modes finish early; the elide modes always
    /// consume the whole source.
    fn done(&self) -> bool {
        match self {
            Self::FirstBytes { remaining } | Self::FirstLines { remaining, .. } => *remaining == 0,
            Self::ElideBytes(_) | Self::ElideLines(_) => false,
        }
    }

    /// Feed the next chunk, writing whatever the mode has decided to emit.
    fn feed(&mut self, chunk: &[u8], out: &dyn Output) -> Result<(), Errno> {
        match self {
            Self::FirstBytes { remaining } => {
                let take = usize::try_from(*remaining).unwrap_or(usize::MAX);
                let take = take.min(chunk.len());
                if take > 0 {
                    out.write_all(&chunk[..take])?;
                    *remaining -= take as u64;
                }
                Ok(())
            }
            Self::FirstLines { remaining, delim } => {
                if *remaining == 0 {
                    return Ok(());
                }
                for (index, &byte) in chunk.iter().enumerate() {
                    if byte == *delim {
                        *remaining -= 1;
                        if *remaining == 0 {
                            return out.write_all(&chunk[..=index]);
                        }
                    }
                }
                out.write_all(chunk)
            }
            Self::ElideBytes(ring) => ring.push(chunk, out),
            Self::ElideLines(tail) => tail.push(chunk, out),
        }
    }

    /// The source is exhausted: settle whatever the mode still holds. The
    /// elide modes drop their retained tail (that is the elision); the
    /// unterminated final fragment counts as a line, as in the GNU tool.
    fn finish(self, out: &dyn Output) -> Result<(), Errno> {
        match self {
            Self::FirstBytes { .. } | Self::FirstLines { .. } | Self::ElideBytes(_) => Ok(()),
            Self::ElideLines(tail) => tail.finish(out),
        }
    }
}

/// The rolling tail buffer of the elide-bytes mode: retains the most recent
/// `keep` bytes and emits every byte that ages out of that window. Memory
/// is bounded by the smaller of `keep` and the bytes actually seen; each
/// pushed byte is copied O(1) times.
struct TailRing {
    keep: usize,
    buf: Vec<u8>,
    /// Index of the oldest byte once `buf` has filled to `keep` (before
    /// that, `buf` is a plain in-order prefix and `start` is `0`).
    start: usize,
}

impl TailRing {
    fn new(keep: u64) -> Self {
        Self {
            // A keep larger than any addressable buffer behaves as
            // "retain everything": nothing is ever emitted, exactly the
            // semantics of eliding more than the input can hold.
            keep: usize::try_from(keep).unwrap_or(usize::MAX),
            buf: Vec::new(),
            start: 0,
        }
    }

    fn push(&mut self, chunk: &[u8], out: &dyn Output) -> Result<(), Errno> {
        if self.keep == 0 {
            return out.write_all(chunk);
        }
        if chunk.len() >= self.keep {
            // The chunk alone covers the whole window: everything buffered,
            // and the chunk's own prefix, ages out at once. In circular
            // order the oldest bytes run from `start` to the end, then
            // wrap to the front.
            let (wrapped, oldest) = self.buf.split_at(self.start.min(self.buf.len()));
            out.write_all(oldest)?;
            out.write_all(wrapped)?;
            out.write_all(&chunk[..chunk.len() - self.keep])?;
            self.buf.clear();
            self.buf
                .extend_from_slice(&chunk[chunk.len() - self.keep..]);
            self.start = 0;
            return Ok(());
        }
        if self.buf.len() < self.keep {
            let total = self.buf.len() + chunk.len();
            if total <= self.keep {
                self.buf.extend_from_slice(chunk);
                return Ok(());
            }
            // Filling-to-full transition: the front of the linear buffer
            // ages out, the remainder plus the chunk is exactly the window.
            let excess = total - self.keep;
            out.write_all(&self.buf[..excess])?;
            let mut window = Vec::with_capacity(self.keep);
            window.extend_from_slice(&self.buf[excess..]);
            window.extend_from_slice(chunk);
            self.buf = window;
            self.start = 0;
            return Ok(());
        }
        // Full circular window: exactly `chunk.len()` oldest bytes age out
        // and the chunk overwrites their slots.
        let len = chunk.len();
        let first_span = len.min(self.keep - self.start);
        out.write_all(&self.buf[self.start..self.start + first_span])?;
        if len > first_span {
            out.write_all(&self.buf[..len - first_span])?;
        }
        self.buf[self.start..self.start + first_span].copy_from_slice(&chunk[..first_span]);
        if len > first_span {
            self.buf[..len - first_span].copy_from_slice(&chunk[first_span..]);
        }
        self.start = (self.start + len) % self.keep;
        Ok(())
    }
}

/// The pending-line queue of the elide-lines mode: retains the most recent
/// `keep` complete lines (plus the growing partial line) and emits every
/// line that ages out. The unterminated final fragment counts as a line at
/// end-of-input, as in the GNU tool.
struct TailLines {
    keep: usize,
    delim: u8,
    queue: VecDeque<Vec<u8>>,
    partial: Vec<u8>,
}

impl TailLines {
    fn new(keep: u64, delim: u8) -> Self {
        Self {
            keep: usize::try_from(keep).unwrap_or(usize::MAX),
            delim,
            queue: VecDeque::new(),
            partial: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8], out: &dyn Output) -> Result<(), Errno> {
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|&b| b == self.delim) {
            self.partial.extend_from_slice(&rest[..=pos]);
            let line = core::mem::take(&mut self.partial);
            self.queue.push_back(line);
            self.age_out(out)?;
            rest = &rest[pos + 1..];
        }
        self.partial.extend_from_slice(rest);
        Ok(())
    }

    fn finish(mut self, out: &dyn Output) -> Result<(), Errno> {
        if !self.partial.is_empty() {
            let line = core::mem::take(&mut self.partial);
            self.queue.push_back(line);
            self.age_out(out)?;
        }
        Ok(())
    }

    /// Emit the oldest queued line once the queue exceeds the retained
    /// window. The queue can exceed `keep` by at most one line per push,
    /// so a single pop restores the invariant.
    fn age_out(&mut self, out: &dyn Output) -> Result<(), Errno> {
        if self.queue.len() > self.keep {
            let aged = self.queue.pop_front().unwrap_or_default();
            out.write_all(&aged)?;
        }
        Ok(())
    }
}

/// Run one [`Command`], reading its sources through `files`/`stdin` and
/// writing the selected bytes to `out`. `locale` is the user's `LANG`
/// preference, if set; `help` is the tool's own `Help/` tree, read by the
/// short-help switches; `err` receives the per-file diagnostics.
///
/// Returns `Ok(true)` when every source was served, `Ok(false)` when at
/// least one source failed (it was diagnosed on `err` and the run moved to
/// the next operand, as in the GNU tool).
///
/// # Errors
///
/// [`HeadError::Output`] — writing `out` or `err` failed. Nothing else is
/// fatal.
pub fn run(
    command: Command,
    locale: Option<&str>,
    files: &dyn FileSource,
    stdin: &dyn Input,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, HeadError> {
    let job = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes).map_err(HeadError::Output)?;
            return Ok(true);
        }
        Command::Head(job) => job,
    };

    let show_headers = match job.headers {
        HeaderMode::Always => true,
        HeaderMode::Never => false,
        HeaderMode::MultipleFiles => job.sources.len() > 1,
    };

    let mut all_ok = true;
    let mut header_written = false;
    for source in &job.sources {
        let ok = match source {
            Source::Stdin => {
                if show_headers {
                    write_header(out, "standard input", &mut header_written)
                        .map_err(HeadError::Output)?;
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
/// the caller moves on. Returns whether the source was served cleanly.
fn serve_stdin(
    job: &Job,
    stdin: &dyn Input,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, HeadError> {
    let mut engine = Engine::new(job);
    let mut buf = vec![0u8; READ_CHUNK];
    while !engine.done() {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => engine.feed(&buf[..n], out).map_err(HeadError::Output)?,
            Err(errno) => {
                diagnose(err, &format!("error reading 'standard input': {errno}"))?;
                return Ok(false);
            }
        }
    }
    engine.finish(out).map_err(HeadError::Output)?;
    Ok(true)
}

/// Serve one named file. The first read doubles as the open probe: its
/// failure is the GNU "cannot open" diagnostic and suppresses the header,
/// exactly as an unopenable file prints no header there. Returns whether
/// the source was served cleanly.
fn serve_path(
    job: &Job,
    path: &str,
    files: &dyn FileSource,
    out: &dyn Output,
    err: &dyn Output,
    show_headers: bool,
    header_written: &mut bool,
) -> Result<bool, HeadError> {
    let mut buf = vec![0u8; READ_CHUNK];
    let first = match files.read(path, 0, &mut buf) {
        Ok(n) => n,
        Err(errno) => {
            diagnose(err, &format!("cannot open '{path}' for reading: {errno}"))?;
            return Ok(false);
        }
    };
    if show_headers {
        write_header(out, path, header_written).map_err(HeadError::Output)?;
    }

    let mut engine = Engine::new(job);
    engine.feed(&buf[..first], out).map_err(HeadError::Output)?;
    let mut offset = first as u64;
    if first > 0 {
        while !engine.done() {
            match files.read(path, offset, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    engine.feed(&buf[..n], out).map_err(HeadError::Output)?;
                    offset += n as u64;
                }
                Err(errno) => {
                    diagnose(err, &format!("error reading '{path}': {errno}"))?;
                    return Ok(false);
                }
            }
        }
    }
    engine.finish(out).map_err(HeadError::Output)?;
    Ok(true)
}

/// One `head: …` diagnostic line on the error stream. A failure to report
/// is itself fatal — the tool never continues silently.
fn diagnose(err: &dyn Output, message: &str) -> Result<(), HeadError> {
    err.write_all(format!("head: {message}\n").as_bytes())
        .map_err(HeadError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::BTreeMap;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use rustos_abi::Errno;
    use rustos_help::{HelpSource, SourceError};

    use super::{run, TailRing, USAGE};
    use crate::command::{parse, Command};
    use crate::error::HeadError;
    use crate::io::{FileSource, Input, Output};

    /// An in-memory filesystem serving reads in bounded slices so a
    /// streamed file crosses several read calls.
    struct MemFiles {
        files: BTreeMap<String, Vec<u8>>,
        /// Largest slice one read call returns.
        chunk: usize,
        /// Fail reads of this path at or beyond this offset, to model a
        /// mid-stream I/O error.
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

        fn text(&self) -> String {
            String::from_utf8(self.bytes()).expect("fixture output is UTF-8")
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

    /// A bundle with no help documents, so the short help falls back to
    /// the usage banner.
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

    /// Run `args` against the fixtures, returning (all-ok, stdout, stderr).
    fn run_case(args: &[&str], files: &MemFiles, stdin: &MemStdin) -> (bool, Vec<u8>, String) {
        let out = Sink::default();
        let err = Sink::default();
        let ok = run(command(args), None, files, stdin, &NoHelp, &out, &err)
            .expect("fixture output streams never fail");
        (ok, out.bytes(), err.text())
    }

    #[test]
    fn default_is_the_first_ten_lines_of_stdin() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n");
        let (ok, out, err) = run_case(&[], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
        assert!(err.is_empty());
    }

    #[test]
    fn first_bytes_across_chunked_reads() {
        let files = MemFiles::new(&[]);
        let mut stdin = MemStdin::new(b"hello world");
        stdin.chunk = 2;
        let (ok, out, _) = run_case(&["-c", "5"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"hello");
    }

    #[test]
    fn first_lines_stops_at_the_counted_delimiter() {
        let mut files = MemFiles::new(&[("f", b"a\nb\nc\n")]);
        files.chunk = 3;
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-n", "2", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"a\nb\n");
    }

    #[test]
    fn fewer_lines_than_asked_serves_the_whole_source() {
        let files = MemFiles::new(&[("f", b"only\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-n", "9", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"only\n");
    }

    #[test]
    fn zero_terminated_lines_use_the_nul_delimiter() {
        let files = MemFiles::new(&[("f", b"a\0b\0")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-z", "-n", "1", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"a\0");
    }

    #[test]
    fn headers_name_each_of_several_sources() {
        let files = MemFiles::new(&[("a", b"A\n"), ("b", b"B\n")]);
        let stdin = MemStdin::new(b"S\n");
        let (ok, out, _) = run_case(&["a", "-", "b"], &files, &stdin);
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
        let (_, out, _) = run_case(&["-v", "a"], &files, &stdin);
        assert_eq!(out, b"==> a <==\nA\n");
        let (_, out, _) = run_case(&["-q", "a", "b"], &files, &stdin);
        assert_eq!(out, b"A\nB\n");
    }

    #[test]
    fn elide_bytes_keeps_all_but_the_tail() {
        let files = MemFiles::new(&[("f", b"hello world")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-c", "-3", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"hello wo");
    }

    #[test]
    fn elide_bytes_larger_than_the_input_emits_nothing() {
        let files = MemFiles::new(&[("f", b"tiny")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, _) = run_case(&["-c", "-99", "f"], &files, &stdin);
        assert!(ok);
        assert_eq!(out, b"");
    }

    #[test]
    fn elide_lines_counts_an_unterminated_tail_as_a_line() {
        let files = MemFiles::new(&[("t", b"a\nb"), ("n", b"a\nb\n")]);
        let stdin = MemStdin::new(b"");
        let (_, out, _) = run_case(&["-q", "-n", "-1", "t"], &files, &stdin);
        assert_eq!(out, b"a\n");
        let (_, out, _) = run_case(&["-q", "-n", "-1", "n"], &files, &stdin);
        assert_eq!(out, b"a\n");
    }

    #[test]
    fn elide_zero_serves_everything() {
        let files = MemFiles::new(&[("f", b"a\nb")]);
        let stdin = MemStdin::new(b"");
        let (_, out, _) = run_case(&["-n", "-0", "f"], &files, &stdin);
        assert_eq!(out, b"a\nb");
        let (_, out, _) = run_case(&["-c", "-0", "f"], &files, &stdin);
        assert_eq!(out, b"a\nb");
    }

    #[test]
    fn a_missing_file_is_diagnosed_and_the_run_continues() {
        let files = MemFiles::new(&[("b", b"B\n")]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["missing", "b"], &files, &stdin);
        assert!(!ok);
        // No header for the unopenable file; the next file's header carries
        // no leading blank line because none was printed before it.
        assert_eq!(out, b"==> b <==\nB\n");
        assert!(
            err.starts_with("head: cannot open 'missing' for reading:"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn a_mid_stream_read_error_is_diagnosed() {
        let mut files = MemFiles::new(&[("f", b"abcdef")]);
        files.chunk = 2;
        files.fail_at = Some((String::from("f"), 4));
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-c", "6", "f"], &files, &stdin);
        assert!(!ok);
        assert_eq!(out, b"abcd");
        assert!(
            err.starts_with("head: error reading 'f':"),
            "unexpected diagnostic: {err}"
        );
    }

    #[test]
    fn short_help_falls_back_to_the_usage_banner() {
        let files = MemFiles::new(&[]);
        let stdin = MemStdin::new(b"");
        let (ok, out, err) = run_case(&["-h"], &files, &stdin);
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
        );
        assert_eq!(result, Err(HeadError::Output(Errno::NotImplemented)));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = rustos_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/head.md");
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
                    "{locale}/head.md must document {switch}"
                );
            }
        }
    }

    /// The ring emits exactly the stream minus its final `keep` bytes for
    /// every chunking pattern, including chunks larger than the window.
    #[test]
    fn tail_ring_matches_the_reference_for_varied_chunkings() {
        let data: Vec<u8> = (0u16..251).map(|v| (v % 251) as u8).collect();
        for keep in [0usize, 1, 3, 7, 64, 250, 251, 300] {
            for chunk in [1usize, 2, 3, 5, 8, 63, 64, 65, 251] {
                let out = Sink::default();
                let mut ring = TailRing::new(keep as u64);
                for piece in data.chunks(chunk) {
                    ring.push(piece, &out).expect("sink never fails");
                }
                let expected = &data[..data.len().saturating_sub(keep)];
                assert_eq!(
                    out.bytes(),
                    expected,
                    "keep={keep} chunk={chunk} diverged from the reference"
                );
            }
        }
    }
}
