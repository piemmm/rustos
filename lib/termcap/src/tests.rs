//! Unit tests for the compiled-in capability database.

use alloc::vec::Vec;

use rustos_vt::{encode_into, Color, Op, Parser};

/// Encode one operation into a fresh `Vec` over the sink API.
fn encode(op: &Op) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(op, &mut out);
    out
}

use crate::capabilities::MouseReporting;
use crate::{from_term, ColorDepth, TermType};

#[test]
fn xterm_caps() {
    let caps = TermType::Xterm.capabilities();
    assert_eq!(caps.term_type, TermType::Xterm);
    assert_eq!(caps.color, ColorDepth::Ansi16);
    assert!(caps.cursor_addressing);
    assert!(caps.erase);
    assert!(caps.scroll_region);
    assert!(caps.alt_screen);
    assert!(caps.cursor_visibility);
    assert!(caps.set_title);
    assert!(!caps.mouse.is_supported());
    assert!(!caps.bracketed_paste);
    assert!(caps.keys.arrows.is_some());
    assert!(caps.keys.function_keys);
}

#[test]
fn xterm_color_caps() {
    let caps = TermType::XtermColor.capabilities();
    assert_eq!(caps.term_type, TermType::XtermColor);
    assert_eq!(caps.color, ColorDepth::Ansi16);
    assert!(caps.alt_screen);
    assert!(!caps.mouse.is_supported());
}

#[test]
fn xterm_16color_caps() {
    let caps = TermType::Xterm16Color.capabilities();
    assert_eq!(caps.term_type, TermType::Xterm16Color);
    assert_eq!(caps.color, ColorDepth::Ansi16);
    assert!(caps
        .color
        .supports(Color::Basic(rustos_vt::BasicColor::BrightCyan)));
    assert!(!caps.color.supports(Color::Indexed(200)));
}

#[test]
fn xterm_256color_caps() {
    let caps = TermType::Xterm256Color.capabilities();
    assert_eq!(caps.term_type, TermType::Xterm256Color);
    assert_eq!(caps.color, ColorDepth::Indexed256);
    assert!(caps.color.supports(Color::Indexed(200)));
    assert!(!caps.color.supports(Color::Rgb(1, 2, 3)));
    assert_eq!(caps.mouse.reporting, MouseReporting::ButtonEvent);
    assert!(caps.mouse.sgr_extended);
    assert!(caps.bracketed_paste);
}

#[test]
fn alacritty_caps() {
    let caps = TermType::Alacritty.capabilities();
    assert_eq!(caps.term_type, TermType::Alacritty);
    assert_eq!(caps.color, ColorDepth::TrueColor);
    assert!(caps.color.supports(Color::Rgb(1, 2, 3)));
    assert_eq!(caps.mouse.reporting, MouseReporting::AnyEvent);
    assert!(caps.bracketed_paste);
}

#[test]
fn xterm_kitty_caps() {
    let caps = TermType::XtermKitty.capabilities();
    assert_eq!(caps.term_type, TermType::XtermKitty);
    assert_eq!(caps.color, ColorDepth::TrueColor);
    assert_eq!(caps.mouse.reporting, MouseReporting::AnyEvent);
    assert!(caps.mouse.sgr_extended);
    assert!(caps.bracketed_paste);
}

#[test]
fn dumb_caps() {
    let caps = TermType::Dumb.capabilities();
    assert_eq!(caps.term_type, TermType::Dumb);
    assert_eq!(caps.color, ColorDepth::None);
    assert!(!caps.cursor_addressing);
    assert!(!caps.erase);
    assert!(!caps.scroll_region);
    assert!(!caps.alt_screen);
    assert!(!caps.cursor_visibility);
    assert!(!caps.set_title);
    assert!(!caps.mouse.is_supported());
    assert!(!caps.bracketed_paste);
    assert!(caps.keys.arrows.is_none());
    assert!(!caps.keys.function_keys);
    assert!(caps.referenced_ops().is_empty());
}

#[test]
fn vt100_caps() {
    let caps = TermType::Vt100.capabilities();
    assert_eq!(caps.term_type, TermType::Vt100);
    assert_eq!(caps.color, ColorDepth::None);
    assert!(caps.cursor_addressing);
    assert!(caps.erase);
    assert!(caps.scroll_region);
    assert!(!caps.alt_screen);
    assert!(!caps.set_title);
    assert!(!caps.mouse.is_supported());
    assert!(caps.keys.arrows.is_some());
    assert!(!caps.keys.function_keys);
    assert!(caps.keys.keypad);
}

#[test]
fn vt220_caps() {
    let caps = TermType::Vt220.capabilities();
    assert_eq!(caps.term_type, TermType::Vt220);
    assert_eq!(caps.color, ColorDepth::None);
    assert!(caps.cursor_addressing);
    assert!(caps.cursor_visibility);
    assert!(caps.keys.function_keys);
    assert!(caps.keys.editing_keys);
    assert!(!caps.mouse.is_supported());
}

#[test]
fn unknown_or_empty_term_falls_back_to_dumb() {
    assert_eq!(from_term(""), TermType::Dumb);
    assert_eq!(from_term("dumb"), TermType::Dumb);
    assert_eq!(from_term("no-such-terminal"), TermType::Dumb);
    assert_eq!(from_term("screen-256color"), TermType::Dumb);
    assert_eq!(from_term("XTERM-256COLOR"), TermType::Dumb);
    assert_eq!(from_term("xterm "), TermType::Dumb);
}

#[test]
fn term_name_round_trips_through_from_term() {
    for term in TermType::ALL {
        assert_eq!(from_term(term.term_name()), term);
    }
}

#[test]
fn color_depth_supports_each_model_at_the_right_depth() {
    for depth in [
        ColorDepth::None,
        ColorDepth::Ansi16,
        ColorDepth::Indexed256,
        ColorDepth::TrueColor,
    ] {
        assert!(depth.supports(Color::Default));
        assert_eq!(
            depth.supports(Color::Basic(rustos_vt::BasicColor::Red)),
            !matches!(depth, ColorDepth::None)
        );
        assert_eq!(
            depth.supports(Color::Indexed(64)),
            matches!(depth, ColorDepth::Indexed256 | ColorDepth::TrueColor)
        );
        assert_eq!(
            depth.supports(Color::Rgb(1, 2, 3)),
            matches!(depth, ColorDepth::TrueColor)
        );
    }
}

#[test]
fn no_record_emits_a_sequence_absent_from_vt() {
    for term in TermType::ALL {
        let caps = term.capabilities();
        for op in caps.referenced_ops() {
            let bytes = encode(&op);
            let mut parser = Parser::new();
            let mut seen: Vec<Op> = Vec::new();
            parser.feed(&bytes, |parsed| seen.push(parsed));
            assert_eq!(
                seen,
                alloc::vec![op.clone()],
                "{}: {op:?} did not round-trip through lib/vt",
                term.term_name(),
            );
        }
    }
}
