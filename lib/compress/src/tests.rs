//! Unit tests for the first-party LZ codec.
//!
//! The suite covers the round-trip identity, a representative corpus, fixed
//! known-answer vectors that pin the on-disk frame format, the
//! incompressible-input path `ARXFS` relies on for its raw-store fallback, and
//! the malformed-input rejection that must never panic
//! (`docs/src/filesystem/arxfs-spec.md` §10).

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use super::{compress, decompress, max_compressed_len, Error, HEADER_LEN, MAGIC};

/// Compress `input` into a generously sized scratch buffer and decompress it
/// back, asserting the round-trip is the identity.
fn round_trip(input: &[u8]) {
    let mut packed = vec![0u8; max_compressed_len(input.len())];
    let n = compress(input, &mut packed).expect("compress fits the bound buffer");
    let mut out = vec![0u8; input.len()];
    let m = decompress(&packed[..n], &mut out).expect("decompress a self-produced frame");
    assert_eq!(m, input.len(), "decompressed length matches the input");
    assert_eq!(&out[..], input, "round-trip is the identity");
}

#[test]
fn round_trip_empty() {
    round_trip(&[]);
}

#[test]
fn round_trip_single_byte() {
    round_trip(&[0x42]);
}

#[test]
fn round_trip_short_incompressible() {
    round_trip(&[1, 2, 3]);
}

#[test]
fn round_trip_highly_repetitive() {
    let input = vec![0xABu8; 9000];
    round_trip(&input);
}

#[test]
fn round_trip_repeated_pattern() {
    let mut input = Vec::new();
    while input.len() < 8000 {
        input.extend_from_slice(b"the quick brown fox ");
    }
    round_trip(&input);
}

#[test]
fn round_trip_run_length_overlap() {
    // A single byte followed by a long run exercises an overlapping
    // back-reference (offset 1, match longer than the offset).
    let mut input = vec![0x7Eu8];
    input.extend(core::iter::repeat(0x7E).take(500));
    round_trip(&input);
}

#[test]
fn corpus_round_trips_byte_identical() {
    // A small corpus of varied shapes: text, structured records, a gradient,
    // and pseudo-random noise. Each must round-trip exactly.
    let text = b"RustOS native filesystem compresses every data record. \
                 Compression is mandatory and not tunable.";
    let mut records = Vec::new();
    for i in 0..256u32 {
        records.extend_from_slice(&i.to_le_bytes());
        records.extend_from_slice(b"record");
    }
    let gradient: Vec<u8> = (0..4096u32)
        .map(|x| u8::try_from(x % 251).unwrap_or(0))
        .collect();
    let mut noise = Vec::new();
    let mut state: u32 = 0x1234_5678;
    for _ in 0..4096 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        noise.push(u8::try_from(state >> 24).unwrap_or(0));
    }

    round_trip(text);
    round_trip(&records);
    round_trip(&gradient);
    round_trip(&noise);
}

#[test]
fn known_answer_empty_frame() {
    // The empty input frames to exactly the header (magic + zero length) plus
    // one literal-only token of length zero. This pins the v1 wire format.
    let mut packed = [0u8; 32];
    let n = compress(&[], &mut packed).expect("compress empty");
    assert_eq!(n, HEADER_LEN + 1);
    assert_eq!(&packed[0..4], &MAGIC);
    assert_eq!(&packed[4..8], &[0, 0, 0, 0]);
    assert_eq!(packed[8], 0x00, "final literal-only token, length 0");
}

#[test]
fn known_answer_four_literals() {
    // Four incompressible bytes: a single token with literal nibble 4 and
    // match nibble 0, followed by the four literals.
    let input = [0xDE, 0xAD, 0xBE, 0xEF];
    let mut packed = [0u8; 32];
    let n = compress(&input, &mut packed).expect("compress four bytes");
    assert_eq!(n, HEADER_LEN + 1 + 4);
    assert_eq!(&packed[0..4], &MAGIC);
    assert_eq!(&packed[4..8], &[4, 0, 0, 0]);
    assert_eq!(packed[8], 0x40, "literal nibble 4, match nibble 0");
    assert_eq!(&packed[9..13], &input);
}

#[test]
fn incompressible_is_stored_via_literals_and_round_trips() {
    // Pseudo-random bytes do not compress; the codec must still produce a
    // valid frame that decodes byte-identically (ARXFS stores such a record
    // raw, but the codec itself must never corrupt it).
    let mut noise = Vec::new();
    let mut state: u32 = 0xC0FF_EE00;
    for _ in 0..2048 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        noise.push(u8::try_from((state >> 16) & 0xFF).unwrap_or(0));
    }
    round_trip(&noise);
}

#[test]
fn compressible_input_actually_shrinks() {
    let input = vec![0x00u8; 16_000];
    let mut packed = vec![0u8; max_compressed_len(input.len())];
    let n = compress(&input, &mut packed).expect("compress a long run");
    assert!(
        n < input.len(),
        "a long constant run must shrink: {n} >= {}",
        input.len()
    );
}

#[test]
fn compress_into_too_small_fails_closed() {
    let input = vec![0x11u8; 64];
    let mut tiny = [0u8; 4];
    assert_eq!(compress(&input, &mut tiny), Err(Error::TooSmall));
}

#[test]
fn decompress_rejects_bad_magic() {
    let mut out = [0u8; 16];
    let bad = [0u8; HEADER_LEN + 4];
    assert_eq!(decompress(&bad, &mut out), Err(Error::Corrupt));
}

#[test]
fn decompress_rejects_short_header() {
    let mut out = [0u8; 16];
    assert_eq!(decompress(&[0u8; 3], &mut out), Err(Error::Corrupt));
}

#[test]
fn decompress_rejects_destination_too_small() {
    // A valid frame declaring more output than the destination can hold is
    // refused before any byte is written (memory bounded up front).
    let input = vec![0x55u8; 1000];
    let mut packed = vec![0u8; max_compressed_len(input.len())];
    let n = compress(&input, &mut packed).expect("compress");
    let mut tiny = [0u8; 10];
    assert_eq!(decompress(&packed[..n], &mut tiny), Err(Error::TooSmall));
}

#[test]
fn decompress_rejects_truncated_stream() {
    let input = vec![0x33u8; 4096];
    let mut packed = vec![0u8; max_compressed_len(input.len())];
    let n = compress(&input, &mut packed).expect("compress");
    // Drop the trailing bytes: the frame can no longer produce its declared
    // length, so decompression must fail closed rather than panic.
    let mut out = vec![0u8; input.len()];
    assert_eq!(decompress(&packed[..n - 2], &mut out), Err(Error::Corrupt));
}

#[test]
fn decompress_never_panics_on_arbitrary_bytes() {
    // Exhaustively flip each byte of a valid frame, plus a structured set of
    // adversarial frames. Every call must return a `Result`, never panic.
    let input = b"compress me, then corrupt every byte of the frame".to_vec();
    let mut packed = vec![0u8; max_compressed_len(input.len())];
    let n = compress(&input, &mut packed).expect("compress");
    let frame = packed[..n].to_vec();

    for i in 0..frame.len() {
        for bit in 0..8u32 {
            let mut bad = frame.clone();
            bad[i] ^= 1u8 << bit;
            let mut out = vec![0u8; input.len()];
            let _ = decompress(&bad, &mut out);
        }
    }

    // A frame claiming a huge length with a back-reference token: the decoder
    // must reject the out-of-range offset, not read out of bounds.
    let mut hostile = MAGIC.to_vec();
    hostile.extend_from_slice(&100u32.to_le_bytes());
    hostile.push(0x0F); // zero literals, match nibble 15
    hostile.extend_from_slice(&[0xFF, 0xFF]); // offset 65535 into 0 bytes produced
    hostile.push(0x00);
    let mut out = vec![0u8; 100];
    assert_eq!(decompress(&hostile, &mut out), Err(Error::Corrupt));
}

#[test]
fn max_compressed_len_bounds_real_output() {
    for len in [0usize, 1, 7, 100, 255, 256, 4096, 65_536] {
        let input = vec![0x9Au8; len];
        let mut packed = vec![0u8; max_compressed_len(len)];
        // Must always fit: `max_compressed_len` is a true upper bound.
        let n = compress(&input, &mut packed).expect("fits the bound");
        assert!(n <= max_compressed_len(len));
    }
}
