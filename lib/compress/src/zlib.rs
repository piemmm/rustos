//! RFC 1950 zlib envelope, both directions.
//!
//! A zlib stream wraps a raw DEFLATE stream ([`crate::inflate`],
//! [`crate::deflate`]) in a 2-byte header and a 4-byte trailing checksum.
//! This is the container PNG's `IDAT` data uses (W3C PNG
//! §"Filtering"/"Compression") and the one `zlib@openssh.com` runs a whole
//! SSH session's traffic through.
//!
//! [`decompress_into`] reads a stream that arrives whole; [`Decoder`] and
//! [`Encoder`] carry a stream across calls, which is what a packetised
//! protocol needs. There is no one-shot *encode* entry point on purpose: an
//! [`Encoder`] is a few hundred kilobytes of match finder, so where it lives
//! is the caller's decision to make rather than a stack frame's.
//!
//! # Wire format
//!
//! ```text
//! [ CMF : 1 ][ FLG : 1 ][ compressed data (DEFLATE) ][ ADLER32 : 4, big-endian ]
//! ```
//!
//! The encoder emits `CINFO = 7` (the full 32 KiB window) with `FDICT`
//! clear, which is what every consumer of this module reads back.
//!
//! `CMF` (compression method and flags) packs the compression method in its
//! low nibble (must be `8`, "deflate") and `CINFO` — the base-2 log of the
//! LZ77 window size minus 8 — in its high nibble; RFC 1950 bounds `CINFO` to
//! `7` (a 32 KiB window, the largest DEFLATE window). `FLG` packs `FCHECK`
//! (chosen by the encoder so that the 16-bit big-endian value `CMF:FLG` is a
//! multiple of 31 — the header's only integrity check), `FDICT` (a preset
//! dictionary was used), and `FLEVEL` (a compression-effort hint with no
//! effect on decoding). [`decompress_into`] refuses `FDICT`: a preset
//! dictionary is an out-of-band value neither PNG nor any other TAIRiX
//! consumer of this module supplies, so a stream requesting one cannot be
//! decoded correctly and is rejected rather than silently decoded wrong.
//!
//! # Trailing-byte policy
//!
//! [`crate::inflate::inflate_into_consumed`] reports exactly how many bytes
//! of the compressed body it consumed, which is where this module expects
//! to find the big-endian Adler-32 trailer. A stream with fewer than four
//! bytes left there is refused as [`Error::MissingTrailer`] — a missing or
//! truncated checksum is never treated as "no checksum to verify". Bytes
//! after the trailer are ignored, exactly as [`crate::inflate`] ignores
//! bytes after the DEFLATE stream it decoded; `IDAT` chunk concatenation in
//! `lib/image` relies on this to frame ancillary chunks around a zlib
//! stream without this module needing to know the outer container.

use crate::deflate::{self, Deflate};
use crate::inflate::{self, Inflater};

pub use crate::deflate::Flush;
pub use crate::inflate::Progress;

/// Why zlib decoding failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// `src` was too short to even hold the 2-byte header.
    HeaderTooShort,
    /// `CMF`'s low nibble (compression method) was not `8` ("deflate").
    UnsupportedCompressionMethod,
    /// `CMF`'s high nibble (`CINFO`) exceeded `7` (a window larger than
    /// DEFLATE's maximum 32 KiB).
    WindowTooLarge,
    /// The `CMF:FLG` 16-bit value was not a multiple of 31 (`FCHECK`
    /// failed) — the header is corrupt or this is not a zlib stream.
    HeaderCheckFailed,
    /// `FDICT` was set: the stream requires a preset dictionary, which no
    /// caller of this module supplies.
    PresetDictionaryUnsupported,
    /// The wrapped DEFLATE body failed to decode.
    Body(inflate::Error),
    /// Fewer than four bytes remained after the DEFLATE body for the
    /// Adler-32 trailer.
    MissingTrailer,
    /// The trailer's Adler-32 did not match the decompressed output.
    ChecksumMismatch,
    /// Compressing the body failed.
    Deflate(deflate::Error),
    /// `dst` was shorter than [`Encoder::bound`]. Nothing was consumed and
    /// the encoder is unchanged.
    OutputOverflow,
    /// The stream has already ended; it accepts no more input.
    Finished,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::HeaderTooShort => f.write_str("zlib header is truncated"),
            Self::UnsupportedCompressionMethod => {
                f.write_str("zlib compression method is not deflate")
            }
            Self::WindowTooLarge => f.write_str("zlib window size exceeds the deflate maximum"),
            Self::HeaderCheckFailed => f.write_str("zlib header check (FCHECK) failed"),
            Self::PresetDictionaryUnsupported => {
                f.write_str("zlib preset dictionary (FDICT) is not supported")
            }
            Self::Body(inner) => write!(f, "zlib body: {inner}"),
            Self::MissingTrailer => f.write_str("zlib stream is missing its Adler-32 trailer"),
            Self::ChecksumMismatch => f.write_str("zlib Adler-32 checksum mismatch"),
            Self::Deflate(inner) => write!(f, "zlib body: {inner}"),
            Self::OutputOverflow => {
                f.write_str("destination buffer is smaller than the encoder's bound")
            }
            Self::Finished => f.write_str("zlib stream is already finished"),
        }
    }
}

/// Bytes in the fixed `CMF`/`FLG` header.
const HEADER_LEN: usize = 2;

/// Bytes in the trailing Adler-32 checksum.
const TRAILER_LEN: usize = 4;

/// The modulus Adler-32 sums are reduced under (RFC 1950 §"ADLER32
/// checksum"): the largest prime below `2^16`.
const ADLER_MOD: u32 = 65_521;

/// Bytes summed between modulus reductions.
///
/// Reducing `a`/`b` after every byte is correct but pays a division per
/// byte; both accumulators fit in a `u32` without overflowing for up to
/// [`ADLER_NMAX`] consecutive additions of a `u8` and a running sum, which
/// is the standard Adler-32 blocking bound, so batching the reduction is a
/// free win on any input long enough for it to matter.
const ADLER_NMAX: usize = 5552;

/// The `CMF`/`FLG` pair the encoder emits: deflate, a 32 KiB window, no
/// preset dictionary, and the check bits that make the pair a multiple of
/// thirty-one.
const HEADER: [u8; HEADER_LEN] = [0x78, 0x9C];

/// A running Adler-32, which a stream accumulates across calls.
#[derive(Copy, Clone)]
struct Adler {
    low: u32,
    high: u32,
}

impl Default for Adler {
    fn default() -> Self {
        Self { low: 1, high: 0 }
    }
}

impl Adler {
    /// Fold `data` in. Both accumulators are reduced every
    /// [`ADLER_NMAX`] bytes, which is as long as they can run without
    /// overflowing.
    fn update(&mut self, data: &[u8]) {
        for chunk in data.chunks(ADLER_NMAX) {
            for &byte in chunk {
                self.low += u32::from(byte);
                self.high += self.low;
            }
            self.low %= ADLER_MOD;
            self.high %= ADLER_MOD;
        }
    }

    fn finish(self) -> u32 {
        (self.high << 16) | self.low
    }
}

/// The Adler-32 checksum of `data` (RFC 1950), zlib's own trailer checksum
/// — distinct from `lib/crc32c`'s CRC-32C (a different algorithm entirely,
/// used by TAIRiX's own on-disk formats) and from `lib/image`'s private
/// CRC-32 (PNG's own, unrelated, framing checksum).
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    let mut state = Adler::default();
    state.update(data);
    state.finish()
}

/// Validate a `CMF`/`FLG` pair.
fn check_header(cmf: u8, flg: u8) -> Result<(), Error> {
    if cmf & 0x0F != 8 {
        return Err(Error::UnsupportedCompressionMethod);
    }
    if cmf >> 4 > 7 {
        return Err(Error::WindowTooLarge);
    }
    if ((u16::from(cmf) << 8) | u16::from(flg)) % 31 != 0 {
        return Err(Error::HeaderCheckFailed);
    }
    if flg & 0x20 != 0 {
        return Err(Error::PresetDictionaryUnsupported);
    }
    Ok(())
}

/// Decompress the zlib stream in `src` into `dst`, returning the number of
/// bytes produced.
///
/// The header is validated (compression method, window size, `FCHECK`,
/// `FDICT`) before any byte of the body is touched; the wrapped DEFLATE
/// body is then decompressed exactly as [`inflate::inflate_into`] would,
/// and finally the Adler-32 trailer immediately following the body is
/// verified over exactly the bytes produced. A stream failing any of these
/// checks — including one missing its trailer outright — is refused rather
/// than accepted with an unverified body.
///
/// # Errors
///
/// See [`Error`] for every fail-closed refusal reason.
pub fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    if src.len() < HEADER_LEN {
        return Err(Error::HeaderTooShort);
    }
    check_header(src[0], src[1])?;

    let body = &src[HEADER_LEN..];
    let (produced, consumed) = inflate::inflate_into_consumed(body, dst).map_err(Error::Body)?;

    let trailer_end = consumed
        .checked_add(TRAILER_LEN)
        .ok_or(Error::MissingTrailer)?;
    let trailer = body
        .get(consumed..trailer_end)
        .ok_or(Error::MissingTrailer)?;
    let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let actual = adler32(&dst[..produced]);
    if actual != expected {
        return Err(Error::ChecksumMismatch);
    }
    Ok(produced)
}

/// A streaming RFC 1950 encoder.
///
/// Wraps a [`Deflate`] in the envelope: the header goes out ahead of the
/// first body byte and the Adler-32 trailer after the last, with the
/// checksum accumulated across every call in between. Around 220 KiB — see
/// [`crate::deflate`] on where to keep it.
pub struct Encoder {
    body: Deflate,
    adler: Adler,
    header_sent: bool,
}

impl Default for Encoder {
    fn default() -> Self {
        Self {
            body: Deflate::new(),
            adler: Adler::default(),
            header_sent: false,
        }
    }
}

impl Encoder {
    /// A fresh encoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore this encoder to a fresh stream, reusing its allocation.
    pub fn reset(&mut self) {
        self.body.reset();
        self.adler = Adler::default();
        self.header_sent = false;
    }

    /// Whether [`Flush::Finish`] has ended this stream.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.body.is_finished()
    }

    /// An upper bound on the bytes [`Self::compress`] can write for
    /// `input_len` further bytes, given what this encoder already holds.
    #[must_use]
    pub fn bound(&self, input_len: usize) -> usize {
        self.body
            .bound(input_len)
            .saturating_add(HEADER_LEN)
            .saturating_add(TRAILER_LEN)
    }

    /// Compress all of `src` into `dst`, returning the bytes written.
    ///
    /// `dst` must be at least [`Self::bound`] of `src.len()`.
    ///
    /// # Errors
    ///
    /// [`Error::OutputOverflow`] for a `dst` below the bound,
    /// [`Error::Finished`] after a [`Flush::Finish`], and [`Error::Deflate`]
    /// for anything the body reports.
    pub fn compress(&mut self, src: &[u8], dst: &mut [u8], flush: Flush) -> Result<usize, Error> {
        if self.body.is_finished() {
            return Err(Error::Finished);
        }
        if dst.len() < self.bound(src.len()) {
            return Err(Error::OutputOverflow);
        }
        let mut written = 0usize;
        if !self.header_sent {
            dst[..HEADER_LEN].copy_from_slice(&HEADER);
            written = HEADER_LEN;
            self.header_sent = true;
        }
        written += self
            .body
            .deflate(src, &mut dst[written..], flush)
            .map_err(Error::Deflate)?;
        self.adler.update(src);
        if flush == Flush::Finish {
            let trailer = self.adler.finish().to_be_bytes();
            dst[written..written + TRAILER_LEN].copy_from_slice(&trailer);
            written += TRAILER_LEN;
        }
        Ok(written)
    }
}

/// A streaming RFC 1950 decoder.
///
/// Reads the header from however the first bytes arrive, decodes the body
/// across as many calls as it takes, and verifies the Adler-32 trailer once
/// the stream ends. Around 34 KiB — see [`Inflater`] on where to keep it.
pub struct Decoder {
    body: Inflater,
    adler: Adler,
    header: [u8; HEADER_LEN],
    header_have: usize,
    trailer: [u8; TRAILER_LEN],
    trailer_have: usize,
    finished: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            body: Inflater::new(),
            adler: Adler::default(),
            header: [0; HEADER_LEN],
            header_have: 0,
            trailer: [0; TRAILER_LEN],
            trailer_have: 0,
            finished: false,
        }
    }
}

impl Decoder {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore this decoder to a fresh stream, reusing its allocation.
    pub fn reset(&mut self) {
        self.body.reset();
        self.adler = Adler::default();
        self.header_have = 0;
        self.trailer_have = 0;
        self.finished = false;
    }

    /// Whether the stream ended and its checksum verified.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Decode as much of `src` into `dst` as both allow.
    ///
    /// # Errors
    ///
    /// See [`Error`]. The checksum is verified the moment the trailer is
    /// complete, and a mismatch refuses the stream rather than letting an
    /// unverified body stand.
    pub fn decompress(&mut self, src: &[u8], dst: &mut [u8]) -> Result<Progress, Error> {
        if self.finished {
            return Err(Error::Finished);
        }
        let mut consumed = 0usize;
        while self.header_have < HEADER_LEN {
            let Some(&byte) = src.get(consumed) else {
                return Ok(Progress {
                    consumed,
                    produced: 0,
                    finished: false,
                });
            };
            self.header[self.header_have] = byte;
            self.header_have += 1;
            consumed += 1;
        }
        check_header(self.header[0], self.header[1])?;

        let mut produced = 0usize;
        if !self.body.is_finished() {
            let progress = self
                .body
                .inflate(&src[consumed..], dst)
                .map_err(Error::Body)?;
            consumed += progress.consumed;
            produced = progress.produced;
            self.adler.update(&dst[..produced]);
        }
        if self.body.is_finished() {
            while self.trailer_have < TRAILER_LEN {
                let Some(&byte) = src.get(consumed) else {
                    break;
                };
                self.trailer[self.trailer_have] = byte;
                self.trailer_have += 1;
                consumed += 1;
            }
            if self.trailer_have == TRAILER_LEN {
                if self.adler.finish() != u32::from_be_bytes(self.trailer) {
                    return Err(Error::ChecksumMismatch);
                }
                self.finished = true;
            }
        }
        Ok(Progress {
            consumed,
            produced,
            finished: self.finished,
        })
    }
}

#[cfg(test)]
#[path = "zlib_tests.rs"]
mod tests;
