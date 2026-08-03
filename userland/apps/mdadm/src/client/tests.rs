//! Engine tests: each command run end to end against in-memory fake seams —
//! the reads, the mutations, the rendered report, the fd-3 advisories, and the
//! fail-closed error mapping (denied read, denied mutation, composer refusal).

extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::raid::{ArrayHealth, RaidLevel};
use tairix_abi::raid_admin::{
    encode_create_reply, RaidArrayRecord, RaidControlOp, RaidMemberDisposition, RaidMemberRecord,
    RAID_SLOT_NONE,
};
use tairix_abi::reply::encode_status_reply;
use tairix_abi::Errno;
use tairix_help::{HelpSource, SourceError};

use super::{run, USAGE};
use crate::command::parse;
use crate::error::MdadmError;

// --- Fixtures -----------------------------------------------------------

fn id_from(prefix: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..prefix.len()].copy_from_slice(prefix);
    out
}

fn array(
    id: [u8; 16],
    health: ArrayHealth,
    member_count: u16,
    active_members: u16,
) -> RaidArrayRecord {
    RaidArrayRecord::new(
        id,
        RaidLevel::Parity,
        health,
        0,
        member_count,
        active_members,
        512,
        128,
        1_048_576,
        7,
        12,
        1_048_576,
        1_048_576,
        5,
    )
}

fn blank(node: u32) -> RaidMemberRecord {
    RaidMemberRecord::new(
        [0u8; 16],
        RaidMemberDisposition::Candidate,
        RAID_SLOT_NONE,
        node,
        100,
        2_097_152,
        512,
        0,
    )
}

fn member(id: [u8; 16], node: u32, slot: u16) -> RaidMemberRecord {
    RaidMemberRecord::new(
        id,
        RaidMemberDisposition::InSync,
        slot,
        node,
        100,
        1_048_576,
        512,
        5,
    )
}

// --- Fake seams ---------------------------------------------------------

struct FakeReader {
    arrays: Result<Vec<RaidArrayRecord>, Errno>,
    members: Result<Vec<RaidMemberRecord>, Errno>,
}

impl FakeReader {
    fn with_arrays(arrays: Vec<RaidArrayRecord>) -> Self {
        Self {
            arrays: Ok(arrays),
            members: Ok(Vec::new()),
        }
    }
}

impl crate::io::Reader for FakeReader {
    fn arrays(&self) -> Result<Vec<RaidArrayRecord>, Errno> {
        self.arrays.clone()
    }

    fn members(&self) -> Result<Vec<RaidMemberRecord>, Errno> {
        self.members.clone()
    }
}

struct FakeController {
    reply: Result<Vec<u8>, Errno>,
    seen: RefCell<Option<Vec<u8>>>,
}

impl FakeController {
    fn replying(reply: Vec<u8>) -> Self {
        Self {
            reply: Ok(reply),
            seen: RefCell::new(None),
        }
    }

    fn unused() -> Self {
        Self {
            reply: Err(Errno::NotImplemented),
            seen: RefCell::new(None),
        }
    }
}

impl crate::io::Controller for FakeController {
    fn call(&self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        *self.seen.borrow_mut() = Some(request.to_vec());
        match &self.reply {
            Ok(bytes) => {
                let n = bytes.len().min(reply.len());
                reply[..n].copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            Err(errno) => Err(*errno),
        }
    }
}

struct Recorder {
    text: RefCell<String>,
    infos: RefCell<Vec<String>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            text: RefCell::new(String::new()),
            infos: RefCell::new(Vec::new()),
        }
    }

    fn text(&self) -> String {
        self.text.borrow().clone()
    }

    fn info_codes(&self) -> Vec<String> {
        self.infos.borrow().clone()
    }
}

impl crate::io::Output for Recorder {
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
        self.text
            .borrow_mut()
            .push_str(core::str::from_utf8(bytes).expect("output is UTF-8"));
        Ok(())
    }

    fn info(&self, record: &[u8]) {
        self.infos
            .borrow_mut()
            .push(String::from_utf8_lossy(record).into_owned());
    }
}

struct NoHelp;

impl HelpSource for NoHelp {
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
        Ok(Vec::new())
    }

    fn read(&self, _locale: &str, _file: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Ok(None)
    }
}

fn any_info_has(codes: &[String], needle: &str) -> bool {
    codes.iter().any(|c| c.contains(needle))
}

// --- Tests --------------------------------------------------------------

#[test]
fn detail_all_lists_arrays_and_advises_degraded_and_blanks() {
    let optimal = array(id_from(&[0x3f, 0x2a]), ArrayHealth::Optimal, 3, 3);
    let degraded = array(id_from(&[0xb1, 0x77]), ArrayHealth::Degraded, 3, 2);
    let reader = FakeReader {
        arrays: Ok(vec![optimal, degraded]),
        members: Ok(vec![blank(20), blank(21)]),
    };
    let out = Recorder::new();
    run(
        parse(&["--detail"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("detail succeeds");
    let text = out.text();
    assert!(text.contains("3f2a0000000000000000000000000000:"), "{text}");
    assert!(text.contains("b1770000000000000000000000000000:"), "{text}");
    assert!(text.contains("State : degraded"), "{text}");
    let codes = out.info_codes();
    assert!(any_info_has(&codes, "raid.redundancy_reduced"), "{codes:?}");
    assert!(
        any_info_has(&codes, "raid.blank_devices_omitted"),
        "{codes:?}"
    );
}

#[test]
fn detail_of_an_optimal_only_machine_advises_nothing() {
    let reader = FakeReader::with_arrays(vec![array(
        id_from(&[0x3f, 0x2a]),
        ArrayHealth::Optimal,
        3,
        3,
    )]);
    let out = Recorder::new();
    run(
        parse(&["-D"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("detail succeeds");
    assert!(out.info_codes().is_empty(), "{:?}", out.info_codes());
}

#[test]
fn detail_single_resolves_and_reports() {
    let reader = FakeReader::with_arrays(vec![
        array(id_from(&[0x3f, 0x2a]), ArrayHealth::Optimal, 3, 3),
        array(id_from(&[0xb1, 0x77]), ArrayHealth::Degraded, 3, 2),
    ]);
    let out = Recorder::new();
    run(
        parse(&["--detail", "b1"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("detail succeeds");
    let text = out.text();
    assert!(text.contains("b1770000000000000000000000000000:"), "{text}");
    // Only the one array is shown.
    assert!(!text.contains("3f2a"), "{text}");
    assert!(any_info_has(&out.info_codes(), "raid.redundancy_reduced"));
}

#[test]
fn detail_of_an_empty_machine_advises_no_arrays() {
    let reader = FakeReader::with_arrays(Vec::new());
    let out = Recorder::new();
    run(
        parse(&["--detail"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("detail succeeds");
    assert!(out.text().is_empty(), "{}", out.text());
    assert!(any_info_has(&out.info_codes(), "raid.no_arrays"));
}

#[test]
fn examine_lists_devices_and_empty_advises_no_devices() {
    let reader = FakeReader {
        arrays: Ok(Vec::new()),
        members: Ok(vec![member(id_from(&[0x3f, 0x2a]), 20, 0), blank(21)]),
    };
    let out = Recorder::new();
    run(
        parse(&["--examine"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("examine succeeds");
    let text = out.text();
    assert!(text.contains("node:20"), "{text}");
    assert!(text.contains("node:21"), "{text}");
    assert!(text.contains("candidate"), "{text}");

    let empty_reader = FakeReader {
        arrays: Ok(Vec::new()),
        members: Ok(Vec::new()),
    };
    let out = Recorder::new();
    run(
        parse(&["-E"]).unwrap(),
        None,
        &empty_reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("examine succeeds");
    assert!(out.text().is_empty(), "{}", out.text());
    assert!(any_info_has(&out.info_codes(), "raid.no_devices"));
}

#[test]
fn create_encodes_the_request_and_prints_the_minted_identity() {
    let minted = id_from(&[0xca, 0xfe]);
    let controller = FakeController::replying(encode_create_reply(Ok(minted)).to_vec());
    let out = Recorder::new();
    run(
        parse(&["-C", "-l", "raid5", "-n", "3", "node:1", "node:2", "node:3"]).unwrap(),
        None,
        &FakeReader::with_arrays(Vec::new()),
        &controller,
        &NoHelp,
        &out,
    )
    .expect("create succeeds");
    assert!(
        out.text()
            .contains("Created array cafe0000000000000000000000000000"),
        "{}",
        out.text()
    );
    // The wire frame carries the level and the members in slot order.
    let seen = controller
        .seen
        .borrow()
        .clone()
        .expect("a frame was posted");
    match RaidControlOp::decode(&seen).expect("frame decodes") {
        RaidControlOp::Create { level, members, .. } => {
            assert_eq!(level, RaidLevel::Parity);
            assert_eq!(members.as_slice(), &[1, 2, 3]);
        }
        other => panic!("wrong op: {other:?}"),
    }
}

#[test]
fn add_resolves_the_array_and_confirms() {
    let reader = FakeReader::with_arrays(vec![array(
        id_from(&[0x3f, 0x2a]),
        ArrayHealth::Degraded,
        3,
        2,
    )]);
    let controller = FakeController::replying(encode_status_reply(Ok(())).to_vec());
    let out = Recorder::new();
    run(
        parse(&["--add", "3f2a", "node:9"]).unwrap(),
        None,
        &reader,
        &controller,
        &NoHelp,
        &out,
    )
    .expect("add succeeds");
    assert!(out.text().contains("Added node:9"), "{}", out.text());
    let frame = controller
        .seen
        .borrow()
        .clone()
        .expect("a frame was posted");
    match RaidControlOp::decode(&frame).expect("frame decodes") {
        RaidControlOp::Add { node, .. } => assert_eq!(node, 9),
        other => panic!("wrong op: {other:?}"),
    }
}

#[test]
fn stop_confirms_and_a_refusal_is_reported() {
    let reader = FakeReader::with_arrays(vec![array(
        id_from(&[0x3f, 0x2a]),
        ArrayHealth::Optimal,
        3,
        3,
    )]);
    // A composer refusal (e.g. the array is still in use) becomes `Refused`.
    let controller = FakeController::replying(encode_status_reply(Err(Errno::NotFound)).to_vec());
    let out = Recorder::new();
    let result = run(
        parse(&["--stop", "3f2a"]).unwrap(),
        None,
        &reader,
        &controller,
        &NoHelp,
        &out,
    );
    assert_eq!(result, Err(MdadmError::Refused(Errno::NotFound)));
    assert!(out.text().is_empty(), "nothing is confirmed on a refusal");
}

#[test]
fn a_denied_read_is_reported_and_nothing_is_fabricated() {
    let reader = FakeReader {
        arrays: Err(Errno::PermissionDenied),
        members: Ok(Vec::new()),
    };
    let out = Recorder::new();
    let result = run(
        parse(&["--detail"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    );
    assert_eq!(result, Err(MdadmError::ReadDenied));
    assert!(out.text().is_empty());
}

#[test]
fn a_denied_mutation_is_reported() {
    let reader = FakeReader::with_arrays(vec![array(
        id_from(&[0x3f, 0x2a]),
        ArrayHealth::Optimal,
        3,
        3,
    )]);
    let controller =
        FakeController::replying(encode_status_reply(Err(Errno::PermissionDenied)).to_vec());
    let out = Recorder::new();
    let result = run(
        parse(&["--stop", "3f2a"]).unwrap(),
        None,
        &reader,
        &controller,
        &NoHelp,
        &out,
    );
    assert_eq!(result, Err(MdadmError::AdminDenied));
}

#[test]
fn an_unresolved_array_in_a_mutation_is_reported() {
    let reader = FakeReader::with_arrays(vec![array(
        id_from(&[0x3f, 0x2a]),
        ArrayHealth::Optimal,
        3,
        3,
    )]);
    let out = Recorder::new();
    let result = run(
        parse(&["--stop", "dead"]).unwrap(),
        None,
        &reader,
        &FakeController::unused(),
        &NoHelp,
        &out,
    );
    assert!(matches!(result, Err(MdadmError::Resolve(_))), "{result:?}");
}

#[test]
fn help_falls_back_to_the_usage_banner() {
    let out = Recorder::new();
    run(
        parse(&["--help"]).unwrap(),
        None,
        &FakeReader::with_arrays(Vec::new()),
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("help renders");
    assert_eq!(out.text(), std::format!("{USAGE}\n"));
}

#[test]
fn version_prints_the_version_line() {
    let out = Recorder::new();
    run(
        parse(&["--version"]).unwrap(),
        None,
        &FakeReader::with_arrays(Vec::new()),
        &FakeController::unused(),
        &NoHelp,
        &out,
    )
    .expect("version renders");
    assert!(out.text().contains("0.1.0"), "{}", out.text());
}
