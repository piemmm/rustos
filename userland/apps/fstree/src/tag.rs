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

use tairix_abi::time::Time64;
use tairix_abi::time::{civil_from_days, days_from_civil};
use tairix_abi::FileKind;

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

/// Seconds per civil day, for the date bounds' midnight arithmetic.
const DAY_SECS: i64 = 86_400;

/// A range the tag-by-pattern prompt (`T`) can name instead of a glob:
/// listed byte sizes (`size:1M..64M`, either bound open) or
/// modification dates (`date:2024-01-01..2024-06-30`, whole days,
/// end-date inclusive). A malformed range is a typed refusal that tags
/// nothing, exactly like a malformed glob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TagRange {
    /// Listed size in bytes, both bounds inclusive when present.
    Size {
        /// The smallest matching size.
        min: Option<u64>,
        /// The largest matching size.
        max: Option<u64>,
    },
    /// Modification instant in seconds: at or after `min` (that day's
    /// midnight), before `max` (the midnight *after* the named end date,
    /// so the end date itself is included).
    Date {
        /// The first matching instant.
        min: Option<i64>,
        /// One past the last matching instant.
        max: Option<i64>,
    },
}

impl TagRange {
    /// Recognise and parse a range spelling. `Ok(None)` means the text is
    /// not a range at all (the prompt then treats it as a glob);
    /// `Err(reason)` means it *is* a range spelling but malformed —
    /// refused whole, never partially applied.
    ///
    /// # Errors
    ///
    /// A human-readable reason when the range fails to parse: a missing
    /// `..`, an empty range, an unparseable size or date, or bounds in
    /// the wrong order.
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        if let Some(rest) = text.strip_prefix("size:") {
            let (low, high) = split_bounds(rest)?;
            let min = low.map(parse_size).transpose()?;
            let max = high.map(parse_size).transpose()?;
            if let (Some(a), Some(b)) = (min, max) {
                if a > b {
                    return Err(String::from("size: bounds are in the wrong order"));
                }
            }
            return Ok(Some(Self::Size { min, max }));
        }
        if let Some(rest) = text.strip_prefix("date:") {
            let (low, high) = split_bounds(rest)?;
            let min = low.map(parse_date).transpose()?;
            // The end date is inclusive: match up to the following
            // midnight.
            let max = high
                .map(parse_date)
                .transpose()?
                .map(|secs| secs + DAY_SECS);
            if let (Some(a), Some(b)) = (min, max) {
                if a >= b {
                    return Err(String::from("date: bounds are in the wrong order"));
                }
            }
            return Ok(Some(Self::Date { min, max }));
        }
        Ok(None)
    }

    /// Whether an entry with the listed `size` and `modified` stamp falls
    /// inside the range. A backing that keeps no stamp (the epoch marker)
    /// never matches a date range — an unknown date is not a date.
    #[must_use]
    pub fn matches(&self, size: u64, modified: Time64) -> bool {
        match self {
            Self::Size { min, max } => {
                min.is_none_or(|m| size >= m) && max.is_none_or(|m| size <= m)
            }
            Self::Date { min, max } => {
                if modified == Time64::UNIX_EPOCH {
                    return false;
                }
                let secs = modified.secs();
                min.is_none_or(|m| secs >= m) && max.is_none_or(|m| secs < m)
            }
        }
    }
}

/// Split `A..B` into its optional bounds; `..` alone, or a missing `..`,
/// is malformed.
fn split_bounds(text: &str) -> Result<(Option<&str>, Option<&str>), String> {
    let Some((low, high)) = text.split_once("..") else {
        return Err(String::from("range needs `..` between its bounds"));
    };
    let low = (!low.is_empty()).then_some(low);
    let high = (!high.is_empty()).then_some(high);
    if low.is_none() && high.is_none() {
        return Err(String::from("range needs at least one bound"));
    }
    Ok((low, high))
}

/// Parse a size bound: decimal bytes with an optional binary-unit suffix
/// (`K`, `M`, `G`, `T`).
fn parse_size(text: &str) -> Result<u64, String> {
    let (digits, shift) = match text.as_bytes().last() {
        Some(b'K' | b'k') => (&text[..text.len() - 1], 10),
        Some(b'M' | b'm') => (&text[..text.len() - 1], 20),
        Some(b'G' | b'g') => (&text[..text.len() - 1], 30),
        Some(b'T' | b't') => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    let value: u64 = digits.parse().map_err(|_| format!("bad size: {text}"))?;
    value
        .checked_shl(shift)
        .filter(|scaled| scaled >> shift == value)
        .ok_or_else(|| format!("size out of range: {text}"))
}

/// Parse a `YYYY-MM-DD` date into the seconds of that day's midnight.
/// The calendar is the arbiter: a spelling that does not round-trip
/// (2024-02-30) is refused.
fn parse_date(text: &str) -> Result<i64, String> {
    let bad = || format!("bad date: {text} (expected YYYY-MM-DD)");
    let mut parts = text.splitn(3, '-');
    let year: i64 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let month: u32 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let day: u32 = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return Err(bad());
    }
    days.checked_mul(DAY_SECS).ok_or_else(bad)
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
