//! TAIRiX `dns-sd` — browse, resolve, and look up link-local services
//! (`plans/ZEROCONF.md` Z4, a `plans/APPS.md` command app).
//!
//! The command-line shape is the one the `dns-sd` tool established: `-B` to
//! browse a type (or, with no type, every type), `-L` to resolve an instance,
//! `-G` to look up a host, each running until interrupted or, with `-t`, for a
//! bounded number of seconds. Every answer is printed as it moves — added,
//! removed, or voided with its link — and names its interface.
//!
//! # What this crate is
//!
//! A parse-and-render engine over the injected [`Discovery`] seam, the
//! [`Output`] streams, and the bundle's own help source, so it is host-tested
//! with no kernel. Every name printed was authored by an unauthenticated peer,
//! so each is printed in RFC 1035 presentation form ([`Presentation`]) and
//! never as it arrived.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); no `unsafe`, and no `unwrap`/`expect`/`panic!` on a
//! production path.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::discovery_ipc::{Answer, Change, Entry, Families, Query, ServiceTypeField};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::Errno;
use tairix_discovery::{DiscoveryError, Waited};
use tairix_help::{own_short_help, HelpSource};
use tairix_net::dns::{Name, Presentation};
use tairix_net::dnssd::{InstanceName, ServiceType, Transport};
use tairix_net::mdns::{is_link_local_name, LOCAL_LABEL};

/// The one-line usage banner.
pub const USAGE: &str =
    "usage: dns-sd [-t seconds] -B [type] | -L instance type | -G v4|v6|v4v6 host";

/// The command word this bundle is named by.
const OWN_WORD: &str = "dns-sd";

/// The type whose instances are every type (RFC 6763 §9).
const SERVICES_TYPE: &str = "_services._dns-sd._udp";

/// A running conversation with the discovery service.
pub trait Discovery {
    /// Start `query`, returning the request's id.
    ///
    /// # Errors
    ///
    /// The service's refusal.
    fn start(&mut self, query: &Query<'_>) -> Result<u32, DiscoveryError>;

    /// Wait until the service rings or the monotonic instant `until` passes,
    /// handing each entry taken to `each`.
    ///
    /// # Errors
    ///
    /// The service's refusal of the collect, or a failed wait.
    fn wait(
        &mut self,
        until: Option<u64>,
        each: &mut dyn FnMut(&Entry<'_>),
    ) -> Result<Waited, DiscoveryError>;

    /// The monotonic clock, in nanoseconds.
    fn now(&self) -> u64;
}

/// A text stream.
pub trait Output {
    /// Write every byte of `bytes`.
    ///
    /// # Errors
    ///
    /// The stream's refusal.
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// What is asked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Every service type the segment offers.
    Types,
    /// The instances of one type.
    Browse(ServiceType),
    /// Where one instance is reached, and what it says of itself.
    Resolve {
        /// The instance's own label.
        instance: InstanceName,
        /// Its type.
        service: ServiceType,
    },
    /// A host's addresses.
    Host {
        /// The host, under `local`.
        name: Name,
        /// The families asked for.
        families: Families,
    },
}

/// A parsed invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render the short help.
    Help,
    /// Ask, for `seconds` or until interrupted.
    Ask {
        /// What is asked.
        request: Box<Request>,
        /// How long to run, or `None` for until interrupted.
        seconds: Option<u32>,
    },
}

/// Why an argument vector was not understood.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// None of `-B`, `-L`, `-G` was given, or more than one.
    NoMode,
    /// An unrecognised option.
    UnknownOption(String),
    /// A mode was given the wrong number of operands.
    Operands,
    /// A service type outside `_name._tcp` / `_name._udp`.
    BadType(String),
    /// An instance name outside RFC 6763's grammar.
    BadInstance,
    /// A host that is not a name under `local`.
    BadHost(String),
    /// A `-G` family other than `v4`, `v6`, or `v4v6`.
    BadFamily(String),
    /// A domain other than `local`, which is all multicast DNS answers for.
    BadDomain(String),
    /// A `-t` that is not a whole number of seconds.
    BadSeconds(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMode => f.write_str("exactly one of -B, -L, or -G is required"),
            Self::UnknownOption(option) => write!(f, "unknown option '{option}'"),
            Self::Operands => f.write_str("wrong number of operands for the mode"),
            Self::BadType(ty) => write!(f, "'{ty}' is not a service type such as _ipp._tcp"),
            Self::BadInstance => f.write_str("not an instance name"),
            Self::BadHost(host) => write!(f, "'{host}' is not a host name under local"),
            Self::BadFamily(family) => write!(f, "'{family}' is not v4, v6, or v4v6"),
            Self::BadDomain(domain) => {
                write!(f, "'{domain}' is not local, the only domain on the link")
            }
            Self::BadSeconds(value) => write!(f, "'{value}' is not a whole number of seconds"),
        }
    }
}

/// Parse a `dns-sd` argument vector.
///
/// # Errors
///
/// A [`ParseError`] describing the first argument not understood.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut mode: Option<char> = None;
    let mut seconds = None;
    let mut operands: Vec<&str> = Vec::new();
    let mut index = 0;
    while let Some(&arg) = args.get(index) {
        index += 1;
        match arg {
            "-?" | "-h" | "--help" => return Ok(Command::Help),
            "-B" | "-L" | "-G" => {
                if mode.is_some() {
                    return Err(ParseError::NoMode);
                }
                mode = arg.chars().nth(1);
            }
            "-t" => {
                let value = args.get(index).ok_or(ParseError::Operands)?;
                index += 1;
                seconds = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| ParseError::BadSeconds(String::from(*value)))?,
                );
            }
            option if option.starts_with('-') && option.len() > 1 => {
                return Err(ParseError::UnknownOption(String::from(option)));
            }
            operand => operands.push(operand),
        }
    }
    let request = match (mode, operands.as_slice()) {
        (Some('B'), []) => Request::Types,
        (Some('B'), [ty] | [ty, _]) if *ty == SERVICES_TYPE => {
            domain(operands.get(1).copied())?;
            Request::Types
        }
        (Some('B'), [ty] | [ty, _]) => {
            domain(operands.get(1).copied())?;
            Request::Browse(service_type(ty)?)
        }
        (Some('L'), [instance, ty] | [instance, ty, _]) => {
            domain(operands.get(2).copied())?;
            Request::Resolve {
                instance: InstanceName::new(instance.as_bytes())
                    .map_err(|_| ParseError::BadInstance)?,
                service: service_type(ty)?,
            }
        }
        (Some('G'), [family, host]) => Request::Host {
            families: match *family {
                "v4" => Families {
                    v4: true,
                    v6: false,
                },
                "v6" => Families {
                    v4: false,
                    v6: true,
                },
                "v4v6" => Families::BOTH,
                other => return Err(ParseError::BadFamily(String::from(other))),
            },
            name: Name::encode(host)
                .ok()
                .filter(is_link_local_name)
                .ok_or_else(|| ParseError::BadHost(String::from(*host)))?,
        },
        (None, _) => return Err(ParseError::NoMode),
        _ => return Err(ParseError::Operands),
    };
    Ok(Command::Ask {
        request: Box::new(request),
        seconds,
    })
}

/// Accept only `local`, with or without its trailing dot.
fn domain(domain: Option<&str>) -> Result<(), ParseError> {
    match domain {
        None => Ok(()),
        Some(given)
            if given
                .strip_suffix('.')
                .unwrap_or(given)
                .as_bytes()
                .eq_ignore_ascii_case(LOCAL_LABEL) =>
        {
            Ok(())
        }
        Some(other) => Err(ParseError::BadDomain(String::from(other))),
    }
}

/// `_name._tcp` or `_name._udp`, a trailing dot allowed.
fn service_type(spelled: &str) -> Result<ServiceType, ParseError> {
    let bad = || ParseError::BadType(String::from(spelled));
    let (name, transport) = spelled
        .strip_suffix('.')
        .unwrap_or(spelled)
        .strip_prefix('_')
        .and_then(|rest| rest.rsplit_once('.'))
        .ok_or_else(bad)?;
    let transport = Transport::from_label(transport.as_bytes()).ok_or_else(bad)?;
    ServiceType::new(name.as_bytes(), transport).map_err(|_| bad())
}

/// Run a parsed command. Returns the exit status: `0` when it ran its course
/// (or wrote its help), `1` when the service refused or is not running.
///
/// # Errors
///
/// The output stream's refusal.
pub fn run(
    command: Command,
    locale: Option<&str>,
    discovery: &mut dyn Discovery,
    help: &dyn HelpSource,
    out: &dyn Output,
    err: &dyn Output,
) -> Result<i32, Errno> {
    let (request, seconds) = match command {
        Command::Help => {
            let bytes = own_short_help(help, locale, OWN_WORD)
                .unwrap_or_else(|| format!("{USAGE}\n").into_bytes());
            out.write_all(&bytes)?;
            return Ok(0);
        }
        Command::Ask { request, seconds } => (*request, seconds),
    };
    let started = discovery.now();
    let until = seconds
        .map(|seconds| started.saturating_add(u64::from(seconds).saturating_mul(1_000_000_000)));
    let query = query_of(&request);
    out.write_all(header(&request).as_bytes())?;
    if let Err(error) = discovery.start(&query) {
        return refused(error, &request, err);
    }
    loop {
        let mut lines = String::new();
        let waited = discovery.wait(until, &mut |entry| render(entry, &request, &mut lines));
        out.write_all(lines.as_bytes())?;
        match waited {
            Ok(Waited::Rung) => {}
            Ok(Waited::TimedOut) => return Ok(0),
            Err(error) => return refused(error, &request, err),
        }
    }
}

fn query_of(request: &Request) -> Query<'_> {
    match request {
        Request::Types => Query::Types,
        Request::Browse(service) => Query::Browse {
            service: field(service),
        },
        Request::Resolve { instance, service } => Query::Resolve {
            instance: instance.as_bytes(),
            service: field(service),
        },
        Request::Host { name, families } => Query::Host {
            name: name.as_wire(),
            families: *families,
        },
    }
}

fn field(service: &ServiceType) -> ServiceTypeField<'_> {
    ServiceTypeField {
        name: service.name(),
        transport: service.transport(),
    }
}

fn header(request: &Request) -> String {
    match request {
        Request::Types => String::from("Browsing for every service type\nA/R    Interface        Service Type\n"),
        Request::Browse(service) => format!(
            "Browsing for {service}\nA/R    Interface        Domain  Service Type          Instance Name\n"
        ),
        Request::Resolve { instance, service } => format!(
            "Lookup {}.{service}.local\n",
            Presentation(instance.as_bytes())
        ),
        Request::Host { name, .. } => {
            format!("Looking up {name}\nA/R    Interface        Address                                   TTL\n")
        }
    }
}

/// One entry as its line.
fn render(entry: &Entry<'_>, request: &Request, lines: &mut String) {
    use core::fmt::Write as _;
    match entry {
        Entry::Answer {
            interface,
            change,
            ttl,
            answer,
            ..
        } => {
            let movement = match change {
                Change::Added => "Add",
                // A renewal moves nothing a reader acts on.
                Change::Refreshed => return,
                Change::Retired => "Rmv",
            };
            let on = interface_name(interface);
            let _ = match (answer, request) {
                (Answer::Instance { label }, Request::Browse(service)) => writeln!(
                    lines,
                    "{movement}    {on:<16} local.  {:<21} {}",
                    format!("{service}."),
                    Presentation(label)
                ),
                (Answer::Type { service }, _) => writeln!(
                    lines,
                    "{movement}    {on:<16} _{}.{}.",
                    Presentation(service.name),
                    service.transport.label()
                ),
                (
                    Answer::Service {
                        priority,
                        weight,
                        port,
                        target,
                    },
                    Request::Resolve { instance, service },
                ) => writeln!(
                    lines,
                    "{movement} {}.{service}.local. can be reached at {}:{port} (interface {on}, priority {priority}, weight {weight})",
                    Presentation(instance.as_bytes()),
                    wire_name(target),
                ),
                (Answer::Text { octets }, _) => {
                    let mut text = String::new();
                    for string in txt_strings(octets) {
                        let _ = write!(text, " {}", Presentation(string));
                    }
                    writeln!(lines, "{movement}   TXT{text} (interface {on})")
                }
                (Answer::Address { address }, _) => {
                    writeln!(lines, "{movement}    {on:<16} {address:<41} {ttl}")
                }
                (Answer::Pointer { target }, _) => {
                    writeln!(lines, "{movement}    {on:<16} {}", wire_name(target))
                }
                // An answer of a shape its request does not ask for.
                _ => Ok(()),
            };
        }
        Entry::Flush { interface, .. } => {
            let _ = writeln!(
                lines,
                "Flush  {:<16} everything learned on it is void",
                interface_name(interface)
            );
        }
        Entry::Lost { .. } => {
            lines.push_str("Lost   updates were dropped; what is shown may be incomplete\n");
        }
    }
}

/// A name the service sent in uncompressed wire form, presented; one that
/// does not decode is shown as the octets it was.
fn wire_name(target: &[u8]) -> String {
    Name::from_wire(target).map_or_else(
        || format!("{}", Presentation(target)),
        |name| format!("{name}."),
    )
}

/// The length-prefixed strings of `TXT` rdata; a string that runs past the
/// end ends the walk.
fn txt_strings(octets: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = octets;
    core::iter::from_fn(move || {
        let (&len, tail) = rest.split_first()?;
        let string = tail.get(..usize::from(len))?;
        rest = &tail[usize::from(len)..];
        Some(string)
    })
}

fn interface_name(interface: &[u8; IF_NAME_LEN]) -> &str {
    let len = interface
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(IF_NAME_LEN);
    core::str::from_utf8(&interface[..len]).unwrap_or("?")
}

/// Report why the request could not go on, returning the exit status.
fn refused(error: DiscoveryError, request: &Request, err: &dyn Output) -> Result<i32, Errno> {
    let reason = match (error, request) {
        (DiscoveryError::Refused(Errno::PermissionDenied), Request::Types) => {
            String::from("enumerating every type needs CAP_NET_DISCOVER_ALL")
        }
        (DiscoveryError::Refused(Errno::PermissionDenied), Request::Browse(service)) => {
            format!("browsing {service} needs a grant for it or CAP_NET_DISCOVER_ALL")
        }
        (other, _) => format!("{other}"),
    };
    err.write_all(format!("dns-sd: {reason}\n").as_bytes())?;
    Ok(1)
}

#[cfg(test)]
mod tests;
