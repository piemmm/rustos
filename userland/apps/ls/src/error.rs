//! The outcomes of running an `ls` command.

use core::fmt;
use tairix_abi::Errno;

/// Why an `ls` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LsError {
    /// The command line carried an unrecognised option. The caller should
    /// print [`crate::USAGE`]. No path is inspected.
    Usage,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing path or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Reading a directory's entries failed. Carries the underlying
    /// [`Errno`].
    Read(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for LsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Read(errno) => write!(f, "cannot read directory: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
