//! The relay loop: one event at a time, over the injected seams.
//!
//! Telnet has to carry both directions at once, but this loop is
//! single-threaded because the [`TelnetIo`] seam owns the multiplexing — it
//! hands over one ordered stream of network, keyboard and close events. That is
//! what lets the whole interactive relay, command mode included, be driven by a
//! scripted in-memory seam in the host tests.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{Errno, InputMode};
use tairix_help::{own_short_help, HelpSource};
use tairix_vt::line::{LineEditor, LineFeed};

use crate::command::{Command, Config, Target};
use crate::error::TelnetError;
use crate::interp::{self, Action};
use crate::io::Output;
use crate::net::{CloseReason, Endpoint, IoEvent, TelnetIo};
use crate::session::Session;

/// The one-line usage banner, printed on a usage error and as the fallback when
/// the bundled help document is unavailable.
pub const USAGE: &str = "usage: telnet [option...] [host [port]]";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "telnet";

/// The `telnet>` prompt, with the carriage return a raw-mode terminal needs.
const PROMPT: &str = "\r\ntelnet> ";

/// Longest command line the interpreter accepts. A fixed bound: the prompt is
/// local input, but an unbounded buffer is still an unbounded buffer.
const MAX_COMMAND: usize = 256;

/// The terminal line speed reported over TERMINAL-SPEED (RFC 1079).
///
/// A TAIRiX console is not a serial line with a negotiated rate, so there is no
/// measurement to report. The option exists to let a host size its output, and
/// the conventional "fast local terminal" figure is the honest answer; the
/// alternative — refusing the option — would make some hosts assume a 300-baud
/// teletype.
const TERMINAL_SPEED: u32 = 38_400;

/// The terminal type reported when the session inherited no `TERM`. Naming the
/// system honestly beats claiming to be a terminal the console may not
/// implement.
pub const FALLBACK_TERM: &str = "TAIRIX";

/// Where the operator's keystrokes are going.
enum Phase {
    /// Straight to the remote host.
    Relay,
    /// Into the `telnet>` command line being typed. Boxed because the line
    /// buffer dwarfs the relay variant, which is the common one.
    Command(Box<CommandLine>),
}

/// The command line being typed at the `telnet>` prompt.
///
/// The editing rules here are the fixed local ones — Backspace, the Delete key,
/// Return — so this is `lib/vt`'s shared line editor rather than the
/// SLC-negotiated one the relay uses.
struct CommandLine {
    editor: LineEditor,
    buf: [u8; MAX_COMMAND],
    len: usize,
}

impl CommandLine {
    fn new() -> Self {
        Self {
            editor: LineEditor::new(),
            buf: [0; MAX_COMMAND],
            len: 0,
        }
    }

    /// Feed one byte, echoing what the operator should see. Returns the
    /// finished line when Return ended it.
    fn push(&mut self, byte: u8, echo: &mut Vec<u8>) -> Option<String> {
        let before = self.len;
        match self.editor.push(&mut self.buf, &mut self.len, byte) {
            LineFeed::Complete => {
                echo.extend_from_slice(b"\r\n");
                let line = String::from_utf8_lossy(&self.buf[..self.len]).into_owned();
                self.len = 0;
                Some(line)
            }
            // A line longer than the buffer is refused rather than truncated:
            // acting on half a command is worse than saying it was too long.
            LineFeed::TooLong => {
                echo.extend_from_slice(b"\r\n?Command too long\r\n");
                self.len = 0;
                Some(String::new())
            }
            LineFeed::Pending | LineFeed::Escape => {
                if self.len > before {
                    echo.extend_from_slice(&self.buf[before..self.len]);
                } else if self.len < before {
                    echo.extend_from_slice(&tairix_vt::control::ERASE_ECHO);
                }
                None
            }
        }
    }
}

/// Run a parsed `telnet` command against the injected seams.
///
/// Returns `Ok(())` for a session that ran (whatever the remote host did with
/// it) or for the short help; a [`TelnetError`] only for a failure that defeats
/// the tool's purpose — a socket it could not open, a host it could not resolve,
/// or a terminal it could not put into the raw read discipline.
///
/// # Errors
///
/// A [`TelnetError`] carrying the reason, which the caller reports on standard
/// error before exiting.
pub fn run(
    command: Command,
    locale: Option<&str>,
    term: &str,
    io: &mut dyn TelnetIo,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(), TelnetError> {
    let (config, target) = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
            return out.write_all(&bytes).map_err(TelnetError::Output);
        }
        Command::Session { config, target } => (config, target),
    };

    let mut session = Session::new(&config, term, TERMINAL_SPEED);
    session.set_terminal_size(io.terminal_size());

    // The raw read discipline: keystrokes reach the relay verbatim, with
    // neither the console's local echo (the server's or our own editor's job)
    // nor an activity indicator painted over the host's output. Restored on
    // every exit path below so the next program on this console sees the
    // interactive default.
    io.set_input_mode(InputMode::Raw);
    let result = relay(&config, target, &mut session, io, out, err);
    io.set_input_mode(InputMode::Cooked);
    // Whatever the session left queued for the terminal is written after the
    // mode is restored, so a closing message is never swallowed.
    flush(&mut session, io, out, err)?;
    result
}

/// The relay proper, with the input mode already taken.
fn relay(
    config: &Config,
    target: Option<Target>,
    session: &mut Session,
    io: &mut dyn TelnetIo,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(), TelnetError> {
    if let Some(target) = target {
        // A target on the command line is the whole point of the invocation, so
        // failing to reach it is fatal rather than a drop into command mode.
        connect(&target, config, session, io, out, err)?;
    } else {
        out.write_all(USAGE.as_bytes())
            .map_err(TelnetError::Output)?;
        out.write_all(b"\r\n").map_err(TelnetError::Output)?;
    }
    let mut phase = if io.connected() {
        Phase::Relay
    } else {
        prompt(session, io, out, err)?;
        Phase::Command(Box::new(CommandLine::new()))
    };

    loop {
        let event = io.next_event().map_err(TelnetError::Receive)?;
        match event {
            IoEvent::Network(bytes) => {
                let fold = session.on_network(&bytes);
                flush(session, io, out, err)?;
                if fold.logout {
                    session.print("\r\nThe remote host asked this session to log out.\r\n");
                    flush(session, io, out, err)?;
                    let _ = io.close();
                    return Ok(());
                }
            }
            IoEvent::Keyboard(bytes) => {
                // A keystroke is a human-rate event, so re-reading the grid
                // here costs nothing and is how a resize reaches NAWS on a
                // system with no `SIGWINCH`.
                session.set_terminal_size(io.terminal_size());
                match feed_keyboard(&bytes, &mut phase, config, session, io, out, err)? {
                    Continue::Running => {}
                    Continue::Finished => return Ok(()),
                }
            }
            IoEvent::KeyboardClosed => {
                // No more input can arrive, so the write side is closed — a
                // server waiting on end-of-input sees it — and the relay keeps
                // carrying the *other* direction until the peer closes too.
                //
                // The historical tool exits here instead, which loses whatever
                // the server was still sending: `telnet host 80 < request`
                // discards the response. Half-closing is what a TCP client
                // does, so the response arrives and the peer's own close ends
                // the session; the divergence is documented in the tool's help.
                if io.shutdown_write().is_err() {
                    let _ = io.close();
                    return Ok(());
                }
                phase = Phase::Relay;
            }
            IoEvent::Closed(reason) => {
                let text = match reason {
                    CloseReason::PeerClosed => "\r\nConnection closed by foreign host.\r\n",
                    CloseReason::Reset => "\r\nConnection reset by foreign host.\r\n",
                    CloseReason::Local => "\r\nConnection closed.\r\n",
                };
                session.print(text);
                flush(session, io, out, err)?;
                return Ok(());
            }
        }
    }
}

/// Whether the relay loop keeps going.
enum Continue {
    Running,
    Finished,
}

/// Route one keyboard read according to the phase, handling however many
/// escapes and command lines it contains.
fn feed_keyboard(
    bytes: &[u8],
    phase: &mut Phase,
    config: &Config,
    session: &mut Session,
    io: &mut dyn TelnetIo,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<Continue, TelnetError> {
    let mut rest = bytes;
    loop {
        match phase {
            Phase::Relay => {
                let fold = session.on_keyboard(rest);
                flush(session, io, out, err)?;
                let Some(at) = fold.escape_at else {
                    return Ok(Continue::Running);
                };
                *phase = Phase::Command(Box::new(CommandLine::new()));
                prompt(session, io, out, err)?;
                rest = &rest[at + 1..];
            }
            Phase::Command(line) => {
                let mut echo = Vec::new();
                let mut command: Option<String> = None;
                let mut consumed = 0usize;
                for (index, &byte) in rest.iter().enumerate() {
                    consumed = index + 1;
                    if let Some(finished) = line.push(byte, &mut echo) {
                        command = Some(finished);
                        break;
                    }
                }
                session.print_bytes(&echo);
                flush(session, io, out, err)?;
                let Some(command) = command else {
                    return Ok(Continue::Running);
                };
                rest = &rest[consumed..];
                match dispatch(&command, config, session, io, out, err)? {
                    Dispatch::Prompt => {
                        prompt(session, io, out, err)?;
                        *phase = Phase::Command(Box::new(CommandLine::new()));
                    }
                    Dispatch::Resume => *phase = Phase::Relay,
                    Dispatch::Finished => return Ok(Continue::Finished),
                }
            }
        }
    }
}

/// What a dispatched command left the loop to do.
enum Dispatch {
    Prompt,
    Resume,
    Finished,
}

/// Execute one command line and carry out the action it asked for.
fn dispatch(
    command: &str,
    config: &Config,
    session: &mut Session,
    io: &mut dyn TelnetIo,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<Dispatch, TelnetError> {
    let action = interp::execute(command, session, io.connected());
    flush(session, io, out, err)?;
    match action {
        Action::Prompt => Ok(Dispatch::Prompt),
        Action::Resume => {
            if io.connected() {
                Ok(Dispatch::Resume)
            } else {
                // With nothing to relay to, returning to the relay would park
                // the operator on a dead session; the prompt is the honest
                // place to be.
                Ok(Dispatch::Prompt)
            }
        }
        Action::Close => {
            let _ = io.close();
            session.reset_connection();
            session.print("Connection closed.\r\n");
            flush(session, io, out, err)?;
            Ok(Dispatch::Prompt)
        }
        Action::Quit => {
            let _ = io.close();
            Ok(Dispatch::Finished)
        }
        Action::Open(target) => {
            if io.connected() {
                let _ = io.close();
                session.reset_connection();
            }
            match connect(&target, config, session, io, out, err) {
                Ok(()) => Ok(Dispatch::Resume),
                // A failed `open` from command mode is not fatal — the operator
                // is still at the prompt and can try another host.
                Err(error) => {
                    session.print(&alloc::format!("{error}\r\n"));
                    flush(session, io, out, err)?;
                    Ok(Dispatch::Prompt)
                }
            }
        }
        Action::Suspend => {
            // The console goes back to its interactive discipline before the
            // process stops, so the shell that regains the terminal is usable;
            // raw mode is retaken when we are continued.
            io.set_input_mode(InputMode::Cooked);
            let refused = io.suspend().err();
            io.set_input_mode(InputMode::Raw);
            if let Some(errno) = refused {
                session.print(&alloc::format!("?Cannot suspend: {errno}\r\n"));
            }
            flush(session, io, out, err)?;
            Ok(Dispatch::Prompt)
        }
    }
}

/// Resolve and connect to `target`, then open the negotiation.
fn connect(
    target: &Target,
    config: &Config,
    session: &mut Session,
    io: &mut dyn TelnetIo,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(), TelnetError> {
    session.print(&alloc::format!("Trying {}...\r\n", target.host));
    flush(session, io, out, err)?;
    let endpoint: Endpoint = io
        .resolve(&target.host, target.port, config.family)
        .ok_or_else(|| TelnetError::Resolve(target.host.clone()))?;
    io.connect(endpoint).map_err(|errno| match errno {
        Errno::PermissionDenied => TelnetError::Denied,
        other => TelnetError::Connect(other),
    })?;
    session.reset_connection();
    session.print(&alloc::format!(
        "Connected to {}.\r\nEscape character is {}.\r\n",
        target.host,
        interp::render_char_opt(session.escape())
    ));
    session.begin(config);
    session.set_terminal_size(io.terminal_size());
    flush(session, io, out, err)
}

/// Write the `telnet>` prompt.
fn prompt(
    session: &mut Session,
    io: &mut dyn TelnetIo,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(), TelnetError> {
    session.print(PROMPT);
    flush(session, io, out, err)
}

/// Drain everything the session has queued: bytes to the wire, bytes to the
/// terminal, and trace lines to standard error.
fn flush(
    session: &mut Session,
    io: &mut dyn TelnetIo,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<(), TelnetError> {
    let screen = session.take_screen();
    if !screen.is_empty() {
        out.write_all(&screen).map_err(TelnetError::Output)?;
    }
    for line in session.take_trace() {
        err.write_all(line.as_bytes())
            .map_err(TelnetError::Output)?;
        err.write_all(b"\r\n").map_err(TelnetError::Output)?;
    }
    let wire = session.take_wire();
    if wire.is_empty() {
        return Ok(());
    }
    // Bytes queued for a connection that has gone are dropped rather than
    // reported: the close itself is what the operator is told about.
    if !io.connected() {
        return Ok(());
    }
    io.send(&wire).map_err(TelnetError::Send)
}

#[cfg(test)]
mod tests;
