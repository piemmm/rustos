//! The editor: modes, registers, the view, and every state transition.
//!
//! [`Editor`] is the I/O-free heart of the tool. It owns the [`Buffer`],
//! the cursor, the mode, the registers, the search state, and the pending
//! normal-mode command; [`Editor::handle_event`] consumes one decoded input
//! event and advances all of it. The render layer draws this state; the
//! [`crate::fileio::FileIo`] seam is the only way it reaches a file.
//!
//! The normal/visual key grammar itself lives in [`crate::normal`]; the ex
//! (`:`) command grammar in [`crate::excmd`]. This module owns the state
//! they act on and the insert/replace and command-line editing transitions.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use rustos_curses::Event;

use crate::buffer::{Buffer, Position};
use crate::fileio::FileIo;
use crate::motion::{self, MotionKind, MotionTarget};
use crate::normal::{self, NormalState};
use crate::pattern::Pattern;

/// The editor's mode.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Normal (command) mode.
    Normal,
    /// Insert mode (`i`, `a`, `o`, …).
    Insert,
    /// Replace mode (`R`): typing overwrites.
    Replace,
    /// Visual mode; `linewise` distinguishes `V` from `v`.
    Visual {
        /// Whole-line selection (`V`).
        linewise: bool,
    },
    /// Command-line mode: an ex command (`:`) or a search (`/`, `?`).
    CmdLine,
}

/// One register's content.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Yank {
    /// The yanked lines (one entry for a within-line span).
    pub lines: Vec<String>,
    /// Whether the yank was linewise (put as whole lines).
    pub linewise: bool,
}

/// The register file: the unnamed register plus `"a` … `"z` (capitals
/// append).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Registers {
    unnamed: Yank,
    named: BTreeMap<char, Yank>,
}

impl Registers {
    /// Store a yank: always into the unnamed register, and into `name`
    /// when one was given (`A`–`Z` append to the lowercase register).
    pub fn store(&mut self, name: Option<char>, yank: Yank) {
        if let Some(name) = name {
            if name.is_ascii_uppercase() {
                let key = name.to_ascii_lowercase();
                let entry = self.named.entry(key).or_default();
                entry.linewise = entry.linewise || yank.linewise;
                if entry.linewise {
                    entry.lines.extend(yank.lines.iter().cloned());
                } else if let (Some(last), Some(first)) =
                    (entry.lines.last_mut(), yank.lines.first())
                {
                    last.push_str(first);
                    entry.lines.extend(yank.lines.iter().skip(1).cloned());
                } else {
                    entry.lines.clone_from(&yank.lines);
                }
            } else {
                self.named.insert(name, yank.clone());
            }
        }
        self.unnamed = yank;
    }

    /// Read a register: the named one, or the unnamed register.
    #[must_use]
    pub fn read(&self, name: Option<char>) -> Option<&Yank> {
        match name {
            Some(name) => self.named.get(&name.to_ascii_lowercase()),
            None => Some(&self.unnamed),
        }
    }
}

/// The visible window geometry the renderer last used; scrolling commands
/// and `H`/`M`/`L` are computed against it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct View {
    /// First visible buffer line.
    pub top: usize,
    /// Leftmost visible screen column (horizontal scroll).
    pub left: usize,
    /// Text rows the window shows.
    pub rows: usize,
    /// Text columns the window shows.
    pub cols: usize,
}

/// The command line being edited (after `:`, `/`, or `?`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CmdLine {
    /// The prompt character: `:`, `/`, or `?`.
    pub prefix: char,
    /// The text typed so far.
    pub text: String,
    /// The cursor's character index within `text`.
    pub cursor: usize,
}

/// A status-line message and whether it is an error (drawn highlighted).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    /// The text shown on the message line.
    pub text: String,
    /// Errors render emphasised, like vim's `ErrorMsg`.
    pub error: bool,
}

/// The remembered `/` / `?` search: the compiled pattern and direction.
#[derive(Clone, Debug)]
pub struct Search {
    /// The compiled pattern `n`/`N` reuse.
    pub pattern: Pattern,
    /// `true` for `/` (forward).
    pub forward: bool,
}

/// The `.`-repeat recorder: the last change's events, the event log of
/// the command in progress, and the replay/insert-session flags that
/// govern what gets recorded.
#[derive(Clone, Debug, Default)]
struct Recorder {
    /// The recorded events of the last change, replayed by `.`.
    dot: Vec<Event>,
    /// The event log of the command in progress.
    log: Vec<Event>,
    /// Whether a `.` replay is feeding events (suppresses recording).
    replaying: bool,
    /// Whether the current insert session is part of a recorded change.
    insert: bool,
}

/// The editor state machine.
pub struct Editor {
    /// The buffer under edit.
    pub buffer: Buffer,
    /// The cursor.
    pub cursor: Position,
    /// The mode.
    pub mode: Mode,
    /// The anchor end of a visual selection.
    pub visual_anchor: Position,
    /// The remembered column for `j`/`k` over short lines.
    pub sticky_col: Option<usize>,
    /// The pending normal-mode command state.
    pub pending: NormalState,
    /// The registers.
    pub registers: Registers,
    /// The last search, reused by `n`/`N` and empty `:s//`.
    pub search: Option<Search>,
    /// Whether search highlighting is on (cleared by `:noh`).
    pub hlsearch: bool,
    /// The last `f`/`F`/`t`/`T`, reused by `;` and `,`: (command, target).
    pub last_find: Option<(char, char)>,
    /// The `'number'` option.
    pub number: bool,
    /// The status message, if any.
    pub message: Option<Message>,
    /// The command line under edit, if any.
    pub cmdline: Option<CmdLine>,
    /// The visible window geometry (updated by the renderer).
    pub view: View,
    /// The argument-list files and the current index (`:n` / `:prev`).
    pub files: Vec<String>,
    /// Index into [`Editor::files`].
    pub file_index: usize,
    /// Set when the session should end, with the exit code.
    pub quit: Option<i32>,
    /// The `.`-repeat recorder.
    recorder: Recorder,
}

impl Editor {
    /// A fresh editor over an empty buffer, with the startup file list.
    #[must_use]
    pub fn new(files: Vec<String>, readonly: bool) -> Editor {
        let mut buffer = Buffer::empty();
        buffer.set_readonly(readonly);
        Editor {
            buffer,
            cursor: Position::default(),
            mode: Mode::Normal,
            visual_anchor: Position::default(),
            sticky_col: None,
            pending: NormalState::default(),
            registers: Registers::default(),
            search: None,
            hlsearch: false,
            last_find: None,
            number: false,
            message: None,
            cmdline: None,
            view: View {
                top: 0,
                left: 0,
                rows: 23,
                cols: 80,
            },
            files,
            file_index: 0,
            quit: None,
            recorder: Recorder::default(),
        }
    }

    /// Show a plain status message.
    pub fn info(&mut self, text: String) {
        self.message = Some(Message { text, error: false });
    }

    /// Show an error message (vim's highlighted `E…` line).
    pub fn error(&mut self, text: String) {
        self.message = Some(Message { text, error: true });
    }

    /// Consume one decoded input event.
    pub fn handle_event(&mut self, event: &Event, io: &dyn FileIo) {
        if !self.recorder.replaying {
            match self.mode {
                Mode::Normal | Mode::Visual { .. } => self.recorder.log.push(event.clone()),
                Mode::Insert | Mode::Replace if self.recorder.insert => {
                    self.recorder.log.push(event.clone());
                }
                _ => {}
            }
        }
        self.message = None;
        match self.mode {
            Mode::Normal | Mode::Visual { .. } => normal::handle(self, event, io),
            Mode::Insert | Mode::Replace => self.insert_event(event),
            Mode::CmdLine => self.cmdline_event(event, io),
        }
    }

    /// Mark the just-finished normal command a *change*: the event log
    /// becomes the `.` repeat. No-op during a replay.
    pub fn finish_change(&mut self) {
        if self.recorder.replaying {
            return;
        }
        if !self.recorder.log.is_empty() {
            self.recorder.dot = core::mem::take(&mut self.recorder.log);
        }
        self.recorder.insert = false;
    }

    /// Mark the just-finished normal command a non-change (a motion, a
    /// yank): the log is discarded.
    pub fn finish_command(&mut self) {
        if !self.recorder.replaying {
            self.recorder.log.clear();
        }
    }

    /// Keep recording into the change through the insert session that a
    /// change command (`i`, `cw`, `o`, …) just opened.
    pub fn record_insert(&mut self) {
        if !self.recorder.replaying {
            self.recorder.insert = true;
        }
    }

    /// Clamp the cursor onto a real character for normal mode (column at
    /// most `len - 1`, line within the buffer).
    pub fn clamp_cursor(&mut self) {
        let line = self.cursor.line.min(self.buffer.len_lines() - 1);
        let len = self.buffer.line_len(line);
        let max_col = if matches!(self.mode, Mode::Insert | Mode::Replace) {
            len
        } else {
            len.saturating_sub(1)
        };
        self.cursor = Position::new(line, self.cursor.col.min(max_col));
    }

    /// Move the cursor along a motion target, maintaining the sticky
    /// column across linewise vertical moves.
    pub fn move_to(&mut self, target: MotionTarget, vertical: bool) {
        if vertical {
            let sticky = *self.sticky_col.get_or_insert(self.cursor.col);
            self.cursor = Position::new(target.pos.line, sticky);
        } else {
            self.cursor = target.pos;
            self.sticky_col = None;
        }
        self.clamp_cursor();
    }

    /// Canonicalise a motion target into the inclusive span an operator
    /// covers: `(start, end)` both inclusive, or [`None`] for an empty
    /// exclusive span. Linewise spans cover whole lines.
    #[must_use]
    pub fn operator_span(&self, target: MotionTarget) -> Option<(Position, Position, bool)> {
        let (lo, hi) = if self.cursor <= target.pos {
            (self.cursor, target.pos)
        } else {
            (target.pos, self.cursor)
        };
        match target.kind {
            MotionKind::Linewise => {
                let start = Position::new(lo.line, 0);
                let end = Position::new(hi.line, self.buffer.line_len(hi.line).saturating_sub(1));
                Some((start, end, true))
            }
            MotionKind::Inclusive => Some((lo, hi, false)),
            MotionKind::Exclusive => {
                let end = motion::step_back(&self.buffer, hi)?;
                if lo > end {
                    return None;
                }
                Some((lo, end, false))
            }
        }
    }

    /// Yank the inclusive span into the registers (no buffer change).
    pub fn yank_span(&mut self, start: Position, end: Position, linewise: bool) {
        let lines = if linewise {
            self.buffer.lines_in(start.line, end.line + 1).to_vec()
        } else {
            motion::span_text(&self.buffer, start, end)
        };
        let register = self.pending.register;
        self.registers.store(register, Yank { lines, linewise });
    }

    /// Delete the inclusive span, yanking it first. The cursor lands at
    /// the span start (first non-blank for a linewise delete). With
    /// `commit` false the undo group stays open — a change (`c`) continues
    /// into insert mode inside the same group. Editing a `-R` buffer in
    /// memory is allowed, exactly as in vim; only writing is refused.
    pub fn delete_span(&mut self, start: Position, end: Position, linewise: bool, commit: bool) {
        self.yank_span(start, end, linewise);
        self.buffer.begin_edit(self.cursor);
        if linewise {
            self.buffer.replace_lines(start.line, end.line + 1, vec![]);
            let line = start.line.min(self.buffer.len_lines() - 1);
            self.cursor = Position::new(line, motion::first_non_blank(&self.buffer, line));
        } else {
            let prefix: String = self
                .buffer
                .line(start.line)
                .chars()
                .take(start.col)
                .collect();
            let suffix: String = self
                .buffer
                .line(end.line)
                .chars()
                .skip(end.col + 1)
                .collect();
            let merged = format!("{prefix}{suffix}");
            self.buffer
                .replace_lines(start.line, end.line + 1, vec![merged]);
            self.cursor = start;
        }
        if commit {
            self.buffer.commit_edit(self.cursor);
            self.clamp_cursor();
        }
    }

    /// Put a register (`p` after / `P` before the cursor), `count` times.
    pub fn put(&mut self, after: bool, count: usize) {
        let Some(yank) = self.registers.read(self.pending.register).cloned() else {
            self.error(String::from("E353: Nothing in register"));
            return;
        };
        if yank.lines.is_empty() {
            return;
        }
        let count = count.max(1);
        self.buffer.begin_edit(self.cursor);
        if yank.linewise {
            let mut block: Vec<String> = Vec::new();
            for _ in 0..count {
                block.extend(yank.lines.iter().cloned());
            }
            let at = if after {
                self.cursor.line + 1
            } else {
                self.cursor.line
            };
            let at = at.min(self.buffer.len_lines());
            // A pure insertion: the undo record carries only the pasted
            // span, never the rest of the file.
            self.buffer.replace_lines(at, at, block);
            self.cursor = Position::new(at, motion::first_non_blank(&self.buffer, at));
        } else {
            let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
            let split = if after && !chars.is_empty() {
                (self.cursor.col + 1).min(chars.len())
            } else {
                self.cursor.col.min(chars.len())
            };
            let prefix: String = chars[..split].iter().collect();
            let suffix: String = chars[split..].iter().collect();
            let mut body: Vec<String> = Vec::new();
            for _ in 0..count {
                for (i, part) in yank.lines.iter().enumerate() {
                    if i == 0 && !body.is_empty() {
                        if let Some(last) = body.last_mut() {
                            last.push_str(part);
                        }
                    } else {
                        body.push(part.clone());
                    }
                }
            }
            let mut new_lines: Vec<String> = Vec::new();
            if body.len() == 1 {
                new_lines.push(format!("{prefix}{}{suffix}", body[0]));
            } else {
                new_lines.push(format!("{prefix}{}", body[0]));
                new_lines.extend(body[1..body.len() - 1].iter().cloned());
                new_lines.push(format!("{}{suffix}", body[body.len() - 1]));
            }
            self.buffer
                .replace_lines(self.cursor.line, self.cursor.line + 1, new_lines);
            self.cursor = Position::new(self.cursor.line, split);
        }
        self.buffer.commit_edit(self.cursor);
        self.clamp_cursor();
    }

    /// Join `count.max(2)` lines (`J`): the next line's leading blanks are
    /// dropped and a single space separates the joined parts.
    pub fn join_lines(&mut self, count: usize) {
        let joins = count.max(2) - 1;
        self.buffer.begin_edit(self.cursor);
        let mut join_col = self.cursor.col;
        for _ in 0..joins {
            let line = self.cursor.line;
            if line + 1 >= self.buffer.len_lines() {
                break;
            }
            let current = String::from(self.buffer.line(line));
            let next = String::from(self.buffer.line(line + 1));
            let trimmed = next.trim_start_matches([' ', '\t']);
            join_col = current.chars().count();
            let joined = if current.is_empty() {
                String::from(trimmed)
            } else if trimmed.is_empty() {
                current.clone()
            } else {
                format!("{current} {trimmed}")
            };
            self.buffer.replace_lines(line, line + 2, vec![joined]);
        }
        self.cursor = Position::new(self.cursor.line, join_col);
        self.buffer.commit_edit(self.cursor);
        self.clamp_cursor();
    }

    /// Replace `count` characters under the cursor with `ch` (`r`). Fails
    /// quietly (no change) when the line is too short, as in vim.
    pub fn replace_chars(&mut self, ch: char, count: usize) {
        let count = count.max(1);
        let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
        if self.cursor.col + count > chars.len() {
            return;
        }
        let mut new: Vec<char> = chars;
        for slot in new.iter_mut().skip(self.cursor.col).take(count) {
            *slot = ch;
        }
        let line: String = new.into_iter().collect();
        self.buffer
            .replace_lines(self.cursor.line, self.cursor.line + 1, vec![line]);
        self.cursor = Position::new(self.cursor.line, self.cursor.col + count - 1);
    }

    /// Toggle the case of `count` characters (`~`), advancing the cursor.
    pub fn toggle_case(&mut self, count: usize) {
        let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
        if chars.is_empty() {
            return;
        }
        let count = count
            .max(1)
            .min(chars.len() - self.cursor.col.min(chars.len() - 1));
        let mut new = chars;
        let start = self.cursor.col.min(new.len() - 1);
        for slot in new.iter_mut().skip(start).take(count) {
            *slot = if slot.is_uppercase() {
                slot.to_lowercase().next().unwrap_or(*slot)
            } else {
                slot.to_uppercase().next().unwrap_or(*slot)
            };
        }
        let line: String = new.into_iter().collect();
        self.buffer
            .replace_lines(self.cursor.line, self.cursor.line + 1, vec![line]);
        self.cursor = Position::new(self.cursor.line, start + count);
        self.clamp_cursor();
    }

    /// Enter insert (or replace) mode. The undo group opened here (or
    /// earlier, by the change command that led here) stays open until the
    /// session ends, so the whole insert undoes as one step.
    pub fn enter_insert(&mut self, replace: bool) {
        self.buffer.begin_edit(self.cursor);
        self.mode = if replace { Mode::Replace } else { Mode::Insert };
        self.record_insert();
        self.sticky_col = None;
    }

    /// Leave insert/replace mode: the cursor steps left (vim's rule),
    /// the undo group closes, and the recorded change becomes `.`.
    pub fn leave_insert(&mut self) {
        self.mode = Mode::Normal;
        self.cursor = Position::new(self.cursor.line, self.cursor.col.saturating_sub(1));
        self.buffer.commit_edit(self.cursor);
        self.finish_change();
        self.clamp_cursor();
    }

    /// Insert `text` at the cursor (insert-mode typing and pastes); `\n`
    /// splits the line.
    pub fn insert_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                self.split_line();
            } else {
                self.insert_char(ch);
            }
        }
    }

    /// Insert one character at the cursor and advance.
    fn insert_char(&mut self, ch: char) {
        let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
        let at = self.cursor.col.min(chars.len());
        let mut new: Vec<char> = chars;
        if matches!(self.mode, Mode::Replace) && at < new.len() {
            new[at] = ch;
        } else {
            new.insert(at, ch);
        }
        let line: String = new.into_iter().collect();
        self.buffer
            .replace_lines(self.cursor.line, self.cursor.line + 1, vec![line]);
        self.cursor = Position::new(self.cursor.line, at + 1);
    }

    /// Split the current line at the cursor (insert-mode Enter).
    fn split_line(&mut self) {
        let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
        let at = self.cursor.col.min(chars.len());
        let head: String = chars[..at].iter().collect();
        let tail: String = chars[at..].iter().collect();
        self.buffer
            .replace_lines(self.cursor.line, self.cursor.line + 1, vec![head, tail]);
        self.cursor = Position::new(self.cursor.line + 1, 0);
    }

    /// Insert-mode rub-out: delete the character before the cursor,
    /// joining onto the previous line at column 0.
    fn insert_backspace(&mut self) {
        if self.cursor.col > 0 {
            let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
            let at = (self.cursor.col - 1).min(chars.len().saturating_sub(1));
            let mut new: Vec<char> = chars;
            if at < new.len() {
                new.remove(at);
            }
            let line: String = new.into_iter().collect();
            self.buffer
                .replace_lines(self.cursor.line, self.cursor.line + 1, vec![line]);
            self.cursor = Position::new(self.cursor.line, at);
        } else if self.cursor.line > 0 {
            let prev = String::from(self.buffer.line(self.cursor.line - 1));
            let here = String::from(self.buffer.line(self.cursor.line));
            let col = prev.chars().count();
            let joined = format!("{prev}{here}");
            self.buffer
                .replace_lines(self.cursor.line - 1, self.cursor.line + 1, vec![joined]);
            self.cursor = Position::new(self.cursor.line - 1, col);
        }
    }

    /// Insert-mode Delete: remove the character under the cursor, joining
    /// the next line at the line end.
    fn insert_delete(&mut self) {
        let chars: Vec<char> = self.buffer.line(self.cursor.line).chars().collect();
        if self.cursor.col < chars.len() {
            let mut new: Vec<char> = chars;
            new.remove(self.cursor.col);
            let line: String = new.into_iter().collect();
            self.buffer
                .replace_lines(self.cursor.line, self.cursor.line + 1, vec![line]);
        } else if self.cursor.line + 1 < self.buffer.len_lines() {
            let here = String::from(self.buffer.line(self.cursor.line));
            let next = String::from(self.buffer.line(self.cursor.line + 1));
            let joined = format!("{here}{next}");
            self.buffer
                .replace_lines(self.cursor.line, self.cursor.line + 2, vec![joined]);
        }
    }

    /// One insert/replace-mode event.
    fn insert_event(&mut self, event: &Event) {
        match event {
            Event::Esc | Event::Ctrl('c') => self.leave_insert(),
            Event::Char(ch) => self.insert_char(*ch),
            Event::Enter => self.split_line(),
            Event::Tab => self.insert_char('\t'),
            Event::Backspace => self.insert_backspace(),
            Event::Delete => self.insert_delete(),
            Event::Paste(text) => self.insert_text(text),
            Event::Up => {
                self.cursor = Position::new(self.cursor.line.saturating_sub(1), self.cursor.col);
                self.clamp_cursor();
            }
            Event::Down => {
                let last = self.buffer.len_lines() - 1;
                self.cursor = Position::new((self.cursor.line + 1).min(last), self.cursor.col);
                self.clamp_cursor();
            }
            Event::Left => {
                self.cursor = Position::new(self.cursor.line, self.cursor.col.saturating_sub(1));
            }
            Event::Right => {
                let len = self.buffer.line_len(self.cursor.line);
                self.cursor = Position::new(self.cursor.line, (self.cursor.col + 1).min(len));
            }
            Event::Home => self.cursor = Position::new(self.cursor.line, 0),
            Event::End => {
                self.cursor =
                    Position::new(self.cursor.line, self.buffer.line_len(self.cursor.line));
            }
            // The remaining keys carry no insert-mode action.
            _ => {}
        }
    }

    /// Open the command line (`:`, `/`, `?`).
    pub fn open_cmdline(&mut self, prefix: char) {
        self.cmdline = Some(CmdLine {
            prefix,
            text: String::new(),
            cursor: 0,
        });
        self.mode = Mode::CmdLine;
    }

    /// One command-line editing event.
    fn cmdline_event(&mut self, event: &Event, io: &dyn FileIo) {
        let Some(mut cmdline) = self.cmdline.take() else {
            self.mode = Mode::Normal;
            return;
        };
        match event {
            Event::Esc | Event::Ctrl('c') => {
                self.mode = Mode::Normal;
                return;
            }
            Event::Enter => {
                self.mode = Mode::Normal;
                match cmdline.prefix {
                    ':' => crate::excmd::execute(self, &cmdline.text, io),
                    prefix => self.run_search_command(&cmdline.text, prefix == '/'),
                }
                return;
            }
            Event::Char(ch) => {
                let at = byte_index(&cmdline.text, cmdline.cursor);
                cmdline.text.insert(at, *ch);
                cmdline.cursor += 1;
            }
            Event::Paste(text) => {
                // A pasted command is content for the line being edited;
                // its line breaks are not "press Enter" (never auto-run).
                for ch in text.chars().filter(|ch| *ch != '\n' && *ch != '\r') {
                    let at = byte_index(&cmdline.text, cmdline.cursor);
                    cmdline.text.insert(at, ch);
                    cmdline.cursor += 1;
                }
            }
            Event::Backspace => {
                if cmdline.cursor == 0 {
                    // Rubbing out the prompt itself cancels, as in vim.
                    self.mode = Mode::Normal;
                    return;
                }
                let at = byte_index(&cmdline.text, cmdline.cursor - 1);
                cmdline.text.remove(at);
                cmdline.cursor -= 1;
            }
            Event::Delete => {
                if cmdline.cursor < cmdline.text.chars().count() {
                    let at = byte_index(&cmdline.text, cmdline.cursor);
                    cmdline.text.remove(at);
                }
            }
            Event::Left => cmdline.cursor = cmdline.cursor.saturating_sub(1),
            Event::Right => {
                cmdline.cursor = (cmdline.cursor + 1).min(cmdline.text.chars().count());
            }
            Event::Home => cmdline.cursor = 0,
            Event::End => cmdline.cursor = cmdline.text.chars().count(),
            // The remaining keys carry no command-line action.
            _ => {}
        }
        self.cmdline = Some(cmdline);
    }

    /// The visual selection's inclusive span and whether it is linewise,
    /// or [`None`] outside visual mode. Shared by the operators and the
    /// renderer's highlight so they can never disagree.
    #[must_use]
    pub fn selection(&self) -> Option<(Position, Position, bool)> {
        let Mode::Visual { linewise } = self.mode else {
            return None;
        };
        let (lo, hi) = if self.visual_anchor <= self.cursor {
            (self.visual_anchor, self.cursor)
        } else {
            (self.cursor, self.visual_anchor)
        };
        if linewise {
            let start = Position::new(lo.line, 0);
            let end = Position::new(hi.line, self.buffer.line_len(hi.line).saturating_sub(1));
            Some((start, end, true))
        } else {
            Some((lo, hi, false))
        }
    }

    /// Run a `/` / `?` search command: compile `text` (an empty line
    /// reuses the previous pattern) and jump to the first match.
    pub fn run_search_command(&mut self, text: &str, forward: bool) {
        let source = if text.is_empty() {
            let Some(search) = &self.search else {
                self.error(String::from("E35: No previous regular expression"));
                return;
            };
            String::from(search.pattern.source())
        } else {
            String::from(text)
        };
        match Pattern::compile(&source) {
            Ok(pattern) => {
                self.search = Some(Search { pattern, forward });
                self.hlsearch = true;
                self.search_next(false);
            }
            Err(_) => self.error(format!("E383: Invalid pattern: {source}")),
        }
    }

    /// Jump to the next match of the remembered search (`n`; `N` with
    /// `reverse`). Wraps around the buffer with vim's wrap notice.
    pub fn search_next(&mut self, reverse: bool) {
        let Some(search) = self.search.clone() else {
            self.error(String::from("E35: No previous regular expression"));
            return;
        };
        let forward = search.forward != reverse;
        let hit = if forward {
            self.find_forward(&search.pattern)
        } else {
            self.find_backward(&search.pattern)
        };
        match hit {
            Some((pos, wrapped)) => {
                self.cursor = pos;
                self.sticky_col = None;
                self.clamp_cursor();
                if wrapped {
                    self.info(String::from(if forward {
                        "search hit BOTTOM, continuing at TOP"
                    } else {
                        "search hit TOP, continuing at BOTTOM"
                    }));
                }
            }
            None => {
                self.error(format!(
                    "E486: Pattern not found: {}",
                    search.pattern.source()
                ));
            }
        }
    }

    /// The first match after the cursor, wrapping to the top. Returns the
    /// position and whether the scan wrapped.
    fn find_forward(&self, pattern: &Pattern) -> Option<(Position, bool)> {
        let lines = self.buffer.len_lines();
        if let Some((start, _)) =
            pattern.find_at(self.buffer.line(self.cursor.line), self.cursor.col + 1)
        {
            return Some((Position::new(self.cursor.line, start), false));
        }
        for offset in 1..=lines {
            let line = (self.cursor.line + offset) % lines;
            let wrapped = self.cursor.line + offset >= lines;
            if let Some((start, _)) = pattern.find_at(self.buffer.line(line), 0) {
                return Some((Position::new(line, start), wrapped));
            }
        }
        None
    }

    /// The first match before the cursor, wrapping to the bottom.
    fn find_backward(&self, pattern: &Pattern) -> Option<(Position, bool)> {
        let lines = self.buffer.len_lines();
        if let Some((start, _)) =
            pattern.rfind_before(self.buffer.line(self.cursor.line), self.cursor.col)
        {
            return Some((Position::new(self.cursor.line, start), false));
        }
        for offset in 1..=lines {
            let line = (self.cursor.line + lines - (offset % lines)) % lines;
            let wrapped = offset > self.cursor.line;
            let text = self.buffer.line(line);
            let len = text.chars().count();
            if let Some((start, _)) = pattern.rfind_before(text, len + 1) {
                return Some((Position::new(line, start), wrapped));
            }
        }
        None
    }

    /// `*`: search forward for the whole word under the cursor.
    pub fn search_word_under_cursor(&mut self) {
        let Some(span) = motion::word_object(&self.buffer, self.cursor, false) else {
            self.error(String::from("E348: No string under cursor"));
            return;
        };
        let word: String = self
            .buffer
            .line(self.cursor.line)
            .chars()
            .skip(span.start.col)
            .take(span.end.col + 1 - span.start.col)
            .collect();
        if word.trim().is_empty() {
            self.error(String::from("E348: No string under cursor"));
            return;
        }
        let source = crate::pattern::whole_word_pattern(&word);
        self.run_search_command(&source, true);
    }

    /// Scroll half a window (`Ctrl-D` / `Ctrl-U`): view and cursor move
    /// together.
    pub fn scroll_half(&mut self, down: bool) {
        let step = (self.view.rows / 2).max(1);
        self.scroll_by(step, down);
    }

    /// Scroll a full window (`Ctrl-F` / `Ctrl-B`).
    pub fn scroll_page(&mut self, down: bool) {
        let step = self.view.rows.saturating_sub(2).max(1);
        self.scroll_by(step, down);
    }

    /// Move view top and cursor `step` lines.
    fn scroll_by(&mut self, step: usize, down: bool) {
        let last = self.buffer.len_lines() - 1;
        if down {
            self.view.top = (self.view.top + step).min(last);
            self.cursor = Position::new((self.cursor.line + step).min(last), self.cursor.col);
        } else {
            self.view.top = self.view.top.saturating_sub(step);
            self.cursor = Position::new(self.cursor.line.saturating_sub(step), self.cursor.col);
        }
        self.clamp_cursor();
    }

    /// Load `path` into the buffer (startup, `:e`, `:n`). Preserves the
    /// session's readonly posture; resets cursor, view, and history.
    pub fn load_file(&mut self, path: &str, io: &dyn FileIo) {
        let readonly = self.buffer.is_readonly();
        match io.read(path) {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let lines = text.split('\n').count().saturating_sub(1).max(1);
                self.buffer = Buffer::from_text(Some(String::from(path)), &text);
                self.info(format!("\"{path}\" {lines}L, {}B", bytes.len()));
            }
            Ok(None) => {
                self.buffer = Buffer::empty();
                self.buffer.set_name(Some(String::from(path)));
                self.info(format!("\"{path}\" [New File]"));
            }
            Err(errno) => {
                self.buffer = Buffer::empty();
                self.buffer.set_name(Some(String::from(path)));
                self.error(format!("E484: Can't open file {path}: {errno}"));
            }
        }
        self.buffer.set_readonly(readonly);
        self.cursor = Position::default();
        self.view.top = 0;
        self.view.left = 0;
        self.sticky_col = None;
    }

    /// Write the buffer (`:w` and friends). Returns whether it was
    /// written. `path` overrides (and re-binds, for an unnamed buffer)
    /// the file name; `force` overrides the readonly posture.
    pub fn write_buffer(&mut self, path: Option<&str>, force: bool, io: &dyn FileIo) -> bool {
        if self.buffer.is_readonly() && !force {
            self.error(String::from(
                "E45: 'readonly' option is set (add ! to override)",
            ));
            return false;
        }
        let target = if let Some(path) = path {
            String::from(path)
        } else {
            let Some(name) = self.buffer.name() else {
                self.error(String::from("E32: No file name"));
                return false;
            };
            String::from(name)
        };
        let text = self.buffer.to_text();
        match io.write(&target, text.as_bytes()) {
            Ok(()) => {
                if self.buffer.name().is_none() {
                    self.buffer.set_name(Some(target.clone()));
                }
                self.buffer.set_modified(false);
                let lines = self.buffer.len_lines();
                self.info(format!("\"{target}\" {lines}L, {}B written", text.len()));
                true
            }
            Err(errno) => {
                self.error(format!(
                    "E212: Can't open file for writing: {target}: {errno}"
                ));
                false
            }
        }
    }

    /// Switch to another file (`:e`, `:n`, `:prev`), guarding unwritten
    /// changes unless forced.
    pub fn edit_file(&mut self, path: &str, force: bool, io: &dyn FileIo) {
        if self.buffer.is_modified() && !force {
            self.error(String::from(
                "E37: No write since last change (add ! to override)",
            ));
            return;
        }
        self.load_file(path, io);
    }

    /// Step through the argument list (`:n` forward, `:prev` back).
    pub fn goto_arg(&mut self, forward: bool, force: bool, io: &dyn FileIo) {
        if self.files.is_empty() {
            self.error(String::from("E163: There is only one file to edit"));
            return;
        }
        let next = if forward {
            self.file_index + 1
        } else {
            self.file_index.wrapping_sub(1)
        };
        if next >= self.files.len() {
            self.error(String::from(if forward {
                "E165: Cannot go beyond last file"
            } else {
                "E164: Cannot go before first file"
            }));
            return;
        }
        if self.buffer.is_modified() && !force {
            self.error(String::from(
                "E37: No write since last change (add ! to override)",
            ));
            return;
        }
        self.file_index = next;
        let path = self.files[next].clone();
        self.load_file(&path, io);
    }

    /// Insert the contents of `path` below the cursor line (`:r`).
    pub fn read_file_into(&mut self, path: &str, io: &dyn FileIo) {
        match io.read(path) {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let incoming = Buffer::from_text(None, &text);
                let block = incoming.lines_in(0, incoming.len_lines()).to_vec();
                let at = self.cursor.line + 1;
                self.buffer.begin_edit(self.cursor);
                self.buffer.replace_lines(at, at, block);
                self.cursor = Position::new(at, motion::first_non_blank(&self.buffer, at));
                self.buffer.commit_edit(self.cursor);
                self.clamp_cursor();
            }
            Ok(None) => self.error(format!("E484: Can't open file {path}")),
            Err(errno) => self.error(format!("E484: Can't open file {path}: {errno}")),
        }
    }

    /// Replay the last change (`.`).
    pub fn repeat_dot(&mut self, io: &dyn FileIo) {
        if self.recorder.dot.is_empty() {
            return;
        }
        self.recorder.replaying = true;
        let events = self.recorder.dot.clone();
        for event in &events {
            self.handle_event(event, io);
        }
        // A replayed insert without its closing Escape still ends.
        if matches!(self.mode, Mode::Insert | Mode::Replace) {
            self.leave_insert();
        }
        self.recorder.replaying = false;
    }
}

/// The byte index of the `char_index`-th character of `text` (its length
/// for an index at or past the end).
fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(at, _)| at)
}
