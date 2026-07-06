//! The outcomes of parsing and running a `wc` command.

use alloc::string::String;
use core::fmt;
use rustos_abi::Errno;

/// Why a `wc` invocation did not complete.
///
/// Usage failures mirror the GNU tool's diagnostics; the runtime failure
/// leans on the frozen [`Errno`] for the wire-level cause so it invents no
/// parallel error set. A *per-file* read failure is not a variant here: the
/// client reports it on the diagnostic stream and continues with the next
/// operand, exactly as the GNU tool does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WcError {
    /// An unrecognised long option; carries the token.
    UnknownLong(String),
    /// An unrecognised short option; carries the flag character.
    UnknownShort(char),
    /// The `--total` value was not one of `auto`, `always`, `only`,
    /// `never`; carries the text.
    InvalidTotal(String),
    /// File operands were given together with `--files0-from`, which is
    /// the GNU conflict.
    Files0Conflict,
    /// `--total` or `--files0-from` was given without a value; carries the
    /// option's spelling for the diagnostic.
    MissingValue(&'static str),
    /// Writing to standard output (or the diagnostic stream) failed.
    /// Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for WcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLong(token) => write!(f, "unrecognized option '{token}'"),
            Self::UnknownShort(flag) => write!(f, "invalid option -- '{flag}'"),
            Self::InvalidTotal(text) => write!(f, "invalid argument '{text}' for '--total'"),
            Self::Files0Conflict => {
                f.write_str("file operands cannot be combined with --files0-from")
            }
            Self::MissingValue(option) => write!(f, "option '{option}' requires an argument"),
            Self::Output(errno) => write!(f, "write error: {errno}"),
        }
    }
}
