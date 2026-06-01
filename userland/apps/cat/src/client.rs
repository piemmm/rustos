//! The streaming engine: pull bytes from each source and write them —
//! optionally line-numbered — to the terminal.

use alloc::format;
use alloc::vec::Vec;

use rustos_abi::Errno;

use crate::command::{Command, Source};
use crate::error::CatError;
use crate::io::{FileSource, Input, Output};

/// The usage banner printed by [`Command::Help`].
pub const USAGE: &str = "\
usage: cat [-n] [--] [file...]

  -n, --number   number output lines, continuously across every source
  -h, --help     show this message

With no file operand, or when a file operand is `-`, cat reads standard
input. `--` ends option parsing: every later argument is a file path.
";

/// Bytes pulled from a source per read call.
///
/// A fixed chunk bounds the per-call buffer so a source of any size streams
/// through a constant amount of memory.
const READ_CHUNK: usize = 4096;

/// Run one [`Command`], reading its sources through `files`/`stdin` and
/// writing the rendered bytes to `out`.
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
    files: &dyn FileSource,
    stdin: &dyn Input,
    out: &dyn Output,
) -> Result<(), CatError> {
    match command {
        Command::Help => out.write_all(USAGE.as_bytes()).map_err(CatError::Output),
        Command::Concat { number, sources } => {
            let mut emitter = Emitter::new(number);
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
        // malformed; refuse it rather than index out of bounds (§2.9).
        let chunk = buf
            .get(..read)
            .ok_or(CatError::Read(Errno::LengthOutOfRange))?;
        emitter.emit(chunk, out)?;
    }
}

/// Renders byte chunks to the terminal, optionally prefixing each line with a
/// continuous line number.
///
/// The line state is carried across chunks and across sources, so a line that
/// straddles a chunk boundary — or a file boundary — is numbered exactly
/// once, when its first byte appears.
struct Emitter {
    number: bool,
    line_no: u64,
    at_line_start: bool,
}

impl Emitter {
    fn new(number: bool) -> Self {
        Self {
            number,
            line_no: 1,
            at_line_start: true,
        }
    }

    /// Emit `chunk`, prefixing line numbers when numbering is enabled.
    fn emit(&mut self, chunk: &[u8], out: &dyn Output) -> Result<(), CatError> {
        if !self.number {
            return out.write_all(chunk).map_err(CatError::Output);
        }
        let mut rendered = Vec::with_capacity(chunk.len());
        for &byte in chunk {
            if self.at_line_start {
                rendered.extend_from_slice(format!("{:>6}\t", self.line_no).as_bytes());
                self.line_no = self.line_no.saturating_add(1);
                self.at_line_start = false;
            }
            rendered.push(byte);
            if byte == b'\n' {
                self.at_line_start = true;
            }
        }
        out.write_all(&rendered).map_err(CatError::Output)
    }
}

#[cfg(test)]
mod tests {
    use super::{run, USAGE};
    use crate::command::{Command, Source};
    use crate::error::CatError;
    use crate::io::{FileSource, Input, Output};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::Errno;

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

    fn concat(number: bool, sources: Vec<Source>) -> Command {
        Command::Concat { number, sources }
    }

    fn path(name: &str) -> Source {
        Source::Path(String::from(name))
    }

    #[test]
    fn help_writes_usage() {
        let fs = MapFs::new();
        let out = Recorder::new();
        assert_eq!(
            run(Command::Help, &fs, &StdinFixture::empty(), &out),
            Ok(())
        );
        assert_eq!(out.text(), USAGE);
    }

    #[test]
    fn single_file_round_trips_bytes() {
        let fs = MapFs::new().with("a.txt", b"hello world\n");
        let out = Recorder::new();
        assert_eq!(
            run(
                concat(false, vec![path("a.txt")]),
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
            run(
                concat(false, vec![path("a"), path("b")]),
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
            run(
                concat(false, vec![Source::Stdin]),
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
            run(
                concat(false, vec![path("a"), Source::Stdin, path("b")]),
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
            run(
                concat(true, vec![path("a"), path("b")]),
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
            run(
                concat(true, vec![path("a")]),
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
            run(
                concat(true, vec![path("big")]),
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
            run(
                concat(true, vec![path("empty")]),
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
            run(
                concat(false, vec![path("absent")]),
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
            run(
                concat(false, vec![path("absent"), path("a")]),
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
            run(
                concat(false, vec![path("a")]),
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
            run(
                concat(false, vec![path("big")]),
                &fs,
                &StdinFixture::empty(),
                &out
            ),
            Ok(())
        );
        assert_eq!(out.bytes.borrow().as_slice(), data.as_slice());
    }
}
