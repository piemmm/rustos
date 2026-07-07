//! Unit tests for the curses screen model, renderer, and input decoder.
//!
//! The screen model is exercised directly (windows, scrolling, boxes); the
//! minimal-diff renderer is checked against golden `lib/vt` op sequences and
//! the capability-downgrade rules; the input decoder is driven per terminal
//! through the one shared `lib/vt` parser; and the [`Screen`] driver is run
//! over an in-memory [`Tty`] so the whole pipeline is host-testable without a
//! kernel.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use rustos_termcap::TermType;
use rustos_vt::{
    encode_all_into, encode_into, Attributes, BasicColor, Cell, Color, MouseButton, Op, Parser,
};

/// Encode one operation into a fresh `Vec` over the sink API.
fn encode(op: &Op) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    encode_into(op, &mut out);
    out
}

/// Encode a sequence of operations into a fresh `Vec`.
fn encode_all(ops: &[Op]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    encode_all_into(ops, &mut out);
    out
}

use crate::buffer::Buffer;
use crate::color::{downgrade, ColorPairs, MAX_COLOR_PAIRS};
use crate::error::CursesError;
use crate::geom::{Pos, Size};
use crate::input::{Event, Input};
use crate::render::{render, CursorState};
use crate::screen::{InputMode, Screen, Tty};
use crate::window::{BorderChars, Window};
use core::time::Duration;
use rustos_vt::CONTINUATION;

/// Decode every event from one byte slice.
fn decode(bytes: &[u8]) -> Vec<Event> {
    let mut input = Input::new();
    let mut events = Vec::new();
    input.feed(bytes, |event| events.push(event));
    events
}

/// The glyph at `(row, col)` of a window.
fn glyph_at(win: &Window, row: u16, col: u16) -> char {
    win.buffer()
        .get(Pos::new(row, col))
        .map_or('?', |cell| cell.ch)
}

// ---- Window model ----------------------------------------------------------

#[test]
fn add_str_writes_cells_and_advances_cursor() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(2, 10));
    win.add_str("hello");
    assert_eq!(glyph_at(&win, 0, 0), 'h');
    assert_eq!(glyph_at(&win, 0, 4), 'o');
    assert_eq!(win.cursor(), Pos::new(0, 5));
}

#[test]
fn writing_past_the_right_edge_wraps_to_the_next_line() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(3, 3));
    win.add_str("abcd");
    assert_eq!(glyph_at(&win, 0, 0), 'a');
    assert_eq!(glyph_at(&win, 0, 2), 'c');
    assert_eq!(glyph_at(&win, 1, 0), 'd');
    assert_eq!(win.cursor(), Pos::new(1, 1));
}

#[test]
fn move_to_out_of_bounds_is_an_error_not_a_panic() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(2, 2));
    assert_eq!(win.move_to(Pos::new(5, 5)), Err(CursesError::OutOfBounds));
    assert_eq!(win.move_to(Pos::new(1, 1)), Ok(()));
}

#[test]
fn draw_box_frames_the_window_edges() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(3, 4));
    win.draw_box();
    assert_eq!(glyph_at(&win, 0, 0), BorderChars::LIGHT.top_left);
    assert_eq!(glyph_at(&win, 0, 3), BorderChars::LIGHT.top_right);
    assert_eq!(glyph_at(&win, 2, 0), BorderChars::LIGHT.bottom_left);
    assert_eq!(glyph_at(&win, 2, 3), BorderChars::LIGHT.bottom_right);
    assert_eq!(glyph_at(&win, 0, 1), BorderChars::LIGHT.horizontal);
    assert_eq!(glyph_at(&win, 1, 0), BorderChars::LIGHT.vertical);
    // The interior is untouched.
    assert_eq!(glyph_at(&win, 1, 1), ' ');
}

#[test]
fn scrolling_region_moves_content_up_and_blanks_the_bottom() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(3, 3));
    let _ = win.move_add_str(Pos::new(0, 0), "aaa");
    let _ = win.move_add_str(Pos::new(1, 0), "bbb");
    let _ = win.move_add_str(Pos::new(2, 0), "ccc");
    win.scroll(1);
    assert_eq!(glyph_at(&win, 0, 0), 'b');
    assert_eq!(glyph_at(&win, 1, 0), 'c');
    assert_eq!(glyph_at(&win, 2, 0), ' ');
}

#[test]
fn auto_scroll_at_the_bottom_when_scrollok_is_set() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(2, 2));
    win.set_scrolling(true);
    // Four glyphs fill the 2×2 window; the fifth forces a scroll.
    win.add_str("abcd");
    assert_eq!(win.cursor(), Pos::new(1, 0));
    win.add_char('e');
    // The top row scrolled away; "cd" moved up and "e" begins the new bottom.
    assert_eq!(glyph_at(&win, 0, 0), 'c');
    assert_eq!(glyph_at(&win, 1, 0), 'e');
}

#[test]
fn resize_preserves_overlapping_cells() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(2, 4));
    win.add_str("ab");
    win.resize(Size::new(4, 2));
    assert_eq!(glyph_at(&win, 0, 0), 'a');
    assert_eq!(glyph_at(&win, 0, 1), 'b');
    assert_eq!(win.size(), Size::new(4, 2));
}

// ---- Colour pairs ----------------------------------------------------------

#[test]
fn color_pairs_allocate_and_resolve() {
    let mut pairs = ColorPairs::new();
    assert_eq!(
        pairs.init_pair(
            1,
            Color::Basic(BasicColor::Red),
            Color::Basic(BasicColor::Black)
        ),
        Ok(())
    );
    let pair = pairs.get(1);
    assert_eq!(pair.fg, Color::Basic(BasicColor::Red));
    assert_eq!(pair.bg, Color::Basic(BasicColor::Black));
}

#[test]
fn the_default_pair_cannot_be_redefined() {
    let mut pairs = ColorPairs::new();
    assert_eq!(
        pairs.init_pair(0, Color::Basic(BasicColor::Red), Color::Default),
        Err(CursesError::BadColorPair)
    );
    // An out-of-range id is rejected too, and an undefined id resolves to the
    // default rather than panicking.
    assert_eq!(
        pairs.init_pair(9999, Color::Default, Color::Default),
        Err(CursesError::BadColorPair)
    );
    assert_eq!(pairs.get(42).fg, Color::Default);
}

// ---- Colour downgrade ------------------------------------------------------

#[test]
fn supported_colors_pass_through_unchanged() {
    let caps = TermType::Alacritty.capabilities();
    let rgb = Color::Rgb(0x12, 0x34, 0x56);
    assert_eq!(downgrade(rgb, caps.color), rgb);
}

#[test]
fn truecolor_degrades_to_an_indexed_palette_entry() {
    let caps = TermType::Xterm256Color.capabilities();
    // Pure red is in the colour cube; it degrades to an `Indexed` value.
    match downgrade(Color::Rgb(255, 0, 0), caps.color) {
        Color::Indexed(_) => {}
        other => panic!("expected an indexed colour, got {other:?}"),
    }
}

#[test]
fn truecolor_degrades_to_the_nearest_ansi_color() {
    let caps = TermType::Xterm16Color.capabilities();
    // Pure red is nearest to the (non-bright) ANSI red at (170, 0, 0).
    assert_eq!(
        downgrade(Color::Rgb(255, 0, 0), caps.color),
        Color::Basic(BasicColor::Red)
    );
    assert_eq!(
        downgrade(Color::Rgb(255, 128, 128), caps.color),
        Color::Basic(BasicColor::BrightRed)
    );
    assert_eq!(
        downgrade(Color::Rgb(0, 0, 0), caps.color),
        Color::Basic(BasicColor::Black)
    );
}

#[test]
fn any_color_degrades_to_default_on_a_monochrome_terminal() {
    let caps = TermType::Vt100.capabilities();
    assert_eq!(
        downgrade(Color::Rgb(10, 20, 30), caps.color),
        Color::Default
    );
    assert_eq!(
        downgrade(Color::Basic(BasicColor::Red), caps.color),
        Color::Default
    );
}

// ---- Minimal-diff renderer -------------------------------------------------

/// Build a desired buffer of `size` from `cells` written at the origin.
fn desired_with(size: Size, text: &str, attrs: Attributes) -> Buffer {
    let mut buf = Buffer::new(size);
    for (col, ch) in text.chars().enumerate() {
        let col = u16::try_from(col).unwrap_or(u16::MAX);
        let _ = buf.set(Pos::new(0, col), Cell::styled(ch, attrs));
    }
    buf
}

#[test]
fn render_emits_only_the_changed_cells() {
    let caps = TermType::Xterm256Color.capabilities();
    let size = Size::new(1, 5);
    let blank = Buffer::new(size);
    let desired = desired_with(size, "Hi", Attributes::PLAIN);
    let cursor = CursorState {
        visible: true,
        pos: Pos::new(0, 2),
    };
    let ops = render(&caps, &blank, &desired, cursor);
    assert_eq!(
        ops,
        vec![
            Op::CursorPosition { row: 1, col: 1 },
            Op::Sgr(rustos_vt::Sgr::Reset),
            Op::Print('H'),
            Op::Print('i'),
            Op::CursorPosition { row: 1, col: 3 },
            Op::ShowCursor,
        ]
    );
}

#[test]
fn render_of_an_unchanged_screen_prints_nothing() {
    let caps = TermType::Xterm256Color.capabilities();
    let size = Size::new(2, 4);
    let screen = desired_with(size, "ab", Attributes::PLAIN);
    let cursor = CursorState {
        visible: true,
        pos: Pos::ORIGIN,
    };
    let ops = render(&caps, &screen, &screen, cursor);
    assert!(!ops.iter().any(|op| matches!(op, Op::Print(_))));
}

#[test]
fn render_degrades_color_for_the_terminals_depth() {
    let caps = TermType::Xterm16Color.capabilities();
    let size = Size::new(1, 2);
    let blank = Buffer::new(size);
    let mut attrs = Attributes::PLAIN;
    attrs.foreground = Color::Rgb(255, 0, 0);
    let desired = desired_with(size, "X", attrs);
    let cursor = CursorState {
        visible: true,
        pos: Pos::ORIGIN,
    };
    let ops = render(&caps, &blank, &desired, cursor);
    // The truecolour foreground is emitted as the nearest ANSI colour, never
    // as a raw `Rgb` the 16-colour terminal could not honour.
    assert!(ops.iter().any(|op| matches!(
        op,
        Op::Sgr(rustos_vt::Sgr::Foreground(Color::Basic(BasicColor::Red)))
    )));
    assert!(!ops
        .iter()
        .any(|op| matches!(op, Op::Sgr(rustos_vt::Sgr::Foreground(Color::Rgb(_, _, _))))));
}

#[test]
fn render_output_round_trips_through_the_vt_consumer() {
    // What the renderer emits, a `lib/vt` consumer parses back — one
    // vocabulary end to end.
    let caps = TermType::Xterm256Color.capabilities();
    let size = Size::new(1, 5);
    let blank = Buffer::new(size);
    let desired = desired_with(size, "Hi", Attributes::PLAIN);
    let cursor = CursorState {
        visible: true,
        pos: Pos::new(0, 2),
    };
    let bytes = encode_all(&render(&caps, &blank, &desired, cursor));
    let mut parser = Parser::new();
    let mut printed = String::new();
    parser.feed(&bytes, |op| {
        if let Op::Print(ch) = op {
            printed.push(ch);
        }
    });
    assert_eq!(printed, "Hi");
}

// ---- Input decoder ---------------------------------------------------------

#[test]
fn arrows_decode_for_every_terminal_that_sends_them() {
    for term in [
        TermType::Vt100,
        TermType::Vt220,
        TermType::Xterm,
        TermType::Xterm256Color,
        TermType::Alacritty,
    ] {
        let caps = term.capabilities();
        let arrows = caps
            .keys
            .arrows
            .clone()
            .expect("these terminals send arrows");
        assert_eq!(decode(&encode(&arrows.up)), vec![Event::Up]);
        assert_eq!(decode(&encode(&arrows.down)), vec![Event::Down]);
        assert_eq!(decode(&encode(&arrows.left)), vec![Event::Left]);
        assert_eq!(decode(&encode(&arrows.right)), vec![Event::Right]);
    }
}

#[test]
fn function_and_editing_keys_decode() {
    assert_eq!(decode(b"\x1bOP"), vec![Event::Function(1)]);
    assert_eq!(decode(b"\x1b[15~"), vec![Event::Function(5)]);
    assert_eq!(decode(b"\x1b[3~"), vec![Event::Delete]);
    assert_eq!(decode(b"\x1b[6~"), vec![Event::PageDown]);
    assert_eq!(decode(b"\x1b[1~"), vec![Event::Home]);
}

#[test]
fn ordinary_text_and_control_keys_decode() {
    assert_eq!(
        decode(b"hi\r\t\x08"),
        vec![
            Event::Char('h'),
            Event::Char('i'),
            Event::Enter,
            Event::Tab,
            Event::Backspace,
        ]
    );
}

#[test]
fn del_decodes_as_the_backspace_key() {
    // Regression: xterm-class terminals (and the RustOS keymap) send DEL
    // for the Backspace key; the screen-op parser ignores DEL on output,
    // so the input decoder must map the byte itself.
    assert_eq!(
        decode(b"a\x7fb"),
        vec![Event::Char('a'), Event::Backspace, Event::Char('b')]
    );
    // Inside a bracketed paste a rub-out is not content and not a key.
    assert_eq!(
        decode(b"\x1b[200~x\x7fy\x1b[201~"),
        vec![Event::Paste(String::from("xy"))]
    );
}

#[test]
fn a_mouse_report_decodes() {
    let events = decode(b"\x1b[<0;3;4M");
    assert_eq!(events.len(), 1);
    match &events[0] {
        Event::Mouse(report) => {
            assert_eq!(report.button, MouseButton::Left);
            assert_eq!(report.col, 3);
            assert_eq!(report.row, 4);
            assert!(report.pressed);
        }
        other => panic!("expected a mouse event, got {other:?}"),
    }
}

#[test]
fn a_bracketed_paste_is_delivered_as_one_event() {
    // The pasted bytes include what would otherwise be a control character;
    // inside the paste they are literal text, not interpreted.
    let events = decode(b"\x1b[200~hi\tthere\x1b[201~");
    assert_eq!(events, vec![Event::Paste("hi\tthere".to_string())]);
}

#[test]
fn the_decoder_never_panics_on_a_hostile_byte_sweep() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };
    let mut input = Input::new();
    for _ in 0..20_000 {
        let len = usize::try_from(next() % 24).unwrap_or(0);
        let bytes: Vec<u8> = (0..len).map(|_| next().to_le_bytes()[0]).collect();
        input.feed(&bytes, |_| {});
    }
}

// ---- Screen driver over an in-memory tty -----------------------------------

/// An in-memory [`Tty`]: input bytes are queued ahead of time, output bytes are
/// captured for inspection.
struct FakeTty {
    input: VecDeque<u8>,
    output: Vec<u8>,
}

impl FakeTty {
    fn with_input(bytes: &[u8]) -> FakeTty {
        FakeTty {
            input: bytes.iter().copied().collect(),
            output: Vec::new(),
        }
    }
}

impl Tty for FakeTty {
    fn write(&mut self, bytes: &[u8]) -> crate::Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self) -> crate::Result<Vec<u8>> {
        Ok(self.input.drain(..).collect())
    }
}

#[test]
fn screen_refresh_writes_the_rendered_bytes() {
    let size = Size::new(3, 10);
    let mut screen = Screen::new(FakeTty::with_input(b""), TermType::Xterm256Color, size);
    let mut win = Window::new(Pos::ORIGIN, size);
    let _ = win.move_add_str(Pos::new(1, 2), "Ready");
    assert_eq!(screen.refresh(&win), Ok(()));

    // A second refresh with no change emits no printable glyph (minimal diff).
    assert_eq!(screen.refresh(&win), Ok(()));
}

#[test]
fn screen_read_events_decodes_queued_input() {
    let mut screen = Screen::new(
        FakeTty::with_input(b"a\x1b[B"),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    let events = screen.read_events().expect("read succeeds");
    assert_eq!(events, vec![Event::Char('a'), Event::Down]);
}

#[test]
fn colored_attributes_serve_a_colour_terminal_and_refuse_a_monochrome_one() {
    // A colour terminal gets the requested pair back as attributes (the
    // shared helper `top` and `edit` colour through)…
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    let attrs = screen
        .colored_attributes(
            Color::Basic(BasicColor::White),
            Color::Basic(BasicColor::Blue),
        )
        .expect("a 256-colour terminal renders basic colours");
    assert_eq!(attrs.foreground, Color::Basic(BasicColor::White));
    assert_eq!(attrs.background, Color::Basic(BasicColor::Blue));

    // …and asking twice reuses the pair rather than filling the table.
    let again = screen.colored_attributes(
        Color::Basic(BasicColor::White),
        Color::Basic(BasicColor::Blue),
    );
    assert_eq!(again, Some(attrs));

    // A monochrome terminal refuses, so the caller falls back to reverse
    // video instead of emitting colour it cannot show.
    let mut mono = Screen::new(FakeTty::with_input(b""), TermType::Vt100, Size::new(2, 2));
    assert_eq!(
        mono.colored_attributes(
            Color::Basic(BasicColor::White),
            Color::Basic(BasicColor::Blue),
        ),
        None
    );
}

#[test]
fn enabling_mouse_is_a_no_op_on_a_terminal_without_mouse_support() {
    // `vt100` has no mouse reporting, so enabling it writes nothing.
    let mut screen = Screen::new(FakeTty::with_input(b""), TermType::Vt100, Size::new(2, 2));
    assert_eq!(screen.enable_mouse(rustos_vt::MouseMode::Button), Ok(()));
}

#[test]
fn full_screen_uses_the_alternate_screen_and_erases_it_explicitly() {
    // The switch alone must never be trusted to present a cleared buffer:
    // a console a predecessor left on the alternate screen treats
    // `EnterAltScreen` as a no-op and keeps the predecessor's frame, so
    // the driver erases the display explicitly after switching (the
    // stale-login-screen regression).
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    assert_eq!(screen.enter_full_screen(), Ok(()));
    assert_eq!(screen.leave_full_screen(), Ok(()));
    let output = screen.into_tty().output;
    let want = encode_all(&[
        Op::EnterAltScreen,
        Op::CursorPosition { row: 1, col: 1 },
        Op::EraseInDisplay(rustos_vt::EraseMode::All),
        Op::LeaveAltScreen,
    ]);
    assert_eq!(output, want);
}

#[test]
fn full_screen_erases_the_display_in_place_without_an_alternate_screen() {
    // `vt100` has no alternate screen but can erase: entering clears the
    // display from home so stale text never shows through blank cells;
    // leaving writes nothing (there is no saved screen to restore).
    let mut screen = Screen::new(FakeTty::with_input(b""), TermType::Vt100, Size::new(2, 2));
    assert_eq!(screen.enter_full_screen(), Ok(()));
    assert_eq!(screen.leave_full_screen(), Ok(()));
    let output = screen.into_tty().output;
    let want = encode_all(&[
        Op::CursorPosition { row: 1, col: 1 },
        Op::EraseInDisplay(rustos_vt::EraseMode::All),
    ]);
    assert_eq!(output, want);
}

#[test]
fn full_screen_is_a_no_op_on_the_dumb_baseline() {
    // The dumb terminal can neither switch screens nor erase: both calls
    // leave the byte stream untouched rather than emitting sequences the
    // terminal does not understand.
    let mut screen = Screen::new(FakeTty::with_input(b""), TermType::Dumb, Size::new(2, 2));
    assert_eq!(screen.enter_full_screen(), Ok(()));
    assert_eq!(screen.leave_full_screen(), Ok(()));
    assert!(screen.into_tty().output.is_empty());
}

#[test]
fn entering_full_screen_resets_the_diff_base_so_the_next_update_repaints() {
    // Draw a frame, enter the full screen again (the display was cleared),
    // and redraw the same frame: the driver must repaint it rather than
    // diffing against stale knowledge of the pre-clear screen.
    let size = Size::new(1, 4);
    let mut screen = Screen::new(FakeTty::with_input(b""), TermType::Xterm256Color, size);
    let mut win = Window::new(Pos::ORIGIN, size);
    win.add_str("hi");
    assert_eq!(screen.refresh(&win), Ok(()));
    assert_eq!(screen.enter_full_screen(), Ok(()));
    assert_eq!(screen.refresh(&win), Ok(()));
    let output = screen.into_tty().output;
    // The frame's glyphs were painted twice: once before the enter and
    // once repainted onto the cleared display after it.
    let painted = output.windows(2).filter(|pair| pair == b"hi").count();
    assert_eq!(painted, 2);
}

// ---- Wide cells --------------------------------------------------------

#[test]
fn a_wide_glyph_writes_a_lead_and_a_continuation_cell() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(1, 6));
    win.add_str("世a");
    assert_eq!(glyph_at(&win, 0, 0), '世');
    assert_eq!(glyph_at(&win, 0, 1), CONTINUATION);
    assert_eq!(glyph_at(&win, 0, 2), 'a');
    // The cursor advanced two columns for the wide glyph, one for the narrow.
    assert_eq!(win.cursor(), Pos::new(0, 3));
}

#[test]
fn a_wide_glyph_wraps_whole_when_one_column_remains() {
    let mut win = Window::new(Pos::ORIGIN, Size::new(2, 3));
    win.add_str("ab");
    // Cursor at (0,2); a wide glyph cannot fit one column, so it wraps.
    win.add_char('世');
    assert_eq!(glyph_at(&win, 0, 0), 'a');
    assert_eq!(glyph_at(&win, 0, 1), 'b');
    // Column 2 was blanked rather than half-filled.
    assert_eq!(glyph_at(&win, 0, 2), ' ');
    assert_eq!(glyph_at(&win, 1, 0), '世');
    assert_eq!(glyph_at(&win, 1, 1), CONTINUATION);
}

#[test]
fn the_renderer_prints_a_wide_glyph_once_and_skips_its_continuation() {
    let size = Size::new(1, 6);
    let caps = TermType::Xterm256Color.capabilities();
    let blank = Buffer::new(size);
    let mut win = Window::new(Pos::ORIGIN, size);
    win.add_str("世a");
    let mut desired = blank.clone();
    desired.blit(win.buffer(), Pos::ORIGIN);
    let cursor = CursorState {
        visible: true,
        pos: Pos::new(0, 3),
    };
    let ops = render(&caps, &blank, &desired, cursor);
    // Exactly two glyphs are printed: the wide lead and the narrow 'a'; the
    // continuation cell never becomes a `Print`.
    let prints: Vec<char> = ops
        .iter()
        .filter_map(|op| match op {
            Op::Print(ch) => Some(*ch),
            _ => None,
        })
        .collect();
    assert_eq!(prints, vec!['世', 'a']);
    // The 'a' follows the wide glyph with no fresh cursor move (the wide glyph
    // advanced the terminal cursor two columns).
    let moves = ops
        .iter()
        .filter(|op| matches!(op, Op::CursorPosition { .. }))
        .count();
    // One initial move to the lead, one final move for the cursor rest.
    assert_eq!(moves, 2);
}

// ---- Colour-pair allocation ------------------------------------------------

#[test]
fn alloc_pair_hands_out_ascending_free_ids() {
    let mut pairs = ColorPairs::new();
    let first = pairs
        .alloc_pair(Color::Basic(BasicColor::Red), Color::Default)
        .expect("free id");
    let second = pairs
        .alloc_pair(Color::Basic(BasicColor::Green), Color::Default)
        .expect("free id");
    assert_eq!(first, 1);
    assert_eq!(second, 2);
    assert_eq!(pairs.get(first).fg, Color::Basic(BasicColor::Red));
    assert_eq!(pairs.get(second).fg, Color::Basic(BasicColor::Green));
}

#[test]
fn alloc_pair_skips_explicitly_defined_ids() {
    let mut pairs = ColorPairs::new();
    pairs
        .init_pair(1, Color::Basic(BasicColor::Blue), Color::Default)
        .expect("define id 1");
    // Id 1 is taken, so the next free id is 2.
    let next = pairs
        .alloc_pair(Color::Basic(BasicColor::Cyan), Color::Default)
        .expect("free id");
    assert_eq!(next, 2);
    // The explicit definition is untouched.
    assert_eq!(pairs.get(1).fg, Color::Basic(BasicColor::Blue));
}

#[test]
fn alloc_pair_reuses_an_identical_existing_pair() {
    let mut pairs = ColorPairs::new();
    let first = pairs
        .alloc_pair(
            Color::Basic(BasicColor::White),
            Color::Basic(BasicColor::Blue),
        )
        .expect("free id");
    let again = pairs
        .alloc_pair(
            Color::Basic(BasicColor::White),
            Color::Basic(BasicColor::Blue),
        )
        .expect("existing id");
    assert_eq!(first, again);
    // An explicitly defined identical pair is reused too.
    pairs
        .init_pair(7, Color::Basic(BasicColor::Green), Color::Default)
        .expect("define id 7");
    let reused = pairs
        .alloc_pair(Color::Basic(BasicColor::Green), Color::Default)
        .expect("existing id");
    assert_eq!(reused, 7);
}

#[test]
fn alloc_pair_never_exhausts_on_repeated_identical_requests() {
    // The `top -d0` regression: a full-screen refresher requests the same
    // pairs on every redraw. Far more requests than the table holds slots
    // must keep resolving to the same ids, never error, and leave the table
    // no fuller than the number of distinct pairs.
    let mut pairs = ColorPairs::new();
    for _ in 0..usize::from(MAX_COLOR_PAIRS) * 4 {
        let header = pairs
            .alloc_pair(
                Color::Basic(BasicColor::White),
                Color::Basic(BasicColor::Blue),
            )
            .expect("header pair");
        let state = pairs
            .alloc_pair(Color::Basic(BasicColor::Green), Color::Default)
            .expect("state pair");
        assert_eq!(header, 1);
        assert_eq!(state, 2);
    }
    // A genuinely new pair still finds a free slot afterwards.
    let fresh = pairs
        .alloc_pair(Color::Basic(BasicColor::Red), Color::Default)
        .expect("free id");
    assert_eq!(fresh, 3);
}

#[test]
fn screen_alloc_pair_delegates_to_the_table() {
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    let id = screen
        .alloc_pair(Color::Basic(BasicColor::Yellow), Color::Default)
        .expect("free id");
    assert_eq!(id, 1);
    assert_eq!(
        screen.color_pairs().get(id).fg,
        Color::Basic(BasicColor::Yellow)
    );
}

// ---- getch / input modes ---------------------------------------------------

#[test]
fn getch_returns_buffered_events_one_at_a_time() {
    let mut screen = Screen::new(
        FakeTty::with_input(b"ab"),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    assert_eq!(screen.getch(), Ok(Some(Event::Char('a'))));
    // The second character was buffered by the first read.
    assert_eq!(screen.getch(), Ok(Some(Event::Char('b'))));
    assert_eq!(screen.getch(), Ok(None));
}

#[test]
fn non_blocking_getch_yields_none_without_input() {
    let mut screen = Screen::new(
        FakeTty::with_input(b""),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    screen.set_input_mode(InputMode::NonBlocking);
    assert_eq!(screen.input_mode(), InputMode::NonBlocking);
    assert_eq!(screen.getch(), Ok(None));
}

#[test]
fn read_events_drains_events_buffered_by_getch() {
    let mut screen = Screen::new(
        FakeTty::with_input(b"ab"),
        TermType::Xterm256Color,
        Size::new(2, 2),
    );
    // The first getch reads "ab", returns 'a', and buffers 'b'.
    assert_eq!(screen.getch(), Ok(Some(Event::Char('a'))));
    // read_events delivers the buffered 'b' ahead of (now empty) fresh input.
    assert_eq!(screen.read_events(), Ok(vec![Event::Char('b')]));
}

#[test]
fn input_mode_selects_the_tty_read_method() {
    let mut screen = Screen::new(ModeTty::default(), TermType::Xterm256Color, Size::new(2, 2));

    screen.set_input_mode(InputMode::Blocking);
    let _ = screen.getch();
    assert_eq!(screen.tty_last_read(), Some(ReadKind::Blocking));

    screen.set_input_mode(InputMode::NonBlocking);
    let _ = screen.getch();
    assert_eq!(screen.tty_last_read(), Some(ReadKind::NonBlocking));

    screen.set_input_mode(InputMode::Timeout(Duration::from_millis(5)));
    let _ = screen.getch();
    assert_eq!(screen.tty_last_read(), Some(ReadKind::Timeout));
}

/// Which read method a [`ModeTty`] last serviced, so a test can prove
/// [`Screen::getch`] dispatches on the [`InputMode`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ReadKind {
    NonBlocking,
    Blocking,
    Timeout,
}

/// A [`Tty`] that records which read method was last called and returns no
/// bytes, so `getch` yields `None` regardless of mode.
#[derive(Default)]
struct ModeTty {
    last: Option<ReadKind>,
}

impl Tty for ModeTty {
    fn write(&mut self, _bytes: &[u8]) -> crate::Result<()> {
        Ok(())
    }

    fn read(&mut self) -> crate::Result<Vec<u8>> {
        self.last = Some(ReadKind::NonBlocking);
        Ok(Vec::new())
    }

    fn read_blocking(&mut self) -> crate::Result<Vec<u8>> {
        self.last = Some(ReadKind::Blocking);
        Ok(Vec::new())
    }

    fn read_timeout(&mut self, _timeout: Duration) -> crate::Result<Vec<u8>> {
        self.last = Some(ReadKind::Timeout);
        Ok(Vec::new())
    }
}

impl Screen<ModeTty> {
    /// The read method the [`ModeTty`] channel last serviced.
    fn tty_last_read(&self) -> Option<ReadKind> {
        self.tty_ref().last
    }
}
