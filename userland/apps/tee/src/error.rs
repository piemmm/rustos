//! The outcomes of parsing and running a `tee` command.

use alloc::string::String;
use core::fmt;
use tairix_abi::Errno;

/// Why a `tee` invocation did not complete.
///
/// Usage failures mirror the GNU tool's diagnostics; the runtime failure
/// leans on the frozen [`Errno`] for the wire-level cause so it invents no
/// parallel error set. A *per-output* open or write failure is not a
/// variant here: the client reports it on the diagnostic stream and
/// continues (or stops) per the selected output-error mode, exactly as the
/// GNU tool does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeeError {
    /// An unrecognised long option; carries the token.
    UnknownLong(String),
    /// An unrecognised short option; carries the flag character.
    UnknownShort(char),
    /// The `--output-error` value was not one of `warn`, `warn-nopipe`,
    /// `exit`, `exit-nopipe`; carries the text.
    InvalidMode(String),
    /// Writing the diagnostic stream failed. Carries the underlying
    /// [`Errno`]. The tool never continues silently past an unreportable
    /// failure.
    Output(Errno),
}

impl fmt::Display for TeeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLong(token) => write!(f, "unrecognized option '{token}'"),
            Self::UnknownShort(flag) => write!(f, "invalid option -- '{flag}'"),
            Self::InvalidMode(text) => {
                write!(f, "invalid argument '{text}' for '--output-error'")
            }
            Self::Output(errno) => write!(f, "write error: {errno}"),
        }
    }
}
