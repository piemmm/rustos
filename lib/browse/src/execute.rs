//! The pure execution model behind the management verbs
//! (`plans/NEW-FILEMANAGER.md` FM7b): how a planned paste is *carried out*.
//!
//! Where [`clipboard`](crate::clipboard) decides *what* moves where (a
//! [`PastePlan`](crate::clipboard::PastePlan) of source→dest items), this
//! module decides *how* each item is executed and *how a byte-for-byte copy is
//! streamed* — both as pure, host-provable models that touch no filesystem.
//! The `files.app` `Run` binary performs the actual `fs_rename` /
//! `fs_read`→`fs_write` / `fs_unlink` under the user's own identity in its
//! own capability-checked tail (§4, §5.4); the engine only names the plan, so
//! composing it grants nothing and the read-only picker never runs it.
//!
//! # Move vs. copy
//!
//! A move within one volume is a single [`fs_rename`]; a move *across* volumes
//! cannot rename and must copy-then-delete, exactly as `mv` decides from
//! `st_dev`. [`paste_strategy`] makes that decision from the clipboard
//! operation and the two items' [`VolumeId`]s, so the file manager and any
//! future caller share one definition of it (§2.2).
//!
//! # Bounded, interruptible copy
//!
//! A copy never holds an unbounded buffer and never spins (§2.23): the
//! [`CopyCursor`] walks a known-length source in fixed [`COPY_CHUNK_LEN`]
//! steps, yielding the next [`CopyChunk`] to transfer. The app reads and
//! writes that chunk, then reports the bytes actually carried with
//! [`CopyCursor::advance`]; between chunks it may cancel or be preempted, and
//! the cursor resumes from exactly where it stopped (see
//! [`CopyCursor::resume`]). It is fail closed: advancing past the source's
//! length — a source that shrank or a miscounted transfer — is
//! [`CopyError::Overrun`], never a silent wrap.
//!
//! # Recursive directory copy
//!
//! Where the [`CopyCursor`] streams one *file*, a [`CopyWalk`] copies a whole
//! *tree*: the copy-side analogue of the delete-side
//! [`DeleteWalk`](crate::delete::DeleteWalk). It drives a depth-first
//! traversal that creates a destination directory *before* streaming its
//! contents into it (the mirror of the delete walk removing contents before
//! their container), keeps its own explicit stack so a deep tree cannot
//! overflow the call stack, is bounded by [`MAX_COPY_DEPTH`], and holds its
//! exact position between steps so the app can cancel or be preempted without
//! losing or repeating work (§2.23, §26.6). It does no I/O — the app performs
//! each `fs_mkdir` / `fs_readdir` / byte-streamed copy — so composing it grants
//! nothing and the read-only picker never runs one.
//!
//! [`fs_rename`]: tairix_abi::SyscallNumber::FS_RENAME

use alloc::string::String;
use alloc::vec::Vec;

use crate::clipboard::ClipboardOp;

/// The stable system-wide identity of the mounted volume a path lives on — the
/// 16-byte [`volume`](tairix_abi::fs::FileId::volume) field the app reads from
/// `fs_stat`. Two paths can be moved with a single rename only when their
/// volume ids are equal; otherwise the move must copy then delete.
///
/// The engine only *compares* two ids — it never mints one, and the bytes are
/// opaque to it (the filesystem driver assigns them).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeId([u8; 16]);

impl VolumeId {
    /// The volume id read from a node's `fs_stat` result.
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw 16-byte identifier.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 16] {
        self.0
    }
}

/// How a single planned paste item must be carried out on the filesystem.
///
/// Exhaustive over the two clipboard operations crossed with the same-/
/// different-volume distinction: a `Copy` always streams; a `Cut` renames
/// within a volume and copies-then-deletes across volumes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PasteStrategy {
    /// A same-volume move: one `fs_rename` from the source to the destination.
    /// The source is gone once it succeeds; nothing is streamed.
    Rename,
    /// A cross-volume move: stream-copy the source to the destination, then
    /// remove the source once the copy has fully succeeded (never before, so a
    /// failed copy loses no data, §5.4).
    CopyThenDelete,
    /// A copy: stream-copy the source to the destination and leave the source
    /// in place.
    Copy,
}

/// Decide how to carry out a paste item, given the clipboard `op` and the
/// [`VolumeId`]s of the source and its destination directory.
///
/// * [`ClipboardOp::Copy`] → [`PasteStrategy::Copy`] (the source stays).
/// * [`ClipboardOp::Cut`] on the same volume → [`PasteStrategy::Rename`].
/// * [`ClipboardOp::Cut`] across volumes → [`PasteStrategy::CopyThenDelete`].
#[must_use]
pub fn paste_strategy(op: ClipboardOp, source: VolumeId, dest: VolumeId) -> PasteStrategy {
    match op {
        ClipboardOp::Copy => PasteStrategy::Copy,
        ClipboardOp::Cut if source == dest => PasteStrategy::Rename,
        ClipboardOp::Cut => PasteStrategy::CopyThenDelete,
    }
}

/// The maximum number of bytes one [`CopyChunk`] transfers.
///
/// A fixed I/O transfer bound, not a hardware-scaled capacity (§24.4): it caps
/// the buffer a single copy step reads and writes so a large copy stays
/// interruptible and its memory fixed, exactly as the kernel bounds one
/// `fs_read`/`fs_write` with [`FS_IO_MAX`](tairix_abi::fs::FS_IO_MAX). A copy
/// of any size is handled by iterating chunks, so this never caps a file's
/// length (§26.6). Chosen to match the per-call transfer bound so one chunk is
/// one syscall's worth of work.
pub const COPY_CHUNK_LEN: u64 = tairix_abi::fs::FS_IO_MAX as u64;

/// One bounded step of a streamed copy: the byte range to read from the source
/// and write to the destination.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CopyChunk {
    offset: u64,
    len: u64,
}

impl CopyChunk {
    /// The byte offset into the source (and destination) this chunk begins at.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The number of bytes this chunk transfers — never more than
    /// [`COPY_CHUNK_LEN`].
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// `true` when the chunk transfers no bytes. A [`CopyCursor`] never yields
    /// an empty chunk (it returns `None` when complete); this exists for
    /// callers that hold a [`CopyChunk`] directly.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Why a copy cursor cannot advance — a fail-closed refusal (§5.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CopyError {
    /// An advance (or a resume) would carry the copied count past the source's
    /// total length: the source shrank under the copy, or a transfer reported
    /// more bytes than it moved. Refused rather than wrapping or over-reading.
    Overrun,
}

impl core::fmt::Display for CopyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Overrun => f.write_str("copy advanced past the source length"),
        }
    }
}

/// A resumable, cancelable cursor over a byte-for-byte file copy.
///
/// Models the bounded, interruptible streaming copy without doing any I/O: it
/// tracks how many bytes of a known-length source have been carried and yields
/// the next [`CopyChunk`]. The app performs the read/write for that chunk with
/// its own capability-checked syscalls, then calls [`advance`](Self::advance)
/// with the number of bytes actually transferred; between chunks it may stop
/// (a Cancel) or the task may be preempted, and the cursor picks up exactly
/// where it left off. There is no unbounded buffer and no spin (§2.23).
///
/// A short transfer (fewer bytes than the chunk asked for) simply advances the
/// cursor by that amount and the next chunk covers the remainder; a transfer
/// of zero bytes with data still outstanding is a truncated or failing source
/// the *app* reports and stops on (§26.5) — the cursor makes no progress and
/// never loops on its own.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CopyCursor {
    total: u64,
    copied: u64,
}

impl CopyCursor {
    /// A cursor for copying a source of `total` bytes, starting from the
    /// beginning.
    #[must_use]
    pub const fn new(total: u64) -> Self {
        Self { total, copied: 0 }
    }

    /// A cursor resumed after `copied` bytes of a `total`-byte source have
    /// already been carried — the state persisted across a cancel or a
    /// preemption.
    ///
    /// # Errors
    ///
    /// [`CopyError::Overrun`] when `copied` exceeds `total`.
    pub const fn resume(total: u64, copied: u64) -> Result<Self, CopyError> {
        if copied > total {
            return Err(CopyError::Overrun);
        }
        Ok(Self { total, copied })
    }

    /// The source's total length in bytes.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// The number of bytes copied so far.
    #[must_use]
    pub const fn copied(&self) -> u64 {
        self.copied
    }

    /// The number of bytes still to copy.
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.total - self.copied
    }

    /// `true` once every byte has been copied.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.copied == self.total
    }

    /// The next chunk to transfer, or `None` when the copy is complete.
    ///
    /// The chunk begins at [`copied`](Self::copied) and spans the lesser of
    /// [`COPY_CHUNK_LEN`] and [`remaining`](Self::remaining), so it is always
    /// non-empty until the copy finishes.
    #[must_use]
    pub const fn next_chunk(&self) -> Option<CopyChunk> {
        let remaining = self.remaining();
        if remaining == 0 {
            return None;
        }
        let len = if remaining < COPY_CHUNK_LEN {
            remaining
        } else {
            COPY_CHUNK_LEN
        };
        Some(CopyChunk {
            offset: self.copied,
            len,
        })
    }

    /// Record that `bytes` more bytes were transferred, advancing the cursor.
    ///
    /// # Errors
    ///
    /// [`CopyError::Overrun`] when `bytes` exceeds [`remaining`](Self::remaining)
    /// (a source that shrank, or a miscounted transfer); the cursor is left
    /// unchanged.
    pub fn advance(&mut self, bytes: u64) -> Result<(), CopyError> {
        if bytes > self.remaining() {
            return Err(CopyError::Overrun);
        }
        self.copied += bytes;
        Ok(())
    }
}

/// Why a [`CopyWalk`] cannot take the requested step — a fail-closed refusal
/// (§5.4).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CopyWalkError {
    /// Listing a source directory would name a child deeper than
    /// [`MAX_COPY_DEPTH`] components: refused rather than recursing without
    /// bound (§26.6). The walk is left unchanged.
    TooDeep,
    /// The walk was driven against the wrong action — reporting a directory
    /// [`created`](CopyWalk::created) or [`expand`](CopyWalk::expand)ed when the
    /// current step is not the matching directory step, or
    /// [`copied_file`](CopyWalk::copied_file) when the current step is not a
    /// file, or any of them on a finished walk. Refused rather than silently
    /// corrupting the traversal; the walk is left unchanged.
    OutOfStep,
}

impl core::fmt::Display for CopyWalkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooDeep => f.write_str("directory nested deeper than the copy-recursion bound"),
            Self::OutOfStep => f.write_str("copy walk driven against the wrong step"),
        }
    }
}

/// The deepest directory nesting a single recursive copy will descend, counted
/// in root-first path components.
///
/// The copy-side counterpart of
/// [`MAX_DELETE_DEPTH`](crate::delete::MAX_DELETE_DEPTH): both name the one
/// shared `MAX_WALK_DEPTH` fail-closed recursion bound,
/// held in a single place so the removal and copy walks can never disagree on
/// how far they will descend (§2.2, §26.6). A source tree deeper than this is
/// refused ([`CopyWalkError::TooDeep`]) rather than followed.
pub const MAX_COPY_DEPTH: usize = crate::MAX_WALK_DEPTH;

/// The next step a [`CopyWalk`] requires of its driver.
///
/// Borrowed from the walk's current position; the driver performs the named
/// filesystem operation under the user's own identity and then reports back
/// with [`created`](CopyWalk::created) (after a [`MakeDir`](Self::MakeDir)),
/// [`expand`](CopyWalk::expand) (after a [`List`](Self::List)), or
/// [`copied_file`](CopyWalk::copied_file) (after a
/// [`CopyFile`](Self::CopyFile)).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CopyAction<'a> {
    /// Create this destination directory (an `fs_mkdir`), *before* copying its
    /// contents, so its children have somewhere to land. Report it with
    /// [`created`](CopyWalk::created).
    MakeDir {
        /// The destination directory's root-first absolute component path.
        dest: &'a [String],
    },
    /// List this source directory's children (an `fs_readdir`) and feed them
    /// back with [`expand`](CopyWalk::expand), so each is copied into the
    /// destination directory already created for it.
    List {
        /// The source directory's root-first absolute component path.
        source: &'a [String],
    },
    /// Copy this leaf file's bytes from `source` to `dest` — the app streams
    /// the transfer with its own [`CopyCursor`] and capability-checked
    /// `fs_read`/`fs_write` — then report it with
    /// [`copied_file`](CopyWalk::copied_file).
    CopyFile {
        /// The source file's root-first absolute component path.
        source: &'a [String],
        /// The destination file's root-first absolute component path.
        dest: &'a [String],
    },
    /// Recreate the symbolic link `source` at `dest`: read the target
    /// `source` stores and create a link at `dest` holding the same spelling,
    /// then acknowledge with [`copied_link`](CopyWalk::copied_link).
    ///
    /// The target is copied **verbatim**, unresolved: a relative one still
    /// reads relative to the *new* link's own directory, which is what
    /// duplicating a link means. Nothing is read through the link and nothing
    /// is written through it.
    CopyLink {
        /// The source link's root-first absolute component path.
        source: &'a [String],
        /// The new link's root-first absolute component path.
        dest: &'a [String],
    },
}

/// What a node being copied *is* — the three shapes a copy must tell apart.
///
/// The distinction is load-bearing, not cosmetic: streaming a symbolic link's
/// bytes would silently produce a regular file holding the target's text, and
/// *following* it would copy something the link only points at. A link is
/// therefore copied by being **recreated** with the same stored target.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CopyKind {
    /// A regular file: copied by streaming its bytes.
    File,
    /// A directory (a bundle included): copied by creating the destination and
    /// recursing into the source's children.
    Directory,
    /// A symbolic link: copied by recreating it with the target it stores.
    Link,
}

impl CopyKind {
    /// Whether this kind is copied by creating a destination directory and
    /// recursing.
    const fn recurses(self) -> bool {
        matches!(self, Self::Directory)
    }
}

/// One node still to be copied: its source and destination paths, what it is,
/// and — for a directory — whether its destination has been created yet.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CopyFrame {
    source: Vec<String>,
    dest: Vec<String>,
    kind: CopyKind,
    created: bool,
}

/// A resumable, interruptible cursor over a depth-first recursive copy.
///
/// The copy-side analogue of [`DeleteWalk`](crate::delete::DeleteWalk): where a
/// deletion removes a directory's contents *before* the directory, a copy
/// *creates* the destination directory *before* streaming its contents into it,
/// so a child always has a parent to land in. Built from the resolved
/// source→destination items of a
/// [`PastePlan`](crate::clipboard::PastePlan) (the app supplies each item's
/// `is_directory`, which the path-only clipboard does not carry), it drives the
/// app one step at a time (see [`CopyAction`]): for a directory,
/// [`MakeDir`](CopyAction::MakeDir) then [`List`](CopyAction::List) then a
/// recursion into its children; for a regular file, a single
/// [`CopyFile`](CopyAction::CopyFile).
///
/// It does no I/O — the app performs each `fs_mkdir`, `fs_readdir`, and
/// byte-streamed `fs_read`/`fs_write` with its own capability-checked syscalls
/// under the user's own identity (§4, §5.4) — and it never recurses on the call
/// stack (its own explicit stack cannot overflow), stays within
/// [`MAX_COPY_DEPTH`], and holds its exact position between steps so the app can
/// cancel or be preempted without losing or repeating work (§2.23, §26.6).
/// Composing the walk grants nothing, so the read-only picker never runs one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyWalk {
    stack: Vec<CopyFrame>,
    copied: usize,
}

impl CopyWalk {
    /// Begin a recursive copy of `items`, in order, or `None` when there is
    /// nothing to copy — an empty item list, or any item whose source or
    /// destination names the root (an empty component list).
    ///
    /// Each item is `(source, dest, kind)`: the source's root-first absolute
    /// path, the destination path it takes under the paste target, and what
    /// the source is ([`CopyKind`]). Refusing an empty or root-naming set here
    /// means the copy verb is simply unavailable rather than a silent no-op or
    /// a copy involving the root (§5.4), exactly as
    /// [`DeletePlan::new`](crate::delete::DeletePlan::new) refuses one.
    #[must_use]
    pub fn from_items(items: Vec<(Vec<String>, Vec<String>, CopyKind)>) -> Option<Self> {
        if items.is_empty()
            || items
                .iter()
                .any(|(source, dest, _)| source.is_empty() || dest.is_empty())
        {
            return None;
        }
        let mut stack = Vec::with_capacity(items.len());
        // Push items in reverse so the first item is on top of the stack and the
        // walk processes them in order (mirrors `DeleteWalk::from_plan`).
        for (source, dest, kind) in items.into_iter().rev() {
            stack.push(CopyFrame {
                source,
                dest,
                kind,
                created: false,
            });
        }
        Some(Self { stack, copied: 0 })
    }

    /// The number of nodes copied so far — the honest figure a progress
    /// indicator reports (§2.24). The total is not known in advance (it depends
    /// on the source tree the reads reveal), so progress is a rising count,
    /// never a fabricated percentage.
    #[must_use]
    pub const fn copied(&self) -> usize {
        self.copied
    }

    /// `true` once every reachable node has been copied and nothing remains.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.stack.is_empty()
    }

    /// The next step the driver must take, or `None` when the copy is complete.
    ///
    /// A directory whose destination has not been created yet yields
    /// [`MakeDir`](CopyAction::MakeDir); a directory whose destination exists
    /// but whose contents have not been listed yet yields
    /// [`List`](CopyAction::List); a regular file yields
    /// [`CopyFile`](CopyAction::CopyFile); a symbolic link yields
    /// [`CopyLink`](CopyAction::CopyLink).
    #[must_use]
    pub fn next_action(&self) -> Option<CopyAction<'_>> {
        let top = self.stack.last()?;
        match top.kind {
            CopyKind::File => Some(CopyAction::CopyFile {
                source: &top.source,
                dest: &top.dest,
            }),
            CopyKind::Link => Some(CopyAction::CopyLink {
                source: &top.source,
                dest: &top.dest,
            }),
            CopyKind::Directory if top.created => Some(CopyAction::List {
                source: &top.source,
            }),
            CopyKind::Directory => Some(CopyAction::MakeDir { dest: &top.dest }),
        }
    }

    /// Report that the destination directory the last
    /// [`MakeDir`](CopyAction::MakeDir) named has been created, so the walk
    /// advances to listing its source contents.
    ///
    /// Counts the directory as copied (its destination now exists), so the
    /// [`copied`](Self::copied) figure rises as the tree is built.
    ///
    /// # Errors
    ///
    /// [`CopyWalkError::OutOfStep`] when the current step is not a
    /// [`MakeDir`](CopyAction::MakeDir) — a file, an already-created directory,
    /// or a finished walk; the walk is left unchanged.
    pub fn created(&mut self) -> Result<(), CopyWalkError> {
        match self.stack.last_mut() {
            Some(top) if top.kind.recurses() && !top.created => {
                top.created = true;
                self.copied += 1;
                Ok(())
            }
            _ => Err(CopyWalkError::OutOfStep),
        }
    }

    /// Report the children read for the source directory the last
    /// [`List`](CopyAction::List) named, so each is copied into the destination
    /// directory already created for it.
    ///
    /// `children` is each child's leaf name and what it is, in the source's
    /// listing order; they are copied in that order. An empty listing simply
    /// finishes the (now-known-empty) directory. Reporting the listing
    /// finishes this directory frame and pushes its children, so the directory
    /// itself was already counted by [`created`](Self::created).
    ///
    /// # Errors
    ///
    /// * [`CopyWalkError::TooDeep`] — a child would sit deeper than
    ///   [`MAX_COPY_DEPTH`] components; the walk is left unchanged.
    /// * [`CopyWalkError::OutOfStep`] — the current step is not a
    ///   [`List`](CopyAction::List) (a file, a not-yet-created directory, or a
    ///   finished walk).
    pub fn expand(&mut self, children: &[(String, CopyKind)]) -> Result<(), CopyWalkError> {
        // The listed directory is always the top frame (a `List` step is only
        // yielded for `stack.last()`), so copy out its paths and replace it with
        // its children — the destination is made and the listing is known, so
        // the directory frame has served its purpose.
        let (source, dest) = match self.stack.last() {
            Some(top) if top.kind.recurses() && top.created => {
                if top.source.len() + 1 > MAX_COPY_DEPTH {
                    return Err(CopyWalkError::TooDeep);
                }
                (top.source.clone(), top.dest.clone())
            }
            _ => return Err(CopyWalkError::OutOfStep),
        };
        self.stack.pop();
        // Push children in reverse so they are copied in the source's listing
        // order (the first child ends up nearest the top of the stack).
        for (name, kind) in children.iter().rev() {
            let mut child_source = source.clone();
            child_source.push(name.clone());
            let mut child_dest = dest.clone();
            child_dest.push(name.clone());
            self.stack.push(CopyFrame {
                source: child_source,
                dest: child_dest,
                kind: *kind,
                created: false,
            });
        }
        Ok(())
    }

    /// Report that the file the last [`CopyFile`](CopyAction::CopyFile) named
    /// has been copied, advancing the walk past it and counting it.
    ///
    /// # Errors
    ///
    /// [`CopyWalkError::OutOfStep`] when the current step is not a
    /// [`CopyFile`](CopyAction::CopyFile) — a directory, or a finished walk; the
    /// walk is left unchanged.
    pub fn copied_file(&mut self) -> Result<(), CopyWalkError> {
        self.leaf_done(CopyKind::File)
    }

    /// Report that the link the last [`CopyLink`](CopyAction::CopyLink) named
    /// has been recreated, advancing the walk past it and counting it.
    ///
    /// # Errors
    ///
    /// [`CopyWalkError::OutOfStep`] when the current step is not a
    /// [`CopyLink`](CopyAction::CopyLink) — a file, a directory, or a finished
    /// walk; the walk is left unchanged.
    pub fn copied_link(&mut self) -> Result<(), CopyWalkError> {
        self.leaf_done(CopyKind::Link)
    }

    /// Advance past the top frame when it is a leaf of exactly `kind`, so an
    /// acknowledgement can never be applied to a step of a different shape.
    fn leaf_done(&mut self, kind: CopyKind) -> Result<(), CopyWalkError> {
        match self.stack.last() {
            Some(top) if top.kind == kind => {
                self.stack.pop();
                self.copied += 1;
                Ok(())
            }
            _ => Err(CopyWalkError::OutOfStep),
        }
    }
}
