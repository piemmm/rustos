//! Unit tests for the RFC 1950 zlib envelope decoder.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use super::{adler32, decompress_into, Error};
use crate::inflate;

/// A raw DEFLATE stream holding one final stored block of `data` — enough
/// to exercise the envelope without needing a Huffman-coded body.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x01u8]; // BFINAL = 1, BTYPE = 00 (stored)
    let len = u16::try_from(data.len()).expect("test data fits a u16 length");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// A well-formed zlib stream wrapping `data` through a stored DEFLATE
/// block: the common `CMF = 0x78, FLG = 0x9C` header (`(0x78 << 8 | 0x9C)
/// % 31 == 0`, `CM = 8`, `CINFO = 7`, `FDICT` clear) plus a correct
/// trailing Adler-32.
fn zlib_stream(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78u8, 0x9C];
    out.extend_from_slice(&deflate_stored(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn decode(src: &[u8], dst_len: usize) -> Result<Vec<u8>, Error> {
    let mut dst = vec![0u8; dst_len];
    let n = decompress_into(src, &mut dst)?;
    dst.truncate(n);
    Ok(dst)
}

// ---- adler32 --------------------------------------------------------

#[test]
fn adler32_of_empty_input_is_one() {
    assert_eq!(adler32(b""), 1);
}

#[test]
fn adler32_matches_the_known_answer_vector() {
    // The canonical worked example (Wikipedia's own Adler-32 article uses
    // its own name as the example input).
    assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
}

#[test]
fn adler32_spans_the_blocking_boundary() {
    // Exercise the >NMAX blocking path in the implementation with a buffer
    // that crosses it, checked against the same byte-by-byte definition
    // computed without blocking.
    let data: Vec<u8> = (0..20_000u32)
        .map(|i| u8::try_from(i % 256).unwrap_or(0))
        .collect();
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in &data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    assert_eq!(adler32(&data), (b << 16) | a);
}

// ---- decompress_into --------------------------------------------------

#[test]
fn decompress_round_trips_a_stored_block() {
    let data = b"zlib wraps a raw deflate stream";
    let src = zlib_stream(data);
    assert_eq!(decode(&src, data.len()), Ok(data.to_vec()));
}

#[test]
fn header_too_short_is_refused() {
    assert_eq!(decode(&[0x78], 8), Err(Error::HeaderTooShort));
    assert_eq!(decode(&[], 8), Err(Error::HeaderTooShort));
}

#[test]
fn unsupported_compression_method_is_refused() {
    // CM = 7, not 8 ("deflate").
    let src = vec![0x77u8, 0x00];
    assert_eq!(decode(&src, 8), Err(Error::UnsupportedCompressionMethod));
}

#[test]
fn window_too_large_is_refused() {
    // CM = 8, CINFO = 8 (exceeds the DEFLATE-maximum CINFO of 7).
    let src = vec![0x88u8, 0x00];
    assert_eq!(decode(&src, 8), Err(Error::WindowTooLarge));
}

#[test]
fn header_check_failure_is_refused() {
    // CM = 8, CINFO = 7, but FLG chosen so CMF:FLG is not a multiple of 31.
    let src = vec![0x78u8, 0x00];
    assert_eq!(decode(&src, 8), Err(Error::HeaderCheckFailed));
}

#[test]
fn preset_dictionary_is_refused() {
    // CMF = 0x78, FLG = 0x20: FDICT set, and 0x7820 is still a multiple of
    // 31 (0x7800 % 31 == 30, and 0x20 == 32 == 31 + 1, completing it).
    let src = vec![0x78u8, 0x20];
    assert_eq!(decode(&src, 8), Err(Error::PresetDictionaryUnsupported));
}

#[test]
fn body_errors_are_wrapped() {
    let mut src = vec![0x78u8, 0x9C];
    src.push(0b0000_0111); // BFINAL = 1, BTYPE = 11 (reserved)
    assert_eq!(
        decode(&src, 8),
        Err(Error::Body(inflate::Error::InvalidBlockType))
    );
}

#[test]
fn missing_trailer_is_refused() {
    let mut src = vec![0x78u8, 0x9C];
    src.extend_from_slice(&deflate_stored(b"abc"));
    // No Adler-32 trailer appended at all.
    assert_eq!(decode(&src, 3), Err(Error::MissingTrailer));
}

#[test]
fn truncated_trailer_is_refused() {
    let mut src = vec![0x78u8, 0x9C];
    src.extend_from_slice(&deflate_stored(b"abc"));
    src.extend_from_slice(&[0, 0, 0]); // only 3 of the 4 trailer bytes
    assert_eq!(decode(&src, 3), Err(Error::MissingTrailer));
}

#[test]
fn checksum_mismatch_is_refused() {
    let mut src = vec![0x78u8, 0x9C];
    src.extend_from_slice(&deflate_stored(b"abc"));
    src.extend_from_slice(&0u32.to_be_bytes()); // wrong Adler-32
    assert_eq!(decode(&src, 3), Err(Error::ChecksumMismatch));
}
