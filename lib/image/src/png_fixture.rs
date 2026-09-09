//! The PNG fixture writer the decoder tests build their inputs with.
//!
//! Shared rather than private to `png_tests` because an icon container's
//! entries may be whole PNG files, so those tests need to write one too. The
//! crate ships no fixture files: every input is assembled here.

use alloc::vec;
use alloc::vec::Vec;

use crate::png::{IDAT, IEND, IHDR, PLTE, TRNS};
use crate::PNG_SIGNATURE;

/// One chunk: length, type, payload, and a real CRC-32 over the last two.
pub(crate) fn chunk(chunk_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u32::try_from(payload.len()).expect("test payload fits a u32 length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&chunk_type);
    out.extend_from_slice(payload);
    let crc = crate::crc32::crc32_of(&[&chunk_type, payload]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

pub(crate) fn ihdr_payload(
    width: u32,
    height: u32,
    bit_depth: u8,
    colour_type: u8,
    interlace: u8,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[bit_depth, colour_type, 0, 0, interlace]);
    out
}

/// Wrap `data` (raw, pre-filter-reconstruction scanline bytes) in a
/// well-formed zlib stream built entirely from STORED deflate blocks, so no
/// compressor is needed to produce a stream the crate's own zlib decoder
/// accepts.
pub(crate) fn zlib_wrap(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78u8, 0x9C];
    if data.is_empty() {
        out.push(0x01);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(!0u16).to_le_bytes());
    } else {
        let mut remaining = data;
        while !remaining.is_empty() {
            let take = remaining.len().min(65_535);
            let (block, rest) = remaining.split_at(take);
            out.push(u8::from(rest.is_empty()));
            let len = u16::try_from(take).expect("block fits a u16 length");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(block);
            remaining = rest;
        }
    }
    out.extend_from_slice(&tairix_compress::zlib::adler32(data).to_be_bytes());
    out
}

/// Assemble a minimal, well-formed PNG: signature, `IHDR`, an optional
/// `PLTE`/`tRNS`, one `IDAT` wrapping `raw_scanlines` (STORED-block zlib),
/// and `IEND`.
// The arguments are the header fields a fixture varies, and naming them at
// the call site is what makes a test readable; a struct would only move the
// same list one line up.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_png(
    width: u32,
    height: u32,
    bit_depth: u8,
    colour_type: u8,
    interlace: u8,
    palette: Option<&[u8]>,
    trns: Option<&[u8]>,
    raw_scanlines: &[u8],
) -> Vec<u8> {
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(chunk(
        IHDR,
        &ihdr_payload(width, height, bit_depth, colour_type, interlace),
    ));
    if let Some(plte) = palette {
        out.extend(chunk(PLTE, plte));
    }
    if let Some(trns) = trns {
        out.extend(chunk(TRNS, trns));
    }
    out.extend(chunk(IDAT, &zlib_wrap(raw_scanlines)));
    out.extend(chunk(IEND, &[]));
    out
}
