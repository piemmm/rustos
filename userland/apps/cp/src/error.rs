//! The outcomes of running a `cp` command.

use core::fmt;
use tairix_abi::Errno;

/// Why a `cp` invocation did not complete.
///
/// The variants are deliberately coarse: the CLI surfaces enough to print a
/// useful diagnostic and set a process exit status, while leaning on the
/// frozen [`Errno`] for the wire-level cause so it invents no parallel error
/// set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpError {
    /// The command line carried an unrecognised option, named fewer than two
    /// operands (`cp` needs at least one source and a destination), or aimed
    /// more than one source at a destination that is not a directory. The
    /// caller should print [`crate::USAGE`]. Nothing is copied.
    Usage,
    /// A source named a directory but `-r` was not given. Nothing about that
    /// operand is copied (`cp` refuses to recurse implicitly).
    IsDirectory,
    /// A directory source's destination already exists and is not a directory,
    /// so the subtree cannot be reproduced there.
    NotADirectory,
    /// Inspecting an operand failed. Carries the underlying [`Errno`] — e.g.
    /// [`Errno::NotFound`] for a missing source or [`Errno::PermissionDenied`]
    /// when the caller may not reach it.
    Stat(Errno),
    /// Reading a source file's bytes or a directory's entries failed. Carries
    /// the underlying [`Errno`].
    Read(Errno),
    /// Creating a destination file or directory failed. Carries the underlying
    /// [`Errno`].
    Create(Errno),
    /// Writing a destination file's bytes failed. Carries the underlying
    /// [`Errno`].
    Write(Errno),
    /// Writing to the terminal failed. Carries the underlying [`Errno`].
    Output(Errno),
    /// Asking the interactive confirmation question failed. Carries the
    /// underlying [`Errno`]. Failing closed: an unanswerable prompt never
    /// counts as consent.
    Prompt(Errno),
}

impl fmt::Display for CpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::IsDirectory => f.write_str("is a directory (use -r to copy)"),
            Self::NotADirectory => f.write_str("destination is not a directory"),
            Self::Stat(errno) => write!(f, "cannot access path: {errno}"),
            Self::Read(errno) => write!(f, "cannot read source: {errno}"),
            Self::Create(errno) => write!(f, "cannot create destination: {errno}"),
            Self::Write(errno) => write!(f, "cannot write destination: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
            Self::Prompt(errno) => write!(f, "cannot read confirmation: {errno}"),
        }
    }
}
