//! Option subnegotiation: the per-option payload codecs.
//!
//! Each option's `IAC SB … IAC SE` payload has its own tiny grammar, defined
//! by its own RFC. This module holds them together — TERMINAL-TYPE (RFC 1091),
//! NAWS (RFC 1073), TERMINAL-SPEED (RFC 1079), TOGGLE-FLOW-CONTROL (RFC 1080),
//! NEW-ENVIRON (RFC 1572) and STATUS (RFC 859) — because they are all the same
//! kind of thing to the session that dispatches them, and none is large enough
//! to earn a module of its own. RFC 1184 LINEMODE is the exception and lives in
//! [`crate::linemode`]: its payload is a state machine, not a field layout.
//!
//! Every decoder takes the already-`IAC`-collapsed, already-length-bounded
//! parameter region [`crate::nvt::Parser`] produced, and every one of them is
//! total: a payload that does not match its grammar yields [`None`] and the
//! caller ignores the subnegotiation rather than acting on a partial read.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::TerminalSize;

use crate::nvt;
use crate::option::{self, Options};

/// The sub-option command a peer may send for TERMINAL-TYPE, TERMINAL-SPEED,
/// NEW-ENVIRON and STATUS: `IS` carries data, `SEND` requests it, and
/// NEW-ENVIRON's `INFO` volunteers a change (RFC 1572 §2).
pub mod cmd {
    /// The payload carries the value.
    pub const IS: u8 = 0;
    /// The payload requests the value.
    pub const SEND: u8 = 1;
    /// The payload volunteers a changed value (NEW-ENVIRON only).
    pub const INFO: u8 = 2;
}

/// RFC 1572 §2 environment-variable type codes.
pub mod env {
    /// A well-known variable name follows.
    pub const VAR: u8 = 0;
    /// A value for the preceding name follows.
    pub const VALUE: u8 = 1;
    /// The next byte is literal, not a type code.
    pub const ESC: u8 = 2;
    /// A user-defined variable name follows.
    pub const USERVAR: u8 = 3;
}

/// RFC 1080 flow-control sub-option commands.
pub mod flow {
    /// The peer must stop honouring the flow-control characters.
    pub const OFF: u8 = 0;
    /// The peer must honour the flow-control characters.
    pub const ON: u8 = 1;
    /// Restart output on any character.
    pub const RESTART_ANY: u8 = 2;
    /// Restart output only on the XON character.
    pub const RESTART_XON: u8 = 3;
}

/// Largest number of environment variables the client will hold for
/// NEW-ENVIRON, and the longest name and value each may carry.
///
/// Fixed security bounds: the table is filled by the operator through
/// `environ define`, and the wire encoding of the whole table must fit the
/// [`crate::nvt::MAX_SUBNEG_LEN`] region the peer is allowed to send back.
pub const MAX_ENVIRON_VARS: usize = 16;
/// Longest environment-variable name.
pub const MAX_ENVIRON_NAME: usize = 64;
/// Longest environment-variable value.
pub const MAX_ENVIRON_VALUE: usize = 128;

/// A decoded TERMINAL-TYPE, TERMINAL-SPEED or STATUS subnegotiation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// The peer asks us for the value.
    Send,
    /// The peer states a value; the payload follows in the caller's slice.
    Is,
    /// The peer volunteers a changed value (NEW-ENVIRON `INFO`).
    Info,
}

/// Decode the leading sub-option command of `params`, returning it with the
/// remainder of the payload.
///
/// Returns [`None`] for an empty payload or a command byte outside the
/// [`cmd`] set — fail closed rather than guess.
#[must_use]
pub fn split_request(params: &[u8]) -> Option<(Request, &[u8])> {
    let (&head, rest) = params.split_first()?;
    let request = match head {
        cmd::IS => Request::Is,
        cmd::SEND => Request::Send,
        cmd::INFO => Request::Info,
        _ => return None,
    };
    Some((request, rest))
}

/// Encode the TERMINAL-TYPE reply `IAC SB TERMINAL-TYPE IS <type> IAC SE`.
///
/// The type is upper-cased as RFC 1091 §"THE TERMINAL-TYPE OPTION" requires,
/// and any byte outside printable ASCII is dropped rather than escaped: a
/// terminal name is a `LDH`-style token, and forwarding raw control bytes from
/// the local environment onto the wire would leak whatever `TERM` happens to
/// hold.
pub fn push_terminal_type(term: &str, out: &mut Vec<u8>) {
    let mut params = Vec::with_capacity(term.len() + 1);
    params.push(cmd::IS);
    for byte in term.bytes() {
        if byte.is_ascii_graphic() {
            params.push(byte.to_ascii_uppercase());
        }
    }
    nvt::push_subnegotiation(option::TERMINAL_TYPE, &params, out);
}

/// Encode the NAWS report `IAC SB NAWS <cols16> <rows16> IAC SE` (RFC 1073).
pub fn push_naws(size: TerminalSize, out: &mut Vec<u8>) {
    let cols = size.cols().to_be_bytes();
    let rows = size.rows().to_be_bytes();
    nvt::push_subnegotiation(option::NAWS, &[cols[0], cols[1], rows[0], rows[1]], out);
}

/// Encode the TERMINAL-SPEED reply `IAC SB TERMINAL-SPEED IS <tx>,<rx> IAC SE`
/// (RFC 1079). Both figures are the same because a TAIRiX console is not a
/// serial line with independent rates; the value is what the option exists to
/// convey, not a measurement.
pub fn push_terminal_speed(speed: u32, out: &mut Vec<u8>) {
    let mut params = alloc::vec![cmd::IS];
    params.extend_from_slice(alloc::format!("{speed},{speed}").as_bytes());
    nvt::push_subnegotiation(option::TERMINAL_SPEED, &params, out);
}

/// Encode a RFC 1080 flow-control acknowledgement.
pub fn push_flow_control(command: u8, out: &mut Vec<u8>) {
    nvt::push_subnegotiation(option::TOGGLE_FLOW_CONTROL, &[command], out);
}

/// Decode a RFC 1080 flow-control command, returning whether the peer asked
/// for the flow-control characters to be honoured.
///
/// Returns [`None`] for a payload that is not exactly one recognised command
/// byte.
#[must_use]
pub fn flow_control_enabled(params: &[u8]) -> Option<bool> {
    match params {
        [flow::OFF] => Some(false),
        [flow::ON | flow::RESTART_ANY | flow::RESTART_XON] => Some(true),
        _ => None,
    }
}

/// Encode the RFC 859 STATUS reply: `IS` followed by one `WILL <option>` for
/// every option enabled on our side and one `DO <option>` for every option
/// enabled on the peer's.
///
/// Only the options this client implements are walked, so the reply describes
/// the real negotiated state and can never claim an option the client does not
/// have.
pub fn push_status(options: &Options, out: &mut Vec<u8>) {
    let mut params = Vec::new();
    params.push(cmd::IS);
    for &code in option::SUPPORTED {
        if options.local(code) {
            params.extend_from_slice(&[nvt::WILL, code]);
        }
        if options.remote(code) {
            params.extend_from_slice(&[nvt::DO, code]);
        }
    }
    nvt::push_subnegotiation(option::STATUS, &params, out);
}

/// One environment variable the operator has defined for NEW-ENVIRON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironVar {
    /// The variable's name.
    pub name: String,
    /// Its value.
    pub value: String,
    /// Whether it is a RFC 1572 well-known `VAR` (`USER`, `JOB`, `ACCT`,
    /// `PRINTER`, `SYSTEMTYPE`, `DISPLAY`) rather than a `USERVAR`.
    pub well_known: bool,
    /// Whether the operator has exported it. An unexported variable is held
    /// but never transmitted.
    pub exported: bool,
}

/// The RFC 1572 well-known variable names, which are sent as `VAR` rather than
/// `USERVAR`.
const WELL_KNOWN: &[&str] = &["ACCT", "DISPLAY", "JOB", "PRINTER", "SYSTEMTYPE", "USER"];

/// Why an `environ define` was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironFault {
    /// The table already holds [`MAX_ENVIRON_VARS`] variables.
    TableFull,
    /// The name is empty or longer than [`MAX_ENVIRON_NAME`].
    NameLength,
    /// The value is longer than [`MAX_ENVIRON_VALUE`].
    ValueLength,
    /// The name or value carries a byte outside printable ASCII.
    NotPrintable,
    /// No variable of that name is defined.
    Unknown,
}

/// The operator-defined environment the client may disclose over NEW-ENVIRON.
///
/// It is **empty** until the operator defines and exports a variable. The
/// client never sweeps its own process environment onto the wire: RFC 1572
/// discloses local configuration to a remote host, so the default is to
/// disclose nothing and every disclosure is an explicit act.
#[derive(Debug, Default)]
pub struct Environ {
    vars: Vec<EnvironVar>,
}

impl Environ {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self { vars: Vec::new() }
    }

    /// Every variable, in definition order.
    #[must_use]
    pub fn vars(&self) -> &[EnvironVar] {
        &self.vars
    }

    /// Define or redefine `name` with `value`, unexported.
    ///
    /// # Errors
    ///
    /// An [`EnvironFault`] describing the bound or character rule the request
    /// broke; the table is left unchanged.
    pub fn define(&mut self, name: &str, value: &str) -> Result<(), EnvironFault> {
        if name.is_empty() || name.len() > MAX_ENVIRON_NAME {
            return Err(EnvironFault::NameLength);
        }
        if value.len() > MAX_ENVIRON_VALUE {
            return Err(EnvironFault::ValueLength);
        }
        if !name.bytes().all(|b| b.is_ascii_graphic())
            || !value.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        {
            return Err(EnvironFault::NotPrintable);
        }
        let well_known = WELL_KNOWN.contains(&name);
        if let Some(existing) = self.vars.iter_mut().find(|var| var.name == name) {
            existing.value = String::from(value);
            existing.well_known = well_known;
            return Ok(());
        }
        if self.vars.len() == MAX_ENVIRON_VARS {
            return Err(EnvironFault::TableFull);
        }
        self.vars.push(EnvironVar {
            name: String::from(name),
            value: String::from(value),
            well_known,
            exported: false,
        });
        Ok(())
    }

    /// Remove `name` from the table.
    ///
    /// # Errors
    ///
    /// [`EnvironFault::Unknown`] when no such variable is defined.
    pub fn undefine(&mut self, name: &str) -> Result<(), EnvironFault> {
        let before = self.vars.len();
        self.vars.retain(|var| var.name != name);
        if self.vars.len() == before {
            return Err(EnvironFault::Unknown);
        }
        Ok(())
    }

    /// Mark `name` as disclosable (`exported = true`) or not.
    ///
    /// # Errors
    ///
    /// [`EnvironFault::Unknown`] when no such variable is defined.
    pub fn set_exported(&mut self, name: &str, exported: bool) -> Result<(), EnvironFault> {
        let var = self
            .vars
            .iter_mut()
            .find(|var| var.name == name)
            .ok_or(EnvironFault::Unknown)?;
        var.exported = exported;
        Ok(())
    }

    /// Encode the RFC 1572 `IS` reply to a `SEND` whose payload is `request`.
    ///
    /// A `SEND` with an empty payload asks for everything; one naming
    /// variables asks for those. Either way only **exported** variables are
    /// disclosed, and a name the table does not hold is answered by name with
    /// no value (RFC 1572 §2: "the value is not defined"), never by silently
    /// substituting another variable's value.
    pub fn push_is_reply(&self, request: &[u8], out: &mut Vec<u8>) {
        let mut params = Vec::new();
        params.push(cmd::IS);
        let names = requested_names(request);
        if names.is_empty() {
            for var in self.vars.iter().filter(|var| var.exported) {
                push_var(var.well_known, &var.name, Some(&var.value), &mut params);
            }
        } else {
            for name in names {
                if let Some(var) = self
                    .vars
                    .iter()
                    .find(|var| var.exported && var.name.as_bytes() == name.as_slice())
                {
                    push_var(var.well_known, &var.name, Some(&var.value), &mut params);
                } else {
                    let text = String::from_utf8_lossy(&name);
                    push_var(
                        WELL_KNOWN.contains(&text.as_ref()),
                        &text,
                        None,
                        &mut params,
                    );
                }
            }
        }
        nvt::push_subnegotiation(option::NEW_ENVIRON, &params, out);
    }
}

/// Split a RFC 1572 `SEND` payload into the variable names it asks for.
///
/// `VAR` and `USERVAR` both introduce a name; `ESC` makes the next byte
/// literal. A trailing type code with no name (the "send all of this class"
/// form) contributes nothing, so an all-`VAR` request reads as "send
/// everything" — which is what [`Environ::push_is_reply`] then does.
fn requested_names(request: &[u8]) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut escaped = false;
    for &byte in request {
        if escaped {
            escaped = false;
            if let Some(name) = current.as_mut() {
                name.push(byte);
            }
            continue;
        }
        match byte {
            env::ESC => escaped = true,
            env::VAR | env::USERVAR => {
                if let Some(name) = current.take() {
                    if !name.is_empty() {
                        names.push(name);
                    }
                }
                current = Some(Vec::new());
            }
            // A `VALUE` in a `SEND` is malformed; ending the current name is
            // the fail-closed reading (the bytes after it are not a name).
            env::VALUE => {
                if let Some(name) = current.take() {
                    if !name.is_empty() {
                        names.push(name);
                    }
                }
            }
            other => {
                if let Some(name) = current.as_mut() {
                    name.push(other);
                }
            }
        }
    }
    if let Some(name) = current {
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
}

/// Append one RFC 1572 `VAR`/`USERVAR` name (and optional `VALUE`) to a reply,
/// `ESC`-escaping any byte that would otherwise read as a type code.
fn push_var(well_known: bool, name: &str, value: Option<&str>, params: &mut Vec<u8>) {
    params.push(if well_known { env::VAR } else { env::USERVAR });
    push_escaped(name.as_bytes(), params);
    if let Some(value) = value {
        params.push(env::VALUE);
        push_escaped(value.as_bytes(), params);
    }
}

/// Append `text` with the RFC 1572 `ESC` escaping applied.
///
/// [`Environ::define`] already refuses a non-printable name or value, so the
/// escape is not reached from the table today; the encoder still applies it
/// because emitting an ambiguous stream is the encoder's defect to avoid, not
/// its caller's to prevent.
fn push_escaped(text: &[u8], params: &mut Vec<u8>) {
    for &byte in text {
        if matches!(byte, env::VAR | env::VALUE | env::ESC | env::USERVAR) {
            params.push(env::ESC);
        }
        params.push(byte);
    }
}

#[cfg(test)]
mod tests;
