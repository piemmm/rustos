//! The file-operation planner and executor over the [`Fs`] seam.
//!
//! Every mutation the panes trigger — copy, move, rename, delete, mkdir —
//! is planned and validated here before any I/O, then driven by the
//! resumable [`FileOp`] executor so a per-file overwrite question can pause
//! the work, hand the decision to the key loop, and continue. The executor
//! streams file bytes through a bounded buffer (never a whole-file slurp),
//! reads directories incrementally as it reaches them, and fails closed:
//! the first refused step aborts the operation with the kernel's own
//! [`Errno`], and a partially written file is removed rather than left
//! looking complete.
//!
//! # A symbolic link is a name, not the thing it names
//!
//! Every verb acts on the link: a copy **recreates** it with the same stored
//! target (streaming its bytes would leave a regular file holding the
//! target's text), and a move or a delete acts on the link while what it
//! named survives. A link already sitting at a destination is asked about and
//! then **removed** before the new object is created, because a create or
//! truncate follows a final link and would otherwise act on whatever it
//! pointed at — which is why the destination probe never follows one
//! ([`crate::fs::Fs::stat_kind`]).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_abi::{Errno, FileKind};

use crate::fs::{Fs, RenameOutcome};
use crate::model::join;

/// The fixed-size chunk used to stream a regular file, matching the
/// granularity the other userland tools share.
const READ_CHUNK: usize = 4096;

/// Why a planned operation was refused before any I/O, or how it failed
/// mid-flight. Rendered on the message line; nothing was changed unless
/// the variant says otherwise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpError {
    /// The typed destination could not be parsed as a path.
    BadPath(String),
    /// Source and destination name the same entry.
    SamePath,
    /// The destination lies inside the source directory's own subtree.
    IntoItself,
    /// The destination exists as a directory where a file must go (or the
    /// reverse); carries the blocking path.
    KindMismatch(String),
    /// A filesystem step was refused; carries the path and the kernel's
    /// [`Errno`]. When mid-copy, the partial target has been removed.
    Fs(String, Errno),
}

impl OpError {
    /// The one-line report the message line shows.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::BadPath(reason) => format!("bad path: {reason}"),
            Self::SamePath => String::from("source and destination are the same entry"),
            Self::IntoItself => String::from("cannot copy or move a directory into itself"),
            Self::KindMismatch(path) => format!("{path}: exists with a different kind"),
            Self::Fs(path, errno) => format!("{path}: {errno:?}"),
        }
    }
}

/// Normalise the destination the user typed, against the directory the
/// file pane lists. A relative spelling is joined onto `base`; an absolute
/// one (view- or alias-rooted) stands alone. The result is the canonical
/// spelling `lib/path` prints — the kernel stays the one authoriser.
///
/// # Errors
///
/// [`OpError::BadPath`] when the spelling fails the shared path grammar.
pub fn resolve_destination(base: &str, typed: &str) -> Result<String, OpError> {
    let parsed = tairix_path::parse(typed).map_err(|e| OpError::BadPath(e.to_string()))?;
    if parsed.is_absolute() {
        return Ok(parsed.to_string());
    }
    let joined = join(base, typed);
    let parsed = tairix_path::parse(&joined).map_err(|e| OpError::BadPath(e.to_string()))?;
    Ok(parsed.to_string())
}

/// Whether `path` lies strictly inside the directory `ancestor` (component
/// boundary respected; equal paths are not "inside").
#[must_use]
pub fn is_inside(ancestor: &str, path: &str) -> bool {
    let ancestor = ancestor.trim_end_matches('/');
    match path.strip_prefix(ancestor) {
        Some(rest) => rest.starts_with('/') && rest.len() > 1,
        None => false,
    }
}

/// The final component of `path` (`/a/b` names `b`; a bare root names
/// itself).
#[must_use]
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return path;
    }
    match trimmed.rfind('/') {
        Some(slash) => &trimmed[slash + 1..],
        None => trimmed,
    }
}

/// The directory `path`'s final component lives in (`/a/b` maps to `/a`;
/// a top-level entry maps to `/`).
#[must_use]
pub fn parent_of(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(slash) => &trimmed[..slash],
        None => trimmed,
    }
}

/// Probe `path`, mapping a missing entry to `None` so a destination can be
/// checked for existence without treating absence as failure.
///
/// # Errors
///
/// Any non-[`Errno::NotFound`] refusal, as [`OpError::Fs`].
fn probe(fs: &mut dyn Fs, path: &str) -> Result<Option<FileKind>, OpError> {
    match fs.stat_kind(path) {
        Ok(kind) => Ok(Some(kind)),
        Err(Errno::NotFound) => Ok(None),
        Err(errno) => Err(OpError::Fs(String::from(path), errno)),
    }
}

/// Resolve where a transfer of `src` must land, given the normalised
/// destination the user typed: an existing directory receives the source
/// *inside* it under its base name; anything else is the target itself.
/// Refuses — before any I/O — a target equal to the source or inside the
/// source's own subtree.
///
/// # Errors
///
/// [`OpError::SamePath`], [`OpError::IntoItself`], or a probe failure.
pub fn plan_target(
    fs: &mut dyn Fs,
    src: &str,
    src_kind: FileKind,
    dst: &str,
) -> Result<String, OpError> {
    let target = match probe(fs, dst)? {
        Some(FileKind::Directory) => join(dst, basename(src)),
        _ => String::from(dst),
    };
    if target.trim_end_matches('/') == src.trim_end_matches('/') {
        return Err(OpError::SamePath);
    }
    if src_kind == FileKind::Directory && is_inside(src, &target) {
        return Err(OpError::IntoItself);
    }
    Ok(target)
}

/// Whether a paused [`FileOp`] transfers or deletes, for the prompts and
/// the completion report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OpKind {
    /// Reproduce the source at the target, leaving the source in place.
    Copy,
    /// Reproduce the source at the target, then remove the source (the
    /// cross-volume fallback; a same-volume move renames instead).
    Move,
    /// Remove the source and, for a directory, everything under it.
    Delete,
}

/// A target name that is already taken, awaiting the user's per-entry
/// decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Conflict {
    /// The source entry whose transfer is paused.
    pub src: String,
    /// The existing name the transfer would replace.
    pub dst: String,
    /// What that name currently holds, so the question names it: replacing a
    /// symbolic link is a different thing from overwriting a file's bytes,
    /// and the user is told which they are agreeing to.
    pub occupant: FileKind,
}

/// The user's answer to a [`Conflict`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Overwrite the existing target and continue.
    Overwrite,
    /// Leave the existing target (and, on a move, the source) and continue.
    Skip,
    /// Stop the operation; steps already applied stay applied.
    Cancel,
}

/// What one [`FileOp::advance`] call achieved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpProgress {
    /// Every step completed; the operation is finished.
    Done,
    /// A [`Conflict`] awaits a [`Decision`] through [`FileOp::resolve`].
    NeedsDecision,
    /// A step failed; the operation stopped with this error.
    Failed(OpError),
}

/// One queued unit of work, processed last-in-first-out so a directory's
/// children complete before the step queued beneath them.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Work {
    /// Rename one entry atomically, falling back to a transfer when the
    /// volumes differ. `approved` carries what the user already agreed to
    /// replace — the destination is probed *before* the rename, so an
    /// existing name is asked about, never silently replaced.
    Rename {
        src: String,
        kind: FileKind,
        dst: String,
        approved: Option<FileKind>,
    },
    /// Copy one entry to `dst` (recursing into a directory by pushing its
    /// children, recreating a symbolic link); on a move, a copied entry's
    /// source is removed in the same step. `approved` carries what the user
    /// already agreed to replace.
    Transfer {
        src: String,
        kind: FileKind,
        dst: String,
        approved: Option<FileKind>,
    },
    /// Remove one entry (expanding a directory into child removals plus a
    /// directory removal beneath them).
    Remove { path: String, kind: FileKind },
    /// Remove one directory the steps queued above it emptied.
    RemoveEmptiedDir { path: String },
}

/// A resumable file operation: a work stack advanced until it finishes,
/// fails, or pauses on a [`Conflict`]. Directories are read incrementally
/// as the walk reaches them (memory follows the frontier, never the
/// subtree), and a paused operation holds no filesystem handle — only the
/// paths still to process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileOp {
    kind: OpKind,
    stack: Vec<Work>,
    /// The paused question and the pre-approved work item an
    /// [`Decision::Overwrite`] pushes back.
    conflict: Option<(Conflict, Work)>,
    /// Sources the user skipped; an emptied-source-dir removal above a
    /// skipped file is silently withheld so a skip never fails the move.
    skipped_srcs: Vec<String>,
    /// Entries completed (files copied/renamed/removed, directories
    /// removed), for the completion report.
    done: usize,
    /// Conflicts the user skipped, for the completion report.
    skipped: usize,
    /// Whether the user cancelled the remaining steps.
    cancelled: bool,
}

impl FileOp {
    /// A copy of `src` (of `kind`) to the planned `target`.
    #[must_use]
    pub fn copy(src: &str, kind: FileKind, target: &str) -> Self {
        Self::new(
            OpKind::Copy,
            Work::Transfer {
                src: String::from(src),
                kind,
                dst: String::from(target),
                approved: None,
            },
        )
    }

    /// A move of `src` (of `kind`) to the planned `target`: an atomic
    /// rename where the volume allows it, the copy-then-remove fallback
    /// where it does not.
    #[must_use]
    pub fn move_to(src: &str, kind: FileKind, target: &str) -> Self {
        Self::new(
            OpKind::Move,
            Work::Rename {
                src: String::from(src),
                kind,
                dst: String::from(target),
                approved: None,
            },
        )
    }

    /// A delete of `path` (of `kind`); a directory is removed with
    /// everything under it, children before parents.
    #[must_use]
    pub fn delete(path: &str, kind: FileKind) -> Self {
        Self::new(
            OpKind::Delete,
            Work::Remove {
                path: String::from(path),
                kind,
            },
        )
    }

    fn new(kind: OpKind, first: Work) -> Self {
        Self {
            kind,
            stack: alloc::vec![first],
            conflict: None,
            skipped_srcs: Vec::new(),
            done: 0,
            skipped: 0,
            cancelled: false,
        }
    }

    /// The operation's kind, for the prompts and reports.
    #[must_use]
    pub fn kind(&self) -> OpKind {
        self.kind
    }

    /// The paused overwrite question, when [`FileOp::advance`] reported
    /// [`OpProgress::NeedsDecision`].
    #[must_use]
    pub fn conflict(&self) -> Option<&Conflict> {
        self.conflict.as_ref().map(|(conflict, _)| conflict)
    }

    /// Whether the user cancelled the operation's remaining steps at an
    /// overwrite question; work already applied stayed applied. The batch
    /// driver reads this to tell a cancelled entry from a completed one.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Answer the paused question: overwrite re-queues the approved step,
    /// skip withholds it (and any removal of its emptied ancestors), and
    /// cancel drops every remaining step — work already applied stays.
    pub fn resolve(&mut self, decision: Decision) {
        let Some((conflict, approved)) = self.conflict.take() else {
            return;
        };
        match decision {
            Decision::Overwrite => self.stack.push(approved),
            Decision::Skip => {
                self.skipped += 1;
                self.skipped_srcs.push(conflict.src);
            }
            Decision::Cancel => {
                self.cancelled = true;
                self.stack.clear();
            }
        }
    }

    /// Drive the work stack until it empties, pauses on a conflict, or
    /// fails. Call again after [`FileOp::resolve`].
    pub fn advance(&mut self, fs: &mut dyn Fs) -> OpProgress {
        if self.conflict.is_some() {
            return OpProgress::NeedsDecision;
        }
        while let Some(work) = self.stack.pop() {
            let step = match work {
                Work::Rename {
                    src,
                    kind,
                    dst,
                    approved,
                } => self.step_rename(fs, src, kind, dst, approved),
                Work::Transfer {
                    src,
                    kind,
                    dst,
                    approved,
                } => self.step_transfer(fs, src, kind, dst, approved),
                Work::Remove { path, kind } => self.step_remove(fs, &path, kind),
                Work::RemoveEmptiedDir { path } => self.step_remove_emptied(fs, &path),
            };
            match step {
                Ok(()) => {}
                Err(StepEnd::Paused) => return OpProgress::NeedsDecision,
                Err(StepEnd::Failed(error)) => return OpProgress::Failed(error),
            }
        }
        OpProgress::Done
    }

    /// The one-line completion report for the message line.
    #[must_use]
    pub fn summary(&self) -> String {
        let verb = match self.kind {
            OpKind::Copy => "copied",
            OpKind::Move => "moved",
            OpKind::Delete => "deleted",
        };
        let noun = if self.done == 1 { "entry" } else { "entries" };
        let mut text = format!("{verb} {} {noun}", self.done);
        if self.skipped > 0 {
            // Writing to a `String` cannot fail.
            let _ = write!(text, ", {} skipped", self.skipped);
        }
        if self.cancelled {
            text.push_str(" (cancelled)");
        }
        text
    }

    fn step_rename(
        &mut self,
        fs: &mut dyn Fs,
        src: String,
        kind: FileKind,
        dst: String,
        approved: Option<FileKind>,
    ) -> Result<(), StepEnd> {
        if approved.is_none() {
            // A rename replaces an existing destination atomically when the
            // two are kind-compatible, so *any* occupied leaf name is asked
            // about — a link included, because replacing one is a real loss
            // the user must agree to. A directory target was already
            // redirected into by the planner; whatever remains (dir over
            // file, dir over dir) is the kernel's to judge, and the rename
            // below surfaces its refusal unchanged.
            let occupant = probe(fs, &dst).map_err(StepEnd::Failed)?;
            if let Some(occupant) = occupant.filter(|held| replaces_leaf(kind, *held)) {
                return Err(self.pause(
                    src.clone(),
                    dst.clone(),
                    occupant,
                    Work::Rename {
                        src,
                        kind,
                        dst,
                        approved: Some(occupant),
                    },
                ));
            }
        }
        match fs.rename(&src, &dst) {
            Ok(RenameOutcome::Renamed) => {
                self.done += 1;
                Ok(())
            }
            Ok(RenameOutcome::CrossDevice) => {
                self.stack.push(Work::Transfer {
                    src,
                    kind,
                    dst,
                    approved,
                });
                Ok(())
            }
            Err(errno) => Err(StepEnd::Failed(OpError::Fs(src, errno))),
        }
    }

    fn step_transfer(
        &mut self,
        fs: &mut dyn Fs,
        src: String,
        kind: FileKind,
        dst: String,
        approved: Option<FileKind>,
    ) -> Result<(), StepEnd> {
        match kind {
            // A leaf — a file's bytes or a link's stored target — replaces an
            // occupied name only with the user's agreement, and replacing it
            // *removes* that name first so nothing is ever written through a
            // link that was already sitting there.
            FileKind::Regular | FileKind::Symlink => {
                let approved = if approved.is_some() {
                    approved
                } else {
                    match probe(fs, &dst).map_err(StepEnd::Failed)? {
                        None => None,
                        Some(FileKind::Directory) => {
                            return Err(StepEnd::Failed(OpError::KindMismatch(dst)));
                        }
                        Some(held) => {
                            return Err(self.pause(
                                src.clone(),
                                dst.clone(),
                                held,
                                Work::Transfer {
                                    src,
                                    kind,
                                    dst,
                                    approved: Some(held),
                                },
                            ));
                        }
                    }
                };
                // An approved link occupant is unlinked before the new object
                // is created: a create or truncate *follows* a final link, so
                // leaving one in place would act on whatever it names.
                if approved == Some(FileKind::Symlink) {
                    fs.remove_file(&dst)
                        .map_err(|errno| StepEnd::Failed(OpError::Fs(dst.clone(), errno)))?;
                }
                match kind {
                    // A link is *recreated* with the target it stores. Copying
                    // its bytes would leave a regular file holding the
                    // target's text, and following it would copy something the
                    // link only points at.
                    FileKind::Symlink => {
                        let target = fs
                            .read_link(&src)
                            .map_err(|errno| OpError::Fs(String::from(&src), errno))
                            .map_err(StepEnd::Failed)?;
                        // A new link never replaces a name, so an approved
                        // file occupant is removed here too.
                        if approved == Some(FileKind::Regular) {
                            fs.remove_file(&dst).map_err(|errno| {
                                StepEnd::Failed(OpError::Fs(dst.clone(), errno))
                            })?;
                        }
                        fs.create_link(&target, &dst)
                            .map_err(|errno| StepEnd::Failed(OpError::Fs(dst.clone(), errno)))?;
                    }
                    FileKind::Regular | FileKind::Directory => {
                        copy_file(fs, &src, &dst).map_err(StepEnd::Failed)?;
                    }
                }
                if self.kind == OpKind::Move {
                    // The name as typed: removing a link removes the link,
                    // never what it names.
                    fs.remove_file(&src)
                        .map_err(|errno| StepEnd::Failed(OpError::Fs(src.clone(), errno)))?;
                }
                self.done += 1;
                Ok(())
            }
            FileKind::Directory => {
                match probe(fs, &dst).map_err(StepEnd::Failed)? {
                    // An existing directory target is merged into.
                    Some(FileKind::Directory) => {}
                    // A leaf cannot hold a directory's contents, and a link
                    // is a leaf however it resolves.
                    Some(FileKind::Regular | FileKind::Symlink) => {
                        return Err(StepEnd::Failed(OpError::KindMismatch(dst)));
                    }
                    None => {
                        fs.mkdir(&dst)
                            .map_err(|errno| StepEnd::Failed(OpError::Fs(dst.clone(), errno)))?;
                    }
                }
                let entries = fs
                    .list_dir(&src)
                    .map_err(|errno| StepEnd::Failed(OpError::Fs(src.clone(), errno)))?;
                if self.kind == OpKind::Move {
                    // Queued beneath the children, so the source directory
                    // is removed only once they have all moved out.
                    self.stack
                        .push(Work::RemoveEmptiedDir { path: src.clone() });
                }
                for entry in entries {
                    self.stack.push(Work::Transfer {
                        src: join(&src, &entry.name),
                        kind: entry.kind,
                        dst: join(&dst, &entry.name),
                        approved: None,
                    });
                }
                Ok(())
            }
        }
    }

    fn step_remove(&mut self, fs: &mut dyn Fs, path: &str, kind: FileKind) -> Result<(), StepEnd> {
        match kind {
            // A link is removed as the leaf it is: `fs_unlink` keeps the name
            // as typed, so what the link points at is untouched.
            FileKind::Symlink | FileKind::Regular => {
                fs.remove_file(path)
                    .map_err(|errno| StepEnd::Failed(OpError::Fs(String::from(path), errno)))?;
                self.done += 1;
                Ok(())
            }
            FileKind::Directory => {
                let entries = fs
                    .list_dir(path)
                    .map_err(|errno| StepEnd::Failed(OpError::Fs(String::from(path), errno)))?;
                self.stack.push(Work::RemoveEmptiedDir {
                    path: String::from(path),
                });
                for entry in entries {
                    self.stack.push(Work::Remove {
                        path: join(path, &entry.name),
                        kind: entry.kind,
                    });
                }
                Ok(())
            }
        }
    }

    fn step_remove_emptied(&mut self, fs: &mut dyn Fs, path: &str) -> Result<(), StepEnd> {
        // A skip left a source in place on purpose; its ancestors are then
        // intentionally not emptied, so their removal is withheld.
        if self
            .skipped_srcs
            .iter()
            .any(|skipped| is_inside(path, skipped))
        {
            return Ok(());
        }
        fs.remove_dir(path)
            .map_err(|errno| StepEnd::Failed(OpError::Fs(String::from(path), errno)))?;
        self.done += 1;
        Ok(())
    }

    fn pause(&mut self, src: String, dst: String, occupant: FileKind, approved: Work) -> StepEnd {
        self.conflict = Some((Conflict { src, dst, occupant }, approved));
        StepEnd::Paused
    }
}

/// Whether moving a `src` of this kind onto a name already holding
/// `occupant` replaces a *leaf* — the case the user is asked about.
///
/// Two leaves (a file or a link either side) replace one another and lose
/// what was there, so the question is real. A directory on either side is
/// not this engine's to judge: the planner already redirected into an
/// existing directory, and anything left is the kernel's refusal to
/// surface.
const fn replaces_leaf(src: FileKind, occupant: FileKind) -> bool {
    is_leaf(src) && is_leaf(occupant)
}

/// Whether a node of this kind is a leaf — something a transfer replaces
/// whole rather than merges into.
const fn is_leaf(kind: FileKind) -> bool {
    matches!(kind, FileKind::Regular | FileKind::Symlink)
}

/// Why a step ended without completing.
enum StepEnd {
    /// The step paused on a conflict already recorded on the operation.
    Paused,
    /// The step failed; the operation stops with this error.
    Failed(OpError),
}

/// Stream a regular file from `src` to `dst` through a bounded buffer. A
/// failure mid-stream removes the partial target (best-effort) so a
/// half-written file never masquerades as the source's copy.
fn copy_file(fs: &mut dyn Fs, src: &str, dst: &str) -> Result<(), OpError> {
    fs.create(dst)
        .map_err(|errno| OpError::Fs(String::from(dst), errno))?;
    let mut buf = [0_u8; READ_CHUNK];
    let mut offset: u64 = 0;
    loop {
        let read = match fs.read(src, offset, &mut buf) {
            Ok(read) => read,
            Err(errno) => {
                let _ = fs.remove_file(dst);
                return Err(OpError::Fs(String::from(src), errno));
            }
        };
        if read == 0 {
            return Ok(());
        }
        // A seam reporting more than the buffer holds would index out of
        // bounds; refuse it rather than trust the count.
        if read > buf.len() {
            let _ = fs.remove_file(dst);
            return Err(OpError::Fs(String::from(src), Errno::LengthOutOfRange));
        }
        if let Err(errno) = fs.write(dst, offset, &buf[..read]) {
            let _ = fs.remove_file(dst);
            return Err(OpError::Fs(String::from(dst), errno));
        }
        offset = offset.saturating_add(read as u64);
    }
}
