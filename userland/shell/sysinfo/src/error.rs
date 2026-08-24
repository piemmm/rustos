//! The outcomes of running a `sysinfo` command.

use core::fmt;
use tairix_abi::Errno;
use tairix_procinfo::{CallError, ListError, ResolveInfoError};

/// Why a `sysinfo` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SysinfoError {
    /// The command line did not name a known query, or carried an
    /// unrecognised argument. The caller should print [`crate::USAGE`].
    Usage,
    /// The service refused the query because the caller lacks the
    /// capability the query declares. Distinguished
    /// from [`SysinfoError::Service`] so the CLI can print the precise
    /// "this query requires a capability you do not hold" diagnostic.
    PermissionDenied,
    /// The transport failed, or the reply did not decode against
    /// `sysinfo-v1`. Carries the underlying [`Errno`].
    Service(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
    /// The `show`/`describe` operand is not a well-formed resource reference:
    /// the shared spelling parser refused it. Distinguished from
    /// [`Usage`](Self::Usage) so the diagnostic can say the reference is
    /// malformed rather than implying the subcommand was wrong.
    BadReference,
    /// The reference is well-formed but the resolver did not produce a value.
    ///
    /// Carries the resolver's own typed refusal verbatim rather than
    /// flattening it, so the diagnostic can distinguish a namespace this tool
    /// does not read from an unknown selector, a missing mandatory
    /// `?window=`, or a capability denial — and, for a denial, name the
    /// capability the query declares.
    Unresolvable(ResolveInfoError),
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

impl From<ResolveInfoError> for SysinfoError {
    fn from(err: ResolveInfoError) -> Self {
        match err {
            // A transport or decode failure is the same class of fault
            // whatever asked for it, so it joins the existing vocabulary
            // rather than hiding inside the resolver variant.
            ResolveInfoError::Service(errno) => Self::Service(errno),
            other => Self::Unresolvable(other),
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
            Self::BadReference => f.write_str("not a well-formed resource reference"),
            // The one shared wording, so a refusal reads the same however
            // it was reached.
            Self::Unresolvable(err) => write!(f, "{err}"),
        }
    }
}
