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
//! stats every child to fill a listing. A **link** is the one exception: the
//! stream reports it as a link and nothing more, so what it resolves to costs
//! one stat, paid only for the link entries.

use alloc::string::String;

use tairix_abi::fs::FileKind;
use tairix_abi::time::Time64;

/// What a listed symbolic link's target resolves to.
///
/// A link is *two* facts — that it is a link, and what it names — and both
/// matter to a file manager: the user must see the link, and every verb must
/// act on the right object. This is the second fact, so it is never nested
/// inside itself: resolution followed every hop, so a resolved target is
/// never another link.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum LinkTarget {
    /// A directory. The browser descends *through* the link into it.
    Directory,
    /// A regular file.
    File,
    /// A `<Name>.app` application bundle — what a desktop shortcut to an
    /// application names.
    Bundle,
    /// Nothing reachable: the link dangles, or its target could not be
    /// described. Every verb that would act on the target refuses; the link
    /// itself is still shown, renamed, and removed like any other name.
    Dangling,
}

/// What an [`Entry`] is, from the browser's point of view.
///
/// The VFS distinguishes directories, regular files, and symbolic links
/// ([`FileKind`]); the browser refines that with the one distinction a file
/// manager must make structurally: an application [`Bundle`](Self::Bundle) —
/// a `<Name>.app` directory — is a *sealed unit* the user launches, not a
/// folder to descend into (the charter's bundle rule). The engine only
/// *models* the distinction here; deciding what happens when a bundle is
/// activated is the launching layer's job.
///
/// A [`Link`](Self::Link) carries what it resolves to rather than being
/// flattened into it, because a file manager needs both facts at once: it
/// shows the link and acts on the target. Any further special-file kind the
/// VFS may one day surface arrives as a new variant rather than by
/// overloading these.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum EntryKind {
    /// A directory the browser can descend into.
    Directory,
    /// A regular file.
    File,
    /// A `<Name>.app` application bundle: a directory on disk, but a sealed
    /// unit the user launches rather than a folder to descend into.
    Bundle,
    /// A symbolic link, carrying what its target resolves to.
    Link(LinkTarget),
}

/// A listed symbolic link's resolution: what its final target is, and the
/// leaf name of the path the link *stores*.
///
/// The leaf name is what decides bundle-ness, and it has to be the target's:
/// a desktop shortcut is named for the application (`Editor`) while its
/// target is the bundle (`/Apps/Editor.app`), so classifying on the link's own
/// name would show a shortcut to an application as a plain folder.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LinkResolution<'a> {
    /// The kind the target reports.
    pub kind: FileKind,
    /// The leaf name of the target the link stores.
    pub target_name: &'a str,
}

impl EntryKind {
    /// The kind the browser shows for a listed child, given the kind the VFS
    /// reported for it, its name, and — for a link — the resolution of its
    /// final target.
    ///
    /// A directory whose name is a `<Name>.app` bundle ([`is_bundle_name`])
    /// is a [`Bundle`](Self::Bundle); any other directory is a
    /// [`Directory`](Self::Directory); a regular file is a
    /// [`File`](Self::File); and a link is a [`Link`](Self::Link) carrying
    /// the same judgement applied to its *target*. This is the one classifier
    /// both the file manager and the trusted picker share, so the two can
    /// never disagree about what a listed name *is*.
    ///
    /// A link with no `resolution` — because it dangles, or because the
    /// caller could not describe its target — is
    /// [`LinkTarget::Dangling`]: the honest answer, never a guess at the
    /// target's kind.
    #[must_use]
    pub fn for_listing(kind: FileKind, name: &str, resolution: Option<LinkResolution<'_>>) -> Self {
        match kind {
            FileKind::Directory => Self::directory_or_bundle(name),
            FileKind::Regular => Self::File,
            FileKind::Symlink => Self::Link(match resolution {
                None => LinkTarget::Dangling,
                Some(resolved) => match resolved.kind {
                    FileKind::Directory => match Self::directory_or_bundle(resolved.target_name) {
                        Self::Bundle => LinkTarget::Bundle,
                        _ => LinkTarget::Directory,
                    },
                    FileKind::Regular => LinkTarget::File,
                    // Resolution follows every hop, so a target that is
                    // itself a link is a backing this classifier will not
                    // trust to name anything.
                    FileKind::Symlink => LinkTarget::Dangling,
                },
            }),
        }
    }

    /// A directory-backed name's kind: the bundle it is named as, or a plain
    /// directory.
    fn directory_or_bundle(name: &str) -> Self {
        if is_bundle_name(name) {
            Self::Bundle
        } else {
            Self::Directory
        }
    }

    /// `true` if the entry is a directory the browser can descend into,
    /// through a link if that is what it takes.
    ///
    /// A [`Bundle`](Self::Bundle) is *not* such a directory: it is a sealed
    /// unit, so this is `false` for it even though it is directory-backed on
    /// disk. A link *to* a directory is one: descending it opens the
    /// directory the link names, spelled through the link.
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory | Self::Link(LinkTarget::Directory))
    }

    /// `true` if the entry is an application bundle, or a link to one.
    #[must_use]
    pub const fn is_bundle(self) -> bool {
        matches!(self, Self::Bundle | Self::Link(LinkTarget::Bundle))
    }

    /// `true` if the entry is a symbolic link, whatever it resolves to.
    #[must_use]
    pub const fn is_link(self) -> bool {
        matches!(self, Self::Link(_))
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
    ///
    /// A [`Link`](Self::Link) is **never** directory-backed, however its
    /// target resolves: a link is a leaf name, so removing one unlinks the
    /// link and recursing into one would walk a tree the name only points at.
    #[must_use]
    pub const fn is_directory_backed(self) -> bool {
        matches!(self, Self::Directory | Self::Bundle)
    }

    /// The kind this entry *behaves* as: a link's resolved target, and
    /// otherwise the entry itself.
    ///
    /// This is the reading a *content* decision wants — which icon to draw,
    /// which application opens it, where it sorts — as against
    /// [`is_directory_backed`](Self::is_directory_backed), which is the
    /// *structural* reading a management verb wants. A dangling link behaves
    /// as nothing, so this reports [`None`] rather than a guess.
    #[must_use]
    pub const fn resolved(self) -> Option<Self> {
        match self {
            Self::Directory | Self::File | Self::Bundle => Some(self),
            Self::Link(LinkTarget::Directory) => Some(Self::Directory),
            Self::Link(LinkTarget::File) => Some(Self::File),
            Self::Link(LinkTarget::Bundle) => Some(Self::Bundle),
            Self::Link(LinkTarget::Dangling) => None,
        }
    }
}

/// The absolute path a link's stored `target` names, given the directory the
/// link itself sits in.
///
/// An absolute target names itself; a relative one is *appended* to the link's
/// own directory. Nothing is normalised or collapsed: a `..` in the result is
/// left for the kernel to resolve **physically**, against the nodes the walk
/// really passes through, because collapsing it here would name a different
/// object the moment another link were involved (`plans/SYMLINKS.md`
/// decision 4).
#[must_use]
pub fn resolve_target(link_directory: &str, target: &str) -> String {
    if target.starts_with('/') {
        return String::from(target);
    }
    tairix_path::join(link_directory, target)
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

/// Whether a listed directory holds anything, as far as the browser knows.
///
/// No VFS surface reports a child count — a directory's `size` is `0` and
/// there is no link count — so occupancy is only knowable by *reading* the
/// directory. That read is a syscall the browser must not make for every
/// listed child, so the state is four-valued and honest rather than a `bool`
/// that would have to guess: a child starts [`Unprobed`](Self::Unprobed), and
/// a probe that is refused or fails records
/// [`Indeterminate`](Self::Indeterminate) so the refusal is never retried.
///
/// Only a plain directory has a meaningful occupancy: a bundle is a sealed
/// unit that draws its own icon and a file has no children, so neither is
/// ever probed.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum Occupancy {
    /// Not yet asked — the entry has never been probed.
    #[default]
    Unprobed,
    /// The directory was read and has no children.
    Empty,
    /// The directory was read and has at least one child.
    NonEmpty,
    /// The probe was refused or failed; the answer is unknown and the entry
    /// is not probed again.
    Indeterminate,
}

/// One child of the directory currently shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    name: String,
    kind: EntryKind,
    target: Option<String>,
    size: u64,
    modified: Time64,
    occupancy: Occupancy,
}

impl Entry {
    /// Construct an entry from its name, kind, and display metadata.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: EntryKind, size: u64, modified: Time64) -> Self {
        Self {
            name: name.into(),
            kind,
            target: None,
            size,
            modified,
            occupancy: Occupancy::Unprobed,
        }
    }

    /// This entry with the target a symbolic link stores attached.
    ///
    /// The target is the spelling the link holds, **verbatim**: it may be
    /// relative and may name nothing, so it is display and launch input, never
    /// something this engine resolves. It rides beside the kind rather than
    /// inside [`EntryKind`] so that kind stays a small `Copy` value every
    /// surface passes around; only the entry itself owns the string.
    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
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

    /// The target a symbolic link stores, verbatim, or [`None`] for an entry
    /// that is not one.
    ///
    /// The spelling is what the link holds — possibly relative, possibly
    /// naming nothing — so a consumer that must *reach* it resolves it
    /// against the link's own directory ([`resolve_target`]) rather than
    /// treating it as a path in its own right.
    #[must_use]
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
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

    /// `true` if the entry is an application bundle, or a link to one.
    #[must_use]
    pub const fn is_bundle(&self) -> bool {
        self.kind.is_bundle()
    }

    /// `true` if the entry is a symbolic link, whatever it resolves to.
    #[must_use]
    pub const fn is_link(&self) -> bool {
        self.kind.is_link()
    }

    /// `true` if the entry is directory-backed on disk — a directory or a
    /// bundle (see [`EntryKind::is_directory_backed`]).
    #[must_use]
    pub const fn is_directory_backed(&self) -> bool {
        self.kind.is_directory_backed()
    }

    /// What the browser knows about this entry's contents.
    #[must_use]
    pub const fn occupancy(&self) -> Occupancy {
        self.occupancy
    }

    /// Record the answer a probe gave for this entry.
    pub const fn set_occupancy(&mut self, occupancy: Occupancy) {
        self.occupancy = occupancy;
    }

    /// `true` if this entry is a plain directory whose occupancy is still
    /// unknown — the one shape [`Browser::resolve_occupancy`] probes.
    ///
    /// [`Browser::resolve_occupancy`]: crate::Browser::resolve_occupancy
    #[must_use]
    pub const fn needs_occupancy_probe(&self) -> bool {
        self.is_directory() && matches!(self.occupancy, Occupancy::Unprobed)
    }
}
