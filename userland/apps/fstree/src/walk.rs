//! The bounded, cancellable branch walker — the one recursive-descent
//! definition shared by the flattened branch view (`v`), the disk-usage
//! statistics (`u`), and the branch searches (`/`, `F`), which are the
//! same walk with a [`Sieve`] deciding which found files reach the list.
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

use tairix_abi::time::Time64;
use tairix_abi::FileKind;
use tairix_glob::Pattern;

use crate::fs::Fs;
use crate::model::join;
use crate::search::ContentScan;

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
    /// A short annotation shown after the row's path (a content search's
    /// match figures); `None` for a plain listing row.
    pub note: Option<String>,
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
        Self::from_seeds(alloc::vec![String::from(root)])
    }

    /// A walk of everything below each of `dirs`, visited in the given
    /// order — the tagged-set content search seeds its tagged directories
    /// this way. An empty seed list is a finished walk.
    #[must_use]
    pub fn from_seeds(mut dirs: Vec<String>) -> Self {
        // The LIFO frontier pops the last entry first, so the seeds are
        // reversed to visit in the given order.
        dirs.reverse();
        Self {
            frontier: dirs,
            files_seen: 0,
            bytes: 0,
            dirs_seen: 0,
            errors: Vec::new(),
        }
    }

    /// Drop every directory still unread (the user stopped the walk);
    /// the counters and errors so far stand.
    pub fn stop(&mut self) {
        self.frontier.clear();
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
                        note: None,
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

/// Which of the walked files reach the flattened list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Sieve {
    /// Every file (the plain flattened view).
    All,
    /// Files whose branch-relative path matches the glob (`/`).
    Name {
        /// The pattern as typed, for the status line.
        text: String,
        /// The compiled matcher.
        pattern: Pattern,
    },
    /// Files whose contents contain the needle (`F`); the scan streams
    /// through its own byte budget per tick.
    Content {
        /// The needle as typed, for the status line.
        text: String,
        /// The streaming scanner and its queue.
        scan: ContentScan,
    },
}

/// A live walk carried by the model between ticks: the walker, its
/// purpose, the sieve, the collected entries (flat view only), and the
/// paging state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalkState {
    /// The directory the walk descends below.
    pub root: String,
    /// What the walk feeds.
    pub purpose: WalkPurpose,
    /// Which found files reach the list.
    pub sieve: Sieve,
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
        Self::sieved(root, purpose, Sieve::All, Walker::new(root), page)
    }

    /// A filename search below `root`: a flattened walk listing only the
    /// files whose branch-relative path matches `pattern`.
    #[must_use]
    pub fn name_search(root: &str, text: &str, pattern: Pattern, page: usize) -> Self {
        let sieve = Sieve::Name {
            text: String::from(text),
            pattern,
        };
        Self::sieved(root, WalkPurpose::Flat, sieve, Walker::new(root), page)
    }

    /// A content search: `walker` supplies the directories to descend
    /// (the focused branch, or the tagged directories) and `seeds` the
    /// files already known (the tagged files); `scan` holds the needle.
    #[must_use]
    pub fn content_search(
        root: &str,
        text: &str,
        mut scan: ContentScan,
        walker: Walker,
        seeds: Vec<FlatEntry>,
        page: usize,
    ) -> Self {
        scan.enqueue(seeds);
        let sieve = Sieve::Content {
            text: String::from(text),
            scan,
        };
        Self::sieved(root, WalkPurpose::Flat, sieve, walker, page)
    }

    fn sieved(root: &str, purpose: WalkPurpose, sieve: Sieve, walker: Walker, page: usize) -> Self {
        Self {
            root: String::from(root),
            purpose,
            sieve,
            walker,
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

    /// Advance the walk by one bounded tick: at most `dir_budget`
    /// directories read, and — for a content search — at most
    /// `byte_budget` file bytes scanned.
    pub fn tick(&mut self, fs: &mut dyn Fs, dir_budget: usize, byte_budget: usize) {
        if !self.ticking() {
            return;
        }
        let found = self.walker.tick(fs, dir_budget);
        let accepted = match &mut self.sieve {
            Sieve::All => found,
            Sieve::Name { pattern, .. } => {
                let root = &self.root;
                found
                    .into_iter()
                    .filter(|entry| pattern.matches(relative_to(root, &entry.path)))
                    .collect()
            }
            Sieve::Content { scan, .. } => {
                scan.enqueue(found);
                scan.tick(fs, byte_budget)
            }
        };
        let scanning = matches!(&self.sieve, Sieve::Content { scan, .. } if scan.busy());
        let more = self.walker.more() || scanning;
        if self.purpose == WalkPurpose::Flat {
            let boundary = (self.entries.len() / self.page + 1) * self.page;
            self.entries.extend(accepted);
            if self.entries.len() >= boundary && more {
                self.paused = true;
            }
        }
        if !more {
            self.finish();
        }
    }

    /// Stop the walk where it stands: nothing further is read or scanned,
    /// and the entries, counters, and errors so far stand.
    pub fn stop(&mut self) {
        self.walker.stop();
        if let Sieve::Content { scan, .. } = &mut self.sieve {
            scan.stop();
        }
        self.finish();
    }

    /// Mark the walk finished, folding a content scan's read errors into
    /// the walker's report so nothing unreadable goes unreported.
    fn finish(&mut self) {
        if let Sieve::Content { scan, .. } = &mut self.sieve {
            self.walker.errors.append(&mut scan.errors);
        }
        self.done = true;
        self.paused = false;
    }

    /// Resume a walk paused at a page boundary.
    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// The status line's name for what fills the list.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.sieve {
            Sieve::All => String::from("flattened"),
            Sieve::Name { text, .. } => format!("search {text}"),
            Sieve::Content { text, .. } => format!("contains \"{text}\""),
        }
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
