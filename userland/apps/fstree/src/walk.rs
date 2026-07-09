//! The bounded, cancellable branch walker — the one recursive-descent
//! definition shared by the flattened branch view (`v`) and the disk-usage
//! statistics (`u`).
//!
//! A walk never recurses through a whole branch in one blocking pass: each
//! [`Walker::tick`] reads at most a fixed number of directories and returns,
//! so the key loop stays responsive and an Esc can cancel between ticks.
//! Memory follows the frontier (the stack of directories still to read),
//! never the subtree, and the flattened view additionally pauses at page
//! boundaries so a huge branch fills the list only as far as the user asks.
//! An unreadable directory is recorded and skipped — the walk continues and
//! the report says exactly what could not be read, never silently partial.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::time::Time64;
use rustos_abi::FileKind;

use crate::fs::Fs;
use crate::model::join;

/// One file a walk found, listed by the flattened branch view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlatEntry {
    /// The file's full path.
    pub path: String,
    /// The entry's kind (regular files only today; the field keeps the
    /// tag and batch machinery honest about what it operates on).
    pub kind: FileKind,
    /// Apparent length in bytes.
    pub size: u64,
    /// Last contents-modification instant ([`Time64::UNIX_EPOCH`] renders
    /// as absent).
    pub modified: Time64,
}

/// The incremental depth-first descent below one root directory.
///
/// Directories are visited children-in-name-order (deterministic
/// regardless of driver order); each visited directory contributes its
/// regular files to the tick's output and its child directories to the
/// frontier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Walker {
    /// Directories still to read, popped last-in-first-out so the descent
    /// is depth-first; children are pushed in reverse name order so they
    /// pop in name order.
    frontier: Vec<String>,
    /// Regular files seen so far.
    pub files_seen: u64,
    /// Bytes of the regular files seen so far (saturating).
    pub bytes: u64,
    /// Directories read so far (the root included once read).
    pub dirs_seen: u64,
    /// Directories that refused to list, as `path: errno` lines for the
    /// walk's report.
    pub errors: Vec<String>,
}

impl Walker {
    /// A walk of everything below `root` (the root directory itself is
    /// the first frontier entry).
    #[must_use]
    pub fn new(root: &str) -> Self {
        Self {
            frontier: alloc::vec![String::from(root)],
            files_seen: 0,
            bytes: 0,
            dirs_seen: 0,
            errors: Vec::new(),
        }
    }

    /// Whether directories remain to be read.
    #[must_use]
    pub fn more(&self) -> bool {
        !self.frontier.is_empty()
    }

    /// Read at most `budget` directories from the frontier, returning the
    /// regular files they contained (in visit order). Counters and the
    /// error report advance as a side effect. A `budget` of zero reads
    /// nothing and reports honestly that more remains.
    pub fn tick(&mut self, fs: &mut dyn Fs, budget: usize) -> Vec<FlatEntry> {
        let mut found = Vec::new();
        for _ in 0..budget {
            let Some(dir) = self.frontier.pop() else {
                break;
            };
            let mut entries = match fs.list_dir(&dir) {
                Ok(entries) => entries,
                Err(errno) => {
                    self.errors.push(format!("{dir}: {errno:?}"));
                    continue;
                }
            };
            self.dirs_seen += 1;
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            // Child directories are pushed in reverse name order so the
            // LIFO frontier pops them in name order.
            for entry in entries.iter().rev() {
                if entry.kind == FileKind::Directory {
                    self.frontier.push(join(&dir, &entry.name));
                }
            }
            for entry in &entries {
                if entry.kind == FileKind::Regular {
                    self.files_seen += 1;
                    self.bytes = self.bytes.saturating_add(entry.size);
                    found.push(FlatEntry {
                        path: join(&dir, &entry.name),
                        kind: entry.kind,
                        size: entry.size,
                        modified: entry.modified,
                    });
                }
            }
        }
        found
    }
}

/// `path` relative to the walk root — the flattened view's row text and
/// its glob-matching subject share this one spelling.
#[must_use]
pub fn relative_to<'p>(root: &str, path: &'p str) -> &'p str {
    path.strip_prefix(root)
        .map_or(path, |rest| rest.trim_start_matches('/'))
}

/// What a live walk feeds: the flattened branch view's list, or the
/// disk-usage counters alone.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WalkPurpose {
    /// Collect every found file for the flattened branch view.
    Flat,
    /// Count files and bytes only (`u`); nothing is collected.
    Usage,
}

/// A live walk carried by the model between ticks: the walker, its
/// purpose, the collected entries (flat view only), and the paging state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalkState {
    /// The directory the walk descends below.
    pub root: String,
    /// What the walk feeds.
    pub purpose: WalkPurpose,
    /// The incremental descent.
    pub walker: Walker,
    /// The files found so far (flat view; empty for a usage walk).
    pub entries: Vec<FlatEntry>,
    /// Entries per expansion page: the flat view pauses at each multiple
    /// until the user asks for more, so a huge branch never fills memory
    /// unasked.
    pub page: usize,
    /// Paused at a page boundary, awaiting the load-more key.
    pub paused: bool,
    /// Every frontier directory has been read (or refused and recorded).
    pub done: bool,
}

impl WalkState {
    /// A walk of everything below `root`; the flat view pauses each time
    /// `page` further entries have accumulated.
    #[must_use]
    pub fn new(root: &str, purpose: WalkPurpose, page: usize) -> Self {
        Self {
            root: String::from(root),
            purpose,
            walker: Walker::new(root),
            entries: Vec::new(),
            page: page.max(1),
            paused: false,
            done: false,
        }
    }

    /// Whether the walk wants further ticks right now (not finished, not
    /// waiting at a page boundary).
    #[must_use]
    pub fn ticking(&self) -> bool {
        !self.done && !self.paused
    }

    /// Advance the walk by one bounded tick of at most `budget`
    /// directories.
    pub fn tick(&mut self, fs: &mut dyn Fs, budget: usize) {
        if !self.ticking() {
            return;
        }
        let found = self.walker.tick(fs, budget);
        if self.purpose == WalkPurpose::Flat {
            let boundary = (self.entries.len() / self.page + 1) * self.page;
            self.entries.extend(found);
            if self.entries.len() >= boundary && self.walker.more() {
                self.paused = true;
            }
        }
        if !self.walker.more() {
            self.done = true;
            self.paused = false;
        }
    }

    /// Resume a walk paused at a page boundary.
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// The one-line progress/summary figures: files, bytes, directories,
    /// and the unreadable count when any.
    #[must_use]
    pub fn figures(&self) -> String {
        let unreadable = if self.walker.errors.is_empty() {
            String::new()
        } else {
            format!(", {} unreadable", self.walker.errors.len())
        };
        format!(
            "{} files, {} bytes, {} dirs{unreadable}",
            self.walker.files_seen, self.walker.bytes, self.walker.dirs_seen
        )
    }
}
