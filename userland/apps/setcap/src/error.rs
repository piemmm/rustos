//! The outcomes of running a `setcap` command.

use core::fmt;
use rustos_abi::Errno;

/// Why a `setcap` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetcapError {
    /// The command line carried an unrecognised option, or named fewer than
    /// two operands (`setcap` needs a capability spec and at least one file).
    /// The caller should print [`crate::USAGE`]. Nothing is changed.
    Usage,
    /// The capability operand was neither a known canonical `CAP_*` name nor
    /// the literal `-` (clear the gate). Nothing is changed.
    BadCapability,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing file or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Applying the new capability gate to a file failed. Carries the
    /// underlying [`Errno`] — e.g. [`Errno::PermissionDenied`] when the caller
    /// lacks the authority to set a gate.
    Apply(Errno),
    /// Reading a directory's entries during a recursive (`-R`) descent failed.
    /// Carries the underlying [`Errno`].
    Read(Errno),
    /// Writing the usage banner to the terminal failed. Carries the underlying
    /// [`Errno`].
    Output(Errno),
}

impl fmt::Display for SetcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::BadCapability => f.write_str("invalid capability"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Apply(errno) => write!(f, "cannot set capability: {errno}"),
            Self::Read(errno) => write!(f, "cannot read directory: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
