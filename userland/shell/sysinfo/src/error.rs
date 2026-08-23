//! The outcomes of running a `sysinfo` command.

use core::fmt;
use tairix_abi::{CapabilityId, Errno};
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
            Self::Unresolvable(err) => write_unresolvable(f, *err),
        }
    }
}

/// Spell a resolver refusal, naming the missing capability when the refusal
/// was a capability denial.
///
/// The capability comes from the frozen `sysinfo-v1` query registry through
/// [`ResolveInfoError::required_capability`] — the same table the broker gates
/// on — so the diagnostic tells the user which grant to ask for instead of a
/// bare "permission denied". A denial whose query the registry declares
/// ungated names none, because there would be nothing to grant.
fn write_unresolvable(f: &mut fmt::Formatter<'_>, err: ResolveInfoError) -> fmt::Result {
    match err {
        ResolveInfoError::NamespaceNotServed => f.write_str(
            "not a readable resource: only info:, state:, and stats: references have values",
        ),
        ResolveInfoError::UnknownSelector => {
            f.write_str("no such resource: the selector names nothing this system serves")
        }
        ResolveInfoError::UnsupportedRequest => f.write_str(
            "unserviceable reference: an unsupported guard, facet, or query parameter, \
             or a rate missing its mandatory ?window=",
        ),
        ResolveInfoError::CapabilityDenied(_) => {
            f.write_str("permission denied: this resource requires ")?;
            match err.required_capability().and_then(CapabilityId::name) {
                Some(name) => f.write_str(name),
                None => f.write_str("a capability you do not hold"),
            }
        }
        ResolveInfoError::Malformed => {
            f.write_str("the system information service replied with a record that did not decode")
        }
        // Mapped to `SysinfoError::Service` by the conversion above, so this
        // arm is unreachable in practice; spelled rather than panicking.
        ResolveInfoError::Service(errno) => {
            write!(f, "system information service error: {errno}")
        }
    }
}
