//! The Supervisor read-eval-print loop and its line reader.
//!
//! [`run_supervisor`] is the engine entry point the boot path calls once the
//! operator has pressed `ESC`. It draws the `* ` prompt, reads one edited
//! command line through the shared read line discipline
//! ([`tairix_vt::line::LineEditor`] — the one definition login and the shell
//! also run), dispatches it, and loops until a command asks to leave. Every
//! read parks on the keyboard through the [`SupInput`] seam (never a
//! busy-spin), and nothing here allocates.

use tairix_vt::line::{LineEditor, LineFeed};

use crate::dispatch::{dispatch, Flow, Session};
use crate::{Report, SupInput, SupervisorEvent, SupervisorExit, SupervisorHost, MAX_LINE_LEN};

/// The Supervisor REPL prompt.
pub const PROMPT: &[u8] = b"* ";

/// The result of reading one edited line from the console.
pub(crate) enum LineOutcome {
    /// A complete line of the given length is in the buffer.
    Line(usize),
    /// The line outgrew the buffer; it was drained and must be ignored.
    TooLong,
    /// The input ended with no pending line.
    Eof,
}

/// Read one edited line into `buf`, returning what happened.
///
/// Reads a byte at a time through [`SupInput::read_byte`] (which parks until a
/// key arrives) and feeds it to a [`LineEditor`], so Backspace and the Delete
/// key edit the line exactly as they do at the login and shell prompts. When
/// `echo` is set, an accepted character is echoed and a rub-out draws the
/// `BS SP BS` erase — the pre-boot raw console has no cooked echo of its own,
/// so the reader provides it. A secret read (`echo` clear) renders nothing.
///
/// A lone `ESC` is not special to the command line — the boot-screen window,
/// not this reader, is where `ESC` drops to the Supervisor — so it is simply
/// held and never resolved here.
pub(crate) fn read_line(
    input: &mut dyn SupInput,
    buf: &mut [u8],
    echo: bool,
    out: &mut dyn Report,
) -> LineOutcome {
    let mut editor = LineEditor::new();
    let mut len = 0usize;
    loop {
        let Some(byte) = input.read_byte() else {
            return if len == 0 {
                LineOutcome::Eof
            } else {
                LineOutcome::Line(len)
            };
        };
        let before = len;
        match editor.push(buf, &mut len, byte) {
            LineFeed::Complete => {
                if echo {
                    out.write_bytes(b"\r\n");
                }
                return LineOutcome::Line(len);
            }
            LineFeed::TooLong => {
                drain_to_newline(input);
                return LineOutcome::TooLong;
            }
            LineFeed::Pending | LineFeed::Escape => {
                if echo {
                    if len > before {
                        // One or more bytes were appended: echo exactly them.
                        out.write_bytes(&buf[before..len]);
                    } else if len < before {
                        // A byte was rubbed out: erase it on screen too.
                        out.write_bytes(b"\x08 \x08");
                    }
                }
            }
        }
    }
}

/// Discard input up to and including the next line terminator, so a
/// subsequent read starts fresh. Stops on a terminator or end of input;
/// never spins.
fn drain_to_newline(input: &mut dyn SupInput) {
    while let Some(byte) = input.read_byte() {
        if byte == b'\n' || byte == b'\r' {
            return;
        }
    }
}

/// Run the Supervisor REPL to completion, returning how the boot should
/// proceed.
///
/// Entering the console is audited immediately. The loop then prompts, reads,
/// and dispatches until a control command returns [`Flow::Exit`] (or the
/// console closes, which resumes the normal boot fail-safe). The line buffer
/// is zeroed after each command so a typed argument does not linger.
pub fn run_supervisor(
    out: &mut dyn Report,
    input: &mut dyn SupInput,
    host: &mut dyn SupervisorHost,
) -> SupervisorExit {
    host.audit(SupervisorEvent::Entered);
    loop {
        out.write_bytes(PROMPT);
        let mut buf = [0u8; MAX_LINE_LEN];
        let outcome = read_line(&mut *input, &mut buf, true, &mut *out);
        match outcome {
            LineOutcome::Line(len) => {
                let mut session = Session {
                    out: &mut *out,
                    input: &mut *input,
                    host: &mut *host,
                };
                let flow = dispatch(&buf[..len], &mut session);
                buf.fill(0);
                if let Flow::Exit(exit) = flow {
                    return exit;
                }
            }
            LineOutcome::TooLong => {
                buf.fill(0);
                out.line("supervisor: input line too long; ignored.");
            }
            LineOutcome::Eof => {
                buf.fill(0);
                return SupervisorExit::ContinueBoot;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run_supervisor;
    use crate::commands::test_support::{MockHost, MockInput, VecReport};
    use crate::{SupervisorEvent, SupervisorExit};

    #[test]
    fn the_prompt_is_a_star_space() {
        assert_eq!(super::PROMPT, b"* ");
    }

    #[test]
    fn entering_the_repl_is_audited_then_continue_exits() {
        let mut out = VecReport::default();
        let mut input = MockInput::new(b"continue\n");
        let mut host = MockHost::default();
        let exit = run_supervisor(&mut out, &mut input, &mut host);
        assert_eq!(exit, SupervisorExit::ContinueBoot);
        assert!(host.audits.contains(&SupervisorEvent::Entered));
        assert!(out.contains("* "));
    }

    #[test]
    fn end_of_input_resumes_the_normal_boot() {
        let mut out = VecReport::default();
        let mut input = MockInput::new(b"");
        let mut host = MockHost::default();
        assert_eq!(
            run_supervisor(&mut out, &mut input, &mut host),
            SupervisorExit::ContinueBoot
        );
    }

    #[test]
    fn several_commands_run_before_exit() {
        let mut out = VecReport::default();
        let mut input = MockInput::new(b"version\nhelp\ncontinue\n");
        let mut host = MockHost::default();
        run_supervisor(&mut out, &mut input, &mut host);
        assert!(out.contains("TAIRiX"));
        assert!(out.contains("Supervisor commands:"));
    }

    #[test]
    fn a_typed_command_is_echoed() {
        let mut out = VecReport::default();
        let mut input = MockInput::new(b"echo hi\ncontinue\n");
        let mut host = MockHost::default();
        run_supervisor(&mut out, &mut input, &mut host);
        // The typed "echo hi" is echoed by the reader, and `echo` prints it.
        assert!(out.contains("echo hi"));
    }
}
