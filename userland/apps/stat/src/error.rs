//! The outcomes of running a `stat` command.

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;

/// Why a `stat` invocation did not complete.
///
/// Each variant carries the operand or specifier the diagnostic names, so
/// the message points at what failed; the wire-level cause is always the
/// frozen [`Errno`], never a parallel error set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatError {
    /// The command line carried an unrecognised option, or a switch that
    /// takes a value was given none. No path is touched.
    Usage,
    /// No operand was given.
    MissingOperand,
    /// A format named a letter that is not a specifier of the active
    /// vocabulary. Carries the letter.
    UnknownSpecifier(char),
    /// A format named a specifier this platform cannot answer honestly.
    /// Carries the letter and the reason.
    Unsupported(char, String),
    /// Writing a rendering or a diagnostic failed. Carries the underlying
    /// [`Errno`]; a stream that stops accepting bytes is fatal.
    Output(Errno),
}

impl fmt::Display for StatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::MissingOperand => f.write_str("missing operand"),
            Self::UnknownSpecifier(letter) => {
                write!(f, "%{letter}: invalid directive")
            }
            Self::Unsupported(letter, reason) => {
                write!(f, "%{letter} is not available: {reason}")
            }
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}
