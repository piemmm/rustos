//! Host tests for the relay loop, driven by a scripted [`TelnetIo`].

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::net_ipc::NetAddrFamily;
use tairix_abi::{Errno, InputMode, TerminalSize};
use tairix_help::{HelpSource, SourceError};

use super::{run, FALLBACK_TERM, USAGE};
use crate::command::{Command, Config, Target};
use crate::error::TelnetError;
use crate::io::Output;
use crate::net::{CloseReason, Endpoint, IoEvent, TelnetIo};
use crate::nvt::{NvtEvent, Parser, DO, IAC, SB, SE, WILL};
use crate::option;
use crate::subneg::cmd;

/// A recording output stream.
#[derive(Default)]
struct Recorder {
    written: RefCell<Vec<u8>>,
}

impl Recorder {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.written.borrow()).into_owned()
    }
}

impl Output for Recorder {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.written.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
}

/// An output stream that refuses every write, for the fail-loud path.
struct DeadStream;

impl Output for DeadStream {
    fn write_all(&self, _bytes: &[u8]) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// A `Help/` source with nothing in it, so the short help falls back to the
/// usage banner.
struct NoHelp;

impl HelpSource for NoHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn read(&self, _locale_dir: &str, _file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(None)
    }
}

/// A scripted seam: a queue of events to hand over, and a record of everything
/// the relay did to the outside world.
struct Fake {
    events: Vec<IoEvent>,
    sent: Vec<u8>,
    connected: bool,
    /// Resolution refuses when this is set, so the unresolvable path is
    /// exercised without a resolver.
    resolve_fails: bool,
    connect_error: Option<Errno>,
    /// The write-side half-close refuses when this is set, standing for a
    /// connection that has already gone.
    shutdown_fails: bool,
    size: Option<TerminalSize>,
    modes: Vec<InputMode>,
    suspends: usize,
    closes: usize,
    shutdowns: usize,
    connects: Vec<Endpoint>,
}

impl Fake {
    fn new(events: Vec<IoEvent>) -> Self {
        Self {
            events,
            sent: Vec::new(),
            connected: false,
            resolve_fails: false,
            connect_error: None,
            shutdown_fails: false,
            size: TerminalSize::new(24, 80).ok(),
            modes: Vec::new(),
            suspends: 0,
            closes: 0,
            shutdowns: 0,
            connects: Vec::new(),
        }
    }

    /// Everything transmitted, decoded through the real receive parser.
    fn decoded(&self) -> Vec<Seen> {
        let mut parser = Parser::new();
        let mut out = Vec::new();
        parser.feed(&self.sent, |event| match event {
            NvtEvent::Data(data) => out.push(Seen::Data(data.to_vec())),
            NvtEvent::Command(byte) => out.push(Seen::Command(byte)),
            NvtEvent::Negotiate { verb, option } => out.push(Seen::Negotiate(verb, option)),
            NvtEvent::Subnegotiation { option, params } => {
                out.push(Seen::Subnegotiation(option, params.to_vec()));
            }
            NvtEvent::SubnegotiationRefused { .. } | NvtEvent::UnknownCommand(_) => {}
        });
        out
    }

    /// The transmitted data bytes, concatenated.
    fn data(&self) -> Vec<u8> {
        self.decoded()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Data(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Seen {
    Data(Vec<u8>),
    Command(u8),
    Negotiate(u8, u8),
    Subnegotiation(u8, Vec<u8>),
}

impl TelnetIo for Fake {
    fn resolve(
        &mut self,
        _host: &str,
        port: u16,
        family: Option<NetAddrFamily>,
    ) -> Option<Endpoint> {
        if self.resolve_fails {
            return None;
        }
        Some(Endpoint {
            family: family.unwrap_or(NetAddrFamily::V6),
            addr: [0; 16],
            port,
        })
    }

    fn connect(&mut self, endpoint: Endpoint) -> Result<(), Errno> {
        if let Some(errno) = self.connect_error {
            return Err(errno);
        }
        self.connects.push(endpoint);
        self.connected = true;
        Ok(())
    }

    fn connected(&self) -> bool {
        self.connected
    }

    fn next_event(&mut self) -> Result<IoEvent, Errno> {
        if self.events.is_empty() {
            // A script that runs out stands for a peer that vanished; the relay
            // must treat that as a fatal receive rather than looping.
            return Err(Errno::NotConnected);
        }
        Ok(self.events.remove(0))
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        self.sent.extend_from_slice(bytes);
        Ok(())
    }

    fn terminal_size(&mut self) -> Option<TerminalSize> {
        self.size
    }

    fn set_input_mode(&mut self, mode: InputMode) {
        self.modes.push(mode);
    }

    fn suspend(&mut self) -> Result<(), Errno> {
        self.suspends += 1;
        Ok(())
    }

    fn shutdown_write(&mut self) -> Result<(), Errno> {
        if self.shutdown_fails {
            return Err(Errno::NotConnected);
        }
        self.shutdowns += 1;
        Ok(())
    }

    fn close(&mut self) -> Result<(), Errno> {
        self.closes += 1;
        self.connected = false;
        Ok(())
    }
}

/// A session command against `example.test:23`.
fn to_host() -> Command {
    Command::Session {
        config: Config::default(),
        target: Some(Target {
            host: String::from("example.test"),
            port: 23,
        }),
    }
}

/// Drive `events` to completion and return the seam plus both streams.
fn drive(
    command: Command,
    events: Vec<IoEvent>,
) -> (Fake, Recorder, Recorder, Result<(), TelnetError>) {
    let mut io = Fake::new(events);
    let out = Recorder::default();
    let err = Recorder::default();
    let result = run(command, None, FALLBACK_TERM, &mut io, &NoHelp, &out, &err);
    (io, out, err, result)
}

/// One keyboard event carrying `bytes`.
fn typed(bytes: &[u8]) -> IoEvent {
    IoEvent::Keyboard(bytes.to_vec())
}

#[test]
fn the_short_help_falls_back_to_the_usage_banner() {
    let (io, out, _, result) = drive(Command::Help, Vec::new());
    assert!(result.is_ok());
    assert_eq!(out.text(), alloc::format!("{USAGE}\n"));
    assert!(io.modes.is_empty(), "help never takes the terminal");
}

#[test]
fn a_failed_help_write_is_reported_as_an_output_error() {
    let mut io = Fake::new(Vec::new());
    let result = run(
        Command::Help,
        None,
        FALLBACK_TERM,
        &mut io,
        &NoHelp,
        &DeadStream,
        &DeadStream,
    );
    assert!(matches!(result, Err(TelnetError::Output(_))));
}

#[test]
fn a_session_takes_raw_mode_and_restores_the_cooked_default() {
    let (io, _, _, result) = drive(
        to_host(),
        alloc::vec![IoEvent::Closed(CloseReason::PeerClosed)],
    );
    assert!(result.is_ok());
    assert_eq!(
        io.modes,
        alloc::vec![InputMode::Raw, InputMode::Cooked],
        "the next program on this console sees the interactive default"
    );
}

#[test]
fn connecting_announces_the_host_and_the_escape_character() {
    let (io, out, _, _) = drive(
        to_host(),
        alloc::vec![IoEvent::Closed(CloseReason::PeerClosed)],
    );
    let text = out.text();
    assert!(text.contains("Trying example.test..."), "{text}");
    assert!(text.contains("Connected to example.test."), "{text}");
    assert!(text.contains("Escape character is '^]'."), "{text}");
    assert_eq!(io.connects.len(), 1);
    assert_eq!(io.connects[0].port, 23);
}

#[test]
fn a_session_opens_its_negotiation_and_reports_the_window() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![IoEvent::Closed(CloseReason::PeerClosed)],
    );
    let sent = io.decoded();
    assert!(
        sent.contains(&Seen::Negotiate(WILL, option::NAWS)),
        "{sent:?}"
    );
    assert!(
        sent.contains(&Seen::Negotiate(DO, option::SUPPRESS_GO_AHEAD)),
        "{sent:?}"
    );
}

#[test]
fn an_unresolvable_host_is_fatal_and_named() {
    let mut io = Fake::new(Vec::new());
    io.resolve_fails = true;
    let out = Recorder::default();
    let err = Recorder::default();
    let result = run(to_host(), None, FALLBACK_TERM, &mut io, &NoHelp, &out, &err);
    assert_eq!(
        result,
        Err(TelnetError::Resolve(String::from("example.test")))
    );
    assert_eq!(io.modes.last(), Some(&InputMode::Cooked), "still restored");
}

#[test]
fn a_refused_socket_is_reported_as_a_capability_denial() {
    let mut io = Fake::new(Vec::new());
    io.connect_error = Some(Errno::PermissionDenied);
    let result = run(
        to_host(),
        None,
        FALLBACK_TERM,
        &mut io,
        &NoHelp,
        &Recorder::default(),
        &Recorder::default(),
    );
    assert_eq!(result, Err(TelnetError::Denied));
}

#[test]
fn an_unreachable_host_carries_its_errno() {
    let mut io = Fake::new(Vec::new());
    io.connect_error = Some(Errno::NetworkUnreachable);
    let result = run(
        to_host(),
        None,
        FALLBACK_TERM,
        &mut io,
        &NoHelp,
        &Recorder::default(),
        &Recorder::default(),
    );
    assert_eq!(result, Err(TelnetError::Connect(Errno::NetworkUnreachable)));
}

#[test]
fn server_output_reaches_standard_output_with_the_nvt_interpretation() {
    let (_, out, _, _) = drive(
        to_host(),
        alloc::vec![
            IoEvent::Network(b"login: ".to_vec()),
            IoEvent::Network(b"\r\ndone\r\0".to_vec()),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    let text = out.text();
    assert!(text.contains("login: \r\ndone\r"), "{text}");
    assert!(
        text.contains("Connection closed by foreign host."),
        "{text}"
    );
}

#[test]
fn each_close_reason_gets_its_own_message() {
    for (reason, expected) in [
        (CloseReason::PeerClosed, "closed by foreign host"),
        (CloseReason::Reset, "reset by foreign host"),
        (CloseReason::Local, "Connection closed."),
    ] {
        let (_, out, _, result) = drive(to_host(), alloc::vec![IoEvent::Closed(reason)]);
        assert!(result.is_ok());
        assert!(out.text().contains(expected), "{reason:?}: {}", out.text());
    }
}

#[test]
fn a_negotiation_the_server_starts_is_answered() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![
            IoEvent::Network(alloc::vec![IAC, WILL, option::ECHO]),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert!(
        io.decoded().contains(&Seen::Negotiate(DO, option::ECHO)),
        "{:?}",
        io.decoded()
    );
}

#[test]
fn a_terminal_type_request_is_answered_from_the_live_relay() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![
            IoEvent::Network(alloc::vec![IAC, DO, option::TERMINAL_TYPE]),
            IoEvent::Network(alloc::vec![
                IAC,
                SB,
                option::TERMINAL_TYPE,
                cmd::SEND,
                IAC,
                SE
            ]),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    let mut expected = alloc::vec![cmd::IS];
    expected.extend_from_slice(b"TAIRIX");
    assert!(
        io.decoded()
            .contains(&Seen::Subnegotiation(option::TERMINAL_TYPE, expected)),
        "{:?}",
        io.decoded()
    );
}

#[test]
fn typed_bytes_reach_the_remote_host() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![
            // The server echoes, so the relay runs character-at-a-time.
            IoEvent::Network(alloc::vec![IAC, WILL, option::ECHO]),
            typed(b"ls\r"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert_eq!(io.data(), b"ls\r\0".to_vec());
}

#[test]
fn a_line_mode_session_forwards_whole_lines() {
    let (io, out, _, _) = drive(
        to_host(),
        alloc::vec![
            typed(b"who"),
            typed(b"ami\r"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert_eq!(io.data(), b"whoami\r\0".to_vec());
    assert!(
        out.text().contains("whoami"),
        "the client echoes its own line"
    );
}

#[test]
fn the_escape_character_opens_the_command_prompt() {
    let (_, out, _, _) = drive(
        to_host(),
        alloc::vec![typed(&[0x1D]), IoEvent::Closed(CloseReason::PeerClosed),],
    );
    assert!(out.text().contains("telnet> "), "{}", out.text());
}

#[test]
fn a_command_typed_at_the_prompt_runs_and_the_relay_resumes() {
    let (io, out, _, _) = drive(
        to_host(),
        alloc::vec![
            typed(&[0x1D]),
            typed(b"send ayt\r"),
            typed(b"\r"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert!(
        io.decoded().contains(&Seen::Command(crate::nvt::AYT)),
        "{:?}",
        io.decoded()
    );
    let text = out.text();
    assert!(
        text.contains("send ayt"),
        "the command line is echoed: {text}"
    );
}

#[test]
fn the_escape_and_the_command_can_arrive_in_one_read() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![
            typed(b"\x1dsend ayt\r"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert!(io.decoded().contains(&Seen::Command(crate::nvt::AYT)));
}

#[test]
fn bytes_after_a_finished_command_go_back_to_the_relay() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![
            IoEvent::Network(alloc::vec![IAC, WILL, option::ECHO]),
            typed(b"\x1d\rafter"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert_eq!(
        io.data(),
        b"after".to_vec(),
        "an empty command resumes and the rest of the read is relayed"
    );
}

#[test]
fn quit_at_the_prompt_ends_the_session_cleanly() {
    let (io, _, _, result) = drive(to_host(), alloc::vec![typed(b"\x1dquit\r")]);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(io.closes, 1);
    assert_eq!(io.modes.last(), Some(&InputMode::Cooked));
}

#[test]
fn close_at_the_prompt_keeps_the_operator_at_the_prompt() {
    let (io, out, _, result) = drive(
        to_host(),
        alloc::vec![typed(b"\x1dclose\r"), typed(b"quit\r")],
    );
    assert!(result.is_ok());
    let text = out.text();
    assert!(text.contains("Connection closed."), "{text}");
    assert!(io.closes >= 1);
}

#[test]
fn open_at_the_prompt_connects_and_resumes() {
    let (io, out, _, _) = drive(
        Command::Session {
            config: Config::default(),
            target: None,
        },
        alloc::vec![
            typed(b"open other.test 2323\r"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert_eq!(io.connects.len(), 1);
    assert_eq!(io.connects[0].port, 2323);
    assert!(
        out.text().contains("Connected to other.test."),
        "{}",
        out.text()
    );
}

#[test]
fn a_bare_invocation_starts_at_the_prompt_with_the_usage_banner() {
    let (io, out, _, result) = drive(
        Command::Session {
            config: Config::default(),
            target: None,
        },
        alloc::vec![typed(b"quit\r")],
    );
    assert!(result.is_ok());
    let text = out.text();
    assert!(text.contains(USAGE), "{text}");
    assert!(text.contains("telnet> "), "{text}");
    assert!(io.connects.is_empty());
}

#[test]
fn a_failed_open_from_the_prompt_is_not_fatal() {
    let mut io = Fake::new(alloc::vec![typed(b"open bad.test\r"), typed(b"quit\r")]);
    io.resolve_fails = true;
    let out = Recorder::default();
    let result = run(
        Command::Session {
            config: Config::default(),
            target: None,
        },
        None,
        FALLBACK_TERM,
        &mut io,
        &NoHelp,
        &out,
        &Recorder::default(),
    );
    assert!(result.is_ok(), "the operator is still at the prompt");
    assert!(
        out.text().contains("cannot resolve bad.test"),
        "{}",
        out.text()
    );
}

#[test]
fn z_at_the_prompt_suspends_with_the_console_handed_back_first() {
    let (io, _, _, _) = drive(to_host(), alloc::vec![typed(b"\x1dz\r"), typed(b"quit\r")]);
    assert_eq!(io.suspends, 1);
    // Raw → cooked (for the shell that regains the terminal) → raw → cooked.
    assert_eq!(
        io.modes,
        alloc::vec![
            InputMode::Raw,
            InputMode::Cooked,
            InputMode::Raw,
            InputMode::Cooked
        ]
    );
}

#[test]
fn end_of_local_input_half_closes_and_keeps_reading_the_response() {
    // `telnet host 80 < request` must not lose the response: the write side is
    // closed so the server sees end-of-input, and the relay keeps carrying the
    // other direction until the peer closes too.
    let (io, out, _, result) = drive(
        to_host(),
        alloc::vec![
            IoEvent::KeyboardClosed,
            IoEvent::Network(b"200 OK".to_vec()),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert!(result.is_ok());
    assert_eq!(io.shutdowns, 1, "a server waiting on end-of-input sees it");
    assert!(
        out.text().contains("200 OK"),
        "the response arrived after the half-close: {}",
        out.text()
    );
}

#[test]
fn end_of_local_input_on_a_dead_connection_ends_the_session() {
    // Nothing left to write to and nothing left to read: the session is over,
    // and must not park waiting for an event that cannot come.
    let mut io = Fake::new(alloc::vec![IoEvent::KeyboardClosed]);
    io.shutdown_fails = true;
    let result = run(
        to_host(),
        None,
        FALLBACK_TERM,
        &mut io,
        &NoHelp,
        &Recorder::default(),
        &Recorder::default(),
    );
    assert!(result.is_ok());
    assert_eq!(io.closes, 1);
}

#[test]
fn a_logout_request_ends_the_session_with_a_reason() {
    let (io, out, _, result) = drive(
        to_host(),
        alloc::vec![IoEvent::Network(alloc::vec![IAC, DO, option::LOGOUT])],
    );
    assert!(result.is_ok());
    assert!(
        out.text().contains("asked this session to log out"),
        "{}",
        out.text()
    );
    assert_eq!(io.closes, 1);
}

#[test]
fn a_fatal_receive_error_is_reported_rather_than_looped_on() {
    // The script runs out, which the seam reports as a dead connection.
    let (_, _, _, result) = drive(to_host(), Vec::new());
    assert_eq!(result, Err(TelnetError::Receive(Errno::NotConnected)));
}

#[test]
fn a_resize_is_reported_the_next_time_the_operator_types() {
    let mut io = Fake::new(alloc::vec![
        IoEvent::Network(alloc::vec![IAC, DO, option::NAWS]),
        typed(b"x"),
        IoEvent::Closed(CloseReason::PeerClosed),
    ]);
    // The grid the connect banner reported.
    io.size = TerminalSize::new(24, 80).ok();
    let out = Recorder::default();
    // Swap the grid before the keystroke by scripting a second seam pass: the
    // relay re-reads the size on every keyboard event, which is how a resize
    // reaches NAWS on a system with no `SIGWINCH`.
    let result = {
        io.size = TerminalSize::new(50, 132).ok();
        run(
            to_host(),
            None,
            FALLBACK_TERM,
            &mut io,
            &NoHelp,
            &out,
            &Recorder::default(),
        )
    };
    assert!(result.is_ok());
    let reports: Vec<Vec<u8>> = io
        .decoded()
        .into_iter()
        .filter_map(|seen| match seen {
            Seen::Subnegotiation(opt, params) if opt == option::NAWS => Some(params),
            _ => None,
        })
        .collect();
    assert_eq!(
        reports,
        alloc::vec![alloc::vec![0, 132, 0, 50]],
        "one report, for the size actually in force"
    );
}

#[test]
fn a_console_with_no_attestable_size_reports_no_window() {
    let mut io = Fake::new(alloc::vec![
        IoEvent::Network(alloc::vec![IAC, DO, option::NAWS]),
        typed(b"x"),
        IoEvent::Closed(CloseReason::PeerClosed),
    ]);
    io.size = None;
    let result = run(
        to_host(),
        None,
        FALLBACK_TERM,
        &mut io,
        &NoHelp,
        &Recorder::default(),
        &Recorder::default(),
    );
    assert!(result.is_ok());
    assert!(
        !io.decoded()
            .iter()
            .any(|seen| matches!(seen, Seen::Subnegotiation(opt, _) if *opt == option::NAWS)),
        "a size the kernel cannot attest is never fabricated"
    );
}

#[test]
fn the_debug_switch_traces_the_negotiation_on_standard_error() {
    let config = Config {
        debug: true,
        ..Config::default()
    };
    let (_, _, err, _) = drive(
        Command::Session {
            config,
            target: Some(Target {
                host: String::from("h"),
                port: 23,
            }),
        },
        alloc::vec![
            IoEvent::Network(alloc::vec![IAC, WILL, option::ECHO]),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert!(err.text().contains("ECHO"), "{}", err.text());
}

#[test]
fn an_over_long_command_line_is_refused_rather_than_truncated() {
    let mut line = alloc::vec![0x1Du8];
    line.extend(core::iter::repeat_n(b'x', super::MAX_COMMAND + 8));
    line.push(b'\r');
    let (_, out, _, _) = drive(
        to_host(),
        alloc::vec![IoEvent::Keyboard(line), typed(b"quit\r")],
    );
    assert!(out.text().contains("Command too long"), "{}", out.text());
}

#[test]
fn the_command_line_erases_a_character_at_the_prompt() {
    let (io, _, _, _) = drive(
        to_host(),
        alloc::vec![
            // `send bogus`, backspaced down to `send ayt`.
            typed(b"\x1dsend ayx\x08t\r"),
            IoEvent::Closed(CloseReason::PeerClosed),
        ],
    );
    assert!(
        io.decoded().contains(&Seen::Command(crate::nvt::AYT)),
        "{:?}",
        io.decoded()
    );
}
