//! The outcomes of running a `telnet` command.

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;

/// Why a `telnet` invocation did not complete.
///
/// A refused socket open (want of `CAP_NET`), a target that will not resolve,
/// or an unreachable host defeats the tool's whole purpose, so each is fatal
/// and reported with an actionable reason rather than a bare exit code. The
/// wire-level causes lean on the frozen [`Errno`] so no parallel error set is
/// invented.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelnetError {
    /// Opening the stream socket was refused for want of `CAP_NET`.
    Denied,
    /// The socket could not be opened for another reason.
    Socket(Errno),
    /// The named host could not be resolved to an address.
    Resolve(String),
    /// The connection to the host could not be established.
    Connect(Errno),
    /// Transmitting on the connection failed.
    Send(Errno),
    /// Receiving from the connection failed.
    Receive(Errno),
    /// A line could not be written to the terminal.
    Output(Errno),
    /// The local terminal could not be put into the raw read discipline, so a
    /// relay would echo every keystroke twice and corrupt the session.
    Terminal(Errno),
}

impl fmt::Display for TelnetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str(
                "opening a network connection requires CAP_NET, which this session lacks",
            ),
            Self::Socket(errno) => write!(f, "cannot open a socket: {errno}"),
            Self::Resolve(host) => write!(f, "cannot resolve {host}"),
            Self::Connect(errno) => write!(f, "cannot connect: {errno}"),
            Self::Send(errno) => write!(f, "cannot send to the remote host: {errno}"),
            Self::Receive(errno) => write!(f, "cannot receive from the remote host: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
            Self::Terminal(errno) => write!(f, "cannot set the terminal read mode: {errno}"),
        }
    }
}
