//! The outcomes of running an `ss` command.

use core::fmt;
use tairix_abi::Errno;
use tairix_procinfo::{CallError, ListError};

/// Why an `ss` invocation did not complete.
///
/// The socket listing is the tool's whole purpose, so — unlike a
/// capacity-less optional query — a refused `NET_SOCKETS` query is fatal:
/// the tool reports the denial and exits rather than printing an empty
/// table that a reader would mistake for "no sockets". The wire-level
/// causes lean on the frozen [`Errno`] so no parallel error set is
/// invented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SsError {
    /// The `NET_SOCKETS` query was refused for want of the required
    /// capability (`CAP_SYSINFO_GLOBAL`). Reported with the actionable
    /// reason rather than a bare errno.
    Denied,
    /// The `NET_SOCKETS` query failed: the transport errored, the service
    /// refused for another reason, or the reply did not decode against
    /// `sysinfo-v1`.
    Service(Errno),
    /// A table row (or the short help) could not be written.
    Output(Errno),
}

impl From<CallError> for SsError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::PermissionDenied => Self::Denied,
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl From<ListError> for SsError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for SsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str(
                "listing all sockets requires CAP_SYSINFO_GLOBAL, which this session lacks",
            ),
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
        }
    }
}
