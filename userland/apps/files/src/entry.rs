//! The directory entries the browser navigates and renders.
//!
//! An [`Entry`] is one child of the directory currently shown: its name and
//! whether it is a directory (which the user can descend into) or a regular
//! file. This is the *userland* listing vocabulary — what an application sees
//! once the VFS has resolved a path and authorised the read — not the
//! driver↔VFS structural surface (`rustos_abi::driver::filesystem`), which
//! belongs to a different layer (`AGENTS.md` §5.4 / §17.4).

use alloc::string::String;

/// Whether an [`Entry`] is a directory or a regular file.
///
/// Only the two kinds the browser distinguishes are modelled; special-file
/// kinds, when the VFS surfaces them, arrive as a new variant rather than by
/// overloading these (`AGENTS.md` §2.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum EntryKind {
    /// A directory the browser can descend into.
    Directory,
    /// A regular file.
    File,
}

impl EntryKind {
    /// `true` if the entry is a directory.
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// One child of the directory currently shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    name: String,
    kind: EntryKind,
}

impl Entry {
    /// Construct an entry from its name and kind.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: EntryKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }

    /// A directory entry named `name`.
    #[must_use]
    pub fn directory(name: impl Into<String>) -> Self {
        Self::new(name, EntryKind::Directory)
    }

    /// A regular-file entry named `name`.
    #[must_use]
    pub fn file(name: impl Into<String>) -> Self {
        Self::new(name, EntryKind::File)
    }

    /// The entry's name (a single path component, no separator).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The entry's kind.
    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    /// `true` if the entry is a directory the browser can descend into.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.kind.is_directory()
    }
}
