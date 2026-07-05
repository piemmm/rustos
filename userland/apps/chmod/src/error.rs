//! The outcomes of running a `chmod` command.

use core::fmt;
use rustos_abi::Errno;

/// Why a `chmod` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChmodError {
    /// The command line carried an unrecognised option, or named fewer than
    /// two operands (`chmod` needs a mode and at least one file). The caller
    /// should print [`crate::USAGE`]. Nothing is changed.
    Usage,
    /// The mode operand could not be parsed as an octal mode (one to four
    /// octal digits) or a symbolic mode (`[ugoa]*[-+=][rwxXst]*`, clauses
    /// separated by commas). Nothing is changed.
    BadMode,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing file or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Applying the new mode to a file failed. Carries the underlying
    /// [`Errno`].
    Apply(Errno),
    /// Reading a directory's entries during a recursive (`-R`) descent failed.
    /// Carries the underlying [`Errno`].
    Read(Errno),
    /// Writing the usage banner to the terminal failed. Carries the underlying
    /// [`Errno`].
    Output(Errno),
    /// One or more operands failed under `-f`: the diagnostics were
    /// suppressed and the run continued, but the failure still fails the
    /// run. Carries no message — that is the point of `-f`.
    Silenced,
}

impl fmt::Display for ChmodError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::BadMode => f.write_str("invalid mode"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Apply(errno) => write!(f, "cannot change mode: {errno}"),
            Self::Read(errno) => write!(f, "cannot read directory: {errno}"),
            Self::Silenced => f.write_str("some operands failed (diagnostics suppressed by -f)"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
