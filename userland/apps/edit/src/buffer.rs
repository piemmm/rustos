//! The line-oriented text buffer the editor works on.
//!
//! A [`TextBuffer`] is the decoded, editable form of one text file: a
//! vector of lines (each a `String` without its line terminator), the
//! remembered presence of the file's final newline (so an edited file
//! round-trips byte-exactly when untouched at the end), and a modified
//! flag every mutation sets.
//!
//! Decoding is fail-closed: the bytes must be UTF-8 text within the
//! [`MAX_FILE_BYTES`] bound, and the only control characters accepted are
//! the line terminators (`\n`, `\r\n`) and the horizontal tab. Anything
//! else — a `NUL`, a lone `\r`, an escape byte — marks the input as not a
//! text file this editor can honestly edit, and the load is refused rather
//! than the data silently mangled.
//!
//! Two classic conversions are applied on load, and both are *reported* to
//! the caller through [`LoadNotices`] so the session can tell the user
//! (never a silent alteration): CRLF line endings become LF, and tab
//! characters are expanded to spaces at [`TAB_STOP`]-column stops — the
//! same conversion the `QuickBasic` editor applied, because the buffer
//! addresses columns, not tab widths.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_vt::char_width;

/// The largest file the editor loads, a fail-closed validation bound on
/// untrusted input (the whole buffer lives in memory, line by line): 16 MiB
/// of text is far beyond any hand-edited file, and a larger input is far
/// more likely a mistake (a disk image, a log) than a document.
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Tab stops every eight columns, the terminal convention.
pub const TAB_STOP: usize = 8;

/// What the loader changed while decoding, so the session can say so on
/// the status line — a conversion is applied loudly, never silently.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoadNotices {
    /// Tab characters were expanded to spaces at [`TAB_STOP`] stops.
    pub tabs_expanded: bool,
    /// CRLF line endings were converted to LF.
    pub crlf_converted: bool,
}

/// Why a byte stream was refused as this editor's input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// The file exceeds [`MAX_FILE_BYTES`].
    TooLarge,
    /// The bytes are not UTF-8 text, or carry a control character other
    /// than `\n`, `\r\n`, or tab (a binary file, or a lone `\r`).
    NotText,
}

/// The editable, in-memory form of one text file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextBuffer {
    /// The lines, without terminators. Never empty: an empty buffer is one
    /// empty line, so the cursor always has a line to rest on.
    lines: Vec<String>,
    /// Whether the file ends with a newline, preserved from load so an
    /// unedited end of file round-trips exactly.
    final_newline: bool,
    /// Set by every mutation, cleared when the buffer is saved.
    modified: bool,
}

impl TextBuffer {
    /// A new, empty, unmodified buffer: one empty line, saved with a final
    /// newline (the POSIX text-file convention for a file this editor
    /// creates).
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: alloc::vec![String::new()],
            final_newline: true,
            modified: false,
        }
    }

    /// Decode `bytes` into a buffer, reporting the conversions applied.
    ///
    /// # Errors
    ///
    /// * [`DecodeError::TooLarge`] — the input exceeds [`MAX_FILE_BYTES`].
    /// * [`DecodeError::NotText`] — the input is not UTF-8, or carries a
    ///   control character other than `\n`, `\r\n`, or tab.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, LoadNotices), DecodeError> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(DecodeError::TooLarge);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| DecodeError::NotText)?;

        let mut notices = LoadNotices::default();
        let mut lines = Vec::new();
        let mut line = String::new();
        // The display column tab expansion is measured against; wide glyphs
        // count their real two columns so a tab after one lands where the
        // terminal would have put it.
        let mut column = 0usize;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\n' => {
                    lines.push(core::mem::take(&mut line));
                    column = 0;
                }
                '\r' => {
                    // Only the CRLF pair is a line ending; a lone `\r` is a
                    // control character this editor cannot represent, so
                    // the load fails closed rather than guessing.
                    if chars.next_if_eq(&'\n').is_none() {
                        return Err(DecodeError::NotText);
                    }
                    notices.crlf_converted = true;
                    lines.push(core::mem::take(&mut line));
                    column = 0;
                }
                '\t' => {
                    notices.tabs_expanded = true;
                    let pad = TAB_STOP - (column % TAB_STOP);
                    for _ in 0..pad {
                        line.push(' ');
                    }
                    column += pad;
                }
                _ if ch.is_control() => return Err(DecodeError::NotText),
                _ => {
                    line.push(ch);
                    column += usize::from(char_width(ch));
                }
            }
        }
        // A trailing newline leaves `line` empty *and* is remembered, so
        // `abc\n` is one line ending with a newline, not two lines.
        let final_newline = !bytes.is_empty() && bytes.last() == Some(&b'\n');
        if !final_newline || lines.is_empty() {
            lines.push(line);
        }
        Ok((
            Self {
                lines,
                final_newline,
                modified: false,
            },
            notices,
        ))
    }

    /// Encode the buffer back to file bytes: lines joined with LF, plus
    /// the remembered final newline.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push(b'\n');
            }
            out.extend_from_slice(line.as_bytes());
        }
        // One empty line with no final newline is the empty file; with it,
        // a file holding exactly one blank line — the exact inverse of
        // `from_bytes`, so an untouched buffer round-trips byte for byte.
        if self.final_newline {
            out.push(b'\n');
        }
        out
    }

    /// The number of lines (always at least one).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The text of line `row`, or the empty string for a row past the end
    /// (a defensive fallback; callers stay in range).
    #[must_use]
    pub fn line(&self, row: usize) -> &str {
        self.lines.get(row).map_or("", String::as_str)
    }

    /// The number of characters on line `row`.
    #[must_use]
    pub fn line_chars(&self, row: usize) -> usize {
        self.line(row).chars().count()
    }

    /// Whether the buffer differs from its last loaded/saved state.
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.modified
    }

    /// Mark the buffer clean — it was just saved.
    pub fn mark_saved(&mut self) {
        self.modified = false;
    }

    /// Insert `ch` at character `col` of line `row`; in `overwrite` mode a
    /// character already at that column is replaced instead. Out-of-range
    /// positions are clamped to the line end.
    pub fn insert_char(&mut self, row: usize, col: usize, ch: char, overwrite: bool) {
        let Some(line) = self.lines.get_mut(row) else {
            return;
        };
        let start = byte_of_char(line, col);
        if overwrite && start < line.len() {
            let end = byte_of_char(line, col + 1);
            line.replace_range(start..end, ch.encode_utf8(&mut [0u8; 4]));
        } else {
            line.insert(start, ch);
        }
        self.modified = true;
    }

    /// Split line `row` at character `col`: the tail becomes a new line
    /// after it (the Enter key).
    pub fn split_line(&mut self, row: usize, col: usize) {
        let Some(line) = self.lines.get_mut(row) else {
            return;
        };
        let at = byte_of_char(line, col);
        let tail = line.split_off(at);
        self.lines.insert(row + 1, tail);
        self.modified = true;
    }

    /// Delete the character at `col` of line `row`; at the line end, join
    /// the next line onto this one instead (the Delete key). Returns
    /// `false` when there was nothing to delete (the end of the buffer).
    pub fn delete_at(&mut self, row: usize, col: usize) -> bool {
        if col < self.line_chars(row) {
            let Some(line) = self.lines.get_mut(row) else {
                return false;
            };
            let start = byte_of_char(line, col);
            let end = byte_of_char(line, col + 1);
            line.replace_range(start..end, "");
            self.modified = true;
            return true;
        }
        if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            if let Some(line) = self.lines.get_mut(row) {
                line.push_str(&next);
            }
            self.modified = true;
            return true;
        }
        false
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// The byte offset of character `col` in `line`, clamped to the line end.
pub(crate) fn byte_of_char(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

/// The display width, in terminal columns, of the first `col` characters
/// of `line` — the cursor's screen column for a cursor at character `col`.
#[must_use]
pub fn width_of_prefix(line: &str, col: usize) -> usize {
    line.chars()
        .take(col)
        .map(|ch| usize::from(char_width(ch)))
        .sum()
}
