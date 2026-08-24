//! The `ping` command shape and its parser.
//!
//! The option surface follows iputils/`ping(8)`: `-c count` bounds the number
//! of requests (the default is unbounded), `-i interval` sets the seconds
//! between requests, `-s size` sets the payload byte count, `-W timeout` bounds
//! the wait for each reply, `-w deadline` bounds the whole run, `-4`/`-6` force
//! the address family, `-n` prints numeric addresses (always in force on TAIRiX
//! — there is no name resolver in this plan, so the target must be a literal
//! address — and accepted for familiarity), and `-q` is quiet (summary only).
//! Short help is the reserved `-?`/`--help` pair (plans/APPS.md §4).
//!
//! A value-taking short flag accepts its value attached (`-c5`) or as the
//! next argument (`-c 5`); long options accept `--count 5` or `--count=5`.
//! Exactly one target operand — a literal IPv4 or IPv6 address — is
//! required. There is no DNS resolution (not in `plans/NETWORK.md`), so a
//! hostname is a usage error, reported honestly rather than silently.

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr};

use tairix_abi::net_ipc::NetAddrFamily;

/// Default payload size in bytes (the classic `ping` 56, which with the
/// 8-byte ICMP header reads as "64 bytes from").
pub const DEFAULT_SIZE: usize = 56;

/// Largest payload the tool will send: the inline socket transport bound.
pub const MAX_SIZE: usize = tairix_abi::net::SOCKET_MAX_DATAGRAM;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Default seconds between echo requests, as nanoseconds.
pub const DEFAULT_INTERVAL_NS: u64 = NANOS_PER_SEC;

/// Default per-reply wait, as nanoseconds.
pub const DEFAULT_TIMEOUT_NS: u64 = NANOS_PER_SEC;

/// A resolved ping target: the literal as typed, its family, and its bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// The address literal exactly as the user typed it (for display).
    pub display: String,
    /// The address family.
    pub family: NetAddrFamily,
    /// The address bytes (IPv4 uses the first four).
    pub addr: [u8; 16],
}

/// A parsed `ping` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `ping`'s own short help (`-?`/`--help`).
    Help,
    /// Ping the target with the resolved configuration.
    Run(Config),
}

/// Everything a `ping` run needs, straight from the option grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// The target address to ping.
    pub target: Target,
    /// Number of requests to send, or [`None`] for unbounded.
    pub count: Option<u32>,
    /// Nanoseconds between requests.
    pub interval_ns: u64,
    /// Nanoseconds to wait for each reply before recording a loss.
    pub timeout_ns: u64,
    /// Overall run deadline in nanoseconds (`-w`), or [`None`].
    pub deadline_ns: Option<u64>,
    /// Payload byte count.
    pub size: usize,
    /// Quiet: print only the header and the final statistics.
    pub quiet: bool,
}

/// Why an argument vector was not a valid `ping` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// An option that is not part of the surface.
    UnknownOption(String),
    /// A value-taking option was given without its value.
    MissingValue(String),
    /// A value-taking option's value did not parse (or was out of range).
    InvalidValue {
        /// The option, e.g. `-c`.
        option: String,
        /// The rejected value.
        value: String,
    },
    /// No target address was given.
    MissingTarget,
    /// More than one target operand was given.
    ExtraOperand(String),
    /// The target was not a literal IP address of the required family
    /// (there is no name resolution in this plan).
    InvalidTarget(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(opt) => write!(f, "unrecognized option '{opt}'"),
            Self::MissingValue(opt) => write!(f, "option '{opt}' requires a value"),
            Self::InvalidValue { option, value } => {
                write!(f, "invalid value '{value}' for option '{option}'")
            }
            Self::MissingTarget => f.write_str("no target address given"),
            Self::ExtraOperand(word) => write!(f, "unexpected extra operand '{word}'"),
            Self::InvalidTarget(word) => write!(
                f,
                "'{word}' is not a literal IP address (name resolution is not supported)"
            ),
        }
    }
}

/// The forced address family, if `-4`/`-6` was given.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Family {
    Any,
    V4,
    V6,
}

/// Accumulated option state during parsing.
struct Builder {
    count: Option<u32>,
    interval_ns: u64,
    timeout_ns: u64,
    deadline_ns: Option<u64>,
    size: usize,
    quiet: bool,
    family: Family,
    target: Option<String>,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            count: None,
            interval_ns: DEFAULT_INTERVAL_NS,
            timeout_ns: DEFAULT_TIMEOUT_NS,
            deadline_ns: None,
            size: DEFAULT_SIZE,
            quiet: false,
            family: Family::Any,
            target: None,
        }
    }
}

/// Parse an argument vector (without the program name).
///
/// # Errors
///
/// A [`ParseError`] naming the offending argument; the caller reports it
/// with the usage banner and exits `2`.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut builder = Builder::default();
    let mut options_ended = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if options_ended || arg == "-" || !arg.starts_with('-') {
            set_target(&mut builder, arg)?;
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            parse_long(&mut builder, long, arg, args, &mut i)?;
            if matches!(builder_help(long), Some(())) {
                return Ok(Command::Help);
            }
            continue;
        }
        // A short cluster or a value-taking short option.
        if parse_short(&mut builder, arg, args, &mut i)? {
            return Ok(Command::Help);
        }
    }
    build(builder)
}

/// `Some(())` when `long` is the help option, so [`parse`] can short-circuit.
fn builder_help(long: &str) -> Option<()> {
    let key = long.split('=').next().unwrap_or(long);
    (key == "help").then_some(())
}

/// Parse one `--long` option (possibly `--key=value`), consuming a
/// following value argument when the option takes one and none is attached.
fn parse_long(
    builder: &mut Builder,
    long: &str,
    original: &str,
    args: &[&str],
    i: &mut usize,
) -> Result<(), ParseError> {
    let (key, inline) = match long.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (long, None),
    };
    let mut take_value = |key: &str| -> Result<String, ParseError> {
        if let Some(v) = inline {
            return Ok(v.to_string());
        }
        if *i < args.len() {
            let v = args[*i].to_string();
            *i += 1;
            Ok(v)
        } else {
            Err(ParseError::MissingValue(format!("--{key}")))
        }
    };
    match key {
        // `help` is detected by the caller (`builder_help`); `numeric` is
        // always in force. Both need no builder change.
        "help" | "numeric" => {}
        "count" => builder.count = Some(parse_count(&take_value(key)?, "--count")?),
        "interval" => builder.interval_ns = parse_seconds(&take_value(key)?, "--interval")?,
        "timeout" => builder.timeout_ns = parse_seconds(&take_value(key)?, "--timeout")?,
        "deadline" => builder.deadline_ns = Some(parse_seconds(&take_value(key)?, "--deadline")?),
        "size" => builder.size = parse_size(&take_value(key)?, "--size")?,
        "quiet" => builder.quiet = true,
        "ipv4" => builder.family = Family::V4,
        "ipv6" => builder.family = Family::V6,
        _ => return Err(ParseError::UnknownOption(original.to_string())),
    }
    Ok(())
}

/// Parse one short-option argument. Returns `Ok(true)` when it requested
/// help (so [`parse`] returns [`Command::Help`]).
fn parse_short(
    builder: &mut Builder,
    arg: &str,
    args: &[&str],
    i: &mut usize,
) -> Result<bool, ParseError> {
    let flags = &arg[1..];
    for (pos, flag) in flags.char_indices() {
        match flag {
            '?' => return Ok(true),
            '4' => builder.family = Family::V4,
            '6' => builder.family = Family::V6,
            'n' => {}
            'q' => builder.quiet = true,
            'c' | 'i' | 's' | 'W' | 'w' => {
                // The value is the rest of this argument, or the next one.
                let rest = &flags[pos + flag.len_utf8()..];
                let value = if rest.is_empty() {
                    if *i < args.len() {
                        let v = args[*i];
                        *i += 1;
                        v
                    } else {
                        return Err(ParseError::MissingValue(format!("-{flag}")));
                    }
                } else {
                    rest
                };
                apply_value_flag(builder, flag, value)?;
                return Ok(false);
            }
            other => return Err(ParseError::UnknownOption(format!("-{other}"))),
        }
    }
    Ok(false)
}

/// Apply a value-taking short flag's parsed value.
fn apply_value_flag(builder: &mut Builder, flag: char, value: &str) -> Result<(), ParseError> {
    match flag {
        'c' => builder.count = Some(parse_count(value, "-c")?),
        'i' => builder.interval_ns = parse_seconds(value, "-i")?,
        's' => builder.size = parse_size(value, "-s")?,
        'W' => builder.timeout_ns = parse_seconds(value, "-W")?,
        'w' => builder.deadline_ns = Some(parse_seconds(value, "-w")?),
        _ => unreachable!("only value flags reach here"),
    }
    Ok(())
}

/// Record the single target operand, rejecting a second one.
fn set_target(builder: &mut Builder, word: &str) -> Result<(), ParseError> {
    if builder.target.is_some() {
        return Err(ParseError::ExtraOperand(word.to_string()));
    }
    builder.target = Some(word.to_string());
    Ok(())
}

/// Finalise the builder into a [`Command`], resolving and validating the
/// target address against the forced family.
fn build(builder: Builder) -> Result<Command, ParseError> {
    let literal = builder.target.ok_or(ParseError::MissingTarget)?;
    let target = resolve_target(&literal, builder.family)?;
    Ok(Command::Run(Config {
        target,
        count: builder.count,
        interval_ns: builder.interval_ns,
        timeout_ns: builder.timeout_ns,
        deadline_ns: builder.deadline_ns,
        size: builder.size,
        quiet: builder.quiet,
    }))
}

/// Resolve a literal IP address string into a [`Target`] of the required
/// family. There is no name resolution in this plan, so a non-literal is a
/// usage error rather than a silent failure (fail loud).
fn resolve_target(literal: &str, family: Family) -> Result<Target, ParseError> {
    let v4 = literal.parse::<Ipv4Addr>().ok();
    let v6 = literal.parse::<Ipv6Addr>().ok();
    match family {
        Family::V4 => v4
            .map(|a| target_v4(literal, a))
            .ok_or_else(|| ParseError::InvalidTarget(literal.to_string())),
        Family::V6 => v6
            .map(|a| target_v6(literal, a))
            .ok_or_else(|| ParseError::InvalidTarget(literal.to_string())),
        Family::Any => v4
            .map(|a| target_v4(literal, a))
            .or_else(|| v6.map(|a| target_v6(literal, a)))
            .ok_or_else(|| ParseError::InvalidTarget(literal.to_string())),
    }
}

fn target_v4(literal: &str, addr: Ipv4Addr) -> Target {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&addr.octets());
    Target {
        display: literal.to_string(),
        family: NetAddrFamily::V4,
        addr: bytes,
    }
}

fn target_v6(literal: &str, addr: Ipv6Addr) -> Target {
    Target {
        display: literal.to_string(),
        family: NetAddrFamily::V6,
        addr: addr.octets(),
    }
}

/// Parse a positive request count (`-c`); zero and overflow are refused.
fn parse_count(value: &str, option: &str) -> Result<u32, ParseError> {
    let n: u32 = value
        .parse()
        .map_err(|_| invalid(option, value))
        .and_then(|n| {
            if n == 0 {
                Err(invalid(option, value))
            } else {
                Ok(n)
            }
        })?;
    Ok(n)
}

/// Parse a payload byte count (`-s`), bounded to [`MAX_SIZE`].
fn parse_size(value: &str, option: &str) -> Result<usize, ParseError> {
    let n: usize = value.parse().map_err(|_| invalid(option, value))?;
    if n > MAX_SIZE {
        return Err(invalid(option, value));
    }
    Ok(n)
}

/// Parse a non-negative decimal number of seconds into nanoseconds, with a
/// fractional part of up to nine digits. Bounded so the conversion cannot
/// overflow a `u64` (a value above `~year` is refused).
fn parse_seconds(value: &str, option: &str) -> Result<u64, ParseError> {
    let (whole_str, frac_str) = match value.split_once('.') {
        Some((w, f)) => (w, f),
        None => (value, ""),
    };
    // Reject empty, sign, or non-digit content: fail closed, never guess.
    if whole_str.is_empty() && frac_str.is_empty() {
        return Err(invalid(option, value));
    }
    let whole: u64 = if whole_str.is_empty() {
        0
    } else {
        whole_str.parse().map_err(|_| invalid(option, value))?
    };
    // A whole-seconds value that would overflow when scaled to ns, or a
    // multi-day interval, is a usage error rather than a wrapped value.
    if whole > 31_536_000 {
        return Err(invalid(option, value));
    }
    let mut nanos = whole
        .checked_mul(NANOS_PER_SEC)
        .ok_or_else(|| invalid(option, value))?;
    if !frac_str.is_empty() {
        if frac_str.len() > 9 || !frac_str.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid(option, value));
        }
        // Right-pad to nanosecond precision (nine digits).
        let mut scaled = frac_str.to_string();
        while scaled.len() < 9 {
            scaled.push('0');
        }
        let frac: u64 = scaled.parse().map_err(|_| invalid(option, value))?;
        nanos = nanos
            .checked_add(frac)
            .ok_or_else(|| invalid(option, value))?;
    }
    Ok(nanos)
}

fn invalid(option: &str, value: &str) -> ParseError {
    ParseError::InvalidValue {
        option: option.to_string(),
        value: value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str]) -> Config {
        match parse(args).expect("parses") {
            Command::Run(config) => config,
            Command::Help => panic!("expected a run, got help"),
        }
    }

    #[test]
    fn a_bare_ipv4_target_defaults_the_rest() {
        let config = config(&["10.0.2.2"]);
        assert_eq!(config.target.family, NetAddrFamily::V4);
        assert_eq!(&config.target.addr[..4], &[10, 0, 2, 2]);
        assert_eq!(config.count, None);
        assert_eq!(config.interval_ns, DEFAULT_INTERVAL_NS);
        assert_eq!(config.timeout_ns, DEFAULT_TIMEOUT_NS);
        assert_eq!(config.size, DEFAULT_SIZE);
        assert!(!config.quiet);
    }

    #[test]
    fn an_ipv6_literal_parses() {
        let config = config(&["fe80::1"]);
        assert_eq!(config.target.family, NetAddrFamily::V6);
        assert_eq!(config.target.addr[0], 0xfe);
        assert_eq!(config.target.addr[1], 0x80);
        assert_eq!(config.target.addr[15], 0x01);
    }

    #[test]
    fn short_value_flags_attach_or_separate() {
        let attached = config(&["-c3", "10.0.0.1"]);
        let separate = config(&["-c", "3", "10.0.0.1"]);
        assert_eq!(attached.count, separate.count);
        assert_eq!(attached.count, Some(3));
    }

    #[test]
    fn boolean_short_flags_cluster() {
        let config = config(&["-nq4", "10.0.0.1"]);
        assert!(config.quiet);
        assert_eq!(config.target.family, NetAddrFamily::V4);
    }

    #[test]
    fn long_options_and_equals_form() {
        let config = config(&["--count=5", "--interval", "0.5", "--size=8", "10.0.0.1"]);
        assert_eq!(config.count, Some(5));
        assert_eq!(config.interval_ns, 500_000_000);
        assert_eq!(config.size, 8);
    }

    #[test]
    fn fractional_seconds_parse_to_nanoseconds() {
        assert_eq!(parse_seconds("1", "-i"), Ok(1_000_000_000));
        assert_eq!(parse_seconds("0.25", "-i"), Ok(250_000_000));
        assert_eq!(parse_seconds("2.5", "-i"), Ok(2_500_000_000));
        assert_eq!(parse_seconds(".001", "-i"), Ok(1_000_000));
        assert!(parse_seconds("", "-i").is_err());
        assert!(parse_seconds("x", "-i").is_err());
        assert!(parse_seconds("1.2345678901", "-i").is_err());
    }

    #[test]
    fn help_wins_from_either_form() {
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-?", "10.0.0.1"]), Ok(Command::Help));
    }

    #[test]
    fn a_forced_family_must_match_the_literal() {
        assert!(matches!(
            parse(&["-6", "10.0.2.2"]),
            Err(ParseError::InvalidTarget(_))
        ));
        assert!(matches!(
            parse(&["-4", "fe80::1"]),
            Err(ParseError::InvalidTarget(_))
        ));
    }

    #[test]
    fn a_hostname_is_a_usage_error_not_a_silent_failure() {
        assert!(matches!(
            parse(&["example.com"]),
            Err(ParseError::InvalidTarget(_))
        ));
    }

    #[test]
    fn a_missing_target_is_refused() {
        assert_eq!(parse(&["-c", "3"]), Err(ParseError::MissingTarget));
    }

    #[test]
    fn a_second_target_is_refused() {
        assert!(matches!(
            parse(&["10.0.0.1", "10.0.0.2"]),
            Err(ParseError::ExtraOperand(_))
        ));
    }

    #[test]
    fn a_zero_count_is_refused() {
        assert!(matches!(
            parse(&["-c", "0", "10.0.0.1"]),
            Err(ParseError::InvalidValue { .. })
        ));
    }

    #[test]
    fn a_missing_value_fails_closed() {
        assert_eq!(
            parse(&["-c"]),
            Err(ParseError::MissingValue("-c".to_string()))
        );
    }

    #[test]
    fn an_oversize_payload_is_refused() {
        let big = (MAX_SIZE + 1).to_string();
        assert!(matches!(
            parse(&["-s", &big, "10.0.0.1"]),
            Err(ParseError::InvalidValue { .. })
        ));
    }

    #[test]
    fn an_unknown_option_fails_closed() {
        assert_eq!(
            parse(&["-z", "10.0.0.1"]),
            Err(ParseError::UnknownOption("-z".to_string()))
        );
        assert_eq!(
            parse(&["--bogus", "10.0.0.1"]),
            Err(ParseError::UnknownOption("--bogus".to_string()))
        );
    }
}
