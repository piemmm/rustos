//! The outcomes of running an `rm` command.

use core::fmt;
use rustos_abi::Errno;

/// Why an `rm` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RmError {
    /// The command line carried an unrecognised option, or named no operand
    /// without `-f`. The caller should print [`crate::USAGE`]. Nothing is
    /// removed.
    Usage,
    /// An operand named a directory but `-r` was not given. Nothing about
    /// that operand is removed (`rm` refuses to recurse implicitly).
    IsDirectory,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing path (suppressed by `-f`) or
    /// [`Errno::PermissionDenied`] when the caller may not reach it.
    Stat(Errno),
    /// Reading a directory's entries failed during recursion. Carries the
    /// underlying [`Errno`].
    Read(Errno),
    /// Removing a file or directory failed. Carries the underlying [`Errno`].
    Remove(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for RmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::IsDirectory => f.write_str("is a directory (use -r to remove)"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Read(errno) => write!(f, "cannot read directory: {errno}"),
            Self::Remove(errno) => write!(f, "cannot remove path: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
