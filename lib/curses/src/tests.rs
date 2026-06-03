//! Unit tests for the curses screen model, renderer, and input decoder.
//!
//! The screen model is exercised directly (windows, scrolling, boxes); the
//! minimal-diff renderer is checked against golden `lib/vt` op sequences and
//! the capability-downgrade rules; the input decoder is driven per terminal
//! through the one shared `lib/vt` parser; and the [`Screen`] driver is run
//! over an in-memory [`Tty`] so the whole pipeline is host-testable without a
//! kernel (`AGENTS.md` §7).

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use rustos_termcap::TermType;
use rustos_vt::{encode, encode_all, Attributes, BasicColor, Cell, Color, MouseButton, Op, Parser};

use crate::buffer::Buffer;
use crate::color::{downgrade, ColorPairs};
use crate::error::CursesError;
use crate::geom::{Pos, Size};
use crate::input::{Event, Input};
use crate::render::{render, CursorState};
use crate::screen::{Screen, Tty};
use crate::window::{BorderChars, Window};

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
    // vocabulary end to end (`AGENTS.md` §2.2).
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
fn enabling_mouse_is_a_no_op_on_a_terminal_without_mouse_support() {
    // `vt100` has no mouse reporting, so enabling it writes nothing.
    let mut screen = Screen::new(FakeTty::with_input(b""), TermType::Vt100, Size::new(2, 2));
    assert_eq!(screen.enable_mouse(rustos_vt::MouseMode::Button), Ok(()));
}
