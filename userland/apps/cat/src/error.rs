//! The outcomes of running a `cat` command.

use core::fmt;
use rustos_abi::Errno;

/// Why a `cat` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set (`AGENTS.md` §2.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatError {
    /// The command line carried an unrecognised option. The caller should
    /// print [`crate::USAGE`]. No source is read.
    Usage,
    /// Reading a source failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing file or [`Errno::PermissionDenied`]
    /// when the caller may not read it.
    Read(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for CatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Read(errno) => write!(f, "cannot read input: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
