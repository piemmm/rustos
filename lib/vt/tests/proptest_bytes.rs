//! Property test for the `lib/vt` streaming parser.
//!
//! Where the fuzz harness (`tests/fuzz_vt.rs`) drives a wall-clock budget
//! of seeded pseudo-random input, this `proptest` shrinks any counterexample to
//! a minimal failing case. Both assert the same fail-closed invariants over
//! arbitrary input:
//!
//! * feeding any byte stream never panics or reads out of bounds;
//! * feeding the stream byte-by-byte yields exactly the same [`tairix_vt::Op`]
//!   sequence as feeding it all at once (the parser is a true stream — chunk
//!   boundaries never change the result);
//! * whatever the emitter produces for an arbitrary [`tairix_vt::Op`] parses
//!   back to that identical operation (the "one vocabulary" guarantee).

use proptest::prelude::*;

use tairix_vt::{encode_into, BasicColor, Color, EraseMode, Op, Parser, Sgr};

/// Encode one operation into a fresh `Vec` over the allocation-free
/// [`encode_into`] sink API.
fn encode(op: &Op) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(op, &mut out);
    out
}

/// Collect every [`Op`] the parser emits for `bytes`, fed as one slice.
fn parse_all(bytes: &[u8]) -> Vec<Op> {
    let mut parser = Parser::new();
    let mut ops = Vec::new();
    parser.feed(bytes, |op| ops.push(op));
    ops
}

/// Collect every [`Op`] the parser emits for `bytes`, fed one byte at a time.
fn parse_byte_by_byte(bytes: &[u8]) -> Vec<Op> {
    let mut parser = Parser::new();
    let mut ops = Vec::new();
    for &byte in bytes {
        parser.feed_byte(byte, |op| ops.push(op));
    }
    ops
}

/// A strategy producing any [`Color`] across all three colour models.
fn color_strategy() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        (0u8..16)
            .prop_map(|i| Color::Basic(BasicColor::from_index(i).unwrap_or(BasicColor::Black))),
        any::<u8>().prop_map(Color::Indexed),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

/// A strategy producing any [`Op`] in the vocabulary.
fn op_strategy() -> impl Strategy<Value = Op> {
    let sgr = prop_oneof![
        Just(Sgr::Reset),
        Just(Sgr::Bold),
        Just(Sgr::Dim),
        Just(Sgr::Italic),
        Just(Sgr::Underline),
        Just(Sgr::Blink),
        Just(Sgr::Reverse),
        Just(Sgr::Strike),
        Just(Sgr::ResetIntensity),
        Just(Sgr::ResetItalic),
        Just(Sgr::ResetUnderline),
        Just(Sgr::ResetBlink),
        Just(Sgr::ResetReverse),
        Just(Sgr::ResetStrike),
        color_strategy().prop_map(Sgr::Foreground),
        color_strategy().prop_map(Sgr::Background),
    ];
    // Printable / non-control scalar values only: `Op::Print` carries a glyph,
    // not a C0 control (which travels as its own `Op`).
    let printable = prop_oneof![0x20u32..0x7f, 0xa0u32..0xd800, 0xe000u32..0x11_0000]
        .prop_map(|c| char::from_u32(c).unwrap_or(' '));
    prop_oneof![
        printable.prop_map(Op::Print),
        Just(Op::Bell),
        Just(Op::Backspace),
        Just(Op::Tab),
        Just(Op::LineFeed),
        Just(Op::CarriageReturn),
        (1u16..=u16::MAX).prop_map(Op::CursorUp),
        (1u16..=u16::MAX).prop_map(Op::CursorDown),
        (1u16..=u16::MAX).prop_map(Op::CursorForward),
        (1u16..=u16::MAX).prop_map(Op::CursorBack),
        (1u16..=u16::MAX).prop_map(Op::CursorColumn),
        (1u16..=u16::MAX, 1u16..=u16::MAX).prop_map(|(row, col)| Op::CursorPosition { row, col }),
        (1u16..=u16::MAX, 1u16..=u16::MAX)
            .prop_map(|(top, bottom)| Op::SetScrollRegion { top, bottom }),
        Just(Op::ResetScrollRegion),
        prop_oneof![
            Just(EraseMode::ToEnd),
            Just(EraseMode::ToStart),
            Just(EraseMode::All)
        ]
        .prop_map(Op::EraseInDisplay),
        sgr.prop_map(Op::Sgr),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Arbitrary bytes are consumed without panic or out-of-bounds access, and
    /// chunking the same bytes one at a time changes nothing.
    #[test]
    fn arbitrary_bytes_are_consumed_safely(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let whole = parse_all(&bytes);
        let chunked = parse_byte_by_byte(&bytes);
        prop_assert_eq!(whole, chunked);
    }

    /// Every emitted operation parses back to the identical operation.
    #[test]
    fn emit_parse_round_trip_identity(op in op_strategy()) {
        let bytes = encode(&op);
        prop_assert_eq!(parse_all(&bytes), vec![op]);
    }
}
