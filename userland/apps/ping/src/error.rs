//! The outcomes of running a `ping` command.

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;

use crate::net::ResolveFailure;

/// Why a `ping` invocation did not complete.
///
/// A target that will not resolve, a refused socket open (want of
/// `CAP_NET`/`CAP_NET_RAW`), or an unreachable network defeats the tool's
/// whole purpose, so it is fatal and reported with an actionable reason
/// (fail loud). The wire-level causes lean on the frozen [`Errno`] so no
/// parallel error set is invented.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PingError {
    /// The target operand did not resolve to an address of the wanted
    /// family, so there is nothing to ping. Carries the host as typed, since
    /// that is what the user needs to see corrected.
    Resolve {
        /// The target operand exactly as it was given.
        host: String,
        /// Why it did not resolve.
        reason: ResolveFailure,
    },
    /// Opening the ICMP echo socket was refused for want of the required
    /// capability (`CAP_NET`/`CAP_NET_RAW`).
    Denied,
    /// The socket could not be opened or connected for another reason.
    Socket(Errno),
    /// A per-request send failed with a fatal (non-transient) error.
    Send(Errno),
    /// A reply receive failed with a fatal error.
    Receive(Errno),
    /// A line (a reply, or the short help) could not be written.
    Output(Errno),
}

impl fmt::Display for PingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The iputils wording, so a user or script that knows `ping`
            // recognises the diagnosis.
            Self::Resolve {
                host,
                reason: ResolveFailure::Unknown,
            } => write!(f, "{host}: Name or service not known"),
            Self::Resolve {
                host,
                reason: ResolveFailure::FamilyMismatch,
            } => write!(f, "{host}: Address family for hostname not supported"),
            Self::Denied => f.write_str(
                "opening an ICMP echo socket requires CAP_NET and CAP_NET_RAW, \
                 which this session lacks",
            ),
            Self::Socket(errno) => write!(f, "cannot open the echo socket: {errno}"),
            Self::Send(errno) => write!(f, "cannot send the echo request: {errno}"),
            Self::Receive(errno) => write!(f, "cannot receive the echo reply: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
        }
    }
}
