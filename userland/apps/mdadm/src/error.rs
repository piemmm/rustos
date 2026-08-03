//! The outcomes of resolving a name and running an `mdadm` command.
//!
//! The wire-level causes lean on the frozen [`Errno`] so no parallel error
//! set is invented; the tool-level variants distinguish only what changes the
//! diagnostic a user sees (a denied capability, a name that resolved to
//! nothing or to more than one array). Every path fails closed: an ambiguous
//! or unknown name is refused, never guessed at.

use core::fmt;

use alloc::string::String;
use tairix_abi::Errno;

/// Why a device or array name did not resolve to exactly one target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    /// An array name that is not a hexadecimal identity (or a prefix of one).
    BadArrayName(String),
    /// A hexadecimal identity (or prefix) that matches no live array.
    ArrayNotFound(String),
    /// A hexadecimal prefix that matches more than one live array; refused
    /// rather than guessing which was meant.
    AmbiguousArray(String),
    /// A device name that is not the `node:<id>` spelling the tool accepts.
    BadDeviceName(String),
    /// The same device was named twice in a `--create`.
    DuplicateDevice(String),
    /// A `--create` named more devices than any array can hold.
    TooManyDevices {
        /// The number of devices named.
        got: usize,
        /// The most an array can hold.
        max: usize,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadArrayName(value) => write!(
                f,
                "'{value}' is not an array identity (a hexadecimal identity or a prefix of one)"
            ),
            Self::ArrayNotFound(value) => write!(f, "no array matches identity '{value}'"),
            Self::AmbiguousArray(value) => write!(
                f,
                "identity '{value}' matches more than one array; use more digits"
            ),
            Self::BadDeviceName(value) => write!(
                f,
                "'{value}' is not a device; name a device by its node id as 'node:<id>'"
            ),
            Self::DuplicateDevice(value) => {
                write!(f, "device '{value}' was named more than once")
            }
            Self::TooManyDevices { got, max } => {
                write!(f, "{got} devices named; an array holds at most {max}")
            }
        }
    }
}

/// Why an `mdadm` invocation did not complete.
///
/// A usage error (a command line that does not parse) is reported by the
/// `Run` binary before the engine runs and exits `2`; these are the runtime
/// outcomes that exit `1`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MdadmError {
    /// A read was refused: the caller lacks `CAP_SYSINFO_HW`.
    ReadDenied,
    /// A mutation was refused: the caller lacks `CAP_STORAGE_ADMIN`.
    AdminDenied,
    /// A read query failed for any other reason, or its reply did not decode.
    Service(Errno),
    /// A device or array name did not resolve.
    Resolve(ResolveError),
    /// The composer refused a mutation with a typed reason (e.g. a device that
    /// is not an unaffiliated candidate, or an array that is still in use).
    Refused(Errno),
    /// The control request could not be encoded.
    Encode(Errno),
    /// A report line (or the short help) could not be written.
    Output(Errno),
}

impl From<ResolveError> for MdadmError {
    fn from(err: ResolveError) -> Self {
        Self::Resolve(err)
    }
}

impl fmt::Display for MdadmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDenied => f.write_str("reading the array inventory requires CAP_SYSINFO_HW"),
            Self::AdminDenied => f.write_str("administering arrays requires CAP_STORAGE_ADMIN"),
            Self::Service(errno) => write!(f, "array inventory service error: {errno}"),
            Self::Resolve(err) => err.fmt(f),
            Self::Refused(errno) => write!(f, "the array composer refused the request: {errno}"),
            Self::Encode(errno) => write!(f, "cannot encode the control request: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
        }
    }
}
