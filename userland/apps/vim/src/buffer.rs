//! The text buffer: a line vector with grouped, span-based undo and redo.
//!
//! A [`Buffer`] holds the file being edited as a vector of lines that is
//! never empty (an empty file is one empty line — the shape vim gives it).
//! Every mutation flows through one primitive, [`Buffer::replace_lines`],
//! which records the *inverse* of the change (the replaced lines) into the
//! undo group currently open, so undo memory is proportional to the lines a
//! change touched, never to the file.
//!
//! Undo is grouped the way vim groups it: everything between
//! [`Buffer::begin_edit`] and [`Buffer::commit_edit`] — one operator
//! application, one whole insert-mode session — undoes as a single step,
//! restoring the cursor to where the change began. Redo replays a group by
//! inverting it again; a fresh edit clears the redo stack.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A cursor location: 0-based line and 0-based character column.
///
/// The column counts characters, not bytes, so a multi-byte glyph is one
/// column step. In normal mode the editor clamps the column onto the line's
/// last character; during an insert the column may equal the line's length
/// (the cursor sits after the last character).
#[derive(Copy, Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Position {
    /// 0-based line index.
    pub line: usize,
    /// 0-based character column.
    pub col: usize,
}

impl Position {
    /// A position at the given line and column.
    #[must_use]
    pub const fn new(line: usize, col: usize) -> Position {
        Position { line, col }
    }
}

/// One reversible line-span replacement: at [`LineEdit::start`], the lines
/// in [`LineEdit::old`] were replaced by [`LineEdit::new_len`] lines.
/// Undoing it captures the current occupants and splices `old` back.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LineEdit {
    start: usize,
    old: Vec<String>,
    new_len: usize,
}

/// One undo step: the edits of a single change, applied and reverted as a
/// unit, plus the cursor positions bracketing the change.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UndoGroup {
    edits: Vec<LineEdit>,
    cursor_before: Position,
    cursor_after: Position,
}

/// The buffer under edit: named lines plus the undo/redo history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Buffer {
    lines: Vec<String>,
    name: Option<String>,
    modified: bool,
    readonly: bool,
    undo: Vec<UndoGroup>,
    redo: Vec<UndoGroup>,
    open: Option<UndoGroup>,
}

impl Buffer {
    /// An empty, unnamed buffer: one empty line, unmodified.
    #[must_use]
    pub fn empty() -> Buffer {
        Buffer {
            lines: vec![String::new()],
            name: None,
            modified: false,
            readonly: false,
            undo: Vec::new(),
            redo: Vec::new(),
            open: None,
        }
    }

    /// A buffer holding `text` split into lines, edited under `name`.
    ///
    /// The split follows vim's file model: each `\n` ends a line, and a
    /// trailing `\n` is the last line's terminator rather than an extra
    /// empty line. An empty file is one empty line. A `\r\n` ending is
    /// preserved as a literal `\r` in the line (the file is treated as
    /// Unix-format text, like `vim -b`'s honest view of foreign endings).
    #[must_use]
    pub fn from_text(name: Option<String>, text: &str) -> Buffer {
        let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        Buffer {
            lines,
            name,
            modified: false,
            readonly: false,
            undo: Vec::new(),
            redo: Vec::new(),
            open: None,
        }
    }

    /// The buffer's contents as file bytes: every line terminated by `\n`.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    /// The file name this buffer edits, if any.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Rename the buffer (the `:w file` / `:e file` binding).
    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    /// Whether the buffer has unwritten changes.
    #[must_use]
    pub const fn is_modified(&self) -> bool {
        self.modified
    }

    /// Mark the buffer written (clean) or dirty.
    pub fn set_modified(&mut self, modified: bool) {
        self.modified = modified;
    }

    /// Whether the buffer refuses writes (`vim -R`).
    #[must_use]
    pub const fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Set the readonly posture.
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    /// The number of lines; at least 1 by invariant.
    #[must_use]
    pub fn len_lines(&self) -> usize {
        self.lines.len()
    }

    /// The line at `index`, or the empty string past the end (a total
    /// accessor so hostile positions never panic).
    #[must_use]
    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map_or("", String::as_str)
    }

    /// The character length of the line at `index`.
    #[must_use]
    pub fn line_len(&self, index: usize) -> usize {
        self.line(index).chars().count()
    }

    /// All lines in `start..end`, clamped to the buffer.
    #[must_use]
    pub fn lines_in(&self, start: usize, end: usize) -> &[String] {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        &self.lines[start..end]
    }

    /// Open an undo group at `cursor`; edits accumulate into it until
    /// [`Buffer::commit_edit`]. Opening while a group is already open keeps
    /// the existing group (nested openers join the outer change).
    pub fn begin_edit(&mut self, cursor: Position) {
        if self.open.is_none() {
            self.open = Some(UndoGroup {
                edits: Vec::new(),
                cursor_before: cursor,
                cursor_after: cursor,
            });
        }
    }

    /// Close the open undo group at `cursor`. A group that recorded no
    /// edits is discarded; otherwise it becomes one undo step and clears
    /// the redo stack.
    pub fn commit_edit(&mut self, cursor: Position) {
        if let Some(mut group) = self.open.take() {
            if group.edits.is_empty() {
                return;
            }
            group.cursor_after = cursor;
            self.undo.push(group);
            self.redo.clear();
        }
    }

    /// Whether an undo group is currently open.
    #[must_use]
    pub const fn edit_open(&self) -> bool {
        self.open.is_some()
    }

    /// Replace the lines in `start..end` with `new`, recording the inverse
    /// into the open undo group (or as a single-edit group when none is
    /// open). Out-of-range bounds are clamped. Removing every line leaves
    /// the one-empty-line shape only via the caller passing `new` with one
    /// empty line — the primitive itself preserves whatever it is given,
    /// except that a buffer emptied entirely is restored to one empty line
    /// to keep the invariant.
    pub fn replace_lines(&mut self, start: usize, end: usize, new: Vec<String>) {
        let end = end.min(self.lines.len());
        let start = start.min(end);
        let old: Vec<String> = self.lines[start..end].to_vec();
        if old == new {
            return;
        }
        let solo = self.open.is_none();
        if solo {
            let at = Position::new(start, 0);
            self.begin_edit(at);
        }
        let new_len = new.len();
        self.lines.splice(start..end, new);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        if let Some(group) = self.open.as_mut() {
            group.edits.push(LineEdit {
                start,
                old,
                new_len,
            });
        }
        self.modified = true;
        if solo {
            let at = Position::new(start, 0);
            self.commit_edit(at);
        }
    }

    /// Undo the newest undo group. Returns the cursor position from before
    /// the change, or [`None`] when the history is empty (vim's "Already at
    /// oldest change").
    pub fn undo(&mut self) -> Option<Position> {
        let group = self.undo.pop()?;
        let inverted = self.invert(&group);
        let at = group.cursor_before;
        self.redo.push(inverted);
        self.modified = true;
        Some(at)
    }

    /// Redo the newest undone group. Returns the cursor position from after
    /// the change, or [`None`] when nothing was undone ("Already at newest
    /// change").
    pub fn redo(&mut self) -> Option<Position> {
        let group = self.redo.pop()?;
        let inverted = self.invert(&group);
        let at = group.cursor_after;
        self.undo.push(inverted);
        self.modified = true;
        Some(at)
    }

    /// Apply the inverse of `group` to the lines and return the group that
    /// re-applies it: each edit, walked newest-first, has its current
    /// occupants captured and its old lines spliced back.
    ///
    /// The produced group stores its edits in the order they were applied
    /// here, so inverting *it* (which also walks newest-first) replays
    /// them in the opposite — original chronological — order. That keeps
    /// undo→redo→undo cycles exact for groups whose edits touch the same
    /// span repeatedly (an insert session retouching one line).
    fn invert(&mut self, group: &UndoGroup) -> UndoGroup {
        let mut inverse = Vec::with_capacity(group.edits.len());
        for edit in group.edits.iter().rev() {
            let end = (edit.start + edit.new_len).min(self.lines.len());
            let start = edit.start.min(end);
            let current: Vec<String> = self.lines[start..end].to_vec();
            self.lines.splice(start..end, edit.old.clone());
            if self.lines.is_empty() {
                self.lines.push(String::new());
            }
            inverse.push(LineEdit {
                start: edit.start,
                old: current,
                new_len: edit.old.len(),
            });
        }
        UndoGroup {
            edits: inverse,
            cursor_before: group.cursor_before,
            cursor_after: group.cursor_after,
        }
    }
}
