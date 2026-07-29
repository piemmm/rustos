//! The hex viewer: the classic offset / 16-byte-hex / ASCII dump of any
//! file, paged through the [`Fs`] seam.
//!
//! Offsets are 64-bit throughout, so a file larger than 4 GiB pages
//! correctly. The view keeps only its top offset and re-reads one
//! screenful per refresh; the byte-sequence and text searches run as
//! byte-budgeted background jobs ticked by the key loop, exactly like the
//! text view's, so a huge file never freezes the session.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::Errno;

use crate::fs::Fs;
use crate::view_text::JobOutcome;

/// Bytes per dump row.
pub const HEX_COLS: usize = 16;

/// [`HEX_COLS`] as an offset stride (the one definition, widened).
const HEX_STRIDE: u64 = HEX_COLS as u64;

/// Bytes one search read covers.
const WINDOW: usize = 4096;

/// What a hex search looks for: an exact byte sequence (a `0x…` spelling)
/// or literal text matched ASCII-case-insensitively.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HexPattern {
    /// Exact bytes.
    Bytes(Vec<u8>),
    /// Text, matched case-insensitively (stored lowered).
    Text(Vec<u8>),
}

impl HexPattern {
    /// Parse a typed pattern: `0x` followed by an even run of hex digits
    /// searches those exact bytes; anything else searches its text.
    /// `None` for an empty or malformed spelling.
    #[must_use]
    pub fn parse(typed: &str) -> Option<Self> {
        if typed.is_empty() {
            return None;
        }
        if let Some(digits) = typed
            .strip_prefix("0x")
            .or_else(|| typed.strip_prefix("0X"))
        {
            if digits.is_empty() || digits.len() % 2 != 0 {
                return None;
            }
            let mut bytes = Vec::with_capacity(digits.len() / 2);
            let raw = digits.as_bytes();
            for pair in raw.as_chunks::<2>().0 {
                let high = hex_value(pair[0])?;
                let low = hex_value(pair[1])?;
                bytes.push(high << 4 | low);
            }
            return Some(Self::Bytes(bytes));
        }
        Some(Self::Text(
            typed.bytes().map(|b| b.to_ascii_lowercase()).collect(),
        ))
    }

    /// The pattern's length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) | Self::Text(bytes) => bytes.len(),
        }
    }

    /// Never empty by construction, but spelled out for callers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The index of the pattern's first occurrence in `window`.
    #[must_use]
    pub fn find_in(&self, window: &[u8]) -> Option<usize> {
        let (needle, fold): (&[u8], bool) = match self {
            Self::Bytes(bytes) => (bytes, false),
            Self::Text(bytes) => (bytes, true),
        };
        let n = needle.len();
        if n == 0 || window.len() < n {
            return None;
        }
        (0..=(window.len() - n)).find(|&start| {
            window[start..start + n]
                .iter()
                .zip(needle)
                .all(|(&b, &want)| {
                    if fold {
                        b.to_ascii_lowercase() == want
                    } else {
                        b == want
                    }
                })
        })
    }
}

/// The value of an ASCII hex digit.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// The live background byte search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HexScan {
    /// What is being searched for.
    pub pattern: HexPattern,
    /// The next read offset.
    pub offset: u64,
    /// The last `pattern.len() - 1` bytes of the previous window.
    pub carry: Vec<u8>,
}

/// The hex-view state: the file, the top row, and the search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HexView {
    /// The viewed file's path.
    pub path: String,
    /// Apparent file length from the listing (display only; the end of
    /// file is governed by short reads, never by this figure).
    pub size: u64,
    /// Offset of the top row, a multiple of [`HEX_COLS`].
    pub top: u64,
    /// The last typed pattern, repeated by `n`.
    pub last_search: Option<String>,
    /// The offset of the last hit, so `n` continues past it.
    pub last_hit: Option<u64>,
    /// The live background search, if one is running.
    pub job: Option<HexScan>,
    /// The bytes of the current page, refreshed after every change.
    pub bytes: Vec<u8>,
    /// Whether the page's last byte ends the file (for the status line).
    pub at_end: bool,
    /// Display rows of the last refresh — the paging unit; a nominal
    /// figure until the first refresh records the real screen.
    pub viewport_rows: usize,
}

impl HexView {
    /// A view of `path` starting at offset `0`.
    #[must_use]
    pub fn new(path: &str, size: u64) -> Self {
        Self {
            path: String::from(path),
            size,
            top: 0,
            last_search: None,
            last_hit: None,
            job: None,
            bytes: Vec::new(),
            at_end: false,
            viewport_rows: 22,
        }
    }

    /// A view of `path` whose top row contains `offset` (a jump from the
    /// text view).
    #[must_use]
    pub fn at_offset(path: &str, size: u64, offset: u64) -> Self {
        let mut view = Self::new(path, size);
        view.top = align_down(offset);
        view
    }

    /// Re-read the page under the top row: up to `rows` dump rows of
    /// bytes for the renderer. A refused read empties the page and
    /// returns the error — stale bytes are never shown as live.
    pub fn refresh(&mut self, fs: &mut dyn Fs, rows: usize) -> Result<(), Errno> {
        self.viewport_rows = rows.max(1);
        self.bytes.clear();
        self.at_end = false;
        let want = rows.saturating_mul(HEX_COLS);
        let mut buf = vec![0_u8; want];
        let mut filled = 0_usize;
        // A short read means end of file, but loop for the seam contract
        // ("short only at end") to stay byte-exact if a backing splits.
        while filled < want {
            match fs.read(&self.path, self.top + filled as u64, &mut buf[filled..]) {
                Ok(0) => {
                    self.at_end = true;
                    break;
                }
                Ok(read) => filled += read,
                Err(errno) => return Err(errno),
            }
        }
        buf.truncate(filled);
        self.bytes = buf;
        Ok(())
    }

    /// Scroll by `rows` dump rows (negative is up), stopping at the
    /// first row and at the last row holding any byte.
    pub fn scroll(&mut self, fs: &mut dyn Fs, rows: i64) -> Result<(), Errno> {
        if rows.is_negative() {
            let back = rows.unsigned_abs().saturating_mul(HEX_STRIDE);
            self.top = self.top.saturating_sub(back);
            return Ok(());
        }
        let mut probe = [0_u8; 1];
        for _ in 0..rows {
            let next = self.top.saturating_add(HEX_STRIDE);
            if fs.read(&self.path, next, &mut probe)? == 0 {
                break;
            }
            self.top = next;
        }
        Ok(())
    }

    /// Jump to the top of the file.
    pub fn go_home(&mut self) {
        self.top = 0;
    }

    /// Jump so the file's last rows fill a `page`-row screen.
    pub fn go_end(&mut self, fs: &mut dyn Fs, page: usize) -> Result<(), Errno> {
        let end = self.confirmed_end(fs)?;
        let last_row = align_down(end.saturating_sub(1));
        let back = (page.saturating_sub(1) as u64).saturating_mul(HEX_STRIDE);
        self.top = last_row.saturating_sub(back);
        Ok(())
    }

    /// Jump to the row containing `offset` (clamped to the file's end).
    pub fn go_to(&mut self, fs: &mut dyn Fs, offset: u64) -> Result<(), Errno> {
        let end = self.confirmed_end(fs)?;
        let clamped = offset.min(end.saturating_sub(1));
        self.top = align_down(clamped);
        Ok(())
    }

    /// The end offset near the listed size, confirmed by bounded probe
    /// reads (a stale listing never strands the view; a file grown far
    /// past its listing is followed by scrolling on).
    fn confirmed_end(&self, fs: &mut dyn Fs) -> Result<u64, Errno> {
        const PROBE_WINDOWS: usize = 64;
        let mut end = self.size;
        let mut probe = vec![0_u8; WINDOW];
        for _ in 0..PROBE_WINDOWS {
            let read = fs.read(&self.path, end, &mut probe)?;
            if read == 0 {
                break;
            }
            end += read as u64;
        }
        Ok(end)
    }

    /// Whether a background search is live (the key loop then ticks it).
    #[must_use]
    pub fn ticking(&self) -> bool {
        self.job.is_some()
    }

    /// Cancel the live search, keeping the view where it stands.
    pub fn cancel_job(&mut self) {
        self.job = None;
    }

    /// Start a search for `typed` from the byte after the top row.
    /// Returns `false` for an empty or malformed pattern.
    pub fn start_search(&mut self, typed: &str) -> bool {
        let Some(pattern) = HexPattern::parse(typed) else {
            return false;
        };
        self.last_search = Some(String::from(typed));
        self.last_hit = None;
        self.job = Some(HexScan {
            pattern,
            offset: self.top.saturating_add(HEX_STRIDE),
            carry: Vec::new(),
        });
        true
    }

    /// Repeat the last search from just past its previous hit. Returns
    /// `false` when no search has been made.
    pub fn search_next(&mut self) -> bool {
        let Some(text) = self.last_search.clone() else {
            return false;
        };
        match self.last_hit {
            None => self.start_search(&text),
            Some(hit) => {
                let Some(pattern) = HexPattern::parse(&text) else {
                    return false;
                };
                self.job = Some(HexScan {
                    pattern,
                    offset: hit + 1,
                    carry: Vec::new(),
                });
                true
            }
        }
    }

    /// Advance the live search by up to `budget` bytes.
    pub fn tick(&mut self, fs: &mut dyn Fs, budget: usize) -> JobOutcome {
        let Some(mut scan) = self.job.take() else {
            return JobOutcome::Pending;
        };
        let mut spent = 0_usize;
        let mut buf = vec![0_u8; WINDOW];
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
            if let Some(index) = scan.pattern.find_in(&window) {
                let hit = scan.offset - carried as u64 + index as u64;
                self.top = align_down(hit);
                self.last_hit = Some(hit);
                return JobOutcome::Moved;
            }
            let keep = scan.pattern.len().saturating_sub(1).min(window.len());
            scan.carry = window[window.len() - keep..].to_vec();
            scan.offset += read as u64;
        }
        self.job = Some(scan);
        JobOutcome::Pending
    }
}

/// Round `offset` down to its dump row.
#[must_use]
pub fn align_down(offset: u64) -> u64 {
    offset - offset % HEX_STRIDE
}

/// Parse a typed goto offset: decimal, or hex with a `0x` prefix.
#[must_use]
pub fn parse_offset(typed: &str) -> Option<u64> {
    if let Some(digits) = typed
        .strip_prefix("0x")
        .or_else(|| typed.strip_prefix("0X"))
    {
        return u64::from_str_radix(digits, 16).ok();
    }
    typed.parse().ok()
}

/// Hex digits spelling the last offset of a `size`-byte file, at least
/// eight — an ordinary file's dump row fits an 80-column screen, while a
/// file past 4 GiB widens to carry its full 64-bit offsets.
#[must_use]
pub fn offset_digits(size: u64) -> usize {
    let mut digits = 1;
    let mut rest = size.saturating_sub(1) >> 4;
    while rest != 0 {
        digits += 1;
        rest >>= 4;
    }
    digits.max(8)
}

/// Format one dump row of the page for display: the `digits`-wide hex
/// offset, the byte pairs (grouped 8+8), and the ASCII column (printable
/// ASCII shown, everything else `.` — file bytes never reach the
/// terminal as raw escapes).
#[must_use]
pub fn dump_row(top: u64, bytes: &[u8], row: usize, digits: usize) -> Option<String> {
    use core::fmt::Write as _;
    let start = row * HEX_COLS;
    if start >= bytes.len() {
        return None;
    }
    let slice = &bytes[start..bytes.len().min(start + HEX_COLS)];
    let offset = top + start as u64;
    let mut hex = String::new();
    for (i, byte) in slice.iter().enumerate() {
        if i == 8 {
            hex.push(' ');
        }
        // Writing into a String cannot fail.
        let _ = write!(hex, "{byte:02x} ");
    }
    let ascii: String = slice
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    // 16 bytes at 3 columns each plus the mid-row gap.
    Some(format!("{offset:0digits$x}  {hex:<49} |{ascii}|"))
}
