use super::{check_text, Reader, WireItem, WireSeq, Writer};
use crate::error::WireError;

/// A two-byte item with one invalid encoding, so the sequence's per-item
/// validation has something to reject.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Pair(u8, u8);

impl WireItem for Pair {
    const WIRE_LEN: usize = 2;

    fn read(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let a = r.u8()?;
        let b = r.u8()?;
        if a == 0xFF {
            return Err(WireError::UnknownDiscriminant);
        }
        Ok(Self(a, b))
    }

    fn write(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u8(self.0)?;
        w.u8(self.1)
    }
}

#[test]
fn scalars_round_trip_little_endian() {
    let mut out = [0u8; 32];
    let mut w = Writer::new(&mut out);
    w.u8(0x12).expect("fits");
    w.u16(0x3456).expect("fits");
    w.i16(-2).expect("fits");
    w.u32(0x89AB_CDEF).expect("fits");
    w.i32(-3).expect("fits");
    w.u64(0x0102_0304_0506_0708).expect("fits");
    let n = w.written();
    assert_eq!(out[0], 0x12);
    assert_eq!(&out[1..3], &[0x56, 0x34]);

    let mut r = Reader::new(&out[..n]);
    assert_eq!(r.u8(), Ok(0x12));
    assert_eq!(r.u16(), Ok(0x3456));
    assert_eq!(r.i16(), Ok(-2));
    assert_eq!(r.u32(), Ok(0x89AB_CDEF));
    assert_eq!(r.i32(), Ok(-3));
    assert_eq!(r.u64(), Ok(0x0102_0304_0506_0708));
    assert_eq!(r.finish(), Ok(()));
}

#[test]
fn reading_past_the_end_is_truncation_not_a_panic() {
    let mut r = Reader::new(&[1, 2, 3]);
    assert_eq!(r.u16(), Ok(0x0201));
    assert_eq!(r.u16(), Err(WireError::Truncated));
    assert_eq!(r.u64(), Err(WireError::Truncated));
    assert_eq!(r.take(usize::MAX), Err(WireError::Truncated));
    assert_eq!(r.remaining(), 1);
}

#[test]
fn trailing_bytes_are_refused() {
    let mut r = Reader::new(&[1, 2, 3]);
    assert_eq!(r.u16(), Ok(0x0201));
    assert_eq!(r.finish(), Err(WireError::TrailingBytes));
}

#[test]
fn a_presence_flag_admits_only_zero_and_one() {
    assert_eq!(Reader::new(&[0]).flag(), Ok(false));
    assert_eq!(Reader::new(&[1]).flag(), Ok(true));
    for bad in [2u8, 3, 0x80, 0xFF] {
        assert_eq!(Reader::new(&[bad]).flag(), Err(WireError::FieldOutOfRange));
    }
}

#[test]
fn padding_must_be_zero() {
    assert_eq!(Reader::new(&[0, 0, 0]).padding(3), Ok(()));
    assert_eq!(
        Reader::new(&[0, 1, 0]).padding(3),
        Err(WireError::NonCanonicalPadding)
    );
    assert_eq!(Reader::new(&[0]).padding(3), Err(WireError::Truncated));
}

#[test]
fn writing_past_the_buffer_is_refused_not_a_panic() {
    let mut out = [0u8; 3];
    let mut w = Writer::new(&mut out);
    assert_eq!(w.u16(1), Ok(()));
    assert_eq!(w.u32(1), Err(WireError::BufferTooSmall));
    assert_eq!(w.u8(7), Ok(()));
    assert_eq!(w.u8(7), Err(WireError::BufferTooSmall));
    assert_eq!(w.written(), 3);
}

#[test]
fn text_refuses_control_characters_and_honours_its_bounds() {
    assert_eq!(check_text("ordinary text", false), Ok(()));
    assert_eq!(
        check_text("with\u{1b}[31m escape", false),
        Err(WireError::BadText)
    );
    assert_eq!(check_text("a\nb", false), Err(WireError::BadText));
    assert_eq!(check_text("a\nb\tc", true), Ok(()));
    assert_eq!(check_text("a\u{7f}b", true), Err(WireError::BadText));
    // A control codepoint spelled as multi-byte UTF-8 is still a control.
    assert_eq!(check_text("a\u{85}b", true), Err(WireError::BadText));
}

#[test]
fn text_fields_round_trip_and_refuse_their_edges() {
    let mut out = [0u8; 64];
    let mut w = Writer::new(&mut out);
    w.text8("realm", 1, 32).expect("fits");
    w.text16("a longer body", 1, 32, false).expect("fits");
    let n = w.written();

    let mut r = Reader::new(&out[..n]);
    assert_eq!(r.text8(1, 32), Ok("realm"));
    assert_eq!(r.text16(1, 32, false), Ok("a longer body"));
    assert_eq!(r.finish(), Ok(()));

    let mut short = [0u8; 8];
    let mut w = Writer::new(&mut short);
    assert_eq!(w.text8("", 1, 32), Err(WireError::BoundExceeded));
    assert_eq!(w.text8("toolong", 1, 3), Err(WireError::BoundExceeded));
    assert_eq!(w.text8("a\u{1b}b", 1, 32), Err(WireError::BadText));
}

#[test]
fn an_oversize_text_length_is_refused_before_the_bytes_are_read() {
    // A length prefix past the field's cap must be refused on the prefix
    // alone, so a hostile frame cannot make the reader walk the input.
    let mut input = [0u8; 4];
    input[0] = 200;
    let mut r = Reader::new(&input);
    assert_eq!(r.text8(1, 32), Err(WireError::BoundExceeded));
    // Nothing past the prefix was consumed.
    assert_eq!(r.remaining(), 3);
}

#[test]
fn a_non_utf8_text_field_is_refused() {
    let input = [2u8, 0xFF, 0xFE];
    assert_eq!(Reader::new(&input).text8(1, 32), Err(WireError::BadText));
}

#[test]
fn a_sequence_round_trips_from_items_and_from_the_wire() {
    let items = [Pair(1, 2), Pair(3, 4), Pair(5, 6)];
    let seq = WireSeq::from_items(&items);
    assert_eq!(seq.len(), 3);
    assert!(!seq.is_empty());

    let mut out = [0u8; 16];
    let mut w = Writer::new(&mut out);
    seq.write(&mut w).expect("fits");
    let n = w.written();
    assert_eq!(n, 6);

    let mut r = Reader::new(&out[..n]);
    let decoded: WireSeq<'_, Pair> = WireSeq::read(&mut r, 3).expect("valid");
    assert_eq!(r.finish(), Ok(()));
    assert_eq!(decoded, seq);
    assert_eq!(decoded.iter().count(), decoded.len());
    assert!(decoded.iter().eq(items.iter().copied()));
    assert_eq!(decoded.get(3), None);

    // Re-encoding a decoded run reproduces the same bytes.
    let mut again = [0u8; 16];
    let rewritten = {
        let mut w = Writer::new(&mut again);
        decoded.write(&mut w).expect("fits");
        w.written()
    };
    assert_eq!(&again[..rewritten], &out[..n]);
}

#[test]
fn an_empty_sequence_reads_writes_and_iterates() {
    let seq: WireSeq<'_, Pair> = WireSeq::empty();
    assert!(seq.is_empty());
    assert_eq!(seq.iter().count(), 0);
    let mut out = [0u8; 4];
    let mut w = Writer::new(&mut out);
    seq.write(&mut w).expect("fits");
    assert_eq!(w.written(), 0);
    let mut r = Reader::new(&[]);
    let decoded: WireSeq<'_, Pair> = WireSeq::read(&mut r, 0).expect("valid");
    assert!(decoded.is_empty());
}

#[test]
fn a_sequence_validates_every_item_when_it_is_read() {
    // The second item is invalid; the whole run must be refused, not
    // silently shortened to the first.
    let bytes = [1u8, 2, 0xFF, 4];
    let mut r = Reader::new(&bytes);
    assert_eq!(
        WireSeq::<'_, Pair>::read(&mut r, 2).map(|s| s.len()),
        Err(WireError::UnknownDiscriminant)
    );
}

#[test]
fn a_short_sequence_is_truncation() {
    let bytes = [1u8, 2, 3];
    let mut r = Reader::new(&bytes);
    assert_eq!(
        WireSeq::<'_, Pair>::read(&mut r, 2).map(|s| s.len()),
        Err(WireError::Truncated)
    );
}

#[test]
fn a_count_whose_span_overflows_is_refused() {
    let mut r = Reader::new(&[1, 2]);
    assert_eq!(
        WireSeq::<'_, Pair>::read(&mut r, usize::MAX).map(|s| s.len()),
        Err(WireError::Truncated)
    );
}

#[test]
fn a_sequence_written_into_a_full_buffer_is_refused() {
    let items = [Pair(1, 2), Pair(3, 4)];
    let seq = WireSeq::from_items(&items);
    let mut out = [0u8; 3];
    let mut w = Writer::new(&mut out);
    assert_eq!(seq.write(&mut w), Err(WireError::BufferTooSmall));
}
