//! The interactive line editor: typed key events in, an edited command line
//! out (`plans/SHELL.md`, "Interactive terminal").
//!
//! One [`Session`] edits one line. The REPL decodes raw input bytes through
//! the shared terminal stack (`tairix_curses::Input` over the one `lib/vt`
//! parser — never a shell-private key table) and feeds each
//! [`tairix_curses::Event`] to [`Session::handle`]; the session repaints
//! itself through the shell's [`Console`] after every event using the same
//! `lib/vt` escape vocabulary. The editor is pure state over injected seams,
//! so every keystroke behaviour is host-testable.
//!
//! The editing set matches what a bash/zsh user's fingers expect:
//!
//! * **History**: Up/Down (and `Ctrl-P`/`Ctrl-N`) walk the session history,
//!   preserving the line under edit as a draft; `Ctrl-R` opens incremental
//!   reverse search (`Ctrl-R` again steps to an older match, `Ctrl-G`
//!   aborts, Escape accepts, Enter accepts and submits).
//! * **Movement**: Left/Right, Home/End (`Ctrl-A`/`Ctrl-E`,
//!   `Ctrl-B`/`Ctrl-F`), and `Alt-B`/`Alt-F` word motion.
//! * **Editing**: Backspace/Delete (`Ctrl-D` on a non-empty line), the
//!   kill/yank set (`Ctrl-K`, `Ctrl-U`, `Ctrl-W`, `Alt-D`, `Ctrl-Y`),
//!   `Ctrl-T` transpose, and bracketed paste as literal text.
//! * **Session**: `Ctrl-C` cancels the line, `Ctrl-D` on an empty line ends
//!   input, `Ctrl-L` repaints on a cleared screen, Tab completes
//!   ([`Completer`]).
//!
//! Rendering is a bounded single-row viewport: the line scrolls horizontally
//! under a fixed prompt, so a repaint never wraps and never depends on the
//! terminal's wrap behaviour. Columns are counted per character (the shared
//! width tables refine this when the curses menu rendering lands).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_complete::common_prefix;
use tairix_curses::Event;
use tairix_vt::{EraseMode, Op};

use crate::complete::Completion;
use crate::host::Console;
use crate::repl::MAX_LINE;

/// Upper bound on retained history entries. A bounded advisory cache, not a
/// scalable capacity: the oldest entry is dropped when the bound is reached.
const HISTORY_CAP: usize = 512;

/// Fallback terminal width when the backing cannot report one.
const DEFAULT_WIDTH: usize = 80;

/// Minimum viewport width the renderer insists on, however long the prompt
/// is, so a huge prompt cannot reduce editing to a zero-width window.
const MIN_VIEW: usize = 8;

/// The completion seam the Tab key drives: the line and cursor in, the
/// shared [`Completion`] result out. Implemented by the REPL over
/// [`crate::complete::complete`] with the shell's `PATH` and the injected
/// directory lister; a fixture in tests.
pub(crate) trait Completer {
    /// Compute candidates for `line` with the cursor after character
    /// `cursor`. Read-only; never changes `$?`.
    fn complete(&self, line: &str, cursor: usize) -> Completion;
}

/// How an edited line read ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReadOutcome {
    /// Enter submitted the line (without its terminator).
    Line(String),
    /// `Ctrl-C` discarded the line under edit.
    Cancelled,
    /// `Ctrl-D` on an empty line: the session asked to end input.
    Eof,
}

/// The editor's cross-line state: the session history.
pub(crate) struct Editor {
    history: Vec<String>,
}

impl Editor {
    pub(crate) fn new() -> Self {
        Self {
            history: Vec::new(),
        }
    }

    /// Remember a submitted line: blank lines and consecutive duplicates are
    /// not retained, and the store is bounded by [`HISTORY_CAP`].
    pub(crate) fn remember(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        if self.history.len() == HISTORY_CAP {
            self.history.remove(0);
        }
        self.history.push(String::from(line));
    }
}

/// Incremental reverse-search state (`Ctrl-R`).
struct Search {
    /// The query typed so far.
    query: String,
    /// Index into the history of the current match, if any.
    match_index: Option<usize>,
    /// The line (and cursor) as they were when the search opened, restored
    /// on abort (`Ctrl-G`).
    saved_line: Vec<char>,
    saved_cursor: usize,
}

/// One line under edit.
pub(crate) struct Session<'a> {
    editor: &'a mut Editor,
    prompt: String,
    prompt_cols: usize,
    width: usize,
    line: Vec<char>,
    cursor: usize,
    /// First visible character of the horizontal viewport.
    scroll: usize,
    /// `Some(i)` while showing history entry `i`; `None` on the live line.
    history_index: Option<usize>,
    /// The live line saved while walking history.
    draft: Vec<char>,
    /// The kill buffer `Ctrl-Y` reinserts.
    yank: Vec<char>,
    search: Option<Search>,
}

impl<'a> Session<'a> {
    /// Open a session for one line under `prompt`, rendering into `width`
    /// columns (the caller passes the terminal's width, or `None` for the
    /// [`DEFAULT_WIDTH`] fallback).
    pub(crate) fn new(editor: &'a mut Editor, prompt: String, width: Option<u16>) -> Self {
        let prompt_cols = prompt.chars().count();
        Self {
            editor,
            prompt,
            prompt_cols,
            width: width.map_or(DEFAULT_WIDTH, usize::from).max(2),
            line: Vec::new(),
            cursor: 0,
            scroll: 0,
            history_index: None,
            draft: Vec::new(),
            yank: Vec::new(),
            search: None,
        }
    }

    /// Draw the prompt and the (empty) line: the caller renders once before
    /// feeding events.
    pub(crate) fn render(&mut self, console: &dyn Console) {
        let (banner, banner_cols) = match &self.search {
            Some(search) => {
                let banner = alloc::format!("(reverse-i-search)`{}': ", search.query);
                let cols = banner.chars().count();
                (banner, cols)
            }
            None => (self.prompt.clone(), self.prompt_cols),
        };
        let avail = self
            .width
            .saturating_sub(banner_cols)
            .saturating_sub(1)
            .max(MIN_VIEW);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor > self.scroll + avail {
            self.scroll = self.cursor - avail;
        }
        let end = (self.scroll + avail).min(self.line.len());
        let mut ops = Vec::new();
        ops.push(Op::CarriageReturn);
        for c in banner.chars() {
            ops.push(Op::Print(c));
        }
        for &c in &self.line[self.scroll..end] {
            ops.push(Op::Print(c));
        }
        ops.push(Op::EraseInLine(EraseMode::ToEnd));
        ops.push(Op::CarriageReturn);
        let col = banner_cols + (self.cursor - self.scroll);
        if let Ok(col) = u16::try_from(col) {
            if col > 0 {
                ops.push(Op::CursorForward(col));
            }
        }
        console.write_stdout(&encode_ops(&ops));
    }

    /// Apply one decoded key event; `Some` ends the read. The caller calls
    /// [`Session::render`] after every event that returns `None`.
    pub(crate) fn handle(
        &mut self,
        event: Event,
        console: &dyn Console,
        completer: &dyn Completer,
    ) -> Option<ReadOutcome> {
        if self.search.is_some() {
            return self.handle_search(event, console, completer);
        }
        match event {
            Event::Char(c) => {
                self.insert_char(c);
                None
            }
            Event::Paste(text) => {
                // Pasted text is literal content: line breaks become spaces
                // (a paste never auto-runs), other control bytes are dropped.
                for c in text.chars() {
                    match c {
                        '\r' | '\n' | '\t' => self.insert_char(' '),
                        c if !c.is_control() => self.insert_char(c),
                        _ => {}
                    }
                }
                None
            }
            Event::Enter => Some(self.submit()),
            Event::Tab => {
                self.complete(console, completer);
                None
            }
            Event::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.line.remove(self.cursor);
                }
                None
            }
            Event::Delete => {
                if self.cursor < self.line.len() {
                    self.line.remove(self.cursor);
                }
                None
            }
            Event::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            Event::Right => {
                self.cursor = (self.cursor + 1).min(self.line.len());
                None
            }
            Event::Home => {
                self.cursor = 0;
                None
            }
            Event::End => {
                self.cursor = self.line.len();
                None
            }
            Event::Up => {
                self.history_step_back();
                None
            }
            Event::Down => {
                self.history_step_forward();
                None
            }
            Event::Ctrl(c) => self.handle_ctrl(c, console),
            Event::Alt(c) => {
                self.handle_alt(c);
                None
            }
            // Bare Escape, the named function keys, mouse reports: nothing
            // for a line editor to do.
            Event::Esc
            | Event::Function(_)
            | Event::Insert
            | Event::PageUp
            | Event::PageDown
            | Event::Mouse(_) => None,
        }
    }

    /// The control-chorded keys (the readline set).
    fn handle_ctrl(&mut self, c: char, console: &dyn Console) -> Option<ReadOutcome> {
        match c {
            'a' => self.cursor = 0,
            'e' => self.cursor = self.line.len(),
            'b' => self.cursor = self.cursor.saturating_sub(1),
            'f' => self.cursor = (self.cursor + 1).min(self.line.len()),
            'c' => return Some(ReadOutcome::Cancelled),
            'd' => {
                if self.line.is_empty() {
                    return Some(ReadOutcome::Eof);
                }
                if self.cursor < self.line.len() {
                    self.line.remove(self.cursor);
                }
            }
            'k' => {
                self.yank = self.line.split_off(self.cursor);
            }
            'u' => {
                let tail = self.line.split_off(self.cursor);
                self.yank = core::mem::replace(&mut self.line, tail);
                self.cursor = 0;
            }
            'w' => {
                let start = self.prev_word_start();
                self.yank = self.line.drain(start..self.cursor).collect();
                self.cursor = start;
            }
            'y' => {
                let yank = self.yank.clone();
                for c in yank {
                    self.insert_char(c);
                }
            }
            't' => self.transpose(),
            'l' => {
                // Clear the screen and let the caller's render repaint the
                // prompt and line at the top.
                console.write_stdout(&encode_ops(&[
                    Op::EraseInDisplay(EraseMode::All),
                    Op::CursorPosition { row: 1, col: 1 },
                ]));
            }
            'p' => self.history_step_back(),
            'n' => self.history_step_forward(),
            'r' => {
                self.search = Some(Search {
                    query: String::new(),
                    match_index: None,
                    saved_line: self.line.clone(),
                    saved_cursor: self.cursor,
                });
            }
            // `Ctrl-Z` suspends the *foreground job*, and at the prompt
            // there is none; job-stop delivery to a running child is the
            // staged kernel work (`.junie/plan-session-shell.md`, part 3).
            // The remaining chords have no line-editor meaning.
            _ => {}
        }
        None
    }

    /// The Alt-chorded word keys.
    fn handle_alt(&mut self, c: char) {
        match c {
            'b' => self.cursor = self.prev_word_start(),
            'f' => self.cursor = self.next_word_end(),
            'd' => {
                let end = self.next_word_end();
                self.yank = self.line.drain(self.cursor..end).collect();
            }
            _ => {}
        }
    }

    /// Reverse-search mode: printable characters extend the query, `Ctrl-R`
    /// steps to an older match, Backspace narrows, `Ctrl-G` aborts, Escape
    /// accepts, Enter accepts and submits, and any other key accepts the
    /// match and is then applied as ordinary editing.
    fn handle_search(
        &mut self,
        event: Event,
        console: &dyn Console,
        completer: &dyn Completer,
    ) -> Option<ReadOutcome> {
        match event {
            Event::Char(c) => {
                if let Some(search) = self.search.as_mut() {
                    search.query.push(c);
                }
                self.search_step(false);
                None
            }
            Event::Backspace => {
                if let Some(search) = self.search.as_mut() {
                    search.query.pop();
                }
                self.search_step(false);
                None
            }
            Event::Ctrl('r') => {
                self.search_step(true);
                None
            }
            Event::Ctrl('g') => {
                if let Some(search) = self.search.take() {
                    self.line = search.saved_line;
                    self.cursor = search.saved_cursor;
                }
                None
            }
            Event::Esc => {
                self.search = None;
                None
            }
            Event::Enter => {
                self.search = None;
                Some(self.submit())
            }
            other => {
                self.search = None;
                self.handle(other, console, completer)
            }
        }
    }

    /// Find (or advance to) the newest history entry containing the query.
    /// `step` searches strictly older than the current match.
    fn search_step(&mut self, step: bool) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let history = &self.editor.history;
        let newest = match (step, search.match_index) {
            (true, Some(current)) => {
                if current == 0 {
                    return;
                }
                current - 1
            }
            _ => history.len().wrapping_sub(1),
        };
        if history.is_empty() || search.query.is_empty() {
            return;
        }
        let found = (0..=newest)
            .rev()
            .find(|&i| history[i].contains(search.query.as_str()));
        if let Some(i) = found {
            search.match_index = Some(i);
            self.line = history[i].chars().collect();
            self.cursor = self.line.len();
        }
    }

    /// Enter: leave history browsing and hand the line over.
    fn submit(&mut self) -> ReadOutcome {
        self.history_index = None;
        let line: String = self.line.iter().collect();
        ReadOutcome::Line(line)
    }

    /// Insert one character at the cursor, respecting the line bound.
    fn insert_char(&mut self, c: char) {
        if self.line.len() >= MAX_LINE {
            return;
        }
        self.line.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Up / `Ctrl-P`: show the previous history entry, saving the live line
    /// as the draft on first use.
    fn history_step_back(&mut self) {
        let history = &self.editor.history;
        let next = match self.history_index {
            None if !history.is_empty() => {
                self.draft = core::mem::take(&mut self.line);
                Some(history.len() - 1)
            }
            Some(i) if i > 0 => Some(i - 1),
            other => other,
        };
        if let Some(i) = next {
            if self.history_index != Some(i) || self.line.is_empty() {
                self.line = history[i].chars().collect();
                self.cursor = self.line.len();
            }
            self.history_index = Some(i);
        }
    }

    /// Down / `Ctrl-N`: show the next entry, or restore the draft past the
    /// newest one.
    fn history_step_forward(&mut self) {
        match self.history_index {
            Some(i) if i + 1 < self.editor.history.len() => {
                self.history_index = Some(i + 1);
                self.line = self.editor.history[i + 1].chars().collect();
                self.cursor = self.line.len();
            }
            Some(_) => {
                self.history_index = None;
                self.line = core::mem::take(&mut self.draft);
                self.cursor = self.line.len();
            }
            None => {}
        }
    }

    /// `Ctrl-T`: transpose the two characters before/under the cursor
    /// (readline semantics: at end of line the last two swap).
    fn transpose(&mut self) {
        let len = self.line.len();
        if len < 2 || self.cursor == 0 {
            return;
        }
        if self.cursor == len {
            self.line.swap(len - 2, len - 1);
        } else {
            self.line.swap(self.cursor - 1, self.cursor);
            self.cursor += 1;
        }
    }

    /// Start of the word before the cursor (whitespace-delimited).
    fn prev_word_start(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.line[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.line[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// End of the word after the cursor (whitespace-delimited).
    fn next_word_end(&self) -> usize {
        let mut i = self.cursor;
        while i < self.line.len() && self.line[i].is_whitespace() {
            i += 1;
        }
        while i < self.line.len() && !self.line[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Tab: complete the word under the cursor. A unique candidate is
    /// inserted (with its closing character); several candidates extend to
    /// their longest common prefix, or — when nothing extends — are listed
    /// inline, after which the caller's render repaints the prompt.
    fn complete(&mut self, console: &dyn Console, completer: &dyn Completer) {
        let line: String = self.line.iter().collect();
        let completion = completer.complete(&line, self.cursor);
        match completion.candidates.as_slice() {
            [] => {}
            [only] => {
                let mut insert: Vec<char> = only.insert.chars().collect();
                if let Some(closing) = only.closing {
                    insert.push(closing);
                }
                self.replace_span(completion.start, completion.end, &insert);
            }
            many => {
                let common = common_prefix(many.iter().map(|c| c.insert.as_str()));
                let common: Vec<char> = common.chars().collect();
                if common.len() > completion.end - completion.start {
                    self.replace_span(completion.start, completion.end, &common);
                } else {
                    self.list_candidates(console, many);
                }
            }
        }
    }

    /// Replace the word span `[start, end)` with `text`, bounded by
    /// [`MAX_LINE`]; the cursor lands after the inserted text.
    fn replace_span(&mut self, start: usize, end: usize, text: &[char]) {
        if start > end || end > self.line.len() {
            return;
        }
        if self.line.len() - (end - start) + text.len() > MAX_LINE {
            return;
        }
        let tail = self.line.split_off(end);
        self.line.truncate(start);
        self.line.extend_from_slice(text);
        self.cursor = self.line.len();
        self.line.extend(tail);
    }

    /// Write the candidate listing on its own lines; the caller's render
    /// then repaints the prompt and line below it.
    fn list_candidates(&self, console: &dyn Console, candidates: &[crate::complete::Candidate]) {
        let mut out = String::from("\r\n");
        let mut col = 0usize;
        for candidate in candidates {
            let cell = candidate.display.chars().count() + 2;
            if col > 0 && col + cell > self.width {
                out.push_str("\r\n");
                col = 0;
            }
            out.push_str(&candidate.display);
            out.push_str("  ");
            col += cell;
        }
        out.push_str("\r\n");
        console.write_stdout(&out);
    }
}

/// Encode a run of terminal operations through the one shared `lib/vt`
/// emitter. The encoded bytes are UTF-8 by construction; encoding failure
/// degrades to writing nothing (fail closed, never a panic).
fn encode_ops(ops: &[Op]) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    tairix_vt::emit::encode_all_into(ops, &mut bytes);
    String::from_utf8(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Completer, Editor, ReadOutcome, Session};
    use crate::complete::{Candidate, Completion};
    use crate::test_support::RecordingConsole;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;
    use tairix_curses::Event;

    /// A scripted completer: returns a fixed span and candidate list.
    struct FixedCompleter {
        start: usize,
        candidates: Vec<Candidate>,
    }

    impl FixedCompleter {
        fn none() -> Self {
            Self {
                start: 0,
                candidates: Vec::new(),
            }
        }

        fn with(start: usize, inserts: &[&str], closing: Option<char>) -> Self {
            Self {
                start,
                candidates: inserts
                    .iter()
                    .map(|insert| Candidate {
                        insert: (*insert).to_string(),
                        display: (*insert).to_string(),
                        closing,
                    })
                    .collect(),
            }
        }
    }

    impl Completer for FixedCompleter {
        fn complete(&self, _line: &str, cursor: usize) -> Completion {
            Completion {
                start: self.start,
                end: cursor,
                candidates: self.candidates.clone(),
            }
        }
    }

    /// Feed `events` into a fresh session over `editor`, returning the
    /// outcome that ended the read (if any) and the console transcript.
    fn drive(
        editor: &mut Editor,
        events: impl IntoIterator<Item = Event>,
        completer: &dyn Completer,
    ) -> (Option<ReadOutcome>, String) {
        let console = RecordingConsole::new();
        let mut session = Session::new(editor, String::from("% "), Some(40));
        session.render(&console);
        for event in events {
            if let Some(outcome) = session.handle(event, &console, completer) {
                return (Some(outcome), console.stdout());
            }
            session.render(&console);
        }
        (None, console.stdout())
    }

    fn chars(text: &str) -> Vec<Event> {
        text.chars().map(Event::Char).collect()
    }

    #[test]
    fn typed_line_submits_on_enter() {
        let mut editor = Editor::new();
        let mut events = chars("echo hi");
        events.push(Event::Enter);
        let (outcome, out) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("echo hi".to_string())));
        assert!(out.contains("echo hi"), "the line was echoed: {out:?}");
    }

    #[test]
    fn arrows_edit_in_place() {
        let mut editor = Editor::new();
        // "ecoh" -> Left, Left, fix to "echo".
        let mut events = chars("ecoh");
        events.extend([Event::Left, Event::Left]);
        events.push(Event::Delete); // drop the 'o' under the cursor -> "ech"
        events.push(Event::End);
        events.push(Event::Char('o'));
        events.push(Event::Enter);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("echo".to_string())));
    }

    #[test]
    fn up_and_down_walk_history_and_preserve_the_draft() {
        let mut editor = Editor::new();
        editor.remember("first");
        editor.remember("second");
        // Type a draft, go two entries back, one forward, then return to the
        // draft and submit it.
        let mut events = chars("draft");
        events.extend([Event::Up, Event::Up, Event::Down, Event::Down]);
        events.push(Event::Enter);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("draft".to_string())));

        // And the entries themselves are reachable.
        let (outcome, _) = drive(
            &mut editor,
            [Event::Up, Event::Up, Event::Enter],
            &FixedCompleter::none(),
        );
        assert_eq!(outcome, Some(ReadOutcome::Line("first".to_string())));
    }

    #[test]
    fn history_skips_blanks_and_consecutive_duplicates_and_is_bounded() {
        let mut editor = Editor::new();
        editor.remember("");
        editor.remember("   ");
        editor.remember("same");
        editor.remember("same");
        assert_eq!(editor.history, ["same"]);
        for i in 0..super::HISTORY_CAP + 10 {
            editor.remember(&alloc::format!("cmd {i}"));
        }
        assert_eq!(editor.history.len(), super::HISTORY_CAP);
        assert_eq!(editor.history.last().map(String::as_str), Some("cmd 521"));
    }

    #[test]
    fn control_keys_move_kill_and_yank() {
        let mut editor = Editor::new();
        // ^A home, ^K kills all into the yank buffer, ^Y ^Y pastes it twice.
        let mut events = chars("abc");
        events.extend([
            Event::Ctrl('a'),
            Event::Ctrl('k'),
            Event::Ctrl('y'),
            Event::Ctrl('y'),
            Event::Enter,
        ]);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("abcabc".to_string())));

        // ^U kills to the start; ^E end; ^W kills the previous word.
        let mut events = chars("one two");
        events.extend([Event::Ctrl('w'), Event::Enter]);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("one ".to_string())));

        let mut events = chars("gone");
        events.extend([Event::Ctrl('u'), Event::Enter]);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line(String::new())));
    }

    #[test]
    fn ctrl_t_transposes() {
        let mut editor = Editor::new();
        let mut events = chars("sl");
        events.extend([Event::Ctrl('t'), Event::Enter]);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("ls".to_string())));
    }

    #[test]
    fn alt_word_motion_and_kill() {
        let mut editor = Editor::new();
        let mut events = chars("one two");
        // Alt-B to the start of "two", Alt-B to the start of "one",
        // Alt-D kills "one".
        events.extend([Event::Alt('b'), Event::Alt('b'), Event::Alt('d')]);
        events.push(Event::Enter);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line(" two".to_string())));
    }

    #[test]
    fn ctrl_c_cancels_and_ctrl_d_ends_or_deletes() {
        let mut editor = Editor::new();
        let mut events = chars("doomed");
        events.push(Event::Ctrl('c'));
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Cancelled));

        // ^D on an empty line is end-of-input.
        let (outcome, _) = drive(&mut editor, [Event::Ctrl('d')], &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Eof));

        // ^D on a non-empty line deletes under the cursor.
        let mut events = chars("axb");
        events.extend([Event::Left, Event::Left, Event::Ctrl('d'), Event::Enter]);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("ab".to_string())));
    }

    #[test]
    fn reverse_search_finds_steps_and_aborts() {
        let mut editor = Editor::new();
        editor.remember("echo alpha");
        editor.remember("cat beta");
        editor.remember("echo gamma");

        // ^R e c h o finds the newest "echo" line; ^R again steps older.
        let mut events = vec![Event::Ctrl('r')];
        events.extend(chars("echo"));
        events.extend([Event::Ctrl('r'), Event::Enter]);
        let (outcome, out) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("echo alpha".to_string())));
        assert!(
            out.contains("(reverse-i-search)"),
            "the search banner renders: {out:?}"
        );

        // ^G aborts back to the pre-search line.
        let mut events = chars("keep");
        events.push(Event::Ctrl('r'));
        events.extend(chars("echo"));
        events.extend([Event::Ctrl('g'), Event::Enter]);
        let (outcome, _) = drive(&mut editor, events, &FixedCompleter::none());
        assert_eq!(outcome, Some(ReadOutcome::Line("keep".to_string())));
    }

    #[test]
    fn tab_inserts_a_unique_candidate_with_its_closing() {
        let mut editor = Editor::new();
        let completer = FixedCompleter::with(0, &["cat"], Some(' '));
        let mut events = chars("ca");
        events.push(Event::Tab);
        events.push(Event::Enter);
        let (outcome, _) = drive(&mut editor, events, &completer);
        assert_eq!(outcome, Some(ReadOutcome::Line("cat ".to_string())));
    }

    #[test]
    fn tab_extends_to_the_common_prefix_or_lists() {
        let mut editor = Editor::new();
        let completer = FixedCompleter::with(0, &["notebooks/", "notes.txt"], None);
        let mut events = chars("no");
        events.push(Event::Tab);
        events.push(Event::Enter);
        let (outcome, _) = drive(&mut editor, events, &completer);
        assert_eq!(outcome, Some(ReadOutcome::Line("note".to_string())));

        // Already at the common prefix: Tab lists the candidates instead.
        let mut events = chars("note");
        events.push(Event::Tab);
        events.push(Event::Enter);
        let (outcome, out) = drive(&mut editor, events, &completer);
        assert_eq!(outcome, Some(ReadOutcome::Line("note".to_string())));
        assert!(out.contains("notebooks/"), "listing shown: {out:?}");
        assert!(out.contains("notes.txt"), "listing shown: {out:?}");
    }

    #[test]
    fn long_lines_scroll_the_viewport() {
        let mut editor = Editor::new();
        // 60 characters into a 40-column terminal: the tail stays visible,
        // the head scrolls out.
        let text: String = core::iter::repeat_n('x', 59).chain(['Z']).collect();
        let (_, out) = drive(&mut editor, chars(&text), &FixedCompleter::none());
        assert!(out.contains('Z'), "the cursor end stays visible");
        let last_paint = out.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
        assert!(
            last_paint.chars().count() <= 40 + 8,
            "one paint never exceeds the width plus its escape tail: {last_paint:?}"
        );
    }
}
