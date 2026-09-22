//! Unit tests for the RFC 1951 DEFLATE decoder.
//!
//! Most streams here are assembled by hand through `BitWriter`, a
//! deliberately tiny test-only bit writer (LSB-first for plain fields,
//! MSB-first for Huffman codes), so each expected output is
//! self-documenting from the bits that produced it rather than pinned to an
//! opaque byte blob. The fixtures at the end are the exception, and are
//! there precisely because they came from somewhere else.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use super::{build_huffman, inflate_into, is_permitted_incomplete, Error, Inflater};

/// A minimal test-only bit writer, the encode-side mirror of [`super::BitReader`].
struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    fn put_bit(&mut self, bit: u32) {
        self.cur |= u8::try_from(bit & 1).unwrap_or(0) << self.nbits;
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    /// Write an `n`-bit plain value, least-significant bit first (every
    /// non-Huffman-code field in RFC 1951).
    fn put_bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            self.put_bit((value >> i) & 1);
        }
    }

    /// Write an `n`-bit Huffman code, most-significant bit first (RFC 1951
    /// §3.1.1's one exception to LSB-first packing).
    fn put_code(&mut self, code: u32, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((code >> i) & 1);
        }
    }

    fn align_to_byte(&mut self) {
        while self.nbits != 0 {
            self.put_bit(0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align_to_byte();
        self.bytes
    }
}

/// The fixed literal/length Huffman code for `symbol` (RFC 1951 §3.2.6),
/// as `(code, bit_length)` with `code` sent MSB-first.
fn fixed_litlen_code(symbol: u16) -> (u32, u32) {
    let v = u32::from(symbol);
    if v <= 143 {
        (0x30 + v, 8)
    } else if v <= 255 {
        (0x190 + (v - 144), 9)
    } else if v <= 279 {
        (v - 256, 7)
    } else {
        (0xC0 + (v - 280), 8)
    }
}

/// Emit one literal byte through the fixed literal/length table.
fn fixed_literal(bw: &mut BitWriter, byte: u8) {
    let (code, len) = fixed_litlen_code(u16::from(byte));
    bw.put_code(code, len);
}

/// Emit the fixed end-of-block symbol (256).
fn fixed_end_of_block(bw: &mut BitWriter) {
    let (code, len) = fixed_litlen_code(256);
    bw.put_code(code, len);
}

/// Emit a length/distance back-reference through the fixed tables, using
/// only base lengths/distances (no extra bits) for a self-documenting test.
fn fixed_match(bw: &mut BitWriter, length_symbol: u16, distance_symbol: u16) {
    let (code, len) = fixed_litlen_code(length_symbol);
    bw.put_code(code, len);
    bw.put_code(u32::from(distance_symbol), 5);
}

fn decode(src: &[u8], dst_len: usize) -> Result<Vec<u8>, Error> {
    let mut dst = vec![0u8; dst_len];
    let n = inflate_into(src, &mut dst)?;
    dst.truncate(n);
    Ok(dst)
}

// ---- stored blocks -----------------------------------------------------

#[test]
fn stored_block_round_trips() {
    let mut bw = BitWriter::new();
    bw.put_bit(1); // BFINAL
    bw.put_bits(0, 2); // BTYPE = 00 (stored)
    bw.align_to_byte();
    let data = b"stored data";
    let len = u16::try_from(data.len()).expect("fits");
    bw.bytes.extend_from_slice(&len.to_le_bytes());
    bw.bytes.extend_from_slice(&(!len).to_le_bytes());
    bw.bytes.extend_from_slice(data);
    let src = bw.finish();

    assert_eq!(decode(&src, data.len()), Ok(data.to_vec()));
}

#[test]
fn stored_block_empty_round_trips() {
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(0, 2);
    bw.align_to_byte();
    bw.bytes.extend_from_slice(&0u16.to_le_bytes());
    bw.bytes.extend_from_slice(&(!0u16).to_le_bytes());
    let src = bw.finish();

    assert_eq!(decode(&src, 0), Ok(Vec::new()));
}

#[test]
fn two_stored_blocks_concatenate() {
    let mut bw = BitWriter::new();
    bw.put_bit(0); // not final
    bw.put_bits(0, 2);
    bw.align_to_byte();
    bw.bytes.extend_from_slice(&3u16.to_le_bytes());
    bw.bytes.extend_from_slice(&(!3u16).to_le_bytes());
    bw.bytes.extend_from_slice(b"abc");

    bw.put_bit(1); // final
    bw.put_bits(0, 2);
    bw.align_to_byte();
    bw.bytes.extend_from_slice(&2u16.to_le_bytes());
    bw.bytes.extend_from_slice(&(!2u16).to_le_bytes());
    bw.bytes.extend_from_slice(b"de");
    let src = bw.finish();

    assert_eq!(decode(&src, 5), Ok(b"abcde".to_vec()));
}

#[test]
fn stored_block_rejects_bad_nlen() {
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(0, 2);
    bw.align_to_byte();
    bw.bytes.extend_from_slice(&5u16.to_le_bytes());
    // NLEN should be `!5`; write `5` again instead.
    bw.bytes.extend_from_slice(&5u16.to_le_bytes());
    bw.bytes.extend_from_slice(b"hello");
    let src = bw.finish();

    assert_eq!(decode(&src, 5), Err(Error::InvalidStoredBlockLength));
}

// ---- fixed huffman -------------------------------------------------------

#[test]
fn fixed_huffman_literals_round_trip() {
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(1, 2); // BTYPE = 01 (fixed huffman)
    for &byte in b"Hi" {
        fixed_literal(&mut bw, byte);
    }
    fixed_end_of_block(&mut bw);
    let src = bw.finish();

    assert_eq!(decode(&src, 2), Ok(b"Hi".to_vec()));
}

#[test]
fn fixed_huffman_backreference_expands_overlap() {
    // "a" followed by a length-4 copy at distance 1: an overlapping
    // (run-length) back-reference that must expand byte-by-byte.
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(1, 2);
    fixed_literal(&mut bw, b'a');
    fixed_match(&mut bw, 258, 0); // length base 4, distance base 1
    fixed_end_of_block(&mut bw);
    let src = bw.finish();

    assert_eq!(decode(&src, 5), Ok(b"aaaaa".to_vec()));
}

#[test]
fn fixed_huffman_rejects_reserved_symbol_286() {
    // Symbol 286 is representable in the fixed code space but RFC 1951
    // never allows it to appear in valid data.
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(1, 2);
    let (code, len) = fixed_litlen_code(286);
    bw.put_code(code, len);
    let src = bw.finish();

    assert_eq!(decode(&src, 8), Err(Error::InvalidSymbol));
}

#[test]
fn fixed_huffman_rejects_distance_before_any_output() {
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(1, 2);
    fixed_match(&mut bw, 258, 0); // a back-reference with nothing produced yet
    let src = bw.finish();

    assert_eq!(decode(&src, 8), Err(Error::DistanceTooFar));
}

#[test]
fn output_buffer_too_small_is_refused() {
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(1, 2);
    fixed_literal(&mut bw, b'A');
    fixed_literal(&mut bw, b'B');
    fixed_end_of_block(&mut bw);
    let src = bw.finish();

    assert_eq!(decode(&src, 1), Err(Error::OutputOverflow));
}

// ---- malformed block headers --------------------------------------------

#[test]
fn invalid_block_type_is_refused() {
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(3, 2); // BTYPE = 11, reserved
    let src = bw.finish();

    assert_eq!(decode(&src, 8), Err(Error::InvalidBlockType));
}

#[test]
fn truncated_stream_is_unexpected_eof() {
    // Only the 3-bit block header exists; a fixed-huffman block needs at
    // least a 7-bit symbol to follow, which this single byte cannot supply.
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(1, 2);
    let src = bw.finish();

    assert_eq!(decode(&src, 8), Err(Error::UnexpectedEof));
}

#[test]
fn empty_input_is_unexpected_eof() {
    assert_eq!(decode(&[], 8), Err(Error::UnexpectedEof));
}

// ---- dynamic huffman -----------------------------------------------------

#[test]
fn dynamic_huffman_single_literal_round_trips() {
    // A minimal, fully hand-assembled dynamic block encoding just the byte
    // `'A'` (65): HLIT = 257 (only literals 0..=256 are codable, so no
    // match is possible), HDIST = 1 (the degenerate single-code distance
    // table RFC 1951 permits when a block never emits a match).
    //
    // Literal/length lengths: symbol 65 and symbol 256 (end-of-block) both
    // get length 1 (a complete 2-code set); every other symbol is unused.
    // Canonical order gives the lower-indexed symbol (65) code `0` and the
    // higher-indexed one (256) code `1`.
    //
    // Distance lengths: the sole declared code (index 0) gets length 1 —
    // incomplete on its own, but never referenced (no match is emitted),
    // which is exactly the case RFC 1951 tolerates it for.
    //
    // The combined 258-length array (257 literal/length + 1 distance) is
    // coded through the 19-symbol code-length alphabet as: 65 leading
    // zeros (repeat code 18), then literal length `1` (symbol 65), then
    // 138 + 52 zeros (two more repeat-18s) to skip to index 256, then
    // literal length `1` twice (symbol 256, then the lone distance code).
    // The code-length alphabet itself uses only symbols `0` and `18`,
    // each given a complete 1-bit code (`0` gets code `0`, `18` gets
    // code `1`, by ascending symbol index).
    let mut bw = BitWriter::new();
    bw.put_bit(1); // BFINAL
    bw.put_bits(2, 2); // BTYPE = 10 (dynamic huffman)
    bw.put_bits(0, 5); // HLIT - 257 = 0
    bw.put_bits(0, 5); // HDIST - 1 = 0
    bw.put_bits(14, 4); // HCLEN - 4 = 14 -> transmit 18 code-length lengths

    // Code-length code lengths, in RFC 1951's transmission order
    // (16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1): only symbols 18 (3rd)
    // and 1 (17th) are used, both length 1.
    let cl_lengths = [0u32, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    for len in cl_lengths {
        bw.put_bits(len, 3);
    }

    // Code-length-alphabet codes: symbol 1 -> code 0 (1 bit), symbol 18 ->
    // code 1 (1 bit), by ascending symbol index among the two length-1 codes.
    let repeat_18 = |bw: &mut BitWriter, extra: u32| {
        bw.put_code(1, 1);
        bw.put_bits(extra, 7);
    };
    let literal_1 = |bw: &mut BitWriter| bw.put_code(0, 1);

    repeat_18(&mut bw, 65 - 11); // 65 zeros: indices 0..65
    literal_1(&mut bw); // index 65 (symbol 'A') = length 1
    repeat_18(&mut bw, 138 - 11); // 138 zeros: indices 66..204
    repeat_18(&mut bw, 52 - 11); // 52 zeros: indices 204..256
    literal_1(&mut bw); // index 256 (end-of-block) = length 1
    literal_1(&mut bw); // index 257 (the lone distance code) = length 1

    // The literal/length data: 'A' (code 0), then end-of-block (code 1).
    bw.put_code(0, 1);
    bw.put_code(1, 1);
    let src = bw.finish();

    assert_eq!(decode(&src, 1), Ok(b"A".to_vec()));
}

#[test]
fn dynamic_huffman_rejects_repeat_16_with_no_previous_length() {
    // A complete code-length table using only symbols 0 and 16 (ascending
    // index: 0 -> code 0, 16 -> code 1), transmitting just enough entries
    // to declare both, then immediately decoding symbol 16 as the very
    // first code-length symbol — which has no preceding length to repeat.
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(2, 2);
    bw.put_bits(0, 5); // HLIT - 257 = 0
    bw.put_bits(0, 5); // HDIST - 1 = 0
    bw.put_bits(0, 4); // HCLEN - 4 = 0 -> transmit 4 code-length lengths

    // Order: 16, 17, 18, 0. Only positions 0 (symbol 16) and 3 (symbol 0)
    // are used.
    for len in [1u32, 0, 0, 1] {
        bw.put_bits(len, 3);
    }

    // Symbol 0 -> code 0, symbol 16 -> code 1 (ascending symbol index).
    bw.put_code(1, 1); // decode symbol 16 first, with index == 0
    let src = bw.finish();

    assert_eq!(decode(&src, 8), Err(Error::InvalidLengthRepeat));
}

#[test]
fn dynamic_huffman_rejects_repeat_count_overrunning_declared_lengths() {
    // HLIT + HDIST = 258 total lengths to fill. Two repeat-18s of the
    // maximum count (138 each) would fill 276, overrunning the 258
    // declared — the second one must be refused before it overruns.
    let mut bw = BitWriter::new();
    bw.put_bit(1);
    bw.put_bits(2, 2);
    bw.put_bits(0, 5);
    bw.put_bits(0, 5);
    bw.put_bits(0, 4); // HCLEN - 4 = 0 -> transmit 4 code-length lengths

    // Order: 16, 17, 18, 0. Symbols 18 and 0 are used (ascending index:
    // 0 -> code 0, 18 -> code 1).
    for len in [0u32, 0, 1, 1] {
        bw.put_bits(len, 3);
    }

    bw.put_code(1, 1); // symbol 18
    bw.put_bits(127, 7); // repeat count 11 + 127 = 138
    bw.put_code(1, 1); // symbol 18 again: 138 + 138 = 276 > 258
    bw.put_bits(127, 7);
    let src = bw.finish();

    assert_eq!(decode(&src, 8), Err(Error::InvalidLengthRepeat));
}

// ---- build_huffman unit tests --------------------------------------------

#[test]
fn build_huffman_rejects_oversubscribed_lengths() {
    // Three symbols all claiming the single-bit code space (which holds
    // only two codes) is a textbook oversubscription.
    assert!(matches!(
        build_huffman(&[1, 1, 1]),
        Err(Error::OversubscribedHuffmanCode)
    ));
}

#[test]
fn build_huffman_reports_incomplete_when_codespace_is_unused() {
    // One length-1 code and one length-3 code: after the length-1 code
    // claims half the codespace, the length-3 code cannot claim the rest.
    let (table, incomplete) = build_huffman(&[1, 3]).expect("not oversubscribed");
    assert!(incomplete);
    // Not the permitted single-code case: a length-3 code is also used.
    assert!(!is_permitted_incomplete(&table));
}

#[test]
fn build_huffman_permits_the_single_length_one_code_case() {
    let (table, incomplete) = build_huffman(&[1]).expect("a single code is not oversubscribed");
    assert!(incomplete);
    assert!(is_permitted_incomplete(&table));
}

#[test]
fn build_huffman_accepts_a_complete_code() {
    // Two symbols of length 1 exactly fill the codespace.
    let (_table, incomplete) = build_huffman(&[1, 1]).expect("complete");
    assert!(!incomplete);
}

// ---------------------------------------------------------------------------
// Foreign-encoder fixtures.
//
// Streams a real zlib produced, pinned here as known answers: a decoder that
// only ever reads its own encoder's output agrees with its own bugs.
// ---------------------------------------------------------------------------

/// zlib level 9 over short repetitive text, which it codes with the fixed
/// Huffman tables.
const FOREIGN_FIXED: [u8; 15] = [
    0x4B, 0x4C, 0x2A, 0x4A, 0x4C, 0x4E, 0x4C, 0x49, 0x04, 0x52, 0x0A, 0x89, 0x08, 0x36, 0x00,
];

/// zlib level 9 over a skewed alphabet long enough that a transmitted tree
/// pays for itself, so this is a dynamic-Huffman block.
const FOREIGN_DYNAMIC: [u8; 105] = [
    0x2B, 0xC9, 0x48, 0x55, 0x28, 0x4A, 0xCC, 0xCC, 0x53, 0x00, 0xA2, 0xE2, 0x02, 0x10, 0x23, 0x2D,
    0x31, 0x27, 0xA7, 0x58, 0x21, 0x17, 0xC8, 0xCC, 0xA9, 0x54, 0xC8, 0xCF, 0x53, 0x28, 0x01, 0xAA,
    0x28, 0xC8, 0x01, 0x72, 0xAD, 0xC1, 0xCC, 0x51, 0xC5, 0x23, 0x46, 0xB1, 0x8D, 0xAD, 0x9D, 0xBD,
    0x83, 0xA3, 0x93, 0xB3, 0x8B, 0xAB, 0x9B, 0xBB, 0x87, 0xA7, 0x97, 0xB7, 0x8F, 0xAF, 0x9F, 0x7F,
    0x40, 0x60, 0x50, 0x70, 0x48, 0x68, 0x58, 0x78, 0x44, 0x64, 0x54, 0x74, 0x4C, 0x6C, 0x5C, 0x7C,
    0x42, 0x62, 0x52, 0x72, 0x4A, 0x6A, 0x5A, 0x7A, 0x46, 0x66, 0x56, 0x76, 0x4E, 0x6E, 0x5E, 0x7E,
    0x41, 0x61, 0x51, 0x71, 0x49, 0x69, 0x59, 0x39, 0x00,
];

/// zlib level 0, which stores every block verbatim.
const FOREIGN_STORED: [u8; 45] = [
    0x01, 0x28, 0x00, 0xD7, 0xFF, 0x73, 0x74, 0x6F, 0x72, 0x65, 0x64, 0x20, 0x62, 0x6C, 0x6F, 0x63,
    0x6B, 0x73, 0x20, 0x63, 0x61, 0x72, 0x72, 0x79, 0x20, 0x74, 0x68, 0x65, 0x69, 0x72, 0x20, 0x62,
    0x79, 0x74, 0x65, 0x73, 0x20, 0x76, 0x65, 0x72, 0x62, 0x61, 0x74, 0x69, 0x6D,
];

/// zlib level 9 over 20 KiB with a repeat at either end, so the match at the
/// far end carries a distance near the top of the alphabet.
const FOREIGN_FAR: [u8; 445] = [
    0xED, 0xDC, 0x67, 0x43, 0x8C, 0x01, 0x00, 0xC0, 0x71, 0x94, 0x95, 0x99, 0x52, 0x76, 0x94, 0x8C,
    0xB8, 0x24, 0x33, 0x19, 0x1D, 0x9E, 0xEA, 0x5C, 0x77, 0xEE, 0x9E, 0x7A, 0xD0, 0x32, 0x2A, 0x65,
    0x13, 0x45, 0x65, 0x64, 0x95, 0xAD, 0x49, 0x5B, 0xCA, 0x88, 0xCC, 0xA2, 0x28, 0x42, 0x43, 0xCB,
    0xA8, 0xEC, 0x3D, 0xB3, 0x65, 0x97, 0xF1, 0xCA, 0x17, 0xF0, 0x01, 0xBC, 0xF8, 0x7F, 0x84, 0xDF,
    0x17, 0xF8, 0x49, 0x6A, 0x85, 0x56, 0x12, 0x64, 0x2A, 0xB9, 0xA8, 0x14, 0x44, 0x99, 0xC6, 0x51,
    0x94, 0x3B, 0x0B, 0x8D, 0x9A, 0xB7, 0xED, 0xD8, 0xCD, 0xCC, 0x62, 0xF0, 0xC8, 0x71, 0xF6, 0x2A,
    0x17, 0x77, 0xAF, 0xB9, 0x4B, 0x56, 0xAC, 0x09, 0xDB, 0x1E, 0x93, 0x98, 0x76, 0xE8, 0x44, 0x6E,
    0x41, 0xC9, 0x95, 0x1B, 0xF7, 0x9F, 0xBD, 0xF9, 0x54, 0xDF, 0xB8, 0x45, 0x3B, 0xA3, 0xEE, 0xBD,
    0x07, 0x58, 0xDB, 0xD8, 0x39, 0xA8, 0x25, 0x0F, 0xEF, 0x79, 0xFE, 0x41, 0xA1, 0xE1, 0x3B, 0x62,
    0x93, 0xD2, 0x0F, 0x9F, 0x3C, 0x73, 0xE1, 0xF2, 0xD5, 0x9B, 0x0F, 0x9E, 0xBF, 0xFD, 0xDC, 0xD0,
    0xA4, 0x65, 0x7B, 0xE3, 0x1E, 0xE6, 0x03, 0x87, 0x8C, 0x92, 0x3B, 0x4E, 0x9E, 0xE2, 0xE9, 0x33,
    0x7F, 0x69, 0xF0, 0xDA, 0x4D, 0x3B, 0x77, 0x25, 0xEF, 0xCB, 0xCC, 0x3A, 0x7B, 0xB1, 0xF4, 0xDA,
    0xAD, 0x87, 0x2F, 0xDE, 0x7D, 0xF9, 0xA9, 0xA3, 0xA7, 0xDF, 0xC9, 0xA4, 0x8F, 0x6C, 0xA8, 0xED,
    0x78, 0x85, 0x66, 0xEA, 0xF4, 0xD9, 0x0B, 0x96, 0x85, 0xAC, 0xDB, 0x1C, 0xB1, 0x3B, 0x65, 0xFF,
    0x91, 0xEC, 0xBC, 0x4B, 0x65, 0xD7, 0x6F, 0x3F, 0x7A, 0xF9, 0xFE, 0xEB, 0x2F, 0xDD, 0x56, 0x1D,
    0x3A, 0xF7, 0xEC, 0x6B, 0x39, 0x6C, 0xF4, 0x84, 0x49, 0xDA, 0x69, 0x33, 0x7C, 0x17, 0x06, 0xAC,
    0x5C, 0xBF, 0x25, 0x32, 0x6E, 0xCF, 0x81, 0xA3, 0xA7, 0xF2, 0x0B, 0xCB, 0xAB, 0xEE, 0x3C, 0xAE,
    0xFD, 0xF0, 0xED, 0x77, 0xD3, 0xD6, 0x06, 0x5D, 0x7A, 0xF5, 0x1B, 0x34, 0x7C, 0xCC, 0x44, 0xA5,
    0xE8, 0x3A, 0xD3, 0x6F, 0x51, 0xE0, 0xAA, 0x0D, 0x5B, 0xA3, 0xE2, 0x53, 0x0F, 0x1E, 0x3B, 0x7D,
    0xAE, 0xA8, 0xA2, 0xFA, 0xEE, 0x93, 0x57, 0x1F, 0xBF, 0xFF, 0x69, 0xD6, 0xC6, 0xB0, 0xAB, 0x69,
    0x7F, 0xAB, 0x11, 0x63, 0x05, 0x27, 0x67, 0xB7, 0x59, 0x73, 0x16, 0x2F, 0x5F, 0xBD, 0x71, 0x5B,
    0x74, 0xC2, 0xDE, 0x8C, 0xE3, 0x39, 0xE7, 0x8B, 0x2B, 0x6B, 0xEE, 0x3D, 0x7D, 0x5D, 0xF7, 0x03,
    0x3A, 0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87, 0x0E, 0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87,
    0x0E, 0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87, 0x0E, 0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xA1,
    0x43, 0x87, 0x0E, 0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87, 0x0E, 0x1D, 0x3A, 0x74, 0xE8,
    0xD0, 0xA1, 0x43, 0x87, 0x0E, 0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87, 0x0E, 0x1D, 0x3A,
    0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87, 0x0E, 0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xA1, 0x43, 0x87, 0x0E,
    0x1D, 0x3A, 0x74, 0xE8, 0xD0, 0xFF, 0x2B, 0xBA, 0xF4, 0x8F, 0x33, 0xEE, 0x2F,
];

/// The plaintext [`FOREIGN_FIXED`] decodes to.
fn foreign_fixed_plain() -> Vec<u8> {
    b"abracadabra abracadabra".to_vec()
}

/// The plaintext [`FOREIGN_DYNAMIC`] decodes to.
fn foreign_dynamic_plain() -> Vec<u8> {
    let mut plain = b"the rain in spain falls mainly on the plain; ".repeat(12);
    plain.extend(60u8..120);
    plain
}

/// The plaintext [`FOREIGN_STORED`] decodes to.
fn foreign_stored_plain() -> Vec<u8> {
    b"stored blocks carry their bytes verbatim".to_vec()
}

/// The plaintext [`FOREIGN_FAR`] decodes to.
fn foreign_far_plain() -> Vec<u8> {
    let mut plain = b"UNIQUE-MARKER-PHRASE".to_vec();
    plain.extend((0u32..20_000).map(|index| u8::try_from((index * 7) % 251).unwrap_or(0)));
    plain.extend_from_slice(b"UNIQUE-MARKER-PHRASE");
    plain
}

#[test]
fn foreign_fixed_huffman_stream_decodes() {
    let plain = foreign_fixed_plain();
    let mut out = vec![0u8; plain.len()];
    let produced = inflate_into(&FOREIGN_FIXED, &mut out).expect("decodes");
    assert_eq!(&out[..produced], &plain[..]);
}

#[test]
fn foreign_dynamic_huffman_stream_decodes() {
    let plain = foreign_dynamic_plain();
    let mut out = vec![0u8; plain.len()];
    let produced = inflate_into(&FOREIGN_DYNAMIC, &mut out).expect("decodes");
    assert_eq!(&out[..produced], &plain[..]);
}

#[test]
fn foreign_stored_stream_decodes() {
    let plain = foreign_stored_plain();
    let mut out = vec![0u8; plain.len()];
    let produced = inflate_into(&FOREIGN_STORED, &mut out).expect("decodes");
    assert_eq!(&out[..produced], &plain[..]);
}

#[test]
fn foreign_long_distance_match_decodes() {
    let plain = foreign_far_plain();
    let mut out = vec![0u8; plain.len()];
    let produced = inflate_into(&FOREIGN_FAR, &mut out).expect("decodes");
    assert_eq!(&out[..produced], &plain[..]);
}

#[test]
fn a_foreign_stream_decodes_the_same_however_it_is_chopped_up() {
    // Feeding one byte at a time exercises every suspension point the
    // machine has, including the ones inside a Huffman code.
    let plain = foreign_dynamic_plain();
    let mut decoder = Box::new(Inflater::new());
    let mut got = Vec::new();
    for byte in FOREIGN_DYNAMIC {
        let mut pending: &[u8] = &[byte];
        while !pending.is_empty() {
            let mut out = [0u8; 64];
            let progress = decoder.inflate(pending, &mut out).expect("decodes");
            got.extend_from_slice(&out[..progress.produced]);
            assert!(
                progress.consumed > 0 || progress.produced > 0,
                "each call makes progress"
            );
            pending = &pending[progress.consumed..];
        }
    }
    assert!(decoder.is_finished());
    assert_eq!(got, plain);
}

#[test]
fn a_back_reference_reaches_into_an_earlier_call() {
    // The window, not this call's output, is what makes the far match at the
    // end of `FOREIGN_FAR` resolvable when the output is delivered piecemeal.
    let plain = foreign_far_plain();
    let mut decoder = Box::new(Inflater::new());
    let mut got = Vec::new();
    let mut offset = 0usize;
    while !decoder.is_finished() {
        let mut out = [0u8; 1024];
        let progress = decoder
            .inflate(&FOREIGN_FAR[offset..], &mut out)
            .expect("decodes");
        offset += progress.consumed;
        got.extend_from_slice(&out[..progress.produced]);
        assert!(
            progress.produced > 0 || progress.finished,
            "the decoder must make progress"
        );
    }
    assert_eq!(got, plain);
}

#[test]
fn a_finished_stream_refuses_more_input() {
    let mut decoder = Box::new(Inflater::new());
    let mut out = vec![0u8; foreign_fixed_plain().len()];
    let progress = decoder.inflate(&FOREIGN_FIXED, &mut out).expect("decodes");
    assert!(progress.finished);
    assert_eq!(decoder.inflate(&[0u8], &mut out), Err(Error::Finished));
    decoder.reset();
    assert!(!decoder.is_finished());
    let progress = decoder.inflate(&FOREIGN_FIXED, &mut out).expect("decodes");
    assert!(progress.finished);
}

#[test]
fn a_full_destination_suspends_rather_than_failing() {
    let plain = foreign_dynamic_plain();
    let mut decoder = Box::new(Inflater::new());
    let mut got = Vec::new();
    let mut offset = 0usize;
    while !decoder.is_finished() {
        // Seven bytes at a time lands the suspension inside literals, inside
        // matches, and inside the window-backed part of a match.
        let mut out = [0u8; 7];
        let progress = decoder
            .inflate(&FOREIGN_DYNAMIC[offset..], &mut out)
            .expect("decodes");
        offset += progress.consumed;
        got.extend_from_slice(&out[..progress.produced]);
    }
    assert_eq!(got, plain);
}
