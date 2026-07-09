//! The streaming file-content scanner behind the `F` content search.
//!
//! A [`ContentScan`] holds the files still to scan and the one file in
//! flight; each [`ContentScan::tick`] reads at most a byte budget through
//! the [`Fs`] seam and returns, so the key loop stays responsive and Esc
//! can stop the search between ticks. A file is never held in memory
//! whole: each read window carries over only the last `needle.len() - 1`
//! bytes, so a match spanning a read boundary is still found. Matching is
//! literal and ASCII-case-insensitive. A file whose first window contains
//! a NUL byte is reported as a binary match — its bytes are counted, never
//! shown. A file that refuses to read is recorded and skipped, never
//! silently dropped.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::fs::Fs;
use crate::walk::FlatEntry;

/// Bytes one read window covers. Small enough that a tick's budget spans
/// several files, large enough that a big file needs few syscalls.
const WINDOW: usize = 16 * 1024;

/// The compiled search subject: the needle lowered once, matched
/// case-insensitively against every window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Needle {
    lowered: Vec<u8>,
}

impl Needle {
    /// Compile `text` for matching; `None` for an empty needle (there is
    /// nothing to search for).
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        Some(Self {
            lowered: text.bytes().map(|b| b.to_ascii_lowercase()).collect(),
        })
    }

    /// The needle's length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lowered.len()
    }

    /// Never empty by construction, but spelled out for callers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lowered.is_empty()
    }

    /// How many (possibly overlapping) occurrences of the needle appear
    /// in `window`, ASCII-case-insensitively.
    #[must_use]
    pub fn count_in(&self, window: &[u8]) -> u64 {
        let n = self.lowered.len();
        if n == 0 || window.len() < n {
            return 0;
        }
        let first = self.lowered[0];
        let mut count = 0;
        for start in 0..=(window.len() - n) {
            if window[start].to_ascii_lowercase() != first {
                continue;
            }
            if window[start + 1..start + n]
                .iter()
                .zip(&self.lowered[1..])
                .all(|(b, needle)| b.to_ascii_lowercase() == *needle)
            {
                count += 1;
            }
        }
        count
    }
}

/// The file currently being read, window by window.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileScan {
    /// The file under scan (the result row, should it match).
    entry: FlatEntry,
    /// The next read offset.
    offset: u64,
    /// The last `needle.len() - 1` bytes of the previous window, so a
    /// match spanning the boundary is still seen.
    carry: Vec<u8>,
    /// A NUL byte appeared in the first window.
    binary: bool,
    /// Occurrences counted so far.
    matches: u64,
}

/// The streaming content search: files awaiting their scan, the file in
/// flight, and the errors met on the way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentScan {
    needle: Needle,
    /// Files still to scan, scanned in discovery order.
    pending: VecDeque<FlatEntry>,
    current: Option<FileScan>,
    /// Files whose read was refused, as `path: errno` lines.
    pub errors: Vec<String>,
    /// Files fully scanned (matched or not).
    pub scanned: u64,
}

impl ContentScan {
    /// A scan for `needle`, starting empty; files arrive through
    /// [`ContentScan::enqueue`] as the walk finds them.
    #[must_use]
    pub fn new(needle: Needle) -> Self {
        Self {
            needle,
            pending: VecDeque::new(),
            current: None,
            errors: Vec::new(),
            scanned: 0,
        }
    }

    /// Queue `files` for scanning, in the given order.
    pub fn enqueue(&mut self, files: Vec<FlatEntry>) {
        self.pending.extend(files);
    }

    /// Whether scanning work remains.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.current.is_some() || !self.pending.is_empty()
    }

    /// Drop everything still unscanned (the user stopped the search);
    /// results already produced stand.
    pub fn stop(&mut self) {
        self.pending.clear();
        self.current = None;
    }

    /// Scan up to `byte_budget` further bytes, returning the files whose
    /// scan completed with at least one match. Read failures are recorded
    /// in [`ContentScan::errors`] and the file skipped.
    pub fn tick(&mut self, fs: &mut dyn Fs, byte_budget: usize) -> Vec<FlatEntry> {
        let mut matched = Vec::new();
        let mut budget = byte_budget;
        let mut buf = vec![0_u8; WINDOW];
        while budget > 0 {
            let Some(mut scan) = self.current.take().or_else(|| {
                self.pending.pop_front().map(|entry| FileScan {
                    entry,
                    offset: 0,
                    carry: Vec::new(),
                    binary: false,
                    matches: 0,
                })
            }) else {
                break;
            };
            let want = WINDOW.min(budget);
            let read = match fs.read(&scan.entry.path, scan.offset, &mut buf[..want]) {
                Ok(read) => read,
                Err(errno) => {
                    // The file is dropped from the scan; the queue shrinks,
                    // so a run of failing files still terminates the loop.
                    self.errors.push(format!("{}: {errno:?}", scan.entry.path));
                    continue;
                }
            };
            budget = budget.saturating_sub(read.max(1));
            if read == 0 {
                self.scanned += 1;
                if scan.matches > 0 {
                    let mut entry = scan.entry;
                    entry.note = Some(match_note(scan.binary, scan.matches));
                    matched.push(entry);
                }
                continue;
            }
            if scan.offset == 0 && buf[..read].contains(&0) {
                scan.binary = true;
            }
            let mut window = core::mem::take(&mut scan.carry);
            window.extend_from_slice(&buf[..read]);
            // Every occurrence is longer than the carried tail, so it
            // necessarily covers new bytes — nothing is counted twice.
            scan.matches += self.needle.count_in(&window);
            let keep = self.needle.len().saturating_sub(1).min(window.len());
            scan.carry = window[window.len() - keep..].to_vec();
            scan.offset += read as u64;
            self.current = Some(scan);
        }
        matched
    }
}

/// The result row's annotation: how the file matched.
fn match_note(binary: bool, matches: u64) -> String {
    let plural = if matches == 1 { "match" } else { "matches" };
    if binary {
        format!("binary, {matches} {plural}")
    } else {
        format!("{matches} {plural}")
    }
}
