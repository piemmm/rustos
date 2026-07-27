//! The font-service IPC protocol (`plans/FONT-SERVICE.md` FS-1): the
//! reserved rendezvous the sandboxed OS font service (`fontd`) binds, and
//! the fixed-width, fail-closed requests a text-drawing client presents to
//! obtain a glyph's coverage bitmap or the monospace cell geometry.
//!
//! Text rendering is a single, sandboxed OS resource (§16.4, §19.5): no
//! process but `fontd` holds a font face or an outline rasteriser, and a
//! client draws by asking this endpoint for the 8-bit coverage of one
//! Unicode scalar at a chosen cell height. The transport carries no font
//! bytes and no outlines — only the small coverage bitmap the client
//! blits — so a malformed face can fault only the service's sandbox, never
//! the compositor or a terminal.
//!
//! The protocol is modelled on [`crate::display_ipc`] / [`crate::mailbox_ipc`]:
//! a fixed-width [`FontRequest`] in, and a status-framed reply out. Drawing
//! text is not a security boundary, so the endpoint requires no capability
//! of its own (§5.2); the reply nonetheless validates every field and fails
//! closed on a corrupt frame. Every request and reply is versioned and
//! hashed under the same ABI discipline as the syscall table (§9) and
//! frozen on the first release — mutable now, `abi-v1` is not frozen.

use crate::le::{put_i32, put_u32, read_i32, read_u32};
use crate::Errno;

/// Reserved well-known call-endpoint id of the font service (`"FNT"`
/// hex-spelled prefix, mirroring [`crate::mailbox_ipc::MAILBOX_ENDPOINT`]'s
/// convention). Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the
/// rendezvous first would feed forged glyph coverage to the compositor and
/// every app, so only the trusted `fontd` service may bind it. One endpoint
/// serves every client — requests carry the scalar and cell height
/// in-protocol.
pub const FONT_ENDPOINT: u64 = 0x464E_5400;

/// Magic number identifying a font-service request (`"FNT1"` little-endian).
pub const FONT_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"FNT1");

/// The `font-v1` protocol version.
pub const FONT_VERSION_V1: u16 = 1;

/// Smallest cell height, in physical pixels, a client may request.
///
/// Below this a monospace glyph loses the strokes that keep it legible;
/// this mirrors the client-side clamp and bounds the reply so a hostile
/// caller cannot demand a degenerate raster. A validation bound, not a
/// capacity.
pub const FONT_MIN_CELL_HEIGHT: u32 = 8;

/// Largest cell height, in physical pixels, a client may request.
///
/// A cell this tall is already a large heading; the bound caps the coverage
/// bitmap a single request can force the service to rasterise and return, so
/// a pathological request cannot demand an unbounded raster. A validation
/// bound, not a capacity.
pub const FONT_MAX_CELL_HEIGHT: u32 = 512;

/// Largest glyph-bitmap width, in physical pixels, a reply may carry.
///
/// A glyph spans at most two monospace cells, and a cell is never wider than
/// it is tall, so a bitmap is at most twice the cell height wide. Bounding
/// against the maximum cell height caps the reply independently of the
/// requested height.
pub const FONT_MAX_GLYPH_WIDTH: u32 = 2 * FONT_MAX_CELL_HEIGHT;

/// Largest coverage payload, in bytes, a glyph reply may carry: one 8-bit
/// alpha sample per pixel of the widest, tallest permitted bitmap.
pub const FONT_MAX_COVERAGE_LEN: usize =
    (FONT_MAX_GLYPH_WIDTH as usize) * (FONT_MAX_CELL_HEIGHT as usize);

/// One font-service operation (`plans/FONT-SERVICE.md` FS-1).
///
/// A [`FontRequest::Glyph`] asks for the coverage bitmap of one Unicode
/// scalar; a [`FontRequest::Metrics`] asks for the monospace cell geometry
/// the client lays text out with. Carrying the scalar as a [`char`] makes an
/// illegal scalar unrepresentable in an accepted request: the decoder
/// rejects a surrogate or out-of-range code point before it is ever built.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FontRequest {
    /// Render the coverage of `scalar` at a monospace cell `cell_height`
    /// pixels tall.
    ///
    /// The service resolves the scalar to the covering face and returns the
    /// 8-bit coverage bitmap; a scalar no face covers renders the U+FFFD
    /// replacement glyph rather than being refused.
    Glyph {
        /// The Unicode scalar to render.
        scalar: char,
        /// The monospace cell height in physical pixels
        /// ([`FONT_MIN_CELL_HEIGHT`]..=[`FONT_MAX_CELL_HEIGHT`]).
        cell_height: u32,
    },
    /// Report the monospace cell geometry (cell width, cell height,
    /// baseline) at a cell `cell_height` pixels tall, so the client can lay
    /// text out without holding any font data.
    Metrics {
        /// The monospace cell height in physical pixels
        /// ([`FONT_MIN_CELL_HEIGHT`]..=[`FONT_MAX_CELL_HEIGHT`]).
        cell_height: u32,
    },
}

/// Wire operation discriminant of [`FontRequest::Glyph`].
const OP_GLYPH: u16 = 1;
/// Wire operation discriminant of [`FontRequest::Metrics`].
const OP_METRICS: u16 = 2;

impl FontRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), and an
    /// 8-byte operation block whose unused tail must be zero.
    pub const WIRE_LEN: usize = 16;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, FONT_REQUEST_MAGIC);
        // The version is a `u16` at offset 4; bytes 4..6.
        out[4..6].copy_from_slice(&FONT_VERSION_V1.to_le_bytes());
        match *self {
            Self::Glyph {
                scalar,
                cell_height,
            } => {
                out[6..8].copy_from_slice(&OP_GLYPH.to_le_bytes());
                put_u32(&mut out, 8, scalar as u32);
                put_u32(&mut out, 12, cell_height);
            }
            Self::Metrics { cell_height } => {
                out[6..8].copy_from_slice(&OP_METRICS.to_le_bytes());
                put_u32(&mut out, 8, cell_height);
            }
        }
        out
    }

    /// Decode a request from `bytes`, failing closed on any malformed input.
    ///
    /// The cell-height bound and the scalar validity a decoder can already
    /// see are enforced here, so no accepted request ever carries a value
    /// the service would have to re-reject structurally.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `font-v1`.
    /// * [`Errno::OutOfRange`] — an operation outside the closed set, or a
    ///   `scalar` that is not a Unicode scalar value (a surrogate or a value
    ///   past `U+10FFFF`).
    /// * [`Errno::LengthOutOfRange`] — a cell height outside
    ///   [`FONT_MIN_CELL_HEIGHT`]..=[`FONT_MAX_CELL_HEIGHT`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != FONT_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if u16::from_le_bytes([bytes[4], bytes[5]]) != FONT_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = u16::from_le_bytes([bytes[6], bytes[7]]);
        match op {
            OP_GLYPH => {
                let scalar = char::from_u32(read_u32(bytes, 8)).ok_or(Errno::OutOfRange)?;
                let cell_height = validate_cell_height(read_u32(bytes, 12))?;
                Ok(Self::Glyph {
                    scalar,
                    cell_height,
                })
            }
            OP_METRICS => {
                reserved_zero(bytes, 12)?;
                let cell_height = validate_cell_height(read_u32(bytes, 8))?;
                Ok(Self::Metrics { cell_height })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Accept a cell height only within [`FONT_MIN_CELL_HEIGHT`]..=[`FONT_MAX_CELL_HEIGHT`].
fn validate_cell_height(height: u32) -> Result<u32, Errno> {
    if (FONT_MIN_CELL_HEIGHT..=FONT_MAX_CELL_HEIGHT).contains(&height) {
        Ok(height)
    } else {
        Err(Errno::LengthOutOfRange)
    }
}

/// Refuse a request whose reserved tail (from `from` to the end of the fixed
/// frame) carries any non-zero byte — wire corruption or a smuggled field,
/// never silently ignored.
fn reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..FontRequest::WIRE_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

/// Fixed prefix of a [`FontRequest::Glyph`] reply: a status word (`0` on
/// success, else the negated [`Errno`] discriminant) followed by the
/// bitmap's width, height, and pen advance, each a little-endian `u32`. The
/// 8-bit coverage samples follow, `width * height` of them.
pub const FONT_GLYPH_REPLY_HEADER_LEN: usize = 16;

/// Largest [`FontRequest::Glyph`] reply, in bytes: the header plus the
/// widest, tallest permitted coverage bitmap. A client sizes its receive
/// buffer to this.
pub const FONT_MAX_GLYPH_REPLY: usize = FONT_GLYPH_REPLY_HEADER_LEN + FONT_MAX_COVERAGE_LEN;

/// A decoded glyph-coverage reply: the bitmap geometry and a borrowed view
/// of its `width * height` 8-bit alpha samples, row-major, that the client
/// blits.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GlyphCoverage<'a> {
    /// Bitmap width in pixels (`1..=FONT_MAX_GLYPH_WIDTH`).
    pub width: u32,
    /// Bitmap height in pixels ([`FONT_MIN_CELL_HEIGHT`]..=[`FONT_MAX_CELL_HEIGHT`]).
    pub height: u32,
    /// The pen advance for this glyph in pixels (one or two cell widths).
    pub advance: u32,
    /// The `width * height` row-major 8-bit coverage samples.
    pub coverage: &'a [u8],
}

/// Encode a successful glyph-coverage reply into `buf`, returning the number
/// of bytes written.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — a `width` outside `1..=FONT_MAX_GLYPH_WIDTH`,
///   a `height` outside [`FONT_MIN_CELL_HEIGHT`]..=[`FONT_MAX_CELL_HEIGHT`],
///   an `advance` outside `1..=FONT_MAX_GLYPH_WIDTH`, or a `coverage` whose
///   length is not exactly `width * height`.
/// * [`Errno::BufferTooSmall`] — `buf` cannot hold the header plus coverage.
pub fn encode_glyph_reply(
    buf: &mut [u8],
    width: u32,
    height: u32,
    advance: u32,
    coverage: &[u8],
) -> Result<usize, Errno> {
    let len = glyph_coverage_len(width, height, advance)?;
    if coverage.len() != len {
        return Err(Errno::LengthOutOfRange);
    }
    let total = FONT_GLYPH_REPLY_HEADER_LEN + len;
    if buf.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    put_u32(buf, 4, width);
    put_u32(buf, 8, height);
    put_u32(buf, 12, advance);
    buf[FONT_GLYPH_REPLY_HEADER_LEN..total].copy_from_slice(coverage);
    Ok(total)
}

/// Encode a fail-closed glyph-reply refusal (a status word only) into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold the status word.
pub fn encode_glyph_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(4)
}

/// Decode a glyph-coverage reply, borrowing its coverage bytes from `reply`.
///
/// # Errors
///
/// * The carried [`Errno`] when the service refused the request.
/// * [`Errno::BufferTooSmall`] — `reply` is shorter than the status word, or
///   shorter than the header plus the coverage its geometry implies (a
///   truncated frame is refused, never read past its bytes).
/// * [`Errno::OutOfRange`] — a positive or undefined status word (wire
///   corruption — fail closed).
/// * [`Errno::LengthOutOfRange`] — a geometry outside the permitted bounds.
pub fn decode_glyph_reply(reply: &[u8]) -> Result<GlyphCoverage<'_>, Errno> {
    if reply.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    let status = read_i32(reply, 0);
    if status != 0 {
        let errno = status
            .checked_neg()
            .and_then(Errno::from_i32)
            .ok_or(Errno::OutOfRange)?;
        return Err(errno);
    }
    if reply.len() < FONT_GLYPH_REPLY_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let width = read_u32(reply, 4);
    let height = read_u32(reply, 8);
    let advance = read_u32(reply, 12);
    let len = glyph_coverage_len(width, height, advance)?;
    let total = FONT_GLYPH_REPLY_HEADER_LEN + len;
    if reply.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    Ok(GlyphCoverage {
        width,
        height,
        advance,
        coverage: &reply[FONT_GLYPH_REPLY_HEADER_LEN..total],
    })
}

/// Validate a glyph geometry and return the coverage length (`width *
/// height`) it implies. The bounds are the same on both the encode and
/// decode sides, so producer and consumer can never disagree on what a
/// well-formed reply looks like.
fn glyph_coverage_len(width: u32, height: u32, advance: u32) -> Result<usize, Errno> {
    if width == 0 || width > FONT_MAX_GLYPH_WIDTH {
        return Err(Errno::LengthOutOfRange);
    }
    if !(FONT_MIN_CELL_HEIGHT..=FONT_MAX_CELL_HEIGHT).contains(&height) {
        return Err(Errno::LengthOutOfRange);
    }
    if advance == 0 || advance > FONT_MAX_GLYPH_WIDTH {
        return Err(Errno::LengthOutOfRange);
    }
    // Each factor is bounded well under `u32::MAX`, so the product fits a
    // `usize` on every target (including 32-bit `wasm32`).
    Ok((width as usize) * (height as usize))
}

/// The monospace cell geometry a client needs to lay text out at a chosen
/// cell height, obtained through [`FontRequest::Metrics`].
///
/// A cell is `cell_width` by `cell_height` pixels and its glyphs sit on a
/// baseline `baseline` rows below the cell top. These three values derive
/// every layout metric the client uses (pen advance, line height, baseline),
/// so no client holds any font data of its own.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FontMetrics {
    /// Cell width (and pen advance) in pixels.
    pub cell_width: u32,
    /// Cell height (and line height) in pixels.
    pub cell_height: u32,
    /// Baseline row within the cell, in pixels below the cell top.
    pub baseline: u32,
}

/// Reply length, in bytes, of a [`FontRequest::Metrics`]: the status word
/// followed by cell width, cell height, and baseline, each a little-endian
/// `u32`.
pub const FONT_METRICS_REPLY_LEN: usize = 16;

/// Encode a metrics outcome: the [`FONT_METRICS_REPLY_LEN`]-byte reply on
/// success, the status word (a negative [`Errno`] discriminant, zero-padded
/// to the same length) on refusal. Padding the refusal keeps the reply
/// length constant, so a client always issues one fixed-size receive.
#[must_use]
pub fn encode_metrics_reply(result: Result<FontMetrics, Errno>) -> [u8; FONT_METRICS_REPLY_LEN] {
    let mut out = [0u8; FONT_METRICS_REPLY_LEN];
    match result {
        Ok(metrics) => {
            put_u32(&mut out, 4, metrics.cell_width);
            put_u32(&mut out, 8, metrics.cell_height);
            put_u32(&mut out, 12, metrics.baseline);
        }
        Err(err) => {
            out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err)));
        }
    }
    out
}

/// Decode a [`FontRequest::Metrics`] reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word (fail closed).
/// * [`Errno::LengthOutOfRange`] — a nonsensical geometry (a cell height out
///   of range, a zero or too-wide cell width, or a baseline below the cell).
/// * The decoded [`Errno`] itself, when the service refused the request.
pub fn decode_metrics_reply(bytes: &[u8]) -> Result<FontMetrics, Errno> {
    if bytes.len() < FONT_METRICS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    let cell_width = read_u32(bytes, 4);
    let cell_height = read_u32(bytes, 8);
    let baseline = read_u32(bytes, 12);
    if !(FONT_MIN_CELL_HEIGHT..=FONT_MAX_CELL_HEIGHT).contains(&cell_height) {
        return Err(Errno::LengthOutOfRange);
    }
    if cell_width == 0 || cell_width > FONT_MAX_GLYPH_WIDTH {
        return Err(Errno::LengthOutOfRange);
    }
    if baseline > cell_height {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(FontMetrics {
        cell_width,
        cell_height,
        baseline,
    })
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::{
        decode_glyph_reply, decode_metrics_reply, encode_glyph_error_reply, encode_glyph_reply,
        encode_metrics_reply, FontMetrics, FontRequest, FONT_ENDPOINT, FONT_GLYPH_REPLY_HEADER_LEN,
        FONT_MAX_CELL_HEIGHT, FONT_MAX_GLYPH_REPLY, FONT_MAX_GLYPH_WIDTH, FONT_METRICS_REPLY_LEN,
        FONT_MIN_CELL_HEIGHT, FONT_REQUEST_MAGIC,
    };
    use crate::Errno;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn magic_and_endpoint_are_frozen() {
        assert_eq!(FONT_REQUEST_MAGIC, u32::from_le_bytes(*b"FNT1"));
        assert_eq!(FONT_ENDPOINT, 0x464E_5400);
        assert!(crate::ipc::is_reserved_endpoint(FONT_ENDPOINT));
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            FontRequest::Glyph {
                scalar: 'A',
                cell_height: 28,
            },
            FontRequest::Glyph {
                scalar: '\u{FFFD}',
                cell_height: FONT_MIN_CELL_HEIGHT,
            },
            FontRequest::Glyph {
                scalar: '\u{10FFFF}',
                cell_height: FONT_MAX_CELL_HEIGHT,
            },
            FontRequest::Metrics { cell_height: 16 },
        ] {
            let bytes = request.to_le_bytes();
            assert_eq!(FontRequest::from_bytes(&bytes), Ok(request));
        }
    }

    #[test]
    fn request_decode_fails_closed_on_malformed_framing() {
        let good = FontRequest::Glyph {
            scalar: 'x',
            cell_height: 20,
        }
        .to_le_bytes();

        assert_eq!(
            FontRequest::from_bytes(&good[..FontRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(FontRequest::from_bytes(&bad_magic), Err(Errno::BadMagic));
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            FontRequest::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        let mut bad_op = good;
        bad_op[6] = 9;
        assert_eq!(FontRequest::from_bytes(&bad_op), Err(Errno::OutOfRange));
    }

    #[test]
    fn request_decode_rejects_a_non_scalar_and_a_bad_cell_height() {
        // A UTF-16 surrogate (U+D800) is not a Unicode scalar value.
        let mut surrogate = FontRequest::Glyph {
            scalar: 'A',
            cell_height: 20,
        }
        .to_le_bytes();
        surrogate[8..12].copy_from_slice(&0xD800u32.to_le_bytes());
        assert_eq!(FontRequest::from_bytes(&surrogate), Err(Errno::OutOfRange));

        // A value past U+10FFFF is not a scalar either.
        let mut past_max = surrogate;
        past_max[8..12].copy_from_slice(&0x11_0000u32.to_le_bytes());
        assert_eq!(FontRequest::from_bytes(&past_max), Err(Errno::OutOfRange));

        // Cell heights outside the permitted band are refused on both arms.
        for op in [
            FontRequest::Glyph {
                scalar: 'A',
                cell_height: FONT_MIN_CELL_HEIGHT - 1,
            },
            FontRequest::Glyph {
                scalar: 'A',
                cell_height: FONT_MAX_CELL_HEIGHT + 1,
            },
            FontRequest::Metrics { cell_height: 0 },
            FontRequest::Metrics {
                cell_height: FONT_MAX_CELL_HEIGHT + 1,
            },
        ] {
            assert_eq!(
                FontRequest::from_bytes(&op.to_le_bytes()),
                Err(Errno::LengthOutOfRange)
            );
        }
    }

    #[test]
    fn request_decode_refuses_a_dirty_metrics_tail() {
        let mut metrics = FontRequest::Metrics { cell_height: 20 }.to_le_bytes();
        metrics[12] = 1;
        assert_eq!(FontRequest::from_bytes(&metrics), Err(Errno::BadMagic));
    }

    #[test]
    fn glyph_reply_round_trips_and_bounds_the_coverage() {
        let width = 6u32;
        let height = 12u32;
        let advance = 6u32;
        let coverage: Vec<u8> = (0..width * height)
            .map(|i| u8::try_from(i % 256).expect("a value modulo 256 fits a u8"))
            .collect();
        let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
        let n = encode_glyph_reply(&mut buf, width, height, advance, &coverage).expect("encodes");
        assert_eq!(n, FONT_GLYPH_REPLY_HEADER_LEN + coverage.len());

        let decoded = decode_glyph_reply(&buf[..n]).expect("decodes");
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_eq!(decoded.advance, advance);
        assert_eq!(decoded.coverage, &coverage[..]);
    }

    #[test]
    fn glyph_reply_encode_rejects_bad_geometry_and_mismatched_coverage() {
        let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
        // Coverage length must equal width * height.
        assert_eq!(
            encode_glyph_reply(&mut buf, 4, 10, 4, &[0u8; 39]),
            Err(Errno::LengthOutOfRange)
        );
        // Zero / oversized geometry is refused.
        assert_eq!(
            encode_glyph_reply(&mut buf, 0, 10, 4, &[]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            encode_glyph_reply(&mut buf, FONT_MAX_GLYPH_WIDTH + 1, 10, 4, &[]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            encode_glyph_reply(&mut buf, 4, FONT_MAX_CELL_HEIGHT + 1, 4, &[]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            encode_glyph_reply(&mut buf, 4, 10, 0, &[0u8; 40]),
            Err(Errno::LengthOutOfRange)
        );
        // A buffer too small for the framed coverage.
        let mut tiny = [0u8; FONT_GLYPH_REPLY_HEADER_LEN + 3];
        assert_eq!(
            encode_glyph_reply(&mut tiny, 2, 8, 2, &[0u8; 16]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn glyph_reply_error_frame_surfaces_its_errno() {
        let mut buf = [0u8; FONT_GLYPH_REPLY_HEADER_LEN];
        let n = encode_glyph_error_reply(&mut buf, Errno::NotFound).expect("encodes");
        assert_eq!(n, 4);
        assert_eq!(decode_glyph_reply(&buf[..n]), Err(Errno::NotFound));
    }

    #[test]
    fn glyph_reply_decode_fails_closed() {
        let width = 4u32;
        let height = 10u32;
        let coverage = vec![0xABu8; (width * height) as usize];
        let mut buf = vec![0u8; FONT_MAX_GLYPH_REPLY];
        let n = encode_glyph_reply(&mut buf, width, height, width, &coverage).expect("encodes");

        // Shorter than the status word.
        assert_eq!(decode_glyph_reply(&buf[..3]), Err(Errno::BufferTooSmall));
        // A success status but a truncated header.
        assert_eq!(
            decode_glyph_reply(&buf[..FONT_GLYPH_REPLY_HEADER_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A success status but the coverage is truncated.
        assert_eq!(
            decode_glyph_reply(&buf[..n - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A positive (corrupt) status word.
        let mut bad_status = buf.clone();
        bad_status[0] = 1;
        assert_eq!(decode_glyph_reply(&bad_status), Err(Errno::OutOfRange));
        // A success status but a nonsensical geometry.
        let mut bad_geom = buf.clone();
        bad_geom[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_glyph_reply(&bad_geom), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn metrics_reply_round_trips_ok_and_error() {
        let metrics = FontMetrics {
            cell_width: 15,
            cell_height: 28,
            baseline: 23,
        };
        assert_eq!(
            decode_metrics_reply(&encode_metrics_reply(Ok(metrics))),
            Ok(metrics)
        );
        assert_eq!(
            decode_metrics_reply(&encode_metrics_reply(Err(Errno::NotFound))),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn metrics_reply_decode_fails_closed() {
        let good = encode_metrics_reply(Ok(FontMetrics {
            cell_width: 15,
            cell_height: 28,
            baseline: 23,
        }));

        assert_eq!(
            decode_metrics_reply(&good[..FONT_METRICS_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A corrupt (positive) status word.
        let mut bad_status = good;
        bad_status[0] = 1;
        assert_eq!(decode_metrics_reply(&bad_status), Err(Errno::OutOfRange));
        // A cell height out of range.
        let mut bad_height = good;
        bad_height[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            decode_metrics_reply(&bad_height),
            Err(Errno::LengthOutOfRange)
        );
        // A baseline below the cell.
        let mut bad_baseline = good;
        bad_baseline[12..16].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(
            decode_metrics_reply(&bad_baseline),
            Err(Errno::LengthOutOfRange)
        );
    }
}
