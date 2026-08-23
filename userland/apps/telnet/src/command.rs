//! The `telnet` argument grammar.
//!
//! The option set and spellings track inetutils `telnet` so a user who knows
//! that tool finds these already familiar. The switches inetutils defines for
//! facilities TAIRiX does not have — `-n tracefile` (no trace file), `-r`
//! (no rlogin), `-c` (no `.telnetrc`, since TAIRiX has no `/etc`) — are
//! deliberately absent rather than accepted and ignored: a switch that silently
//! does nothing is worse than one that is honestly unknown.

use alloc::string::{String, ToString};
use core::fmt;

use tairix_abi::net_ipc::NetAddrFamily;

/// The default port when the command line names none: the assigned telnet port.
pub const DEFAULT_PORT: u16 = 23;

/// The default escape character, `^]`, as BSD telnet uses.
pub const DEFAULT_ESCAPE: u8 = 0x1D;

/// The remote endpoint an invocation or an `open` command names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// The host as the user spelled it — a literal address or a name to
    /// resolve. Kept verbatim so every diagnostic names what was asked for.
    pub host: String,
    /// The TCP port.
    pub port: u16,
}

/// The operating parameters an invocation sets up.
// The bools transcribe inetutils `telnet`'s independent switches one-to-one
// (the `ss`/`df` Options precedent); folding them into state enums would
// obscure the grammar they mirror.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// Restrict resolution to one address family (`-4` / `-6`).
    pub family: Option<NetAddrFamily>,
    /// Request BINARY on the receive side (`-8`).
    pub binary_in: bool,
    /// Request BINARY on the transmit side (`-8` / `-L`).
    pub binary_out: bool,
    /// The escape character, or [`None`] when `-E` disabled it.
    pub escape: Option<u8>,
    /// Disclose the login name over NEW-ENVIRON (`-a` / `-l`).
    pub auto_login: bool,
    /// The login name to disclose (`-l`); with `-a` alone it comes from the
    /// session's `USER`.
    pub user: Option<String>,
    /// A local address to bind before connecting (`-b`).
    pub bind: Option<String>,
    /// Trace option negotiation on standard error (`-d`).
    pub debug: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            family: None,
            binary_in: false,
            binary_out: false,
            escape: Some(DEFAULT_ESCAPE),
            auto_login: false,
            user: None,
            bind: None,
            debug: false,
        }
    }
}

/// A parsed `telnet` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `telnet`'s own short help (`-?`/`-h`/`--help`).
    Help,
    /// Start a session. With no [`Target`] the tool opens in command mode, as
    /// a bare `telnet` does, and the operator connects with `open`.
    Session {
        /// The operating parameters.
        config: Config,
        /// The host to connect to at once, if one was named.
        target: Option<Target>,
    },
}

/// Why an argument vector was not understood.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// An unrecognised option.
    UnknownOption(String),
    /// An option that takes a value was given none.
    MissingValue(String),
    /// The port operand is not a number in `1..=65535`. TAIRiX has no services
    /// database, so a service *name* cannot be looked up.
    BadPort(String),
    /// More operands than `host [port]`.
    TooManyOperands,
    /// The `-e` value is not a single character or `^X` control spelling.
    BadEscape(String),
    /// `-4` and `-6` were both given.
    FamilyConflict,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(opt) => write!(f, "unknown option '{opt}'"),
            Self::MissingValue(opt) => write!(f, "option '{opt}' requires a value"),
            Self::BadPort(port) => write!(
                f,
                "'{port}' is not a port number (a service name cannot be resolved)"
            ),
            Self::TooManyOperands => f.write_str("only 'host [port]' may be given"),
            Self::BadEscape(value) => write!(
                f,
                "'{value}' is not an escape character (give one character, or '^X')"
            ),
            Self::FamilyConflict => f.write_str("'-4' and '-6' cannot both be given"),
        }
    }
}

/// Decode an escape-character spelling.
///
/// An empty value means "no escape character" (the `-E` spelling in value
/// form). `^X` is the control character `X` names, `^?` is Delete, and a single
/// byte is itself. Returns [`None`] for anything else, which the caller reports
/// rather than guessing at.
#[must_use]
pub fn parse_escape(value: &str) -> Option<Option<u8>> {
    if value.is_empty() {
        return Some(None);
    }
    let bytes = value.as_bytes();
    match *bytes {
        [b'^', b'?'] => Some(Some(0x7F)),
        [b'^', letter] if letter.is_ascii() && letter >= 0x40 => Some(Some(letter & 0x1F)),
        [single] => Some(Some(single)),
        _ => None,
    }
}

/// Parse a `telnet` argument vector.
///
/// Options precede the operands; `--` ends option processing. A value-taking
/// option accepts it attached (`-e^]`, `--escape=^]`) or as the next argument.
/// `-?`, `-h` and `--help` request the short help and win over any operand.
///
/// # Errors
///
/// A [`ParseError`] describing the first malformed argument.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut config = Config::default();
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut want_help = false;
    let mut options_ended = false;
    let mut saw_v4 = false;
    let mut saw_v6 = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_ended || !arg.starts_with('-') || arg == "-" {
            // A bare `-` is an operand, not an option.
            if host.is_none() {
                host = Some(arg.to_string());
            } else if port.is_none() {
                port = Some(parse_port(arg)?);
            } else {
                return Err(ParseError::TooManyOperands);
            }
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if apply_flag(arg, &mut config, &mut want_help, &mut saw_v4, &mut saw_v6) {
            continue;
        }
        // The value-taking options resolve their value once, from whichever
        // spelling was used, so the attached and separated forms cannot drift.
        if let Some((kind, attached)) = value_option(arg) {
            let value = if let Some(value) = attached {
                value
            } else {
                let next = args
                    .get(index)
                    .ok_or_else(|| ParseError::MissingValue(arg.to_string()))?;
                index += 1;
                next
            };
            apply_value(kind, value, &mut config)?;
            continue;
        }
        return Err(ParseError::UnknownOption(arg.to_string()));
    }

    if want_help {
        return Ok(Command::Help);
    }
    if saw_v4 && saw_v6 {
        return Err(ParseError::FamilyConflict);
    }
    if saw_v4 {
        config.family = Some(NetAddrFamily::V4);
    } else if saw_v6 {
        config.family = Some(NetAddrFamily::V6);
    }
    // A port is only ever taken as the *second* operand, so it cannot exist
    // without a host to attach it to.
    let target = host.map(|host| Target {
        host,
        port: port.unwrap_or(DEFAULT_PORT),
    });
    Ok(Command::Session { config, target })
}

/// Parse a port operand: a decimal number in `1..=65535`.
///
/// # Errors
///
/// [`ParseError::BadPort`] for anything else, including a service name — there
/// is no services database to look one up in, and silently defaulting to 23
/// would connect somewhere the user did not ask for.
pub fn parse_port(text: &str) -> Result<u16, ParseError> {
    match text.parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(ParseError::BadPort(text.to_string())),
    }
}

/// Apply one argument-free option, reporting whether `arg` named one.
fn apply_flag(
    arg: &str,
    config: &mut Config,
    want_help: &mut bool,
    saw_v4: &mut bool,
    saw_v6: &mut bool,
) -> bool {
    match arg {
        "-?" | "-h" | "--help" => *want_help = true,
        "-4" | "--ipv4" => *saw_v4 = true,
        "-6" | "--ipv6" => *saw_v6 = true,
        "-8" | "--binary" => {
            config.binary_in = true;
            config.binary_out = true;
        }
        "-L" | "--eight-bit-output" => config.binary_out = true,
        "-E" | "--no-escape" => config.escape = None,
        "-a" | "--login" => config.auto_login = true,
        "-d" | "--debug" => config.debug = true,
        _ => return false,
    }
    true
}

/// Apply one value-taking option.
fn apply_value(kind: ValueOption, value: &str, config: &mut Config) -> Result<(), ParseError> {
    match kind {
        ValueOption::Escape => {
            config.escape =
                parse_escape(value).ok_or_else(|| ParseError::BadEscape(value.to_string()))?;
        }
        ValueOption::User => {
            config.user = Some(value.to_string());
            config.auto_login = true;
        }
        ValueOption::Bind => config.bind = Some(value.to_string()),
    }
    Ok(())
}

/// The options that take a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueOption {
    /// `-e` / `--escape`.
    Escape,
    /// `-l` / `--user`.
    User,
    /// `-b` / `--bind`.
    Bind,
}

/// The value-taking option `arg` names, with its attached value when it
/// carried one (`-e^]`, `--escape=^]`) and [`None`] when the value is the next
/// argument. [`None`] overall when `arg` names no value-taking option.
fn value_option(arg: &str) -> Option<(ValueOption, Option<&str>)> {
    for (kind, short, long) in [
        (ValueOption::Escape, "-e", "--escape"),
        (ValueOption::User, "-l", "--user"),
        (ValueOption::Bind, "-b", "--bind"),
    ] {
        if arg == short || arg == long {
            return Some((kind, None));
        }
        if let Some(rest) = arg.strip_prefix(long) {
            if let Some(value) = rest.strip_prefix('=') {
                return Some((kind, Some(value)));
            }
            continue;
        }
        if let Some(rest) = arg.strip_prefix(short) {
            if !rest.is_empty() {
                return Some((kind, Some(rest)));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
