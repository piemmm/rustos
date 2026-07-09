//! The outcomes of running an `lsusb` command.

use core::fmt;

use rustos_abi::Errno;
use rustos_procinfo::CallError;

/// Why an `lsusb` invocation did not complete.
///
/// Listing the hardware inventory is the tool's whole purpose, so — unlike
/// an incidental refusal — a denied `HARDWARE_TREE` query is fatal: the
/// reason lands on standard error and the tool exits, never a fabricated
/// empty listing. The wire-level causes lean on the frozen [`Errno`] so no
/// parallel error set is invented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LsusbError {
    /// The service refused the query: the caller lacks `CAP_SYSINFO_HW`.
    PermissionDenied,
    /// The `HARDWARE_TREE` query failed: the transport errored or the
    /// reply did not decode as whole hardware-tree nodes.
    Service(Errno),
    /// A listing line (or the short help) could not be written to the
    /// terminal.
    Output(Errno),
}

impl From<CallError> for LsusbError {
    fn from(err: CallError) -> Self {
        match err {
            CallError::PermissionDenied => Self::PermissionDenied,
            CallError::Service(errno) => Self::Service(errno),
        }
    }
}

impl fmt::Display for LsusbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => {
                f.write_str("reading the hardware inventory requires CAP_SYSINFO_HW")
            }
            Self::Service(errno) => write!(f, "system information service error: {errno}"),
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
        }
    }
}
