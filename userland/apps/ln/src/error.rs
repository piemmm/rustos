//! The outcomes of running an `ln` command.

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;

/// Why an `ln` invocation did not complete.
///
/// Each variant carries the operand the GNU tool names in its own
/// diagnostic, so the message a user reads points at the path that failed;
/// the wire-level cause is the frozen [`Errno`], never a parallel error set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LnError {
    /// The command line carried an unrecognised option, or an option pair
    /// that cannot both hold (`-t` with `-T`). No path is touched.
    Usage,
    /// `-r` was given without `-s`. A hard link stores no target, so there
    /// is nothing to make relative; refused rather than ignored.
    RelativeNeedsSymbolic,
    /// No operand was given.
    MissingOperand,
    /// A form that needs a destination operand was given only one operand.
    MissingDestination(String),
    /// More operands than the form accepts (`-T` takes exactly two).
    ExtraOperand(String),
    /// The destination that must be an existing directory (`-t`, or the last
    /// of three or more operands) is not one.
    NotADirectory(String),
    /// A path could not be inspected; carries the link name and the
    /// underlying [`Errno`].
    Stat(String, Errno),
    /// `-r` could not canonicalise a path it needs to compute the relative
    /// target — the target itself, or the link's own directory. Carries the
    /// path and the underlying [`Errno`].
    Canonicalize(String, Errno),
    /// An existing name `-f` or `-i` said to replace could not be removed.
    Remove(String, Errno),
    /// The link could not be created; carries the link name and the
    /// underlying [`Errno`] ([`Errno::AlreadyExists`] for a taken name,
    /// [`Errno::NotSupported`] on a format that stores no links).
    Create(String, Errno),
    /// A confirmation could not be read. Never treated as consent.
    Prompt(Errno),
    /// Writing the usage banner or a `-v` report failed.
    Output(Errno),
}

impl fmt::Display for LnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::RelativeNeedsSymbolic => f.write_str("cannot do --relative without --symbolic"),
            Self::MissingOperand => f.write_str("missing file operand"),
            Self::MissingDestination(target) => {
                write!(f, "missing destination file operand after '{target}'")
            }
            Self::ExtraOperand(operand) => write!(f, "extra operand '{operand}'"),
            Self::NotADirectory(path) => write!(f, "target '{path}' is not a directory"),
            Self::Stat(path, errno) => write!(f, "cannot access '{path}': {errno}"),
            Self::Canonicalize(path, errno) => {
                write!(f, "cannot canonicalize '{path}': {errno}")
            }
            Self::Remove(path, errno) => write!(f, "cannot remove '{path}': {errno}"),
            Self::Create(link, errno) => {
                write!(f, "failed to create symbolic link '{link}': {errno}")
            }
            Self::Prompt(errno) => write!(f, "cannot read the reply: {errno}"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
