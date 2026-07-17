//! The outcomes of running the `top` viewer.

use core::fmt;

use tairix_abi::Errno;
use tairix_curses::CursesError;
use tairix_procinfo::{CallError, ListError};

/// Why a `top` session ended other than by the user quitting.
///
/// The variants are deliberately coarse: enough to print a useful diagnostic
/// and set an exit status, while leaning on the frozen [`Errno`] for the
/// wire-level cause so the tool invents no parallel error set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopError {
    /// The command line carried an unrecognised option or an operand. The
    /// caller should print [`crate::USAGE`]. The viewer never starts.
    Usage,
    /// The service refused the system-wide process listing because the
    /// caller lacks `CAP_SYSINFO_GLOBAL`. Distinguished
    /// from [`TopError::Service`] so the viewer can show the precise
    /// "global view needs a capability you do not hold" message.
    PermissionDenied,
    /// The transport failed, or the reply did not decode against
    /// `sysinfo-v1`. Carries the underlying [`Errno`].
    Service(Errno),
    /// Drawing to or reading from the terminal failed.
    Terminal(CursesError),
}

impl From<CallError> for TopError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::PermissionDenied => Self::PermissionDenied,
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl From<ListError> for TopError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            // The viewer's per-record sink only stores into a vector and
            // cannot fail, so a sink error is surfaced as a service error
            // rather than silently dropped.
            ListError::Sink(errno) => Self::Service(errno),
        }
    }
}

impl From<CursesError> for TopError {
    fn from(err: CursesError) -> Self {
        Self::Terminal(err)
    }
}

impl fmt::Display for TopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::PermissionDenied => f.write_str(
                "permission denied: the system-wide process view requires a capability you do not hold",
            ),
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Terminal(err) => write!(f, "terminal error: {err:?}"),
        }
    }
}
