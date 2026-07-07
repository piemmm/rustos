//! The editor's I/O-free view state and its event handling.
//!
//! The [`Model`] owns the [`TextBuffer`], the cursor, the scroll offsets,
//! and the interaction [`Mode`] (editing, the menu bar, a one-line prompt,
//! or a confirm question). Every keystroke flows through
//! [`Model::handle_event`], which mutates the state and performs file
//! loads/saves through the injected [`Fs`] seam — so the whole editor is
//! host-testable against an in-memory filesystem and a scripted terminal.
//!
//! File failures are session events, not fatal errors: a refused open or
//! save posts a status-line notice and the user keeps their buffer.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_curses::Event;

use crate::buffer::{width_of_prefix, DecodeError, TextBuffer};

/// The filesystem seam the editor loads and saves through. The production
/// implementation wraps the kernel-authorised `fs_*` syscalls (adding no
/// authority of its own — every path and per-inode check stays
/// kernel-side); tests inject an in-memory map.
pub trait Fs {
    /// Read the whole of `path`.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the open or read was refused with.
    fn read(&self, path: &str) -> Result<Vec<u8>, Errno>;

    /// Create or truncate `path` and write `bytes` as its whole content.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the open or a write was refused with.
    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Errno>;
}

/// What a one-line status prompt is asking for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptIntent {
    /// `File > Open`: the path of the file to load.
    Open,
    /// `File > Save As` (or a save of an unnamed buffer): the path to
    /// write.
    SaveAs,
    /// `Search > Find`: the text to look for.
    Find,
}

/// The action a "save changes first?" question guards, resumed after the
/// user's answer (and after the save it may trigger) is settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pending {
    /// `File > New`: replace the buffer with an empty one.
    New,
    /// `File > Open`: prompt for and load another file.
    Open,
    /// `File > Exit`: leave the editor.
    Exit,
}

/// The editor's interaction mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Keys edit the buffer.
    Edit,
    /// The menu bar is open on `menu`, with `item` highlighted.
    Menu {
        /// Index into [`MENUS`].
        menu: usize,
        /// Index into that menu's item list.
        item: usize,
    },
    /// A one-line prompt on the status row.
    Prompt {
        /// What the prompt is asking for.
        intent: PromptIntent,
        /// The text typed so far.
        input: String,
    },
    /// The "save changes first?" question for the [`Pending`] action.
    Confirm(Pending),
}

/// What the caller's loop should do after an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Keep going: redraw and wait for the next event.
    Continue,
    /// The user left the editor; end the session cleanly.
    Quit,
}

/// One menu-bar entry: its title and its items (each an item label).
pub struct Menu {
    /// The title shown on the bar.
    pub title: &'static str,
    /// The item labels, in order.
    pub items: &'static [&'static str],
}

/// The menu bar: `File` and `Search`, in the `QuickBasic` editor's shape.
pub const MENUS: &[Menu] = &[
    Menu {
        title: "File",
        items: &["New", "Open...", "Save", "Save As...", "Exit"],
    },
    Menu {
        title: "Search",
        items: &["Find...", "Repeat Last Find"],
    },
];

/// The editor state.
pub struct Model {
    buffer: TextBuffer,
    /// The file the buffer belongs to, once it has a name.
    path: Option<String>,
    /// Cursor position: line index and character index within it.
    cursor_row: usize,
    cursor_col: usize,
    /// The column the cursor aims for when moving vertically across
    /// shorter lines (the classic "sticky column").
    sticky_col: usize,
    /// First buffer row / leftmost display column shown.
    scroll_top: usize,
    scroll_left: usize,
    /// Text-area size in rows/columns, set by the renderer each pass.
    view_rows: usize,
    view_cols: usize,
    /// Overwrite mode (the Insert key toggles it).
    overwrite: bool,
    mode: Mode,
    /// A transient status-line message, cleared by the next edit key.
    notice: Option<String>,
    /// The last successful `Find` needle, for `Repeat Last Find`.
    last_find: Option<String>,
    /// Whether the key-summary overlay is showing (F1).
    help_visible: bool,
    /// A save triggered by a confirm question resumes this action.
    pending: Option<Pending>,
}

impl Model {
    /// A fresh editor on an empty, unnamed buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: TextBuffer::new(),
            path: None,
            cursor_row: 0,
            cursor_col: 0,
            sticky_col: 0,
            scroll_top: 0,
            scroll_left: 0,
            view_rows: 1,
            view_cols: 1,
            overwrite: false,
            mode: Mode::Edit,
            notice: None,
            last_find: None,
            help_visible: false,
            pending: None,
        }
    }

    /// Load the file named on the command line, or start a named new
    /// buffer when it does not exist yet (the editor creates it on the
    /// first save).
    ///
    /// # Errors
    ///
    /// Any refusal other than "not found" — permission, a non-text file,
    /// an over-large file — as the message the caller reports; opening an
    /// unreadable file must fail loudly, never open an empty buffer over
    /// real data.
    pub fn open_initial(&mut self, fs: &dyn Fs, path: &str) -> Result<(), String> {
        match fs.read(path) {
            Ok(bytes) => match TextBuffer::from_bytes(&bytes) {
                Ok((buffer, notices)) => {
                    self.buffer = buffer;
                    self.notice = load_notice(notices);
                }
                Err(err) => return Err(format!("{path}: {}", decode_message(err))),
            },
            Err(Errno::NotFound) => {
                self.notice = Some(String::from("New file"));
            }
            Err(errno) => return Err(format!("{path}: {errno}")),
        }
        self.path = Some(String::from(path));
        Ok(())
    }

    /// The open file's name, when the buffer has one.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// The buffer being edited.
    #[must_use]
    pub fn buffer(&self) -> &TextBuffer {
        &self.buffer
    }

    /// The cursor's line index.
    #[must_use]
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// The cursor's character index within its line.
    #[must_use]
    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// The first buffer row shown.
    #[must_use]
    pub fn scroll_top(&self) -> usize {
        self.scroll_top
    }

    /// The leftmost display column shown.
    #[must_use]
    pub fn scroll_left(&self) -> usize {
        self.scroll_left
    }

    /// Whether typed characters replace instead of insert.
    #[must_use]
    pub fn overwrite(&self) -> bool {
        self.overwrite
    }

    /// The interaction mode.
    #[must_use]
    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// The transient status-line message, if any.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Whether the key-summary overlay is showing.
    #[must_use]
    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    /// Record the text area's size and pull the scroll window over the
    /// cursor. The renderer calls this each pass, so a resized terminal
    /// re-clamps on the next draw.
    pub fn set_viewport(&mut self, rows: usize, cols: usize) {
        self.view_rows = rows.max(1);
        self.view_cols = cols.max(1);
        self.scroll_to_cursor();
    }

    /// React to one input event, loading and saving through `fs` where the
    /// user asks for it.
    pub fn handle_event(&mut self, event: &Event, fs: &dyn Fs) -> Action {
        // Any key dismisses the key-summary overlay; the key itself is
        // consumed, so a stray character never edits the buffer unseen.
        if self.help_visible {
            self.help_visible = false;
            return Action::Continue;
        }
        match self.mode.clone() {
            Mode::Edit => self.handle_edit(event, fs),
            Mode::Menu { menu, item } => self.handle_menu(event, fs, menu, item),
            Mode::Prompt { intent, input } => self.handle_prompt(event, fs, intent, input),
            Mode::Confirm(pending) => self.handle_confirm(event, fs, pending),
        }
    }

    /// Keys while editing.
    fn handle_edit(&mut self, event: &Event, fs: &dyn Fs) -> Action {
        // A transient notice lives until the next keystroke.
        self.notice = None;
        match event {
            Event::Char(ch) if !ch.is_control() => {
                self.buffer
                    .insert_char(self.cursor_row, self.cursor_col, *ch, self.overwrite);
                self.cursor_col += 1;
                self.sticky_col = self.cursor_col;
            }
            Event::Enter => {
                self.buffer.split_line(self.cursor_row, self.cursor_col);
                self.cursor_row += 1;
                self.cursor_col = 0;
                self.sticky_col = 0;
            }
            Event::Tab => {
                // Insert spaces to the next tab stop, the same expansion
                // the loader applies — the buffer never holds a tab.
                let col = width_of_prefix(self.buffer.line(self.cursor_row), self.cursor_col);
                let pad = crate::buffer::TAB_STOP - (col % crate::buffer::TAB_STOP);
                for _ in 0..pad {
                    self.buffer
                        .insert_char(self.cursor_row, self.cursor_col, ' ', false);
                    self.cursor_col += 1;
                }
                self.sticky_col = self.cursor_col;
            }
            Event::Backspace => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                    self.buffer.delete_at(self.cursor_row, self.cursor_col);
                } else if self.cursor_row > 0 {
                    let join_col = self.buffer.line_chars(self.cursor_row - 1);
                    self.cursor_row -= 1;
                    self.cursor_col = join_col;
                    self.buffer.delete_at(self.cursor_row, join_col);
                }
                self.sticky_col = self.cursor_col;
            }
            Event::Delete => {
                self.buffer.delete_at(self.cursor_row, self.cursor_col);
            }
            Event::Up => self.move_vertical(-1),
            Event::Down => self.move_vertical(1),
            Event::PageUp => {
                let rows = isize::try_from(self.view_rows).unwrap_or(1);
                self.move_vertical(-rows);
            }
            Event::PageDown => {
                let rows = isize::try_from(self.view_rows).unwrap_or(1);
                self.move_vertical(rows);
            }
            Event::Left => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                } else if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.cursor_col = self.buffer.line_chars(self.cursor_row);
                }
                self.sticky_col = self.cursor_col;
            }
            Event::Right => {
                if self.cursor_col < self.buffer.line_chars(self.cursor_row) {
                    self.cursor_col += 1;
                } else if self.cursor_row + 1 < self.buffer.line_count() {
                    self.cursor_row += 1;
                    self.cursor_col = 0;
                }
                self.sticky_col = self.cursor_col;
            }
            Event::Home => {
                self.cursor_col = 0;
                self.sticky_col = 0;
            }
            Event::End => {
                self.cursor_col = self.buffer.line_chars(self.cursor_row);
                self.sticky_col = self.cursor_col;
            }
            Event::Insert => self.overwrite = !self.overwrite,
            Event::Paste(text) => self.paste(text),
            Event::Function(1) => self.help_visible = true,
            Event::Function(2) => return self.save(fs),
            Event::Function(3) => self.find_next(),
            Event::Function(10) => self.mode = Mode::Menu { menu: 0, item: 0 },
            // Control characters, other function keys, and mouse reports
            // carry no editing meaning here.
            _ => {}
        }
        self.scroll_to_cursor();
        Action::Continue
    }

    /// Keys while the menu bar is open.
    fn handle_menu(&mut self, event: &Event, fs: &dyn Fs, menu: usize, item: usize) -> Action {
        let menus = MENUS.len();
        match event {
            Event::Left => {
                self.mode = Mode::Menu {
                    menu: (menu + menus - 1) % menus,
                    item: 0,
                };
            }
            Event::Right => {
                self.mode = Mode::Menu {
                    menu: (menu + 1) % menus,
                    item: 0,
                };
            }
            Event::Up | Event::Down => {
                let items = MENUS[menu].items.len();
                let item = match event {
                    Event::Up => (item + items - 1) % items,
                    _ => (item + 1) % items,
                };
                self.mode = Mode::Menu { menu, item };
            }
            Event::Enter => return self.execute_menu_item(fs, menu, item),
            Event::Function(10) => self.mode = Mode::Edit,
            _ => {}
        }
        Action::Continue
    }

    /// Carry out the selected menu item.
    fn execute_menu_item(&mut self, fs: &dyn Fs, menu: usize, item: usize) -> Action {
        self.mode = Mode::Edit;
        self.notice = None;
        match (menu, item) {
            // File
            (0, 0) => self.guarded(Pending::New),
            (0, 1) => self.guarded(Pending::Open),
            (0, 2) => self.save(fs),
            (0, 3) => {
                self.mode = Mode::Prompt {
                    intent: PromptIntent::SaveAs,
                    input: self.path.clone().unwrap_or_default(),
                };
                Action::Continue
            }
            (0, 4) => self.guarded(Pending::Exit),
            // Search
            (1, 0) => {
                self.mode = Mode::Prompt {
                    intent: PromptIntent::Find,
                    input: String::new(),
                };
                Action::Continue
            }
            (1, 1) => {
                self.find_next();
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    /// Start `pending`, asking about unsaved changes first when there are
    /// any.
    fn guarded(&mut self, pending: Pending) -> Action {
        if self.buffer.is_modified() {
            self.mode = Mode::Confirm(pending);
            return Action::Continue;
        }
        self.pending = Some(pending);
        self.resume_pending()
    }

    /// Keys while a "save changes first?" question is up.
    fn handle_confirm(&mut self, event: &Event, fs: &dyn Fs, pending: Pending) -> Action {
        match event {
            Event::Char('y' | 'Y') => {
                self.mode = Mode::Edit;
                self.pending = Some(pending);
                self.save(fs)
            }
            Event::Char('n' | 'N') => {
                self.mode = Mode::Edit;
                self.pending = Some(pending);
                self.resume_pending()
            }
            Event::Char('c' | 'C') | Event::Function(10) => {
                self.mode = Mode::Edit;
                self.pending = None;
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    /// Keys while a one-line prompt is up.
    fn handle_prompt(
        &mut self,
        event: &Event,
        fs: &dyn Fs,
        intent: PromptIntent,
        mut input: String,
    ) -> Action {
        match event {
            Event::Char(ch) if !ch.is_control() => {
                input.push(*ch);
                self.mode = Mode::Prompt { intent, input };
            }
            Event::Paste(text) => {
                input.extend(text.chars().filter(|ch| !ch.is_control()));
                self.mode = Mode::Prompt { intent, input };
            }
            Event::Backspace => {
                input.pop();
                self.mode = Mode::Prompt { intent, input };
            }
            Event::Enter => {
                self.mode = Mode::Edit;
                if input.is_empty() {
                    // Nothing entered: the prompt is abandoned, exactly as
                    // a cancel — an empty path or needle is never acted on.
                    self.pending = None;
                    return Action::Continue;
                }
                return self.commit_prompt(fs, intent, input);
            }
            Event::Function(10) => {
                self.mode = Mode::Edit;
                self.pending = None;
            }
            _ => self.mode = Mode::Prompt { intent, input },
        }
        Action::Continue
    }

    /// Act on a completed prompt.
    fn commit_prompt(&mut self, fs: &dyn Fs, intent: PromptIntent, input: String) -> Action {
        match intent {
            PromptIntent::Open => {
                self.load(fs, &input);
                Action::Continue
            }
            PromptIntent::SaveAs => {
                self.path = Some(input);
                self.save(fs)
            }
            PromptIntent::Find => {
                self.last_find = Some(input);
                self.find_next();
                Action::Continue
            }
        }
    }

    /// Replace the buffer with the decoded content of `path` (or a named
    /// empty buffer when the file does not exist yet). A refusal posts the
    /// reason and leaves the current buffer untouched.
    fn load(&mut self, fs: &dyn Fs, path: &str) {
        match fs.read(path) {
            Ok(bytes) => match TextBuffer::from_bytes(&bytes) {
                Ok((buffer, notices)) => {
                    self.buffer = buffer;
                    self.path = Some(String::from(path));
                    self.reset_cursor();
                    self.notice = load_notice(notices);
                }
                Err(err) => {
                    self.notice = Some(format!("{path}: {}", decode_message(err)));
                }
            },
            Err(Errno::NotFound) => {
                self.buffer = TextBuffer::new();
                self.path = Some(String::from(path));
                self.reset_cursor();
                self.notice = Some(String::from("New file"));
            }
            Err(errno) => {
                self.notice = Some(format!("{path}: {errno}"));
            }
        }
    }

    /// Save the buffer to its file, asking for a name first when it has
    /// none. On success any pending guarded action resumes; on refusal the
    /// reason is posted and the buffer stays modified.
    fn save(&mut self, fs: &dyn Fs) -> Action {
        let Some(path) = self.path.clone() else {
            self.mode = Mode::Prompt {
                intent: PromptIntent::SaveAs,
                input: String::new(),
            };
            return Action::Continue;
        };
        match fs.write(&path, &self.buffer.to_bytes()) {
            Ok(()) => {
                self.buffer.mark_saved();
                self.notice = Some(format!("Saved {path}"));
                self.resume_pending()
            }
            Err(errno) => {
                self.notice = Some(format!("{path}: {errno}"));
                self.pending = None;
                Action::Continue
            }
        }
    }

    /// Carry out the action a confirm question guarded, now that the
    /// buffer is settled.
    fn resume_pending(&mut self) -> Action {
        match self.pending.take() {
            Some(Pending::New) => {
                self.buffer = TextBuffer::new();
                self.path = None;
                self.reset_cursor();
                Action::Continue
            }
            Some(Pending::Open) => {
                self.mode = Mode::Prompt {
                    intent: PromptIntent::Open,
                    input: String::new(),
                };
                Action::Continue
            }
            Some(Pending::Exit) => Action::Quit,
            None => Action::Continue,
        }
    }

    /// Move the cursor to the top of a fresh buffer.
    fn reset_cursor(&mut self) {
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.sticky_col = 0;
        self.scroll_top = 0;
        self.scroll_left = 0;
    }

    /// Insert pasted `text` at the cursor: printable characters and line
    /// breaks only, exactly as if typed.
    fn paste(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.buffer.split_line(self.cursor_row, self.cursor_col);
                self.cursor_row += 1;
                self.cursor_col = 0;
            } else if !ch.is_control() {
                self.buffer
                    .insert_char(self.cursor_row, self.cursor_col, ch, false);
                self.cursor_col += 1;
            }
        }
        self.sticky_col = self.cursor_col;
    }

    /// Move the cursor `delta` lines, keeping the sticky column.
    fn move_vertical(&mut self, delta: isize) {
        let last = self.buffer.line_count() - 1;
        let row = self.cursor_row.saturating_add_signed(delta).min(last);
        self.cursor_row = row;
        self.cursor_col = self.sticky_col.min(self.buffer.line_chars(row));
    }

    /// Search forward for the last needle, wrapping at the end of the
    /// buffer, and move the cursor to the match.
    fn find_next(&mut self) {
        let Some(needle) = self.last_find.clone() else {
            self.notice = Some(String::from("No previous search"));
            return;
        };
        // Forward from just past the cursor, wrapping around to it: every
        // line is visited once, the cursor's own line twice (its tail
        // first, its head on the wrap).
        let count = self.buffer.line_count();
        let start_byte =
            crate::buffer::byte_of_char(self.buffer.line(self.cursor_row), self.cursor_col + 1);
        for step in 0..=count {
            let row = (self.cursor_row + step) % count;
            let line = self.buffer.line(row);
            let from = if step == 0 {
                start_byte.min(line.len())
            } else {
                0
            };
            let Some(slice) = line.get(from..) else {
                continue;
            };
            if let Some(offset) = slice.find(needle.as_str()) {
                // On the wrap revisit (`step == count`) everything at or
                // after the start position was already covered by step 0,
                // so any match found now genuinely precedes the cursor and
                // is the wrapped hit.
                let byte = from + offset;
                self.cursor_row = row;
                self.cursor_col = line[..byte].chars().count();
                self.sticky_col = self.cursor_col;
                self.scroll_to_cursor();
                return;
            }
        }
        self.notice = Some(String::from("Match not found"));
    }

    /// Pull the scroll window so the cursor is visible.
    fn scroll_to_cursor(&mut self) {
        if self.cursor_row < self.scroll_top {
            self.scroll_top = self.cursor_row;
        } else if self.cursor_row >= self.scroll_top + self.view_rows {
            self.scroll_top = self.cursor_row + 1 - self.view_rows;
        }
        let col = width_of_prefix(self.buffer.line(self.cursor_row), self.cursor_col);
        if col < self.scroll_left {
            self.scroll_left = col;
        } else if col >= self.scroll_left + self.view_cols {
            self.scroll_left = col + 1 - self.view_cols;
        }
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

/// The status notice for the conversions a load applied, or `None` when
/// the bytes were taken as they were.
fn load_notice(notices: crate::buffer::LoadNotices) -> Option<String> {
    match (notices.tabs_expanded, notices.crlf_converted) {
        (true, true) => Some(String::from(
            "Note: tabs expanded to spaces; CRLF line endings converted to LF",
        )),
        (true, false) => Some(String::from("Note: tabs expanded to spaces")),
        (false, true) => Some(String::from("Note: CRLF line endings converted to LF")),
        (false, false) => None,
    }
}

/// The human message for a refused decode.
fn decode_message(err: DecodeError) -> &'static str {
    match err {
        DecodeError::TooLarge => "file is too large for this editor (16 MiB limit)",
        DecodeError::NotText => "not a text file",
    }
}
