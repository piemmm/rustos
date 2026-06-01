//! The outcomes of running a `sysinfo` command.

use core::fmt;
use rustos_abi::Errno;
use rustos_procinfo::{CallError, ListError};

/// Why a `sysinfo` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SysinfoError {
    /// The command line did not name a known query, or carried an
    /// unrecognised argument. The caller should print [`crate::USAGE`].
    Usage,
    /// The service refused the query because the caller lacks the
    /// capability the query declares (`AGENTS.md` §16.6). Distinguished
    /// from [`SysinfoError::Service`] so the CLI can print the precise
    /// "this query requires a capability you do not hold" diagnostic.
    PermissionDenied,
    /// The transport failed, or the reply did not decode against
    /// `sysinfo-v1`. Carries the underlying [`Errno`].
    Service(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl From<CallError> for SysinfoError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::PermissionDenied => Self::PermissionDenied,
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl From<ListError> for SysinfoError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for SysinfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::PermissionDenied => {
                f.write_str("permission denied: this query requires a capability you do not hold")
            }
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
