//! A deterministic 8-bit RGBA PNG encoder, for the contact sheets.
//!
//! Host tool code: only the PNG framing lives here. `lib/image` decodes PNG
//! and deliberately exposes no encoder, but the compressed stream inside an
//! `IDAT` is an ordinary zlib stream, so it comes from `lib/compress` rather
//! than from a second copy of a compressor here. Every byte is a function of
//! the pixels, so two runs over the same frame produce the same file.
//!
//! It is proven rather than eyeballed: the tests round-trip what it writes
//! through `lib/image`'s own decoder, which is the decoder the desktop uses.

use tairix_compress::zlib::{Encoder, Flush};

/// Encode `rgba` — row-major, straight alpha, exactly `width * height * 4`
/// bytes — as a PNG.
///
/// # Errors
///
/// A geometry that is zero, that overflows, or that does not match the
/// pixel buffer it was handed.
pub fn encode(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(usize::try_from(height).ok()?))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "artsheet: a picture that large could not be encoded".to_owned())?;
    if width == 0 || height == 0 || expected != rgba.len() {
        return Err(format!(
            "artsheet: {width}x{height} wants {expected} bytes, got {}",
            rgba.len()
        ));
    }

    let mut out = Vec::with_capacity(rgba.len() + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // Eight bits per channel, truecolour with alpha, deflate, no filtering,
    // no interlace.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, *b"IHDR", &header);

    // Each row is prefixed with its filter type. Zero — "None": a contact
    // sheet is looked at once and regenerated, so choosing a predictor per
    // row would cost more than the bytes it saves.
    let stride = usize::try_from(width)
        .unwrap_or(usize::MAX)
        .saturating_mul(4);
    let mut raw = Vec::with_capacity(rgba.len() + rgba.len() / stride.max(1) + 1);
    for row in rgba.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    chunk(&mut out, *b"IDAT", &zlib_stream(&raw)?);
    chunk(&mut out, *b"IEND", &[]);
    Ok(out)
}

/// Append one PNG chunk: its length, its type, its data, and their CRC.
fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let mut crc = Crc::new();
    crc.eat(&kind);
    crc.eat(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// `data` as one finished zlib stream.
///
/// The encoder is a few hundred kilobytes of match finder, so it is boxed
/// rather than built on this frame.
fn zlib_stream(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = Box::new(Encoder::new());
    let mut out = vec![0u8; encoder.bound(data.len())];
    let written = encoder
        .compress(data, &mut out, Flush::Finish)
        .map_err(|error| format!("artsheet: compressing the picture failed: {error}"))?;
    out.truncate(written);
    Ok(out)
}

/// CRC-32/ISO-HDLC, the one PNG chunks carry.
struct Crc(u32);

impl Crc {
    const fn new() -> Self {
        Self(u32::MAX)
    }

    fn eat(&mut self, bytes: &[u8]) {
        /// The reflected generator polynomial PNG names.
        const POLY: u32 = 0xEDB8_8320;
        for &byte in bytes {
            self.0 ^= u32::from(byte);
            for _ in 0..8 {
                let carry = self.0 & 1;
                self.0 >>= 1;
                if carry != 0 {
                    self.0 ^= POLY;
                }
            }
        }
    }

    const fn finish(self) -> u32 {
        self.0 ^ u32::MAX
    }
}

#[cfg(test)]
#[path = "png_tests.rs"]
mod tests;
