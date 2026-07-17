//! The outcomes of running a `du` command.

use core::fmt;
use tairix_abi::Errno;

/// Why a `du` invocation did not complete.
///
/// Only a failure of the tool's own machinery is fatal: a path that
/// cannot be statted or a directory that cannot be read is diagnosed on
/// standard error and the walk continues (the GNU behaviour), surfacing
/// as a `false` clean flag rather than an error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DuError {
    /// A usage row (or the short help) could not be written to the
    /// terminal, carrying the [`Errno`] the stream raised.
    Output(Errno),
}

impl fmt::Display for DuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output(errno) => write!(f, "cannot write output: {errno}"),
        }
    }
}
