//! Multi-file tagging and the batch-operation driver.
//!
//! A [`TagSet`] is the ordered set of entries the user marked (`t`, the
//! glob prompt, invert); a [`Batch`] drives one [`FileOp`] per tagged
//! entry, **continuing past a failed entry** and collecting a per-file
//! failure report so a batch is never silently partial — the report says
//! exactly what happened to every entry. An overwrite conflict pauses the
//! whole batch through the same key-loop question a single operation uses,
//! and a cancel there drops every remaining entry (work already applied
//! stays applied, and the report says so).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::FileKind;

use crate::fs::Fs;
use crate::ops::{plan_target, Conflict, Decision, FileOp, OpError, OpKind, OpProgress};

/// One tagged entry: the path and the listing fields the status line's
/// figures and the batch planner need.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TagEntry {
    /// The entry's full path.
    pub path: String,
    /// The entry's kind (a tagged directory batches recursively).
    pub kind: FileKind,
    /// Apparent length in bytes as listed when tagged (`0` for a
    /// directory); the status line's total is a listing-time figure.
    pub size: u64,
}

/// The ordered set of tagged entries: insertion order is preserved (a
/// batch processes entries in the order they were tagged) and each path
/// appears at most once.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TagSet {
    entries: Vec<TagEntry>,
}

impl TagSet {
    /// The empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether nothing is tagged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries are tagged.
    #[must_use]
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// The sum of the tagged entries' listed sizes (saturating).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .fold(0_u64, |sum, e| sum.saturating_add(e.size))
    }

    /// Whether `path` is tagged.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        self.entries.iter().any(|e| e.path == path)
    }

    /// The tagged entries in tag order.
    #[must_use]
    pub fn entries(&self) -> &[TagEntry] {
        &self.entries
    }

    /// Tag `entry` if untagged, untag it if tagged; reports whether the
    /// entry is tagged afterwards.
    pub fn toggle(&mut self, entry: TagEntry) -> bool {
        if let Some(index) = self.entries.iter().position(|e| e.path == entry.path) {
            self.entries.remove(index);
            false
        } else {
            self.entries.push(entry);
            true
        }
    }

    /// Tag `entry` (a no-op when its path is already tagged).
    pub fn insert(&mut self, entry: TagEntry) {
        if !self.contains(&entry.path) {
            self.entries.push(entry);
        }
    }

    /// Untag `path` (a no-op when untagged).
    pub fn remove(&mut self, path: &str) {
        self.entries.retain(|e| e.path != path);
    }

    /// Untag `path` and everything inside it — used when a directory is
    /// deleted or moved so tags never point into a gone subtree.
    pub fn remove_under(&mut self, path: &str) {
        self.entries
            .retain(|e| e.path != path && !crate::ops::is_inside(path, &e.path));
    }

    /// Untag everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// What one [`Batch::advance`] call achieved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchProgress {
    /// Every entry was processed (some may have failed — see the report).
    Done,
    /// A [`Conflict`] awaits a [`Decision`] through [`Batch::resolve`].
    NeedsDecision,
}

/// A batch operation over the tagged set: one [`FileOp`] per entry,
/// processed in tag order, continuing past per-entry failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Batch {
    kind: OpKind,
    /// Entries still to process, reversed so `pop` yields tag order.
    pending: Vec<TagEntry>,
    /// The destination directory of a copy/move batch (`None` for delete).
    dest: Option<String>,
    /// The entry in flight and its operation.
    current: Option<(String, FileOp)>,
    /// Per-entry failures, in encounter order, as `path: reason` lines.
    failures: Vec<String>,
    /// Sources whose operation completed uncancelled, for untagging.
    succeeded: Vec<String>,
    /// Files the user chose to skip at overwrite questions, summed across
    /// entries.
    skipped: usize,
    /// Whether the user cancelled the remaining entries.
    cancelled: bool,
}

impl Batch {
    /// A batch copy of `items` into the existing directory `dest`.
    #[must_use]
    pub fn copy(items: &[TagEntry], dest: &str) -> Self {
        Self::new(OpKind::Copy, items, Some(String::from(dest)))
    }

    /// A batch move of `items` into the existing directory `dest`.
    #[must_use]
    pub fn move_to(items: &[TagEntry], dest: &str) -> Self {
        Self::new(OpKind::Move, items, Some(String::from(dest)))
    }

    /// A batch delete of `items`.
    #[must_use]
    pub fn delete(items: &[TagEntry]) -> Self {
        Self::new(OpKind::Delete, items, None)
    }

    fn new(kind: OpKind, items: &[TagEntry], dest: Option<String>) -> Self {
        let mut pending: Vec<TagEntry> = items.to_vec();
        pending.reverse();
        Self {
            kind,
            pending,
            dest,
            current: None,
            failures: Vec::new(),
            succeeded: Vec::new(),
            skipped: 0,
            cancelled: false,
        }
    }

    /// The batch's kind, for the prompts and reports.
    #[must_use]
    pub fn kind(&self) -> OpKind {
        self.kind
    }

    /// The paused overwrite question, when [`Batch::advance`] reported
    /// [`BatchProgress::NeedsDecision`].
    #[must_use]
    pub fn conflict(&self) -> Option<&Conflict> {
        self.current.as_ref().and_then(|(_, op)| op.conflict())
    }

    /// Answer the paused question. A cancel drops every remaining entry
    /// of the batch, not just the current one — the user said stop.
    pub fn resolve(&mut self, decision: Decision) {
        if decision == Decision::Cancel {
            self.cancelled = true;
            self.pending.clear();
        }
        if decision == Decision::Skip {
            self.skipped += 1;
        }
        if let Some((_, op)) = &mut self.current {
            op.resolve(decision);
        }
    }

    /// Drive the batch until every entry is processed or an overwrite
    /// question pauses it. A failed entry is recorded and the batch
    /// continues with the next.
    pub fn advance(&mut self, fs: &mut dyn Fs) -> BatchProgress {
        loop {
            if let Some((src, mut op)) = self.current.take() {
                match op.advance(fs) {
                    OpProgress::Done => {
                        if op.cancelled() {
                            self.failures.push(format!("{src}: cancelled"));
                        } else {
                            self.succeeded.push(src);
                        }
                    }
                    OpProgress::NeedsDecision => {
                        self.current = Some((src, op));
                        return BatchProgress::NeedsDecision;
                    }
                    OpProgress::Failed(error) => {
                        self.failures.push(failure_line(&src, &error));
                    }
                }
                continue;
            }
            let Some(item) = self.pending.pop() else {
                return BatchProgress::Done;
            };
            match self.kind {
                OpKind::Delete => {
                    self.current = Some((item.path.clone(), FileOp::delete(&item.path, item.kind)));
                }
                OpKind::Copy | OpKind::Move => {
                    // The destination directory is fixed; each entry's
                    // landing spot is planned as it comes up, so earlier
                    // entries' effects are visible to later plans.
                    let Some(dest) = self.dest.clone() else {
                        // Unreachable by construction (copy/move carry a
                        // destination); fail closed per entry, never panic.
                        self.failures.push(format!("{}: no destination", item.path));
                        continue;
                    };
                    match plan_target(fs, &item.path, item.kind, &dest) {
                        Ok(target) => {
                            let op = if self.kind == OpKind::Move {
                                FileOp::move_to(&item.path, item.kind, &target)
                            } else {
                                FileOp::copy(&item.path, item.kind, &target)
                            };
                            self.current = Some((item.path, op));
                        }
                        Err(error) => {
                            self.failures.push(failure_line(&item.path, &error));
                        }
                    }
                }
            }
        }
    }

    /// The per-entry failure lines, in encounter order.
    #[must_use]
    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// The sources whose operation completed, for untagging.
    #[must_use]
    pub fn succeeded(&self) -> &[String] {
        &self.succeeded
    }

    /// The one-line completion report for the message line; the failure
    /// lines themselves are shown by the report overlay.
    ///
    /// The figures name what was processed: succeeded plus failed entries
    /// (a cancel's unprocessed remainder is the "(cancelled)" marker's to
    /// explain, never silently folded into a count).
    #[must_use]
    pub fn summary(&self) -> String {
        let verb = match self.kind {
            OpKind::Copy => "copied",
            OpKind::Move => "moved",
            OpKind::Delete => "deleted",
        };
        let total = self.succeeded.len() + self.failures.len();
        let mut text = format!("batch: {verb} {} of {total} tagged", self.succeeded.len());
        if !self.failures.is_empty() {
            // Writing to a `String` cannot fail.
            let _ = write!(text, ", {} failed", self.failures.len());
        }
        if self.skipped > 0 {
            let _ = write!(text, ", {} skipped", self.skipped);
        }
        if self.cancelled {
            text.push_str(" (cancelled)");
        }
        text
    }
}

/// One report line for a failed entry. An error that already names its
/// failing path ([`OpError::Fs`], [`OpError::KindMismatch`]) stands alone;
/// the rest are prefixed with the entry they refused so every line names
/// exactly one subject, never the same path twice.
fn failure_line(src: &str, error: &OpError) -> String {
    match error {
        OpError::Fs(..) | OpError::KindMismatch(_) => error.describe(),
        _ => format!("{src}: {}", error.describe()),
    }
}
