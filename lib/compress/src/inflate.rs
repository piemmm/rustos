//! RFC 1951 DEFLATE decompression.
//!
//! One resumable state machine serves two shapes. [`inflate_into`] decodes a
//! whole stream in one call — what `lib/image`'s PNG `IDAT` path wants, and
//! what a *foreign* encoder's finished output is. [`Inflater`] carries the
//! same machine plus a 32 KiB history window across calls, so a stream that
//! arrives in pieces — one SSH packet at a time, back-referencing packets
//! already delivered — decodes without ever holding the whole of it.
//!
//! A peer that flushes with zlib's `Z_PARTIAL_FLUSH` leaves the bit stream
//! mid-byte at a packet boundary, so the machine suspends and resumes at an
//! arbitrary bit, not merely between blocks. [`Inflater`] absorbs every byte
//! it is handed, so a caller never has to carry a remainder forward.
//!
//! # Bit and code ordering
//!
//! Per RFC 1951 §3.1.1, multi-bit *values* (block-type bits, HLIT/HDIST/
//! HCLEN, extra-length and extra-distance bits) are packed least-significant
//! bit first, while a Huffman *code* is packed most-significant bit first.
//! The internal bit accumulator serves the former; `decode_symbol` serves the
//! latter by growing the candidate code one bit at a time.
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
//! [`inflate_into_consumed`] reports that boundary (rounded up to the byte
//! containing the last bit of the final block — DEFLATE never leaves a
//! stream mid-byte) as a second return value. Bytes in `src` beyond that
//! boundary are never inspected by this module; whether they are trailing
//! garbage or, as in zlib, a meaningful trailer is entirely the caller's
//! concern. [`Progress::consumed`] reports the same boundary once
//! [`Progress::finished`] is set.

use crate::format::{
    fixed_literal_length_lengths, CODE_LENGTH_ORDER, CODE_LENGTH_SYMBOLS, DIST_BASE, DIST_EXTRA,
    FIXED_DISTANCE_LENGTHS, LENGTH_BASE, LENGTH_EXTRA, LIT_SYMBOLS, MAX_BITS, WINDOW_SIZE,
};

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
    /// A back-reference distance pointed further back than any byte the
    /// stream has produced, or than the history window still holds.
    DistanceTooFar,
    /// `dst` is too small to hold the decompressed output.
    OutputOverflow,
    /// More input was fed to a stream that had already ended.
    Finished,
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
            Self::DistanceTooFar => "back-reference distance exceeds the history available",
            Self::OutputOverflow => "destination buffer is too small",
            Self::Finished => "deflate stream is already finished",
        };
        f.write_str(text)
    }
}

/// Largest symbol alphabet used by any table in RFC 1951 (the 0..=287
/// literal/length alphabet). Sizing every [`HuffmanTable`] to this bound
/// keeps the table a plain fixed-size array, so decoding never allocates.
const MAX_SYMBOLS: usize = LIT_SYMBOLS;

/// A canonical Huffman decode table: how many codes exist at each bit
/// length, and which symbol each canonically-ordered code maps to.
#[derive(Copy, Clone)]
struct HuffmanTable {
    count: [u16; MAX_BITS + 1],
    symbol: [u16; MAX_SYMBOLS],
}

impl Default for HuffmanTable {
    fn default() -> Self {
        Self {
            count: [0; MAX_BITS + 1],
            symbol: [0; MAX_SYMBOLS],
        }
    }
}

/// Build a canonical Huffman table from per-symbol bit lengths (`0` marks an
/// unused symbol).
///
/// Returns the table plus whether the code set was *incomplete* (left
/// codespace unreached). The caller decides whether an incomplete result is
/// tolerated — RFC 1951 permits it only for a table with a single
/// length-1 code (see the module documentation).
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

/// The bit accumulator.
///
/// It pulls a byte only when a read actually needs one, so once a read has
/// completed it holds fewer than eight bits — which is what makes the byte
/// offset the decoder reports at the end of a stream the exact offset of
/// whatever follows it, rather than a few bytes past.
///
/// A step that needs a symbol *and* its trailing extra-bit field must take
/// both or neither, so reads are addressed by an `at` offset into the held
/// bits and only [`Bits::consume`] makes them gone. Bytes pulled along the
/// way are absorbed either way: a caller never has to re-present them.
#[derive(Copy, Clone, Default)]
struct Bits {
    hold: u64,
    count: u32,
}

/// Held bits above which no further byte is pulled, leaving room for the
/// widest step (a 15-bit code and its 13 extra bits).
const HOLD_LIMIT: u32 = 48;

impl Bits {
    /// Hold at least `want` bits, pulling from `src`.
    fn ensure(&mut self, src: &[u8], pos: &mut usize, want: u32) -> bool {
        while self.count < want {
            if self.count > HOLD_LIMIT {
                return false;
            }
            let Some(&byte) = src.get(*pos) else {
                return false;
            };
            *pos += 1;
            self.hold |= u64::from(byte) << self.count;
            self.count += 8;
        }
        true
    }

    /// The bit `at` positions into the held bits.
    fn bit(&mut self, src: &[u8], pos: &mut usize, at: u32) -> Option<u32> {
        if !self.ensure(src, pos, at + 1) {
            return None;
        }
        Some(u32::try_from((self.hold >> at) & 1).unwrap_or(0))
    }

    /// The `n`-bit field `at` positions into the held bits.
    fn field(&mut self, src: &[u8], pos: &mut usize, at: u32, n: u32) -> Option<u32> {
        if !self.ensure(src, pos, at + n) {
            return None;
        }
        if n == 0 {
            return Some(0);
        }
        let mask = (1u64 << n) - 1;
        u32::try_from((self.hold >> at) & mask).ok()
    }

    /// Drop the low `n` bits for good.
    fn consume(&mut self, n: u32) {
        self.hold >>= n;
        self.count -= n;
    }

    /// Discard the rest of the current byte (RFC 1951 §3.2.4 — a stored
    /// block begins byte-aligned).
    fn align(&mut self) {
        self.consume(self.count % 8);
    }
}

/// Decode one symbol from `bits`, starting `at` bits into what is held.
///
/// Reads one bit at a time, most-significant-bit first, tracking the first
/// code and symbol-table offset seen at each length so far — the reference
/// canonical count/offset walk (RFC 1951's normative decoding algorithm) —
/// rather than a linear scan over every code in the table. Returns the
/// symbol and the bits it spanned, or `Ok(None)` when the input ran out
/// before a code completed. Nothing is consumed either way.
fn decode_symbol(
    table: &HuffmanTable,
    bits: &mut Bits,
    src: &[u8],
    pos: &mut usize,
    at: u32,
) -> Result<Option<(u16, u32)>, Error> {
    let mut code: i32 = 0;
    let mut first: i32 = 0;
    let mut index: usize = 0;
    for len in 1..=MAX_BITS {
        let used = u32::try_from(len).unwrap_or(0);
        let Some(bit) = bits.bit(src, pos, at + used - 1) else {
            return Ok(None);
        };
        code |= i32::try_from(bit).unwrap_or(0);
        let count = i32::from(table.count[len]);
        if code - count < first {
            let offset = usize::try_from(code - first).map_err(|_| Error::InvalidSymbol)?;
            return table
                .symbol
                .get(index + offset)
                .copied()
                .map(|symbol| Some((symbol, used)))
                .ok_or(Error::InvalidSymbol);
        }
        index += usize::from(table.count[len]);
        first = (first + count) << 1;
        code <<= 1;
    }
    Err(Error::InvalidSymbol)
}

/// Where the decoder is between calls.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Mode {
    BlockHeader,
    StoredHeader,
    StoredCopy,
    TableHeader,
    CodeLengths,
    LengthSequence,
    Symbol,
    Distance,
    CopyOut,
    Literal(u8),
    Done,
}

/// The decoder proper: everything a suspended stream must remember except
/// the history a back-reference may reach into.
struct Core {
    mode: Mode,
    bits: Bits,
    last_block: bool,
    lit_table: HuffmanTable,
    dist_table: HuffmanTable,
    code_length_table: HuffmanTable,
    code_length_lengths: [u8; CODE_LENGTH_SYMBOLS],
    lengths: [u8; MAX_SYMBOLS + 32],
    hlit: usize,
    hdist: usize,
    hclen: usize,
    have: usize,
    stored_remaining: usize,
    copy_length: usize,
    copy_distance: usize,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            mode: Mode::BlockHeader,
            bits: Bits::default(),
            last_block: false,
            lit_table: HuffmanTable::default(),
            dist_table: HuffmanTable::default(),
            code_length_table: HuffmanTable::default(),
            code_length_lengths: [0; CODE_LENGTH_SYMBOLS],
            lengths: [0; MAX_SYMBOLS + 32],
            hlit: 0,
            hdist: 0,
            hclen: 0,
            have: 0,
            stored_remaining: 0,
            copy_length: 0,
            copy_distance: 0,
        }
    }
}

/// The last [`WINDOW_SIZE`] bytes produced, which a back-reference in a
/// later call may still reach into.
struct History {
    buf: [u8; WINDOW_SIZE],
    next: usize,
    have: usize,
}

impl Default for History {
    // The 32 KiB window is the format's, and this crate links no allocator,
    // so the value is returned for the caller to box rather than placed here.
    #[allow(clippy::large_stack_arrays)]
    fn default() -> Self {
        Self {
            buf: [0; WINDOW_SIZE],
            next: 0,
            have: 0,
        }
    }
}

impl History {
    /// The byte `back` positions before the most recent one.
    fn byte(&self, back: usize) -> Option<u8> {
        if back == 0 || back > self.have {
            return None;
        }
        self.buf
            .get((self.next + WINDOW_SIZE - back) % WINDOW_SIZE)
            .copied()
    }

    /// Record `data` as the most recent output. Only its tail can ever be
    /// reached again, so only the tail is copied.
    fn push(&mut self, data: &[u8]) {
        let tail = data
            .get(data.len().saturating_sub(WINDOW_SIZE)..)
            .unwrap_or(data);
        let first = (WINDOW_SIZE - self.next).min(tail.len());
        self.buf[self.next..self.next + first].copy_from_slice(&tail[..first]);
        let wrapped = tail.len() - first;
        if wrapped > 0 {
            self.buf[..wrapped].copy_from_slice(&tail[first..]);
        }
        self.next = (self.next + tail.len()) % WINDOW_SIZE;
        self.have = (self.have + tail.len()).min(WINDOW_SIZE);
    }
}

/// How far one decode call got.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Progress {
    /// Bytes of `src` absorbed. Everything up to here is now in the
    /// decoder's state; a caller never re-presents them.
    pub consumed: usize,
    /// Bytes written to `dst`.
    pub produced: usize,
    /// Whether the final block has been decoded.
    pub finished: bool,
}

impl Core {
    /// Read `n` bits, or nothing at all if the input cannot supply them.
    fn take(&mut self, src: &[u8], pos: &mut usize, n: u32) -> Option<u32> {
        let value = self.bits.field(src, pos, 0, n)?;
        self.bits.consume(n);
        Some(value)
    }

    /// Install the fixed code set a `BTYPE = 01` block uses.
    ///
    /// The fixed lengths are a compile-time-known-good constant of the
    /// format, so this can never actually observe an oversubscribed or
    /// (impermissibly) incomplete code; propagating the `Result` rather than
    /// assuming it keeps this module free of `unwrap`/`expect` even here.
    fn fixed_tables(&mut self) -> Result<(), Error> {
        let (lit_table, lit_incomplete) = build_huffman(&fixed_literal_length_lengths())?;
        let (dist_table, dist_incomplete) = build_huffman(&FIXED_DISTANCE_LENGTHS)?;
        if lit_incomplete || dist_incomplete {
            return Err(Error::IncompleteHuffmanCode);
        }
        self.lit_table = lit_table;
        self.dist_table = dist_table;
        Ok(())
    }

    /// Install the code set a `BTYPE = 10` block transmitted.
    fn dynamic_tables(&mut self) -> Result<(), Error> {
        let total = self.hlit + self.hdist;
        let (lit_table, lit_incomplete) = build_huffman(&self.lengths[..self.hlit])?;
        if lit_incomplete && !is_permitted_incomplete(&lit_table) {
            return Err(Error::IncompleteHuffmanCode);
        }
        let (dist_table, dist_incomplete) = build_huffman(&self.lengths[self.hlit..total])?;
        if dist_incomplete && !is_permitted_incomplete(&dist_table) {
            return Err(Error::IncompleteHuffmanCode);
        }
        self.lit_table = lit_table;
        self.dist_table = dist_table;
        Ok(())
    }
}

impl Core {
    /// Decode from `src` into `dst`, suspending when either runs out.
    fn run(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        history: Option<&History>,
    ) -> Result<Progress, Error> {
        let mut pos = 0usize;
        let mut produced = 0usize;
        loop {
            match self.mode {
                Mode::Done => break,
                Mode::BlockHeader => {
                    let Some(value) = self.take(src, &mut pos, 3) else {
                        break;
                    };
                    self.last_block = value & 1 == 1;
                    match value >> 1 {
                        0 => self.mode = Mode::StoredHeader,
                        1 => {
                            self.fixed_tables()?;
                            self.mode = Mode::Symbol;
                        }
                        2 => self.mode = Mode::TableHeader,
                        _ => return Err(Error::InvalidBlockType),
                    }
                }
                Mode::StoredHeader => {
                    if !self.stored_header(src, &mut pos)? {
                        break;
                    }
                }
                Mode::StoredCopy => {
                    if !self.stored_copy(src, &mut pos, dst, &mut produced) {
                        break;
                    }
                }
                Mode::TableHeader => {
                    if !self.table_header(src, &mut pos) {
                        break;
                    }
                }
                Mode::CodeLengths => {
                    if !self.code_lengths_step(src, &mut pos)? {
                        break;
                    }
                }
                Mode::LengthSequence => {
                    if self.have >= self.hlit + self.hdist {
                        self.dynamic_tables()?;
                        self.mode = Mode::Symbol;
                        continue;
                    }
                    if !self.length_sequence_step(src, &mut pos)? {
                        break;
                    }
                }
                Mode::Symbol => {
                    if !self.symbol_step(src, &mut pos, dst, &mut produced)? {
                        break;
                    }
                }
                Mode::Literal(byte) => {
                    if produced >= dst.len() {
                        break;
                    }
                    dst[produced] = byte;
                    produced += 1;
                    self.mode = Mode::Symbol;
                }
                Mode::Distance => {
                    if !self.distance_step(src, &mut pos)? {
                        break;
                    }
                }
                Mode::CopyOut => {
                    self.copy_out(dst, &mut produced, history)?;
                    if self.copy_length > 0 {
                        break;
                    }
                    self.mode = Mode::Symbol;
                }
            }
        }
        Ok(Progress {
            consumed: pos,
            produced,
            finished: self.mode == Mode::Done,
        })
    }

    /// Read a stored block's `LEN`/`NLEN` pair.
    ///
    /// Aligning and then taking a whole multiple of eight bits empties the
    /// accumulator, so [`Self::stored_copy`] can read straight from the input.
    fn stored_header(&mut self, src: &[u8], pos: &mut usize) -> Result<bool, Error> {
        self.bits.align();
        let (Some(len), Some(nlen)) = (
            self.bits.field(src, pos, 0, 16),
            self.bits.field(src, pos, 16, 16),
        ) else {
            return Ok(false);
        };
        self.bits.consume(32);
        if len != (!nlen & 0xFFFF) {
            return Err(Error::InvalidStoredBlockLength);
        }
        self.stored_remaining = usize::try_from(len).unwrap_or(0);
        self.mode = Mode::StoredCopy;
        Ok(true)
    }

    /// Copy a stored block's bytes across. `false` means one side ran out.
    fn stored_copy(
        &mut self,
        src: &[u8],
        pos: &mut usize,
        dst: &mut [u8],
        produced: &mut usize,
    ) -> bool {
        let take = self
            .stored_remaining
            .min(dst.len() - *produced)
            .min(src.len() - *pos);
        dst[*produced..*produced + take].copy_from_slice(&src[*pos..*pos + take]);
        *produced += take;
        *pos += take;
        self.stored_remaining -= take;
        if self.stored_remaining > 0 {
            return false;
        }
        self.end_of_block();
        true
    }

    /// Read a dynamic block's `HLIT`/`HDIST`/`HCLEN` counts.
    fn table_header(&mut self, src: &[u8], pos: &mut usize) -> bool {
        let Some(value) = self.take(src, pos, 14) else {
            return false;
        };
        self.hlit = usize::try_from(value & 0x1F).unwrap_or(0) + 257;
        self.hdist = usize::try_from((value >> 5) & 0x1F).unwrap_or(0) + 1;
        self.hclen = usize::try_from((value >> 10) & 0x0F).unwrap_or(0) + 4;
        self.code_length_lengths = [0; CODE_LENGTH_SYMBOLS];
        self.have = 0;
        self.mode = Mode::CodeLengths;
        true
    }

    /// Read one of the code-length alphabet's own 3-bit lengths, or build its
    /// table once they are all in.
    fn code_lengths_step(&mut self, src: &[u8], pos: &mut usize) -> Result<bool, Error> {
        if self.have >= self.hclen {
            let (table, incomplete) = build_huffman(&self.code_length_lengths)?;
            // The alphabet describing the real tables must always be
            // complete: the RFC's single-code exception applies only to the
            // tables it goes on to describe.
            if incomplete {
                return Err(Error::IncompleteHuffmanCode);
            }
            self.code_length_table = table;
            self.lengths = [0; MAX_SYMBOLS + 32];
            self.have = 0;
            self.mode = Mode::LengthSequence;
            return Ok(true);
        }
        let Some(value) = self.take(src, pos, 3) else {
            return Ok(false);
        };
        self.code_length_lengths[CODE_LENGTH_ORDER[self.have]] = u8::try_from(value).unwrap_or(0);
        self.have += 1;
        Ok(true)
    }

    /// Move on from a completed block.
    fn end_of_block(&mut self) {
        self.mode = if self.last_block {
            Mode::Done
        } else {
            Mode::BlockHeader
        };
    }

    /// Decode one code-length symbol and its repeat count as a single step.
    /// `false` means the input ran out with nothing consumed.
    fn length_sequence_step(&mut self, src: &[u8], pos: &mut usize) -> Result<bool, Error> {
        let total = self.hlit + self.hdist;
        let Some((symbol, used)) =
            decode_symbol(&self.code_length_table, &mut self.bits, src, pos, 0)?
        else {
            return Ok(false);
        };
        let (repeat, value, extra_bits) = match symbol {
            0..=15 => (1usize, u8::try_from(symbol).unwrap_or(0), 0),
            16 => {
                let previous = self.have.checked_sub(1).ok_or(Error::InvalidLengthRepeat)?;
                let value = self.lengths[previous];
                let Some(extra) = self.bits.field(src, pos, used, 2) else {
                    return Ok(false);
                };
                (3 + usize::try_from(extra).unwrap_or(0), value, 2)
            }
            17 => {
                let Some(extra) = self.bits.field(src, pos, used, 3) else {
                    return Ok(false);
                };
                (3 + usize::try_from(extra).unwrap_or(0), 0, 3)
            }
            18 => {
                let Some(extra) = self.bits.field(src, pos, used, 7) else {
                    return Ok(false);
                };
                (11 + usize::try_from(extra).unwrap_or(0), 0, 7)
            }
            _ => return Err(Error::InvalidSymbol),
        };
        self.bits.consume(used + extra_bits);
        let end = self
            .have
            .checked_add(repeat)
            .ok_or(Error::InvalidLengthRepeat)?;
        if end > total {
            return Err(Error::InvalidLengthRepeat);
        }
        self.lengths
            .get_mut(self.have..end)
            .ok_or(Error::InvalidLengthRepeat)?
            .fill(value);
        self.have = end;
        Ok(true)
    }

    /// Decode one literal/length symbol, taking its extra-length field in
    /// the same step. `false` means the input ran out with nothing consumed.
    fn symbol_step(
        &mut self,
        src: &[u8],
        pos: &mut usize,
        dst: &mut [u8],
        produced: &mut usize,
    ) -> Result<bool, Error> {
        let Some((symbol, used)) = decode_symbol(&self.lit_table, &mut self.bits, src, pos, 0)?
        else {
            return Ok(false);
        };
        match symbol {
            0..=255 => {
                self.bits.consume(used);
                let byte = u8::try_from(symbol).unwrap_or(0);
                if *produced < dst.len() {
                    dst[*produced] = byte;
                    *produced += 1;
                } else {
                    self.mode = Mode::Literal(byte);
                    return Ok(false);
                }
            }
            256 => {
                self.bits.consume(used);
                self.end_of_block();
            }
            257..=285 => {
                let index = usize::from(symbol) - 257;
                let extra_bits = u32::from(LENGTH_EXTRA[index]);
                let Some(extra) = self.bits.field(src, pos, used, extra_bits) else {
                    return Ok(false);
                };
                self.bits.consume(used + extra_bits);
                self.copy_length =
                    usize::from(LENGTH_BASE[index]) + usize::try_from(extra).unwrap_or(0);
                self.mode = Mode::Distance;
            }
            _ => return Err(Error::InvalidSymbol),
        }
        Ok(true)
    }

    /// Decode one distance symbol, taking its extra-distance field in the
    /// same step.
    fn distance_step(&mut self, src: &[u8], pos: &mut usize) -> Result<bool, Error> {
        let Some((symbol, used)) = decode_symbol(&self.dist_table, &mut self.bits, src, pos, 0)?
        else {
            return Ok(false);
        };
        let index = usize::from(symbol);
        let base = *DIST_BASE.get(index).ok_or(Error::InvalidSymbol)?;
        let extra_bits = u32::from(*DIST_EXTRA.get(index).ok_or(Error::InvalidSymbol)?);
        let Some(extra) = self.bits.field(src, pos, used, extra_bits) else {
            return Ok(false);
        };
        self.bits.consume(used + extra_bits);
        self.copy_distance = usize::from(base) + usize::try_from(extra).unwrap_or(0);
        self.mode = Mode::CopyOut;
        Ok(true)
    }

    /// Expand the pending back-reference, taking the part that reaches
    /// before this call's output from the history window.
    fn copy_out(
        &mut self,
        dst: &mut [u8],
        produced: &mut usize,
        history: Option<&History>,
    ) -> Result<(), Error> {
        let distance = self.copy_distance;
        while self.copy_length > 0 && *produced < dst.len() && distance > *produced {
            let byte = history
                .and_then(|window| window.byte(distance - *produced))
                .ok_or(Error::DistanceTooFar)?;
            dst[*produced] = byte;
            *produced += 1;
            self.copy_length -= 1;
        }
        // The remainder reaches only into bytes this call has already
        // written, so it needs no per-byte source decision.
        while self.copy_length > 0 && *produced < dst.len() {
            dst[*produced] = dst[*produced - distance];
            *produced += 1;
            self.copy_length -= 1;
        }
        Ok(())
    }
}

/// A streaming RFC 1951 DEFLATE decoder.
///
/// Around 34 KiB — the 32 KiB history a back-reference may reach into, plus
/// the suspended machine. Heap-own it rather than putting one on a stack.
/// For a stream that arrives whole, [`inflate_into`] runs the same machine
/// with no window at all.
#[derive(Default)]
pub struct Inflater {
    core: Core,
    history: History,
}

impl Inflater {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore this decoder to a fresh stream, reusing its allocation.
    ///
    /// Forgetting how much history is valid is what makes reuse safe between
    /// streams: a back-reference past the new stream's own output finds
    /// nothing rather than what the last one left behind.
    pub fn reset(&mut self) {
        self.core = Core::default();
        self.history.next = 0;
        self.history.have = 0;
    }

    /// Whether the final block has been decoded.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.core.mode == Mode::Done
    }

    /// Decode as much of `src` into `dst` as both allow.
    ///
    /// Every byte of `src` the decoder can absorb is absorbed, so a caller
    /// only re-presents what [`Progress::consumed`] leaves behind — which is
    /// nothing unless `dst` filled first.
    ///
    /// # Errors
    ///
    /// See [`Error`]. [`Error::Finished`] if the stream has already ended.
    pub fn inflate(&mut self, src: &[u8], dst: &mut [u8]) -> Result<Progress, Error> {
        if self.core.mode == Mode::Done {
            return Err(Error::Finished);
        }
        let progress = self.core.run(src, dst, Some(&self.history))?;
        self.history.push(&dst[..progress.produced]);
        Ok(progress)
    }
}

/// Decompress the DEFLATE stream in `src` into `dst`, returning `(bytes
/// produced, bytes of `src` consumed)`.
///
/// See the module documentation for the trailing-byte policy: `consumed` is
/// the byte offset immediately following the final block, which is what a
/// caller (the zlib envelope) needs to locate a trailer that follows the
/// stream.
///
/// The whole stream must be present: a back-reference resolves against the
/// output produced so far, so no history window is kept and none is needed.
///
/// # Errors
///
/// See [`Error`] for every fail-closed refusal reason.
pub fn inflate_into_consumed(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), Error> {
    let mut core = Core::default();
    let progress = core.run(src, dst, None)?;
    if !progress.finished {
        return Err(if progress.produced == dst.len() {
            Error::OutputOverflow
        } else {
            Error::UnexpectedEof
        });
    }
    Ok((progress.produced, progress.consumed))
}

/// Decompress the whole DEFLATE stream in `src` into `dst`, returning the
/// number of bytes produced.
///
/// See the module documentation for the Huffman-table strategy and the
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
