//! The outcomes of running an `unmount` command.

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;
use tairix_procinfo::{CallError, ListError};

/// Why an `unmount` invocation did not complete.
///
/// The wire-level causes lean on the frozen [`Errno`] so no parallel
/// error set is invented; the name-shaped cases carry the operand so the
/// diagnostic names what the user typed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnmountError {
    /// The command line was not understood (unknown option, or a number
    /// of operands other than one).
    Usage,
    /// No mounted filesystem matches the named volume.
    NotFound(String),
    /// The named mount exists but carries no detachable volume identity
    /// (a boot volume or an in-RAM view binding — permanent by design).
    NotDetachable(String),
    /// The kernel refused or failed the detach. `unavailable` is `true`
    /// when the mount listing already showed the volume as
    /// surprise-removed, so the diagnostic can spell out the `--force`
    /// consequence.
    Detach {
        /// The kernel's exact refusal.
        errno: Errno,
        /// Whether the volume was listed as unavailable (dirty or lost).
        unavailable: bool,
    },
    /// The `MOUNT_LIST` query failed: the transport errored, the service
    /// refused, or the reply did not decode against `sysinfo-v1`.
    Service(Errno),
    /// A diagnostic (or the short help) could not be written.
    Output(Errno),
}

impl From<CallError> for UnmountError {
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

impl From<ListError> for UnmountError {
    fn from(err: ListError) -> Self {
        match err {
            ListError::Call(call) => call.into(),
            ListError::Sink(errno) => Self::Output(errno),
        }
    }
}

impl fmt::Display for UnmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::NotFound(name) => write!(f, "{name}: not mounted"),
            Self::NotDetachable(name) => {
                write!(f, "{name}: not a detachable volume")
            }
            Self::Detach {
                errno,
                unavailable: true,
            } => write!(
                f,
                "volume holds retained uncommitted data \
                 (use --force to discard it): {errno}"
            ),
            Self::Detach {
                errno,
                unavailable: false,
            } => write!(f, "detach failed: {errno}"),
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
        }
    }
}
