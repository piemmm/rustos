//! Driving one `servicectl` operation and rendering its outcome.
//!
//! The whole behaviour sits behind two seams — [`ControlChannel`] for the
//! endpoint and [`ToolIo`] for the inherited streams — so every path,
//! including each refusal, is host-tested without a kernel.

use tairix_abi::service_control::{decode_reply, ServiceControlRequest, REPLY_LEN, REQUEST_LEN};
use tairix_abi::{Errno, ServiceControlOp, ServiceState};

use crate::command::{Command, UsageError, USAGE};

/// Exit status of a `servicectl` run.
///
/// The coreutils shape: success, a general failure, and a usage error, with
/// the same numbers a shell script would expect from any other tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum Exit {
    /// The operation was applied, or the short help was shown.
    Ok = 0,
    /// The manager refused the operation, or the endpoint could not be
    /// reached.
    Failed = 1,
    /// The command line was not understood; nothing was sent.
    Usage = 2,
}

impl Exit {
    /// The process exit code this status reports.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// The control endpoint, as a seam.
pub trait ControlChannel {
    /// Post an encoded request frame and return the reply frame's length,
    /// or the raw negative kernel result (`-errno`).
    ///
    /// The tool never interprets the kernel's refusal beyond reporting it: a
    /// missing `CAP_SERVICE_CONTROL` is the kernel's answer to reaching the
    /// endpoint at all, not something the tool checks for itself.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, i64>;
}

/// The inherited standard streams, as a seam.
pub trait ToolIo {
    /// Write one line to the primary output (fd 1).
    fn write_line(&mut self, line: &str);
    /// Write one line to the diagnostic stream (fd 2).
    fn write_error(&mut self, line: &str);
}

/// Render the parse failure and return the usage status.
///
/// Every refusal states its reason before exiting, so a non-zero status is
/// never silent.
pub fn report_usage<I: ToolIo>(io: &mut I, err: UsageError) -> Exit {
    let reason = match err {
        UsageError::MissingCommand => "servicectl: no command given",
        UsageError::UnknownCommand => "servicectl: unknown command",
        UsageError::WrongOperandCount => "servicectl: exactly one service name is required",
        UsageError::NameTooLong => "servicectl: service name is too long",
    };
    io.write_error(reason);
    io.write_error(USAGE);
    Exit::Usage
}

/// Drive one parsed control command over `channel`, reporting the outcome.
///
/// [`Command::Help`] is the caller's to render (it needs the bundle's own
/// Help tree), so this handles only the control path.
pub fn run<C: ControlChannel, I: ToolIo>(
    channel: &mut C,
    io: &mut I,
    op: ServiceControlOp,
    service: &str,
) -> Exit {
    let mut frame = [0u8; REQUEST_LEN];
    let request = ServiceControlRequest { op, name: service };
    let Ok(written) = request.encode(&mut frame) else {
        // The parser already bounded the name, so this is unreachable from a
        // parsed line; it stays a reported refusal rather than a panic.
        io.write_error("servicectl: the request could not be encoded");
        return Exit::Failed;
    };

    let mut reply = [0u8; REPLY_LEN];
    let length = match channel.call(&frame[..written], &mut reply) {
        Ok(length) => length,
        Err(err) => {
            io.write_error(&unreachable_message(err));
            return Exit::Failed;
        }
    };

    match decode_reply(&reply[..length]) {
        Ok(state) => {
            io.write_line(&applied_message(op, service, state));
            Exit::Ok
        }
        Err(err) => {
            io.write_error(&refused_message(op, service, err));
            Exit::Failed
        }
    }
}

/// One line stating what the manager did, naming the state it left the
/// service in — the fact a script or an operator actually wants.
fn applied_message(
    op: ServiceControlOp,
    service: &str,
    state: ServiceState,
) -> alloc::string::String {
    alloc::format!("{}: {service} is now {}", verb(op), state_name(state))
}

/// One line stating that a reachable request was refused, and why.
fn refused_message(op: ServiceControlOp, service: &str, err: Errno) -> alloc::string::String {
    let reason = match err {
        Errno::NotFound => "no such service is registered",
        Errno::Busy => "the service is not in a state to be started",
        Errno::NotSupported => "the service could not be launched",
        Errno::PermissionDenied => "permission denied",
        // The stable ABI already spells every other code; restating them
        // here would be a second copy that could drift.
        other => return alloc::format!("servicectl: {} {service}: {other}", verb(op)),
    };
    alloc::format!("servicectl: {} {service}: {reason}", verb(op))
}

/// One line stating that the endpoint itself could not be reached.
///
/// A missing capability is the commonest cause by far and is named
/// explicitly, because "permission denied" against an endpoint the caller
/// never saw is otherwise baffling.
fn unreachable_message(err: i64) -> alloc::string::String {
    let errno = Errno::from_syscall(err);
    if errno == Errno::PermissionDenied {
        return alloc::string::String::from(
            "servicectl: permission denied: this account may not control services",
        );
    }
    if errno == Errno::NotFound {
        return alloc::string::String::from(
            "servicectl: no service manager is serving the control endpoint",
        );
    }
    alloc::format!("servicectl: the control endpoint refused the call: {errno}")
}

/// The verb a message names an operation by, matching the command line the
/// user typed rather than the ABI's own spelling.
const fn verb(op: ServiceControlOp) -> &'static str {
    match op {
        ServiceControlOp::Start => "start",
        ServiceControlOp::Stop => "stop",
    }
}

/// The lifecycle state a reply carries, spelled for a terminal.
const fn state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Inactive => "inactive",
        ServiceState::Starting => "starting",
        ServiceState::Ready => "ready",
        ServiceState::Running => "running",
        ServiceState::Stopping => "stopping",
        ServiceState::Stopped => "stopped",
        ServiceState::Failed => "failed",
    }
}

/// Dispatch a parsed command that is not the help switch.
///
/// Kept beside [`run`] so the binary's `main` is only seam binding.
pub fn dispatch<C: ControlChannel, I: ToolIo>(
    channel: &mut C,
    io: &mut I,
    command: Command<'_>,
) -> Option<Exit> {
    match command {
        Command::Control { op, service } => Some(run(channel, io, op, service)),
        // The caller renders its own bundle's help.
        Command::Help => None,
    }
}
