//! The outcomes of running an `mv` command.

use core::fmt;
use rustos_abi::Errno;

/// Why an `mv` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MvError {
    /// The command line carried an unrecognised option, named fewer than two
    /// operands (`mv` needs at least one source and a destination), or aimed
    /// more than one source at a destination that is not a directory. The
    /// caller should print [`crate::USAGE`]. Nothing is moved.
    Usage,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing source or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Renaming a source onto its destination failed for a reason other than
    /// crossing a filesystem boundary (a boundary crossing is not an error —
    /// it triggers the copy-then-remove relocation). Carries the underlying
    /// [`Errno`].
    Rename(Errno),
    /// During a cross-device relocation, reading a source file's bytes or a
    /// directory's entries failed. Carries the underlying [`Errno`].
    Read(Errno),
    /// During a cross-device relocation, creating a destination file or
    /// directory failed. Carries the underlying [`Errno`].
    Create(Errno),
    /// During a cross-device relocation, writing a destination file's bytes
    /// failed. Carries the underlying [`Errno`].
    Write(Errno),
    /// Removing the source after a successful cross-device copy failed.
    /// Carries the underlying [`Errno`]. The destination already holds the
    /// data; the source could not be unlinked.
    Remove(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
    /// Asking the interactive confirmation question failed. Carries the
    /// underlying [`Errno`]. Failing closed: an unanswerable prompt never
    /// counts as consent.
    Prompt(Errno),
}

impl fmt::Display for MvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Rename(errno) => write!(f, "cannot rename: {errno}"),
            Self::Read(errno) => write!(f, "cannot read source: {errno}"),
            Self::Create(errno) => write!(f, "cannot create destination: {errno}"),
            Self::Write(errno) => write!(f, "cannot write destination: {errno}"),
            Self::Remove(errno) => write!(f, "cannot remove source: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
            Self::Prompt(errno) => write!(f, "cannot read confirmation: {errno}"),
        }
    }
}
