//! [`VimError`]: the outcomes of the editor session loop.

use core::fmt;

/// Why the editor session ended abnormally. In-session problems (an
/// unwritable file, a failed search) are vim messages on the status line,
/// not errors; this type covers only the failures that end the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VimError {
    /// The terminal failed: a write or read on the session's byte channel
    /// was refused.
    Terminal,
}

impl fmt::Display for VimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VimError::Terminal => write!(f, "terminal error: the display could not be driven"),
        }
    }
}
