//! The normal/visual-mode key grammar: counts, registers, operators,
//! motions, and text objects.
//!
//! [`handle`] consumes one decoded event against the [`NormalState`]
//! accumulated so far (`"a3d2w` style prefixes) and drives the
//! [`Editor`] primitives. The same motion code paths move the cursor and
//! bound an operator, so they can never disagree.

use alloc::string::String;

use tairix_curses::Event;

use crate::buffer::Position;
use crate::editor::{Editor, Mode};
use crate::fileio::FileIo;
use crate::motion::{self, MotionTarget};

/// A pending operator.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Operator {
    /// `d` — delete.
    Delete,
    /// `c` — change (delete, then insert).
    Change,
    /// `y` — yank.
    Yank,
}

/// What the next key is awaited as.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum Await {
    /// Nothing special: the next key is a command.
    #[default]
    Command,
    /// After `"`: the next key names a register.
    Register,
    /// After `f`/`F`/`t`/`T`: the next key is the target character; the
    /// payload is the find command itself.
    FindChar(char),
    /// After `r`: the next key replaces the character(s) under the cursor.
    ReplaceChar,
    /// After `g`: the next key completes a `g` command (`gg`).
    GPrefix,
    /// After `i`/`a` while an operator (or visual mode) pends: the next
    /// key names a text object; `true` for the `a` (around) form.
    Object(bool),
    /// After `Z`: the next key completes `ZZ` (write and quit) or `ZQ`
    /// (quit without writing).
    ZPrefix,
}

/// The accumulated prefix state of a normal-mode command.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NormalState {
    /// The count before the operator (`3dw`).
    pub count_before: Option<usize>,
    /// The count after the operator (`d3w`).
    pub count_after: Option<usize>,
    /// The named register (`"a`).
    pub register: Option<char>,
    /// The pending operator.
    pub operator: Option<Operator>,
    /// What the next key completes.
    pub awaiting: Await,
}

impl NormalState {
    /// The effective count: both counts multiply, absent counts are 1
    /// (vim's rule for `2d3w` = 6 words).
    #[must_use]
    pub fn count(&self) -> usize {
        self.count_before.unwrap_or(1) * self.count_after.unwrap_or(1)
    }

    /// Whether any count was explicitly given.
    #[must_use]
    pub fn has_count(&self) -> bool {
        self.count_before.is_some() || self.count_after.is_some()
    }

    /// Push one decimal digit onto whichever count is being typed.
    fn push_digit(&mut self, digit: usize) {
        let slot = if self.operator.is_some() {
            &mut self.count_after
        } else {
            &mut self.count_before
        };
        // Saturate: a count past any real file size behaves like "to the
        // end", never an overflow.
        let current = slot.unwrap_or(0);
        *slot = Some(current.saturating_mul(10).saturating_add(digit));
    }
}

/// Handle one normal- or visual-mode event.
pub fn handle(editor: &mut Editor, event: &Event, io: &dyn FileIo) {
    match core::mem::take(&mut editor.pending.awaiting) {
        Await::Command => command_key(editor, event, io),
        Await::Register => register_key(editor, event),
        Await::FindChar(cmd) => find_char_key(editor, cmd, event),
        Await::ReplaceChar => replace_char_key(editor, event),
        Await::GPrefix => g_key(editor, event),
        Await::Object(around) => object_key(editor, around, event),
        Await::ZPrefix => z_key(editor, event, io),
    }
}

/// Reset the pending state after a completed or abandoned command.
fn reset(editor: &mut Editor) {
    editor.pending = NormalState::default();
}

/// After `"`: bind the register name.
fn register_key(editor: &mut Editor, event: &Event) {
    match event {
        Event::Char(name) if name.is_ascii_alphabetic() => {
            editor.pending.register = Some(*name);
        }
        _ => reset(editor),
    }
}

/// After `f`/`F`/`t`/`T`: run the find with the typed target.
fn find_char_key(editor: &mut Editor, cmd: char, event: &Event) {
    let Event::Char(target) = event else {
        reset(editor);
        editor.finish_command();
        return;
    };
    editor.last_find = Some((cmd, *target));
    run_find(editor, cmd, *target);
}

/// Execute `f`/`F`/`t`/`T` (also `;`/`,` repeats) and any pending
/// operator over it.
fn run_find(editor: &mut Editor, cmd: char, target: char) {
    let count = editor.pending.count();
    let forward = cmd == 'f' || cmd == 't';
    let till = cmd == 't' || cmd == 'T';
    let found = motion::find_char(&editor.buffer, editor.cursor, target, count, forward, till);
    if let Some(hit) = found {
        apply_motion(editor, hit, false);
    } else {
        reset(editor);
        editor.finish_command();
    }
}

/// After `r`: replace the character(s) under the cursor.
fn replace_char_key(editor: &mut Editor, event: &Event) {
    let count = editor.pending.count();
    if let Event::Char(ch) = event {
        editor.replace_chars(*ch, count);
        reset(editor);
        editor.finish_change();
    } else {
        reset(editor);
        editor.finish_command();
    }
}

/// After `i`/`a` with an operator (or visual mode) pending: complete the
/// text object.
fn object_key(editor: &mut Editor, around: bool, event: &Event) {
    let Event::Char(ch) = event else {
        reset(editor);
        editor.finish_command();
        return;
    };
    let span = match *ch {
        'w' => motion::word_object(&editor.buffer, editor.cursor, around),
        '(' | ')' | 'b' => motion::pair_object(&editor.buffer, editor.cursor, '(', ')', around),
        '[' | ']' => motion::pair_object(&editor.buffer, editor.cursor, '[', ']', around),
        '{' | '}' | 'B' => motion::pair_object(&editor.buffer, editor.cursor, '{', '}', around),
        '<' | '>' => motion::pair_object(&editor.buffer, editor.cursor, '<', '>', around),
        '"' => motion::quote_object(&editor.buffer, editor.cursor, '"', around),
        '\'' => motion::quote_object(&editor.buffer, editor.cursor, '\'', around),
        '`' => motion::quote_object(&editor.buffer, editor.cursor, '`', around),
        _ => None,
    };
    let Some(span) = span else {
        reset(editor);
        editor.finish_command();
        return;
    };
    if let Some(op) = editor.pending.operator {
        apply_operator_span(editor, op, span.start, span.end, false);
        return;
    }
    // In visual mode the object becomes the selection.
    if matches!(editor.mode, Mode::Visual { .. }) {
        editor.visual_anchor = span.start;
        editor.cursor = span.end;
        editor.clamp_cursor();
    }
    reset(editor);
}

/// Apply a completed motion: bound the pending operator with it, extend a
/// visual selection, or just move the cursor.
fn apply_motion(editor: &mut Editor, target: MotionTarget, vertical: bool) {
    if let Some(op) = editor.pending.operator {
        if !matches!(editor.mode, Mode::Visual { .. }) {
            if let Some((start, end, linewise)) = editor.operator_span(target) {
                apply_operator_span(editor, op, start, end, linewise);
            } else {
                // An empty exclusive span: the operator does nothing.
                reset(editor);
                editor.finish_command();
            }
            return;
        }
    }
    editor.move_to(target, vertical);
    reset(editor);
    if !matches!(editor.mode, Mode::Visual { .. }) {
        editor.finish_command();
    }
}

/// Run an operator over an inclusive span.
fn apply_operator_span(
    editor: &mut Editor,
    op: Operator,
    start: Position,
    end: Position,
    linewise: bool,
) {
    match op {
        Operator::Yank => {
            editor.yank_span(start, end, linewise);
            // The cursor lands at the span start; a linewise yank keeps
            // its column, as in vim.
            editor.cursor = if linewise {
                Position::new(start.line, editor.cursor.col)
            } else {
                start
            };
            editor.clamp_cursor();
            reset(editor);
            editor.finish_command();
        }
        Operator::Delete => {
            editor.delete_span(start, end, linewise, true);
            reset(editor);
            editor.finish_change();
        }
        Operator::Change => {
            if linewise {
                // A linewise change replaces the lines with one open line
                // to type into (never routed through the delete path,
                // whose empty-buffer restoration would leave a stray
                // second line).
                editor.yank_span(start, end, true);
                editor.buffer.begin_edit(editor.cursor);
                editor
                    .buffer
                    .replace_lines(start.line, end.line + 1, alloc::vec![String::new()]);
                editor.cursor = Position::new(start.line, 0);
            } else {
                editor.delete_span(start, end, false, false);
            }
            reset(editor);
            editor.enter_insert(false);
        }
    }
}

/// Run an operator over the visual selection and leave visual mode. The
/// span is the one shared [`Editor::selection`] the renderer highlights.
fn visual_operator(editor: &mut Editor, op: Operator) {
    let Some((start, end, linewise)) = editor.selection() else {
        return;
    };
    editor.mode = Mode::Normal;
    apply_operator_span(editor, op, start, end, linewise);
}

/// After `g`: only `gg` is a command here.
fn g_key(editor: &mut Editor, event: &Event) {
    if let Event::Char('g') = event {
        let target = if editor.pending.has_count() {
            editor.pending.count()
        } else {
            1
        };
        let hit = motion::goto_line(&editor.buffer, target);
        apply_motion(editor, hit, false);
    } else {
        reset(editor);
        editor.finish_command();
    }
}

/// After `Z`: `ZZ` writes and quits, `ZQ` quits without writing.
fn z_key(editor: &mut Editor, event: &Event, io: &dyn FileIo) {
    match event {
        Event::Char('Z') => {
            if !editor.buffer.is_modified() || editor.write_buffer(None, false, io) {
                editor.quit = Some(0);
            }
        }
        Event::Char('Q') => editor.quit = Some(0),
        _ => {}
    }
    reset(editor);
    editor.finish_command();
}

/// One event in normal or visual mode when nothing more specific pends.
fn command_key(editor: &mut Editor, event: &Event, io: &dyn FileIo) {
    let count = editor.pending.count();
    match event {
        Event::Char(ch) => char_key(editor, *ch, io),
        Event::Ctrl(ch) => ctrl_key(editor, *ch),
        // Backspace moves left in normal mode, like `h`.
        Event::Left | Event::Backspace => {
            apply_motion(editor, motion::left(editor.cursor, count), false);
        }
        Event::Right => {
            apply_motion(
                editor,
                motion::right(&editor.buffer, editor.cursor, count),
                false,
            );
        }
        Event::Up => apply_motion(editor, motion::up(editor.cursor, count), true),
        Event::Down => {
            apply_motion(
                editor,
                motion::down(&editor.buffer, editor.cursor, count),
                true,
            );
        }
        Event::Home => apply_motion(editor, motion::line_start(editor.cursor), false),
        Event::End => {
            apply_motion(
                editor,
                motion::line_end(&editor.buffer, editor.cursor, count),
                false,
            );
        }
        Event::Enter => {
            // Enter moves to the first non-blank of the next line, like
            // vim's `+`.
            let down = motion::down(&editor.buffer, editor.cursor, count);
            let line = down.pos.line;
            let target = Position::new(line, motion::first_non_blank(&editor.buffer, line));
            apply_motion(editor, MotionTarget::linewise(target), false);
        }
        Event::PageDown => {
            editor.scroll_page(true);
            reset(editor);
            editor.finish_command();
        }
        Event::PageUp => {
            editor.scroll_page(false);
            reset(editor);
            editor.finish_command();
        }
        Event::Delete => delete_under_cursor(editor, count, false),
        Event::Insert => {
            editor.enter_insert(false);
            reset(editor);
        }
        Event::Esc => {
            if matches!(editor.mode, Mode::Visual { .. }) {
                editor.mode = Mode::Normal;
            }
            reset(editor);
            editor.finish_command();
        }
        // Everything else (function keys, mouse, pastes) carries no
        // normal-mode command.
        _ => {
            reset(editor);
            editor.finish_command();
        }
    }
}

/// A control-chorded normal-mode command.
fn ctrl_key(editor: &mut Editor, ch: char) {
    match ch {
        'd' => editor.scroll_half(true),
        'u' => editor.scroll_half(false),
        'f' => editor.scroll_page(true),
        'b' => editor.scroll_page(false),
        'r' => {
            match editor.buffer.redo() {
                Some(pos) => {
                    editor.cursor = pos;
                    editor.clamp_cursor();
                }
                None => editor.info(String::from("Already at newest change")),
            }
            reset(editor);
            editor.finish_command();
            return;
        }
        'g' => {
            let name = editor.buffer.name().unwrap_or("[No Name]");
            let lines = editor.buffer.len_lines();
            let modified = if editor.buffer.is_modified() {
                " [Modified]"
            } else {
                ""
            };
            let percent = ((editor.cursor.line + 1) * 100) / lines;
            editor.info(alloc::format!(
                "\"{name}\"{modified} {lines} lines --{percent}%--"
            ));
        }
        // Everything else — including `Ctrl-L` (redraw), which the
        // renderer's full repaint each cycle already provides — is a
        // no-op.
        _ => {}
    }
    reset(editor);
    editor.finish_command();
}

/// Delete `count` characters under/after (`x`) or before (`X`) the cursor.
fn delete_under_cursor(editor: &mut Editor, count: usize, before: bool) {
    let len = editor.buffer.line_len(editor.cursor.line);
    if before {
        if editor.cursor.col == 0 {
            reset(editor);
            editor.finish_command();
            return;
        }
        let start = Position::new(editor.cursor.line, editor.cursor.col.saturating_sub(count));
        let end = Position::new(editor.cursor.line, editor.cursor.col - 1);
        editor.delete_span(start, end, false, true);
    } else {
        if len == 0 {
            reset(editor);
            editor.finish_command();
            return;
        }
        let end = Position::new(
            editor.cursor.line,
            (editor.cursor.col + count - 1).min(len - 1),
        );
        editor.delete_span(editor.cursor, end, false, true);
    }
    reset(editor);
    editor.finish_change();
}

/// An operator key (`d`, `c`, `y`): apply to a visual selection, double
/// into a linewise whole-line operation, or start pending.
fn operator_key(editor: &mut Editor, op: Operator) {
    if matches!(editor.mode, Mode::Visual { .. }) {
        visual_operator(editor, op);
        return;
    }
    match editor.pending.operator {
        None => editor.pending.operator = Some(op),
        Some(pending) if pending == op => {
            // `dd` / `cc` / `yy`: the current line, count lines down.
            let count = editor.pending.count();
            let start = Position::new(editor.cursor.line, 0);
            let end_line = (editor.cursor.line + count - 1).min(editor.buffer.len_lines() - 1);
            let end = Position::new(end_line, editor.buffer.line_len(end_line).saturating_sub(1));
            apply_operator_span(editor, op, start, end, true);
        }
        Some(_) => {
            // A conflicting operator abandons the command, as in vim.
            reset(editor);
            editor.finish_command();
        }
    }
}

/// A printable-character command in normal or visual mode: the prefix
/// keys extend the pending command, the movement keys complete a motion
/// (or bound a pending operator), and the rest edit or drive the session.
fn char_key(editor: &mut Editor, ch: char, io: &dyn FileIo) {
    if prefix_key(editor, ch) {
        return;
    }
    if movement_key(editor, ch) || jump_key(editor, ch) {
        return;
    }
    if insert_entry_key(editor, ch) || edit_key(editor, ch) {
        return;
    }
    session_key(editor, ch, io);
}

/// The keys that extend the pending command instead of completing one:
/// count digits, the register/find/replace/object/`g`/`Z` prefixes, and
/// the operator keys. Returns whether `ch` was consumed.
fn prefix_key(editor: &mut Editor, ch: char) -> bool {
    let counting = editor.pending.count_before.is_some()
        || (editor.pending.operator.is_some() && editor.pending.count_after.is_some());
    let visual = matches!(editor.mode, Mode::Visual { .. });
    match ch {
        '0' if counting => editor.pending.push_digit(0),
        '1'..='9' => {
            let digit = (ch as u8 - b'0') as usize;
            editor.pending.push_digit(digit);
        }
        '"' => editor.pending.awaiting = Await::Register,
        'd' => operator_key(editor, Operator::Delete),
        'c' => operator_key(editor, Operator::Change),
        'y' => operator_key(editor, Operator::Yank),
        'g' => editor.pending.awaiting = Await::GPrefix,
        'Z' => editor.pending.awaiting = Await::ZPrefix,
        'f' | 'F' | 't' | 'T' => editor.pending.awaiting = Await::FindChar(ch),
        'r' => editor.pending.awaiting = Await::ReplaceChar,
        'i' if editor.pending.operator.is_some() || visual => {
            editor.pending.awaiting = Await::Object(false);
        }
        'a' if editor.pending.operator.is_some() || visual => {
            editor.pending.awaiting = Await::Object(true);
        }
        _ => return false,
    }
    true
}

/// The single-key motions (and the `;`/`,` find repeats). Returns whether
/// `ch` was one.
fn movement_key(editor: &mut Editor, ch: char) -> bool {
    let count = editor.pending.count();
    match ch {
        '0' => apply_motion(editor, motion::line_start(editor.cursor), false),
        'h' => apply_motion(editor, motion::left(editor.cursor, count), false),
        'l' | ' ' => {
            apply_motion(
                editor,
                motion::right(&editor.buffer, editor.cursor, count),
                false,
            );
        }
        'j' => apply_motion(
            editor,
            motion::down(&editor.buffer, editor.cursor, count),
            true,
        ),
        'k' => apply_motion(editor, motion::up(editor.cursor, count), true),
        'w' if editor.pending.operator == Some(Operator::Change) => {
            // vim's special case: `cw` behaves like `ce` on a word.
            let hit = motion::word_end(&editor.buffer, editor.cursor, count, false);
            apply_motion(editor, hit, false);
        }
        'w' => {
            apply_motion(
                editor,
                motion::word_forward(&editor.buffer, editor.cursor, count, false),
                false,
            );
        }
        'W' if editor.pending.operator == Some(Operator::Change) => {
            let hit = motion::word_end(&editor.buffer, editor.cursor, count, true);
            apply_motion(editor, hit, false);
        }
        'W' => {
            apply_motion(
                editor,
                motion::word_forward(&editor.buffer, editor.cursor, count, true),
                false,
            );
        }
        'e' => {
            apply_motion(
                editor,
                motion::word_end(&editor.buffer, editor.cursor, count, false),
                false,
            );
        }
        'E' => {
            apply_motion(
                editor,
                motion::word_end(&editor.buffer, editor.cursor, count, true),
                false,
            );
        }
        'b' => {
            apply_motion(
                editor,
                motion::word_back(&editor.buffer, editor.cursor, count, false),
                false,
            );
        }
        'B' => {
            apply_motion(
                editor,
                motion::word_back(&editor.buffer, editor.cursor, count, true),
                false,
            );
        }
        '^' => apply_motion(
            editor,
            motion::first_non_blank_motion(&editor.buffer, editor.cursor),
            false,
        ),
        '$' => apply_motion(
            editor,
            motion::line_end(&editor.buffer, editor.cursor, count),
            false,
        ),
        '{' => apply_motion(
            editor,
            motion::paragraph(&editor.buffer, editor.cursor, count, false),
            false,
        ),
        '}' => apply_motion(
            editor,
            motion::paragraph(&editor.buffer, editor.cursor, count, true),
            false,
        ),
        _ => return false,
    }
    true
}

/// The jump motions computed from more than plain position arithmetic:
/// `G`, `%`, the `;`/`,` find repeats, and the window rows `H`/`M`/`L`.
/// Returns whether `ch` was one.
fn jump_key(editor: &mut Editor, ch: char) -> bool {
    let count = editor.pending.count();
    match ch {
        'G' => {
            let target = if editor.pending.has_count() {
                count
            } else {
                editor.buffer.len_lines()
            };
            apply_motion(editor, motion::goto_line(&editor.buffer, target), false);
        }
        '%' => {
            if let Some(hit) = motion::match_pair(&editor.buffer, editor.cursor) {
                apply_motion(editor, hit, false);
            } else {
                reset(editor);
                editor.finish_command();
            }
        }
        ';' | ',' => {
            if let Some((cmd, target)) = editor.last_find {
                let cmd = if ch == ',' {
                    // `,` repeats with the direction reversed.
                    match cmd {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    }
                } else {
                    cmd
                };
                run_find(editor, cmd, target);
            } else {
                reset(editor);
                editor.finish_command();
            }
        }
        'H' => {
            let line = editor.view.top + count.saturating_sub(1);
            apply_motion(editor, motion::goto_line(&editor.buffer, line + 1), false);
        }
        'M' => {
            let line = editor.view.top + editor.view.rows / 2;
            apply_motion(editor, motion::goto_line(&editor.buffer, line + 1), false);
        }
        'L' => {
            let line = (editor.view.top + editor.view.rows).saturating_sub(count);
            apply_motion(editor, motion::goto_line(&editor.buffer, line + 1), false);
        }
        _ => return false,
    }
    true
}

/// The insert-mode entries (`i`, `a`, `I`, `A`, `o`, `O`, `R`) and the
/// visual anchor swap (`o` in visual mode). Returns whether `ch` was one.
fn insert_entry_key(editor: &mut Editor, ch: char) -> bool {
    let visual = matches!(editor.mode, Mode::Visual { .. });
    match ch {
        'i' => {
            editor.enter_insert(false);
            reset(editor);
        }
        'a' => {
            let len = editor.buffer.line_len(editor.cursor.line);
            editor.cursor = Position::new(editor.cursor.line, (editor.cursor.col + 1).min(len));
            editor.enter_insert(false);
            reset(editor);
        }
        'I' => {
            editor.cursor = Position::new(
                editor.cursor.line,
                motion::first_non_blank(&editor.buffer, editor.cursor.line),
            );
            editor.enter_insert(false);
            reset(editor);
        }
        'A' => {
            editor.cursor = Position::new(
                editor.cursor.line,
                editor.buffer.line_len(editor.cursor.line),
            );
            editor.enter_insert(false);
            reset(editor);
        }
        'o' if visual => {
            core::mem::swap(&mut editor.visual_anchor, &mut editor.cursor);
            reset(editor);
        }
        'o' => open_line(editor, true),
        'O' => open_line(editor, false),
        'R' => {
            editor.enter_insert(true);
            reset(editor);
        }
        _ => return false,
    }
    true
}

/// The completing edits: deletes, changes, yanks of the shorthand forms,
/// case toggle, join, and puts. Returns whether `ch` was one.
fn edit_key(editor: &mut Editor, ch: char) -> bool {
    let count = editor.pending.count();
    let visual = matches!(editor.mode, Mode::Visual { .. });
    match ch {
        'x' if visual => visual_operator(editor, Operator::Delete),
        'x' => delete_under_cursor(editor, count, false),
        'X' => delete_under_cursor(editor, count, true),
        's' if visual => visual_operator(editor, Operator::Change),
        's' => {
            let len = editor.buffer.line_len(editor.cursor.line);
            if len == 0 {
                editor.enter_insert(false);
                reset(editor);
            } else {
                let end = Position::new(
                    editor.cursor.line,
                    (editor.cursor.col + count - 1).min(len - 1),
                );
                editor.delete_span(editor.cursor, end, false, false);
                reset(editor);
                editor.enter_insert(false);
            }
        }
        'S' => {
            editor.pending.operator = Some(Operator::Change);
            operator_key(editor, Operator::Change);
        }
        'D' => {
            let target = motion::line_end(&editor.buffer, editor.cursor, count);
            editor.pending.operator = Some(Operator::Delete);
            apply_motion(editor, target, false);
        }
        'C' => {
            let target = motion::line_end(&editor.buffer, editor.cursor, count);
            editor.pending.operator = Some(Operator::Change);
            apply_motion(editor, target, false);
        }
        'Y' => {
            editor.pending.operator = Some(Operator::Yank);
            operator_key(editor, Operator::Yank);
        }
        '~' => {
            editor.toggle_case(count);
            reset(editor);
            editor.finish_change();
        }
        'J' => {
            if let Some((start, end, _)) = editor.selection() {
                editor.mode = Mode::Normal;
                editor.cursor = Position::new(start.line, editor.cursor.col);
                let span_lines = end.line - start.line + 1;
                editor.join_lines(span_lines.max(2));
            } else {
                editor.join_lines(count);
            }
            reset(editor);
            editor.finish_change();
        }
        'p' => {
            editor.put(true, count);
            reset(editor);
            editor.finish_change();
        }
        'P' => {
            editor.put(false, count);
            reset(editor);
            editor.finish_change();
        }
        _ => return false,
    }
    true
}

/// The session commands: undo/redo, repeat, search jumps, visual-mode
/// toggles, and the command-line openers. The final stop: an unbound key
/// abandons whatever was pending, silently (vim beeps; there is no bell
/// on this console path).
fn session_key(editor: &mut Editor, ch: char, io: &dyn FileIo) {
    match ch {
        'u' => {
            match editor.buffer.undo() {
                Some(pos) => {
                    editor.cursor = pos;
                    editor.clamp_cursor();
                }
                None => editor.info(String::from("Already at oldest change")),
            }
            reset(editor);
            editor.finish_command();
        }
        '.' => {
            editor.repeat_dot(io);
            reset(editor);
            editor.finish_command();
        }
        'n' => {
            editor.search_next(false);
            reset(editor);
            editor.finish_command();
        }
        'N' => {
            editor.search_next(true);
            reset(editor);
            editor.finish_command();
        }
        '*' => {
            editor.search_word_under_cursor();
            reset(editor);
            editor.finish_command();
        }
        'v' => {
            editor.mode = if matches!(editor.mode, Mode::Visual { linewise: false }) {
                Mode::Normal
            } else {
                editor.visual_anchor = editor.cursor;
                Mode::Visual { linewise: false }
            };
            reset(editor);
        }
        'V' => {
            editor.mode = if matches!(editor.mode, Mode::Visual { linewise: true }) {
                Mode::Normal
            } else {
                editor.visual_anchor = editor.cursor;
                Mode::Visual { linewise: true }
            };
            reset(editor);
        }
        ':' | '/' | '?' => {
            editor.open_cmdline(ch);
            reset(editor);
            editor.finish_command();
        }
        _ => {
            reset(editor);
            editor.finish_command();
        }
    }
}

/// `o` / `O`: open a line below/above and enter insert mode.
fn open_line(editor: &mut Editor, below: bool) {
    editor.buffer.begin_edit(editor.cursor);
    let at = if below {
        editor.cursor.line + 1
    } else {
        editor.cursor.line
    };
    editor
        .buffer
        .replace_lines(at, at, alloc::vec![String::new()]);
    editor.cursor = Position::new(at, 0);
    reset(editor);
    editor.enter_insert(false);
}
