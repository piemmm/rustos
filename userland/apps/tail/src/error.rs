//! The outcomes of parsing and running a `tail` command.

use alloc::string::String;
use core::fmt;
use tairix_abi::Errno;

/// Why a `tail` invocation did not complete.
///
/// Usage failures mirror the GNU tool's diagnostics; the runtime failure
/// leans on the frozen [`Errno`] for the wire-level cause so it invents no
/// parallel error set. A *per-file* read failure is not a variant here: the
/// client reports it on the diagnostic stream and continues with the next
/// operand, exactly as the GNU tool does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TailError {
    /// An unrecognised long option; carries the token.
    UnknownLong(String),
    /// An unrecognised short option; carries the flag character.
    UnknownShort(char),
    /// The `-n`/`--lines` value was not a valid count; carries the text.
    InvalidLines(String),
    /// The `-c`/`--bytes` value was not a valid count; carries the text.
    InvalidBytes(String),
    /// A digit (or other letter the obsolete form does not accept) trailed
    /// an option anywhere but as the first argument, e.g. `tail -q -5`;
    /// carries the offending character.
    InvalidTrailing(char),
    /// `-c`/`-n` (or their long forms) were given without a value; carries
    /// the option's spelling for the diagnostic.
    MissingValue(&'static str),
    /// `--pid` was given a value that is not a valid process id; carries the
    /// text.
    InvalidPid(String),
    /// `--sleep-interval` was given a value that is not a valid non-negative
    /// number of seconds; carries the text.
    InvalidSleep(String),
    /// `--max-unchanged-stats` was given a value that is not a valid count;
    /// carries the text.
    InvalidMaxUnchanged(String),
    /// Writing to standard output (or the diagnostic stream) failed.
    /// Carries the underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for TailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLong(token) => write!(f, "unrecognized option '{token}'"),
            Self::UnknownShort(flag) => write!(f, "invalid option -- '{flag}'"),
            Self::InvalidLines(text) => write!(f, "invalid number of lines: '{text}'"),
            Self::InvalidBytes(text) => write!(f, "invalid number of bytes: '{text}'"),
            Self::InvalidTrailing(flag) => write!(f, "invalid trailing option -- {flag}"),
            Self::MissingValue(option) => {
                write!(f, "option '{option}' requires an argument")
            }
            Self::InvalidPid(text) => write!(f, "invalid PID: '{text}'"),
            Self::InvalidSleep(text) => write!(f, "invalid number of seconds: '{text}'"),
            Self::InvalidMaxUnchanged(text) => {
                write!(
                    f,
                    "invalid maximum number of unchanged stats between opens: '{text}'"
                )
            }
            Self::Output(errno) => write!(f, "write error: {errno}"),
        }
    }
}
