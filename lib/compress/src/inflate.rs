//! RFC 1951 DEFLATE decompression.
//!
//! This module is decode-only interoperability with a **foreign** format:
//! PNG's `IDAT` stream is a zlib-wrapped DEFLATE stream (RFC 1950 / RFC
//! 1951), and `lib/image`'s PNG decoder is the reason this module exists.
//! TAIRiX-native compression stays the crate's own RLZ codec (the crate's
//! top-level rustdoc) — this module never becomes a replacement for it, and
//! there is deliberately no DEFLATE **compressor**: nothing in the tree
//! produces a DEFLATE stream, only foreign encoders do, so only the decode
//! direction is implemented.
//!
//! # Bit and code ordering
//!
//! Per RFC 1951 §3.1.1, multi-bit *values* (block-type bits, HLIT/HDIST/
//! HCLEN, extra-length and extra-distance bits) are packed least-significant
//! bit first, while a Huffman *code* is packed most-significant bit first.
//! The internal `BitReader` serves the former; the internal `decode_symbol`
//! serves the latter by growing the candidate code one bit at a time.
//!
//! # Huffman table strategy
//!
//! Symbols are decoded through the canonical count/offset walk from the
//! RFC's reference algorithm (as implemented by zlib's public-domain `puff`
//! decoder): the internal `build_huffman` counts codes per bit length,
//! checks the codespace is neither over- nor under-subscribed, and lays out
//! symbols in canonical order; `decode_symbol` then reads one bit at a
//! time, tracking the first code and symbol-table offset at each length,
//! and returns as soon as the bit-length band containing the accumulated
//! code is reached. This is O(code length) per symbol — never a linear scan
//! over all codes — and needs no allocation: every alphabet in RFC 1951
//! (288 literal/length symbols, 32 distance symbols, 19 code-length
//! symbols) fits a fixed-size table, so the internal `HuffmanTable` is a
//! plain array, keeping this module exactly as allocation-free as the rest
//! of the crate.
//!
//! A canonical Huffman code set is normally required to be *complete*
//! (every codepoint reachable): RFC 1951 permits exactly one exception,
//! reproduced from the reference decoder — a set with precisely one
//! nonzero-length code, of length 1, is under-subscribed but still legal
//! (some encoders emit this degenerate single-code table for a symbol
//! alphabet, such as a distance alphabet, that a particular block never
//! actually uses). `build_huffman` reports this case to the caller, which
//! accepts it only for the literal/length and distance tables — the
//! code-length alphabet that describes them must always be complete.
//!
//! # Trailing-byte policy
//!
//! [`inflate_into`] decompresses the whole stream in `src` and returns only
//! the number of bytes produced, per its signature. The zlib envelope
//! (`crate::zlib`) additionally needs to know exactly where the DEFLATE
//! stream ends within `src`, to locate the Adler-32 trailer that follows
//! it — a boundary that is only known once the final block has been
//! decoded, since DEFLATE carries no overall compressed-length field.
//! [`inflate_into_consumed`] is the shared implementation both entry points
//! call: it reports that boundary (rounded up to the byte containing the
//! last bit of the final block — DEFLATE never leaves a stream mid-byte) as
//! a second return value. Bytes in `src` beyond that boundary are never
//! inspected by this module; whether they are trailing garbage or, as in
//! zlib, a meaningful trailer is entirely the caller's concern.

/// Why decompression failed. Every variant is a fail-closed refusal: no
/// malformed, truncated, or adversarial DEFLATE stream produces a panic or a
/// silently wrong answer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// The stream ended before a block, code, or extra-bits field it
    /// declared was fully read.
    UnexpectedEof,
    /// A block's 2-bit type field was `3` (reserved, never valid).
    InvalidBlockType,
    /// A stored block's `LEN` and one's-complement `NLEN` fields disagreed.
    InvalidStoredBlockLength,
    /// A Huffman code-length set claimed more codes than its bit-length
    /// budget can hold.
    OversubscribedHuffmanCode,
    /// A Huffman code-length set left codepoints unreachable, and this
    /// table is not the single-code case RFC 1951 permits it for.
    IncompleteHuffmanCode,
    /// A code-length repeat symbol (16, 17, or 18) was invalid where it
    /// appeared — a `16` with no preceding length, or a repeat count that
    /// would overrun the declared number of code lengths.
    InvalidLengthRepeat,
    /// A decoded literal/length or distance symbol was outside its valid
    /// range, or no symbol matched the accumulated Huffman code.
    InvalidSymbol,
    /// A back-reference distance pointed further back than any byte this
    /// call has produced so far.
    DistanceTooFar,
    /// `dst` is too small to hold the decompressed output.
    OutputOverflow,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::UnexpectedEof => "deflate stream ended unexpectedly",
            Self::InvalidBlockType => "invalid deflate block type",
            Self::InvalidStoredBlockLength => "stored block LEN/NLEN mismatch",
            Self::OversubscribedHuffmanCode => "oversubscribed huffman code set",
            Self::IncompleteHuffmanCode => "incomplete huffman code set",
            Self::InvalidLengthRepeat => "invalid code-length repeat",
            Self::InvalidSymbol => "invalid or out-of-range symbol",
            Self::DistanceTooFar => "back-reference distance exceeds output produced so far",
            Self::OutputOverflow => "destination buffer is too small",
        };
        f.write_str(text)
    }
}

/// Largest Huffman code length RFC 1951 permits.
const MAX_BITS: usize = 15;

/// Largest symbol alphabet used by any table in RFC 1951 (the 0..=287
/// literal/length alphabet). Sizing every [`HuffmanTable`] to this bound
/// keeps the table a plain fixed-size array, so decoding never allocates.
const MAX_SYMBOLS: usize = 288;

/// Number of symbols in the code-length alphabet (RFC 1951 §3.2.7).
const CODE_LENGTH_SYMBOLS: usize = 19;

/// The order code-length code lengths are transmitted in (RFC 1951
/// §3.2.7) — deliberately not ascending, so the common case of a handful of
/// short lengths and many omitted (zero) ones front-loads the codes an
/// encoder is likely to actually use.
const CODE_LENGTH_ORDER: [usize; CODE_LENGTH_SYMBOLS] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Base length for length symbols 257..=285, indexed by `symbol - 257`
/// (RFC 1951 §3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// Extra bits following each length symbol, same indexing as
/// [`LENGTH_BASE`].
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Base distance for distance symbols 0..=29 (RFC 1951 §3.2.5).
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// Extra bits following each distance symbol, same indexing as
/// [`DIST_BASE`].
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// A least-significant-bit-first reader over a byte slice.
///
/// RFC 1951 packs ordinary multi-bit fields LSB-first; Huffman codes are the
/// one exception ([`decode_symbol`] handles their MSB-first packing itself,
/// one bit at a time, so it reads through the same [`BitReader::bit`]
/// primitive).
struct BitReader<'a> {
    src: &'a [u8],
    pos: usize,
    buf: u32,
    count: u32,
}

impl<'a> BitReader<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            pos: 0,
            buf: 0,
            count: 0,
        }
    }

    /// Read a single bit, refilling the bit buffer one byte at a time.
    fn bit(&mut self) -> Result<u32, Error> {
        if self.count == 0 {
            let byte = *self.src.get(self.pos).ok_or(Error::UnexpectedEof)?;
            self.pos += 1;
            self.buf = u32::from(byte);
            self.count = 8;
        }
        let bit = self.buf & 1;
        self.buf >>= 1;
        self.count -= 1;
        Ok(bit)
    }

    /// Read an `n`-bit value, LSB-first (RFC 1951 §3.1.1). `n` is always a
    /// small compile-time-bounded count (at most 13, the widest DEFLATE
    /// extra-bits field), so no overflow is possible.
    fn bits(&mut self, n: u32) -> Result<u32, Error> {
        let mut value = 0u32;
        for i in 0..n {
            value |= self.bit()? << i;
        }
        Ok(value)
    }

    /// Discard any unread bits in the current byte, so the next read starts
    /// a fresh byte (RFC 1951 §3.2.4 — stored blocks begin byte-aligned).
    fn align_to_byte(&mut self) {
        self.buf = 0;
        self.count = 0;
    }

    /// Read one whole byte. The caller is responsible for having aligned to
    /// a byte boundary first when that matters (stored blocks).
    fn read_byte(&mut self) -> Result<u8, Error> {
        let byte = *self.src.get(self.pos).ok_or(Error::UnexpectedEof)?;
        self.pos += 1;
        Ok(byte)
    }

    /// Bytes of `src` consumed so far, rounded up to include a byte the
    /// buffer partially holds bits from (DEFLATE never needs to distinguish
    /// the two: the stream never resumes mid-byte after this point).
    fn consumed(&self) -> usize {
        self.pos
    }
}

/// A canonical Huffman decode table: how many codes exist at each bit
/// length, and which symbol each canonically-ordered code maps to.
struct HuffmanTable {
    count: [u16; MAX_BITS + 1],
    symbol: [u16; MAX_SYMBOLS],
}

/// Build a canonical Huffman table from per-symbol bit lengths (`0` marks an
/// unused symbol).
///
/// Returns the table plus whether the code set was *incomplete* (left
/// codespace unreached). The caller decides whether an incomplete result is
/// tolerated — RFC 1951 permits it only for a table with a single
/// length-1 code (see the module documentation).
///
/// # Errors
///
/// [`Error::OversubscribedHuffmanCode`] if the declared lengths claim more
/// codes than the bit-length budget can hold.
fn build_huffman(lengths: &[u8]) -> Result<(HuffmanTable, bool), Error> {
    let mut count = [0u16; MAX_BITS + 1];
    for &len in lengths {
        count[usize::from(len)] += 1;
    }

    // Walk the codespace one bit length at a time: `left` is the number of
    // not-yet-assigned codepoints remaining at the current length. Starting
    // from one root codepoint, each additional bit doubles the available
    // codepoints; subtracting this length's code count can never go
    // negative for a valid (non-oversubscribed) set.
    let mut left: i32 = 1;
    for &codes_at_len in &count[1..=MAX_BITS] {
        let doubled = left
            .checked_mul(2)
            .ok_or(Error::OversubscribedHuffmanCode)?;
        left = doubled
            .checked_sub(i32::from(codes_at_len))
            .ok_or(Error::OversubscribedHuffmanCode)?;
        if left < 0 {
            return Err(Error::OversubscribedHuffmanCode);
        }
    }
    let incomplete = left > 0;

    // The first canonical-order table index for each length, derived from
    // how many shorter codes precede it.
    let mut offset = [0u16; MAX_BITS + 1];
    for len in 1..MAX_BITS {
        offset[len + 1] = offset[len] + count[len];
    }

    let mut symbol = [0u16; MAX_SYMBOLS];
    for (index, &len) in lengths.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let slot = offset
            .get_mut(usize::from(len))
            .ok_or(Error::OversubscribedHuffmanCode)?;
        let symbol_slot = symbol
            .get_mut(usize::from(*slot))
            .ok_or(Error::OversubscribedHuffmanCode)?;
        *symbol_slot = u16::try_from(index).map_err(|_| Error::OversubscribedHuffmanCode)?;
        *slot += 1;
    }

    Ok((HuffmanTable { count, symbol }, incomplete))
}

/// Whether an incomplete [`build_huffman`] result is the single spec-legal
/// degenerate case: exactly one symbol has a nonzero length, and that
/// length is 1.
fn is_permitted_incomplete(table: &HuffmanTable) -> bool {
    table.count[1] == 1 && table.count[2..].iter().all(|&c| c == 0)
}

/// Decode one symbol from `bits` using canonical table `table`.
///
/// Reads one bit at a time, most-significant-bit first, tracking the first
/// code and symbol-table offset seen at each length so far — the reference
/// canonical count/offset walk (RFC 1951's normative decoding algorithm) —
/// rather than a linear scan over every code in the table.
fn decode_symbol(table: &HuffmanTable, bits: &mut BitReader<'_>) -> Result<u16, Error> {
    let mut code: i32 = 0;
    let mut first: i32 = 0;
    let mut index: usize = 0;
    for len in 1..=MAX_BITS {
        code |= i32::try_from(bits.bit()?).unwrap_or(0);
        let count = i32::from(table.count[len]);
        if code - count < first {
            let offset = usize::try_from(code - first).map_err(|_| Error::InvalidSymbol)?;
            return table
                .symbol
                .get(index + offset)
                .copied()
                .ok_or(Error::InvalidSymbol);
        }
        index += usize::from(table.count[len]);
        first = (first + count) << 1;
        code <<= 1;
    }
    Err(Error::InvalidSymbol)
}

/// The fixed literal/length code lengths (RFC 1951 §3.2.6), used by
/// `BTYPE = 01` blocks.
fn fixed_literal_length_lengths() -> [u8; MAX_SYMBOLS] {
    let mut lengths = [0u8; MAX_SYMBOLS];
    lengths[0..144].fill(8);
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    lengths[280..288].fill(8);
    lengths
}

/// The fixed distance code lengths (RFC 1951 §3.2.6): every one of the 32
/// codes is 5 bits, even though only 0..=29 are ever legally used.
const FIXED_DISTANCE_LENGTHS: [u8; 32] = [5; 32];

/// Build the tables for a fixed-Huffman (`BTYPE = 01`) block.
///
/// The fixed lengths are a compile-time-known-good constant of the format,
/// so this can never actually observe an oversubscribed or (impermissibly)
/// incomplete code; propagating the `Result` rather than assuming it keeps
/// this module free of `unwrap`/`expect` even here.
fn fixed_tables() -> Result<(HuffmanTable, HuffmanTable), Error> {
    let (lit_table, lit_incomplete) = build_huffman(&fixed_literal_length_lengths())?;
    if lit_incomplete {
        return Err(Error::IncompleteHuffmanCode);
    }
    let (dist_table, dist_incomplete) = build_huffman(&FIXED_DISTANCE_LENGTHS)?;
    if dist_incomplete {
        return Err(Error::IncompleteHuffmanCode);
    }
    Ok((lit_table, dist_table))
}

/// Read a dynamic-Huffman (`BTYPE = 10`) block header and build its
/// literal/length and distance tables (RFC 1951 §3.2.7).
fn dynamic_tables(bits: &mut BitReader<'_>) -> Result<(HuffmanTable, HuffmanTable), Error> {
    let hlit = usize::try_from(bits.bits(5)?).unwrap_or(0) + 257;
    let hdist = usize::try_from(bits.bits(5)?).unwrap_or(0) + 1;
    let hclen = usize::try_from(bits.bits(4)?).unwrap_or(0) + 4;

    let mut code_length_lengths = [0u8; CODE_LENGTH_SYMBOLS];
    for &position in CODE_LENGTH_ORDER.iter().take(hclen) {
        code_length_lengths[position] = u8::try_from(bits.bits(3)?).unwrap_or(0);
    }
    let (code_length_table, incomplete) = build_huffman(&code_length_lengths)?;
    // The code-length alphabet describing the real tables must always be
    // complete: RFC 1951's single-code exception applies only to the
    // literal/length and distance tables it goes on to describe.
    if incomplete {
        return Err(Error::IncompleteHuffmanCode);
    }

    let total = hlit + hdist;
    let mut lengths = [0u8; MAX_SYMBOLS + 32];
    let mut index = 0usize;
    while index < total {
        let symbol = decode_symbol(&code_length_table, bits)?;
        match symbol {
            0..=15 => {
                let slot = lengths.get_mut(index).ok_or(Error::InvalidLengthRepeat)?;
                *slot = u8::try_from(symbol).unwrap_or(0);
                index += 1;
            }
            16..=18 => {
                let (repeat, value) = if symbol == 16 {
                    let previous = index.checked_sub(1).ok_or(Error::InvalidLengthRepeat)?;
                    let value = *lengths.get(previous).ok_or(Error::InvalidLengthRepeat)?;
                    (3 + bits.bits(2)?, value)
                } else if symbol == 17 {
                    (3 + bits.bits(3)?, 0)
                } else {
                    (11 + bits.bits(7)?, 0)
                };
                let repeat = usize::try_from(repeat).unwrap_or(0);
                let end = index
                    .checked_add(repeat)
                    .ok_or(Error::InvalidLengthRepeat)?;
                if end > total {
                    return Err(Error::InvalidLengthRepeat);
                }
                lengths
                    .get_mut(index..end)
                    .ok_or(Error::InvalidLengthRepeat)?
                    .fill(value);
                index = end;
            }
            _ => return Err(Error::InvalidSymbol),
        }
    }

    let (lit_table, lit_incomplete) = build_huffman(&lengths[..hlit])?;
    if lit_incomplete && !is_permitted_incomplete(&lit_table) {
        return Err(Error::IncompleteHuffmanCode);
    }
    let (dist_table, dist_incomplete) = build_huffman(&lengths[hlit..total])?;
    if dist_incomplete && !is_permitted_incomplete(&dist_table) {
        return Err(Error::IncompleteHuffmanCode);
    }
    Ok((lit_table, dist_table))
}

/// Decode one Huffman-coded block's symbols into `output[..]`, starting at
/// `out`, until the end-of-block symbol (256). Returns the new `out`.
fn inflate_block(
    lit_table: &HuffmanTable,
    dist_table: &HuffmanTable,
    bits: &mut BitReader<'_>,
    output: &mut [u8],
    mut out: usize,
) -> Result<usize, Error> {
    loop {
        let symbol = decode_symbol(lit_table, bits)?;
        match symbol {
            0..=255 => {
                let byte = u8::try_from(symbol).unwrap_or(0);
                *output.get_mut(out).ok_or(Error::OutputOverflow)? = byte;
                out += 1;
            }
            256 => return Ok(out),
            257..=285 => {
                let length_index = usize::from(symbol) - 257;
                let extra = bits.bits(u32::from(LENGTH_EXTRA[length_index]))?;
                let length =
                    usize::from(LENGTH_BASE[length_index]) + usize::try_from(extra).unwrap_or(0);

                let dist_symbol = decode_symbol(dist_table, bits)?;
                let dist_index = usize::from(dist_symbol);
                let dist_base = *DIST_BASE.get(dist_index).ok_or(Error::InvalidSymbol)?;
                let dist_extra_bits = *DIST_EXTRA.get(dist_index).ok_or(Error::InvalidSymbol)?;
                let extra = bits.bits(u32::from(dist_extra_bits))?;
                let distance = usize::from(dist_base) + usize::try_from(extra).unwrap_or(0);

                if distance > out || distance == 0 {
                    return Err(Error::DistanceTooFar);
                }
                let end = out.checked_add(length).ok_or(Error::OutputOverflow)?;
                if end > output.len() {
                    return Err(Error::OutputOverflow);
                }
                let mut from = out - distance;
                while out < end {
                    output[out] = output[from];
                    out += 1;
                    from += 1;
                }
            }
            _ => return Err(Error::InvalidSymbol),
        }
    }
}

/// Decompress the DEFLATE stream in `src` into `dst`, returning `(bytes
/// produced, bytes of `src` consumed)`.
///
/// See the module documentation for the trailing-byte policy: `consumed` is
/// the byte offset immediately following the final block, which is what a
/// caller (the zlib envelope) needs to locate a trailer that follows the
/// stream. This is the shared implementation behind [`inflate_into`].
///
/// # Errors
///
/// See [`Error`] for every fail-closed refusal reason.
pub fn inflate_into_consumed(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), Error> {
    let mut bits = BitReader::new(src);
    let mut out = 0usize;
    loop {
        let bfinal = bits.bit()?;
        let btype = bits.bits(2)?;
        match btype {
            0 => {
                bits.align_to_byte();
                let len = u16::from_le_bytes([bits.read_byte()?, bits.read_byte()?]);
                let nlen = u16::from_le_bytes([bits.read_byte()?, bits.read_byte()?]);
                if len != !nlen {
                    return Err(Error::InvalidStoredBlockLength);
                }
                let len = usize::from(len);
                let end = out.checked_add(len).ok_or(Error::OutputOverflow)?;
                if end > dst.len() {
                    return Err(Error::OutputOverflow);
                }
                for slot in &mut dst[out..end] {
                    *slot = bits.read_byte()?;
                }
                out = end;
            }
            1 => {
                let (lit_table, dist_table) = fixed_tables()?;
                out = inflate_block(&lit_table, &dist_table, &mut bits, dst, out)?;
            }
            2 => {
                let (lit_table, dist_table) = dynamic_tables(&mut bits)?;
                out = inflate_block(&lit_table, &dist_table, &mut bits, dst, out)?;
            }
            _ => return Err(Error::InvalidBlockType),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok((out, bits.consumed()))
}

/// Decompress the whole DEFLATE stream in `src` into `dst`, returning the
/// number of bytes produced.
///
/// See the module documentation for the huffman-table strategy and the
/// trailing-byte policy. Memory is bounded up front: every write goes
/// through a checked index into the caller-provided `dst`, so an
/// [`Error::OutputOverflow`] is returned the moment the declared/implied
/// output would exceed it, before any out-of-bounds byte is touched.
///
/// # Errors
///
/// See [`Error`] for every fail-closed refusal reason.
pub fn inflate_into(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    inflate_into_consumed(src, dst).map(|(produced, _consumed)| produced)
}

#[cfg(test)]
#[path = "inflate_tests.rs"]
mod tests;
