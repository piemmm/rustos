//! The read-eval-print loop that drives a [`Shell`] over its inherited
//! standard streams (`plans/PI.md` P6e-3b).
//!
//! [`Shell`] is a pure interpreter: it decides *what* to run but never reads
//! input or writes output itself. This module is the loop around it. It reads
//! command lines from standard input (fd 0), writes the prompt through the
//! shell's [`Console`] (fd 1), feeds each complete line to
//! [`Shell::run_line`], and stops when the `exit` builtin requests it or the
//! input stream ends. Advisory metadata about the session goes to the
//! standard information stream (fd 3).
//!
//! # Read line discipline
//!
//! Line assembly runs the read line discipline's **buffer** half
//! ([`rustos_vt::line`]), the one shared definition the kernel console echo
//! and login's prompt reads also key off: CR and LF both terminate a line
//! (a serial terminal sends CR for the Return key, a pipe or script LF, and
//! a CRLF pair counts once), and the erase control (Backspace / Delete)
//! rubs out the last kept byte exactly as the echo rubs it off the screen.
//!
//! # Streams, never devices
//!
//! The loop reaches the outside world only through the injected [`ReplInput`]
//! seam and the shell's [`Console`]. On a running kernel those are backed by
//! the `abi-v1` standard-stream syscalls (`stream_read` / `stream_write`); in
//! tests they are in-memory fixtures. The loop never names a console, UART, or
//! framebuffer: binding to a device would be ambient authority and hidden coupling, and the same loop must work
//! whatever the spawner backed the streams with (device independence is
//! a property of the stream layer, not the program).
//!
//! # End of input
//!
//! *Blocking* until a byte arrives is the stream backing's responsibility, not
//! the program's: the kernel-core console backing parks an empty-handed reader
//! and wakes it when UART RX (or any other stream source) delivers bytes, so
//! an interactive session waits at the prompt rather than spinning. A
//! zero-length read therefore means the input stream has genuinely ended — a
//! closed pipe, an exhausted scripted transcript, or no backing at all — which
//! the loop treats as a clean session end.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{Human, Severity, StdInfoKind, StdInfoRecord};
use rustos_vt::control;
use rustos_vt::line::{push_line_byte, LineFeed};

use crate::host::Console;
use crate::parser::CommandList;
use crate::shell::Shell;

/// The interactive prompt written to standard output before each line.
const PROMPT: &str = "elsh$ ";

/// The continuation prompt written while collecting here-document body lines.
const HERE_DOC_PROMPT: &str = "> ";

/// Maximum length of a single command line, in bytes. A line longer than this
/// is discarded rather than buffered without bound, so untrusted input cannot
/// drive unbounded kernel-heap growth. The discarded
/// line is reported on `stdinfo` (fd 3).
const MAX_LINE: usize = 4096;

/// Size of each `stdin` read the loop issues. The loop reassembles lines
/// across reads, so this only bounds one syscall's transfer.
const READ_CHUNK: usize = 256;

/// `stdinfo` producer name for the records this loop emits.
const PRODUCER: &str = "elsh";

/// The loop's standard-input (fd 0) and standard-information (fd 3) seam.
///
/// Standard output and standard error are the shell's [`Console`]; this seam
/// carries the two streams the [`Console`] does not: the line input the loop
/// reads, and the optional advisory metadata it emits.
pub trait ReplInput {
    /// Read up to `buf.len()` bytes from standard input (fd 0), returning the
    /// number read. The stream *backing* owns blocking: a
    /// read with no pending input waits in the backing until input arrives,
    /// so a return of `0` means the input stream has genuinely ended (or has
    /// no backing at all), which the loop treats as end of input.
    fn read(&mut self, buf: &mut [u8]) -> usize;

    /// Write one framed advisory record to the standard information stream
    /// (fd 3). Best-effort and ignorable: it must never
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

/// Assembles edited command lines from `stdin` reads by running the read
/// line discipline's **buffer** half ([`rustos_vt::line`]) over each input
/// byte — the same shared definition login's prompt reads and the kernel
/// console echo key off, so what the reader keeps always matches what the
/// screen shows. CR and LF both terminate a line (a serial terminal sends CR
/// for the Return key, a pipe or script LF, and CRLF counts once); the erase
/// control (Backspace / Delete) rubs out the last kept byte.
struct LineReader {
    /// Bytes read from `stdin` but not yet fed to the discipline.
    raw: Vec<u8>,
    /// Index of the next unprocessed byte in [`Self::raw`].
    pos: usize,
    /// The line under edit: a fixed [`MAX_LINE`]-byte buffer the discipline
    /// edits in place, with [`Self::len`] bytes accepted so far.
    line: Vec<u8>,
    /// Accepted length of the line under edit.
    len: usize,
    /// Set once a read returned zero: no more input will arrive.
    eof: bool,
    /// Set after an over-length line: bytes are dropped up to and including
    /// the next line terminator so the tail of the discarded line is not
    /// mistaken for a fresh command.
    discarding: bool,
    /// Set after a CR terminated a line: an immediately following LF is the
    /// second half of a CRLF pair and is swallowed rather than completing a
    /// spurious empty line. Carried in the reader because the pair can be
    /// split across two reads.
    skip_lf: bool,
}

impl LineReader {
    fn new() -> Self {
        Self {
            raw: Vec::new(),
            pos: 0,
            line: alloc::vec![0u8; MAX_LINE],
            len: 0,
            eof: false,
            discarding: false,
            skip_lf: false,
        }
    }

    /// Read one more chunk from `input` into [`Self::raw`], setting
    /// [`Self::eof`] on a zero-length read.
    fn fill(&mut self, input: &mut dyn ReplInput) {
        let mut chunk = [0u8; READ_CHUNK];
        let read = input.read(&mut chunk);
        let read = read.min(READ_CHUNK);
        if read == 0 {
            self.eof = true;
        } else {
            self.raw.extend_from_slice(&chunk[..read]);
        }
    }

    /// Take the finished line out of the edit buffer, decoding it lossily so
    /// invalid UTF-8 becomes the replacement character rather than aborting
    /// the session, and reset the buffer for the next line.
    fn take_line(&mut self) -> String {
        let line = String::from_utf8_lossy(&self.line[..self.len]).into_owned();
        self.line[..self.len].fill(0);
        self.len = 0;
        line
    }

    /// Return the next complete command line, reading from `input` as needed.
    fn next_line(&mut self, input: &mut dyn ReplInput) -> LineEvent {
        loop {
            while self.pos < self.raw.len() {
                let byte = self.raw[self.pos];
                self.pos += 1;
                if core::mem::take(&mut self.skip_lf) && byte == control::LF {
                    continue;
                }
                if self.discarding {
                    if byte == control::CR || byte == control::LF {
                        self.discarding = false;
                        self.skip_lf = byte == control::CR;
                    }
                    continue;
                }
                match push_line_byte(&mut self.line, &mut self.len, byte) {
                    LineFeed::Pending => {}
                    LineFeed::Complete => {
                        self.skip_lf = byte == control::CR;
                        return LineEvent::Line(self.take_line());
                    }
                    // The line outgrew the limit: drop what was kept and
                    // discard the remainder up to the next terminator, so the
                    // tail of the dropped line never runs as a command.
                    LineFeed::TooLong => {
                        self.line[..self.len].fill(0);
                        self.len = 0;
                        self.discarding = true;
                        return LineEvent::Truncated;
                    }
                }
            }

            // Every read byte is consumed; drop the exhausted backlog.
            self.raw.clear();
            self.pos = 0;

            if self.eof {
                if self.len > 0 {
                    // A final line with no trailing terminator.
                    return LineEvent::Line(self.take_line());
                }
                return LineEvent::Eof;
            }

            self.fill(input);
        }
    }
}

/// Emit the over-length-line advisory on `stdinfo` (fd 3).
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

/// Collect the pending here-document bodies of a parsed line, prompting with
/// the continuation prompt for each body line.
///
/// A truncated (over-length) body line is reported on `stdinfo` and poisons
/// the document: its body can no longer be trusted, so the line will fail
/// closed, but collection still runs to the terminator so the remaining body
/// lines are never misread as commands. End of input leaves the document
/// unterminated, and running the list then fails it closed.
fn collect_here_docs(
    list: &mut CommandList,
    console: &dyn Console,
    input: &mut dyn ReplInput,
    reader: &mut LineReader,
) {
    while list.pending_here_doc().is_some() {
        console.write_stdout(HERE_DOC_PROMPT);
        match reader.next_line(input) {
            LineEvent::Line(line) => list.feed_here_doc_line(&line),
            LineEvent::Truncated => {
                report_truncated_line(input);
                list.poison_pending_here_doc();
            }
            LineEvent::Eof => return,
        }
    }
}

/// Run the read-eval-print loop until the shell requests exit or input ends,
/// returning the session's exit code.
///
/// The loop writes the prompt to standard output, reads one command line from
/// `input`, and runs it through `shell`. A line that does not parse is already
/// reported through the shell's `Console`, so its [`crate::ParseError`] is
/// dropped here — a bad line is not a fatal session error.
/// The loop ends when the `exit` builtin sets [`Shell::exit_request`] or the
/// input stream ends; on end-of-input with no explicit `exit`, the session
/// exits `0`.
pub fn run(shell: &mut Shell<'_>, console: &dyn Console, input: &mut dyn ReplInput) -> i32 {
    let mut reader = LineReader::new();
    loop {
        console.write_stdout(PROMPT);
        match reader.next_line(input) {
            LineEvent::Line(line) => {
                if let Ok(mut list) = shell.parse_line(&line) {
                    collect_here_docs(&mut list, console, input, &mut reader);
                    let _ = shell.run_list(&list);
                }
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
        /// Upper bound on the bytes one `read` hands out, so a test can force
        /// a line (or a CRLF pair) to be split across reads.
        max_chunk: usize,
    }

    impl ScriptedInput {
        fn new(input: &str) -> Self {
            Self {
                input: input.as_bytes().to_vec(),
                pos: 0,
                info: Vec::new(),
                max_chunk: usize::MAX,
            }
        }

        /// A transcript handed out at most `max_chunk` bytes per read.
        fn chunked(input: &str, max_chunk: usize) -> Self {
            Self {
                max_chunk,
                ..Self::new(input)
            }
        }
    }

    impl ReplInput for ScriptedInput {
        fn read(&mut self, buf: &mut [u8]) -> usize {
            let remaining = &self.input[self.pos..];
            let take = remaining.len().min(buf.len()).min(self.max_chunk);
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
    fn here_document_body_is_collected_across_lines() {
        use crate::host::RedirAction;
        use alloc::string::ToString;

        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("cat <<EOF\nhello\nworld\nEOF\n");
        assert_eq!(run(&mut shell, &console, &mut input), 0);

        let launches = host.launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(
            launches[0].commands[0].redirections[0].action,
            RedirAction::HereString {
                content: "hello\nworld\n".to_string(),
            }
        );
        // The continuation prompt precedes each of the three body reads.
        assert_eq!(console.stdout().matches(super::HERE_DOC_PROMPT).count(), 3);
    }

    #[test]
    fn here_document_hitting_end_of_input_fails_closed() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        // The input ends before the `EOF` terminator line.
        let mut input = ScriptedInput::new("cat <<EOF\npartial body\n");
        assert_eq!(run(&mut shell, &console, &mut input), 0);

        assert!(host.launches().is_empty(), "nothing runs");
        assert!(console.stderr().contains("missing its terminator"));
    }

    #[test]
    fn here_document_with_a_truncated_body_line_fails_closed() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        // One body line exceeds the line limit and is dropped; the body can
        // no longer be trusted, so the command must not run — but collection
        // still finds the terminator and the following line still runs.
        let mut transcript = String::from("cat <<EOF\n");
        transcript.push_str(&"y".repeat(MAX_LINE + 50));
        transcript.push_str("\nEOF\necho after\n");
        let mut input = ScriptedInput::new(&transcript);
        assert_eq!(run(&mut shell, &console, &mut input), 0);

        assert!(
            host.launches().is_empty(),
            "the here-doc command never runs"
        );
        assert!(console.stderr().contains("here-document too large"));
        assert!(console.stdout().contains("after\n"), "the next line runs");
        // The dropped line was reported on fd 3.
        assert_eq!(input.info.len(), 1);
        assert!(input.info[0].contains("shell.line_truncated"));
    }

    #[test]
    fn a_cr_terminated_line_runs() {
        // The regression that motivated the shared read line discipline: a
        // serial terminal (and the keymap's Enter encoding) sends a bare CR
        // for the Return key, never an LF, and the shell must still run the
        // line rather than waiting forever for an LF.
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("echo hi\rexit 3\r");
        assert_eq!(run(&mut shell, &console, &mut input), 3);
        assert!(console.stdout().contains("hi\n"));
    }

    #[test]
    fn a_crlf_pair_counts_as_one_terminator() {
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("echo a\r\nexit 5\r\n");
        assert_eq!(run(&mut shell, &console, &mut input), 5);
        let out = console.stdout();
        assert!(out.contains("a\n"));
        // Two consumed lines mean two prompts: the LF of each CRLF completes
        // no spurious empty third line.
        assert_eq!(out.matches(PROMPT).count(), 2);
    }

    #[test]
    fn a_crlf_pair_split_across_reads_counts_once() {
        // One byte per read forces every CR and LF into its own read, so the
        // pair-collapse state must survive across reads.
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::chunked("echo a\r\nexit 5\r\n", 1);
        assert_eq!(run(&mut shell, &console, &mut input), 5);
        let out = console.stdout();
        assert!(out.contains("a\n"));
        assert_eq!(out.matches(PROMPT).count(), 2);
    }

    #[test]
    fn an_erase_edits_the_kept_line() {
        // A typo rubbed out with Backspace (DEL) must leave the kept bytes
        // matching what the kernel echo left on screen.
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut input = ScriptedInput::new("echoZ\x7f ok\n");
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        assert!(console.stdout().contains("ok\n"));
    }

    #[test]
    fn an_over_length_cr_terminated_line_recovers() {
        // The over-length discard must also end at a CR terminator, so an
        // interactive session (which never sends LF) recovers at the next
        // Return.
        let host = ScriptedHost::new();
        let console = RecordingConsole::new();
        let mut shell = Shell::new(&host, &console);
        let mut transcript = "y".repeat(MAX_LINE + 50);
        transcript.push('\r');
        transcript.push_str("echo after\r");
        let mut input = ScriptedInput::new(&transcript);
        assert_eq!(run(&mut shell, &console, &mut input), 0);
        let out = console.stdout();
        assert!(out.contains("after\n"), "the next line runs: {out:?}");
        assert!(!out.contains("yyyy"), "the dropped line never runs");
        assert_eq!(input.info.len(), 1);
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
