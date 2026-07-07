//! Host unit tests for the `vim` editor core.
//!
//! The editor is driven exactly as a terminal drives it: decoded input
//! events into [`Editor::handle_event`] against an in-memory [`FileIo`],
//! then assertions over the buffer, cursor, registers, and messages. The
//! renderer is exercised against a real curses [`Window`] buffer.

extern crate std;

use core::fmt::Write as _;

use std::collections::BTreeMap;
use std::sync::Mutex;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_curses::Event;

use crate::buffer::{Buffer, Position};
use crate::command::{parse, Command, Start};
use crate::editor::{Editor, Mode};
use crate::fileio::FileIo;
use crate::pattern::{Pattern, PatternError};

/// An in-memory [`FileIo`]: a path → bytes map behind a lock.
#[derive(Default)]
struct MemFs {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    deny: Option<Errno>,
}

impl MemFs {
    fn new() -> MemFs {
        MemFs::default()
    }

    fn with_file(path: &str, contents: &str) -> MemFs {
        let fs = MemFs::new();
        fs.put(path, contents);
        fs
    }

    fn put(&self, path: &str, contents: &str) {
        if let Ok(mut files) = self.files.lock() {
            files.insert(String::from(path), Vec::from(contents.as_bytes()));
        }
    }

    fn get(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .ok()?
            .get(path)
            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
    }
}

impl FileIo for MemFs {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>, Errno> {
        if let Some(errno) = self.deny {
            return Err(errno);
        }
        let files = self.files.lock().map_err(|_| Errno::NotImplemented)?;
        Ok(files.get(path).cloned())
    }

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(errno) = self.deny {
            return Err(errno);
        }
        let mut files = self.files.lock().map_err(|_| Errno::NotImplemented)?;
        files.insert(String::from(path), Vec::from(bytes));
        Ok(())
    }
}

/// An editor over `text` with an empty file list.
fn editor_with(text: &str) -> Editor {
    let mut editor = Editor::new(Vec::new(), false);
    editor.buffer = Buffer::from_text(None, text);
    editor
}

/// Feed a string of keys as character events (plus `\x1b` as Escape and
/// `\r` as Enter), the way the decoded stream delivers them.
fn keys(editor: &mut Editor, io: &dyn FileIo, input: &str) {
    for ch in input.chars() {
        let event = match ch {
            '\x1b' => Event::Esc,
            '\r' | '\n' => Event::Enter,
            '\x08' => Event::Backspace,
            '\t' => Event::Tab,
            other => Event::Char(other),
        };
        editor.handle_event(&event, io);
    }
}

/// The buffer's full text.
fn text(editor: &Editor) -> String {
    editor.buffer.to_text()
}

// ---- A smoke check the module compiles against ------------------------

#[test]
fn an_empty_editor_holds_one_empty_line() {
    let editor = editor_with("");
    assert_eq!(editor.buffer.len_lines(), 1);
    assert_eq!(editor.buffer.line(0), "");
    assert_eq!(editor.cursor, Position::new(0, 0));
    assert!(matches!(editor.mode, Mode::Normal));
}

#[test]
fn parse_accepts_the_documented_grammar() {
    assert_eq!(parse(&["vim", "-h"]), Ok(Command::Help));
    assert_eq!(parse(&["vim", "-?"]), Ok(Command::Help));
    assert_eq!(
        parse(&["vim", "-R", "+7", "a.txt", "b.txt"]),
        Ok(Command::Run {
            readonly: true,
            start: Some(Start::Line(7)),
            files: vec![String::from("a.txt"), String::from("b.txt")],
        })
    );
    assert_eq!(
        parse(&["vim", "+/needle", "hay.txt"]),
        Ok(Command::Run {
            readonly: false,
            start: Some(Start::Pattern(String::from("needle"))),
            files: vec![String::from("hay.txt")],
        })
    );
    assert_eq!(
        parse(&["vim", "+", "--", "-R"]),
        Ok(Command::Run {
            readonly: false,
            start: Some(Start::LastLine),
            files: vec![String::from("-R")],
        })
    );
    assert!(parse(&["vim", "-x"]).is_err());
    assert!(parse(&["vim", "+zero"]).is_err());
}

// ---- Buffer and undo ---------------------------------------------------

#[test]
fn buffer_text_round_trips_with_trailing_newline() {
    let buffer = Buffer::from_text(None, "one\ntwo\n");
    assert_eq!(buffer.len_lines(), 2);
    assert_eq!(buffer.line(0), "one");
    assert_eq!(buffer.line(1), "two");
    assert_eq!(buffer.to_text(), "one\ntwo\n");
    // A file without a trailing newline gains one on write, as vim does.
    let bare = Buffer::from_text(None, "one");
    assert_eq!(bare.to_text(), "one\n");
}

#[test]
fn undo_reverts_a_whole_group_and_redo_replays_it() {
    let mut buffer = Buffer::from_text(None, "a\nb\nc\n");
    buffer.begin_edit(Position::new(1, 0));
    buffer.replace_lines(1, 2, vec![String::from("B")]);
    buffer.replace_lines(2, 3, vec![String::from("C")]);
    buffer.commit_edit(Position::new(2, 0));
    assert_eq!(buffer.to_text(), "a\nB\nC\n");
    let at = buffer.undo();
    assert_eq!(at, Some(Position::new(1, 0)));
    assert_eq!(buffer.to_text(), "a\nb\nc\n");
    let at = buffer.redo();
    assert_eq!(at, Some(Position::new(2, 0)));
    assert_eq!(buffer.to_text(), "a\nB\nC\n");
}

#[test]
fn a_fresh_edit_clears_the_redo_stack() {
    let mut buffer = Buffer::from_text(None, "a\n");
    buffer.replace_lines(0, 1, vec![String::from("b")]);
    assert!(buffer.undo().is_some());
    buffer.replace_lines(0, 1, vec![String::from("c")]);
    assert!(buffer.redo().is_none());
    assert_eq!(buffer.to_text(), "c\n");
}

#[test]
fn undo_on_an_empty_history_reports_none() {
    let mut buffer = Buffer::from_text(None, "a\n");
    assert!(buffer.undo().is_none());
    assert!(buffer.redo().is_none());
}

// ---- Motions -----------------------------------------------------------

#[test]
fn word_motions_step_class_runs_and_lines() {
    use crate::motion;
    let buffer = Buffer::from_text(None, "foo bar()\nbaz\n");
    let at = Position::new(0, 0);
    // `w` stops at `bar`, then at `(`, then at `baz` on the next line.
    let w1 = motion::word_forward(&buffer, at, 1, false).pos;
    assert_eq!(w1, Position::new(0, 4));
    let w2 = motion::word_forward(&buffer, w1, 1, false).pos;
    assert_eq!(w2, Position::new(0, 7));
    let w3 = motion::word_forward(&buffer, w2, 1, false).pos;
    assert_eq!(w3, Position::new(1, 0));
    // `W` treats `bar()` as one WORD.
    let big = motion::word_forward(&buffer, w1, 1, true).pos;
    assert_eq!(big, Position::new(1, 0));
    // `e` lands on the last character of the word; `b` returns to starts.
    assert_eq!(
        motion::word_end(&buffer, at, 1, false).pos,
        Position::new(0, 2)
    );
    assert_eq!(
        motion::word_back(&buffer, w2, 1, false).pos,
        Position::new(0, 4)
    );
}

#[test]
fn find_char_and_till_scan_the_line_only() {
    use crate::motion;
    let buffer = Buffer::from_text(None, "abcabc\n");
    let at = Position::new(0, 0);
    let f = motion::find_char(&buffer, at, 'c', 1, true, false);
    assert_eq!(f.map(|t| t.pos), Some(Position::new(0, 2)));
    let second = motion::find_char(&buffer, at, 'c', 2, true, false);
    assert_eq!(second.map(|t| t.pos), Some(Position::new(0, 5)));
    let till = motion::find_char(&buffer, at, 'c', 1, true, true);
    assert_eq!(till.map(|t| t.pos), Some(Position::new(0, 1)));
    assert!(motion::find_char(&buffer, at, 'z', 1, true, false).is_none());
}

#[test]
fn match_pair_balances_nested_brackets_across_lines() {
    use crate::motion;
    let buffer = Buffer::from_text(None, "fn x(a,\n  (b))\n");
    // The outer opener matches the outer closer, across the line break.
    let hit = motion::match_pair(&buffer, Position::new(0, 4));
    assert_eq!(hit.map(|t| t.pos), Some(Position::new(1, 5)));
    // From the outer closer back to the outer opener; the inner closer
    // matches the inner opener.
    let back = motion::match_pair(&buffer, Position::new(1, 5));
    assert_eq!(back.map(|t| t.pos), Some(Position::new(0, 4)));
    let inner = motion::match_pair(&buffer, Position::new(1, 4));
    assert_eq!(inner.map(|t| t.pos), Some(Position::new(1, 2)));
    // `%` scans forward on the line for a bracket first, so from column 0
    // it still finds the `(`; a bracket-free line has nothing to match.
    let plain = Buffer::from_text(None, "no brackets here\n");
    assert!(motion::match_pair(&plain, Position::new(0, 0)).is_none());
}

#[test]
fn text_objects_select_words_pairs_and_quotes() {
    use crate::motion;
    let buffer = Buffer::from_text(None, "say(\"hi there\", x)\n");
    let iw = motion::word_object(&buffer, Position::new(0, 6), false);
    assert_eq!(
        iw,
        Some(crate::motion::ObjectSpan {
            start: Position::new(0, 5),
            end: Position::new(0, 6),
        })
    );
    let inner = motion::pair_object(&buffer, Position::new(0, 8), '(', ')', false);
    assert_eq!(
        inner.map(|s| (s.start, s.end)),
        Some((Position::new(0, 4), Position::new(0, 16)))
    );
    let quoted = motion::quote_object(&buffer, Position::new(0, 8), '"', false);
    assert_eq!(
        quoted.map(|s| (s.start, s.end)),
        Some((Position::new(0, 5), Position::new(0, 12)))
    );
}

#[test]
fn pattern_compile_fails_closed_on_unsupported_syntax() {
    assert_eq!(Pattern::compile(""), Err(PatternError::Empty));
    assert_eq!(Pattern::compile("[ab"), Err(PatternError::UnclosedClass));
    assert_eq!(Pattern::compile("a\\"), Err(PatternError::TrailingEscape));
    assert_eq!(
        Pattern::compile("a\\+"),
        Err(PatternError::UnsupportedEscape('+'))
    );
}

// ---- Normal-mode editing ------------------------------------------------

#[test]
fn x_deletes_under_the_cursor_and_counts_apply() {
    let fs = MemFs::new();
    let mut editor = editor_with("abcdef\n");
    keys(&mut editor, &fs, "x");
    assert_eq!(text(&editor), "bcdef\n");
    keys(&mut editor, &fs, "2x");
    assert_eq!(text(&editor), "def\n");
    // `X` deletes before the cursor.
    keys(&mut editor, &fs, "lX");
    assert_eq!(text(&editor), "ef\n");
}

#[test]
fn dd_deletes_lines_into_the_unnamed_register_and_p_puts_them() {
    let fs = MemFs::new();
    let mut editor = editor_with("one\ntwo\nthree\n");
    keys(&mut editor, &fs, "dd");
    assert_eq!(text(&editor), "two\nthree\n");
    keys(&mut editor, &fs, "p");
    assert_eq!(text(&editor), "two\none\nthree\n");
    assert_eq!(editor.cursor, Position::new(1, 0));
    // `P` puts above.
    keys(&mut editor, &fs, "ggP");
    assert_eq!(text(&editor), "one\ntwo\none\nthree\n");
}

#[test]
fn dw_and_dollar_operators_span_charwise() {
    let fs = MemFs::new();
    let mut editor = editor_with("foo bar baz\n");
    keys(&mut editor, &fs, "dw");
    assert_eq!(text(&editor), "bar baz\n");
    keys(&mut editor, &fs, "wd$");
    assert_eq!(text(&editor), "bar \n");
}

#[test]
fn counts_multiply_across_the_operator() {
    let fs = MemFs::new();
    let mut editor = editor_with("a b c d e f g\n");
    keys(&mut editor, &fs, "2d2w");
    assert_eq!(text(&editor), "e f g\n");
}

#[test]
fn cw_changes_like_ce_and_dot_repeats_the_whole_change() {
    let fs = MemFs::new();
    let mut editor = editor_with("alpha beta gamma\n");
    keys(&mut editor, &fs, "cwX\x1b");
    assert_eq!(text(&editor), "X beta gamma\n");
    // `.` repeats `cwX` on the next word.
    keys(&mut editor, &fs, "ww.");
    assert_eq!(text(&editor), "X beta X\n");
}

#[test]
fn dot_repeats_dd_and_insert_sessions() {
    let fs = MemFs::new();
    let mut editor = editor_with("one\ntwo\nthree\nfour\n");
    keys(&mut editor, &fs, "dd.");
    assert_eq!(text(&editor), "three\nfour\n");
    let mut editor = editor_with("x\n");
    keys(&mut editor, &fs, "ihi \x1b");
    assert_eq!(text(&editor), "hi x\n");
    // The cursor sits on the space after leaving insert, so the replay
    // inserts before it — exactly vim's outcome.
    keys(&mut editor, &fs, ".");
    assert_eq!(text(&editor), "hihi  x\n");
}

#[test]
fn yank_put_and_named_registers_round_trip() {
    let fs = MemFs::new();
    let mut editor = editor_with("one two\n");
    keys(&mut editor, &fs, "\"ayw");
    // The named yank also fills the unnamed register.
    keys(&mut editor, &fs, "$\"ap");
    assert_eq!(text(&editor), "one twoone \n");
    // Uppercase appends to the lowercase register.
    let mut editor = editor_with("ab\ncd\n");
    keys(&mut editor, &fs, "\"qyyj\"Qyygg\"qp");
    assert_eq!(text(&editor), "ab\nab\ncd\ncd\n");
}

#[test]
fn yy_p_are_linewise_and_keep_the_source() {
    let fs = MemFs::new();
    let mut editor = editor_with("one\ntwo\n");
    keys(&mut editor, &fs, "yyp");
    assert_eq!(text(&editor), "one\none\ntwo\n");
    keys(&mut editor, &fs, "2gg2yyggp");
    assert_eq!(text(&editor), "one\none\ntwo\none\ntwo\n");
}

#[test]
fn join_replace_and_toggle_case_edit_in_place() {
    let fs = MemFs::new();
    let mut editor = editor_with("one\n   two\nthree\n");
    keys(&mut editor, &fs, "J");
    assert_eq!(text(&editor), "one two\nthree\n");
    assert_eq!(editor.cursor, Position::new(0, 3));
    keys(&mut editor, &fs, "ggrX");
    assert_eq!(text(&editor), "Xne two\nthree\n");
    keys(&mut editor, &fs, "gg3~");
    assert_eq!(text(&editor), "xNE two\nthree\n");
}

#[test]
fn open_line_above_and_below_enter_insert() {
    let fs = MemFs::new();
    let mut editor = editor_with("mid\n");
    keys(&mut editor, &fs, "obelow\x1b");
    assert_eq!(text(&editor), "mid\nbelow\n");
    keys(&mut editor, &fs, "ggOabove\x1b");
    assert_eq!(text(&editor), "above\nmid\nbelow\n");
}

#[test]
fn undo_and_redo_walk_whole_changes() {
    let fs = MemFs::new();
    let mut editor = editor_with("start\n");
    keys(&mut editor, &fs, "ihello \x1b");
    assert_eq!(text(&editor), "hello start\n");
    keys(&mut editor, &fs, "u");
    assert_eq!(text(&editor), "start\n");
    editor.handle_event(&Event::Ctrl('r'), &fs);
    assert_eq!(text(&editor), "hello start\n");
    keys(&mut editor, &fs, "u");
    keys(&mut editor, &fs, "u");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.text.contains("oldest")));
}

#[test]
fn text_object_operators_delete_inside_and_around() {
    let fs = MemFs::new();
    let mut editor = editor_with("call(arg one, two)\n");
    keys(&mut editor, &fs, "f(di(");
    assert_eq!(text(&editor), "call()\n");
    let mut editor = editor_with("a \"quoted\" b\n");
    keys(&mut editor, &fs, "fqda\"");
    assert_eq!(text(&editor), "a  b\n");
    let mut editor = editor_with("one two three\n");
    keys(&mut editor, &fs, "wdaw");
    assert_eq!(text(&editor), "one three\n");
}

#[test]
fn s_and_cc_change_with_insert() {
    let fs = MemFs::new();
    let mut editor = editor_with("word\n");
    keys(&mut editor, &fs, "2sWO\x1b");
    assert_eq!(text(&editor), "WOrd\n");
    let mut editor = editor_with("  indented line\n");
    keys(&mut editor, &fs, "ccnew\x1b");
    assert_eq!(text(&editor), "new\n");
}

#[test]
fn capital_d_c_y_operate_to_line_end() {
    let fs = MemFs::new();
    let mut editor = editor_with("keep drop\n");
    keys(&mut editor, &fs, "wD");
    assert_eq!(text(&editor), "keep \n");
    let mut editor = editor_with("keep drop\n");
    keys(&mut editor, &fs, "wCnew\x1b");
    assert_eq!(text(&editor), "keep new\n");
    let mut editor = editor_with("line\n");
    keys(&mut editor, &fs, "Yp");
    assert_eq!(text(&editor), "line\nline\n");
}

// ---- Visual mode --------------------------------------------------------

#[test]
fn visual_charwise_delete_and_yank() {
    let fs = MemFs::new();
    let mut editor = editor_with("abcdef\n");
    keys(&mut editor, &fs, "vlld");
    assert_eq!(text(&editor), "def\n");
    let mut editor = editor_with("abc\n");
    keys(&mut editor, &fs, "vly$p");
    assert_eq!(text(&editor), "abcab\n");
}

#[test]
fn visual_linewise_selects_whole_lines() {
    let fs = MemFs::new();
    let mut editor = editor_with("one\ntwo\nthree\n");
    keys(&mut editor, &fs, "Vjd");
    assert_eq!(text(&editor), "three\n");
}

#[test]
fn visual_mode_escape_and_anchor_swap() {
    let fs = MemFs::new();
    let mut editor = editor_with("abcdef\n");
    keys(&mut editor, &fs, "lv");
    assert!(matches!(editor.mode, Mode::Visual { linewise: false }));
    keys(&mut editor, &fs, "llo");
    assert_eq!(editor.cursor, Position::new(0, 1));
    keys(&mut editor, &fs, "\x1b");
    assert!(matches!(editor.mode, Mode::Normal));
}

#[test]
fn visual_object_selection_widens_to_the_object() {
    let fs = MemFs::new();
    let mut editor = editor_with("say(hello)\n");
    keys(&mut editor, &fs, "6lvi(d");
    assert_eq!(text(&editor), "say()\n");
}

// ---- Search --------------------------------------------------------------

/// Run a `/` or `?` search by typing it on the command line.
fn search(editor: &mut Editor, io: &dyn FileIo, prefix: char, pattern: &str) {
    keys(editor, io, &alloc::format!("{prefix}{pattern}\r"));
}

#[test]
fn slash_search_jumps_wraps_and_repeats() {
    let fs = MemFs::new();
    let mut editor = editor_with("alpha\nbeta\nalpha again\n");
    search(&mut editor, &fs, '/', "alpha");
    assert_eq!(editor.cursor, Position::new(2, 0));
    keys(&mut editor, &fs, "n");
    assert_eq!(editor.cursor, Position::new(0, 0));
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.text.contains("TOP")));
    keys(&mut editor, &fs, "N");
    assert_eq!(editor.cursor, Position::new(2, 0));
}

#[test]
fn question_mark_searches_backward() {
    let fs = MemFs::new();
    let mut editor = editor_with("one\ntwo\none\n");
    keys(&mut editor, &fs, "G");
    search(&mut editor, &fs, '?', "one");
    assert_eq!(editor.cursor, Position::new(0, 0));
}

#[test]
fn failed_search_reports_e486_and_keeps_the_cursor() {
    let fs = MemFs::new();
    let mut editor = editor_with("text\n");
    search(&mut editor, &fs, '/', "missing");
    assert_eq!(editor.cursor, Position::new(0, 0));
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.error && m.text.contains("E486")));
}

#[test]
fn star_searches_the_whole_word_under_the_cursor() {
    let fs = MemFs::new();
    let mut editor = editor_with("abc abcd\nabc\n");
    keys(&mut editor, &fs, "*");
    // `abcd` is not a whole-word match; the next `abc` is on line 2.
    assert_eq!(editor.cursor, Position::new(1, 0));
}

#[test]
fn pattern_engine_matches_the_vim_subset() {
    let compiled = Pattern::compile("^a.c*d$").ok();
    assert!(compiled.is_some());
    let pattern =
        compiled.unwrap_or_else(|| Pattern::compile("x").unwrap_or_else(|_| unreachable!()));
    assert_eq!(pattern.find_at("abccd", 0), Some((0, 5)));
    assert_eq!(pattern.find_at("abd", 0), Some((0, 3)));
    assert_eq!(pattern.find_at("xabd", 0), None);
    let class = Pattern::compile("[a-c]x").ok();
    assert!(class
        .as_ref()
        .is_some_and(|p| p.find_at("zbx", 0) == Some((1, 3))));
    let negated = Pattern::compile("[^0-9]").ok();
    assert!(negated
        .as_ref()
        .is_some_and(|p| p.find_at("12a", 0) == Some((2, 3))));
    let word = Pattern::compile("\\<ab\\>").ok();
    assert!(word
        .as_ref()
        .is_some_and(|p| p.find_at("cab ab", 0) == Some((4, 6))));
}

#[test]
fn pathological_nested_stars_fail_closed_within_the_budget() {
    let mut line = String::new();
    for _ in 0..512 {
        line.push('a');
    }
    let pattern = Pattern::compile("a*a*a*a*a*a*b").ok();
    // No match exists; the scan must stop within the budget, not hang.
    assert!(pattern.is_some_and(|p| p.find_at(&line, 0).is_none()));
}

// ---- Ex commands ----------------------------------------------------------

#[test]
fn write_and_quit_commands_drive_the_file_seam() {
    let fs = MemFs::new();
    let mut editor = editor_with("hello\n");
    keys(&mut editor, &fs, ":w out.txt\r");
    assert_eq!(fs.get("out.txt").as_deref(), Some("hello\n"));
    assert!(!editor.buffer.is_modified());
    assert_eq!(editor.buffer.name(), Some("out.txt"));
    keys(&mut editor, &fs, ":q\r");
    assert_eq!(editor.quit, Some(0));
}

#[test]
fn quit_guards_unwritten_changes_until_forced() {
    let fs = MemFs::new();
    let mut editor = editor_with("x\n");
    keys(&mut editor, &fs, "ix\x1b:q\r");
    assert!(editor.quit.is_none());
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.text.contains("E37")));
    keys(&mut editor, &fs, ":q!\r");
    assert_eq!(editor.quit, Some(0));
}

#[test]
fn wq_writes_and_quits_and_x_writes_only_when_dirty() {
    let fs = MemFs::with_file("f.txt", "old\n");
    let mut editor = editor_with("");
    editor.load_file("f.txt", &fs);
    keys(&mut editor, &fs, "inew \x1b:wq\r");
    assert_eq!(fs.get("f.txt").as_deref(), Some("new old\n"));
    assert_eq!(editor.quit, Some(0));
    // `:x` on a clean buffer quits without rewriting.
    let fs = MemFs::with_file("f.txt", "same\n");
    let mut editor = editor_with("");
    editor.load_file("f.txt", &fs);
    fs.put("f.txt", "changed behind the editor\n");
    keys(&mut editor, &fs, ":x\r");
    assert_eq!(
        fs.get("f.txt").as_deref(),
        Some("changed behind the editor\n")
    );
    assert_eq!(editor.quit, Some(0));
}

#[test]
fn edit_read_and_argument_list_commands_load_files() {
    let fs = MemFs::with_file("a.txt", "AAA\n");
    fs.put("b.txt", "BBB\n");
    let mut editor = editor_with("");
    keys(&mut editor, &fs, ":e a.txt\r");
    assert_eq!(text(&editor), "AAA\n");
    keys(&mut editor, &fs, ":r b.txt\r");
    assert_eq!(text(&editor), "AAA\nBBB\n");
    // A missing file is a new file, not an error.
    keys(&mut editor, &fs, ":e! fresh.txt\r");
    assert_eq!(text(&editor), "\n");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.text.contains("New File")));
}

#[test]
fn next_and_previous_walk_the_argument_list() {
    let fs = MemFs::with_file("a.txt", "AAA\n");
    fs.put("b.txt", "BBB\n");
    let mut editor = Editor::new(vec![String::from("a.txt"), String::from("b.txt")], false);
    editor.load_file("a.txt", &fs);
    keys(&mut editor, &fs, ":n\r");
    assert_eq!(text(&editor), "BBB\n");
    keys(&mut editor, &fs, ":n\r");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.text.contains("E165")));
    keys(&mut editor, &fs, ":prev\r");
    assert_eq!(text(&editor), "AAA\n");
}

#[test]
fn goto_line_addresses_and_ranges() {
    let fs = MemFs::new();
    let mut editor = editor_with("1\n2\n3\n4\n5\n");
    keys(&mut editor, &fs, ":4\r");
    assert_eq!(editor.cursor.line, 3);
    keys(&mut editor, &fs, ":$\r");
    assert_eq!(editor.cursor.line, 4);
    keys(&mut editor, &fs, ":.-2\r");
    assert_eq!(editor.cursor.line, 2);
    keys(&mut editor, &fs, ":2,3d\r");
    assert_eq!(text(&editor), "1\n4\n5\n");
}

#[test]
fn substitute_replaces_within_ranges_and_reports() {
    let fs = MemFs::new();
    let mut editor = editor_with("aa bb\ncc aa\naa\n");
    keys(&mut editor, &fs, ":%s/aa/XX/g\r");
    assert_eq!(text(&editor), "XX bb\ncc XX\nXX\n");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.text.contains("3 substitutions on 3 lines")));
    // Without `g` only the first match of each line in range changes; `&`
    // reinserts the match.
    let mut editor = editor_with("x xx\n");
    keys(&mut editor, &fs, ":s/x/[&]/\r");
    assert_eq!(text(&editor), "[x] xx\n");
    // A failed substitute is vim's E486.
    let mut editor = editor_with("abc\n");
    keys(&mut editor, &fs, ":s/zzz/y/\r");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.error && m.text.contains("E486")));
}

#[test]
fn set_number_toggles_and_unknown_commands_report_e492() {
    let fs = MemFs::new();
    let mut editor = editor_with("x\n");
    keys(&mut editor, &fs, ":set nu\r");
    assert!(editor.number);
    keys(&mut editor, &fs, ":set nonumber\r");
    assert!(!editor.number);
    keys(&mut editor, &fs, ":bogus\r");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.error && m.text.contains("E492")));
    keys(&mut editor, &fs, ":set wildmenu\r");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.error && m.text.contains("E518")));
}

#[test]
fn readonly_buffers_refuse_writes_until_forced() {
    let fs = MemFs::with_file("ro.txt", "text\n");
    let mut editor = Editor::new(vec![String::from("ro.txt")], true);
    editor.load_file("ro.txt", &fs);
    keys(&mut editor, &fs, "ix\x1b:w\r");
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.error && m.text.contains("E45")));
    assert_eq!(fs.get("ro.txt").as_deref(), Some("text\n"));
    keys(&mut editor, &fs, ":w!\r");
    assert_eq!(fs.get("ro.txt").as_deref(), Some("xtext\n"));
}

#[test]
fn a_denied_file_read_reports_the_kernel_errno() {
    let fs = MemFs {
        deny: Some(Errno::PermissionDenied),
        ..MemFs::default()
    };
    let mut editor = editor_with("");
    editor.load_file("secret.txt", &fs);
    assert!(editor
        .message
        .as_ref()
        .is_some_and(|m| m.error && m.text.contains("E484")));
    // The session survives with an empty buffer named after the file.
    assert_eq!(editor.buffer.name(), Some("secret.txt"));
}

// ---- Command line editing -------------------------------------------------

#[test]
fn cmdline_editing_backspace_and_escape_cancel() {
    let fs = MemFs::new();
    let mut editor = editor_with("x\n");
    keys(&mut editor, &fs, ":qq");
    editor.handle_event(&Event::Backspace, &fs);
    editor.handle_event(&Event::Backspace, &fs);
    // Rubbing out the prompt cancels command-line mode entirely.
    editor.handle_event(&Event::Backspace, &fs);
    assert!(editor.cmdline.is_none());
    assert!(matches!(editor.mode, Mode::Normal));
    assert!(editor.quit.is_none());
    keys(&mut editor, &fs, ":q");
    editor.handle_event(&Event::Esc, &fs);
    assert!(editor.cmdline.is_none());
    assert!(editor.quit.is_none());
}

#[test]
fn cmdline_paste_never_auto_runs_its_line_breaks() {
    let fs = MemFs::new();
    let mut editor = editor_with("x\n");
    keys(&mut editor, &fs, ":");
    editor.handle_event(&Event::Paste(String::from("q!\nq!\n")), &fs);
    // Still editing: the pasted newlines were content, not Enter.
    assert!(editor.cmdline.is_some());
    assert!(editor.quit.is_none());
}

// ---- Insert mode ------------------------------------------------------------

#[test]
fn insert_mode_types_splits_and_joins_lines() {
    let fs = MemFs::new();
    let mut editor = editor_with("ab\n");
    keys(&mut editor, &fs, "lisplit\rhere\x1b");
    assert_eq!(text(&editor), "asplit\nhereb\n");
    // Backspace at column 0 joins onto the previous line.
    let mut editor = editor_with("one\ntwo\n");
    keys(&mut editor, &fs, "ji\x08\x1b");
    assert_eq!(text(&editor), "onetwo\n");
}

#[test]
fn replace_mode_overwrites_and_extends() {
    let fs = MemFs::new();
    let mut editor = editor_with("abcd\n");
    keys(&mut editor, &fs, "RXY\x1b");
    assert_eq!(text(&editor), "XYcd\n");
    let mut editor = editor_with("ab\n");
    keys(&mut editor, &fs, "RXYZW\x1b");
    assert_eq!(text(&editor), "XYZW\n");
}

#[test]
fn insert_paste_event_inserts_text_with_newlines() {
    let fs = MemFs::new();
    let mut editor = editor_with("\n");
    keys(&mut editor, &fs, "i");
    editor.handle_event(&Event::Paste(String::from("a\nb")), &fs);
    editor.handle_event(&Event::Esc, &fs);
    assert_eq!(text(&editor), "a\nb\n");
}

// ---- Scrolling and view -----------------------------------------------------

#[test]
fn half_page_scrolling_moves_view_and_cursor_together() {
    let fs = MemFs::new();
    let mut lines = String::new();
    for i in 0..100 {
        let _ = writeln!(lines, "line {i}");
    }
    let mut editor = editor_with(&lines);
    editor.view.rows = 20;
    editor.handle_event(&Event::Ctrl('d'), &fs);
    assert_eq!(editor.cursor.line, 10);
    assert_eq!(editor.view.top, 10);
    editor.handle_event(&Event::Ctrl('u'), &fs);
    assert_eq!(editor.cursor.line, 0);
    assert_eq!(editor.view.top, 0);
    editor.handle_event(&Event::Ctrl('f'), &fs);
    assert_eq!(editor.cursor.line, 18);
}

// ---- Renderer ---------------------------------------------------------------

use rustos_curses::{Pos, Size, Window};

/// Render `editor` into a fresh window of the given grid.
fn rendered(editor: &mut Editor, rows: u16, cols: u16) -> Window {
    let mut window = Window::new(Pos::new(0, 0), Size::new(rows, cols));
    crate::render::render(editor, &mut window);
    window
}

/// The text of one window row, trailing blanks trimmed.
fn row_text(window: &Window, row: u16) -> String {
    let mut text = String::new();
    if let Some(cells) = window.buffer().row(row) {
        for cell in cells {
            text.push(cell.ch);
        }
    }
    String::from(text.trim_end())
}

#[test]
fn renderer_draws_text_tildes_status_and_cursor() {
    let mut editor = editor_with("hello\nworld\n");
    let window = rendered(&mut editor, 6, 20);
    assert_eq!(row_text(&window, 0), "hello");
    assert_eq!(row_text(&window, 1), "world");
    // Rows past the end of the buffer show vim's `~` filler.
    assert_eq!(row_text(&window, 2), "~");
    assert_eq!(row_text(&window, 3), "~");
    // The status line names the (unnamed) buffer and the position.
    let status = row_text(&window, 4);
    assert!(status.contains("[No Name]"), "status was {status:?}");
    assert!(status.contains("1,1"), "status was {status:?}");
    // The terminal cursor sits on the text cell under the editor cursor.
    assert_eq!(window.cursor(), Pos::new(0, 0));
}

#[test]
fn renderer_shows_the_number_gutter_and_modified_flag() {
    let fs = MemFs::new();
    let mut editor = editor_with("alpha\nbeta\n");
    keys(&mut editor, &fs, ":set nu\r");
    keys(&mut editor, &fs, "ix\x1b");
    let window = rendered(&mut editor, 5, 20);
    assert_eq!(row_text(&window, 0), "1 xalpha");
    assert_eq!(row_text(&window, 1), "2 beta");
    let status = row_text(&window, 3);
    assert!(status.contains("[+]"), "status was {status:?}");
}

#[test]
fn renderer_reverses_the_visual_selection_and_underlines_matches() {
    let fs = MemFs::new();
    let mut editor = editor_with("abcdef\n");
    keys(&mut editor, &fs, "vll");
    let window = rendered(&mut editor, 4, 16);
    let selected = window.buffer().get(Pos::new(0, 1));
    assert!(selected.is_some_and(|cell| cell.attrs.reverse));
    let outside = window.buffer().get(Pos::new(0, 4));
    assert!(outside.is_some_and(|cell| !cell.attrs.reverse));
    assert_eq!(row_text(&window, 3), "-- VISUAL --");
    // Search highlighting underlines every match while hlsearch is lit.
    let mut editor = editor_with("aba aba\n");
    search(&mut editor, &fs, '/', "aba");
    let window = rendered(&mut editor, 4, 12);
    let hit = window.buffer().get(Pos::new(0, 0));
    assert!(hit.is_some_and(|cell| cell.attrs.underline));
    keys(&mut editor, &fs, ":noh\r");
    let window = rendered(&mut editor, 4, 12);
    let cleared = window.buffer().get(Pos::new(0, 0));
    assert!(cleared.is_some_and(|cell| !cell.attrs.underline));
}

#[test]
fn renderer_puts_the_cursor_on_the_command_line_while_typing_one() {
    let fs = MemFs::new();
    let mut editor = editor_with("x\n");
    keys(&mut editor, &fs, ":wq");
    let window = rendered(&mut editor, 5, 20);
    assert_eq!(row_text(&window, 4), ":wq");
    assert_eq!(window.cursor(), Pos::new(4, 3));
}

#[test]
fn renderer_scrolls_vertically_and_horizontally_to_the_cursor() {
    let fs = MemFs::new();
    let mut lines = String::new();
    for i in 0..50 {
        let _ = writeln!(lines, "row {i} with plenty of text after it");
    }
    let mut editor = editor_with(&lines);
    keys(&mut editor, &fs, "30gg");
    let window = rendered(&mut editor, 10, 20);
    // The cursor line is visible: the view followed it down.
    assert!(editor.view.top <= 29 && 29 < editor.view.top + 8);
    assert!(row_text(&window, 0).starts_with(&alloc::format!("row {}", editor.view.top)));
    // A long-line `$` side-scrolls the view.
    keys(&mut editor, &fs, "$");
    let _ = rendered(&mut editor, 10, 20);
    assert!(editor.view.left > 0);
}

#[test]
fn renderer_expands_tabs_to_the_fixed_stops() {
    let mut editor = editor_with("\tx\n");
    let window = rendered(&mut editor, 4, 20);
    assert_eq!(row_text(&window, 0), "        x");
}
