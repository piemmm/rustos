//! The directory entries the browser navigates and renders.
//!
//! An [`Entry`] is one child of the directory currently shown: its name, what
//! kind of thing it is, and the display metadata a file manager needs — its
//! byte size and its last-modification instant. This is the *userland* listing
//! vocabulary — what an application sees once the VFS has resolved a path and
//! authorised the read — not the driver↔VFS structural surface
//! (`tairix_abi::driver::filesystem`), which belongs to a different layer.
//!
//! The metadata comes straight from the one `fs_readdir` stream the source
//! already produced (each [`DirEntry`](tairix_abi::fs::DirEntry) carries its
//! child's `size` and modification `Time64`), so the browser never opens and
//! stats every child to fill a listing.

use alloc::string::String;

use tairix_abi::time::Time64;

/// What an [`Entry`] is, from the browser's point of view.
///
/// The VFS itself distinguishes only directories and regular files
/// ([`FileKind`](tairix_abi::fs::FileKind)); the browser refines that with the
/// one distinction a file manager must make structurally: an application
/// [`Bundle`](Self::Bundle) — a `<Name>.app` directory — is a *sealed unit*
/// the user launches, not a folder to descend into (the charter's bundle
/// rule). The engine only *models* the distinction here; deciding what happens
/// when a bundle is activated is the launching layer's job.
///
/// Any further special-file kind the VFS may one day surface arrives as a new
/// variant rather than by overloading these.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum EntryKind {
    /// A directory the browser can descend into.
    Directory,
    /// A regular file.
    File,
    /// A `<Name>.app` application bundle: a directory on disk, but a sealed
    /// unit the user launches rather than a folder to descend into.
    Bundle,
}

impl EntryKind {
    /// The kind the browser shows for a listed child, given whether the VFS
    /// reported it as a directory and its name.
    ///
    /// A directory whose name is a `<Name>.app` bundle
    /// ([`is_bundle_name`]) is a [`Bundle`](Self::Bundle); any other
    /// directory is a [`Directory`](Self::Directory); anything else is a
    /// [`File`](Self::File). This is the one classifier both the file manager
    /// and the trusted picker share, so the two can never disagree about what
    /// a listed name *is*.
    #[must_use]
    pub fn for_listing(is_directory: bool, name: &str) -> Self {
        if is_directory {
            if is_bundle_name(name) {
                Self::Bundle
            } else {
                Self::Directory
            }
        } else {
            Self::File
        }
    }

    /// `true` if the entry is a directory the browser can descend into.
    ///
    /// A [`Bundle`](Self::Bundle) is *not* such a directory: it is a sealed
    /// unit, so this is `false` for it even though it is directory-backed on
    /// disk.
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }

    /// `true` if the entry is an application bundle.
    #[must_use]
    pub const fn is_bundle(self) -> bool {
        matches!(self, Self::Bundle)
    }

    /// `true` if the entry is directory-backed on disk — a plain
    /// [`Directory`](Self::Directory) *or* a [`Bundle`](Self::Bundle).
    ///
    /// Unlike [`is_directory`](Self::is_directory), this reports the node's
    /// *structural* shape rather than whether the browser descends into it, so
    /// a management verb removes or recurses into a bundle as the directory it
    /// really is: it selects the
    /// [`DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY) unlink flag and
    /// recursion for either kind, while a [`File`](Self::File) is a leaf.
    #[must_use]
    pub const fn is_directory_backed(self) -> bool {
        matches!(self, Self::Directory | Self::Bundle)
    }
}

/// Whether `name` is an application bundle name — a non-empty base name
/// followed by the `.app` suffix (`Example.app`, but not the bare `.app`).
///
/// The suffix match is ASCII-case-insensitive so a bundle reads as one
/// however the volume cased its extension.
#[must_use]
pub fn is_bundle_name(name: &str) -> bool {
    const SUFFIX: &str = ".app";
    name.len() > SUFFIX.len()
        && name
            .get(name.len() - SUFFIX.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(SUFFIX))
}

/// One child of the directory currently shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    name: String,
    kind: EntryKind,
    size: u64,
    modified: Time64,
}

impl Entry {
    /// Construct an entry from its name, kind, and display metadata.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: EntryKind, size: u64, modified: Time64) -> Self {
        Self {
            name: name.into(),
            kind,
            size,
            modified,
        }
    }

    /// A directory entry named `name`, with no metadata (size `0`, modified at
    /// the epoch) — the convenient form for tests and callers that only model
    /// structure.
    #[must_use]
    pub fn directory(name: impl Into<String>) -> Self {
        Self::new(name, EntryKind::Directory, 0, Time64::UNIX_EPOCH)
    }

    /// A regular-file entry named `name`, with no metadata (see
    /// [`directory`](Self::directory)).
    #[must_use]
    pub fn file(name: impl Into<String>) -> Self {
        Self::new(name, EntryKind::File, 0, Time64::UNIX_EPOCH)
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

    /// The entry's apparent size in bytes; `0` for a directory or bundle.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The entry's last contents-modification instant, as the mounted format
    /// stores it (a backing that keeps no per-node stamp reports
    /// [`Time64::UNIX_EPOCH`], never a fabricated wall time).
    #[must_use]
    pub const fn modified(&self) -> Time64 {
        self.modified
    }

    /// `true` if the entry is a directory the browser can descend into (never
    /// true for a [`Bundle`](EntryKind::Bundle)).
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.kind.is_directory()
    }

    /// `true` if the entry is an application bundle.
    #[must_use]
    pub const fn is_bundle(&self) -> bool {
        self.kind.is_bundle()
    }

    /// `true` if the entry is directory-backed on disk — a directory or a
    /// bundle (see [`EntryKind::is_directory_backed`]).
    #[must_use]
    pub const fn is_directory_backed(&self) -> bool {
        self.kind.is_directory_backed()
    }
}
