//! Unit tests for the RFC 1950 zlib envelope, both directions.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use super::{adler32, decompress_into, Decoder, Encoder, Error, Flush};
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

// ---------------------------------------------------------------------------
// Foreign-encoder fixtures and the encode direction.
// ---------------------------------------------------------------------------

/// A zlib-enveloped dynamic-Huffman stream a real zlib produced at level 9.
const FOREIGN_ENVELOPE: [u8; 111] = [
    0x78, 0xDA, 0x2B, 0xC9, 0x48, 0x55, 0x28, 0x4A, 0xCC, 0xCC, 0x53, 0x00, 0xA2, 0xE2, 0x02, 0x10,
    0x23, 0x2D, 0x31, 0x27, 0xA7, 0x58, 0x21, 0x17, 0xC8, 0xCC, 0xA9, 0x54, 0xC8, 0xCF, 0x53, 0x28,
    0x01, 0xAA, 0x28, 0xC8, 0x01, 0x72, 0xAD, 0xC1, 0xCC, 0x51, 0xC5, 0x23, 0x46, 0xB1, 0x8D, 0xAD,
    0x9D, 0xBD, 0x83, 0xA3, 0x93, 0xB3, 0x8B, 0xAB, 0x9B, 0xBB, 0x87, 0xA7, 0x97, 0xB7, 0x8F, 0xAF,
    0x9F, 0x7F, 0x40, 0x60, 0x50, 0x70, 0x48, 0x68, 0x58, 0x78, 0x44, 0x64, 0x54, 0x74, 0x4C, 0x6C,
    0x5C, 0x7C, 0x42, 0x62, 0x52, 0x72, 0x4A, 0x6A, 0x5A, 0x7A, 0x46, 0x66, 0x56, 0x76, 0x4E, 0x6E,
    0x5E, 0x7E, 0x41, 0x61, 0x51, 0x71, 0x49, 0x69, 0x59, 0x39, 0x00, 0xCF, 0x1C, 0xD5, 0x43,
];

/// The same shape OpenSSH's `zlib@openssh.com` puts on the wire: one zlib
/// stream for the whole session, flushed with `Z_PARTIAL_FLUSH` per packet,
/// so each frame ends mid-byte and the next resumes there.
const FOREIGN_PARTIAL_FLUSH: [u8; 61] = [
    0x78, 0x9C, 0xCA, 0xC9, 0x4F, 0xCF, 0xCC, 0xB3, 0x52, 0x00, 0x08, 0xA0, 0x82, 0xC4, 0xE2, 0xE2,
    0xF2, 0xFC, 0xA2, 0x14, 0x2B, 0x05, 0x80, 0x00, 0x52, 0x51, 0xC8, 0x29, 0x56, 0xD0, 0xCD, 0xE1,
    0xE5, 0x02, 0x08, 0xA0, 0x92, 0xFC, 0x92, 0xC4, 0x1C, 0x05, 0x03, 0x5E, 0x2E, 0x80, 0x00, 0x82,
    0x8B, 0x00, 0x04, 0x90, 0x8A, 0x42, 0x6A, 0x45, 0x66, 0x09, 0x2F, 0x17, 0x40,
];

/// How [`FOREIGN_PARTIAL_FLUSH`] divides into per-packet frames.
const FOREIGN_FRAME_SIZES: [usize; 6] = [11, 13, 11, 12, 4, 10];

/// The plaintext each frame of [`FOREIGN_PARTIAL_FLUSH`] carries.
const FOREIGN_FRAME_PLAIN: [&[u8]; 6] = [
    b"login: ",
    b"password: ",
    b"$ ls -l\r\n",
    b"total 0\r\n",
    b"$ ls -l\r\n",
    b"$ exit\r\n",
];

/// The plaintext [`FOREIGN_ENVELOPE`] decodes to.
fn foreign_envelope_plain() -> Vec<u8> {
    let mut plain = b"the rain in spain falls mainly on the plain; ".repeat(12);
    plain.extend(60u8..120);
    plain
}

#[test]
fn a_foreign_envelope_decodes_one_shot() {
    let plain = foreign_envelope_plain();
    let mut out = vec![0u8; plain.len()];
    let produced = decompress_into(&FOREIGN_ENVELOPE, &mut out).expect("decodes");
    assert_eq!(&out[..produced], &plain[..]);
}

#[test]
fn a_foreign_envelope_decodes_streamed_a_byte_at_a_time() {
    let plain = foreign_envelope_plain();
    let mut decoder = Box::new(Decoder::new());
    let mut got = Vec::new();
    for byte in FOREIGN_ENVELOPE {
        // A single compressed byte can expand past this destination, so the
        // output is drained until the byte itself is taken.
        let mut pending: &[u8] = &[byte];
        while !pending.is_empty() {
            let mut out = [0u8; 64];
            let progress = decoder.decompress(pending, &mut out).expect("decodes");
            got.extend_from_slice(&out[..progress.produced]);
            assert!(
                progress.consumed > 0 || progress.produced > 0,
                "each call makes progress"
            );
            pending = &pending[progress.consumed..];
        }
    }
    assert!(decoder.is_finished(), "the trailer verified");
    assert_eq!(got, plain);
}

#[test]
fn an_openssh_shaped_partial_flush_stream_decodes_frame_by_frame() {
    let mut decoder = Box::new(Decoder::new());
    let mut offset = 0usize;
    for (index, size) in FOREIGN_FRAME_SIZES.into_iter().enumerate() {
        let frame = &FOREIGN_PARTIAL_FLUSH[offset..offset + size];
        offset += size;
        let mut out = [0u8; 256];
        let progress = decoder.decompress(frame, &mut out).expect("decodes");
        assert_eq!(
            progress.consumed, size,
            "frame {index} must be absorbed whole, leaving nothing to carry"
        );
        assert_eq!(&out[..progress.produced], FOREIGN_FRAME_PLAIN[index]);
        assert!(!progress.finished, "the session stream never ends");
    }
}

#[test]
fn a_corrupt_trailer_is_refused_by_the_streaming_decoder() {
    let mut damaged = FOREIGN_ENVELOPE;
    let last = damaged.len() - 1;
    damaged[last] ^= 0x01;
    let mut decoder = Box::new(Decoder::new());
    let mut out = vec![0u8; foreign_envelope_plain().len()];
    assert_eq!(
        decoder.decompress(&damaged, &mut out),
        Err(Error::ChecksumMismatch)
    );
}

#[test]
fn the_encoder_round_trips_through_the_one_shot_decoder() {
    let plain = foreign_envelope_plain();
    let mut encoder = Box::new(Encoder::new());
    let mut stream = vec![0u8; encoder.bound(plain.len())];
    let written = encoder
        .compress(&plain, &mut stream, Flush::Finish)
        .expect("fits");
    let mut out = vec![0u8; plain.len()];
    let produced = decompress_into(&stream[..written], &mut out).expect("decodes");
    assert_eq!(&out[..produced], &plain[..]);
}

#[test]
fn the_encoder_emits_a_header_the_decoder_accepts() {
    let mut encoder = Box::new(Encoder::new());
    let mut stream = vec![0u8; encoder.bound(0)];
    let written = encoder
        .compress(b"", &mut stream, Flush::Finish)
        .expect("fits");
    assert_eq!(&stream[..2], &[0x78, 0x9C], "deflate, 32 KiB window");
    let mut out = [0u8; 4];
    assert_eq!(decompress_into(&stream[..written], &mut out), Ok(0));
}

#[test]
fn a_sync_flushed_session_round_trips_through_the_streaming_pair() {
    let mut encoder = Box::new(Encoder::new());
    let mut decoder = Box::new(Decoder::new());
    for round in 0..32u32 {
        let message = match round % 3 {
            0 => b"$ ls -l /System/Commands\r\n".to_vec(),
            1 => b"total 0\r\n".to_vec(),
            _ => vec![b'.'; usize::try_from(round).unwrap_or(0) * 97],
        };
        let mut wire = vec![0u8; encoder.bound(message.len())];
        let written = encoder
            .compress(&message, &mut wire, Flush::Sync)
            .expect("fits");
        let mut out = vec![0u8; message.len() + 64];
        let progress = decoder
            .decompress(&wire[..written], &mut out)
            .expect("decodes");
        assert_eq!(
            progress.consumed, written,
            "a sync flush leaves nothing over"
        );
        assert_eq!(&out[..progress.produced], &message[..]);
    }
}

#[test]
fn the_encoder_refuses_a_destination_below_its_bound() {
    let mut encoder = Box::new(Encoder::new());
    let mut tiny = [0u8; 4];
    assert_eq!(
        encoder.compress(b"hello", &mut tiny, Flush::Finish),
        Err(Error::OutputOverflow)
    );
}

#[test]
fn a_finished_encoder_and_decoder_both_refuse_more() {
    let mut encoder = Box::new(Encoder::new());
    let mut stream = vec![0u8; encoder.bound(2)];
    let written = encoder
        .compress(b"hi", &mut stream, Flush::Finish)
        .expect("fits");
    assert!(encoder.is_finished());
    assert_eq!(
        encoder.compress(b"hi", &mut stream, Flush::Finish),
        Err(Error::Finished)
    );

    let mut decoder = Box::new(Decoder::new());
    let mut out = [0u8; 8];
    let progress = decoder
        .decompress(&stream[..written], &mut out)
        .expect("decodes");
    assert!(progress.finished);
    assert_eq!(decoder.decompress(&[0u8], &mut out), Err(Error::Finished));
}
