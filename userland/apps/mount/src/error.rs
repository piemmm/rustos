//! The outcomes of running a `mount` command.

use core::fmt;
use rustos_abi::Errno;
use rustos_procinfo::{CallError, ListError};

/// Why a `mount` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountError {
    /// The command line carried an unrecognised option or a number of
    /// operands other than zero (list) or two (mount). The caller should
    /// print [`crate::USAGE`].
    Usage,
    /// A `-o` option value named an unknown mount option, or `-t`/`-o` was
    /// given an empty value.
    BadOption,
    /// The mount operation was refused or failed. Carries the underlying
    /// [`Errno`] — e.g. [`Errno::PermissionDenied`] when the caller lacks
    /// `CAP_FS_MOUNT`, which is the kernel's decision to make, not the
    /// tool's (`AGENTS.md` §5.4).
    Mount(Errno),
    /// Listing the mount table failed: the transport errored or the reply
    /// did not decode against `sysinfo-v1`. Carries the underlying
    /// [`Errno`].
    Service(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl From<CallError> for MountError {
    fn from(err: CallError) -> Self {
        match err {
            // The mount-list query is ungated (`AGENTS.md` §16.6), so a
            // denial is a service-level anomaly rather than a missing
            // capability the user could grant; report it as such.
            CallError::PermissionDenied => Self::Service(Errno::PermissionDenied),
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl From<ListError> for MountError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for MountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::BadOption => f.write_str("invalid mount option"),
            Self::Mount(errno) => write!(f, "mount failed: {errno}"),
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
