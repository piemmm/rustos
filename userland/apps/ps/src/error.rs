//! The outcomes of running a `ps` command.

use core::fmt;
use tairix_abi::Errno;
use tairix_procinfo::{CallError, ListError};

/// Why a `ps` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsError {
    /// The command line carried an unrecognised option. The caller should
    /// print [`crate::USAGE`].
    Usage,
    /// The service refused the query because the caller lacks the
    /// capability the query declares. Distinguished
    /// from [`PsError::Service`] so the CLI can print the precise "this
    /// listing requires a capability you do not hold" diagnostic.
    PermissionDenied,
    /// The transport failed, or the reply did not decode against
    /// `sysinfo-v1`. Carries the underlying [`Errno`].
    Service(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl From<CallError> for PsError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::PermissionDenied => Self::PermissionDenied,
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl From<ListError> for PsError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for PsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::PermissionDenied => f.write_str(
                "permission denied: listing every process requires a capability you do not hold",
            ),
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
