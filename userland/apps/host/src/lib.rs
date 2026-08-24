//! TAIRiX `host` — look up a name over DNS (`plans/DNS.md` DNS3, a
//! `plans/APPS.md` command app).
//!
//! `host` resolves a domain name to its addresses in the familiar
//! `bind-utils` shape: one line per answer (`<name> has address <ipv4>` /
//! `<name> has IPv6 address <ipv6>`), a `has no <TYPE> record` line for an
//! empty answer, and the `not found` / `connection timed out` diagnostics
//! for a negative or unreachable lookup. With no `-t`, it looks up both the
//! `A` and `AAAA` records the stub resolver supports; `-t <type>` restricts
//! the lookup to one. `-?`/`--help` render the tool's own short help from
//! its bundled `Help/` tree through the shared `lib/help` engine.
//!
//! # Where the answers come from
//!
//! Name resolution is done by the shared userland stub resolver
//! ([`tairix_resolver`]): it reads the host's configured recursive DNS servers
//! through the ungated `NET_RESOLVER_SERVERS` System Information query (the one
//! active-server set `state:net/resolver/servers` also exposes) and drives the
//! pure, RFC 1035 / RFC 5452-hardened DNS engine in `tairix-net` over an
//! ordinary capability-gated UDP socket. There is no second DNS implementation
//! and no `/etc/resolv.conf`; every response is validated by the engine before
//! an address is surfaced.
//!
//! # What this crate is
//!
//! A **parse-and-render engine**, not a resolver: for one parsed [`Command`]
//! it drives the injected seams and renders the answers. The operations that
//! touch the outside world are injected so the whole engine is host-tested
//! with no kernel and no network:
//!
//! * [`Resolver`] — resolve one `(name, record type)` to a
//!   [`Resolution`].
//! * [`Output`] — the answers on standard output, diagnostics on standard
//!   error.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree and read back at
//! runtime through the injected [`tairix_help::HelpSource`] seam
//! (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, the `lib/net` DNS vocabulary,
//! and the `lib/resolver` client, so this userland tool never links a kernel
//! or driver crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};
use tairix_net::dns::{RecordType, Resolution, ResolveStatus};
use tairix_resolver::ResolveError;

/// The one-line usage banner, printed on a usage error and as the fallback
/// when the bundled help document is unavailable.
pub const USAGE: &str = "usage: host [-t type] name";

/// The command word this bundle is named by, for the own-help lookup.
const OWN_WORD: &str = "host";

/// The resolver seam: resolve one `(name, record type)` pair.
///
/// Keeping resolution behind this object-safe trait lets the render engine
/// run against a scripted in-memory resolver with no kernel and no network.
/// The production implementation (`src/run.rs`) drives the socket-backed
/// [`tairix_resolver`] client; host tests drive a fake.
pub trait Resolver {
    /// Resolve `name`/`record_type`, returning the concluded
    /// [`Resolution`] (success, negative, or timeout) or a hard
    /// [`ResolveError`] that prevents the lookup.
    ///
    /// # Errors
    ///
    /// A [`ResolveError`] when the lookup could not proceed at all (an
    /// invalid name, no configured server, a failed server-set query, or a
    /// transport failure). A negative or timed-out lookup is **not** an
    /// error; it is a [`Resolution`].
    fn resolve(&mut self, name: &str, record_type: RecordType) -> Result<Resolution, ResolveError>;
}

/// The text output seam: standard output for answers, standard error for
/// diagnostics.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// A parsed `host` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `host`'s own short help (`-?`/`--help`/`-h`).
    Help,
    /// Look the name up.
    Lookup(Lookup),
}

/// A resolved lookup request: the name and the ordered record types to query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lookup {
    /// The name to resolve.
    pub name: String,
    /// The record types to query, in order. With no `-t` this is `A` then
    /// `AAAA`; with `-t <type>` it is the single requested type.
    pub types: Vec<RecordType>,
}

/// Why an argument vector was not understood.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// No name operand was given.
    MissingName,
    /// More than one name operand was given.
    TooManyOperands,
    /// An unrecognised option.
    UnknownOption(String),
    /// A `-t`/`--type` value that is not a supported record type.
    UnknownType(String),
    /// A `-t`/`--type` with no value following it.
    MissingTypeValue,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingName => f.write_str("a name to look up is required"),
            Self::TooManyOperands => f.write_str("only one name may be given"),
            Self::UnknownOption(opt) => write!(f, "unknown option '{opt}'"),
            Self::UnknownType(ty) => {
                write!(
                    f,
                    "unsupported record type '{ty}' (only A and AAAA are supported)"
                )
            }
            Self::MissingTypeValue => f.write_str("option '-t' requires a record type"),
        }
    }
}

/// Parse the record type named by `-t`/`--type`, case-insensitively. Only the
/// `A` and `AAAA` records the stub resolver can look up are accepted; any
/// other type (`MX`, `TXT`, …) is rejected honestly rather than silently
/// treated as `A`.
fn parse_record_type(value: &str) -> Option<RecordType> {
    match value.to_ascii_uppercase().as_str() {
        "A" => Some(RecordType::A),
        "AAAA" => Some(RecordType::Aaaa),
        _ => None,
    }
}

/// Parse a `host` argument vector.
///
/// Options precede the single name operand; `--` ends option processing.
/// `-t`/`--type` takes a record type, attached (`-tA`, `--type=A`) or as the
/// following argument (`-t A`, `--type A`). `-?`, `-h`, and `--help` request
/// the short help and win over any operand.
///
/// # Errors
///
/// A [`ParseError`] describing the first malformed argument.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut record_type: Option<RecordType> = None;
    let mut name: Option<String> = None;
    let mut options_ended = false;
    let mut want_help = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_ended || !arg.starts_with('-') || arg == "-" {
            // A bare `-` is an operand (a name), not an option.
            if name.is_some() {
                return Err(ParseError::TooManyOperands);
            }
            name = Some(arg.to_string());
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if arg == "-?" || arg == "-h" || arg == "--help" {
            want_help = true;
            continue;
        }
        if let Some(value) = type_option_value(arg) {
            // The value was attached (`-tA`, `--type=A`).
            let ty = parse_record_type(value)
                .ok_or_else(|| ParseError::UnknownType(value.to_string()))?;
            record_type = Some(ty);
            continue;
        }
        if arg == "-t" || arg == "--type" {
            let value = args.get(index).ok_or(ParseError::MissingTypeValue)?;
            index += 1;
            let ty = parse_record_type(value)
                .ok_or_else(|| ParseError::UnknownType((*value).to_string()))?;
            record_type = Some(ty);
            continue;
        }
        return Err(ParseError::UnknownOption(arg.to_string()));
    }

    if want_help {
        return Ok(Command::Help);
    }
    let name = name.ok_or(ParseError::MissingName)?;
    let types = match record_type {
        Some(ty) => alloc::vec![ty],
        None => alloc::vec![RecordType::A, RecordType::Aaaa],
    };
    Ok(Command::Lookup(Lookup { name, types }))
}

/// Extract an attached `-t`/`--type` value: `-tA` → `A`, `--type=A` → `A`.
/// Returns [`None`] for the separate-argument forms (`-t`, `--type`) and for
/// any other argument.
fn type_option_value(arg: &str) -> Option<&str> {
    if let Some(rest) = arg.strip_prefix("--type=") {
        return Some(rest);
    }
    if let Some(rest) = arg.strip_prefix("-t") {
        if !rest.is_empty() {
            return Some(rest);
        }
    }
    None
}

/// Run a parsed `host` command against the injected seams.
///
/// Returns `Ok(true)` when at least one address was found (or the short help
/// was written) and `Ok(false)` when the name resolved to no address (a
/// negative answer, a timeout, or a hard resolver failure) — matching
/// `bind-utils` `host`, whose exit status distinguishes "found" from
/// "not found". Diagnostics for a negative or failed lookup are written to
/// `err`; the answers go to `out`.
///
/// # Errors
///
/// [`Errno`] only when writing to `out` or `err` failed — a resolver failure
/// is reported as a diagnostic and folded into `Ok(false)`, never propagated
/// as an output error.
pub fn run(
    command: Command,
    locale: Option<&str>,
    resolver: &mut dyn Resolver,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, Errno> {
    let lookup = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes)?;
            return Ok(true);
        }
        Command::Lookup(lookup) => lookup,
    };

    let mut found = false;
    for record_type in &lookup.types {
        match resolver.resolve(&lookup.name, *record_type) {
            Ok(resolution) => {
                if render_resolution(&lookup.name, *record_type, &resolution, out, err)? {
                    found = true;
                }
                // A definitive "name does not exist" or an unreachable server
                // is the same verdict for every record type, so report it
                // once and stop rather than repeating it per type.
                if matches!(
                    resolution.status,
                    ResolveStatus::NonExistent | ResolveStatus::Timeout
                ) {
                    break;
                }
            }
            Err(error) => {
                // A hard failure (invalid name, no server, transport error)
                // aborts the whole lookup; it is the same for every type.
                render_error(&lookup.name, error, err)?;
                return Ok(false);
            }
        }
    }
    Ok(found)
}

/// Render one record type's [`Resolution`] for `name`, returning whether it
/// yielded at least one address.
fn render_resolution(
    name: &str,
    record_type: RecordType,
    resolution: &Resolution,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<bool, Errno> {
    match resolution.status {
        ResolveStatus::Success => {
            for addr in resolution.addresses.as_slice() {
                let line = match record_type {
                    RecordType::A => format!("{name} has address {addr}\n"),
                    RecordType::Aaaa => format!("{name} has IPv6 address {addr}\n"),
                };
                out.write_all(line.as_bytes())?;
            }
            Ok(!resolution.addresses.is_empty())
        }
        ResolveStatus::NoData => {
            let line = format!("{name} has no {} record\n", type_label(record_type));
            out.write_all(line.as_bytes())?;
            Ok(false)
        }
        ResolveStatus::NonExistent => {
            out.write_all(format!("Host {name} not found: 3(NXDOMAIN)\n").as_bytes())?;
            Ok(false)
        }
        ResolveStatus::Timeout => {
            err.write_all(b";; connection timed out; no servers could be reached\n")?;
            Ok(false)
        }
    }
}

/// Render a hard resolver failure as a diagnostic on standard error.
fn render_error(name: &str, error: ResolveError, err: &dyn Output) -> Result<(), Errno> {
    let line = match error {
        ResolveError::InvalidName(_) => format!("host: '{name}' is not a valid domain name\n"),
        other => format!("host: {other}\n"),
    };
    err.write_all(line.as_bytes())
}

/// The wire spelling of a record type for the `has no <TYPE> record` line.
fn type_label(record_type: RecordType) -> &'static str {
    match record_type {
        RecordType::A => "A",
        RecordType::Aaaa => "AAAA",
    }
}

#[cfg(test)]
mod tests;
