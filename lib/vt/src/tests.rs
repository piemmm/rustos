//! Unit tests for the shared vocabulary.
//!
//! The headline guarantee is that the emitter and the parser agree
//! (`AGENTS.md` §2.2): the `*_round_trip` tests encode an [`Op`] and assert it
//! parses straight back. The `*_never_panics`/robustness tests exercise the
//! fail-closed parser (`AGENTS.md` §2.9) on hostile and partial input.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::attr::Attributes;
use crate::color::{BasicColor, Color};
use crate::op::EraseMode;
use crate::{encode, encode_all, Op, Parser, Sgr};

/// Parse `bytes` to completion, collecting every emitted [`Op`].
fn parse_all(bytes: &[u8]) -> Vec<Op> {
    let mut parser = Parser::new();
    let mut ops = Vec::new();
    parser.feed(bytes, |op| ops.push(op));
    ops
}

/// Encoding `op` then parsing the bytes yields exactly `[op]`.
fn assert_round_trip(op: Op) {
    let bytes = encode(&op);
    let parsed = parse_all(&bytes);
    assert_eq!(parsed, vec![op], "round trip failed");
}

/// Every SGR colour model: the 16 basic colours, the 256-colour palette, and
/// truecolour, for both foreground and background.
fn all_colors() -> Vec<Color> {
    let mut colors = vec![Color::Default];
    for basic in BasicColor::ALL {
        colors.push(Color::Basic(basic));
    }
    for index in [0u8, 1, 7, 8, 15, 16, 128, 231, 255] {
        colors.push(Color::Indexed(index));
    }
    for rgb in [(0, 0, 0), (0x30, 0x70, 0xf0), (255, 255, 255), (1, 2, 3)] {
        colors.push(Color::Rgb(rgb.0, rgb.1, rgb.2));
    }
    colors
}

/// Every distinct SGR operation the vocabulary defines.
fn all_sgr() -> Vec<Sgr> {
    let mut sgr = vec![
        Sgr::Reset,
        Sgr::Bold,
        Sgr::Dim,
        Sgr::Italic,
        Sgr::Underline,
        Sgr::Blink,
        Sgr::Reverse,
        Sgr::Strike,
        Sgr::ResetIntensity,
        Sgr::ResetItalic,
        Sgr::ResetUnderline,
        Sgr::ResetBlink,
        Sgr::ResetReverse,
        Sgr::ResetStrike,
    ];
    for color in all_colors() {
        sgr.push(Sgr::Foreground(color));
        sgr.push(Sgr::Background(color));
    }
    sgr
}

#[test]
fn every_sgr_op_round_trips() {
    for sgr in all_sgr() {
        assert_round_trip(Op::Sgr(sgr));
    }
}

#[test]
fn every_c0_control_round_trips() {
    for op in [
        Op::Bell,
        Op::Backspace,
        Op::Tab,
        Op::LineFeed,
        Op::CarriageReturn,
    ] {
        assert_round_trip(op);
    }
}

#[test]
fn cursor_movement_ops_round_trip() {
    for n in [1u16, 2, 9, 80, 999, u16::from(u8::MAX), 0xffff] {
        assert_round_trip(Op::CursorUp(n));
        assert_round_trip(Op::CursorDown(n));
        assert_round_trip(Op::CursorForward(n));
        assert_round_trip(Op::CursorBack(n));
        assert_round_trip(Op::CursorNextLine(n));
        assert_round_trip(Op::CursorPrevLine(n));
        assert_round_trip(Op::CursorColumn(n));
        assert_round_trip(Op::ScrollUp(n));
        assert_round_trip(Op::ScrollDown(n));
    }
}

#[test]
fn cursor_position_and_scroll_region_round_trip() {
    for (a, b) in [(1u16, 1u16), (1, 24), (12, 40), (24, 80), (0xffff, 0xffff)] {
        assert_round_trip(Op::CursorPosition { row: a, col: b });
        assert_round_trip(Op::SetScrollRegion { top: a, bottom: b });
    }
    assert_round_trip(Op::ResetScrollRegion);
}

#[test]
fn erase_ops_round_trip() {
    for mode in [EraseMode::ToEnd, EraseMode::ToStart, EraseMode::All] {
        assert_round_trip(Op::EraseInDisplay(mode));
        assert_round_trip(Op::EraseInLine(mode));
    }
}

#[test]
fn mode_and_cursor_state_ops_round_trip() {
    for op in [
        Op::EnterAltScreen,
        Op::LeaveAltScreen,
        Op::ShowCursor,
        Op::HideCursor,
        Op::SaveCursor,
        Op::RestoreCursor,
    ] {
        assert_round_trip(op);
    }
}

#[test]
fn printable_and_unicode_chars_round_trip() {
    for ch in [
        'A', 'z', '0', ' ', '~', '!', 'é', 'ß', '€', '中', '🦀', '\u{85}',
    ] {
        assert_round_trip(Op::Print(ch));
    }
}

#[test]
fn window_title_round_trips() {
    for title in [
        "",
        "rustos",
        "a long title with spaces",
        "semi;colon",
        "café",
    ] {
        assert_round_trip(Op::SetTitle(title.to_string()));
    }
}

#[test]
fn a_whole_program_stream_round_trips() {
    let ops = vec![
        Op::SetTitle("rustos".to_string()),
        Op::EnterAltScreen,
        Op::HideCursor,
        Op::CursorPosition { row: 1, col: 1 },
        Op::Sgr(Sgr::Bold),
        Op::Sgr(Sgr::Foreground(Color::Basic(BasicColor::Green))),
        Op::Print('h'),
        Op::Print('i'),
        Op::Sgr(Sgr::Reset),
        Op::CarriageReturn,
        Op::LineFeed,
        Op::EraseInLine(EraseMode::All),
        Op::ShowCursor,
        Op::LeaveAltScreen,
    ];
    let bytes = encode_all(&ops);
    assert_eq!(parse_all(&bytes), ops);
}

#[test]
fn one_sgr_sequence_can_carry_many_attributes() {
    // `CSI 1;31;4m` is three operations in one sequence.
    let ops = parse_all(b"\x1b[1;31;4m");
    assert_eq!(
        ops,
        vec![
            Op::Sgr(Sgr::Bold),
            Op::Sgr(Sgr::Foreground(Color::Basic(BasicColor::Red))),
            Op::Sgr(Sgr::Underline),
        ]
    );
}

#[test]
fn bare_sgr_is_a_reset() {
    assert_eq!(parse_all(b"\x1b[m"), vec![Op::Sgr(Sgr::Reset)]);
    assert_eq!(parse_all(b"\x1b[0m"), vec![Op::Sgr(Sgr::Reset)]);
}

#[test]
fn default_movement_parameter_is_one() {
    // `CSI A` with no parameter (and `CSI 0 A`) both mean "up one".
    assert_eq!(parse_all(b"\x1b[A"), vec![Op::CursorUp(1)]);
    assert_eq!(parse_all(b"\x1b[0A"), vec![Op::CursorUp(1)]);
}

#[test]
fn oversized_parameter_saturates_without_overflow() {
    // Far more digits than `u16` can hold: the accumulator saturates at
    // `PARAM_MAX` and the parameter clamps to `u16::MAX`, never overflowing.
    let ops = parse_all(b"\x1b[99999999999999A");
    assert_eq!(ops, vec![Op::CursorUp(u16::MAX)]);
}

#[test]
fn unrecognised_sequences_are_dropped_not_panicked() {
    // An unknown CSI final, an unmodelled escape, and an unknown private mode
    // each produce no operation but leave the parser usable.
    assert_eq!(parse_all(b"\x1b[5Z"), vec![]);
    assert_eq!(parse_all(b"\x1bZ"), vec![]);
    assert_eq!(parse_all(b"\x1b[?9999h"), vec![]);
    // ...and a following good sequence still parses.
    assert_eq!(parse_all(b"\x1b[5Z\x1b[A"), vec![Op::CursorUp(1)]);
}

#[test]
fn a_truncated_escape_does_not_lose_the_next_escape() {
    // The lone `ESC` before the second `ESC [ A` must not swallow it.
    assert_eq!(parse_all(b"\x1b\x1b[A"), vec![Op::CursorUp(1)]);
}

#[test]
fn split_sequence_across_feeds_round_trips() {
    let bytes = encode(&Op::Sgr(Sgr::Foreground(Color::Rgb(1, 2, 3))));
    let mut parser = Parser::new();
    let mut ops = Vec::new();
    for &byte in &bytes {
        parser.feed_byte(byte, |op| ops.push(op));
    }
    assert_eq!(ops, vec![Op::Sgr(Sgr::Foreground(Color::Rgb(1, 2, 3)))]);
}

#[test]
fn basic_color_index_round_trips() {
    for basic in BasicColor::ALL {
        assert_eq!(BasicColor::from_index(basic.index()), Some(basic));
    }
    assert_eq!(BasicColor::from_index(16), None);
}

#[test]
fn attributes_fold_sgr_operations() {
    let mut attrs = Attributes::PLAIN;
    attrs.apply(Sgr::Bold);
    attrs.apply(Sgr::Underline);
    attrs.apply(Sgr::Foreground(Color::Basic(BasicColor::Cyan)));
    assert!(attrs.bold);
    assert!(attrs.underline);
    assert_eq!(attrs.foreground, Color::Basic(BasicColor::Cyan));

    attrs.apply(Sgr::ResetIntensity);
    assert!(!attrs.bold);
    assert!(attrs.underline);

    attrs.apply(Sgr::Reset);
    assert_eq!(attrs, Attributes::PLAIN);
}

#[test]
fn parser_consumes_a_deterministic_byte_sweep_without_panic() {
    // A fixed-seed LCG drives a smoke sweep of arbitrary byte strings; the
    // single invariant is that `feed` never panics and the parser stays usable.
    // The dedicated wall-clock budgeted run lives in `tests/fuzz_vt.rs`.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut parser = Parser::new();
    let mut sink = String::new();
    for _ in 0..50_000 {
        let len = usize::try_from(next() % 32).unwrap_or(0);
        let bytes: Vec<u8> = (0..len).map(|_| next().to_le_bytes()[0]).collect();
        parser.feed(&bytes, |op| {
            if let Op::Print(ch) = op {
                sink.push(ch);
            }
        });
    }
    // Reaching here without a panic is the assertion.
    let _ = sink;
}
