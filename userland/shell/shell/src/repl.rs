//! The read-eval-print loop that drives a [`Shell`] over its inherited
//! standard streams (`AGENTS.md` §20, `plans/PI.md` P6e-3b).
//!
//! [`Shell`] is a pure interpreter: it decides *what* to run but never reads
//! input or writes output itself. This module is the loop around it. It reads
//! command lines from standard input (fd 0), writes the prompt through the
//! shell's [`Console`] (fd 1), feeds each complete line to
//! [`Shell::run_line`], and stops when the `exit` builtin requests it or the
//! input stream ends. Advisory metadata about the session goes to the
//! standard information stream (fd 3, `AGENTS.md` §20.1).
//!
//! # Streams, never devices
//!
//! The loop reaches the outside world only through the injected [`ReplInput`]
//! seam and the shell's [`Console`]. On a running kernel those are backed by
//! the `abi-v1` standard-stream syscalls (`stream_read` / `stream_write`); in
//! tests they are in-memory fixtures. The loop never names a console, UART, or
//! framebuffer: binding to a device would be ambient authority (`AGENTS.md`
//! §4) and hidden coupling (§17.3 / §17.4), and the same loop must work
//! whatever the spawner backed the streams with (§20 — device independence is
//! a property of the stream layer, not the program).
//!
//! # End of input
//!
//! A zero-length read means the input stream yielded nothing, which the loop
//! treats as end of input and a clean session end. *Blocking* until a byte
//! arrives is the stream backing's responsibility, not the program's (§20);
//! over a UART that does not yet block on receive, the session simply ends
//! when no input is pending — live interactive receive over silicon is an
//! on-metal item (`plans/PI.md` P6e-2 / P6e-3a).

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{Human, Severity, StdInfoKind, StdInfoRecord};

use crate::host::Console;
use crate::shell::Shell;

/// The interactive prompt written to standard output before each line.
const PROMPT: &str = "rustos$ ";

/// Maximum length of a single command line, in bytes. A line longer than this
/// is discarded rather than buffered without bound, so untrusted input cannot
/// drive unbounded kernel-heap growth (`AGENTS.md` §4 / §2.9). The discarded
/// line is reported on `stdinfo` (fd 3).
const MAX_LINE: usize = 4096;

/// Size of each `stdin` read the loop issues. The loop reassembles lines
/// across reads, so this only bounds one syscall's transfer.
const READ_CHUNK: usize = 256;

/// `stdinfo` producer name for the records this loop emits (`AGENTS.md` §20.1).
const PRODUCER: &str = "rustos-shell";

/// The loop's standard-input (fd 0) and standard-information (fd 3) seam.
///
/// Standard output and standard error are the shell's [`Console`]; this seam
/// carries the two streams the [`Console`] does not: the line input the loop
/// reads, and the optional advisory metadata it emits.
pub trait ReplInput {
    /// Read up to `buf.len()` bytes from standard input (fd 0), returning the
    /// number read. A return of `0` means no input is pending, which the loop
    /// treats as end of input.
    fn read(&mut self, buf: &mut [u8]) -> usize;

    /// Write one framed advisory record to the standard information stream
    /// (fd 3, `AGENTS.md` §20.1). Best-effort and ignorable: it must never
    /// affect correctness, so the implementation discards any short write.
    fn write_info(&mut self, bytes: &[u8]);
}

/// What [`LineReader::next_line`] produced.
enum LineEvent {
    /// A complete command line (newline stripped).
    Line(String),
    /// A line exceeded [`MAX_LINE`] and was discarded.
    Truncated,
    /// The input stream ended with no further complete line.
    Eof,
}

/// Reassembles newline-terminated command lines from `stdin` reads.
struct LineReader {
    /// Bytes read but not yet returned as a line.
    buf: Vec<u8>,
    /// Set once a read returned zero: no more input will arrive.
    eof: bool,
    /// Set after an over-length line: bytes are dropped up to and including
    /// the next newline so the tail of the discarded line is not mistaken for
    /// a fresh command.
    discarding: bool,
}

impl LineReader {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            eof: false,
            discarding: false,
        }
    }

    /// Read one more chunk from `input` into [`Self::buf`], setting
    /// [`Self::eof`] on a zero-length read.
    fn fill(&mut self, input: &mut dyn ReplInput) {
        let mut chunk = [0u8; READ_CHUNK];
        let read = input.read(&mut chunk);
        let read = read.min(READ_CHUNK);
        if read == 0 {
            self.eof = true;
        } else {
            self.buf.extend_from_slice(&chunk[..read]);
        }
    }

    /// Split off and return the bytes up to the first newline as a line.
    ///
    /// `newline` is the index of the `\n`. The newline and a preceding `\r`
    /// (CRLF) are stripped; the bytes are decoded lossily so invalid UTF-8
    /// becomes the replacement character rather than aborting the session.
    fn take_line(&mut self, newline: usize) -> String {
        let mut line: Vec<u8> = self.buf.drain(..=newline).collect();
        line.pop(); // the '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8_lossy(&line).into_owned()
    }

    /// Return the next complete command line, reading from `input` as needed.
    fn next_line(&mut self, input: &mut dyn ReplInput) -> LineEvent {
        loop {
            if self.discarding {
                if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                    self.buf.drain(..=pos);
                    self.discarding = false;
                } else {
                    self.buf.clear();
                    if self.eof {
                        return LineEvent::Eof;
                    }
                    self.fill(input);
                    continue;
                }
            }

            match self.buf.iter().position(|&b| b == b'\n') {
                // A newline within the limit completes a normal line.
                Some(pos) if pos <= MAX_LINE => return LineEvent::Line(self.take_line(pos)),
                // A newline beyond the limit: the whole over-length line
                // (including its newline) is discarded, not run.
                Some(pos) => {
                    self.buf.drain(..=pos);
                    return LineEvent::Truncated;
                }
                None => {}
            }

            // No newline yet. If the buffer has already grown past the limit
            // there is no point reading further: drop it and discard the
            // remainder of the line up to the next newline.
            if self.buf.len() > MAX_LINE {
                self.buf.clear();
                self.discarding = true;
                return LineEvent::Truncated;
            }

            if self.eof {
                if self.buf.is_empty() {
                    return LineEvent::Eof;
                }
                // A final line with no trailing newline.
                let mut line = core::mem::take(&mut self.buf);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return LineEvent::Line(String::from_utf8_lossy(&line).into_owned());
            }

            self.fill(input);
        }
    }
}

/// Emit the over-length-line advisory on `stdinfo` (fd 3, `AGENTS.md` §20.1).
///
/// This is the canonical `omission` kind: input was dropped. The record is
/// best-effort and ignorable; a serialisation failure is silently skipped, as
/// fd 3 must never affect correctness.
fn report_truncated_line(input: &mut dyn ReplInput) {
    let record = StdInfoRecord::new(
        PRODUCER,
        StdInfoKind::Omission,
        "shell.line_truncated",
        Severity::Info,
        Human::with_suggestion(
            "An input line exceeded the 4096-byte limit and was discarded.",
            "Shorten the command line.",
        ),
    );
    let mut buf = [0u8; 256];
    if let Ok(len) = record.write_jsonl(&mut buf) {
        input.write_info(&buf[..len]);
    }
}

/// Run the read-eval-print loop until the shell requests exit or input ends,
/// returning the session's exit code.
///
/// The loop writes the prompt to standard output, reads one command line from
/// `input`, and runs it through `shell`. A line that does not parse is already
/// reported through the shell's `Console`, so its [`crate::ParseError`] is
/// dropped here — a bad line is not a fatal session error (`AGENTS.md` §2.9).
/// The loop ends when the `exit` builtin sets [`Shell::exit_request`] or the
/// input stream ends; on end-of-input with no explicit `exit`, the session
/// exits `0`.
pub fn run(shell: &mut Shell<'_>, console: &dyn Console, input: &mut dyn ReplInput) -> i32 {
    let mut reader = LineReader::new();
    loop {
        console.write_stdout(PROMPT);
        match reader.next_line(input) {
            LineEvent::Line(line) => {
                let _ = shell.run_line(&line);
                if let Some(code) = shell.exit_request() {
                    return code;
                }
            }
            LineEvent::Truncated => {
                report_truncated_line(input);
            }
            LineEvent::Eof => return shell.exit_request().unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{run, ReplInput, MAX_LINE, PROMPT};
    use crate::test_support::{RecordingConsole, ScriptedHost};
    use crate::Shell;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// A scripted `stdin` / `stdinfo` seam: hands out a fixed input transcript
    /// in chunks and records every `stdinfo` record written.
    struct ScriptedInput {
        input: Vec<u8>,
        pos: usize,
        info: Vec<String>,
    }

    impl ScriptedInput {
        fn new(input: &str) -> Self {
            Self {
                input: input.as_bytes().to_vec(),
                pos: 0,
                info: Vec::new(),
            }
        }
    }

    impl ReplInput for ScriptedInput {
        fn read(&mut self, buf: &mut [u8]) -> usize {
            let remaining = &self.input[self.pos..];
            let take = remaining.len().min(buf.len());
            buf[..take].copy_from_slice(&remaining[..take]);
            self.pos += take;
            take
        }

        fn write_info(&mut self, bytes: &[u8]) {
            self.info.push(String::from_utf8_lossy(bytes).into_owned());
        }
    }

    #[test]
    fn prompt_is_written_before_reading_and_eof_ends_the_session() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("");
        // No input: the loop writes one prompt, reads end-of-input, and exits 0.
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        assert_eq!(console.stdout(), PROMPT);
    }

    #[test]
    fn builtin_lines_run_and_exit_builtin_sets_the_code() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("echo hello\nexit 7\necho never\n");
        assert_eq!(run(&mut shell, &console, &mut input), 7);
        let out = console.stdout();
        assert!(out.contains("hello\n"), "echo ran: {out:?}");
        assert!(
            !out.contains("never"),
            "lines after exit do not run: {out:?}"
        );
        // A prompt precedes each of the two consumed lines (echo, exit); the
        // third line is never reached.
        assert_eq!(out.matches(PROMPT).count(), 2);
    }

    #[test]
    fn a_line_split_across_reads_is_reassembled() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        // READ_CHUNK is 256; a line longer than one chunk must still parse as
        // one command. `echo` + a 300-'x' argument.
        let mut line = String::from("echo ");
        line.push_str(&"x".repeat(300));
        line.push('\n');
        let mut input = ScriptedInput::new(&line);
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        assert!(console.stdout().contains(&"x".repeat(300)));
    }

    #[test]
    fn a_final_line_without_a_newline_still_runs() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("echo tail");
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        assert!(console.stdout().contains("tail\n"));
    }

    #[test]
    fn an_over_length_line_is_discarded_and_reported_on_stdinfo() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        // One over-length line (no newline within the limit), then a real
        // command on the next line. The over-length line is dropped, its tail
        // is not mistaken for a command, and the following line runs.
        let mut transcript = "y".repeat(MAX_LINE + 50);
        transcript.push('\n');
        transcript.push_str("echo after\n");
        let mut input = ScriptedInput::new(&transcript);
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        let out = console.stdout();
        assert!(
            out.contains("after\n"),
            "the line after the dropped one runs: {out:?}"
        );
        assert!(
            !out.contains("yyyy"),
            "the dropped line never reaches the shell"
        );
        // Exactly one `omission` advisory was emitted on fd 3.
        assert_eq!(input.info.len(), 1);
        assert!(input.info[0].contains("\"kind\":\"omission\""));
        assert!(input.info[0].contains("shell.line_truncated"));
    }

    #[test]
    fn carriage_returns_are_stripped_from_crlf_lines() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("echo crlf\r\nexit 0\r\n");
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        let out = console.stdout();
        // The trailing '\r' must not survive into the echoed argument.
        assert!(out.contains("crlf\n"));
        assert!(!out.contains("crlf\r"));
    }
}
