//! Host tests for the `servicectl` round trip and its rendering.

extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::service_control::{
    encode_error_reply, encode_reply, ServiceControlRequest, REPLY_LEN,
};
use tairix_abi::{Errno, ServiceControlOp, ServiceState};

use crate::command::{parse, Command, UsageError};
use crate::session::{dispatch, report_usage, run, ControlChannel, Exit, ToolIo};

/// A channel double: records the request frame it was given and replays a
/// scripted answer.
struct MockChannel {
    answer: Result<Vec<u8>, i64>,
    seen: Vec<Vec<u8>>,
}

impl MockChannel {
    /// A manager that applies the operation and reports `state`.
    fn applying(state: ServiceState) -> Self {
        let mut reply = [0u8; REPLY_LEN];
        let n = encode_reply(&mut reply, state).expect("encodes");
        Self {
            answer: Ok(reply[..n].to_vec()),
            seen: Vec::new(),
        }
    }

    /// A manager that refuses with `err`.
    fn refusing(err: Errno) -> Self {
        let mut reply = [0u8; REPLY_LEN];
        let n = encode_error_reply(&mut reply, err).expect("encodes");
        Self {
            answer: Ok(reply[..n].to_vec()),
            seen: Vec::new(),
        }
    }

    /// A kernel that refuses the call itself (`-errno`), the shape a caller
    /// without the capability sees.
    fn unreachable(err: Errno) -> Self {
        Self {
            answer: Err(-i64::from(err.as_i32())),
            seen: Vec::new(),
        }
    }
}

impl ControlChannel for MockChannel {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, i64> {
        self.seen.push(request.to_vec());
        match &self.answer {
            Ok(frame) => {
                reply[..frame.len()].copy_from_slice(frame);
                Ok(frame.len())
            }
            Err(err) => Err(*err),
        }
    }
}

/// A stream double recording both channels separately, so a test can assert
/// a diagnosis went to fd 2 rather than polluting fd 1.
#[derive(Default)]
struct MockIo {
    out: Vec<String>,
    err: Vec<String>,
}

impl ToolIo for MockIo {
    fn write_line(&mut self, line: &str) {
        self.out.push(line.to_string());
    }
    fn write_error(&mut self, line: &str) {
        self.err.push(line.to_string());
    }
}

#[test]
fn a_successful_stop_reports_the_resulting_state_on_stdout() {
    let mut channel = MockChannel::applying(ServiceState::Stopping);
    let mut io = MockIo::default();
    let exit = run(&mut channel, &mut io, ServiceControlOp::Stop, "timed");

    assert_eq!(exit, Exit::Ok);
    assert_eq!(exit.code(), 0);
    assert_eq!(io.out, ["stop: timed is now stopping"]);
    assert!(io.err.is_empty(), "a success writes nothing to fd 2");
}

#[test]
fn the_frame_on_the_wire_is_the_operation_and_name_that_were_asked_for() {
    let mut channel = MockChannel::applying(ServiceState::Starting);
    let mut io = MockIo::default();
    run(&mut channel, &mut io, ServiceControlOp::Start, "netstack");

    let sent = channel.seen.first().expect("one request was posted");
    let decoded = ServiceControlRequest::decode(sent).expect("the tool posts a valid frame");
    assert_eq!(decoded.op, ServiceControlOp::Start);
    assert_eq!(decoded.name, "netstack");
}

#[test]
fn each_manager_refusal_is_named_on_stderr_and_fails() {
    for (err, expected) in [
        (Errno::NotFound, "no such service is registered"),
        (Errno::Busy, "the service is not in a state to be started"),
        (Errno::NotSupported, "the service could not be launched"),
    ] {
        let mut channel = MockChannel::refusing(err);
        let mut io = MockIo::default();
        let exit = run(&mut channel, &mut io, ServiceControlOp::Start, "svc");

        assert_eq!(exit, Exit::Failed);
        assert_eq!(exit.code(), 1);
        assert!(io.out.is_empty(), "a refusal writes nothing to fd 1");
        let line = io.err.first().expect("the refusal is stated");
        assert!(line.contains(expected), "{line} should explain {err:?}");
        assert!(line.contains("svc"), "{line} should name the service");
    }
}

#[test]
fn an_errno_with_no_special_wording_still_states_its_own_reason() {
    // The stable ABI already spells every code; an unenumerated one must
    // still produce a diagnosis rather than a bare status.
    let mut channel = MockChannel::refusing(Errno::Interrupted);
    let mut io = MockIo::default();
    let exit = run(&mut channel, &mut io, ServiceControlOp::Stop, "svc");

    assert_eq!(exit, Exit::Failed);
    let line = io.err.first().expect("the refusal is stated");
    assert!(line.contains("svc"), "{line} should name the service");
    assert!(
        line.len() > "servicectl: stop svc: ".len(),
        "{line} is bare"
    );
}

#[test]
fn a_missing_capability_says_so_rather_than_naming_an_endpoint() {
    // The kernel refuses the call before the manager sees it. "Permission
    // denied" against an endpoint the caller never reached is baffling, so
    // the tool names the actual cause.
    let mut channel = MockChannel::unreachable(Errno::PermissionDenied);
    let mut io = MockIo::default();
    let exit = run(&mut channel, &mut io, ServiceControlOp::Stop, "timed");

    assert_eq!(exit, Exit::Failed);
    let line = io.err.first().expect("the refusal is stated");
    assert!(line.contains("may not control services"), "{line}");
}

#[test]
fn no_manager_serving_the_endpoint_is_distinguished_from_a_denial() {
    let mut channel = MockChannel::unreachable(Errno::NotFound);
    let mut io = MockIo::default();
    let exit = run(&mut channel, &mut io, ServiceControlOp::Start, "timed");

    assert_eq!(exit, Exit::Failed);
    let line = io.err.first().expect("the refusal is stated");
    assert!(line.contains("no service manager"), "{line}");
}

#[test]
fn a_corrupt_reply_fails_closed_rather_than_reporting_success() {
    // A success status word with a garbage state byte: the decoder refuses
    // it, and the tool must not claim the operation was applied.
    let mut channel = MockChannel {
        answer: Ok(alloc::vec![0, 0, 0, 0, 0xEE, 0, 0, 0]),
        seen: Vec::new(),
    };
    let mut io = MockIo::default();
    let exit = run(&mut channel, &mut io, ServiceControlOp::Stop, "svc");

    assert_eq!(exit, Exit::Failed);
    assert!(io.out.is_empty(), "a corrupt reply never reads as applied");
}

#[test]
fn every_usage_failure_states_its_reason_and_the_banner() {
    for err in [
        UsageError::MissingCommand,
        UsageError::UnknownCommand,
        UsageError::WrongOperandCount,
        UsageError::NameTooLong,
    ] {
        let mut io = MockIo::default();
        let exit = report_usage(&mut io, err);
        assert_eq!(exit, Exit::Usage);
        assert_eq!(exit.code(), 2);
        assert_eq!(io.err.len(), 2, "the reason and the usage banner");
        assert!(io.err[1].contains("servicectl"), "{:?}", io.err);
    }
}

#[test]
fn a_help_request_is_left_to_the_caller() {
    // The bundle's own Help tree is the binary's to read, so `dispatch`
    // reports nothing rather than inventing text here.
    let mut channel = MockChannel::applying(ServiceState::Running);
    let mut io = MockIo::default();
    assert_eq!(dispatch(&mut channel, &mut io, Command::Help), None);
    assert!(channel.seen.is_empty(), "help posts no control request");
}

#[test]
fn a_parsed_line_dispatches_to_the_manager() {
    let command = parse(&["stop", "timed"]).expect("parses");
    let mut channel = MockChannel::applying(ServiceState::Stopping);
    let mut io = MockIo::default();
    assert_eq!(dispatch(&mut channel, &mut io, command), Some(Exit::Ok));
    assert_eq!(channel.seen.len(), 1);
}
