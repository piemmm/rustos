//! Host tests for `dns-sd`'s parse-and-render engine.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::discovery_ipc::{Answer, Change, Entry, Families, Query};
use tairix_abi::net_ipc::IF_NAME_LEN;
use tairix_abi::Errno;
use tairix_discovery::{DiscoveryError, Waited};
use tairix_help::{HelpSource, SourceError};
use tairix_net::dns::Name;

use super::{parse, run, Command, Discovery, Output, ParseError, Request};

fn iface(name: &[u8]) -> [u8; IF_NAME_LEN] {
    let mut out = [0u8; IF_NAME_LEN];
    out[..name.len()].copy_from_slice(name);
    out
}

/// The request a parse yielded, and its seconds.
fn asked(args: &[&str]) -> (Request, Option<u32>) {
    match parse(args) {
        Ok(Command::Ask { request, seconds }) => (*request, seconds),
        other => panic!("{args:?} parsed to {other:?}"),
    }
}

#[test]
fn each_mode_parses_to_its_request() {
    assert_eq!(asked(&["-B"]), (Request::Types, None));
    assert_eq!(
        asked(&["-B", "_services._dns-sd._udp", "local."]).0,
        Request::Types
    );
    let (Request::Browse(service), seconds) = asked(&["-t", "5", "-B", "_ipp._tcp"]) else {
        panic!("a browse");
    };
    assert_eq!(alloc::format!("{service}"), "_ipp._tcp");
    assert_eq!(seconds, Some(5));
    assert!(matches!(
        asked(&["-L", "Hall Printer", "_ipp._tcp", "local"]).0,
        Request::Resolve { .. }
    ));
    assert!(matches!(
        asked(&["-G", "v4v6", "printer.local"]).0,
        Request::Host {
            families: Families::BOTH,
            ..
        }
    ));
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
}

#[test]
fn what_multicast_dns_does_not_answer_is_refused_at_the_command_line() {
    for (args, error) in [
        (
            &["-B", "ipp._tcp"][..],
            ParseError::BadType(String::from("ipp._tcp")),
        ),
        (
            &["-B", "_ipp._tcp", "example.com"][..],
            ParseError::BadDomain(String::from("example.com")),
        ),
        (
            &["-G", "v4", "printer.example.com"][..],
            ParseError::BadHost(String::from("printer.example.com")),
        ),
        (
            &["-G", "v5", "printer.local"][..],
            ParseError::BadFamily(String::from("v5")),
        ),
        (
            &["-t", "soon", "-B"][..],
            ParseError::BadSeconds(String::from("soon")),
        ),
        (&["-B", "-L"][..], ParseError::NoMode),
        (&[][..], ParseError::NoMode),
        (&["-L", "only-one"][..], ParseError::Operands),
        (&["-x"][..], ParseError::UnknownOption(String::from("-x"))),
    ] {
        assert_eq!(parse(args), Err(error), "{args:?}");
    }
}

/// A scripted service: every wait hands over the next batch, then times out.
struct Scripted {
    started: RefCell<Vec<String>>,
    batches: Vec<Vec<Entry<'static>>>,
    refuse: Option<DiscoveryError>,
}

impl Discovery for Scripted {
    fn start(&mut self, query: &Query<'_>) -> Result<u32, DiscoveryError> {
        self.started.borrow_mut().push(alloc::format!("{query:?}"));
        self.refuse.map_or(Ok(3), Err)
    }

    fn wait(
        &mut self,
        _until: Option<u64>,
        each: &mut dyn FnMut(&Entry<'_>),
    ) -> Result<Waited, DiscoveryError> {
        if self.batches.is_empty() {
            return Ok(Waited::TimedOut);
        }
        for entry in self.batches.remove(0) {
            each(&entry);
        }
        Ok(Waited::Rung)
    }

    fn now(&self) -> u64 {
        0
    }
}

#[derive(Default)]
struct Captured(RefCell<Vec<u8>>);

impl Output for Captured {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).expect("text")
    }
}

struct NoHelp;

impl HelpSource for NoHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn read(&self, _locale_dir: &str, _file_name: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(None)
    }
}

fn instance(change: Change, label: &'static [u8]) -> Entry<'static> {
    Entry::Answer {
        request: 3,
        interface: iface(b"eth0"),
        change,
        ttl: 4500,
        answer: Answer::Instance { label },
    }
}

fn run_with(args: &[&str], batches: Vec<Vec<Entry<'static>>>) -> (i32, String, String) {
    let command = parse(args).expect("parses");
    let mut discovery = Scripted {
        started: RefCell::new(Vec::new()),
        batches,
        refuse: None,
    };
    let (out, err) = (Captured::default(), Captured::default());
    let status = run(command, None, &mut discovery, &NoHelp, &out, &err).expect("writes");
    (status, out.text(), err.text())
}

#[test]
fn a_browse_prints_each_instance_as_it_moves_escaped() {
    let (status, out, err) = run_with(
        &["-t", "1", "-B", "_ipp._tcp"],
        alloc::vec![
            alloc::vec![instance(Change::Added, b"Hall Printer")],
            alloc::vec![
                instance(Change::Refreshed, b"Hall Printer"),
                instance(Change::Added, b"\x1b[2Jevil"),
                instance(Change::Retired, b"Hall Printer"),
            ],
        ],
    );
    assert_eq!(status, 0);
    assert!(err.is_empty());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Browsing for _ipp._tcp");
    assert!(lines[2].starts_with("Add    eth0"));
    assert!(lines[2].ends_with("Hall\\032Printer"));
    assert!(
        lines[3].ends_with("\\027[2Jevil"),
        "a control byte never reaches the terminal"
    );
    assert!(lines[4].starts_with("Rmv"));
    assert_eq!(lines.len(), 5, "a renewal prints nothing");
    assert!(!out.contains('\x1b'));
}

#[test]
fn a_host_lookup_prints_each_address_and_a_flush_says_what_it_voids() {
    let (_, out, _) = run_with(
        &["-t", "1", "-G", "v4", "printer.local"],
        alloc::vec![alloc::vec![
            Entry::Answer {
                request: 3,
                interface: iface(b"wlan0"),
                change: Change::Added,
                ttl: 120,
                answer: Answer::Address {
                    address: core::net::IpAddr::V4(core::net::Ipv4Addr::new(169, 254, 3, 4)),
                },
            },
            Entry::Flush {
                request: 3,
                interface: iface(b"wlan0"),
            },
            Entry::Lost { request: 3 },
        ]],
    );
    assert!(out.contains("Looking up printer.local"));
    assert!(out.contains("Add    wlan0            169.254.3.4"));
    assert!(out.contains("Flush  wlan0"));
    assert!(out.contains("Lost"));
}

#[test]
fn a_resolve_prints_where_and_every_txt_string() {
    let target = Name::encode("printer.local").unwrap();
    let wire: &'static [u8] = alloc::boxed::Box::leak(target.as_wire().to_vec().into_boxed_slice());
    let (_, out, _) = run_with(
        &["-t", "1", "-L", "Hall Printer", "_ipp._tcp"],
        alloc::vec![alloc::vec![
            Entry::Answer {
                request: 3,
                interface: iface(b"eth0"),
                change: Change::Added,
                ttl: 120,
                answer: Answer::Service {
                    priority: 0,
                    weight: 0,
                    port: 631,
                    target: wire,
                },
            },
            Entry::Answer {
                request: 3,
                interface: iface(b"eth0"),
                change: Change::Added,
                ttl: 4500,
                answer: Answer::Text {
                    octets: b"\x09txtvers=1\x0bnote=Hall 2",
                },
            },
        ]],
    );
    assert!(out.contains("Hall\\032Printer._ipp._tcp.local. can be reached at printer.local.:631"));
    assert!(out.contains("TXT txtvers=1 note=Hall\\0322"));
}

#[test]
fn a_refusal_names_what_the_caller_lacked() {
    let command = parse(&["-B"]).expect("parses");
    let mut discovery = Scripted {
        started: RefCell::new(Vec::new()),
        batches: Vec::new(),
        refuse: Some(DiscoveryError::Refused(Errno::PermissionDenied)),
    };
    let (out, err) = (Captured::default(), Captured::default());
    assert_eq!(
        run(command, None, &mut discovery, &NoHelp, &out, &err),
        Ok(1)
    );
    assert_eq!(
        err.text(),
        "dns-sd: enumerating every type needs CAP_NET_DISCOVER_ALL\n"
    );
    let command = parse(&["-G", "v6", "printer.local"]).expect("parses");
    let mut absent = Scripted {
        started: RefCell::new(Vec::new()),
        batches: Vec::new(),
        refuse: Some(DiscoveryError::Unavailable),
    };
    let err = Captured::default();
    assert_eq!(
        run(
            command,
            None,
            &mut absent,
            &NoHelp,
            &Captured::default(),
            &err
        ),
        Ok(1)
    );
    assert_eq!(err.text(), "dns-sd: link-local discovery is not running\n");
}
