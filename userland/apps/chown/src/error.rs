//! The outcomes of running a `chown` command.

use core::fmt;
use rustos_abi::Errno;

/// Why a `chown` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChownError {
    /// The command line carried an unrecognised option, or named fewer than
    /// two operands (`chown` needs an owner spec and at least one file). The
    /// caller should print [`crate::USAGE`]. Nothing is changed.
    Usage,
    /// The owner operand could not be parsed as `OWNER`, `OWNER:GROUP`, or
    /// `:GROUP`, where `OWNER` and `GROUP` are decimal user/group ids.
    /// Nothing is changed.
    BadOwner,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing file or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Applying the new owner to a file failed. Carries the underlying
    /// [`Errno`].
    Apply(Errno),
    /// Reading a directory's entries during a recursive (`-R`) descent failed.
    /// Carries the underlying [`Errno`].
    Read(Errno),
    /// Writing the usage banner to the terminal failed. Carries the underlying
    /// [`Errno`].
    Output(Errno),
}

impl fmt::Display for ChownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::BadOwner => f.write_str("invalid owner"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Apply(errno) => write!(f, "cannot change owner: {errno}"),
            Self::Read(errno) => write!(f, "cannot read directory: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
