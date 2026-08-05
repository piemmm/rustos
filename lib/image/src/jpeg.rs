//! A complete, fail-closed JPEG decoder (ITU-T T.81), framed as JFIF or
//! Adobe.
//!
//! Supported: baseline sequential (SOF0), extended sequential (SOF1), and
//! progressive (SOF2) DCT frames, all Huffman-coded, at 8-bit sample
//! precision; 1-component greyscale and 3-component YCbCr, plus RGB when
//! an Adobe APP14 marker declares a colour transform of zero; any
//! per-component sampling factor from 1 to 4; up to four DC and four AC
//! Huffman tables; 8-bit and 16-bit quantisation tables; restart markers;
//! and multi-scan streams. Everything else — arithmetic coding, lossless
//! and hierarchical frames, 12-bit precision, 2- or 4-component images,
//! and a deferred height via `DNL` — is a typed, fail-closed refusal
//! rather than a best-effort guess.
//!
//! # Decoding shape
//!
//! The stream is a sequence of markers (ITU-T T.81 Annex B): tables
//! (`DQT`, `DHT`, `DRI`), exactly one frame header (`SOF0`/`SOF1`/`SOF2`),
//! and one or more scans (`SOS` followed by its entropy-coded data). A
//! non-progressive frame's scans always cover the whole coefficient
//! spectrum for the components they list, so this decoder streams each
//! scan straight into per-component sample planes and never holds more
//! than one block's coefficients at a time. A progressive frame's scans
//! instead each carry a partial spectral band or a successive-approximation
//! refinement (Annex G), so nothing can be reconstructed until every scan
//! has been read: this decoder buffers every component's every block's
//! coefficients — bounded by
//! [`crate::DecodeLimits::max_progressive_coefficient_bytes`] — and only
//! dequantises and inverse-transforms them once, after the last scan.
//!
//! Every declared size — a segment length, a table length, a scan's
//! component count — is validated against the bytes actually available,
//! or against a value computed purely from already-bounded geometry,
//! before it is used to allocate or index anything; see the crate
//! documentation for the full bounds policy.

use alloc::vec;
use alloc::vec::Vec;

use crate::{DecodeError, DecodeLimits, FitBox, RasterImage};

// ---------------------------------------------------------------------
// Marker codes (ITU-T T.81 Table B.1)
// ---------------------------------------------------------------------

const SOF0: u8 = 0xC0;
const SOF1: u8 = 0xC1;
const SOF2: u8 = 0xC2;
const SOF3: u8 = 0xC3;
const DHT: u8 = 0xC4;
const SOF5: u8 = 0xC5;
const SOF6: u8 = 0xC6;
const SOF7: u8 = 0xC7;
const SOF9: u8 = 0xC9;
const SOF10: u8 = 0xCA;
const SOF11: u8 = 0xCB;
const DAC: u8 = 0xCC;
const SOF13: u8 = 0xCD;
const SOF14: u8 = 0xCE;
const SOF15: u8 = 0xCF;
const RST0: u8 = 0xD0;
const RST7: u8 = 0xD7;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DNL: u8 = 0xDC;
const DRI: u8 = 0xDD;
const DHP: u8 = 0xDE;
const EXP: u8 = 0xDF;
const APP0: u8 = 0xE0;
const APP14: u8 = 0xEE;
const APP15: u8 = 0xEF;
const COM: u8 = 0xFE;

/// Whether `marker` is a `RSTn` restart marker, and if so its cyclic
/// sequence number (`0..=7`).
const fn restart_index(marker: u8) -> Option<u8> {
    if marker >= RST0 && marker <= RST7 {
        Some(marker - RST0)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// The standard zig-zag scan order and the fixed-point inverse-DCT basis
// ---------------------------------------------------------------------

/// The natural (row-major) index of each zig-zag position (ITU-T T.81
/// Figure A.6), used both to expand a `DQT`'s element order into natural
/// order and to place a decoded coefficient at its natural position.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Fixed-point precision of [`IDCT_BASIS`]'s entries: each is
/// `alpha(u) * cos((2x+1) * u * pi / 16)` scaled by `1 << IDCT_SCALE_BITS`
/// and rounded to the nearest integer, `alpha(0) = 1/sqrt(2)`, `alpha(u) =
/// 1` otherwise (ITU-T T.81 Annex A.3.3, the inverse DCT definition).
const IDCT_SCALE_BITS: u32 = 13;

/// The 8-point inverse-DCT basis matrix, precomputed once rather than
/// evaluated with trigonometric functions this `no_std` crate has no
/// access to. A reduced-scale inverse DCT of size `m` (`1`, `2`, `4`, or
/// `8`) uses exactly the top-left `m`×`m` submatrix: restricting both the
/// coefficients summed over and the output positions evaluated to `0..m`
/// is the standard scaled-IDCT technique (the basis itself is unchanged,
/// since it is still an 8-point transform, only a subset of its inputs
/// and outputs are used), which is why one table serves every scale.
const IDCT_BASIS: [[i32; 8]; 8] = [
    [5793, 8035, 7568, 6811, 5793, 4551, 3135, 1598],
    [5793, 6811, 3135, -1598, -5793, -8035, -7568, -4551],
    [5793, 4551, -3135, -8035, -5793, 1598, 7568, 6811],
    [5793, 1598, -7568, -4551, 5793, 6811, -3135, -8035],
    [5793, -1598, -7568, 4551, 5793, -6811, -3135, 8035],
    [5793, -4551, -3135, 8035, -5793, -1598, 7568, -6811],
    [5793, -6811, 3135, 1598, -5793, 8035, -7568, 4551],
    [5793, -8035, 7568, -6811, 5793, -4551, 3135, -1598],
];

// ---------------------------------------------------------------------
// Reduced-scale decoding
// ---------------------------------------------------------------------

/// A JPEG DCT decode scale: how many of the 8 samples along each block
/// edge the inverse DCT actually reconstructs.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Scale {
    Eighth,
    Quarter,
    Half,
    Full,
}

impl Scale {
    /// The block-edge sample count this scale reconstructs.
    const fn m(self) -> u32 {
        match self {
            Self::Eighth => 1,
            Self::Quarter => 2,
            Self::Half => 4,
            Self::Full => 8,
        }
    }

    /// Every scale, smallest output first — the order [`Self::choose`]
    /// searches.
    const ASCENDING: [Self; 4] = [Self::Eighth, Self::Quarter, Self::Half, Self::Full];

    /// The scale to decode a frame of natural `width`/`height` at, to
    /// serve a caller's `fit` within its `limits`.
    ///
    /// The preference is the smallest scale whose output still covers
    /// `fit` on both axes; this never scales up, so a `fit` larger than
    /// the natural image decodes at [`Self::Full`]. Where that covering
    /// scale's own output would breach `limits`, the largest scale that
    /// stays within them is chosen instead — deliberately trading
    /// sharpness for a decode the caller can actually afford. Only when
    /// even [`Self::Eighth`] breaches them is the frame refused, and then
    /// with whichever limit that smallest possible output broke.
    ///
    /// Decided entirely from the frame header's declared geometry, so the
    /// answer is known before a single coefficient or sample buffer is
    /// allocated: no scale is ever attempted and abandoned.
    fn choose(
        width: u32,
        height: u32,
        fit: FitBox,
        limits: &DecodeLimits,
    ) -> Result<Self, DecodeError> {
        let output = |scale: Self| {
            (
                output_dimension(width, scale.m()),
                output_dimension(height, scale.m()),
            )
        };
        let admitted = |scale: Self| {
            let (w, h) = output(scale);
            limits.check(w, h)
        };
        let covering = Self::ASCENDING
            .into_iter()
            .find(|&scale| {
                let (w, h) = output(scale);
                w >= fit.width() && h >= fit.height()
            })
            .unwrap_or(Self::Full);
        // Output size grows monotonically with the scale, so the scales a
        // set of limits admits form a prefix of `ASCENDING`: searching
        // downwards from the covering scale finds the sharpest affordable
        // one on its first hit.
        let affordable = Self::ASCENDING
            .into_iter()
            .rev()
            .filter(|&scale| scale.m() <= covering.m())
            .find(|&scale| admitted(scale).is_ok());
        match affordable {
            Some(scale) => Ok(scale),
            // An eighth is the smallest output a reduced inverse DCT can
            // produce, so its verdict is the last word on the frame.
            None => admitted(Self::Eighth).map(|()| Self::Eighth),
        }
    }
}

/// The output size along one axis at scale `m` (`1`, `2`, `4`, or `8`
/// eighths of natural size): `ceil(natural * m / 8)`, per libjpeg's
/// long-established DCT-scaling convention. `natural` is a JPEG frame
/// dimension, always at most `0xFFFF`, so this never overflows a `u64`.
fn output_dimension(natural: u32, m: u32) -> u32 {
    let scaled = u64::from(natural).saturating_mul(u64::from(m));
    u32::try_from(scaled.div_ceil(8)).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------
// Frame and component geometry
// ---------------------------------------------------------------------

/// One frame component (ITU-T T.81 §B.2.2).
#[derive(Copy, Clone, Debug)]
struct Component {
    id: u8,
    h: u32,
    v: u32,
    quant_table: u8,
}

/// A validated frame header (`SOF0`/`SOF1`/`SOF2`) and the block-grid
/// geometry it implies. Every field here is scale-independent: it
/// describes the coefficient/block structure the entropy-coded data
/// walks, never the eventual pixel output size (see [`Scale`] for that).
///
/// Cheap to clone (a handful of `u32`s plus a 1- or 3-element `Vec`), which
/// lets a scan decode hold its own owned copy instead of borrowing it from
/// the decoder — the decoder's other fields need `&mut self` at the same
/// time a scan is being decoded, and an owned copy sidesteps that borrow
/// entirely.
#[derive(Clone)]
struct Frame {
    progressive: bool,
    width: u32,
    height: u32,
    components: Vec<Component>,
    mcus_per_row: u32,
    mcus_per_col: u32,
}

impl Frame {
    /// The number of 8×8 blocks per MCU row/column this component
    /// occupies (`ITU-T T.81 §A.2`): its own sampling factor times the
    /// number of MCUs. This is the coefficient-storage and
    /// interleaved-scan-traversal grid — always a whole number of MCUs
    /// wide/tall, including any padding blocks beyond the visible image.
    fn blocks_per_line_padded(&self, component: &Component) -> u32 {
        self.mcus_per_row.saturating_mul(component.h)
    }

    fn blocks_per_col_padded(&self, component: &Component) -> u32 {
        self.mcus_per_col.saturating_mul(component.v)
    }

    /// The horizontal/vertical sampling factor of whichever component
    /// samples most densely — the reference every component's
    /// subsampling ratio and every MCU's pixel footprint is measured
    /// against.
    fn h_max(&self) -> u32 {
        self.components.iter().map(|c| c.h).max().unwrap_or(1)
    }

    fn v_max(&self) -> u32 {
        self.components.iter().map(|c| c.v).max().unwrap_or(1)
    }

    /// A component's own sample-plane width/height at decode scale `m`
    /// (`ceil(width * h_i * m / (h_max * 8))`), i.e. the visible extent of
    /// its (possibly subsampled) samples once reconstructed at that
    /// scale — the crop applied to the padded block grid before
    /// upsampling, so the trailing padding blocks a non-MCU-multiple
    /// image needs never leak into the output.
    fn component_sample_width(&self, component: &Component, m: u32) -> u32 {
        component_sample_extent(self.width, component.h, self.h_max(), m)
    }

    fn component_sample_height(&self, component: &Component, m: u32) -> u32 {
        component_sample_extent(self.height, component.v, self.v_max(), m)
    }

    /// The non-interleaved block-grid extent (`ITU-T T.81 §A.2.4`) a scan
    /// naming exactly this one component walks: `ceil(natural sample
    /// extent / 8)`, which can be smaller than
    /// [`Self::blocks_per_line_padded`] when this component's sampling
    /// factor is below `h_max`/`v_max`. Entropy decoding always walks the
    /// natural (full, unscaled) block grid — coefficients are transmitted
    /// at their real bit depth regardless of which DCT decode scale the
    /// eventual pixel output uses — so this always measures at `m == 8`.
    fn blocks_per_line_actual(&self, component: &Component) -> u32 {
        self.component_sample_width(component, 8).div_ceil(8)
    }

    fn blocks_per_col_actual(&self, component: &Component) -> u32 {
        self.component_sample_height(component, 8).div_ceil(8)
    }

    /// The output pixel size at decode scale `m`.
    fn output_width(&self, m: u32) -> u32 {
        output_dimension(self.width, m)
    }

    fn output_height(&self, m: u32) -> u32 {
        output_dimension(self.height, m)
    }
}

/// `ceil(natural * factor * m / (factor_max * 8))`, in `u64` throughout:
/// `natural` is at most `0xFFFF`, `factor`/`factor_max` at most `4`, and
/// `m` at most `8`, so the product never approaches `u64`'s range and the
/// only defensive step needed is against a (unreachable) zero divisor.
fn component_sample_extent(natural: u32, factor: u32, factor_max: u32, m: u32) -> u32 {
    let numerator = u64::from(natural)
        .saturating_mul(u64::from(factor))
        .saturating_mul(u64::from(m));
    let denominator = u64::from(factor_max).saturating_mul(8).max(1);
    u32::try_from(numerator.div_ceil(denominator)).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------
// Quantisation tables
// ---------------------------------------------------------------------

/// A quantisation table, expanded from `DQT`'s zig-zag element order into
/// natural (row-major) order, matching how coefficients are stored.
#[derive(Clone)]
struct QuantTable {
    natural: [u16; 64],
}

// ---------------------------------------------------------------------
// Huffman tables
// ---------------------------------------------------------------------

/// The number of low bits of the entropy stream a Huffman decode peeks at
/// once, before falling back to a bit-at-a-time search for a longer code.
/// Chosen well above the length of any commonly-occurring JPEG code
/// (JFIF's own example tables never exceed 9 bits) without growing the
/// fast table (`1 << FAST_BITS` entries) unreasonably.
const FAST_BITS: u32 = 9;

/// A canonical Huffman table (ITU-T T.81 Annex C), plus a fast lookup for
/// codes of at most [`FAST_BITS`] bits.
struct HuffmanTable {
    /// `fast[prefix]` is `Some((symbol, length))` when a code of `length`
    /// (`<= FAST_BITS`) bits matches every entry-`prefix` bit pattern that
    /// starts with that code — i.e. every possible padding of the
    /// remaining `FAST_BITS - length` bits. `None` means no code that
    /// short exists with this prefix: the caller must have a genuine
    /// [`FAST_BITS`]-bit window (not one padded with invented bits) before
    /// trusting a `None` to mean "fall back to the slow search", since a
    /// short real window could still hold a valid short code.
    fast: Vec<Option<(u8, u8)>>,
    /// The standard `mincode`/`maxcode`/`valptr` slow-path decode arrays
    /// (ITU-T T.81 Annex F.2.2.3, Figure F.16), indexed by code length
    /// `1..=16`; `maxcode[len] == -1` means no code of that length exists.
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [usize; 17],
    /// The symbols (`HUFFVAL`), in the same code order [`Self::valptr`]
    /// indexes into.
    symbols: Vec<u8>,
}

impl HuffmanTable {
    /// Build a canonical Huffman table from its 16 code-length counts
    /// (`bits[i]` is the count of codes of length `i + 1`) and its symbol
    /// list, in code order (ITU-T T.81 Annex C.2).
    fn build(bits: &[u8; 16], symbols: &[u8]) -> Result<Self, DecodeError> {
        let mut mincode = [0i32; 17];
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0usize; 17];
        let mut fast = vec![None; 1usize << FAST_BITS];

        let mut code: u32 = 0;
        let mut k: usize = 0;
        for len in 1u32..=16 {
            let count = usize::from(bits[usize::try_from(len - 1).unwrap_or(0)]);
            if count > 0 {
                valptr[usize::try_from(len).unwrap_or(0)] = k;
                mincode[usize::try_from(len).unwrap_or(0)] =
                    i32::try_from(code).unwrap_or(i32::MAX);
            }
            for _ in 0..count {
                let symbol = *symbols.get(k).ok_or(DecodeError::JpegInvalidHuffmanTable)?;
                if len <= FAST_BITS {
                    fill_fast_entries(&mut fast, code, len, symbol);
                }
                // A canonical code can never need more than 16 bits; a
                // `code` that has already grown past what `len` bits can
                // hold means `bits` describes an impossible assignment
                // (too many codes of an early length starve the codes
                // that must follow), which this decoder refuses rather
                // than silently building a broken table.
                if code >= (1u32 << len) {
                    return Err(DecodeError::JpegInvalidHuffmanTable);
                }
                code += 1;
                k += 1;
            }
            if count > 0 {
                maxcode[usize::try_from(len).unwrap_or(0)] = i32::try_from(code - 1).unwrap_or(-1);
            }
            code <<= 1;
        }
        if k != symbols.len() {
            return Err(DecodeError::JpegInvalidHuffmanTable);
        }
        Ok(Self {
            fast,
            mincode,
            maxcode,
            valptr,
            symbols: symbols.to_vec(),
        })
    }

    /// Decode one Huffman-coded symbol from `bits`.
    fn decode(&self, bits: &mut BitReader<'_>) -> Result<u8, DecodeError> {
        let (window, available) = bits.peek(FAST_BITS);
        if available == FAST_BITS {
            if let Some((symbol, length)) = self.fast[window as usize] {
                bits.consume(u32::from(length))?;
                return Ok(symbol);
            }
        }
        let mut code: i32 = 0;
        for len in 1usize..=16 {
            code = (code << 1) | i32::try_from(bits.next_bit()?).unwrap_or(0);
            if self.maxcode[len] >= 0 && code <= self.maxcode[len] {
                let offset = usize::try_from(code - self.mincode[len]).unwrap_or(usize::MAX);
                let index = self.valptr[len]
                    .checked_add(offset)
                    .ok_or(DecodeError::JpegHuffmanCodeNotFound)?;
                return self
                    .symbols
                    .get(index)
                    .copied()
                    .ok_or(DecodeError::JpegHuffmanCodeNotFound);
            }
        }
        Err(DecodeError::JpegHuffmanCodeNotFound)
    }
}

/// Populate every fast-table slot whose top `length` bits equal `code`
/// (there are `1 << (FAST_BITS - length)` of them, one per possible
/// padding of the remaining low bits) with `(symbol, length)`.
fn fill_fast_entries(fast: &mut [Option<(u8, u8)>], code: u32, length: u32, symbol: u8) {
    let pad_bits = FAST_BITS - length;
    let base = code << pad_bits;
    let pad_count = 1u32 << pad_bits;
    let length = u8::try_from(length).unwrap_or(u8::MAX);
    for pad in 0..pad_count {
        if let Some(slot) = fast.get_mut(usize::try_from(base | pad).unwrap_or(usize::MAX)) {
            *slot = Some((symbol, length));
        }
    }
}

// ---------------------------------------------------------------------
// The entropy-coded bit reader
// ---------------------------------------------------------------------

/// A big-endian bit reader over a JPEG entropy-coded segment, handling
/// `0xFF00` byte-stuffing transparently and stopping cleanly (without
/// consuming) at a genuine marker.
///
/// Bytes are fetched into the buffer one at a time, each validated for
/// stuffing before being trusted as data, so buffering several bytes
/// ahead (for the Huffman fast-table peek) can never misread a restart
/// marker's bytes as entropy data: whichever byte the marker's leading
/// `0xFF` is, that byte is checked at the moment it would be fetched, not
/// blindly consumed.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    count: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            buf: 0,
            count: 0,
        }
    }

    /// Try to have at least `want` bits buffered (capped at 32); fetches
    /// stop early, with fewer bits than `want` buffered, at the end of
    /// input or at a genuine marker.
    fn fill_to(&mut self, want: u32) {
        while self.count < want && self.count <= 24 {
            let Some(&byte) = self.data.get(self.pos) else {
                break;
            };
            if byte == 0xFF {
                match self.data.get(self.pos + 1) {
                    Some(0x00) => {
                        self.pos += 2;
                        self.push_byte(0xFF);
                    }
                    // A genuine marker (or a trailing `0xFF` with nothing
                    // to check it against): stop without consuming it, so
                    // the marker-scanning loop above finds it intact.
                    _ => break,
                }
            } else {
                self.pos += 1;
                self.push_byte(byte);
            }
        }
    }

    fn push_byte(&mut self, byte: u8) {
        self.buf = (self.buf << 8) | u32::from(byte);
        self.count += 8;
    }

    /// Peek the next `n` bits (`n <= 32`), left-justified into an `n`-bit
    /// value; the second element is how many of those bits are genuinely
    /// buffered (`<= n`) — a caller must not trust the value as more than
    /// padding unless this equals `n`.
    fn peek(&mut self, n: u32) -> (u32, u32) {
        self.fill_to(n);
        let available = self.count.min(n);
        if available == 0 {
            return (0, 0);
        }
        let raw = (self.buf >> (self.count - available)) & mask(available);
        (raw << (n - available), available)
    }

    /// Consume `n` bits already known to be available (via a prior
    /// [`Self::peek`] of at least `n`).
    fn consume(&mut self, n: u32) -> Result<(), DecodeError> {
        self.fill_to(n);
        if self.count < n {
            return Err(DecodeError::JpegEntropyDataTruncated);
        }
        self.count -= n;
        Ok(())
    }

    fn next_bit(&mut self) -> Result<u32, DecodeError> {
        self.fill_to(1);
        if self.count == 0 {
            return Err(DecodeError::JpegEntropyDataTruncated);
        }
        self.count -= 1;
        Ok((self.buf >> self.count) & 1)
    }

    /// Read `n` (`<= 16`) raw bits, MSB first, as an unsigned value.
    ///
    /// Taken from the buffer in one go rather than a bit at a time: every
    /// non-zero coefficient in a scan reads its magnitude bits here, so this
    /// is the hottest loop in an entropy decode and a per-bit shift-and-test
    /// would multiply its cost by the magnitude's width for no gain.
    fn read_bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        let (value, available) = self.peek(n);
        if available < n {
            return Err(DecodeError::JpegEntropyDataTruncated);
        }
        self.consume(n)?;
        Ok(value)
    }

    /// Discard any bits buffered short of a byte boundary and expect the
    /// next bytes to be a restart marker with cyclic sequence number
    /// `expected` (ITU-T T.81 §B.2.5): the entropy coder pads the current
    /// byte with 1-bits before emitting the marker, so any leftover
    /// buffered bits are exactly that padding, never real data.
    fn expect_restart(&mut self, expected: u8) -> Result<(), DecodeError> {
        self.buf = 0;
        self.count = 0;
        let marker = self
            .data
            .get(self.pos..self.pos + 2)
            .ok_or(DecodeError::JpegRestartMarkerMismatch)?;
        if marker[0] != 0xFF || restart_index(marker[1]) != Some(expected) {
            return Err(DecodeError::JpegRestartMarkerMismatch);
        }
        self.pos += 2;
        Ok(())
    }

    /// The byte position just past the last byte this reader has drawn
    /// bits from (i.e. the position a marker-scanning loop should resume
    /// from once the caller is done with entropy data).
    const fn position(&self) -> usize {
        self.pos
    }
}

const fn mask(bits: u32) -> u32 {
    if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    }
}

/// Sign-extend a Huffman-decoded magnitude (ITU-T T.81 §F.2.2.1's
/// `EXTEND` procedure): `category` additional bits were read as `value`;
/// this recovers the signed coefficient they encode.
fn extend(value: u32, category: u32) -> i32 {
    if category == 0 {
        return 0;
    }
    let half = 1i32 << (category - 1);
    let v = i32::try_from(value).unwrap_or(i32::MAX);
    if v < half {
        v - (1i32 << category) + 1
    } else {
        v
    }
}

// ---------------------------------------------------------------------
// Marker and segment parsing
// ---------------------------------------------------------------------

/// Scan forward from `pos` past any fill bytes (`ITU-T T.81 §B.1.1.3`
/// permits any number of `0xFF` bytes before a marker code) and return
/// the marker code and the position just after it.
fn read_marker(bytes: &[u8], pos: usize) -> Result<(u8, usize), DecodeError> {
    let first = *bytes.get(pos).ok_or(DecodeError::JpegMarkerTruncated)?;
    if first != 0xFF {
        return Err(DecodeError::JpegUnknownMarker);
    }
    let mut pos = pos + 1;
    loop {
        let byte = *bytes.get(pos).ok_or(DecodeError::JpegMarkerTruncated)?;
        pos += 1;
        if byte != 0xFF {
            return Ok((byte, pos));
        }
    }
}

/// Read a length-prefixed segment's payload starting at `pos` (just after
/// the marker code), returning the payload and the position just after it.
fn read_segment(bytes: &[u8], pos: usize) -> Result<(&[u8], usize), DecodeError> {
    let len_bytes = bytes
        .get(pos..pos + 2)
        .ok_or(DecodeError::JpegMarkerTruncated)?;
    let length = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    if length < 2 {
        return Err(DecodeError::JpegSegmentTooShort);
    }
    let payload_start = pos + 2;
    let payload_end = payload_start
        .checked_add(length - 2)
        .ok_or(DecodeError::JpegSegmentLengthExceedsInput)?;
    let payload = bytes
        .get(payload_start..payload_end)
        .ok_or(DecodeError::JpegSegmentLengthExceedsInput)?;
    Ok((payload, payload_end))
}

/// Parse a `SOF0`/`SOF1`/`SOF2` payload into a structurally validated
/// [`Frame`].
///
/// Validates the header's own shape only; the declared geometry is
/// weighed against the caller's limits by [`Decoder::choose_scale`], which
/// is where the decode scale — and so the size actually being asked for —
/// is settled. Nothing here allocates more than the frame's own one to
/// three component descriptors.
fn parse_sof(payload: &[u8], progressive: bool) -> Result<Frame, DecodeError> {
    let &[precision, h0, h1, w0, w1, nf, ref rest @ ..] = payload else {
        return Err(DecodeError::JpegInvalidFrameHeader);
    };
    if precision != 8 {
        return Err(DecodeError::JpegUnsupportedPrecision);
    }
    if nf != 1 && nf != 3 {
        return Err(DecodeError::JpegUnsupportedComponentCount);
    }
    if rest.len() != usize::from(nf) * 3 {
        return Err(DecodeError::JpegInvalidFrameHeader);
    }
    let height = u32::from(u16::from_be_bytes([h0, h1]));
    let width = u32::from(u16::from_be_bytes([w0, w1]));

    let mut components = Vec::with_capacity(usize::from(nf));
    for &[id, sampling, quant_table] in rest.as_chunks::<3>().0 {
        let h = u32::from(sampling >> 4);
        let v = u32::from(sampling & 0x0F);
        if h == 0 || h > 4 || v == 0 || v > 4 {
            return Err(DecodeError::JpegInvalidSamplingFactor);
        }
        if quant_table > 3 {
            return Err(DecodeError::JpegInvalidQuantizationTable);
        }
        if components.iter().any(|c: &Component| c.id == id) {
            return Err(DecodeError::JpegInvalidFrameHeader);
        }
        components.push(Component {
            id,
            h,
            v,
            quant_table,
        });
    }

    let h_max = components.iter().map(|c| c.h).max().unwrap_or(1);
    let v_max = components.iter().map(|c| c.v).max().unwrap_or(1);
    let mcu_w = 8u32.saturating_mul(h_max);
    let mcu_h = 8u32.saturating_mul(v_max);
    let mcus_per_row = width.div_ceil(mcu_w.max(1));
    let mcus_per_col = height.div_ceil(mcu_h.max(1));

    Ok(Frame {
        progressive,
        width,
        height,
        components,
        mcus_per_row,
        mcus_per_col,
    })
}

/// Parse a `DQT` payload into however many quantisation tables it
/// carries, calling `store` with each `(index, table)`.
fn parse_dqt(
    payload: &[u8],
    mut store: impl FnMut(usize, QuantTable) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    let mut pos = 0usize;
    while pos < payload.len() {
        let header = *payload
            .get(pos)
            .ok_or(DecodeError::JpegInvalidQuantizationTable)?;
        let precision16 = match header >> 4 {
            0 => false,
            1 => true,
            _ => return Err(DecodeError::JpegInvalidQuantizationTable),
        };
        let index = usize::from(header & 0x0F);
        if index > 3 {
            return Err(DecodeError::JpegInvalidQuantizationTable);
        }
        pos += 1;
        let element_bytes = if precision16 { 2 } else { 1 };
        let table_bytes = 64 * element_bytes;
        let raw = payload
            .get(pos..pos + table_bytes)
            .ok_or(DecodeError::JpegInvalidQuantizationTable)?;
        pos += table_bytes;

        let mut natural = [0u16; 64];
        for (i, &zigzag_index) in ZIGZAG.iter().enumerate() {
            let value = if precision16 {
                u16::from_be_bytes([raw[i * 2], raw[i * 2 + 1]])
            } else {
                u16::from(raw[i])
            };
            natural[zigzag_index] = value;
        }
        store(index, QuantTable { natural })?;
    }
    Ok(())
}

/// Parse a `DHT` payload into however many Huffman tables it carries,
/// calling `store` with each `(class, index, table)` (`class` `0` = DC,
/// `1` = AC).
fn parse_dht(
    payload: &[u8],
    mut store: impl FnMut(u8, usize, HuffmanTable) -> Result<(), DecodeError>,
) -> Result<(), DecodeError> {
    let mut pos = 0usize;
    while pos < payload.len() {
        let header = *payload
            .get(pos)
            .ok_or(DecodeError::JpegInvalidHuffmanTable)?;
        let class = header >> 4;
        let index = usize::from(header & 0x0F);
        if class > 1 || index > 3 {
            return Err(DecodeError::JpegInvalidHuffmanTable);
        }
        pos += 1;
        let counts = payload
            .get(pos..pos + 16)
            .ok_or(DecodeError::JpegInvalidHuffmanTable)?;
        let mut bits = [0u8; 16];
        bits.copy_from_slice(counts);
        pos += 16;
        let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
        let symbols = payload
            .get(pos..pos + total)
            .ok_or(DecodeError::JpegInvalidHuffmanTable)?;
        pos += total;
        store(class, index, HuffmanTable::build(&bits, symbols)?)?;
    }
    Ok(())
}

/// One scan component selector (ITU-T T.81 §B.2.3): a component id plus
/// its DC/AC Huffman table selectors.
struct ScanComponent {
    id: u8,
    dc_table: u8,
    ac_table: u8,
}

/// A validated `SOS` header.
struct ScanHeader {
    components: Vec<ScanComponent>,
    spectral_start: u32,
    spectral_end: u32,
    successive_high: u32,
    successive_low: u32,
}

/// Parse a `SOS` payload, checking its shape but not yet its components
/// against the frame (the caller does that, since it needs the frame).
fn parse_sos(payload: &[u8], progressive: bool) -> Result<ScanHeader, DecodeError> {
    let &[ns, ref rest @ ..] = payload else {
        return Err(DecodeError::JpegInvalidScanHeader);
    };
    let ns = usize::from(ns);
    if ns == 0 || ns > 4 || rest.len() != ns * 2 + 3 {
        return Err(DecodeError::JpegInvalidScanHeader);
    }
    let (selectors, tail) = rest.split_at(ns * 2);
    let &[ss, se, ahal] = tail else {
        return Err(DecodeError::JpegInvalidScanHeader);
    };

    let mut components = Vec::with_capacity(ns);
    for &[id, tables] in selectors.as_chunks::<2>().0 {
        let dc_table = tables >> 4;
        let ac_table = tables & 0x0F;
        if dc_table > 3 || ac_table > 3 {
            return Err(DecodeError::JpegInvalidScanHeader);
        }
        components.push(ScanComponent {
            id,
            dc_table,
            ac_table,
        });
    }

    let spectral_start = u32::from(ss);
    let spectral_end = u32::from(se);
    let successive_high = u32::from(ahal >> 4);
    let successive_low = u32::from(ahal & 0x0F);
    if spectral_start > 63 || spectral_end > 63 || spectral_start > spectral_end {
        return Err(DecodeError::JpegInvalidScanHeader);
    }
    if successive_high > 13 || successive_low > 13 {
        return Err(DecodeError::JpegInvalidScanHeader);
    }
    if !progressive && (spectral_start != 0 || spectral_end != 63 || ahal != 0) {
        return Err(DecodeError::JpegInvalidScanHeader);
    }
    // A progressive AC scan (`Ss > 0`) names exactly one component
    // (ITU-T T.81 §G.1.1.1); DC scans (`Ss == 0`) may interleave several.
    if progressive && spectral_start > 0 && components.len() != 1 {
        return Err(DecodeError::JpegInvalidScanHeader);
    }

    Ok(ScanHeader {
        components,
        spectral_start,
        spectral_end,
        successive_high,
        successive_low,
    })
}

/// Parse a `DRI` payload: exactly one 2-byte restart interval.
fn parse_dri(payload: &[u8]) -> Result<u32, DecodeError> {
    let &[hi, lo] = payload else {
        return Err(DecodeError::JpegInvalidRestartInterval);
    };
    Ok(u32::from(u16::from_be_bytes([hi, lo])))
}

/// The Adobe APP14 marker's 12-byte payload (Adobe TN5116): only its
/// trailing colour-transform byte matters here (`0` = the components are
/// already RGB, not YCbCr). A payload of any other shape is simply not
/// the Adobe marker this decoder looks for, so it is ignored rather than
/// refused — an APP segment's contents are advisory by nature.
fn parse_adobe_transform(payload: &[u8]) -> Option<u8> {
    if payload.len() == 12 && payload.starts_with(b"Adobe") {
        payload.last().copied()
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------

/// Read the natural size the frame header declares, decoding nothing.
///
/// Walks only the markers that may precede a frame header, skipping each by
/// its own declared length, and hands the frame header to the same parser a
/// full decode uses — so a probe refuses 12-bit precision, an unsupported
/// component count, a lossless or hierarchical frame, arithmetic coding and
/// a `DNL` height exactly as a decode would, and never reads a byte of
/// entropy-coded data.
///
/// The markers it walks past are exactly those a decode itself walks past
/// before a frame header: the tables, the restart interval, the application
/// and comment segments. Anything else there is a stream a decode would
/// refuse, so a probe refuses it too rather than guessing its length.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    if !bytes.starts_with(&crate::JPEG_SIGNATURE[..2]) {
        return Err(DecodeError::JpegBadSignature);
    }
    let mut pos = 2usize; // past the 2-byte SOI marker
    loop {
        let (marker, next) = read_marker(bytes, pos)?;
        pos = next;
        match marker {
            SOF0 | SOF1 | SOF2 => {
                let (payload, _after) = read_segment(bytes, pos)?;
                let frame = parse_sof(payload, marker == SOF2)?;
                return Ok((frame.width, frame.height));
            }
            SOF3 | SOF5 | SOF6 | SOF7 | DHP | EXP => {
                return Err(DecodeError::JpegLosslessOrHierarchicalUnsupported);
            }
            SOF9 | SOF10 | SOF11 | SOF13 | SOF14 | SOF15 | DAC => {
                return Err(DecodeError::JpegArithmeticCodingUnsupported);
            }
            DNL => return Err(DecodeError::JpegDnlUnsupported),
            SOS | EOI => return Err(DecodeError::JpegMissingFrameHeader),
            DQT | DHT | DRI => {
                let (_payload, after) = read_segment(bytes, pos)?;
                pos = after;
            }
            marker if (APP0..=APP15).contains(&marker) || marker == COM => {
                let (_payload, after) = read_segment(bytes, pos)?;
                pos = after;
            }
            marker if restart_index(marker).is_some() => {
                return Err(DecodeError::JpegRestartMarkerMismatch);
            }
            _ => return Err(DecodeError::JpegUnknownMarker),
        }
    }
}

/// Decode `bytes` at natural size.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    decode_inner(bytes, limits, None)
}

/// Decode `bytes` at the smallest DCT scale that still covers `fit`, or at
/// the largest scale `limits` admit when that covering scale is too large
/// for them (see [`Scale::choose`]).
pub(crate) fn decode_fitted(
    bytes: &[u8],
    limits: &DecodeLimits,
    fit: FitBox,
) -> Result<RasterImage, DecodeError> {
    decode_inner(bytes, limits, Some(fit))
}

/// Decoder state accumulated while scanning markers.
struct Decoder<'a> {
    limits: &'a DecodeLimits,
    fit: Option<FitBox>,
    quant_tables: [Option<QuantTable>; 4],
    dc_tables: [Option<HuffmanTable>; 4],
    ac_tables: [Option<HuffmanTable>; 4],
    restart_interval: u32,
    adobe_transform: Option<u8>,
    frame: Option<Frame>,
    scale: Scale,
    /// Baseline/extended-sequential streaming sample planes, one per
    /// frame component, each sized to that component's padded block grid
    /// — `blocks_per_line_padded` by `blocks_per_col_padded` blocks, each
    /// block `m` samples square. Filled scan by scan; never used for a
    /// progressive frame.
    sample_planes: Vec<Vec<u8>>,
    /// Progressive coefficient store, one flat buffer per frame
    /// component, `blocks_per_line_padded * blocks_per_col_padded * 64`
    /// entries in natural (row-major) per-block order. Only allocated for
    /// a progressive frame, once its geometry is known.
    coefficients: Vec<Vec<i16>>,
    eobrun: u32,
}

/// Which block of which component, under which scan header, a block
/// decoder is being asked to decode. Every scan shape needs the same
/// context, so it travels as one value rather than as seven arguments
/// repeated across five signatures.
#[derive(Copy, Clone)]
struct BlockContext<'a> {
    frame: &'a Frame,
    component: &'a Component,
    comp_index: usize,
    scan_component: &'a ScanComponent,
    header: &'a ScanHeader,
    block_col: u32,
    block_row: u32,
}

fn decode_inner(
    bytes: &[u8],
    limits: &DecodeLimits,
    fit: Option<FitBox>,
) -> Result<RasterImage, DecodeError> {
    if !bytes.starts_with(&crate::JPEG_SIGNATURE[..2]) {
        return Err(DecodeError::JpegBadSignature);
    }
    let mut decoder = Decoder {
        limits,
        fit,
        quant_tables: [None, None, None, None],
        dc_tables: [None, None, None, None],
        ac_tables: [None, None, None, None],
        restart_interval: 0,
        adobe_transform: None,
        frame: None,
        scale: Scale::Full,
        sample_planes: Vec::new(),
        coefficients: Vec::new(),
        eobrun: 0,
    };
    decoder.run(bytes)
}

impl Decoder<'_> {
    fn run(&mut self, bytes: &[u8]) -> Result<RasterImage, DecodeError> {
        let mut pos = 2usize; // past the 2-byte SOI marker
        loop {
            let (marker, next) = read_marker(bytes, pos)?;
            pos = next;
            if marker == EOI {
                return self.finish();
            }
            if restart_index(marker).is_some() {
                return Err(DecodeError::JpegRestartMarkerMismatch);
            }
            match marker {
                SOI => return Err(DecodeError::JpegUnknownMarker),
                DQT => {
                    let (payload, after) = read_segment(bytes, pos)?;
                    pos = after;
                    let tables = &mut self.quant_tables;
                    parse_dqt(payload, |index, table| {
                        tables[index] = Some(table);
                        Ok(())
                    })?;
                }
                DHT => {
                    let (payload, after) = read_segment(bytes, pos)?;
                    pos = after;
                    let dc = &mut self.dc_tables;
                    let ac = &mut self.ac_tables;
                    parse_dht(payload, |class, index, table| {
                        if class == 0 {
                            dc[index] = Some(table);
                        } else {
                            ac[index] = Some(table);
                        }
                        Ok(())
                    })?;
                }
                DRI => {
                    let (payload, after) = read_segment(bytes, pos)?;
                    pos = after;
                    self.restart_interval = parse_dri(payload)?;
                }
                SOF0 | SOF1 | SOF2 => {
                    let (payload, after) = read_segment(bytes, pos)?;
                    pos = after;
                    if self.frame.is_some() {
                        return Err(DecodeError::JpegDuplicateFrameHeader);
                    }
                    let progressive = marker == SOF2;
                    let frame = parse_sof(payload, progressive)?;
                    self.scale = self.choose_scale(&frame)?;
                    if progressive {
                        self.allocate_coefficient_store(&frame)?;
                    } else {
                        self.allocate_sample_planes(&frame);
                    }
                    self.frame = Some(frame);
                }
                SOF3 | SOF5 | SOF6 | SOF7 | DHP | EXP => {
                    return Err(DecodeError::JpegLosslessOrHierarchicalUnsupported);
                }
                SOF9 | SOF10 | SOF11 | SOF13 | SOF14 | SOF15 | DAC => {
                    return Err(DecodeError::JpegArithmeticCodingUnsupported);
                }
                DNL => return Err(DecodeError::JpegDnlUnsupported),
                APP14 => {
                    let (payload, after) = read_segment(bytes, pos)?;
                    pos = after;
                    self.adobe_transform = parse_adobe_transform(payload);
                }
                SOS => {
                    let progressive = self
                        .frame
                        .as_ref()
                        .ok_or(DecodeError::JpegMissingFrameHeader)?
                        .progressive;
                    let (payload, after) = read_segment(bytes, pos)?;
                    let header = parse_sos(payload, progressive)?;
                    pos = self.decode_scan(bytes, after, &header)?;
                }
                marker if (APP0..=APP15).contains(&marker) || marker == COM => {
                    let (_, after) = read_segment(bytes, pos)?;
                    pos = after;
                }
                _ => return Err(DecodeError::JpegUnknownMarker),
            }
        }
    }

    /// Settle the decode scale for a just-parsed frame header, and with it
    /// the size this decode is committing to, before anything is allocated
    /// for it.
    ///
    /// A natural-size decode holds the frame's declared geometry to the
    /// limits directly. A fitted decode instead holds the *output* of the
    /// scale it picks to them, which is what lets a source far larger than
    /// the limits still be served at a reduced scale.
    fn choose_scale(&self, frame: &Frame) -> Result<Scale, DecodeError> {
        let Some(fit) = self.fit else {
            self.limits.check(frame.width, frame.height)?;
            return Ok(Scale::Full);
        };
        Scale::choose(frame.width, frame.height, fit, self.limits)
    }

    /// Allocate one streaming sample plane per component, sized to that
    /// component's padded block grid at the chosen decode scale. Used for
    /// a non-progressive frame, which never needs a persistent
    /// coefficient store: each block is dequantised and inverse-DCT'd the
    /// moment its scan decodes it.
    fn allocate_sample_planes(&mut self, frame: &Frame) {
        let m = self.scale.m();
        self.sample_planes = frame
            .components
            .iter()
            .map(|component| {
                let width = frame.blocks_per_line_padded(component).saturating_mul(m);
                let height = frame.blocks_per_col_padded(component).saturating_mul(m);
                let len = usize::try_from(u64::from(width).saturating_mul(u64::from(height)))
                    .unwrap_or(usize::MAX);
                vec![0u8; len]
            })
            .collect();
    }

    /// Allocate the progressive coefficient store, refusing before
    /// allocating anything if the total size a progressive frame's
    /// geometry implies exceeds
    /// [`DecodeLimits::max_progressive_coefficient_bytes`].
    fn allocate_coefficient_store(&mut self, frame: &Frame) -> Result<(), DecodeError> {
        let mut sizes = Vec::with_capacity(frame.components.len());
        let mut total_bytes = 0u64;
        for component in &frame.components {
            let blocks = u64::from(frame.blocks_per_line_padded(component))
                .checked_mul(u64::from(frame.blocks_per_col_padded(component)))
                .ok_or(DecodeError::DimensionsOverflow)?;
            let coefficients = blocks
                .checked_mul(64)
                .ok_or(DecodeError::DimensionsOverflow)?;
            let bytes = coefficients
                .checked_mul(2)
                .ok_or(DecodeError::DimensionsOverflow)?;
            total_bytes = total_bytes
                .checked_add(bytes)
                .ok_or(DecodeError::DimensionsOverflow)?;
            sizes.push(usize::try_from(coefficients).map_err(|_| DecodeError::DimensionsOverflow)?);
        }
        if total_bytes > self.limits.max_progressive_coefficient_bytes() {
            return Err(DecodeError::JpegProgressiveCoefficientStoreExceedsLimit);
        }
        self.coefficients = sizes.into_iter().map(|n| vec![0i16; n]).collect();
        Ok(())
    }

    /// Decode one scan's entropy-coded data, returning the byte position
    /// just after it (where the marker-scanning loop resumes).
    fn decode_scan(
        &mut self,
        bytes: &[u8],
        pos: usize,
        header: &ScanHeader,
    ) -> Result<usize, DecodeError> {
        let frame = self
            .frame
            .clone()
            .ok_or(DecodeError::JpegMissingFrameHeader)?;
        let mut comp_indices = Vec::with_capacity(header.components.len());
        for scan_component in &header.components {
            let index = frame
                .components
                .iter()
                .position(|c| c.id == scan_component.id)
                .ok_or(DecodeError::JpegComponentIdMismatch)?;
            comp_indices.push(index);
        }

        let kind = ScanKind::classify(frame.progressive, header);
        self.eobrun = 0;
        let mut dc_predictors = vec![0i32; comp_indices.len()];
        let mut bits = BitReader::new(bytes, pos);
        let interleaved = header.components.len() > 1;
        let mut units_since_restart = 0u32;
        let mut restart_seq = 0u8;

        if interleaved {
            let total_units =
                u64::from(frame.mcus_per_row).saturating_mul(u64::from(frame.mcus_per_col));
            let mut unit_index = 0u64;
            for mcu_row in 0..frame.mcus_per_col {
                for mcu_col in 0..frame.mcus_per_row {
                    for (slot, &comp_index) in comp_indices.iter().enumerate() {
                        let component = frame.components[comp_index];
                        let scan_component = &header.components[slot];
                        for dv in 0..component.v {
                            for dh in 0..component.h {
                                let ctx = BlockContext {
                                    frame: &frame,
                                    component: &component,
                                    comp_index,
                                    scan_component,
                                    header,
                                    block_col: mcu_col.saturating_mul(component.h) + dh,
                                    block_row: mcu_row.saturating_mul(component.v) + dv,
                                };
                                self.decode_one_block(
                                    &mut bits,
                                    ctx,
                                    kind,
                                    &mut dc_predictors[slot],
                                )?;
                            }
                        }
                    }
                    unit_index += 1;
                    if unit_index < total_units {
                        self.restart_if_due(
                            &mut bits,
                            &mut units_since_restart,
                            &mut restart_seq,
                            &mut dc_predictors,
                        )?;
                    }
                }
            }
        } else {
            let comp_index = *comp_indices
                .first()
                .ok_or(DecodeError::JpegInvalidScanHeader)?;
            let component = frame.components[comp_index];
            let scan_component = &header.components[0];
            let blocks_wide = frame.blocks_per_line_actual(&component);
            let blocks_tall = frame.blocks_per_col_actual(&component);
            let total_units = u64::from(blocks_wide).saturating_mul(u64::from(blocks_tall));
            let mut unit_index = 0u64;
            for block_row in 0..blocks_tall {
                for block_col in 0..blocks_wide {
                    let ctx = BlockContext {
                        frame: &frame,
                        component: &component,
                        comp_index,
                        scan_component,
                        header,
                        block_col,
                        block_row,
                    };
                    self.decode_one_block(&mut bits, ctx, kind, &mut dc_predictors[0])?;
                    unit_index += 1;
                    if unit_index < total_units {
                        self.restart_if_due(
                            &mut bits,
                            &mut units_since_restart,
                            &mut restart_seq,
                            &mut dc_predictors,
                        )?;
                    }
                }
            }
        }

        Ok(bits.position())
    }

    /// If the declared restart interval has been reached, consume the
    /// expected `RSTn` marker and reset every piece of per-scan decode
    /// state a restart resets: the DC predictors and the AC end-of-band
    /// run (ITU-T T.81 §F.2.2.5, §G.1.2.2). Never called after the scan's
    /// last unit, since no restart marker follows it — only a scan
    /// boundary or `EOI` does.
    fn restart_if_due(
        &mut self,
        bits: &mut BitReader<'_>,
        units_since_restart: &mut u32,
        restart_seq: &mut u8,
        dc_predictors: &mut [i32],
    ) -> Result<(), DecodeError> {
        *units_since_restart += 1;
        if self.restart_interval > 0 && *units_since_restart == self.restart_interval {
            bits.expect_restart(*restart_seq)?;
            *restart_seq = (*restart_seq + 1) % 8;
            *units_since_restart = 0;
            self.eobrun = 0;
            for predictor in dc_predictors.iter_mut() {
                *predictor = 0;
            }
        }
        Ok(())
    }

    /// Decode one 8×8 block, dispatching on which of the five scan shapes
    /// (ITU-T T.81 §G.1.2) `kind` is.
    fn decode_one_block(
        &mut self,
        bits: &mut BitReader<'_>,
        ctx: BlockContext<'_>,
        kind: ScanKind,
        dc_predictor: &mut i32,
    ) -> Result<(), DecodeError> {
        match kind {
            ScanKind::Sequential => self.decode_block_sequential(bits, ctx, dc_predictor),
            ScanKind::DcFirst => self.decode_block_dc_first(bits, ctx, dc_predictor),
            ScanKind::DcRefine => self.decode_block_dc_refine(bits, ctx),
            ScanKind::AcFirst => self.decode_block_ac_first(bits, ctx),
            ScanKind::AcRefine => self.decode_block_ac_refine(bits, ctx),
        }
    }

    /// The mutable 64-entry (natural order) coefficient slice for the
    /// block `ctx` names, in the progressive coefficient store.
    fn coefficient_block_mut(&mut self, ctx: BlockContext<'_>) -> Result<&mut [i16], DecodeError> {
        let start = block_offset(ctx.frame, ctx.component, ctx.block_col, ctx.block_row)?;
        self.coefficients
            .get_mut(ctx.comp_index)
            .and_then(|c| c.get_mut(start..start + 64))
            .ok_or(DecodeError::DimensionsOverflow)
    }

    /// A baseline/extended-sequential block (`ITU-T T.81 §F.2`): decode a
    /// full DC + AC block via Huffman, dequantise, inverse-DCT it at the
    /// chosen scale, and write the samples straight into this component's
    /// streaming sample plane.
    fn decode_block_sequential(
        &mut self,
        bits: &mut BitReader<'_>,
        ctx: BlockContext<'_>,
        dc_predictor: &mut i32,
    ) -> Result<(), DecodeError> {
        let mut coeffs = [0i32; 64];

        let dc_table = self.dc_tables[usize::from(ctx.scan_component.dc_table)]
            .as_ref()
            .ok_or(DecodeError::JpegMissingHuffmanTable)?;
        let category = u32::from(dc_table.decode(bits)?);
        if category > 15 {
            return Err(DecodeError::JpegHuffmanCodeNotFound);
        }
        let raw = if category > 0 {
            bits.read_bits(category)?
        } else {
            0
        };
        *dc_predictor = dc_predictor.saturating_add(extend(raw, category));
        coeffs[0] = *dc_predictor;

        decode_ac_into(
            bits,
            self.ac_tables[usize::from(ctx.scan_component.ac_table)]
                .as_ref()
                .ok_or(DecodeError::JpegMissingHuffmanTable)?,
            1,
            63,
            &mut coeffs,
        )?;

        let quant = self.quant_tables[usize::from(ctx.component.quant_table)]
            .as_ref()
            .ok_or(DecodeError::JpegMissingQuantizationTable)?
            .natural;
        let m = self.scale.m();
        let plane_stride = usize::try_from(
            ctx.frame
                .blocks_per_line_padded(ctx.component)
                .saturating_mul(m),
        )
        .unwrap_or(usize::MAX);
        let plane = self
            .sample_planes
            .get_mut(ctx.comp_index)
            .ok_or(DecodeError::DimensionsOverflow)?;
        idct_and_store(
            &coeffs,
            &quant,
            m,
            plane,
            plane_stride,
            usize::try_from(ctx.block_row.saturating_mul(m)).unwrap_or(0),
            usize::try_from(ctx.block_col.saturating_mul(m)).unwrap_or(0),
        );
        Ok(())
    }

    /// A progressive DC-first block (`ITU-T T.81 §G.1.2.1`).
    fn decode_block_dc_first(
        &mut self,
        bits: &mut BitReader<'_>,
        ctx: BlockContext<'_>,
        dc_predictor: &mut i32,
    ) -> Result<(), DecodeError> {
        let dc_table = self.dc_tables[usize::from(ctx.scan_component.dc_table)]
            .as_ref()
            .ok_or(DecodeError::JpegMissingHuffmanTable)?;
        let category = u32::from(dc_table.decode(bits)?);
        if category > 15 {
            return Err(DecodeError::JpegHuffmanCodeNotFound);
        }
        let raw = if category > 0 {
            bits.read_bits(category)?
        } else {
            0
        };
        *dc_predictor = dc_predictor.saturating_add(extend(raw, category));
        let value = shift_to_i16(*dc_predictor, ctx.header.successive_low);
        let block = self.coefficient_block_mut(ctx)?;
        block[0] = value;
        Ok(())
    }

    /// A progressive DC-refinement block (`ITU-T T.81 §G.1.2.2`): one raw
    /// bit, no Huffman table involved.
    fn decode_block_dc_refine(
        &mut self,
        bits: &mut BitReader<'_>,
        ctx: BlockContext<'_>,
    ) -> Result<(), DecodeError> {
        if bits.next_bit()? != 0 {
            let addition = 1i16.checked_shl(ctx.header.successive_low).unwrap_or(0);
            let block = self.coefficient_block_mut(ctx)?;
            block[0] |= addition;
        }
        Ok(())
    }

    /// A progressive AC-first block (`ITU-T T.81 §G.1.2.3`), including the
    /// end-of-band run that can span several blocks.
    fn decode_block_ac_first(
        &mut self,
        bits: &mut BitReader<'_>,
        ctx: BlockContext<'_>,
    ) -> Result<(), DecodeError> {
        if self.eobrun > 0 {
            self.eobrun -= 1;
            return Ok(());
        }
        let mut band = [0i16; 64];
        {
            let ac_table = self.ac_tables[usize::from(ctx.scan_component.ac_table)]
                .as_ref()
                .ok_or(DecodeError::JpegMissingHuffmanTable)?;
            let mut k = ctx.header.spectral_start;
            while k <= ctx.header.spectral_end {
                let rs = ac_table.decode(bits)?;
                let run = u32::from(rs >> 4);
                let size = u32::from(rs & 0x0F);
                if size == 0 {
                    if run < 15 {
                        let mut eob = (1u32 << run).saturating_sub(1);
                        if run > 0 {
                            eob = eob.saturating_add(bits.read_bits(run)?);
                        }
                        self.eobrun = eob;
                        break;
                    }
                    k += 16;
                    continue;
                }
                k += run;
                if k > ctx.header.spectral_end {
                    return Err(DecodeError::JpegCoefficientRunOverflow);
                }
                let raw = bits.read_bits(size)?;
                let value = extend(raw, size);
                band[ZIGZAG[usize::try_from(k).unwrap_or(0)]] =
                    shift_to_i16(value, ctx.header.successive_low);
                k += 1;
            }
        }
        let spectral_start = ctx.header.spectral_start;
        let spectral_end = ctx.header.spectral_end;
        let block = self.coefficient_block_mut(ctx)?;
        for pos in spectral_start..=spectral_end {
            let natural = ZIGZAG[usize::try_from(pos).unwrap_or(0)];
            block[natural] = band[natural];
        }
        Ok(())
    }

    /// A progressive AC-refinement block (`ITU-T T.81 §G.1.2.4`), the most
    /// intricate scan shape: it both refines every already-nonzero
    /// coefficient it passes over and, once any zero-history run
    /// completes, places at most one freshly-coded coefficient.
    fn decode_block_ac_refine(
        &mut self,
        bits: &mut BitReader<'_>,
        ctx: BlockContext<'_>,
    ) -> Result<(), DecodeError> {
        let start = block_offset(ctx.frame, ctx.component, ctx.block_col, ctx.block_row)?;
        let al = ctx.header.successive_low;
        let p1 = 1i16.checked_shl(al).unwrap_or(0);
        let m1 = p1.checked_neg().unwrap_or(i16::MIN);

        let mut k = ctx.header.spectral_start;
        if self.eobrun == 0 {
            while k <= ctx.header.spectral_end {
                let ac_table = self.ac_tables[usize::from(ctx.scan_component.ac_table)]
                    .as_ref()
                    .ok_or(DecodeError::JpegMissingHuffmanTable)?;
                let rs = ac_table.decode(bits)?;
                let mut run = u32::from(rs >> 4);
                let size = u32::from(rs & 0x0F);
                let mut new_value = 0i16;
                if size == 0 {
                    if run < 15 {
                        let mut eob = 1u32 << run;
                        if run > 0 {
                            eob = eob.saturating_add(bits.read_bits(run)?);
                        }
                        self.eobrun = eob;
                        break;
                    }
                    // `run == 15`: ZRL, skip 16 zero-history coefficients.
                } else {
                    // A freshly-coded refinement coefficient always has
                    // magnitude 1; only its sign is transmitted.
                    new_value = if bits.next_bit()? != 0 { p1 } else { m1 };
                }
                let block = self
                    .coefficients
                    .get_mut(ctx.comp_index)
                    .and_then(|c| c.get_mut(start..start + 64))
                    .ok_or(DecodeError::DimensionsOverflow)?;
                while k <= ctx.header.spectral_end {
                    let natural = ZIGZAG[usize::try_from(k).unwrap_or(0)];
                    let existing = block[natural];
                    if existing != 0 {
                        if bits.next_bit()? != 0 && existing & p1 == 0 {
                            block[natural] = refine_coefficient(existing, p1, m1);
                        }
                    } else if run == 0 {
                        if size != 0 {
                            block[natural] = new_value;
                        }
                        k += 1;
                        break;
                    } else {
                        run -= 1;
                    }
                    k += 1;
                }
            }
        }
        if self.eobrun > 0 {
            let block = self
                .coefficients
                .get_mut(ctx.comp_index)
                .and_then(|c| c.get_mut(start..start + 64))
                .ok_or(DecodeError::DimensionsOverflow)?;
            while k <= ctx.header.spectral_end {
                let natural = ZIGZAG[usize::try_from(k).unwrap_or(0)];
                let existing = block[natural];
                if existing != 0 && bits.next_bit()? != 0 && existing & p1 == 0 {
                    block[natural] = refine_coefficient(existing, p1, m1);
                }
                k += 1;
            }
            self.eobrun -= 1;
        }
        Ok(())
    }

    /// Reached `EOI`: finish a progressive frame's coefficients into
    /// sample planes if needed, then assemble the final image.
    fn finish(&mut self) -> Result<RasterImage, DecodeError> {
        let frame = self
            .frame
            .clone()
            .ok_or(DecodeError::JpegMissingFrameHeader)?;
        if frame.progressive {
            self.finalise_progressive(&frame)?;
        }
        self.assemble(&frame)
    }

    /// Dequantise and inverse-DCT every stored coefficient block of a
    /// progressive frame, once, into fresh sample planes — the one point
    /// at which a progressive frame's buffered coefficients ever become
    /// pixels.
    fn finalise_progressive(&mut self, frame: &Frame) -> Result<(), DecodeError> {
        let m = self.scale.m();
        let mut planes = Vec::with_capacity(frame.components.len());
        for (comp_index, component) in frame.components.iter().enumerate() {
            let stride_blocks = frame.blocks_per_line_padded(component);
            let row_blocks = frame.blocks_per_col_padded(component);
            let plane_width = stride_blocks.saturating_mul(m);
            let plane_height = row_blocks.saturating_mul(m);
            let plane_len =
                usize::try_from(u64::from(plane_width).saturating_mul(u64::from(plane_height)))
                    .unwrap_or(usize::MAX);
            let mut plane = vec![0u8; plane_len];
            let quant = self.quant_tables[usize::from(component.quant_table)]
                .as_ref()
                .ok_or(DecodeError::JpegMissingQuantizationTable)?
                .natural;
            let coeffs_flat = self
                .coefficients
                .get(comp_index)
                .ok_or(DecodeError::DimensionsOverflow)?;
            let plane_stride = usize::try_from(plane_width).unwrap_or(usize::MAX);
            for block_row in 0..row_blocks {
                for block_col in 0..stride_blocks {
                    let start = block_offset(frame, component, block_col, block_row)?;
                    let raw = coeffs_flat
                        .get(start..start + 64)
                        .ok_or(DecodeError::DimensionsOverflow)?;
                    let mut block = [0i32; 64];
                    for (dst, &src) in block.iter_mut().zip(raw) {
                        *dst = i32::from(src);
                    }
                    idct_and_store(
                        &block,
                        &quant,
                        m,
                        &mut plane,
                        plane_stride,
                        usize::try_from(block_row.saturating_mul(m)).unwrap_or(0),
                        usize::try_from(block_col.saturating_mul(m)).unwrap_or(0),
                    );
                }
            }
            planes.push(plane);
        }
        self.sample_planes = planes;
        Ok(())
    }

    /// Crop, upsample, colour-convert, and assemble the final
    /// straight-alpha RGBA8 image from the (already scale-`m`) sample
    /// planes.
    ///
    /// Upsampling is hoisted out of the per-pixel loop entirely: each
    /// component resolves one whole output-width row of samples at a time
    /// (see [`Upsampler`]), so the pixel loop does nothing but read three
    /// bytes and colour-convert, and a component already sampled as densely
    /// as the frame — luma, or every channel of an RGB image — is read
    /// straight from its plane with no copy at all.
    fn assemble(&self, frame: &Frame) -> Result<RasterImage, DecodeError> {
        let m = self.scale.m();
        let output_width = frame.output_width(m);
        let output_height = frame.output_height(m);
        let pixel_count = u64::from(output_width)
            .checked_mul(u64::from(output_height))
            .ok_or(DecodeError::DimensionsOverflow)?;
        let byte_len = pixel_count
            .checked_mul(4)
            .ok_or(DecodeError::DimensionsOverflow)?;
        let mut out =
            vec![0u8; usize::try_from(byte_len).map_err(|_| DecodeError::DimensionsOverflow)?];

        let use_rgb = frame.components.len() == 3 && self.adobe_transform == Some(0);
        let h_max = frame.h_max().max(1);
        let v_max = frame.v_max().max(1);
        let row_bytes =
            usize::try_from(u64::from(output_width).saturating_mul(4)).unwrap_or(usize::MAX);

        let mut upsamplers: Vec<Upsampler<'_>> = frame
            .components
            .iter()
            .enumerate()
            .map(|(i, component)| {
                Upsampler::new(
                    CompMap {
                        plane: self.sample_planes.get(i).map_or(&[][..], Vec::as_slice),
                        stride: usize::try_from(
                            frame.blocks_per_line_padded(component).saturating_mul(m),
                        )
                        .unwrap_or(usize::MAX),
                        h: component.h,
                        v: component.v,
                        x_limit: frame.component_sample_width(component, m).saturating_sub(1),
                        y_limit: frame
                            .component_sample_height(component, m)
                            .saturating_sub(1),
                    },
                    output_width,
                    h_max,
                    v_max,
                )
            })
            .collect();

        for y in 0..output_height {
            let y_byte = usize::try_from(y)
                .unwrap_or(usize::MAX)
                .saturating_mul(row_bytes);
            let Some(out_row) = y_byte
                .checked_add(row_bytes)
                .and_then(|end| out.get_mut(y_byte..end))
            else {
                continue;
            };

            for upsampler in &mut upsamplers {
                upsampler.prepare(y);
            }
            let mut rows: [&[u8]; 3] = [&[], &[], &[]];
            for (row, upsampler) in rows.iter_mut().zip(upsamplers.iter()) {
                *row = upsampler.samples();
            }

            let (pixels, _tail) = out_row.as_chunks_mut::<4>();
            for (x, px) in (0usize..).zip(pixels.iter_mut()) {
                let sample = |row: &[u8]| row.get(x).copied().unwrap_or(0);
                let rgb = if upsamplers.len() == 1 {
                    let s = sample(rows[0]);
                    [s, s, s]
                } else if use_rgb {
                    [sample(rows[0]), sample(rows[1]), sample(rows[2])]
                } else {
                    ycbcr_to_rgb(sample(rows[0]), sample(rows[1]), sample(rows[2]))
                };
                px[0] = rgb[0];
                px[1] = rgb[1];
                px[2] = rgb[2];
                px[3] = 255;
            }
        }
        Ok(RasterImage::from_parts(output_width, output_height, out))
    }
}

/// One component's hoisted upsampling parameters for [`Decoder::assemble`]:
/// its (already scale-`m`) sample plane, that plane's row stride in
/// samples, the last valid sample coordinate on each axis (the crop that
/// keeps padding samples out of the output), and its own sampling factors.
struct CompMap<'a> {
    plane: &'a [u8],
    stride: usize,
    h: u32,
    v: u32,
    x_limit: u32,
    y_limit: u32,
}

impl CompMap<'_> {
    /// The half-open byte range of sample row `y` within the plane.
    fn row_range(&self, y: u32) -> (usize, usize) {
        let start = usize::try_from(y)
            .unwrap_or(usize::MAX)
            .saturating_mul(self.stride);
        (start, start.saturating_add(self.stride))
    }
}

/// Fixed-point one for an upsampling tap weight.
const UPSAMPLE_ONE: u32 = 256;

/// The two samples an upsampled coordinate lies between, and how far
/// towards the second of them it falls.
///
/// `weight` is zero exactly when the coordinate lands on `low` itself, so a
/// component that needs no upsampling on this axis is recognised by its
/// taps rather than by a separate flag.
#[derive(Copy, Clone)]
struct Tap {
    low: u32,
    high: u32,
    weight: u32,
}

/// The samples output coordinate `output` interpolates between, for a
/// component whose sampling factor on this axis is `factor` out of the
/// frame's `max_factor`, with `limit` its last real sample.
///
/// A subsampled component's samples sit at the *centres* of the output
/// pixels they cover, not at their leading edges: output pixel `o` maps to
/// component coordinate `(o + 0.5) * factor / max_factor - 0.5`. Getting
/// that half-sample offset right is the whole point — projecting `o` by a
/// bare ratio instead lands between the real samples and shifts the chroma
/// half a pixel against the luma, which shows as coloured fringing along
/// every hard edge. Carried as a rational in halves of a component sample
/// so the floor and the fraction are both exact.
///
/// A component sampled as densely as the frame maps straight through with
/// no interpolation, which is both exact and free.
fn axis_tap(output: u32, factor: u32, max_factor: u32, limit: u32) -> Tap {
    if factor == max_factor {
        let sample = output.min(limit);
        return Tap {
            low: sample,
            high: sample,
            weight: 0,
        };
    }
    let denominator = 2 * i64::from(max_factor);
    let numerator = (2 * i64::from(output) + 1) * i64::from(factor) - i64::from(max_factor);
    let floor = numerator.div_euclid(denominator);
    let fraction = numerator.rem_euclid(denominator);
    let weight = fraction * i64::from(UPSAMPLE_ONE) / denominator;
    Tap {
        low: clamp_sample(floor, limit),
        high: clamp_sample(floor + 1, limit),
        weight: u32::try_from(weight).unwrap_or(0),
    }
}

/// A component sample coordinate held inside `0..=limit`, so a tap that
/// reaches past an edge reads that edge's own sample rather than a padding
/// sample or nothing at all.
fn clamp_sample(coordinate: i64, limit: u32) -> u32 {
    let held = coordinate.clamp(0, i64::from(limit));
    u32::try_from(held).unwrap_or(0)
}

/// Interpolate between two samples: `low` when `weight` is zero, `high`
/// when it is [`UPSAMPLE_ONE`], rounded to nearest in between.
fn interpolate(low: u8, high: u8, weight: u32) -> u8 {
    let complement = UPSAMPLE_ONE.saturating_sub(weight);
    let sum = u32::from(low) * complement + u32::from(high) * weight + UPSAMPLE_ONE / 2;
    u8::try_from(sum / UPSAMPLE_ONE).unwrap_or(u8::MAX)
}

/// Which buffer the row an [`Upsampler`] last prepared lives in.
#[derive(Copy, Clone)]
enum Prepared {
    /// A range of the component's own sample plane, needing no upsampling
    /// on either axis.
    Plane(usize, usize),
    /// The vertical interpolation of two plane rows, already output-width.
    Blended,
    /// The horizontal interpolation of a plane or blended row.
    Upsampled,
}

/// One component's upsampler: it turns that component's (possibly
/// subsampled) sample plane into one output-width row of samples at a time.
///
/// Chroma is reconstructed by **triangle interpolation** on both axes, the
/// same reconstruction a quality JPEG decoder performs. Replicating each
/// chroma sample across the output pixels it covers instead — the "fast"
/// reconstruction the standard permits — reproduces the chroma grid as
/// 2×2 blocks of flat colour across the whole photograph, and that is
/// visible as blockiness long before any resampling stage is reached.
///
/// The interpolation is planned once, not per pixel: the horizontal taps
/// are the same for every row, so they are computed for the whole output
/// width when the upsampler is built, and each row then costs one pass of
/// multiply-adds rather than a division per sample.
struct Upsampler<'a> {
    map: CompMap<'a>,
    /// The frame's tallest sampling factor, which this component's own
    /// vertical factor is resolved against.
    v_max: u32,
    /// Horizontal taps per output column, empty when the component is
    /// already as wide as the output.
    columns: Vec<Tap>,
    /// The vertical interpolation of two plane rows, empty when the
    /// component is already as tall as the output.
    blended: Vec<u8>,
    /// The finished output-width row, empty when neither axis interpolates.
    upsampled: Vec<u8>,
    prepared: Prepared,
}

impl<'a> Upsampler<'a> {
    /// An upsampler for `map` feeding an `output_width`-wide image whose
    /// frame's widest and tallest components carry `h_max` and `v_max`
    /// samples.
    fn new(map: CompMap<'a>, output_width: u32, h_max: u32, v_max: u32) -> Self {
        let width = usize::try_from(output_width).unwrap_or(usize::MAX);
        let horizontal = map.h != h_max;
        let vertical = map.v != v_max;
        let columns = if horizontal {
            (0..output_width)
                .map(|x| axis_tap(x, map.h, h_max, map.x_limit))
                .collect()
        } else {
            Vec::new()
        };
        let blended = if vertical {
            vec![0u8; map.stride]
        } else {
            Vec::new()
        };
        let upsampled = if horizontal {
            vec![0u8; width]
        } else {
            Vec::new()
        };
        Self {
            map,
            v_max,
            columns,
            blended,
            upsampled,
            prepared: Prepared::Plane(0, 0),
        }
    }

    /// Resolve output row `y` of this component.
    fn prepare(&mut self, y: u32) {
        let tap = axis_tap(y, self.map.v, self.v_max, self.map.y_limit);
        let low = self.map.row_range(tap.low);
        let source = if tap.weight == 0 || self.blended.is_empty() {
            Prepared::Plane(low.0, low.1)
        } else {
            let high = self.map.row_range(tap.high);
            blend_rows(self.map.plane, low, high, tap.weight, &mut self.blended);
            Prepared::Blended
        };
        if self.columns.is_empty() {
            self.prepared = source;
            return;
        }
        let row: &[u8] = match source {
            Prepared::Plane(start, end) => self.map.plane.get(start..end).unwrap_or(&[]),
            _ => self.blended.as_slice(),
        };
        for (tap, slot) in self.columns.iter().zip(self.upsampled.iter_mut()) {
            let low = row.get(tap.low as usize).copied().unwrap_or(0);
            let high = row.get(tap.high as usize).copied().unwrap_or(0);
            *slot = interpolate(low, high, tap.weight);
        }
        self.prepared = Prepared::Upsampled;
    }

    /// The output-width samples the last [`prepare`](Self::prepare)
    /// resolved.
    fn samples(&self) -> &[u8] {
        match self.prepared {
            Prepared::Plane(start, end) => self.map.plane.get(start..end).unwrap_or(&[]),
            Prepared::Blended => self.blended.as_slice(),
            Prepared::Upsampled => self.upsampled.as_slice(),
        }
    }
}

/// Write the interpolation of two plane rows into `out`.
fn blend_rows(
    plane: &[u8],
    low: (usize, usize),
    high: (usize, usize),
    weight: u32,
    out: &mut [u8],
) {
    let top = plane.get(low.0..low.1).unwrap_or(&[]);
    let bottom = plane.get(high.0..high.1).unwrap_or(&[]);
    for (slot, (&above, &below)) in out.iter_mut().zip(top.iter().zip(bottom.iter())) {
        *slot = interpolate(above, below, weight);
    }
}

/// The flat starting index, in a component's coefficient store, of the
/// block at `(block_col, block_row)` (row-major over the padded block
/// grid, 64 entries per block).
fn block_offset(
    frame: &Frame,
    component: &Component,
    block_col: u32,
    block_row: u32,
) -> Result<usize, DecodeError> {
    let stride = frame.blocks_per_line_padded(component);
    let block_index = u64::from(block_row)
        .saturating_mul(u64::from(stride))
        .saturating_add(u64::from(block_col));
    let start = block_index
        .checked_mul(64)
        .ok_or(DecodeError::DimensionsOverflow)?;
    usize::try_from(start).map_err(|_| DecodeError::DimensionsOverflow)
}

/// Decode a baseline/extended-sequential AC band `first..=last` (ITU-T
/// T.81 §F.2.2.2) via `table`, writing each coefficient into `coeffs`
/// (natural order) at its zig-zag position. A sequential block always
/// covers the whole spectrum (`first == 1`, `last == 63`); progressive
/// AC-first has its own variant, since it additionally tracks an
/// end-of-band run spanning several blocks, which this simpler loop does
/// not need to.
fn decode_ac_into(
    bits: &mut BitReader<'_>,
    table: &HuffmanTable,
    first: u32,
    last: u32,
    coeffs: &mut [i32; 64],
) -> Result<(), DecodeError> {
    let mut k = first;
    while k <= last {
        let rs = table.decode(bits)?;
        let run = u32::from(rs >> 4);
        let size = u32::from(rs & 0x0F);
        if size == 0 {
            if run == 15 {
                k += 16;
                continue;
            }
            break;
        }
        k += run;
        if k > last {
            return Err(DecodeError::JpegCoefficientRunOverflow);
        }
        let raw = bits.read_bits(size)?;
        let value = extend(raw, size);
        coeffs[ZIGZAG[usize::try_from(k).unwrap_or(0)]] = value;
        k += 1;
    }
    Ok(())
}

/// Apply one AC-refinement correction bit to an already-nonzero
/// coefficient (ITU-T T.81 §G.1.2.4): the magnitude grows by `p1` (never
/// shrinks, and never changes sign), regardless of which sign `existing`
/// already carries.
fn refine_coefficient(existing: i16, p1: i16, m1: i16) -> i16 {
    if existing >= 0 {
        existing.saturating_add(p1)
    } else {
        existing.saturating_add(m1)
    }
}

/// Widen `value << shift` into an `i16`, saturating rather than
/// overflowing. `shift` is a successive-approximation bit position
/// (`0..=13` in any well-formed stream), but this stays total even for a
/// pathological one: computing in `i64` first means the shift itself can
/// never overflow, and the final range clamp is what actually bounds the
/// stored coefficient.
fn shift_to_i16(value: i32, shift: u32) -> i16 {
    let widened = i64::from(value) << shift.min(62);
    i16::try_from(widened.clamp(i64::from(i16::MIN), i64::from(i16::MAX))).unwrap_or(0)
}

/// Round `value / (1 << scale_bits)` to the nearest integer, correctly
/// for either sign (round-half-away-from-zero).
fn scaled_round(value: i32, scale_bits: u32) -> i32 {
    let denom = 1i64 << scale_bits;
    let half = denom / 2;
    let value = i64::from(value);
    let rounded = if value >= 0 {
        (value + half) / denom
    } else {
        -((-value + half) / denom)
    };
    i32::try_from(rounded).unwrap_or(0)
}

/// Clamp `value` into `0..=255`.
fn clamp_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(0)
}

/// JFIF's fixed-point YCbCr → RGB matrix (ITU-T T.871), scaled by
/// `1 << 16`: `r = y + 1.402(cr-128)`, `g = y - 0.344136(cb-128) -
/// 0.714136(cr-128)`, `b = y + 1.772(cb-128)`.
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    const SCALE_BITS: u32 = 16;
    const CR_TO_R: i32 = 91_881;
    const CB_TO_G: i32 = -22_553;
    const CR_TO_G: i32 = -46_802;
    const CB_TO_B: i32 = 116_130;

    let y = i32::from(y);
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    let r = y + scaled_round(cr.saturating_mul(CR_TO_R), SCALE_BITS);
    let g = y + scaled_round(
        cb.saturating_mul(CB_TO_G)
            .saturating_add(cr.saturating_mul(CR_TO_G)),
        SCALE_BITS,
    );
    let b = y + scaled_round(cb.saturating_mul(CB_TO_B), SCALE_BITS);
    [clamp_u8(r), clamp_u8(g), clamp_u8(b)]
}

/// The five scan shapes ITU-T T.81 §G.1.2 defines. `Sequential` is this
/// decoder's own name for the non-progressive case (baseline/extended
/// sequential), which the standard does not itself subdivide.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ScanKind {
    Sequential,
    DcFirst,
    DcRefine,
    AcFirst,
    AcRefine,
}

impl ScanKind {
    fn classify(progressive: bool, header: &ScanHeader) -> Self {
        if !progressive {
            return Self::Sequential;
        }
        if header.spectral_start == 0 {
            if header.successive_high == 0 {
                Self::DcFirst
            } else {
                Self::DcRefine
            }
        } else if header.successive_high == 0 {
            Self::AcFirst
        } else {
            Self::AcRefine
        }
    }
}

/// Dequantise `coeffs` (natural order) against `quant`, producing the 64
/// dequantised DCT coefficients in natural (row-major) order.
///
/// The product is formed with `wrapping_mul`: for any real 8-bit JPEG the
/// dequantised coefficients are the frame's true DCT coefficients, bounded
/// in magnitude by roughly `2^11`, so no wrap ever occurs and the result is
/// exact. A hostile file can drive a stored coefficient (a saturated DC
/// predictor) or a 16-bit quantiser to values whose product exceeds `i32`;
/// there the wrap is harmless, because both inverse-DCT paths below finish
/// with a fixed clamp to `0..=255`, so a wrapped coefficient can only yield
/// a meaningless-but-in-range pixel, never a panic under the workspace's
/// overflow checks.
fn dequantize(coeffs: &[i32; 64], quant: &[u16; 64]) -> [i32; 64] {
    let mut deq = [0i32; 64];
    for (out, (&coeff, &q)) in deq.iter_mut().zip(coeffs.iter().zip(quant.iter())) {
        *out = coeff.wrapping_mul(i32::from(q));
    }
    deq
}

/// Dequantise `coeffs` and inverse-DCT them at scale `m` (`1`, `2`, `4`, or
/// `8`), writing the resulting `m`×`m` samples (level-shifted by 128 and
/// clamped to `0..=255`) into `plane` at `(row_offset, col_offset)`,
/// `plane_stride` samples per row.
///
/// The full-scale (`m == 8`) case — the overwhelming majority of a
/// non-reduced decode's work — runs the fast fixed-point AAN /
/// Loeffler-Ligtenberg-Moerlein row-column inverse DCT ([`idct8_islow`]).
/// The reduced scales (`m` of `1`, `2`, or `4`) keep the direct
/// scaled-basis matrix routine ([`idct_general`]): at those sizes it
/// evaluates only a handful of outputs and is already cheap, and it is the
/// reference the fast routine's equivalence test checks against. Neither
/// path re-derives the dequantisation, which happens once here.
fn idct_and_store(
    coeffs: &[i32; 64],
    quant: &[u16; 64],
    m: u32,
    plane: &mut [u8],
    plane_stride: usize,
    row_offset: usize,
    col_offset: usize,
) {
    let deq = dequantize(coeffs, quant);
    if m == 8 {
        let samples = idct8_islow(&deq);
        for r in 0..8 {
            let Some(row_samples) = samples.get(r * 8..r * 8 + 8) else {
                continue;
            };
            let row = row_offset.saturating_add(r);
            let dst_start = row.saturating_mul(plane_stride).saturating_add(col_offset);
            if let Some(end) = dst_start.checked_add(8) {
                if let Some(dst) = plane.get_mut(dst_start..end) {
                    dst.copy_from_slice(row_samples);
                }
            }
        }
    } else {
        idct_general(&deq, m, plane, plane_stride, row_offset, col_offset);
    }
}

/// The two guard bits the column pass keeps before the row pass descales
/// them away — libjpeg's `PASS1_BITS`.
const IDCT_PASS1_BITS: u32 = 2;

/// One 8-point 1-D inverse DCT: the standard integer AAN /
/// Loeffler-Ligtenberg-Moerlein butterfly, the same formulation libjpeg
/// names `jpeg_idct_islow` (implemented here directly from the published
/// algorithm, not translated from any source). `s` is 8 (dequantised, or
/// column-pass) inputs; the return is the 8 spatial samples in natural
/// order, still scaled by `2^IDCT_SCALE_BITS` for the caller to descale.
///
/// Every step is `wrapping_*` (the two left shifts are by the constant
/// `IDCT_SCALE_BITS < 32`, which never panics on value overflow). The
/// overflow argument lives on [`idct8_islow`], the two-pass caller.
fn idct_1d(s: &[i32; 8]) -> [i32; 8] {
    // Fixed-point cosine products, `round(c * 2^13)`, matching [`IDCT_BASIS`]'s
    // own 13-bit precision so the two routines agree to within the rounding
    // the JPEG standard's accuracy requirement allows.
    const FIX_0_298631336: i32 = 2446;
    const FIX_0_390180644: i32 = 3196;
    const FIX_0_541196100: i32 = 4433;
    const FIX_0_765366865: i32 = 6270;
    const FIX_0_899976223: i32 = 7373;
    const FIX_1_175875602: i32 = 9633;
    const FIX_1_501321110: i32 = 12299;
    const FIX_1_847759065: i32 = 15137;
    const FIX_1_961570560: i32 = 16069;
    const FIX_2_053119869: i32 = 16819;
    const FIX_2_562915447: i32 = 20995;
    const FIX_3_072711026: i32 = 25172;

    // Even part -> the four symmetric "sum" terms.
    let rot = s[2].wrapping_add(s[6]).wrapping_mul(FIX_0_541196100);
    let rot_hi = rot.wrapping_add(s[6].wrapping_mul(-FIX_1_847759065));
    let rot_lo = rot.wrapping_add(s[2].wrapping_mul(FIX_0_765366865));
    let sum04 = s[0].wrapping_add(s[4]) << IDCT_SCALE_BITS;
    let dif04 = s[0].wrapping_sub(s[4]) << IDCT_SCALE_BITS;
    let even = [
        sum04.wrapping_add(rot_lo),
        dif04.wrapping_add(rot_hi),
        dif04.wrapping_sub(rot_hi),
        sum04.wrapping_sub(rot_lo),
    ];

    // Odd part -> the four "difference" terms.
    let corner = s[7].wrapping_add(s[1]).wrapping_mul(-FIX_0_899976223);
    let centre = s[5].wrapping_add(s[3]).wrapping_mul(-FIX_2_562915447);
    let diag = s[7].wrapping_add(s[3]);
    let anti = s[5].wrapping_add(s[1]);
    let shared = diag.wrapping_add(anti).wrapping_mul(FIX_1_175875602);
    let diag = diag.wrapping_mul(-FIX_1_961570560).wrapping_add(shared);
    let anti = anti.wrapping_mul(-FIX_0_390180644).wrapping_add(shared);
    let odd = [
        s[7].wrapping_mul(FIX_0_298631336)
            .wrapping_add(corner)
            .wrapping_add(diag),
        s[5].wrapping_mul(FIX_2_053119869)
            .wrapping_add(centre)
            .wrapping_add(anti),
        s[3].wrapping_mul(FIX_3_072711026)
            .wrapping_add(centre)
            .wrapping_add(diag),
        s[1].wrapping_mul(FIX_1_501321110)
            .wrapping_add(corner)
            .wrapping_add(anti),
    ];

    // The butterfly: each even term ± its mirror odd term.
    [
        even[0].wrapping_add(odd[3]),
        even[1].wrapping_add(odd[2]),
        even[2].wrapping_add(odd[1]),
        even[3].wrapping_add(odd[0]),
        even[3].wrapping_sub(odd[0]),
        even[2].wrapping_sub(odd[1]),
        even[1].wrapping_sub(odd[2]),
        even[0].wrapping_sub(odd[3]),
    ]
}

/// The fast full-scale (`8`×`8`) inverse DCT: [`idct_1d`] run over the
/// eight columns and then the eight rows. `deq` is the 64 dequantised
/// coefficients in natural order; the return is the 64 level-shifted,
/// clamped `0..=255` samples in natural (row-major) order.
///
/// This replaces the direct routine's per-block `O(8^3)` matrix multiply
/// with `O(8^2)` multiply-adds, in `i32` fixed point rather than `i64`,
/// and takes a per-column shortcut for the flat (all-AC-zero) block a
/// smoothly shaded image produces in abundance — the constant-DC case
/// whose every column reconstructs to `dc << IDCT_PASS1_BITS`.
///
/// Overflow: [`idct_1d`]'s arithmetic is `wrapping_*`. For a valid 8-bit
/// frame `|deq| <= ~2^11`, so the largest intermediate the butterfly forms
/// stays around `2^27` in the column pass and `2^28` in the row pass —
/// comfortably inside `i32` — and no wrap ever occurs: the transform is
/// exact. A hostile file whose coefficients exceed that range can only make
/// an intermediate wrap, and the closing clamp to `0..=255` turns any such
/// value into a valid, if meaningless, sample. The result is therefore
/// total: never a panic under the workspace's overflow checks, and never a
/// pixel outside `0..=255`.
fn idct8_islow(deq: &[i32; 64]) -> [u8; 64] {
    // Two passes' 13 basis bits, the 2 guard bits, and a 3-bit `1/8`.
    const ROW_SHIFT: u32 = IDCT_SCALE_BITS + IDCT_PASS1_BITS + 3;

    let mut ws = [0i32; 64];
    for c in 0..8 {
        let column = [
            deq[c],
            deq[8 + c],
            deq[16 + c],
            deq[24 + c],
            deq[32 + c],
            deq[40 + c],
            deq[48 + c],
            deq[56 + c],
        ];
        // Flat column (a DC-only block's every column): the inverse DCT is
        // the constant DC term, so skip the butterfly.
        if (column[1] | column[2] | column[3] | column[4] | column[5] | column[6] | column[7]) == 0
        {
            let dcval = column[0] << IDCT_PASS1_BITS;
            for k in 0..8 {
                ws[k * 8 + c] = dcval;
            }
            continue;
        }
        let spatial = idct_1d(&column);
        for (k, &value) in spatial.iter().enumerate() {
            ws[k * 8 + c] = descale(value, IDCT_SCALE_BITS - IDCT_PASS1_BITS);
        }
    }

    let mut out = [0u8; 64];
    for r in 0..8 {
        let base = r * 8;
        let row: [i32; 8] = core::array::from_fn(|k| ws[base + k]);
        let spatial = idct_1d(&row);
        for (k, &value) in spatial.iter().enumerate() {
            out[base + k] = clamp_u8(descale(value, ROW_SHIFT).wrapping_add(128));
        }
    }
    out
}

/// Round `x / 2^n` to the nearest integer via a fixed-point descale
/// (`(x + 2^(n-1)) >> n`, an arithmetic shift). Both call sites pass a small
/// compile-time constant, so the round bias folds away; the add wraps
/// (harmless for the hostile-input case [`idct8_islow`] documents) and the
/// shift is exact for the in-range case. The bias shift is saturating so the
/// routine is total for every `n` including `0`, rather than leaving an
/// underflow trap for a later caller to fall into.
const fn descale(x: i32, n: u32) -> i32 {
    x.wrapping_add(1i32.wrapping_shl(n.saturating_sub(1))) >> n
}

/// The direct scaled-basis inverse DCT for the reduced scales (`m` of `1`,
/// `2`, or `4`): a separable two-pass matrix multiply over the top-left
/// `m`×`m` submatrix of [`IDCT_BASIS`], writing the `m`×`m` samples into
/// `plane` at `(row_offset, col_offset)`.
///
/// `deq` is the already-dequantised coefficients in natural order. The
/// accumulation stays in `i64`: a hostile coefficient set can drive the
/// two-pass sum well past `i32`, and `i64` with saturating steps keeps the
/// routine total — a pathological input degrades to a clamped-but-safe
/// pixel, never a panic — at a cost that is irrelevant for the at-most
/// 16 outputs a reduced scale evaluates. (This same routine, invoked at
/// `m == 8`, is the reference [`idct8_islow`]'s equivalence test measures
/// against.)
fn idct_general(
    deq: &[i32; 64],
    m: u32,
    plane: &mut [u8],
    plane_stride: usize,
    row_offset: usize,
    col_offset: usize,
) {
    let m = usize::try_from(m).unwrap_or(8).clamp(1, 8);

    let mut intermediate = [[0i64; 8]; 8];
    for (x, row) in intermediate.iter_mut().enumerate().take(m) {
        for (v, cell) in row.iter_mut().enumerate().take(m) {
            let mut sum = 0i64;
            for (u, &basis) in IDCT_BASIS[x].iter().enumerate().take(m) {
                sum =
                    sum.saturating_add(i64::from(basis).saturating_mul(i64::from(deq[u * 8 + v])));
            }
            *cell = sum;
        }
    }

    let shift_bits = IDCT_SCALE_BITS * 2 + 2; // divide by SCALE^2 * 4
    let denom = 1i64 << shift_bits;
    let half = denom / 2;
    for (x, inter_row) in intermediate.iter().enumerate().take(m) {
        for (y, basis_row) in IDCT_BASIS.iter().enumerate().take(m) {
            let mut sum = 0i64;
            for (&basis, &inter) in basis_row.iter().zip(inter_row.iter()).take(m) {
                sum = sum.saturating_add(i64::from(basis).saturating_mul(inter));
            }
            let rounded = if sum >= 0 {
                sum.saturating_add(half) / denom
            } else {
                -((-sum).saturating_add(half) / denom)
            };
            let sample = clamp_u8(i32::try_from(rounded.saturating_add(128)).unwrap_or(255));
            let row = row_offset.saturating_add(x);
            let col = col_offset.saturating_add(y);
            if let Some(slot) = plane.get_mut(row.saturating_mul(plane_stride).saturating_add(col))
            {
                *slot = sample;
            }
        }
    }
}

#[cfg(test)]
#[path = "jpeg_tests.rs"]
mod tests;
