//! The outcomes of running the `edit` session.

use core::fmt;

use tairix_curses::CursesError;

/// Why an `edit` session ended other than by the user leaving the editor.
///
/// File failures never end the session: a refused load or save is reported
/// on the status line and the user keeps their buffer. Only a broken
/// terminal — the session's one irreplaceable channel — or a command line
/// the tool does not understand is fatal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditError {
    /// The command line carried an unrecognised option or too many
    /// operands. The caller should print [`crate::USAGE`]. The editor
    /// never starts.
    Usage,
    /// Drawing to or reading from the terminal failed.
    Terminal(CursesError),
}

impl From<CursesError> for EditError {
    fn from(err: CursesError) -> Self {
        Self::Terminal(err)
    }
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("invalid usage"),
            Self::Terminal(err) => write!(f, "terminal error: {err:?}"),
        }
    }
}
