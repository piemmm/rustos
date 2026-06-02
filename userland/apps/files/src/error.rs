//! The outcomes of a browser navigation.
//!
//! Every navigation fails closed (`AGENTS.md` §5.4): a refused read or a
//! request for an entry that is not there leaves the browser on the directory
//! it was already showing, and surfaces a [`BrowseError`] rather than
//! guessing, fabricating entries, or partially applying the move.

use core::fmt;

use rustos_abi::Errno;

/// Why a browser navigation did not complete.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BrowseError {
    /// The directory source could not list the target directory. The wrapped
    /// [`Errno`] is the kernel boundary's reason — most often
    /// [`Errno::PermissionDenied`] when the caller lacks the capability to
    /// read the directory (`AGENTS.md` §5.3), or [`Errno::NotFound`] when it
    /// has been removed underneath the browser.
    Source(Errno),
    /// The selected or indexed entry does not exist in the current listing.
    NoSuchEntry,
    /// The target entry is a regular file, not a directory, so the browser
    /// cannot descend into it.
    NotADirectory,
}

impl BrowseError {
    /// The directory-source [`Errno`] behind a [`BrowseError::Source`], if any.
    #[must_use]
    pub const fn source_errno(self) -> Option<Errno> {
        match self {
            Self::Source(errno) => Some(errno),
            _ => None,
        }
    }
}

impl fmt::Display for BrowseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(errno) => write!(f, "directory read failed: {errno}"),
            Self::NoSuchEntry => f.write_str("no such entry"),
            Self::NotADirectory => f.write_str("not a directory"),
        }
    }
}
