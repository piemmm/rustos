//! The RFC 854 Network Virtual Terminal wire codec: the command vocabulary,
//! the incremental receive parser, and the transmit escaper.
//!
//! Everything a telnet server sends is attacker-controlled, so the parser is
//! total (no input sequence panics), bounded (the subnegotiation region is
//! capped by [`MAX_SUBNEG_LEN`], a fixed security bound rather than a capacity
//! to raise), and fails closed — an over-long or malformed subnegotiation is
//! discarded whole and parsing resumes cleanly at the next `IAC SE`, never
//! surfaced half-decoded.
//!
//! Commands beyond RFC 854 that this client speaks are included in the one
//! vocabulary: `EOR` (RFC 885) and `EOF`/`SUSP`/`ABORT` (RFC 1184 §5).

use alloc::vec::Vec;

/// Interpret As Command: the escape prefix of every telnet command.
pub const IAC: u8 = 255;
/// Refuse, or demand the disabling of, an option.
pub const DONT: u8 = 254;
/// Request, or confirm, that the sender's peer enable an option.
pub const DO: u8 = 253;
/// Refuse, or announce the disabling of, an option on the sender's side.
pub const WONT: u8 = 252;
/// Offer, or confirm, an option on the sender's side.
pub const WILL: u8 = 251;
/// Begin option subnegotiation.
pub const SB: u8 = 250;
/// Go Ahead: the half-duplex line turnaround.
pub const GA: u8 = 249;
/// Erase Line.
pub const EL: u8 = 248;
/// Erase Character.
pub const EC: u8 = 247;
/// Are You There.
pub const AYT: u8 = 246;
/// Abort Output.
pub const AO: u8 = 245;
/// Interrupt Process.
pub const IP: u8 = 244;
/// Break.
pub const BRK: u8 = 243;
/// Data Mark: the Synch stream marker.
pub const DM: u8 = 242;
/// No operation.
pub const NOP: u8 = 241;
/// End of subnegotiation parameters.
pub const SE: u8 = 240;
/// End of Record (RFC 885).
pub const EOR: u8 = 239;
/// Abort the process (RFC 1184 §5).
pub const ABORT: u8 = 238;
/// Suspend the process (RFC 1184 §5).
pub const SUSP: u8 = 237;
/// End of file (RFC 1184 §5). Spelled `XEOF` because `EOF` names a different
/// thing everywhere else in this crate.
pub const XEOF: u8 = 236;

/// Largest subnegotiation parameter region this client will accumulate, after
/// `IAC IAC` collapsing and excluding the leading option byte.
///
/// A fixed security bound on untrusted input, not a capacity: it is generous
/// for every option the client implements (RFC 1184's 30-function SLC table is
/// 90 bytes, its `FORWARDMASK` 32, an `RFC 1572` variable list rather less)
/// while denying a hostile server unbounded growth.
pub const MAX_SUBNEG_LEN: usize = 512;

/// One decoded element of the received stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvtEvent<'a> {
    /// A run of ordinary data bytes, with `IAC IAC` already collapsed to one
    /// `IAC`. Emitted in arrival order and never empty.
    Data(&'a [u8]),
    /// A command with no argument: one of `NOP`/`DM`/`BRK`/`IP`/`AO`/`AYT`/
    /// `EC`/`EL`/`GA`/`EOR`/`XEOF`/`SUSP`/`ABORT`.
    Command(u8),
    /// An option negotiation: `verb` is `WILL`/`WONT`/`DO`/`DONT`.
    Negotiate {
        /// The negotiation verb.
        verb: u8,
        /// The option it concerns.
        option: u8,
    },
    /// A complete subnegotiation for `option`, its parameters `IAC IAC`-collapsed.
    Subnegotiation {
        /// The option being subnegotiated.
        option: u8,
        /// The parameter region between the option byte and `IAC SE`.
        params: &'a [u8],
    },
    /// A subnegotiation whose parameters exceeded [`MAX_SUBNEG_LEN`], or which
    /// was ended by a bare `IAC` command other than `SE`. Reported so the
    /// caller can note the refusal; nothing partial is applied.
    SubnegotiationRefused {
        /// The option whose subnegotiation was discarded.
        option: u8,
    },
    /// An `IAC` followed by a byte that names no telnet command. Reported
    /// rather than silently dropped so `toggle netdata` can show it.
    UnknownCommand(u8),
}

/// Whether a command byte after `IAC` is a standalone (argument-free) command.
const fn is_standalone(byte: u8) -> bool {
    matches!(
        byte,
        NOP | DM | BRK | IP | AO | AYT | EC | EL | GA | EOR | ABORT | SUSP | XEOF
    )
}

/// Where the receive parser is between bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Ordinary data.
    Data,
    /// An `IAC` was seen; the next byte is the command.
    Iac,
    /// A negotiation verb was seen; the next byte is the option.
    Verb(u8),
    /// `IAC SB` was seen; the next byte is the option being subnegotiated.
    SbOption,
    /// Accumulating subnegotiation parameters for the held option.
    SbParams(u8),
    /// An `IAC` inside a subnegotiation; the next byte decides whether it was
    /// an escaped `IAC`, the terminating `SE`, or a refusal.
    SbIac(u8),
    /// The subnegotiation overflowed or was malformed: skip to `IAC SE` and
    /// report the refusal, so a hostile server cannot desynchronise the stream.
    SbDiscard(u8),
    /// An `IAC` inside a discarded subnegotiation.
    SbDiscardIac(u8),
}

/// The incremental RFC 854 receive parser.
///
/// One parser drives one connection for its whole life: telnet commands may be
/// split across any number of reads, so the state and the partially-collected
/// subnegotiation are carried between [`feed`](Self::feed) calls.
#[derive(Debug)]
pub struct Parser {
    state: State,
    sb: Vec<u8>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    /// A fresh parser positioned in the data stream.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: State::Data,
            sb: Vec::new(),
        }
    }

    /// Feed `input` through the parser, reporting each decoded element to
    /// `on_event` in arrival order.
    ///
    /// Data runs are reported as borrowed sub-slices of `input`, so an
    /// ordinary data-only read costs no copy; an escaped `IAC` splits the run
    /// and is reported as its own one-byte [`NvtEvent::Data`].
    pub fn feed<F>(&mut self, input: &[u8], mut on_event: F)
    where
        F: FnMut(NvtEvent<'_>),
    {
        let mut run_start = 0usize;
        for (index, &byte) in input.iter().enumerate() {
            match self.state {
                State::Data => {
                    if byte == IAC {
                        if index > run_start {
                            on_event(NvtEvent::Data(&input[run_start..index]));
                        }
                        self.state = State::Iac;
                    }
                    continue;
                }
                State::Iac => {
                    self.state = match byte {
                        IAC => {
                            on_event(NvtEvent::Data(&input[index..=index]));
                            State::Data
                        }
                        SB => State::SbOption,
                        WILL | WONT | DO | DONT => State::Verb(byte),
                        other if is_standalone(other) => {
                            on_event(NvtEvent::Command(other));
                            State::Data
                        }
                        // `SE` outside a subnegotiation, and every unassigned
                        // byte, are reported and dropped: the stream stays in
                        // sync because neither carries an argument.
                        other => {
                            on_event(NvtEvent::UnknownCommand(other));
                            State::Data
                        }
                    };
                }
                State::Verb(verb) => {
                    on_event(NvtEvent::Negotiate { verb, option: byte });
                    self.state = State::Data;
                }
                State::SbOption => {
                    self.sb.clear();
                    self.state = State::SbParams(byte);
                }
                State::SbParams(option) => {
                    if byte == IAC {
                        self.state = State::SbIac(option);
                    } else if self.sb.len() < MAX_SUBNEG_LEN {
                        self.sb.push(byte);
                    } else {
                        self.sb.clear();
                        self.state = State::SbDiscard(option);
                    }
                }
                State::SbIac(option) => match byte {
                    SE => {
                        on_event(NvtEvent::Subnegotiation {
                            option,
                            params: &self.sb,
                        });
                        self.sb.clear();
                        self.state = State::Data;
                    }
                    IAC if self.sb.len() < MAX_SUBNEG_LEN => {
                        self.sb.push(IAC);
                        self.state = State::SbParams(option);
                    }
                    // Any other command inside a subnegotiation is malformed
                    // (RFC 855 allows only `IAC SE` and an escaped `IAC`), and
                    // an escaped `IAC` past the bound is an overflow: both
                    // discard the whole subnegotiation rather than apply part.
                    _ => {
                        self.sb.clear();
                        on_event(NvtEvent::SubnegotiationRefused { option });
                        self.state = State::Data;
                    }
                },
                State::SbDiscard(option) => {
                    if byte == IAC {
                        self.state = State::SbDiscardIac(option);
                    }
                }
                State::SbDiscardIac(option) => {
                    self.state = if byte == SE {
                        on_event(NvtEvent::SubnegotiationRefused { option });
                        State::Data
                    } else {
                        State::SbDiscard(option)
                    };
                }
            }
            run_start = index + 1;
        }
        if matches!(self.state, State::Data) && run_start < input.len() {
            on_event(NvtEvent::Data(&input[run_start..]));
        }
    }

    /// Release the subnegotiation buffer's capacity, keeping the parser
    /// usable. Called when a connection closes so a session that met a large
    /// subnegotiation does not hold its buffer for the next one.
    pub fn reset(&mut self) {
        self.state = State::Data;
        self.sb = Vec::new();
    }
}

/// How outbound bytes are cooked for the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransmitMode {
    /// BINARY (RFC 856) is in force on the transmit side: bytes pass through
    /// with no NVT line convention at all.
    pub binary: bool,
    /// Send the RFC 854 `CR LF` line terminator rather than `CR NUL`
    /// (BSD telnet's `toggle crlf`).
    pub crlf: bool,
    /// Treat a local `LF` as a line terminator too (BSD telnet's
    /// `toggle crmod`), for a terminal whose Return key sends `LF`.
    pub crmod: bool,
}

impl Default for TransmitMode {
    /// NVT ASCII, `CR NUL` for the line terminator, no `LF` mapping — the
    /// state a fresh connection starts in.
    fn default() -> Self {
        Self {
            binary: false,
            crlf: false,
            crmod: false,
        }
    }
}

/// Append `data` to `out` with the RFC 854 transmit cooking applied.
///
/// `IAC` is always doubled. Outside BINARY mode a `CR` becomes the configured
/// line terminator, because RFC 854 reserves a bare `CR` for the `CR LF`
/// end-of-line and so a lone `CR` must never reach the wire; with `crmod` a
/// local `LF` is mapped the same way. Stateless, and therefore total across
/// any chunking of `data`.
pub fn escape_into(data: &[u8], mode: TransmitMode, out: &mut Vec<u8>) {
    for &byte in data {
        match byte {
            IAC => out.extend_from_slice(&[IAC, IAC]),
            b'\r' if !mode.binary => push_eol(mode, out),
            b'\n' if !mode.binary && mode.crmod => push_eol(mode, out),
            other => out.push(other),
        }
    }
}

/// Append the line terminator `mode` selects to `out` — the one place the
/// `CR LF` / `CR NUL` / verbatim decision is made.
pub fn push_eol(mode: TransmitMode, out: &mut Vec<u8>) {
    if mode.binary {
        out.push(b'\r');
    } else if mode.crlf {
        out.extend_from_slice(b"\r\n");
    } else {
        out.extend_from_slice(b"\r\0");
    }
}

/// Append the two-byte command `IAC <command>` to `out`.
pub fn push_command(command: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&[IAC, command]);
}

/// Append the three-byte negotiation `IAC <verb> <option>` to `out`.
pub fn push_negotiate(verb: u8, option: u8, out: &mut Vec<u8>) {
    out.extend_from_slice(&[IAC, verb, option]);
}

/// Append a complete subnegotiation `IAC SB <option> <params> IAC SE` to
/// `out`, doubling any `IAC` inside `params` so the region cannot be ended
/// early by its own payload.
pub fn push_subnegotiation(option: u8, params: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[IAC, SB, option]);
    for &byte in params {
        if byte == IAC {
            out.push(IAC);
        }
        out.push(byte);
    }
    out.extend_from_slice(&[IAC, SE]);
}

/// The human name of a command byte, for `toggle netdata` traces and the
/// `send ?` listing. Returns [`None`] for a byte that names no command.
#[must_use]
pub const fn command_name(command: u8) -> Option<&'static str> {
    Some(match command {
        IAC => "IAC",
        DONT => "DONT",
        DO => "DO",
        WONT => "WONT",
        WILL => "WILL",
        SB => "SB",
        GA => "GA",
        EL => "EL",
        EC => "EC",
        AYT => "AYT",
        AO => "AO",
        IP => "IP",
        BRK => "BRK",
        DM => "DM",
        NOP => "NOP",
        SE => "SE",
        EOR => "EOR",
        ABORT => "ABORT",
        SUSP => "SUSP",
        XEOF => "EOF",
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
