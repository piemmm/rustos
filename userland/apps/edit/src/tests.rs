//! Unit tests for the `edit` tool: the command parser, the text buffer,
//! the model's editing/menu/prompt/confirm state machine (against an
//! in-memory filesystem), and the renderer (against an in-memory tty).

extern crate std;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::collections::BTreeMap;

use tairix_abi::Errno;
use tairix_curses::{Event, Screen, Size};
use tairix_termcap::TermType;

use crate::buffer::{width_of_prefix, DecodeError, TextBuffer, MAX_FILE_BYTES};
use crate::command::{parse, Command};
use crate::error::EditError;
use crate::model::{Action, Fs, Mode, Model, Pending, PromptIntent};

// ---- Test seams -------------------------------------------------------

/// An in-memory [`Fs`]: a path → bytes map, with an optional blanket
/// write refusal to exercise the failure paths.
struct MapFs {
    files: RefCell<BTreeMap<String, Vec<u8>>>,
    deny_writes: Option<Errno>,
}

impl MapFs {
    fn new() -> Self {
        Self {
            files: RefCell::new(BTreeMap::new()),
            deny_writes: None,
        }
    }

    fn with(path: &str, bytes: &[u8]) -> Self {
        let fs = Self::new();
        fs.files
            .borrow_mut()
            .insert(path.to_owned(), bytes.to_vec());
        fs
    }

    fn denying_writes(errno: Errno) -> Self {
        Self {
            files: RefCell::new(BTreeMap::new()),
            deny_writes: Some(errno),
        }
    }

    fn contents(&self, path: &str) -> Option<Vec<u8>> {
        self.files.borrow().get(path).cloned()
    }
}

impl Fs for MapFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, Errno> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or(Errno::NotFound)
    }

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(errno) = self.deny_writes {
            return Err(errno);
        }
        self.files
            .borrow_mut()
            .insert(path.to_owned(), bytes.to_vec());
        Ok(())
    }
}

/// A tty capturing rendered bytes (input is unused by the render tests).
struct FakeTty {
    output: Vec<u8>,
}

impl FakeTty {
    fn new() -> Self {
        Self { output: Vec::new() }
    }
}

impl tairix_curses::Tty for FakeTty {
    fn write(&mut self, bytes: &[u8]) -> tairix_curses::Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self) -> tairix_curses::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

/// Feed `keys` through the model against `fs`, returning the last action.
fn feed(model: &mut Model, fs: &dyn Fs, keys: &[Event]) -> Action {
    let mut last = Action::Continue;
    for key in keys {
        last = model.handle_event(key, fs);
    }
    last
}

/// Type `text` as character events.
fn type_str(model: &mut Model, fs: &dyn Fs, text: &str) {
    for ch in text.chars() {
        model.handle_event(&Event::Char(ch), fs);
    }
}

/// Whether `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---- Command parsing ---------------------------------------------------

#[test]
fn parse_accepts_no_operand_one_operand_and_help() {
    assert_eq!(parse(&[]), Ok(Command::Run { path: None }));
    assert_eq!(
        parse(&["notes.txt"]),
        Ok(Command::Run {
            path: Some(String::from("notes.txt"))
        })
    );
    assert_eq!(parse(&["-h"]), Ok(Command::Help));
    assert_eq!(parse(&["-?"]), Ok(Command::Help));
    assert_eq!(parse(&["--help"]), Ok(Command::Help));
}

#[test]
fn parse_refuses_options_and_extra_operands() {
    assert_eq!(parse(&["-x"]), Err(EditError::Usage));
    assert_eq!(parse(&["--frobnicate"]), Err(EditError::Usage));
    assert_eq!(parse(&["a.txt", "b.txt"]), Err(EditError::Usage));
}

#[test]
fn double_dash_makes_a_dashed_operand_a_file() {
    assert_eq!(
        parse(&["--", "-h"]),
        Ok(Command::Run {
            path: Some(String::from("-h"))
        })
    );
}

// ---- Text buffer -------------------------------------------------------

#[test]
fn plain_text_round_trips_byte_for_byte() {
    for bytes in [
        b"".as_slice(),
        b"\n",
        b"abc",
        b"abc\n",
        b"one\ntwo\n",
        b"one\n\nthree",
    ] {
        let (buffer, notices) = TextBuffer::from_bytes(bytes).expect("decodes");
        assert_eq!(notices, crate::buffer::LoadNotices::default());
        assert_eq!(buffer.to_bytes(), bytes, "round trip of {bytes:?}");
        assert!(!buffer.is_modified());
    }
}

#[test]
fn crlf_is_converted_and_reported() {
    let (buffer, notices) = TextBuffer::from_bytes(b"one\r\ntwo\r\n").expect("decodes");
    assert!(notices.crlf_converted);
    assert!(!notices.tabs_expanded);
    assert_eq!(buffer.to_bytes(), b"one\ntwo\n");
}

#[test]
fn tabs_expand_to_eight_column_stops_and_are_reported() {
    let (buffer, notices) = TextBuffer::from_bytes(b"a\tb\n\tc\n").expect("decodes");
    assert!(notices.tabs_expanded);
    assert_eq!(buffer.line(0), "a       b");
    assert_eq!(buffer.line(1), "        c");
}

#[test]
fn tab_stops_measure_display_width_for_wide_glyphs() {
    // "你" is two columns wide, so the tab pads six columns to the stop.
    let (buffer, _) = TextBuffer::from_bytes("你\tx\n".as_bytes()).expect("decodes");
    assert_eq!(buffer.line(0), "你      x");
    assert_eq!(width_of_prefix(buffer.line(0), 7), 8);
}

#[test]
fn non_text_input_is_refused() {
    // Invalid UTF-8, NUL, a lone CR, and an escape byte all fail closed.
    for bytes in [
        b"\xff\xfe".as_slice(),
        b"a\0b",
        b"mac\rline",
        b"esc\x1b[31m",
    ] {
        assert_eq!(
            TextBuffer::from_bytes(bytes),
            Err(DecodeError::NotText),
            "{bytes:?} must be refused"
        );
    }
}

#[test]
fn an_over_large_file_is_refused() {
    let bytes = alloc::vec![b'a'; MAX_FILE_BYTES + 1];
    assert_eq!(TextBuffer::from_bytes(&bytes), Err(DecodeError::TooLarge));
}

#[test]
fn editing_primitives_insert_overwrite_split_and_delete() {
    let (mut buffer, _) = TextBuffer::from_bytes(b"abd\n").expect("decodes");
    buffer.insert_char(0, 2, 'c', false);
    assert_eq!(buffer.line(0), "abcd");
    buffer.insert_char(0, 3, 'X', true);
    assert_eq!(buffer.line(0), "abcX");
    assert!(buffer.is_modified());

    buffer.split_line(0, 2);
    assert_eq!((buffer.line(0), buffer.line(1)), ("ab", "cX"));

    // Delete at a line end joins the next line back on.
    assert!(buffer.delete_at(0, 2));
    assert_eq!(buffer.line(0), "abcX");
    assert!(buffer.delete_at(0, 0));
    assert_eq!(buffer.line(0), "bcX");
    // Nothing after the last position: refused, not a panic.
    assert!(!buffer.delete_at(0, 3));
}

// ---- Model: editing ----------------------------------------------------

#[test]
fn typing_inserts_and_moves_the_cursor() {
    let fs = MapFs::new();
    let mut model = Model::new();
    type_str(&mut model, &fs, "hi");
    assert_eq!(model.buffer().line(0), "hi");
    assert_eq!((model.cursor_row(), model.cursor_col()), (0, 2));
    assert!(model.buffer().is_modified());
}

#[test]
fn enter_splits_and_backspace_joins() {
    let fs = MapFs::new();
    let mut model = Model::new();
    type_str(&mut model, &fs, "ab");
    feed(&mut model, &fs, &[Event::Left, Event::Enter]);
    assert_eq!((model.buffer().line(0), model.buffer().line(1)), ("a", "b"));
    assert_eq!((model.cursor_row(), model.cursor_col()), (1, 0));
    feed(&mut model, &fs, &[Event::Backspace]);
    assert_eq!(model.buffer().line(0), "ab");
    assert_eq!((model.cursor_row(), model.cursor_col()), (0, 1));
}

#[test]
fn the_insert_key_toggles_overwrite() {
    let fs = MapFs::new();
    let mut model = Model::new();
    type_str(&mut model, &fs, "abc");
    assert!(!model.overwrite());
    feed(&mut model, &fs, &[Event::Insert, Event::Home]);
    assert!(model.overwrite());
    type_str(&mut model, &fs, "X");
    assert_eq!(model.buffer().line(0), "Xbc");
}

#[test]
fn the_tab_key_inserts_spaces_to_the_next_stop() {
    let fs = MapFs::new();
    let mut model = Model::new();
    type_str(&mut model, &fs, "ab");
    feed(&mut model, &fs, &[Event::Tab]);
    assert_eq!(model.buffer().line(0), "ab      ");
    assert_eq!(model.cursor_col(), 8);
}

#[test]
fn vertical_moves_keep_the_sticky_column() {
    let fs = MapFs::with("f", b"a long first line\nx\nanother long line\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    feed(&mut model, &fs, &[Event::End]);
    let want = model.cursor_col();
    feed(&mut model, &fs, &[Event::Down]);
    assert_eq!((model.cursor_row(), model.cursor_col()), (1, 1));
    feed(&mut model, &fs, &[Event::Down]);
    // The remembered column is re-applied on the longer line below.
    assert_eq!(model.cursor_row(), 2);
    assert_eq!(model.cursor_col(), want.min(model.buffer().line_chars(2)));
}

#[test]
fn arrows_clamp_at_the_buffer_edges() {
    let fs = MapFs::new();
    let mut model = Model::new();
    feed(
        &mut model,
        &fs,
        &[
            Event::Up,
            Event::Left,
            Event::Down,
            Event::Right,
            Event::PageUp,
            Event::PageDown,
        ],
    );
    assert_eq!((model.cursor_row(), model.cursor_col()), (0, 0));
}

#[test]
fn the_view_scrolls_to_follow_the_cursor() {
    let fs = MapFs::with("f", b"0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    model.set_viewport(3, 10);
    feed(&mut model, &fs, &[Event::PageDown, Event::PageDown]);
    model.set_viewport(3, 10);
    assert!(model.cursor_row() >= model.scroll_top());
    assert!(model.cursor_row() < model.scroll_top() + 3);
}

// ---- Model: files ------------------------------------------------------

#[test]
fn open_initial_loads_and_saving_writes_the_exact_bytes() {
    let fs = MapFs::with("notes.txt", b"one\ntwo\n");
    let mut model = Model::new();
    model.open_initial(&fs, "notes.txt").expect("loads");
    assert_eq!(model.path(), Some("notes.txt"));
    type_str(&mut model, &fs, "X");
    assert_eq!(
        feed(&mut model, &fs, &[Event::Function(2)]),
        Action::Continue
    );
    assert_eq!(
        fs.contents("notes.txt").as_deref(),
        Some(b"Xone\ntwo\n".as_slice())
    );
    assert!(!model.buffer().is_modified());
    assert_eq!(model.notice(), Some("Saved notes.txt"));
}

#[test]
fn open_initial_on_a_missing_file_starts_a_named_new_buffer() {
    let fs = MapFs::new();
    let mut model = Model::new();
    model
        .open_initial(&fs, "new.txt")
        .expect("a new file is fine");
    assert_eq!(model.path(), Some("new.txt"));
    assert_eq!(model.notice(), Some("New file"));
}

#[test]
fn open_initial_fails_loudly_on_a_non_text_file() {
    let fs = MapFs::with("blob", b"\xff\xfe\x00");
    let mut model = Model::new();
    let err = model.open_initial(&fs, "blob").expect_err("must refuse");
    assert!(err.contains("not a text file"));
}

#[test]
fn saving_an_unnamed_buffer_asks_for_a_name_first() {
    let fs = MapFs::new();
    let mut model = Model::new();
    type_str(&mut model, &fs, "hello");
    feed(&mut model, &fs, &[Event::Function(2)]);
    assert!(matches!(
        model.mode(),
        Mode::Prompt {
            intent: PromptIntent::SaveAs,
            ..
        }
    ));
    type_str(&mut model, &fs, "out.txt");
    feed(&mut model, &fs, &[Event::Enter]);
    assert_eq!(
        fs.contents("out.txt").as_deref(),
        Some(b"hello\n".as_slice())
    );
    assert_eq!(model.path(), Some("out.txt"));
}

#[test]
fn a_refused_save_posts_the_reason_and_keeps_the_buffer_dirty() {
    let fs = MapFs::denying_writes(Errno::PermissionDenied);
    let mut model = Model::new();
    model.open_initial(&fs, "ro.txt").expect("named new buffer");
    type_str(&mut model, &fs, "x");
    feed(&mut model, &fs, &[Event::Function(2)]);
    assert!(model.buffer().is_modified());
    assert!(model.notice().is_some_and(|n| n.starts_with("ro.txt:")));
}

#[test]
fn a_prompt_cancelled_with_f10_changes_nothing() {
    let fs = MapFs::new();
    let mut model = Model::new();
    type_str(&mut model, &fs, "x");
    feed(&mut model, &fs, &[Event::Function(2)]);
    type_str(&mut model, &fs, "victim.txt");
    feed(&mut model, &fs, &[Event::Function(10)]);
    assert!(matches!(model.mode(), Mode::Edit));
    assert!(fs.contents("victim.txt").is_none());
    assert!(model.path().is_none());
}

// ---- Model: menu and confirm -------------------------------------------

#[test]
fn f10_opens_the_menu_and_navigation_wraps() {
    let fs = MapFs::new();
    let mut model = Model::new();
    feed(&mut model, &fs, &[Event::Function(10)]);
    assert_eq!(model.mode(), &Mode::Menu { menu: 0, item: 0 });
    feed(&mut model, &fs, &[Event::Right, Event::Down]);
    assert_eq!(model.mode(), &Mode::Menu { menu: 1, item: 1 });
    feed(&mut model, &fs, &[Event::Down]);
    assert_eq!(model.mode(), &Mode::Menu { menu: 1, item: 0 });
    feed(&mut model, &fs, &[Event::Function(10)]);
    assert_eq!(model.mode(), &Mode::Edit);
}

#[test]
fn alt_accelerators_open_switch_and_toggle_the_menus() {
    let fs = MapFs::new();
    let mut model = Model::new();
    // Alt-F opens File; case does not matter.
    feed(&mut model, &fs, &[Event::Alt('f')]);
    assert_eq!(model.mode(), &Mode::Menu { menu: 0, item: 0 });
    // Alt-S switches straight to Search.
    feed(&mut model, &fs, &[Event::Alt('S')]);
    assert_eq!(model.mode(), &Mode::Menu { menu: 1, item: 0 });
    // The open menu's own accelerator toggles it closed.
    feed(&mut model, &fs, &[Event::Alt('s')]);
    assert_eq!(model.mode(), &Mode::Edit);
    // An Alt chord no menu claims neither opens a menu nor edits text.
    feed(&mut model, &fs, &[Event::Alt('q')]);
    assert_eq!(model.mode(), &Mode::Edit);
    assert_eq!(model.buffer().line(0), "");
}

#[test]
fn escape_closes_the_menu_and_cancels_a_prompt_and_a_confirm() {
    let fs = MapFs::new();
    let mut model = Model::new();
    // Esc closes an open menu without acting.
    feed(&mut model, &fs, &[Event::Alt('f'), Event::Esc]);
    assert_eq!(model.mode(), &Mode::Edit);
    // Esc abandons a Save As prompt: nothing is written, no name is kept.
    type_str(&mut model, &fs, "x");
    feed(&mut model, &fs, &[Event::Function(2)]);
    type_str(&mut model, &fs, "victim.txt");
    feed(&mut model, &fs, &[Event::Esc]);
    assert!(matches!(model.mode(), Mode::Edit));
    assert!(fs.contents("victim.txt").is_none());
    assert!(model.path().is_none());
    // Esc cancels a "save changes?" question, keeping the session open.
    let action = feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Up, Event::Enter, Event::Esc],
    );
    assert_eq!(action, Action::Continue);
    assert!(matches!(model.mode(), Mode::Edit));
}

#[test]
fn exit_with_a_clean_buffer_quits_at_once() {
    let fs = MapFs::new();
    let mut model = Model::new();
    // File > Exit: open the menu, move to the last item, select it.
    let action = feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Up, Event::Enter],
    );
    assert_eq!(action, Action::Quit);
}

#[test]
fn exit_with_unsaved_changes_asks_and_y_saves_then_quits() {
    let fs = MapFs::with("f.txt", b"old\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f.txt").expect("loads");
    type_str(&mut model, &fs, "new ");
    let action = feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Up, Event::Enter],
    );
    assert_eq!(action, Action::Continue);
    assert_eq!(model.mode(), &Mode::Confirm(Pending::Exit));
    let action = feed(&mut model, &fs, &[Event::Char('y')]);
    assert_eq!(action, Action::Quit);
    assert_eq!(
        fs.contents("f.txt").as_deref(),
        Some(b"new old\n".as_slice())
    );
}

#[test]
fn exit_with_unsaved_changes_n_discards_and_quits() {
    let fs = MapFs::with("f.txt", b"old\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f.txt").expect("loads");
    type_str(&mut model, &fs, "new ");
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Up, Event::Enter],
    );
    let action = feed(&mut model, &fs, &[Event::Char('n')]);
    assert_eq!(action, Action::Quit);
    // Nothing was written.
    assert_eq!(fs.contents("f.txt").as_deref(), Some(b"old\n".as_slice()));
}

#[test]
fn a_cancelled_confirm_returns_to_editing_with_the_buffer_intact() {
    let fs = MapFs::with("f.txt", b"old\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f.txt").expect("loads");
    type_str(&mut model, &fs, "new ");
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Up, Event::Enter],
    );
    let action = feed(&mut model, &fs, &[Event::Char('c')]);
    assert_eq!(action, Action::Continue);
    assert_eq!(model.mode(), &Mode::Edit);
    assert!(model.buffer().is_modified());
    // A later exit still asks: the pending action was truly cancelled.
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Up, Event::Enter],
    );
    assert_eq!(model.mode(), &Mode::Confirm(Pending::Exit));
}

#[test]
fn open_via_the_menu_loads_the_named_file() {
    let fs = MapFs::with("other.txt", b"other content\n");
    let mut model = Model::new();
    // File > Open… on a clean buffer prompts straight away.
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Down, Event::Enter],
    );
    assert!(matches!(
        model.mode(),
        Mode::Prompt {
            intent: PromptIntent::Open,
            ..
        }
    ));
    type_str(&mut model, &fs, "other.txt");
    feed(&mut model, &fs, &[Event::Enter]);
    assert_eq!(model.buffer().line(0), "other content");
    assert_eq!(model.path(), Some("other.txt"));
}

#[test]
fn a_refused_open_keeps_the_current_buffer() {
    let fs = MapFs::with("blob", b"\xff\xfe");
    let mut model = Model::new();
    type_str(&mut model, &fs, "keep me");
    // The buffer is dirty, so Open asks first; discard, then name the blob.
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Down, Event::Enter],
    );
    feed(&mut model, &fs, &[Event::Char('n')]);
    type_str(&mut model, &fs, "blob");
    feed(&mut model, &fs, &[Event::Enter]);
    assert_eq!(model.buffer().line(0), "keep me");
    assert!(model
        .notice()
        .is_some_and(|n| n.contains("not a text file")));
}

// ---- Model: find -------------------------------------------------------

#[test]
fn find_moves_to_the_match_and_f3_repeats_with_wrap() {
    let fs = MapFs::with("f", b"alpha\nbeta\nalpha again\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    // Search > Find…
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Right, Event::Enter],
    );
    type_str(&mut model, &fs, "alpha");
    feed(&mut model, &fs, &[Event::Enter]);
    // The cursor starts on the first "alpha", so the search finds the next.
    assert_eq!((model.cursor_row(), model.cursor_col()), (2, 0));
    // Repeat wraps back around to the first.
    feed(&mut model, &fs, &[Event::Function(3)]);
    assert_eq!((model.cursor_row(), model.cursor_col()), (0, 0));
}

#[test]
fn an_unmatched_find_says_so_and_stays_put() {
    let fs = MapFs::with("f", b"nothing here\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    feed(
        &mut model,
        &fs,
        &[Event::Function(10), Event::Right, Event::Enter],
    );
    type_str(&mut model, &fs, "absent");
    feed(&mut model, &fs, &[Event::Enter]);
    assert_eq!(model.notice(), Some("Match not found"));
    assert_eq!((model.cursor_row(), model.cursor_col()), (0, 0));
}

#[test]
fn repeat_find_without_a_previous_search_reports_it() {
    let fs = MapFs::new();
    let mut model = Model::new();
    feed(&mut model, &fs, &[Event::Function(3)]);
    assert_eq!(model.notice(), Some("No previous search"));
}

// ---- Model: help overlay and notices ------------------------------------

#[test]
fn f1_shows_the_key_summary_and_any_key_dismisses_it_unread() {
    let fs = MapFs::new();
    let mut model = Model::new();
    feed(&mut model, &fs, &[Event::Function(1)]);
    assert!(model.help_visible());
    // The dismissing key is consumed, never typed into the buffer.
    feed(&mut model, &fs, &[Event::Char('x')]);
    assert!(!model.help_visible());
    assert_eq!(model.buffer().line(0), "");
}

#[test]
fn a_resize_leaves_the_key_summary_up_and_reaches_no_mode_handler() {
    let fs = MapFs::with("f", b"hello\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    feed(&mut model, &fs, &[Event::Function(1)]);
    assert!(model.help_visible());

    // The terminal changed size; the user did not press a key, so the
    // overlay stays up and nothing is typed or moved.
    feed(&mut model, &fs, &[Event::Resize(Size::new(30, 100))]);
    assert!(model.help_visible());
    assert_eq!(model.buffer().line(0), "hello");
    assert_eq!((model.cursor_row(), model.cursor_col()), (0, 0));
}

#[test]
fn a_resize_re_clamps_the_view_over_the_cursor() {
    let fs = MapFs::with("f", b"a\nb\nc\nd\ne\nf\ng\nh\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    model.set_viewport(8, 40);
    feed(&mut model, &fs, &[Event::End, Event::PageDown]);
    let tall_top = model.scroll_top();

    // The renderer sizes the view from the live screen before each frame,
    // so a shorter terminal pulls the window back over the cursor.
    feed(&mut model, &fs, &[Event::Resize(Size::new(6, 40))]);
    model.set_viewport(2, 40);
    assert!(model.scroll_top() > tall_top);
    assert!(model.cursor_row() >= model.scroll_top());
    assert!(model.cursor_row() < model.scroll_top() + 2);
}

#[test]
fn the_next_keystroke_clears_a_notice() {
    let fs = MapFs::new();
    let mut model = Model::new();
    feed(&mut model, &fs, &[Event::Function(3)]);
    assert!(model.notice().is_some());
    feed(&mut model, &fs, &[Event::Right]);
    assert!(model.notice().is_none());
}

// ---- Rendering ----------------------------------------------------------

#[test]
fn render_draws_the_menu_bar_text_and_status() {
    let fs = MapFs::with("f.txt", b"hello world\n");
    let mut model = Model::new();
    model.open_initial(&fs, "f.txt").expect("loads");
    // Size the view first, as the run loop does before any event, then
    // consume the load notice so the status line shows the position.
    model.set_viewport(8, 40);
    feed(&mut model, &fs, &[Event::Right]);
    let mut screen = Screen::new(FakeTty::new(), TermType::Xterm256Color, Size::new(10, 40));
    crate::app::render(&model, &mut screen).expect("renders");
    let output = &screen.into_tty().output;
    // Each closed title's accelerator letter is drawn in its own rendition,
    // so the title reaches the wire split around an SGR change: the letter,
    // a rendition switch, then the tail.
    assert!(contains(output, b"ile"));
    assert!(contains(output, b"earch"));
    assert!(contains(output, b"hello world"));
    assert!(contains(output, b"f.txt"));
    assert!(contains(output, b"Ln 1, Col 2"));
}

#[test]
fn menu_bar_highlights_the_accelerator_letters_in_red() {
    // Borland-style discoverability: on a colour terminal each menu title's
    // accelerator letter is red on the white bar (`SGR 31` immediately
    // before the letter), distinct from the black-on-white title text.
    let fs = MapFs::new();
    let mut model = Model::new();
    feed(&mut model, &fs, &[Event::Right]);
    let mut screen = Screen::new(FakeTty::new(), TermType::Xterm256Color, Size::new(10, 40));
    crate::app::render(&model, &mut screen).expect("renders");
    let output = &screen.into_tty().output;
    assert!(contains(output, b"31mF"));
    assert!(contains(output, b"31mS"));
}

#[test]
fn render_shows_the_open_menu_and_the_prompt() {
    let fs = MapFs::new();
    let mut model = Model::new();
    let mut screen = Screen::new(FakeTty::new(), TermType::Xterm256Color, Size::new(12, 40));
    feed(&mut model, &fs, &[Event::Function(10)]);
    crate::app::render(&model, &mut screen).expect("renders");
    let output = screen.into_tty().output;
    assert!(contains(&output, b"New"));
    assert!(contains(&output, b"Save As..."));

    // Search > Find… prompt on the status row.
    feed(&mut model, &fs, &[Event::Right, Event::Enter]);
    type_str(&mut model, &fs, "needle");
    let mut screen = Screen::new(FakeTty::new(), TermType::Xterm256Color, Size::new(12, 40));
    crate::app::render(&model, &mut screen).expect("renders");
    assert!(contains(&screen.into_tty().output, b"Find: needle"));
}

#[test]
fn render_survives_a_tiny_screen() {
    let mut model = Model::new();
    let mut screen = Screen::new(FakeTty::new(), TermType::Xterm256Color, Size::new(1, 4));
    model.set_viewport(0, 4);
    crate::app::render(&model, &mut screen).expect("renders without panicking");
}

#[test]
fn a_wide_line_scrolls_horizontally_without_tearing_glyphs() {
    let fs = MapFs::with("f", "你好世界 wide\n".as_bytes());
    let mut model = Model::new();
    model.open_initial(&fs, "f").expect("loads");
    model.set_viewport(3, 6);
    // Walk right past the window edge; the render must stay well-formed.
    for _ in 0..8 {
        feed(&mut model, &fs, &[Event::Right]);
    }
    model.set_viewport(3, 6);
    let mut screen = Screen::new(FakeTty::new(), TermType::Xterm256Color, Size::new(5, 6));
    crate::app::render(&model, &mut screen).expect("renders");
}

// ---- Help documents ------------------------------------------------------

/// Every locale's `OPTIONS` section documents exactly the switches this
/// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
/// language-neutral, so each translated document must carry the same keys
/// as the canonical one. The documents are read from the bundle's own
/// on-disk `Help/` tree — the single source the image builder plants —
/// never a copy embedded in this crate.
#[test]
fn help_documents_the_parser_switches() {
    use alloc::format;
    use std::fs;

    let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
    let locales = tairix_help::REQUIRED_LOCALES;
    for locale in locales {
        let path = format!("{help_root}/{locale}/edit.md");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let switch = "`-h, -?`";
        assert!(
            text.contains(switch),
            "{locale}/edit.md must document {switch}"
        );
    }
}
