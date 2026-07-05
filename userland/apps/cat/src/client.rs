//! The streaming engine: pull bytes from each source and write them —
//! optionally line-numbered — to the terminal.

use alloc::format;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_help::{own_short_help, HelpSource};

use crate::command::{Command, Numbering, Render, Source};
use crate::error::CatError;
use crate::io::{FileSource, Input, Output};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `cat`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: cat [-AbeEnstTuv] [--] [file...]";

/// `cat`'s own command word: the short-help switches render its own Help
/// document through the same engine as any other command's.
const OWN_WORD: &str = "cat";

/// Bytes pulled from a source per read call.
///
/// A fixed chunk bounds the per-call buffer so a source of any size streams
/// through a constant amount of memory.
const READ_CHUNK: usize = 4096;

/// Run one [`Command`], reading its sources through `files`/`stdin` and
/// writing the rendered bytes to `out`. `locale` is the user's `LANG`
/// preference, if set; `help` is the tool's own `Help/` tree, read by the
/// short-help switches.
///
/// Sources are read in order; numbering (when enabled) is continuous across
/// every source, exactly like the POSIX tool.
///
/// # Errors
///
/// * [`CatError::Read`] — a source could not be read; carries the
///   underlying [`Errno`] (e.g. [`Errno::NotFound`]).
/// * [`CatError::Output`] — writing the terminal failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    files: &dyn FileSource,
    stdin: &dyn Input,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), CatError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Concat { render, sources } => {
            let mut emitter = Emitter::new(render);
            for source in &sources {
                match source {
                    Source::Stdin => pump(|buf| stdin.read(buf), &mut emitter, out)?,
                    Source::Path(path) => {
                        let mut offset: u64 = 0;
                        pump(
                            |buf| {
                                let read = files.read(path, offset, buf)?;
                                offset = offset.saturating_add(read as u64);
                                Ok(read)
                            },
                            &mut emitter,
                            out,
                        )?;
                    }
                }
            }
            Ok(())
        }
    }
}

/// Render `cat`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own Help tree through the one shared engine; when no document
/// can be served (a build without the bundle's documents) the usage banner
/// stands in — the tool's own text, not fabricated help content — so `-h`
/// never fails.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), CatError> {
    let bytes =
        own_short_help(help, locale, OWN_WORD).unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
    out.write_all(&bytes).map_err(CatError::Output)
}

/// Drain one source — calling `fill` for successive chunks until it reports
/// end-of-stream (`0`) — feeding each chunk through `emitter` to `out`.
fn pump<F>(mut fill: F, emitter: &mut Emitter, out: &dyn Output) -> Result<(), CatError>
where
    F: FnMut(&mut [u8]) -> Result<usize, Errno>,
{
    let mut buf = [0u8; READ_CHUNK];
    loop {
        let read = fill(&mut buf).map_err(CatError::Read)?;
        if read == 0 {
            return Ok(());
        }
        // A source that claims to have written more than the buffer holds is
        // malformed; refuse it rather than index out of bounds.
        let chunk = buf
            .get(..read)
            .ok_or(CatError::Read(Errno::LengthOutOfRange))?;
        emitter.emit(chunk, out)?;
    }
}

/// Renders byte chunks to the terminal, applying the [`Render`] options:
/// line numbering, blank-line squeezing, end-of-line markers, and the
/// `^`/`M-` visible notation.
///
/// The line state is carried across chunks and across sources, so a line that
/// straddles a chunk boundary — or a file boundary — is numbered exactly
/// once, when its first byte appears, and a blank-line run that straddles a
/// boundary squeezes correctly.
struct Emitter {
    render: Render,
    line_no: u64,
    at_line_start: bool,
    prev_line_blank: bool,
}

impl Emitter {
    fn new(render: Render) -> Self {
        Self {
            render,
            line_no: 1,
            at_line_start: true,
            prev_line_blank: false,
        }
    }

    /// Emit `chunk` through the render options.
    fn emit(&mut self, chunk: &[u8], out: &dyn Output) -> Result<(), CatError> {
        if self.render == Render::PLAIN {
            return out.write_all(chunk).map_err(CatError::Output);
        }
        let mut rendered = Vec::with_capacity(chunk.len());
        for &byte in chunk {
            if byte == b'\n' {
                let blank = self.at_line_start;
                self.at_line_start = true;
                if blank {
                    if self.render.squeeze_blank && self.prev_line_blank {
                        continue;
                    }
                    self.prev_line_blank = true;
                    // Only `-n` numbers a blank line; `-b` leaves it bare.
                    if self.render.numbering == Numbering::All {
                        self.push_line_number(&mut rendered);
                    }
                } else {
                    self.prev_line_blank = false;
                }
                if self.render.show_ends {
                    rendered.push(b'$');
                }
                rendered.push(b'\n');
                continue;
            }
            if self.at_line_start {
                if self.render.numbering != Numbering::None {
                    self.push_line_number(&mut rendered);
                }
                self.at_line_start = false;
            }
            self.push_visible(byte, &mut rendered);
        }
        out.write_all(&rendered).map_err(CatError::Output)
    }

    fn push_line_number(&mut self, rendered: &mut Vec<u8>) {
        rendered.extend_from_slice(format!("{:>6}\t", self.line_no).as_bytes());
        self.line_no = self.line_no.saturating_add(1);
    }

    /// Push one non-newline byte, applying the `-T` tab marker and the `-v`
    /// `^`/`M-` notation for control and non-ASCII bytes.
    fn push_visible(&self, byte: u8, rendered: &mut Vec<u8>) {
        if byte == b'\t' {
            if self.render.show_tabs {
                rendered.extend_from_slice(b"^I");
            } else {
                rendered.push(b'\t');
            }
            return;
        }
        if !self.render.show_nonprinting {
            rendered.push(byte);
            return;
        }
        match byte {
            0..=31 => {
                rendered.push(b'^');
                rendered.push(byte + 64);
            }
            127 => rendered.extend_from_slice(b"^?"),
            128..=255 => {
                rendered.extend_from_slice(b"M-");
                match byte - 128 {
                    low @ 0..=31 => {
                        rendered.push(b'^');
                        rendered.push(low + 64);
                    }
                    127 => rendered.extend_from_slice(b"^?"),
                    low => rendered.push(low),
                }
            }
            _ => rendered.push(byte),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::{Command, Numbering, Render, Source};
    use crate::error::CatError;
    use crate::io::{FileSource, Input, Output};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::Errno;
    use rustos_help::{HelpSource, SourceError};

    /// A Help tree with no documents at all: the short-help fallback path.
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

    /// A Help tree holding one canonical `cat.md` document.
    struct OneDoc;

    const DOC: &str = "## NAME\n\ncat — concatenate files to standard output\n\n\
                       ## SYNOPSIS\n\n`cat [-n] [--] [file...]`\n\n\
                       ## DESCRIPTION\n\nConcatenates things.\n";

    impl HelpSource for OneDoc {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(alloc::vec![String::from("default")])
        }

        fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
            if locale_dir == "default" && file_name == "cat.md" {
                Ok(Some(DOC.as_bytes().to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// An in-memory filesystem keyed by path.
    struct MapFs {
        files: Vec<(String, Vec<u8>)>,
    }

    impl MapFs {
        fn new() -> Self {
            Self { files: Vec::new() }
        }

        fn with(mut self, path: &str, bytes: &[u8]) -> Self {
            self.files.push((String::from(path), bytes.to_vec()));
            self
        }
    }

    impl FileSource for MapFs {
        fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let bytes = self
                .files
                .iter()
                .find(|(name, _)| name == path)
                .map(|(_, data)| data)
                .ok_or(Errno::NotFound)?;
            let start = usize::try_from(offset).map_err(|_| Errno::LengthOutOfRange)?;
            if start >= bytes.len() {
                return Ok(0);
            }
            let take = core::cmp::min(buf.len(), bytes.len() - start);
            buf[..take].copy_from_slice(&bytes[start..start + take]);
            Ok(take)
        }
    }

    /// Standard input backed by a byte buffer, drained on each read.
    struct StdinFixture {
        bytes: RefCell<Vec<u8>>,
    }

    impl StdinFixture {
        fn new(bytes: &[u8]) -> Self {
            Self {
                bytes: RefCell::new(bytes.to_vec()),
            }
        }

        fn empty() -> Self {
            Self::new(&[])
        }
    }

    impl Input for StdinFixture {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            let mut bytes = self.bytes.borrow_mut();
            if bytes.is_empty() {
                return Ok(0);
            }
            let take = core::cmp::min(buf.len(), bytes.len());
            buf[..take].copy_from_slice(&bytes[..take]);
            bytes.drain(..take);
            Ok(take)
        }
    }

    /// Captures every byte written; optionally fails on the Nth write call.
    struct Recorder {
        bytes: RefCell<Vec<u8>>,
        writes: RefCell<usize>,
        fail_at: Option<usize>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
                writes: RefCell::new(0),
                fail_at: None,
            }
        }

        fn failing_at(index: usize) -> Self {
            Self {
                bytes: RefCell::new(Vec::new()),
                writes: RefCell::new(0),
                fail_at: Some(index),
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.bytes.borrow().clone()).unwrap()
        }
    }

    impl Output for Recorder {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            let mut writes = self.writes.borrow_mut();
            if self.fail_at == Some(*writes) {
                return Err(Errno::NotFound);
            }
            *writes += 1;
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
    }

    fn concat(render: Render, sources: Vec<Source>) -> Command {
        Command::Concat { render, sources }
    }

    /// The plain pass-through rendering.
    fn plain() -> Render {
        Render::PLAIN
    }

    /// A rendering with `-n`-style all-lines numbering.
    fn numbered() -> Render {
        Render {
            numbering: Numbering::All,
            ..Render::PLAIN
        }
    }

    fn path(name: &str) -> Source {
        Source::Path(String::from(name))
    }

    fn run_cat(
        command: Command,
        files: &dyn FileSource,
        stdin: &dyn Input,
        out: &Recorder,
    ) -> Result<(), CatError> {
        run(command, None, files, stdin, &NoHelp, out)
    }

    #[test]
    fn help_renders_the_short_help_from_the_document() {
        let fs = MapFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Help,
                None,
                &fs,
                &StdinFixture::empty(),
                &OneDoc,
                &out
            ),
            Ok(())
        );
        let text = out.text();
        assert!(
            text.contains("cat — concatenate files to standard output"),
            "{text}"
        );
        assert!(text.contains("cat [-n] [--] [file...]"), "{text}");
    }

    #[test]
    fn help_falls_back_to_the_usage_banner_without_a_tree() {
        let fs = MapFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(
                Command::Help,
                None,
                &fs,
                &StdinFixture::empty(),
                &NoHelp,
                &out
            ),
            Ok(())
        );
        let mut expected = String::from(USAGE);
        expected.push('\n');
        assert_eq!(out.text(), expected);
    }

    #[test]
    fn single_file_round_trips_bytes() {
        let fs = MapFs::new().with("a.txt", b"hello world\n");
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("a.txt")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "hello world\n");
    }

    #[test]
    fn multiple_files_are_concatenated_in_order() {
        let fs = MapFs::new().with("a", b"one\n").with("b", b"two\n");
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("a"), path("b")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "one\ntwo\n");
    }

    #[test]
    fn stdin_is_streamed_by_default() {
        let fs = MapFs::new();
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(plain(), vec![Source::Stdin]),
                &fs,
                &StdinFixture::new(b"from stdin\n"),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "from stdin\n");
    }

    #[test]
    fn dash_reads_stdin_between_files() {
        let fs = MapFs::new().with("a", b"A\n").with("b", b"B\n");
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("a"), Source::Stdin, path("b")]),
                &fs,
                &StdinFixture::new(b"S\n"),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "A\nS\nB\n");
    }

    #[test]
    fn numbering_is_continuous_across_files() {
        let fs = MapFs::new()
            .with("a", b"first\nsecond\n")
            .with("b", b"third\n");
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(numbered(), vec![path("a"), path("b")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "     1\tfirst\n     2\tsecond\n     3\tthird\n");
    }

    #[test]
    fn numbering_handles_a_missing_trailing_newline() {
        let fs = MapFs::new().with("a", b"no newline");
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(numbered(), vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "     1\tno newline");
    }

    #[test]
    fn numbering_a_line_split_across_chunks_numbers_once() {
        // A file larger than READ_CHUNK with no newline is one line; it must
        // be numbered exactly once even though it spans several reads.
        let mut data = Vec::new();
        data.resize(super::READ_CHUNK + 100, b'x');
        let fs = MapFs::new().with("big", &data);
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(numbered(), vec![path("big")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        let text = out.text();
        // Exactly one line-number prefix, then the data.
        assert!(text.starts_with("     1\t"));
        assert_eq!(text.matches('\t').count(), 1);
        assert_eq!(text.len(), "     1\t".len() + data.len());
    }

    #[test]
    fn empty_file_with_numbering_emits_nothing() {
        let fs = MapFs::new().with("empty", b"");
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(numbered(), vec![path("empty")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn missing_file_fails_closed() {
        let fs = MapFs::new();
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("absent")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Err(CatError::Read(Errno::NotFound))
        );
    }

    #[test]
    fn a_read_error_stops_before_later_sources() {
        let fs = MapFs::new().with("a", b"A\n");
        let out = Recorder::new();
        // The missing first file aborts; "a" is never read.
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("absent"), path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Err(CatError::Read(Errno::NotFound))
        );
        assert_eq!(out.text(), "");
    }

    #[test]
    fn output_failure_propagates() {
        let fs = MapFs::new().with("a", b"hello\n");
        let out = Recorder::failing_at(0);
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Err(CatError::Output(Errno::NotFound))
        );
    }

    #[test]
    fn large_file_streams_in_chunks() {
        // Two-and-a-bit chunks exercise the multi-read streaming loop.
        let pattern = b"rustos!";
        let mut data = Vec::new();
        for i in 0..(super::READ_CHUNK * 2 + 7) {
            data.push(pattern[i % pattern.len()]);
        }
        let fs = MapFs::new().with("big", &data);
        let out = Recorder::new();
        assert_eq!(
            run_cat(
                concat(plain(), vec![path("big")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.bytes.borrow().as_slice(), data.as_slice());
    }

    #[test]
    fn nonblank_numbering_skips_blank_lines() {
        let fs = MapFs::new().with("a", b"one\n\ntwo\n\n\nthree\n");
        let out = Recorder::new();
        let render = Render {
            numbering: Numbering::NonBlank,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(
            out.text(),
            "     1\tone\n\n     2\ttwo\n\n\n     3\tthree\n"
        );
    }

    #[test]
    fn squeeze_blank_collapses_runs_to_one() {
        let fs = MapFs::new().with("a", b"one\n\n\n\ntwo\n\nthree\n");
        let out = Recorder::new();
        let render = Render {
            squeeze_blank: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "one\n\ntwo\n\nthree\n");
    }

    #[test]
    fn squeeze_blank_spans_source_boundaries() {
        // A blank-line run split across two files still squeezes to one.
        let fs = MapFs::new().with("a", b"one\n\n\n").with("b", b"\n\ntwo\n");
        let out = Recorder::new();
        let render = Render {
            squeeze_blank: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a"), path("b")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "one\n\ntwo\n");
    }

    #[test]
    fn squeeze_with_numbering_does_not_number_squeezed_lines() {
        let fs = MapFs::new().with("a", b"one\n\n\n\ntwo\n");
        let out = Recorder::new();
        let render = Render {
            numbering: Numbering::All,
            squeeze_blank: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "     1\tone\n     2\t\n     3\ttwo\n");
    }

    #[test]
    fn show_ends_marks_every_line() {
        let fs = MapFs::new().with("a", b"one\n\ntwo");
        let out = Recorder::new();
        let render = Render {
            show_ends: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        // The final line has no newline, so no `$` is invented for it.
        assert_eq!(out.text(), "one$\n$\ntwo");
    }

    #[test]
    fn show_ends_with_nonblank_numbering_leaves_blank_lines_bare() {
        let fs = MapFs::new().with("a", b"one\n\ntwo\n");
        let out = Recorder::new();
        let render = Render {
            numbering: Numbering::NonBlank,
            show_ends: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "     1\tone$\n$\n     2\ttwo$\n");
    }

    #[test]
    fn show_tabs_renders_caret_i() {
        let fs = MapFs::new().with("a", b"a\tb\n");
        let out = Recorder::new();
        let render = Render {
            show_tabs: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "a^Ib\n");
    }

    #[test]
    fn show_nonprinting_uses_caret_and_meta_notation() {
        // NUL, BEL, DEL, a meta control byte, a meta printable byte, and
        // M-DEL, with TAB and LFD left alone (only `-T` marks tabs).
        let fs = MapFs::new().with("a", &[0x00, 0x07, 0x7f, 0x80, 0xc1, 0xff, b'\t', b'\n']);
        let out = Recorder::new();
        let render = Render {
            show_nonprinting: true,
            ..Render::PLAIN
        };
        assert_eq!(
            run_cat(
                concat(render, vec![path("a")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.text(), "^@^G^?M-^@M-AM-^?\t\n");
    }
}
