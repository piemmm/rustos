//! The outcomes of running a `getcap` command.

use core::fmt;
use tairix_abi::Errno;

/// Why a `getcap` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GetcapError {
    /// The command line carried an unrecognised option, or named no file
    /// operand. The caller should print [`crate::USAGE`]. Nothing is
    /// reported.
    Usage,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing file or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Reading a node's capability gate failed. Carries the underlying
    /// [`Errno`].
    Query(Errno),
    /// Reading a directory's entries during a recursive (`-R`) descent failed.
    /// Carries the underlying [`Errno`].
    Read(Errno),
    /// Writing the report or the usage banner to the terminal failed. Carries
    /// the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for GetcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Query(errno) => write!(f, "cannot read capability: {errno}"),
            Self::Read(errno) => write!(f, "cannot read directory: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
