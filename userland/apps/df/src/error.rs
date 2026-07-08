//! The outcomes of running a `df` command.

use core::fmt;
use rustos_abi::Errno;
use rustos_procinfo::{CallError, ListError};

/// Why a `df` invocation did not complete.
///
/// A `file` operand that cannot be probed is diagnosed on standard error
/// and the report continues (the GNU behaviour), surfacing as a `false`
/// clean flag rather than an error. Fatal are only the failures of the
/// tool's own machinery — the mount-table query and the output stream —
/// plus the GNU `no file systems processed` outcome when the type
/// filters leave nothing to report. The wire-level causes lean on the
/// frozen [`Errno`] so no parallel error set is invented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DfError {
    /// The `MOUNT_LIST` query failed: the transport errored, the service
    /// refused, or the reply did not decode against `sysinfo-v1`.
    Service(Errno),
    /// A table row (or the short help) could not be written to the
    /// terminal.
    Output(Errno),
    /// Every filesystem was filtered away (`-t`/`-x` left nothing to
    /// report).
    NothingProcessed,
}

impl From<CallError> for DfError {
    fn from(err: CallError) -> Self {
        match err {
            // The mount-list query is ungated, so a denial is a
            // service-level anomaly rather than a missing capability the
            // user could grant; report it as such.
            CallError::PermissionDenied => Self::Service(Errno::PermissionDenied),
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl From<ListError> for DfError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for DfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
            Self::NothingProcessed => f.write_str("no file systems processed"),
        }
    }
}
