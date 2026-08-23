//! The session engine: one state machine folding both directions of a telnet
//! connection.
//!
//! It owns the negotiation state, the LINEMODE state, the local line editor and
//! the receive-side NVT interpretation, and it performs no I/O at all: network
//! bytes and keyboard bytes are folded in, and what comes out is three buffers
//! the caller drains — bytes for the wire, bytes for the user's terminal, and
//! trace lines for standard error. That is what makes the whole interactive
//! relay host-testable against a scripted server.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::TerminalSize;

use crate::command::Config;
use crate::edit::{EditAction, Editor};
use crate::linemode::{mode, slc, Linemode};
use crate::nvt::{self, NvtEvent, Parser, TransmitMode};
use crate::option::{self, NegotiationFault, Options};
use crate::subneg::{self, Environ, Request};

/// How keystrokes reach the server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Relay {
    /// Every keystroke is transmitted as it is typed; the server echoes.
    Character,
    /// The client edits a line locally and forwards it whole.
    Line,
}

/// The `toggle`-able local behaviours (BSD telnet's `toggle` command).
// Each bool is one independent `toggle` argument, exactly as BSD telnet spells
// them; folding them into state enums would obscure the surface they mirror.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Flags {
    /// Recognise the local signal characters and map them to telnet commands.
    pub localchars: bool,
    /// Send the line terminator as `CR LF` rather than `CR NUL`.
    pub crlf: bool,
    /// Map a local `LF` to the line terminator on transmit, and a received
    /// bare `LF` to a new line on display.
    pub crmod: bool,
    /// Trace every option negotiation.
    pub options: bool,
    /// Trace every non-data byte in both directions.
    pub netdata: bool,
    /// Flush pending terminal output when a signal character is recognised.
    pub autoflush: bool,
    /// Send a Synch when a signal character is recognised.
    pub autosynch: bool,
    /// Honour the negotiated flow-control characters locally.
    pub flow_control: bool,
}

impl Default for Flags {
    /// The defaults BSD telnet starts an interactive session with.
    fn default() -> Self {
        Self {
            localchars: true,
            crlf: false,
            crmod: false,
            options: false,
            netdata: false,
            autoflush: true,
            autosynch: true,
            flow_control: true,
        }
    }
}

/// What folding keyboard input produced.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyFold {
    /// How many input bytes were consumed before the escape character stopped
    /// the fold. [`None`] when the whole input was consumed.
    pub escape_at: Option<usize>,
}

/// What folding network input produced.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetFold {
    /// The server asked us to log out (the `LOGOUT` option, RFC 727).
    pub logout: bool,
}

/// One telnet session's whole protocol state.
#[derive(Debug)]
pub struct Session {
    options: Options,
    linemode: Linemode,
    editor: Editor,
    parser: Parser,
    environ: Environ,
    flags: Flags,
    /// The escape character, or [`None`] when `-E` disabled it.
    escape: Option<u8>,
    /// Trace negotiation unconditionally (`-d`).
    debug: bool,
    /// The terminal type to report over TERMINAL-TYPE.
    term: String,
    /// The line speed to report over TERMINAL-SPEED.
    speed: u32,
    /// The grid last reported over NAWS, so an unchanged size is not re-sent
    /// and a changed one is.
    reported_size: Option<TerminalSize>,
    /// The current terminal grid, as last learned from the seam.
    size: Option<TerminalSize>,
    /// A `CR` held from the previous network chunk, so the RFC 854 `CR LF` /
    /// `CR NUL` interpretation survives an arbitrary read boundary.
    pending_cr: bool,
    wire: Vec<u8>,
    screen: Vec<u8>,
    trace: Vec<String>,
}

impl Session {
    /// A fresh session for `config`, reporting `term` and `speed` to a server
    /// that asks.
    #[must_use]
    pub fn new(config: &Config, term: &str, speed: u32) -> Self {
        let mut session = Self {
            options: Options::new(),
            linemode: Linemode::new(),
            editor: Editor::new(),
            parser: Parser::new(),
            environ: Environ::new(),
            flags: Flags::default(),
            escape: config.escape,
            debug: config.debug,
            term: term.to_string(),
            speed,
            reported_size: None,
            size: None,
            pending_cr: false,
            wire: Vec::new(),
            screen: Vec::new(),
            trace: Vec::new(),
        };
        if let Some(user) = &config.user {
            // A `-l`/`-a` login name is the one thing an invocation may
            // disclose, and only because the operator asked for it by name.
            let _ = session.environ.define("USER", user);
            let _ = session.environ.set_exported("USER", true);
        }
        session
    }

    /// The negotiation table.
    #[must_use]
    pub const fn options(&self) -> &Options {
        &self.options
    }

    /// The negotiation table, mutably (the interpreter's `toggle binary` and
    /// `mode` commands negotiate through it).
    pub const fn options_mut(&mut self) -> &mut Options {
        &mut self.options
    }

    /// The LINEMODE state.
    #[must_use]
    pub const fn linemode(&self) -> &Linemode {
        &self.linemode
    }

    /// The LINEMODE state, mutably (the `set`/`unset` commands edit the SLC
    /// table through it).
    pub const fn linemode_mut(&mut self) -> &mut Linemode {
        &mut self.linemode
    }

    /// The local `toggle` flags.
    #[must_use]
    pub const fn flags(&self) -> &Flags {
        &self.flags
    }

    /// The local `toggle` flags, mutably.
    pub const fn flags_mut(&mut self) -> &mut Flags {
        &mut self.flags
    }

    /// The NEW-ENVIRON table.
    #[must_use]
    pub const fn environ(&self) -> &Environ {
        &self.environ
    }

    /// The NEW-ENVIRON table, mutably (the `environ` command).
    pub const fn environ_mut(&mut self) -> &mut Environ {
        &mut self.environ
    }

    /// The escape character, or [`None`] when there is none.
    #[must_use]
    pub const fn escape(&self) -> Option<u8> {
        self.escape
    }

    /// Set the escape character (the `set escape` command); [`None`] disables it.
    pub const fn set_escape(&mut self, escape: Option<u8>) {
        self.escape = escape;
    }

    /// How keystrokes currently reach the server.
    ///
    /// LINEMODE decides when it is in force; otherwise a server that echoes
    /// for us is a character-at-a-time server and one that does not leaves the
    /// client editing and echoing (the historical line-by-line mode).
    #[must_use]
    pub fn relay(&self) -> Relay {
        if self.options.local(option::LINEMODE) {
            return if self.linemode.edit() {
                Relay::Line
            } else {
                Relay::Character
            };
        }
        if self.options.remote(option::ECHO) {
            Relay::Character
        } else {
            Relay::Line
        }
    }

    /// Whether the client is echoing keystrokes itself.
    #[must_use]
    pub fn local_echo(&self) -> bool {
        !self.options.remote(option::ECHO)
    }

    /// The transmit cooking currently in force.
    #[must_use]
    pub fn transmit_mode(&self) -> TransmitMode {
        TransmitMode {
            binary: self.options.local(option::BINARY),
            crlf: self.flags.crlf,
            crmod: self.flags.crmod,
        }
    }

    /// Take the bytes to transmit.
    pub fn take_wire(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.wire)
    }

    /// Take the bytes to write to the user's terminal.
    pub fn take_screen(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.screen)
    }

    /// Take the accumulated trace lines.
    pub fn take_trace(&mut self) -> Vec<String> {
        core::mem::take(&mut self.trace)
    }

    /// Append `text` to the terminal output (the command interpreter's replies
    /// share the session's one output buffer, so ordering with server output is
    /// never ambiguous).
    pub fn print(&mut self, text: &str) {
        self.screen.extend_from_slice(text.as_bytes());
    }

    /// Append raw bytes to the terminal output (the command line's own echo).
    pub fn print_bytes(&mut self, bytes: &[u8]) {
        self.screen.extend_from_slice(bytes);
    }

    /// Append raw bytes to the wire (the interpreter's `send` command).
    pub fn push_wire(&mut self, bytes: &[u8]) {
        self.wire.extend_from_slice(bytes);
    }

    /// Reset the per-connection state for a fresh connection, keeping the
    /// operator's settings (escape character, toggles, environment).
    pub fn reset_connection(&mut self) {
        self.options = Options::new();
        self.linemode.reset();
        self.editor.discard();
        self.parser.reset();
        self.reported_size = None;
        self.pending_cr = false;
    }

    /// Open the negotiation a client offers as soon as a connection is up.
    ///
    /// The requests are the ones that make an interactive session work, and
    /// each is only a *request*: the server's answer decides, and the RFC 1143
    /// table makes a refusal final.
    pub fn begin(&mut self, config: &Config) {
        let mut wire = core::mem::take(&mut self.wire);
        let _ = self
            .options
            .ask_remote_enable(option::SUPPRESS_GO_AHEAD, &mut wire);
        let _ = self
            .options
            .ask_local_enable(option::TERMINAL_TYPE, &mut wire);
        let _ = self.options.ask_local_enable(option::NAWS, &mut wire);
        let _ = self
            .options
            .ask_local_enable(option::TERMINAL_SPEED, &mut wire);
        let _ = self
            .options
            .ask_local_enable(option::NEW_ENVIRON, &mut wire);
        let _ = self.options.ask_local_enable(option::LINEMODE, &mut wire);
        if config.binary_out {
            let _ = self.options.ask_local_enable(option::BINARY, &mut wire);
        }
        if config.binary_in {
            let _ = self.options.ask_remote_enable(option::BINARY, &mut wire);
        }
        self.wire = wire;
    }

    /// Learn the terminal's current grid, reporting it over NAWS when the
    /// option is enabled and the size actually changed.
    ///
    /// TAIRiX has no `SIGWINCH`, so there is no resize *event* to park on; the
    /// caller consults the size after each keystroke, which is a human-rate
    /// event, so a resize is reported as soon as the user next types.
    pub fn set_terminal_size(&mut self, size: Option<TerminalSize>) {
        self.size = size;
        self.report_naws();
    }

    /// Emit the NAWS report if it is due.
    fn report_naws(&mut self) {
        if !self.options.local(option::NAWS) {
            return;
        }
        let Some(size) = self.size else {
            return;
        };
        if self.reported_size == Some(size) {
            return;
        }
        self.reported_size = Some(size);
        let mut wire = core::mem::take(&mut self.wire);
        subneg::push_naws(size, &mut wire);
        self.wire = wire;
    }

    /// Fold `bytes` received from the network.
    pub fn on_network(&mut self, bytes: &[u8]) -> NetFold {
        // The parser borrows its own buffer while reporting, so the events are
        // collected first and folded after; a read is bounded by the socket
        // ABI's delivery size, so the vector is bounded too.
        let mut events: Vec<Decoded> = Vec::new();
        self.parser
            .feed(bytes, |event| events.push(Decoded::from(event)));
        let mut fold = NetFold::default();
        for event in events {
            match event {
                Decoded::Data(data) => self.on_data(&data),
                Decoded::Command(byte) => self.on_command(byte),
                Decoded::Negotiate(verb, opt) => {
                    if self.on_negotiate(verb, opt) {
                        fold.logout = true;
                    }
                }
                Decoded::Subnegotiation(opt, params) => self.on_subnegotiation(opt, &params),
                Decoded::Refused(opt) => self.note(&alloc::format!(
                    "refused an over-long or malformed {} subnegotiation",
                    option_label(opt)
                )),
                Decoded::Unknown(byte) => {
                    self.note(&alloc::format!("ignored unknown command {byte}"));
                }
            }
        }
        fold
    }

    /// Fold received data bytes, applying the RFC 854 receive interpretation.
    fn on_data(&mut self, data: &[u8]) {
        if self.options.remote(option::BINARY) {
            self.screen.extend_from_slice(data);
            return;
        }
        for &byte in data {
            if self.pending_cr {
                self.pending_cr = false;
                match byte {
                    // `CR LF` is one new line; `CR NUL` is a bare carriage
                    // return; a `CR` before anything else is still a carriage
                    // return and the byte stands on its own.
                    b'\n' => {
                        self.screen.extend_from_slice(b"\r\n");
                        continue;
                    }
                    0 => {
                        self.screen.push(b'\r');
                        continue;
                    }
                    _ => self.screen.push(b'\r'),
                }
            }
            if byte == b'\r' {
                self.pending_cr = true;
                continue;
            }
            self.push_display(byte);
        }
    }

    /// Append one already-interpreted display byte, applying `crmod`.
    fn push_display(&mut self, byte: u8) {
        if byte == b'\n' && self.flags.crmod {
            self.screen.extend_from_slice(b"\r\n");
        } else {
            self.screen.push(byte);
        }
    }

    /// React to a received argument-free command.
    ///
    /// Only `AYT` calls for a visible answer. `DM` arrives as a bare Data Mark
    /// because the socket ABI exposes no TCP urgent data, so there is nothing
    /// in flight to discard ahead of it; and the erase, interrupt and abort
    /// commands are a *server's* to receive, so a client has no action for
    /// them. Each is therefore traced rather than acted on — never silently
    /// dropped.
    fn on_command(&mut self, byte: u8) {
        let name = nvt::command_name(byte).unwrap_or("an unnamed command");
        // The server asking whether anyone is there wants evidence on the
        // operator's screen. Answering on the wire would inject bytes into the
        // server's own input stream, which no client should do.
        if byte == nvt::AYT {
            self.screen.extend_from_slice(b"\r\n[Yes]\r\n");
        }
        self.note(&alloc::format!("< IAC {name}"));
    }

    /// Fold a negotiation, returning whether the server asked us to log out.
    fn on_negotiate(&mut self, verb: u8, opt: u8) -> bool {
        let label = option_label(opt);
        if self.flags.options || self.debug {
            let verb_name = nvt::command_name(verb).unwrap_or("?");
            self.trace.push(alloc::format!("< {verb_name} {label}"));
        }
        // RFC 727: a server may ask the client to close down. It is a request,
        // not a command, and the caller decides — so it is reported rather
        // than acted on here, and never *accepted* as an option.
        if opt == option::LOGOUT && verb == nvt::DO {
            let mut wire = core::mem::take(&mut self.wire);
            nvt::push_negotiate(nvt::WONT, option::LOGOUT, &mut wire);
            self.wire = wire;
            return true;
        }
        let outcome = match verb {
            nvt::WILL => self.options.on_will(opt),
            nvt::WONT => self.options.on_wont(opt),
            nvt::DO => self.options.on_do(opt),
            _ => self.options.on_dont(opt),
        };
        if let Some((reply_verb, reply_opt)) = outcome.reply {
            let mut wire = core::mem::take(&mut self.wire);
            nvt::push_negotiate(reply_verb, reply_opt, &mut wire);
            self.wire = wire;
            if self.flags.options || self.debug {
                let verb_name = nvt::command_name(reply_verb).unwrap_or("?");
                self.trace.push(alloc::format!("> {verb_name} {label}"));
            }
        }
        if let Some(fault) = outcome.fault {
            let text = match fault {
                NegotiationFault::AnsweredWrongWay => "answered the wrong way round",
                NegotiationFault::AlreadyThere => "already in that state",
                NegotiationFault::AlreadyQueued => "already queued",
            };
            self.note(&alloc::format!("{label}: {text}"));
        }
        if let Some(enabled) = outcome.changed {
            self.on_option_change(opt, verb, enabled);
        }
        false
    }

    /// Act on an option that just became enabled or disabled.
    fn on_option_change(&mut self, opt: u8, verb: u8, enabled: bool) {
        let ours = matches!(verb, nvt::DO | nvt::DONT);
        match (opt, ours, enabled) {
            (option::NAWS, true, true) => self.report_naws(),
            (option::LINEMODE, true, true) => {
                // With LINEMODE agreed, state the editing we want and the
                // characters we hold. The server's acknowledgement decides.
                let mut wire = core::mem::take(&mut self.wire);
                self.linemode
                    .request_mode(mode::EDIT | mode::TRAPSIG | mode::SOFT_TAB, &mut wire);
                self.linemode.slc().push_export(&mut wire);
                self.wire = wire;
            }
            (option::LINEMODE, true, false) => self.linemode.reset(),
            // A server that stops echoing hands the echo back to us, so a
            // partly-typed line must not be left half-edited under the old
            // rules.
            (option::ECHO, false, _) => self.editor.discard(),
            _ => {}
        }
    }

    /// Dispatch a subnegotiation to its option's handler.
    ///
    /// RFC 855 allows a subnegotiation only for an option that is *enabled*, so
    /// one for an option neither side negotiated is dropped. That is not
    /// pedantry: without the gate a server that never asked could make the
    /// client answer `NEW-ENVIRON` and disclose the operator's exported
    /// variables, or report its terminal and window, purely on request.
    fn on_subnegotiation(&mut self, opt: u8, params: &[u8]) {
        if self.flags.options || self.debug {
            self.trace.push(alloc::format!(
                "< SB {} ({} bytes)",
                option_label(opt),
                params.len()
            ));
        }
        if !self.subnegotiation_allowed(opt) {
            self.trace.push(alloc::format!(
                "ignored a subnegotiation this client cannot act on: {}",
                option_label(opt)
            ));
            return;
        }
        let mut wire = core::mem::take(&mut self.wire);
        match opt {
            option::TERMINAL_TYPE => {
                if matches!(subneg::split_request(params), Some((Request::Send, _))) {
                    subneg::push_terminal_type(&self.term, &mut wire);
                }
            }
            option::TERMINAL_SPEED => {
                if matches!(subneg::split_request(params), Some((Request::Send, _))) {
                    subneg::push_terminal_speed(self.speed, &mut wire);
                }
            }
            option::STATUS => {
                if matches!(subneg::split_request(params), Some((Request::Send, _))) {
                    subneg::push_status(&self.options, &mut wire);
                }
            }
            option::NEW_ENVIRON => {
                if let Some((Request::Send, request)) = subneg::split_request(params) {
                    self.environ.push_is_reply(request, &mut wire);
                }
            }
            option::TOGGLE_FLOW_CONTROL => match subneg::flow_control_enabled(params) {
                Some(enabled) => self.flags.flow_control = enabled,
                None => self.trace.push(String::from(
                    "ignored a malformed TOGGLE FLOW CONTROL subnegotiation",
                )),
            },
            option::LINEMODE => {
                let outcome = self.linemode.fold(params);
                wire.extend_from_slice(&outcome.reply);
                if outcome.refused {
                    self.trace
                        .push(String::from("ignored a malformed LINEMODE subnegotiation"));
                }
                if outcome.mode_changed.is_some() {
                    // The editing rules changed under a part-typed line; it
                    // cannot be carried across, so it is dropped rather than
                    // forwarded under rules the user did not type it in.
                    self.editor.discard();
                }
            }
            // The gate above admits exactly the options handled here, so this
            // arm is the belt to that brace and never runs.
            other => self.trace.push(alloc::format!(
                "ignored a subnegotiation for option {other}"
            )),
        }
        self.wire = wire;
    }

    /// Whether an inbound subnegotiation for `opt` may be acted on.
    ///
    /// The list is exactly the options for which a *server* has something to
    /// say and this client something to do about it, each gated on the side
    /// that makes it meaningful. Reporting the terminal, its speed, the
    /// environment and the option status, and editing under LINEMODE, are all
    /// things the client enables on its own side with `WILL`. Flow control is
    /// the opposite: the peer offers it with `WILL` and then tells the client
    /// what to honour. NAWS is deliberately absent — it travels client to
    /// server only, so an inbound one is meaningless whatever its state.
    fn subnegotiation_allowed(&self, opt: u8) -> bool {
        match opt {
            option::TERMINAL_TYPE
            | option::TERMINAL_SPEED
            | option::NEW_ENVIRON
            | option::STATUS
            | option::LINEMODE => self.options.local(opt),
            option::TOGGLE_FLOW_CONTROL => self.options.remote(opt),
            _ => false,
        }
    }

    /// Fold keyboard bytes, stopping at the escape character.
    pub fn on_keyboard(&mut self, bytes: &[u8]) -> KeyFold {
        for (index, &byte) in bytes.iter().enumerate() {
            if self.escape == Some(byte) {
                return KeyFold {
                    escape_at: Some(index),
                };
            }
            match self.relay() {
                Relay::Line => self.type_line_mode(byte),
                Relay::Character => self.type_character_mode(byte),
            }
        }
        KeyFold::default()
    }

    /// One keystroke in line mode: the local editor owns it.
    fn type_line_mode(&mut self, byte: u8) {
        let mut echo = Vec::new();
        let action = {
            let (editor, linemode) = (&mut self.editor, &self.linemode);
            editor.push(byte, linemode, &mut echo)
        };
        if self.local_echo() {
            self.screen.extend_from_slice(&echo);
        }
        match action {
            EditAction::Pending => {}
            EditAction::Line => {
                let line = self.editor.take_line();
                let mode = self.transmit_mode();
                let mut wire = core::mem::take(&mut self.wire);
                nvt::escape_into(&line, mode, &mut wire);
                nvt::push_eol(mode, &mut wire);
                self.wire = wire;
            }
            EditAction::Forward => {
                let line = self.editor.take_line();
                let mode = self.transmit_mode();
                let mut wire = core::mem::take(&mut self.wire);
                nvt::escape_into(&line, mode, &mut wire);
                self.wire = wire;
            }
            EditAction::Signal(function) => {
                // A signal discards the line being edited: the server never
                // saw it, and forwarding it after an interrupt would replay
                // input the user just abandoned.
                self.editor.discard();
                self.send_signal(function);
            }
        }
    }

    /// One keystroke in character mode: it goes out as typed, unless it is a
    /// signal character the operator has asked to have mapped.
    fn type_character_mode(&mut self, byte: u8) {
        if self.flags.localchars {
            if let Some(function) = self.linemode.slc().function_for(byte) {
                if is_signal(function) {
                    self.send_signal(function);
                    return;
                }
            }
        }
        let mode = self.transmit_mode();
        let mut wire = core::mem::take(&mut self.wire);
        nvt::escape_into(&[byte], mode, &mut wire);
        self.wire = wire;
        if self.local_echo() {
            self.screen.push(byte);
        }
    }

    /// Transmit the telnet command an SLC signal function stands for.
    pub fn send_signal(&mut self, function: u8) {
        let mut wire = core::mem::take(&mut self.wire);
        match function {
            slc::IP => nvt::push_command(nvt::IP, &mut wire),
            slc::BRK => nvt::push_command(nvt::BRK, &mut wire),
            slc::ABORT => nvt::push_command(nvt::ABORT, &mut wire),
            slc::SUSP => nvt::push_command(nvt::SUSP, &mut wire),
            slc::EOF => nvt::push_command(nvt::XEOF, &mut wire),
            slc::AO => nvt::push_command(nvt::AO, &mut wire),
            slc::AYT => nvt::push_command(nvt::AYT, &mut wire),
            slc::EOR => nvt::push_command(nvt::EOR, &mut wire),
            slc::SYNCH => nvt::push_command(nvt::DM, &mut wire),
            // The flow-control characters act on the *local* terminal, so they
            // are sent as data when the server asked us to honour them there.
            other => {
                let mode = TransmitMode {
                    binary: self.options.local(option::BINARY),
                    crlf: self.flags.crlf,
                    crmod: self.flags.crmod,
                };
                if let Some(byte) = self.linemode.slc().char_for(other) {
                    nvt::escape_into(&[byte], mode, &mut wire);
                }
            }
        }
        // RFC 854 pairs an interrupting function with a Synch so the server
        // discards what is already in flight. The socket ABI exposes no urgent
        // data, so the Data Mark rides in-band: a server that scans for it
        // still finds it, in order.
        if self.flags.autosynch && matches!(function, slc::IP | slc::ABORT | slc::BRK) {
            nvt::push_command(nvt::DM, &mut wire);
        }
        self.wire = wire;
    }

    /// Record a note, shown only when the operator asked for traces.
    fn note(&mut self, text: &str) {
        if self.flags.options || self.flags.netdata || self.debug {
            self.trace.push(text.to_string());
        }
    }
}

/// The display label of an option: its name, or its number when this client has
/// none for it.
fn option_label(opt: u8) -> String {
    option::option_name(opt).map_or_else(|| alloc::format!("option {opt}"), String::from)
}

/// Whether an SLC function maps to a telnet command rather than a character.
const fn is_signal(function: u8) -> bool {
    matches!(
        function,
        slc::IP | slc::BRK | slc::ABORT | slc::SUSP | slc::EOF | slc::AO | slc::AYT | slc::SYNCH
    )
}

/// An owned [`NvtEvent`], so the parser's borrow of its own subnegotiation
/// buffer ends before the session folds the event and mutates itself.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Decoded {
    Data(Vec<u8>),
    Command(u8),
    Negotiate(u8, u8),
    Subnegotiation(u8, Vec<u8>),
    Refused(u8),
    Unknown(u8),
}

impl From<NvtEvent<'_>> for Decoded {
    fn from(event: NvtEvent<'_>) -> Self {
        match event {
            NvtEvent::Data(bytes) => Self::Data(bytes.to_vec()),
            NvtEvent::Command(byte) => Self::Command(byte),
            NvtEvent::Negotiate { verb, option } => Self::Negotiate(verb, option),
            NvtEvent::Subnegotiation { option, params } => {
                Self::Subnegotiation(option, params.to_vec())
            }
            NvtEvent::SubnegotiationRefused { option } => Self::Refused(option),
            NvtEvent::UnknownCommand(byte) => Self::Unknown(byte),
        }
    }
}

#[cfg(test)]
mod tests;
