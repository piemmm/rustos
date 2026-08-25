//! Host tests for the `host` parse-and-render engine.
//!
//! The resolver, help, and output seams are all fakes, so the whole engine
//! runs with no kernel and no network: the DNS engine and the socket client
//! are covered by `lib/net` and `lib/resolver`; here we prove the argument
//! grammar and the `bind-utils`-shaped rendering.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::Errno;
use tairix_help::{HelpSource, SourceError};
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::dns::{AddrList, Answer, Name, RecordType, Resolution, ResolveStatus};
use tairix_resolver::ResolveError;

use super::{parse, run, Command, Lookup, Output, ParseError, Resolver, USAGE};

// -- Fakes ---------------------------------------------------------------

/// A scripted resolver: answers each `(record type)` from a fixed script and
/// records the types it was asked to resolve, in order.
struct ScriptResolver {
    a: Option<Result<Resolution, ResolveError>>,
    aaaa: Option<Result<Resolution, ResolveError>>,
    ptr: Option<Result<Resolution, ResolveError>>,
    queried: RefCell<Vec<RecordType>>,
    names: RefCell<Vec<String>>,
}

impl ScriptResolver {
    fn new() -> Self {
        Self {
            a: None,
            aaaa: None,
            ptr: None,
            queried: RefCell::new(Vec::new()),
            names: RefCell::new(Vec::new()),
        }
    }

    fn with_a(mut self, result: &Result<Resolution, ResolveError>) -> Self {
        self.a = Some(*result);
        self
    }

    fn with_aaaa(mut self, result: &Result<Resolution, ResolveError>) -> Self {
        self.aaaa = Some(*result);
        self
    }

    fn with_ptr(mut self, result: &Result<Resolution, ResolveError>) -> Self {
        self.ptr = Some(*result);
        self
    }
}

impl Resolver for ScriptResolver {
    fn resolve(&mut self, name: &str, record_type: RecordType) -> Result<Resolution, ResolveError> {
        self.queried.borrow_mut().push(record_type);
        self.names.borrow_mut().push(name.to_string());
        let scripted = match record_type {
            RecordType::A => self.a,
            RecordType::Aaaa => self.aaaa,
            RecordType::Ptr => self.ptr,
        };
        scripted.unwrap_or(Ok(timeout()))
    }
}

/// A capturing text sink.
#[derive(Default)]
struct Capture {
    text: RefCell<String>,
}

impl Output for Capture {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.text
            .borrow_mut()
            .push_str(core::str::from_utf8(bytes).expect("utf-8"));
        Ok(())
    }
}

/// A help source with no documents, so the short help falls back to the
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

// -- Resolution builders -------------------------------------------------

fn success(addrs: &[IpAddr]) -> Resolution {
    Resolution {
        status: ResolveStatus::Success,
        answer: Answer::Addresses(AddrList::from_addrs(addrs)),
        ttl_secs: 300,
    }
}

/// A successful reverse resolution pointing at `name`.
fn pointer(name: &str) -> Resolution {
    Resolution {
        status: ResolveStatus::Success,
        answer: Answer::Pointer(Some(Name::encode(name).expect("a valid name"))),
        ttl_secs: 300,
    }
}

fn nodata() -> Resolution {
    Resolution {
        status: ResolveStatus::NoData,
        answer: Answer::Addresses(AddrList::default()),
        ttl_secs: 0,
    }
}

fn nxdomain() -> Resolution {
    Resolution {
        status: ResolveStatus::NonExistent,
        answer: Answer::Addresses(AddrList::default()),
        ttl_secs: 0,
    }
}

fn timeout() -> Resolution {
    Resolution {
        status: ResolveStatus::Timeout,
        answer: Answer::Addresses(AddrList::default()),
        ttl_secs: 0,
    }
}

fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(a, b, c, d))
}

fn run_lookup(resolver: &mut ScriptResolver, args: &[&str]) -> (bool, String, String) {
    let command = parse(args).expect("parse");
    let out = Capture::default();
    let err = Capture::default();
    let found = run(command, None, resolver, &NoHelp, &out, &err).expect("no output error");
    (found, out.text.into_inner(), err.text.into_inner())
}

// -- parse ---------------------------------------------------------------

#[test]
fn no_type_queries_a_then_aaaa() {
    let command = parse(&["example.com"]).expect("parse");
    assert_eq!(
        command,
        Command::Lookup(Lookup {
            name: "example.com".to_string(),
            types: alloc::vec![RecordType::A, RecordType::Aaaa],
        })
    );
}

#[test]
fn type_flag_restricts_to_one_type() {
    for args in [
        alloc::vec!["-t", "AAAA", "example.com"],
        alloc::vec!["-tAAAA", "example.com"],
        alloc::vec!["--type=aaaa", "example.com"],
        alloc::vec!["--type", "AaAa", "example.com"],
    ] {
        let command = parse(&args).expect("parse");
        assert_eq!(
            command,
            Command::Lookup(Lookup {
                name: "example.com".to_string(),
                types: alloc::vec![RecordType::Aaaa],
            }),
            "args {args:?}"
        );
    }
}

#[test]
fn help_switches_win() {
    for flag in ["-?", "-h", "--help"] {
        assert_eq!(parse(&[flag]).expect("parse"), Command::Help);
        // Even with a name, help wins.
        assert_eq!(parse(&[flag, "example.com"]).expect("parse"), Command::Help);
    }
}

#[test]
fn a_double_dash_ends_options() {
    // `-t` after `--` is an operand, not the type option.
    let command = parse(&["--", "-t"]).expect("parse");
    assert_eq!(
        command,
        Command::Lookup(Lookup {
            name: "-t".to_string(),
            types: alloc::vec![RecordType::A, RecordType::Aaaa],
        })
    );
}

#[test]
fn parse_errors_are_precise() {
    assert_eq!(parse(&[]), Err(ParseError::MissingName));
    assert_eq!(parse(&["a.com", "b.com"]), Err(ParseError::TooManyOperands));
    assert_eq!(
        parse(&["-z", "a.com"]),
        Err(ParseError::UnknownOption("-z".to_string()))
    );
    assert_eq!(
        parse(&["-t", "MX", "a.com"]),
        Err(ParseError::UnknownType("MX".to_string()))
    );
    assert_eq!(parse(&["-t"]), Err(ParseError::MissingTypeValue));
}

// -- run -----------------------------------------------------------------

#[test]
fn a_success_prints_the_address_line() {
    let mut resolver = ScriptResolver::new()
        .with_a(&Ok(success(&[v4(93, 184, 216, 34)])))
        .with_aaaa(&Ok(nodata()));
    let (found, out, err) = run_lookup(&mut resolver, &["example.com"]);
    assert!(found);
    assert!(out.contains("example.com has address 93.184.216.34"));
    assert!(out.contains("example.com has no AAAA record"));
    assert!(err.is_empty());
    // Both types were queried, A before AAAA.
    assert_eq!(
        resolver.queried.borrow().as_slice(),
        &[RecordType::A, RecordType::Aaaa]
    );
}

#[test]
fn an_aaaa_success_uses_the_ipv6_phrasing() {
    let addr = IpAddr::V6(Ipv6Addr::new(0x2606, 0x2800, 0x220, 0, 0, 0, 0, 0x1));
    let mut resolver = ScriptResolver::new().with_aaaa(&Ok(success(&[addr])));
    let (found, out, _err) = run_lookup(&mut resolver, &["-t", "AAAA", "example.com"]);
    assert!(found);
    assert!(out.contains("example.com has IPv6 address 2606:2800:220::1"));
    assert_eq!(resolver.queried.borrow().as_slice(), &[RecordType::Aaaa]);
}

#[test]
fn nxdomain_is_reported_once_and_stops_further_queries() {
    let mut resolver = ScriptResolver::new()
        .with_a(&Ok(nxdomain()))
        .with_aaaa(&Ok(success(&[v4(1, 2, 3, 4)])));
    let (found, out, _err) = run_lookup(&mut resolver, &["nope.invalid"]);
    assert!(!found);
    assert!(out.contains("Host nope.invalid not found: 3(NXDOMAIN)"));
    // AAAA was never queried — NXDOMAIN is definitive for the name.
    assert_eq!(resolver.queried.borrow().as_slice(), &[RecordType::A]);
}

#[test]
fn a_timeout_goes_to_stderr_and_stops() {
    let mut resolver = ScriptResolver::new().with_a(&Ok(timeout()));
    let (found, out, err) = run_lookup(&mut resolver, &["slow.example"]);
    assert!(!found);
    assert!(out.is_empty(), "no answer on stdout");
    assert!(err.contains("connection timed out"));
    assert_eq!(resolver.queried.borrow().as_slice(), &[RecordType::A]);
}

#[test]
fn no_configured_server_is_a_stderr_diagnostic() {
    let mut resolver = ScriptResolver::new().with_a(&Err(ResolveError::NoServers));
    let (found, out, err) = run_lookup(&mut resolver, &["example.com"]);
    assert!(!found);
    assert!(out.is_empty());
    assert!(err.contains("no DNS server is configured"));
    // The hard failure aborts before AAAA is attempted.
    assert_eq!(resolver.queried.borrow().as_slice(), &[RecordType::A]);
}

#[test]
fn help_falls_back_to_usage() {
    let mut resolver = ScriptResolver::new();
    let out = Capture::default();
    let err = Capture::default();
    let found = run(Command::Help, None, &mut resolver, &NoHelp, &out, &err).expect("ok");
    assert!(found);
    assert!(out.text.into_inner().contains(USAGE));
    assert!(
        resolver.queried.borrow().is_empty(),
        "help touches no resolver"
    );
}

// -- Reverse lookup ------------------------------------------------------

#[test]
fn an_ipv4_operand_becomes_a_ptr_lookup_of_its_reverse_name() {
    let command = parse(&["192.0.2.133"]).expect("parse");
    assert_eq!(
        command,
        Command::Lookup(Lookup {
            name: "133.2.0.192.in-addr.arpa".to_string(),
            types: alloc::vec![RecordType::Ptr],
        })
    );
}

#[test]
fn an_ipv6_operand_becomes_a_ptr_lookup_of_its_reverse_name() {
    let command = parse(&["2001:db8::1"]).expect("parse");
    let expected = "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.\
                    0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa";
    assert_eq!(
        command,
        Command::Lookup(Lookup {
            name: expected.to_string(),
            types: alloc::vec![RecordType::Ptr],
        })
    );
}

#[test]
fn an_explicit_type_still_applies_to_an_address_operand() {
    // `bind-utils` rewrites the operand to its reverse name and queries the
    // requested type against it, so `-t A <address>` is a well-defined (if
    // unusual) question rather than a usage error.
    let command = parse(&["-t", "A", "10.0.2.2"]).expect("parse");
    assert_eq!(
        command,
        Command::Lookup(Lookup {
            name: "2.2.0.10.in-addr.arpa".to_string(),
            types: alloc::vec![RecordType::A],
        })
    );
}

#[test]
fn ptr_is_an_accepted_type_name() {
    let command = parse(&["-t", "ptr", "1.2.0.192.in-addr.arpa"]).expect("parse");
    assert_eq!(
        command,
        Command::Lookup(Lookup {
            name: "1.2.0.192.in-addr.arpa".to_string(),
            types: alloc::vec![RecordType::Ptr],
        })
    );
}

#[test]
fn a_found_pointer_prints_the_bind_utils_line() {
    let mut resolver = ScriptResolver::new().with_ptr(&Ok(pointer("gateway.example")));
    let (found, out, err) = run_lookup(&mut resolver, &["10.0.2.2"]);
    assert!(found);
    assert_eq!(
        out,
        "2.2.0.10.in-addr.arpa domain name pointer gateway.example.\n"
    );
    assert!(err.is_empty());
    assert_eq!(
        resolver.names.borrow().as_slice(),
        &["2.2.0.10.in-addr.arpa".to_string()],
        "the reverse name is what gets queried"
    );
    assert_eq!(resolver.queried.borrow().as_slice(), &[RecordType::Ptr]);
}

#[test]
fn an_address_with_no_pointer_reports_no_ptr_record() {
    let mut resolver = ScriptResolver::new().with_ptr(&Ok(nodata()));
    let (found, out, _err) = run_lookup(&mut resolver, &["10.0.2.2"]);
    assert!(!found);
    assert_eq!(out, "2.2.0.10.in-addr.arpa has no PTR record\n");
}

#[test]
fn an_unknown_reverse_name_reports_nxdomain_against_the_reverse_name() {
    let mut resolver = ScriptResolver::new().with_ptr(&Ok(nxdomain()));
    let (found, out, _err) = run_lookup(&mut resolver, &["10.0.2.2"]);
    assert!(!found);
    assert_eq!(out, "Host 2.2.0.10.in-addr.arpa not found: 3(NXDOMAIN)\n");
}
