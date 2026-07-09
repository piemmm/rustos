//! The text viewer: streaming, read-only paging of a file's contents.
//!
//! The file is never held in memory whole. The view keeps only the start
//! of its top row and re-reads one screenful through the [`Fs`] seam per
//! refresh, in bounded windows. A *row* is the unit of paging: the bytes
//! up to and including the next newline, capped at [`ROW_MAX`] bytes so a
//! newline-free file still pages in bounded memory (an over-long physical
//! line simply continues on the next row). Bytes are decoded as UTF-8
//! with lossy replacement, and control characters are made visible
//! through the shared sanitiser — untrusted file content reaches the
//! curses grid as printable characters only, never as raw terminal
//! escapes.
//!
//! Line search (`/`) and goto-line run as byte-budgeted background jobs
//! ticked by the key loop, so a huge file never freezes the session and
//! Esc cancels between ticks.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::Errno;

use crate::fs::Fs;
use crate::search::Needle;

/// Longest row in bytes: a physical line beyond this continues on the
/// next row, so paging a newline-free file stays bounded.
pub const ROW_MAX: usize = 4096;

/// The start of a row: its byte offset, and the count of newlines before
/// it when known. The line is `None` after a mid-file jump (switching
/// from the hex view) where counting from the start was not paid for;
/// goto-line re-anchors it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RowStart {
    /// Byte offset of the row's first byte.
    pub offset: u64,
    /// Newlines before `offset` (0-based line number), when known.
    pub line: Option<u64>,
}

/// One decoded display row of the current page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextRow {
    /// The row's sanitised text (printable characters only).
    pub text: String,
    /// The row's 0-based line number, when known.
    pub line: Option<u64>,
}

/// A live background scan: the goto-line seek or the literal search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextJob {
    /// Counting newlines forward from an anchor to reach a target line.
    Goto(GotoScan),
    /// Searching forward for a literal needle.
    Search(SearchScan),
}

/// The goto-line scan state: walks forward from `at`, counting newlines,
/// until `target` (0-based) is reached or the file ends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GotoScan {
    /// The 0-based line the scan seeks.
    pub target: u64,
    /// The next read offset.
    pub offset: u64,
    /// Newlines counted before `offset`.
    pub line: u64,
    /// Start of the row containing `offset`.
    pub row_start: u64,
}

/// The literal-search scan state: walks forward from the row after the
/// top; a hit snaps the top onto the row containing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchScan {
    /// The compiled needle (ASCII-case-insensitive literal).
    pub needle: Needle,
    /// The next read offset.
    pub offset: u64,
    /// Newlines counted before `offset`, when known.
    pub line: Option<u64>,
    /// The last `needle.len() - 1` bytes of the previous window, so a
    /// match spanning a read boundary is still found.
    pub carry: Vec<u8>,
}

/// What a finished [`TextJob`] produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobOutcome {
    /// Still scanning; call again on the next tick.
    Pending,
    /// The scan landed the top on a new row.
    Moved,
    /// A search reached the end without a hit.
    NotFound,
    /// A goto-line target lies beyond the last line; the top is on the
    /// last row instead.
    PastEnd,
    /// A read was refused; the view keeps its place.
    Failed(Errno),
}

/// The text-view state: the file, the top row, and the paging options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextView {
    /// The viewed file's path.
    pub path: String,
    /// Apparent file length from the listing (display only; the end of
    /// file is governed by short reads, never by this figure).
    pub size: u64,
    /// The top row shown.
    pub top: RowStart,
    /// Whether long rows wrap onto further screen lines (on) or truncate
    /// at the right edge (off).
    pub wrap: bool,
    /// The needle of the last search, repeated by `n`.
    pub last_search: Option<String>,
    /// The offset of the last hit, so `n` continues past it.
    pub last_hit: Option<u64>,
    /// The live background scan, if one is running.
    pub job: Option<TextJob>,
    /// The decoded page the renderer draws, refreshed after every change.
    pub rows: Vec<TextRow>,
    /// Whether the page's last row ends the file (for the status line).
    pub at_end: bool,
    /// Display rows of the last refresh — the paging unit; a nominal
    /// figure until the first refresh records the real screen.
    pub viewport_rows: usize,
    /// Display columns of the last refresh, for the wrap arithmetic.
    pub viewport_cols: usize,
}

impl TextView {
    /// A view of `path` starting at the top of the file.
    #[must_use]
    pub fn new(path: &str, size: u64) -> Self {
        Self {
            path: String::from(path),
            size,
            top: RowStart {
                offset: 0,
                line: Some(0),
            },
            wrap: true,
            last_search: None,
            last_hit: None,
            job: None,
            rows: Vec::new(),
            at_end: false,
            viewport_rows: 22,
            viewport_cols: 80,
        }
    }

    /// A view of `path` whose top is snapped to the row containing
    /// `offset` (a jump from the hex view); the line number is unknown
    /// until a goto-line re-anchors it.
    pub fn at_offset(fs: &mut dyn Fs, path: &str, size: u64, offset: u64) -> Self {
        let mut view = Self::new(path, size);
        if offset > 0 {
            let start = row_start_at(fs, path, offset).unwrap_or(0);
            view.top = RowStart {
                offset: start,
                line: if start == 0 { Some(0) } else { None },
            };
        }
        view
    }
}

/// Read one row starting at `offset`: its raw bytes (newline excluded)
/// and the next row's start, or `None` at end of file.
///
/// # Errors
///
/// Any [`Errno`] the read raises.
pub fn read_row(
    fs: &mut dyn Fs,
    path: &str,
    offset: u64,
) -> Result<Option<(Vec<u8>, u64, bool)>, Errno> {
    let mut buf = vec![0_u8; ROW_MAX];
    let read = fs.read(path, offset, &mut buf)?;
    if read == 0 {
        return Ok(None);
    }
    match buf[..read].iter().position(|&b| b == b'\n') {
        Some(nl) => Ok(Some((buf[..nl].to_vec(), offset + nl as u64 + 1, true))),
        None if read == ROW_MAX => Ok(Some((buf, offset + ROW_MAX as u64, false))),
        // The file ends inside this row: the row is the remaining bytes.
        None => Ok(Some((buf[..read].to_vec(), offset + read as u64, false))),
    }
}

/// The start of the row containing `offset`: scan the window before it
/// for the last newline, then step forward in [`ROW_MAX`] chunks. A
/// physical line longer than the window is chunked from the window's
/// start — bounded work, consistent within the walk.
///
/// # Errors
///
/// Any [`Errno`] the read raises.
pub fn row_start_at(fs: &mut dyn Fs, path: &str, offset: u64) -> Result<u64, Errno> {
    if offset == 0 {
        return Ok(0);
    }
    let window = ROW_MAX as u64;
    let from = offset.saturating_sub(window);
    let len = usize::try_from(offset - from).unwrap_or(ROW_MAX);
    let mut buf = vec![0_u8; len];
    let read = fs.read(path, from, &mut buf)?;
    let seen = &buf[..read];
    // The line start within view: after the last newline strictly before
    // `offset`, or the window start when none is visible.
    let mut start = match seen.iter().rposition(|&b| b == b'\n') {
        Some(nl) => from + nl as u64 + 1,
        None => from,
    };
    // Step forward in row chunks until the row containing `offset`.
    while start + window <= offset {
        start += window;
    }
    Ok(start)
}

impl TextView {
    /// Scroll down `count` rows, stopping at the last row. A refused read
    /// keeps the place and returns the error.
    pub fn scroll_down(&mut self, fs: &mut dyn Fs, count: usize) -> Result<(), Errno> {
        for _ in 0..count {
            let Some((_, next, newline)) = read_row(fs, &self.path, self.top.offset)? else {
                break;
            };
            // Never scroll onto an empty page: stop while the next row
            // still has bytes.
            let mut probe = [0_u8; 1];
            if fs.read(&self.path, next, &mut probe)? == 0 {
                break;
            }
            self.top = RowStart {
                offset: next,
                line: match (self.top.line, newline) {
                    (Some(line), true) => Some(line + 1),
                    (line, false) => line,
                    (None, _) => None,
                },
            };
        }
        Ok(())
    }

    /// Scroll up `count` rows, stopping at the first.
    pub fn scroll_up(&mut self, fs: &mut dyn Fs, count: usize) -> Result<(), Errno> {
        for _ in 0..count {
            if self.top.offset == 0 {
                self.top.line = Some(0);
                break;
            }
            let start = row_start_at(fs, &self.path, self.top.offset - 1)?;
            // The step crossed a newline exactly when the byte before the
            // old top ended a line.
            let mut before = [0_u8; 1];
            let crossed =
                fs.read(&self.path, self.top.offset - 1, &mut before)? == 1 && before[0] == b'\n';
            self.top = RowStart {
                offset: start,
                line: match (self.top.line, crossed) {
                    (Some(line), true) => Some(line.saturating_sub(1)),
                    (line, false) => line,
                    (None, _) => None,
                },
            };
        }
        if self.top.offset == 0 {
            self.top.line = Some(0);
        }
        Ok(())
    }

    /// Jump to the top of the file.
    pub fn go_home(&mut self) {
        self.top = RowStart {
            offset: 0,
            line: Some(0),
        };
    }

    /// Jump so the last rows fill the page: step back `page` rows from
    /// the end of the file.
    pub fn go_end(&mut self, fs: &mut dyn Fs, page: usize) -> Result<(), Errno> {
        let end = end_of_file(fs, &self.path, self.size)?;
        if end == 0 {
            self.go_home();
            return Ok(());
        }
        self.top = RowStart {
            offset: row_start_at(fs, &self.path, end - 1)?,
            line: None,
        };
        self.scroll_up(fs, page.saturating_sub(1))?;
        Ok(())
    }
}

/// The file's end offset near the listing size: the figure confirmed (or
/// corrected) by bounded probe reads, so a stale listing never strands
/// the view and a file grown far past its listing never stalls the key
/// loop — the confirmation stops after a bounded budget and the user
/// scrolls on from there.
fn end_of_file(fs: &mut dyn Fs, path: &str, size: u64) -> Result<u64, Errno> {
    /// Windows the confirmation may read past the listed size.
    const PROBE_WINDOWS: usize = 64;
    let mut end = size;
    let mut probe = vec![0_u8; ROW_MAX];
    for _ in 0..PROBE_WINDOWS {
        let read = fs.read(path, end, &mut probe)?;
        if read == 0 {
            break;
        }
        end += read as u64;
    }
    Ok(end)
}

impl TextView {
    /// Re-read the page under the top row: up to `rows` display rows,
    /// decoded and sanitised for the renderer. With wrap on, one file row
    /// fills several display rows of `cols` columns each. A refused read
    /// empties the page and returns the error — stale rows are never
    /// shown as live.
    pub fn refresh(&mut self, fs: &mut dyn Fs, rows: usize, cols: usize) -> Result<(), Errno> {
        self.viewport_rows = rows.max(1);
        self.viewport_cols = cols;
        self.rows.clear();
        self.at_end = false;
        let result = self.fill_page(fs, rows, cols);
        if result.is_err() {
            self.rows.clear();
        }
        result
    }

    fn fill_page(&mut self, fs: &mut dyn Fs, rows: usize, cols: usize) -> Result<(), Errno> {
        let mut at = self.top;
        let mut tail_hidden = false;
        while self.rows.len() < rows {
            let Some((bytes, next, newline)) = read_row(fs, &self.path, at.offset)? else {
                self.at_end = true;
                return Ok(());
            };
            let text = sanitise(&bytes);
            if self.wrap && cols > 0 {
                let pieces = wrap_segments(&text, cols);
                let shown = pieces.len().min(rows - self.rows.len());
                tail_hidden = shown < pieces.len();
                for piece in pieces.into_iter().take(shown) {
                    self.rows.push(TextRow {
                        text: piece,
                        line: at.line,
                    });
                }
            } else {
                self.rows.push(TextRow {
                    text,
                    line: at.line,
                });
            }
            at = RowStart {
                offset: next,
                line: match (at.line, newline) {
                    (Some(line), true) => Some(line + 1),
                    (line, false) => line,
                    (None, _) => None,
                },
            };
        }
        // The page filled exactly: it ends the file only when nothing
        // (not even a hidden wrapped tail) follows it.
        if !tail_hidden {
            let mut probe = [0_u8; 1];
            if fs.read(&self.path, at.offset, &mut probe)? == 0 {
                self.at_end = true;
            }
        }
        Ok(())
    }
}

/// Decode row bytes for display: UTF-8 with lossy replacement, tabs
/// expanded to the next 8-column stop, a trailing carriage return (CRLF)
/// dropped, and every other control character replaced by `·` — file
/// content reaches the grid as printable characters only, never as raw
/// terminal escapes.
#[must_use]
pub fn sanitise(bytes: &[u8]) -> String {
    let mut rest = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    let mut text = String::new();
    let mut col = 0_usize;
    loop {
        match core::str::from_utf8(rest) {
            Ok(valid) => {
                for ch in valid.chars() {
                    push_visible(&mut text, &mut col, ch);
                }
                return text;
            }
            Err(error) => {
                let (valid, bad) = rest.split_at(error.valid_up_to());
                // The prefix is valid by construction of the error.
                for ch in core::str::from_utf8(valid).unwrap_or("").chars() {
                    push_visible(&mut text, &mut col, ch);
                }
                text.push('\u{FFFD}');
                col += 1;
                // Skip the malformed sequence — or everything left when
                // the input ends mid-character.
                let skip = error.error_len().unwrap_or(bad.len());
                rest = &bad[skip..];
            }
        }
    }
}

/// Append `ch` to `text` in visible form, advancing the column count.
fn push_visible(text: &mut String, col: &mut usize, ch: char) {
    if ch == '\t' {
        let stop = (*col / 8 + 1) * 8;
        while *col < stop {
            text.push(' ');
            *col += 1;
        }
    } else if ch.is_control() {
        text.push('\u{00B7}');
        *col += 1;
    } else {
        text.push(ch);
        *col += usize::from(rustos_vt::char_width(ch));
    }
}

impl TextView {
    /// Whether a background scan is live (the key loop then ticks it).
    #[must_use]
    pub fn ticking(&self) -> bool {
        self.job.is_some()
    }

    /// Cancel the live scan, keeping the view where it stands.
    pub fn cancel_job(&mut self) {
        self.job = None;
    }

    /// Start the goto-line scan seeking `target` (1-based, as typed).
    /// The scan anchors at the top row when its line is known and lies
    /// before the target, else at the start of the file.
    pub fn start_goto(&mut self, target: u64) {
        let target = target.saturating_sub(1);
        let (offset, line) = match self.top.line {
            Some(line) if line <= target => (self.top.offset, line),
            _ => (0, 0),
        };
        self.job = Some(TextJob::Goto(GotoScan {
            target,
            offset,
            line,
            row_start: offset,
        }));
    }

    /// Start a search for `text` from the row after the top. Returns
    /// `false` for an empty needle (nothing to search for).
    pub fn start_search(&mut self, fs: &mut dyn Fs, text: &str) -> bool {
        let Some(needle) = Needle::new(text) else {
            return false;
        };
        self.last_search = Some(String::from(text));
        self.last_hit = None;
        let (offset, line) = match read_row(fs, &self.path, self.top.offset) {
            Ok(Some((_, next, newline))) => (
                next,
                match (self.top.line, newline) {
                    (Some(line), true) => Some(line + 1),
                    (line, _) => line,
                },
            ),
            // The top row is the last (or unreadable): the scan starts
            // there and ends (or fails) on its first tick.
            _ => (self.top.offset, self.top.line),
        };
        self.job = Some(TextJob::Search(SearchScan {
            needle,
            offset,
            line,
            carry: Vec::new(),
        }));
        true
    }

    /// Repeat the last search from just past its previous hit. Returns
    /// `false` when no search has been made.
    pub fn search_next(&mut self, fs: &mut dyn Fs) -> bool {
        let Some(text) = self.last_search.clone() else {
            return false;
        };
        match self.last_hit {
            None => self.start_search(fs, &text),
            Some(hit) => {
                let Some(needle) = Needle::new(&text) else {
                    return false;
                };
                self.job = Some(TextJob::Search(SearchScan {
                    needle,
                    offset: hit + 1,
                    line: None,
                    carry: Vec::new(),
                }));
                true
            }
        }
    }

    /// Advance the live scan by up to `budget` bytes. [`JobOutcome::Pending`]
    /// means more ticks are needed; any other outcome ends the job.
    pub fn tick(&mut self, fs: &mut dyn Fs, budget: usize) -> JobOutcome {
        match self.job.take() {
            None => JobOutcome::Pending,
            Some(TextJob::Goto(scan)) => self.tick_goto(fs, scan, budget),
            Some(TextJob::Search(scan)) => self.tick_search(fs, scan, budget),
        }
    }

    fn tick_goto(&mut self, fs: &mut dyn Fs, mut scan: GotoScan, budget: usize) -> JobOutcome {
        if scan.line == scan.target {
            self.top = RowStart {
                offset: scan.offset,
                line: Some(scan.line),
            };
            return JobOutcome::Moved;
        }
        let mut spent = 0_usize;
        let mut buf = vec![0_u8; ROW_MAX];
        while spent < budget {
            let read = match fs.read(&self.path, scan.offset, &mut buf) {
                Ok(read) => read,
                Err(errno) => return JobOutcome::Failed(errno),
            };
            if read == 0 {
                return self.goto_past_end(fs, &scan);
            }
            spent += read;
            for (index, &byte) in buf[..read].iter().enumerate() {
                let at = scan.offset + index as u64;
                if byte == b'\n' {
                    scan.line += 1;
                    scan.row_start = at + 1;
                    if scan.line == scan.target {
                        self.top = RowStart {
                            offset: at + 1,
                            line: Some(scan.line),
                        };
                        return JobOutcome::Moved;
                    }
                } else if at + 1 - scan.row_start == ROW_MAX as u64 {
                    scan.row_start = at + 1;
                }
            }
            scan.offset += read as u64;
        }
        self.job = Some(TextJob::Goto(scan));
        JobOutcome::Pending
    }

    /// The goto target lies past the last line: land on the last row.
    fn goto_past_end(&mut self, fs: &mut dyn Fs, scan: &GotoScan) -> JobOutcome {
        if scan.row_start == scan.offset && scan.offset > 0 {
            // The file ends right after a newline; the last real row is
            // the one before it.
            match row_start_at(fs, &self.path, scan.offset - 1) {
                Ok(start) => {
                    self.top = RowStart {
                        offset: start,
                        line: Some(scan.line.saturating_sub(1)),
                    };
                }
                Err(errno) => return JobOutcome::Failed(errno),
            }
        } else {
            self.top = RowStart {
                offset: scan.row_start,
                line: Some(scan.line),
            };
        }
        JobOutcome::PastEnd
    }

    fn tick_search(&mut self, fs: &mut dyn Fs, mut scan: SearchScan, budget: usize) -> JobOutcome {
        let mut spent = 0_usize;
        let mut buf = vec![0_u8; ROW_MAX];
        while spent < budget {
            let read = match fs.read(&self.path, scan.offset, &mut buf) {
                Ok(read) => read,
                Err(errno) => return JobOutcome::Failed(errno),
            };
            if read == 0 {
                return JobOutcome::NotFound;
            }
            spent += read;
            let mut window = core::mem::take(&mut scan.carry);
            let carried = window.len();
            window.extend_from_slice(&buf[..read]);
            if let Some(index) = scan.needle.find_in(&window) {
                let hit = scan.offset - carried as u64 + index as u64;
                // Newlines before the hit: those counted before this
                // window plus those in the fresh bytes ahead of it. A hit
                // starting inside the carry hides none — the carry bytes
                // after it are matched needle bytes, and a typed needle
                // never contains a newline.
                let fresh_before_hit = index.saturating_sub(carried);
                let line = scan
                    .line
                    .map(|line| line + count_newlines(&buf[..fresh_before_hit]));
                return self.land_on_hit(fs, hit, line);
            }
            scan.line = scan.line.map(|line| line + count_newlines(&buf[..read]));
            let keep = scan.needle.len().saturating_sub(1).min(window.len());
            scan.carry = window[window.len() - keep..].to_vec();
            scan.offset += read as u64;
        }
        self.job = Some(TextJob::Search(scan));
        JobOutcome::Pending
    }

    /// Snap the top onto the row containing `hit` and remember it for `n`.
    fn land_on_hit(&mut self, fs: &mut dyn Fs, hit: u64, line: Option<u64>) -> JobOutcome {
        match row_start_at(fs, &self.path, hit) {
            Ok(start) => {
                self.top = RowStart {
                    offset: start,
                    line,
                };
                self.last_hit = Some(hit);
                JobOutcome::Moved
            }
            Err(errno) => JobOutcome::Failed(errno),
        }
    }
}

/// Newlines in `bytes`. The plain scan is deliberate: the window is a
/// bounded 4 KiB, not worth an external SIMD-counting dependency.
#[allow(clippy::naive_bytecount)]
fn count_newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b == b'\n').count() as u64
}

/// Split `text` into wrap segments of at most `cols` display columns,
/// never splitting a double-width glyph. An empty row yields one empty
/// segment.
fn wrap_segments(text: &str, cols: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    loop {
        let piece = rustos_vt::truncate_to_width(rest, cols);
        // A double-width glyph wider than the budget cannot make
        // progress; take one character rather than loop forever.
        let taken = if piece.is_empty() && !rest.is_empty() {
            let mut end = 1;
            while !rest.is_char_boundary(end) {
                end += 1;
            }
            &rest[..end]
        } else {
            piece
        };
        pieces.push(String::from(taken));
        rest = &rest[taken.len()..];
        if rest.is_empty() {
            return pieces;
        }
    }
}
