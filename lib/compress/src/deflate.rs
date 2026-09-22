//! RFC 1951 DEFLATE compression.
//!
//! [`Deflate`] is a *stream* encoder: one value carries the sliding window,
//! the match finder, and the partially-written bit across calls, so a
//! consumer can compress a conversation packet by packet and flush at each
//! packet boundary while later packets still back-reference earlier ones.
//! That is what `zlib@openssh.com` needs from a compressor, and a one-shot
//! `fn(src, dst)` cannot express it.
//!
//! # Output sizing
//!
//! The encoder writes straight into the caller's `dst` and cannot rewind, so
//! the caller sizes `dst` with [`Deflate::bound`] — the crate's existing
//! discipline (`crate::max_compressed_len`) applied to a stream. A `dst`
//! shorter than that is refused before any state changes.
//!
//! The bound holds because a block is never emitted larger than the bytes it
//! covers plus its header: a block's input span is capped below the 64 KiB a
//! stored block can carry, so the stored form is always available as the
//! floor, and [`Deflate`] picks whichever of stored, fixed-Huffman, and
//! dynamic-Huffman is cheapest for that block.
//!
//! # Memory
//!
//! A [`Deflate`] is around 220 KiB — a 32 KiB window held in the 64 KiB
//! buffer a slide needs, the hash head and chain tables, and one block's
//! tokens. That is what a full-window DEFLATE match finder costs; heap-own
//! it rather than putting one on a stack. The window size is the format's,
//! fixed; the hash and token table sizes are tuning, documented on their
//! constants.

use crate::format::{
    self, CODE_LENGTH_ORDER, CODE_LENGTH_SYMBOLS, DIST_SYMBOLS, END_OF_BLOCK,
    FIXED_DISTANCE_LENGTHS, LIT_CODED, LIT_SYMBOLS, MAX_BITS, MAX_MATCH, MAX_STORED, MIN_MATCH,
    WINDOW_SIZE,
};

/// Why compression failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// `dst` was shorter than [`Deflate::bound`]. Nothing was consumed and
    /// the encoder is unchanged, so the call can be retried with more room.
    OutputOverflow,
    /// The stream was finished by a [`Flush::Finish`]; it accepts no more
    /// input. Call [`Deflate::reset`] to start a new one.
    Finished,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::OutputOverflow => "destination buffer is smaller than the encoder's bound",
            Self::Finished => "deflate stream is already finished",
        };
        f.write_str(text)
    }
}

/// How much of the stream a [`Deflate::deflate`] call must make readable to
/// the far end.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Flush {
    /// Buffer freely. Output appears only as whole blocks fill, so the peer
    /// cannot yet decode everything that was fed in.
    None,
    /// End the current block and append an empty stored block, so the peer
    /// can decode every byte fed so far and the stream resumes on a byte
    /// boundary. This is zlib's `Z_SYNC_FLUSH`, the per-message flush a
    /// packetised protocol needs.
    Sync,
    /// End the stream: the last block is marked final and the bit buffer is
    /// padded out to a byte.
    Finish,
}

/// Buffer the window slides within: the 32 KiB history plus 32 KiB of room
/// ahead of it, so a slide is one copy every 32 KiB rather than per byte.
const BUFFER_SIZE: usize = 2 * WINDOW_SIZE;

/// Mask taking a window position to its hash-chain slot.
const WINDOW_MASK: usize = WINDOW_SIZE - 1;

/// Bytes that must be readable ahead of the cursor for a full-length match
/// to be findable.
const MIN_LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;

/// Largest back-reference the encoder emits. Short of [`WINDOW_SIZE`] by the
/// lookahead, so a longest match found at the limit still ends inside the
/// window when the buffer slides.
const MAX_DIST: usize = WINDOW_SIZE - MIN_LOOKAHEAD;

/// Log2 of the match-finder head table. Tuning, not format: one slot per
/// window position keeps the chains short without a second table's memory.
const HASH_BITS: usize = 15;

/// Slots in the match-finder head table.
const HASH_SIZE: usize = 1 << HASH_BITS;

/// Mask reducing a 3-byte hash to a head-table slot.
const HASH_MASK: usize = HASH_SIZE - 1;

/// Empty head/chain slot. Position zero doubles as the sentinel, so the
/// window's very first byte is never a match source — zlib's own convention,
/// and one lost match rather than a second table of validity bits.
const NIL: u16 = 0;

/// Chain steps searched per position. Tuning: the knee of the ratio/CPU
/// curve, matching zlib's default level.
const MAX_CHAIN: usize = 128;

/// A match this long ends the chain walk; longer is not worth the search.
const NICE_MATCH: usize = 128;

/// Above this previous-match length the chain walk is quartered: a long
/// match is unlikely to be beaten and the search is the expensive part.
const GOOD_MATCH: usize = 8;

/// Above this length the lazy evaluation stops looking for a better match
/// one byte later.
const MAX_LAZY: usize = 16;

/// A three-byte match further back than this costs more in distance bits
/// than the literals it replaces.
const TOO_FAR: usize = 4096;

/// Tokens one block may hold. Tuning: the block header is amortised over
/// this many symbols, and the table is 24 KiB of the encoder's footprint.
const TOKENS: usize = 8192;

/// Input span at which a block is closed regardless of token count, so the
/// span always stays below what a stored block can carry.
const BLOCK_SPAN_LIMIT: usize = WINDOW_SIZE;

/// A block's span always leaves the stored form available, which is what
/// bounds the output at its input plus a header.
const _: () = assert!(BLOCK_SPAN_LIMIT + MAX_MATCH <= MAX_STORED);

/// Smallest input span the bound may assume for a non-final block. Every
/// token covers at least one byte, so a token-full block covers at least
/// this many; halving it again absorbs the short block a window slide can
/// close early.
const BOUND_BLOCK_SPAN: usize = TOKENS / 2;

/// Bytes the bound charges per block: the stored header, its `LEN`/`NLEN`
/// pair, and the byte a preceding partial bit can spill into.
const BOUND_BLOCK_OVERHEAD: usize = 8;

/// Bytes the bound charges once, for the flush marker and the pending bits
/// a previous call left behind.
const BOUND_FIXED_OVERHEAD: usize = 16;

/// Nodes in a Huffman tree over the largest alphabet: a leaf per symbol and
/// an internal node per merge.
const TREE_NODES: usize = 2 * LIT_CODED + 1;

/// Code-length symbol repeating the previous length 3..=6 times.
const REPEAT_PREVIOUS: usize = 16;

/// Code-length symbol repeating a zero length 3..=10 times.
const REPEAT_ZERO_SHORT: usize = 17;

/// Code-length symbol repeating a zero length 11..=138 times.
const REPEAT_ZERO_LONG: usize = 18;

/// Longest code the code-length alphabet's own Huffman code may use
/// (RFC 1951 §3.2.7).
const MAX_CODE_LENGTH_BITS: usize = 7;

/// Entries the code-length sequence can hold: one per literal/length and
/// distance code, which is the worst case when no run repeats.
const SEQUENCE_LEN: usize = LIT_SYMBOLS + DIST_SYMBOLS;

/// A bit-level cursor over the caller's destination slice.
///
/// DEFLATE packs values least-significant bit first; a Huffman code is the
/// one exception and is stored pre-reversed so it goes through the same
/// path.
struct Writer<'a> {
    dst: &'a mut [u8],
    pos: usize,
    buf: u32,
    count: u32,
}

impl Writer<'_> {
    /// Append the low `n` bits of `value`. `n` never exceeds 16 and `count`
    /// is always below 8 on entry, so the accumulator cannot overflow.
    fn put(&mut self, value: u32, n: u32) -> Result<(), Error> {
        if n == 0 {
            return Ok(());
        }
        let mask = (1u32 << n) - 1;
        self.buf |= (value & mask) << self.count;
        self.count += n;
        while self.count >= 8 {
            let byte = self.buf.to_le_bytes()[0];
            *self.dst.get_mut(self.pos).ok_or(Error::OutputOverflow)? = byte;
            self.pos += 1;
            self.buf >>= 8;
            self.count -= 8;
        }
        Ok(())
    }

    /// Pad out to the next byte boundary with zero bits.
    fn align(&mut self) -> Result<(), Error> {
        if self.count > 0 {
            let byte = self.buf.to_le_bytes()[0];
            *self.dst.get_mut(self.pos).ok_or(Error::OutputOverflow)? = byte;
            self.pos += 1;
        }
        self.buf = 0;
        self.count = 0;
        Ok(())
    }

    /// Copy `bytes` out. Only ever called immediately after [`Self::align`].
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(Error::OutputOverflow)?;
        self.dst
            .get_mut(self.pos..end)
            .ok_or(Error::OutputOverflow)?
            .copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    /// Bits of padding a stored block's byte alignment will cost from here.
    fn stored_padding(&self) -> u64 {
        u64::from((8 - ((self.count + 3) % 8)) % 8)
    }
}

/// Scratch for one canonical Huffman construction, reused for the
/// literal/length, distance, and code-length alphabets in turn.
struct Trees {
    freq: [u32; TREE_NODES],
    dad: [u16; TREE_NODES],
    len: [u8; TREE_NODES],
    code: [u16; TREE_NODES],
    heap: [u16; TREE_NODES],
    depth: [u8; TREE_NODES],
    bl_count: [u16; MAX_BITS + 2],
    heap_len: usize,
    heap_max: usize,
}

/// A streaming RFC 1951 DEFLATE encoder.
///
/// Feed input with [`Self::deflate`], sizing the destination with
/// [`Self::bound`]. [`Flush::Sync`] makes everything fed so far decodable by
/// the peer; [`Flush::Finish`] ends the stream. See the module documentation
/// for the memory footprint.
pub struct Deflate {
    window: [u8; BUFFER_SIZE],
    prev: [u16; WINDOW_SIZE],
    head: [u16; HASH_SIZE],

    tok_lit: [u8; TOKENS],
    tok_dist: [u16; TOKENS],
    tok_count: usize,
    extra_bits: u64,

    lit_freq: [u16; LIT_CODED],
    dist_freq: [u16; DIST_SYMBOLS],
    lit_len: [u8; LIT_SYMBOLS],
    lit_code: [u16; LIT_SYMBOLS],
    dist_len: [u8; DIST_SYMBOLS],
    dist_code: [u16; DIST_SYMBOLS],
    cl_len: [u8; CODE_LENGTH_SYMBOLS],
    cl_code: [u16; CODE_LENGTH_SYMBOLS],
    cl_freq: [u16; CODE_LENGTH_SYMBOLS],
    seq_sym: [u8; SEQUENCE_LEN],
    seq_extra: [u8; SEQUENCE_LEN],
    seq_count: usize,
    hclen: usize,
    lit_codes: usize,
    dist_codes: usize,
    header_bits: u64,

    trees: Trees,

    strstart: usize,
    lookahead: usize,
    block_start: usize,
    match_start: usize,
    match_length: usize,
    prev_match: usize,
    prev_length: usize,
    match_available: bool,

    bit_buf: u32,
    bit_count: u32,
    finished: bool,
    poisoned: bool,
}

impl Default for Deflate {
    // The window and match-finder tables are what a full-window DEFLATE
    // encoder *is*, and this crate links no allocator, so it cannot place
    // them: the value is returned for the caller to box. Clippy's stack-array
    // heuristic is reading a deliberate API as an accident.
    #[allow(clippy::large_stack_arrays)]
    fn default() -> Self {
        let mut encoder = Self {
            window: [0; BUFFER_SIZE],
            prev: [0; WINDOW_SIZE],
            head: [0; HASH_SIZE],
            tok_lit: [0; TOKENS],
            tok_dist: [0; TOKENS],
            tok_count: 0,
            extra_bits: 0,
            lit_freq: [0; LIT_CODED],
            dist_freq: [0; DIST_SYMBOLS],
            lit_len: [0; LIT_SYMBOLS],
            lit_code: [0; LIT_SYMBOLS],
            dist_len: [0; DIST_SYMBOLS],
            dist_code: [0; DIST_SYMBOLS],
            cl_len: [0; CODE_LENGTH_SYMBOLS],
            cl_code: [0; CODE_LENGTH_SYMBOLS],
            cl_freq: [0; CODE_LENGTH_SYMBOLS],
            seq_sym: [0; SEQUENCE_LEN],
            seq_extra: [0; SEQUENCE_LEN],
            seq_count: 0,
            hclen: 4,
            lit_codes: 0,
            dist_codes: 0,
            header_bits: 0,
            trees: Trees {
                freq: [0; TREE_NODES],
                dad: [0; TREE_NODES],
                len: [0; TREE_NODES],
                code: [0; TREE_NODES],
                heap: [0; TREE_NODES],
                depth: [0; TREE_NODES],
                bl_count: [0; MAX_BITS + 2],
                heap_len: 0,
                heap_max: 0,
            },
            strstart: 0,
            lookahead: 0,
            block_start: 0,
            match_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            prev_length: MIN_MATCH - 1,
            match_available: false,
            bit_buf: 0,
            bit_count: 0,
            finished: false,
            poisoned: false,
        };
        encoder.start_block();
        encoder
    }
}

impl Deflate {
    /// A fresh encoder. Around 220 KiB; heap-own it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore this encoder to a fresh stream, reusing its allocation.
    ///
    /// Clearing the hash heads is what makes reuse safe between streams: no
    /// chain reaches what the last stream left in the window, so none of its
    /// bytes can be back-referenced into the new stream's output.
    pub fn reset(&mut self) {
        self.head.fill(NIL);
        self.strstart = 0;
        self.lookahead = 0;
        self.block_start = 0;
        self.match_start = 0;
        self.match_length = MIN_MATCH - 1;
        self.prev_match = 0;
        self.prev_length = MIN_MATCH - 1;
        self.match_available = false;
        self.bit_buf = 0;
        self.bit_count = 0;
        self.finished = false;
        self.poisoned = false;
        self.start_block();
    }

    /// Whether [`Flush::Finish`] has ended this stream.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// An upper bound on the bytes [`Self::deflate`] can write for
    /// `input_len` further bytes, given what this encoder already holds.
    ///
    /// Sizing `dst` to at least this never provokes an
    /// [`Error::OutputOverflow`].
    #[must_use]
    pub fn bound(&self, input_len: usize) -> usize {
        let owed = (self.strstart - self.block_start)
            .saturating_add(self.lookahead)
            .saturating_add(input_len);
        let blocks = (owed / BOUND_BLOCK_SPAN).saturating_add(3);
        owed.saturating_add(blocks.saturating_mul(BOUND_BLOCK_OVERHEAD))
            .saturating_add(BOUND_FIXED_OVERHEAD)
    }

    /// Compress all of `src` into `dst`, returning the bytes written.
    ///
    /// `dst` must be at least [`Self::bound`] of `src.len()`; a shorter one
    /// is refused up front and the encoder is unchanged. Should an internal
    /// write ever find no room despite that, the encoder fails closed for
    /// good rather than emitting a half-written block: every later call
    /// refuses too.
    ///
    /// # Errors
    ///
    /// [`Error::OutputOverflow`] for a `dst` below the bound,
    /// [`Error::Finished`] after a [`Flush::Finish`].
    pub fn deflate(&mut self, src: &[u8], dst: &mut [u8], flush: Flush) -> Result<usize, Error> {
        if self.finished {
            return Err(Error::Finished);
        }
        if self.poisoned || dst.len() < self.bound(src.len()) {
            return Err(Error::OutputOverflow);
        }
        let mut writer = Writer {
            dst,
            pos: 0,
            buf: self.bit_buf,
            count: self.bit_count,
        };
        let outcome = self.run(src, &mut writer, flush);
        self.bit_buf = writer.buf;
        self.bit_count = writer.count;
        let written = writer.pos;
        match outcome {
            Ok(()) => Ok(written),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    /// Consume `src`, then apply `flush`.
    fn run(&mut self, src: &[u8], writer: &mut Writer<'_>, flush: Flush) -> Result<(), Error> {
        let mut consumed = 0usize;
        loop {
            if self.lookahead < MIN_LOOKAHEAD && consumed < src.len() {
                self.fill_window(src, &mut consumed, writer)?;
            }
            let drained = consumed == src.len();
            self.tokenise(drained && flush != Flush::None, writer)?;
            if drained {
                break;
            }
        }
        match flush {
            Flush::None => Ok(()),
            Flush::Sync => {
                self.close_block(writer, false)?;
                Self::emit_empty_stored(writer)
            }
            Flush::Finish => {
                self.close_block(writer, true)?;
                self.finished = true;
                writer.align()
            }
        }
    }

    /// Move bytes from `src` into the window, sliding it when the cursor has
    /// run far enough that the history behind it is safe to drop.
    fn fill_window(
        &mut self,
        src: &[u8],
        consumed: &mut usize,
        writer: &mut Writer<'_>,
    ) -> Result<(), Error> {
        if self.strstart >= WINDOW_SIZE + MAX_DIST {
            // The stored form addresses the block through the window, so the
            // block must close before its bytes move — and closing needs every
            // byte of its span tallied.
            self.flush_deferred_literal();
            self.close_block(writer, false)?;
            self.slide();
        }
        let filled = self.strstart + self.lookahead;
        let room = BUFFER_SIZE - filled;
        let take = room.min(src.len() - *consumed);
        self.window[filled..filled + take].copy_from_slice(&src[*consumed..*consumed + take]);
        *consumed += take;
        self.lookahead += take;
        Ok(())
    }

    /// Drop the oldest half of the buffer and rebase every position into it.
    fn slide(&mut self) {
        self.window.copy_within(WINDOW_SIZE..BUFFER_SIZE, 0);
        self.strstart -= WINDOW_SIZE;
        self.block_start -= WINDOW_SIZE;
        self.match_start = self.match_start.saturating_sub(WINDOW_SIZE);
        self.prev_match = self.prev_match.saturating_sub(WINDOW_SIZE);
        let rebase = |slot: &mut u16| {
            let value = usize::from(*slot);
            *slot = if value >= WINDOW_SIZE {
                u16::try_from(value - WINDOW_SIZE).unwrap_or(NIL)
            } else {
                NIL
            };
        };
        self.head.iter_mut().for_each(rebase);
        self.prev.iter_mut().for_each(rebase);
    }
}

/// How many leading bytes `left` and `right` share.
///
/// A word at a time: this is the encoder's innermost loop, run once per
/// surviving chain step, and a byte-at-a-time compare spends most of its
/// time on matches that are tens of bytes long.
fn common_prefix(left: &[u8], right: &[u8]) -> usize {
    const STEP: usize = size_of::<u64>();
    let mut matched = 0usize;
    while matched + STEP <= left.len() {
        let here = u64::from_le_bytes(
            left[matched..matched + STEP]
                .try_into()
                .unwrap_or([0; STEP]),
        );
        let there = u64::from_le_bytes(
            right[matched..matched + STEP]
                .try_into()
                .unwrap_or([0; STEP]),
        );
        if here != there {
            // Little-endian, so the lowest differing bit is in the earliest
            // differing byte whatever the target's own order.
            let differing = usize::try_from((here ^ there).trailing_zeros() / 8).unwrap_or(0);
            return matched + differing;
        }
        matched += STEP;
    }
    while matched < left.len() && left[matched] == right[matched] {
        matched += 1;
    }
    matched
}

/// The match-finder key for the three bytes at `pos`.
fn hash3(window: &[u8; BUFFER_SIZE], pos: usize) -> usize {
    let a = usize::from(window[pos]) << 10;
    let b = usize::from(window[pos + 1]) << 5;
    (a ^ b ^ usize::from(window[pos + 2])) & HASH_MASK
}

impl Deflate {
    /// Link `pos` into its hash chain and return the position that headed it.
    fn insert_string(&mut self, pos: usize) -> u16 {
        let slot = hash3(&self.window, pos);
        let head = self.head[slot];
        self.prev[pos & WINDOW_MASK] = head;
        self.head[slot] = u16::try_from(pos).unwrap_or(NIL);
        head
    }

    /// The longest match beating the one already in hand, walking the chain
    /// from `candidate`. `None` leaves [`Self::match_start`] alone, so a
    /// previously found match stays addressable.
    fn longest_match(&self, mut candidate: usize) -> Option<(usize, usize)> {
        let max_len = self.lookahead.min(MAX_MATCH);
        let mut best_len = self.prev_length.max(MIN_MATCH - 1);
        if max_len < MIN_MATCH || best_len >= max_len {
            return None;
        }
        let limit = self.strstart.saturating_sub(MAX_DIST);
        let mut chain = if best_len >= GOOD_MATCH {
            MAX_CHAIN >> 2
        } else {
            MAX_CHAIN
        };
        let target = &self.window[self.strstart..self.strstart + max_len];
        let mut best_start = None;
        while candidate > limit && chain > 0 {
            chain -= 1;
            // The byte one past the best so far must match, or this
            // candidate cannot beat it: one compare rejects most chain steps.
            if self.window[candidate + best_len] == target[best_len] {
                let here = &self.window[candidate..candidate + max_len];
                let length = common_prefix(here, target);
                if length > best_len {
                    best_len = length;
                    best_start = Some(candidate);
                    if length >= NICE_MATCH || length >= max_len {
                        break;
                    }
                }
            }
            candidate = usize::from(self.prev[candidate & WINDOW_MASK]);
        }
        best_start.map(|start| (best_len, start))
    }

    /// Turn the buffered lookahead into tokens, closing blocks as they fill.
    ///
    /// `flushing` drains the last bytes too, where a full-length match can no
    /// longer be found.
    fn tokenise(&mut self, flushing: bool, writer: &mut Writer<'_>) -> Result<(), Error> {
        while self.lookahead >= MIN_LOOKAHEAD || (flushing && self.lookahead > 0) {
            let head = if self.lookahead >= MIN_MATCH {
                self.insert_string(self.strstart)
            } else {
                NIL
            };
            self.prev_length = self.match_length;
            self.prev_match = self.match_start;
            self.match_length = MIN_MATCH - 1;
            if head != NIL
                && self.prev_length < MAX_LAZY
                && self.strstart - usize::from(head) <= MAX_DIST
            {
                if let Some((length, start)) = self.longest_match(usize::from(head)) {
                    self.match_length = length;
                    self.match_start = start;
                    if length == MIN_MATCH && self.strstart - start > TOO_FAR {
                        self.match_length = MIN_MATCH - 1;
                    }
                }
            }

            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                self.take_match();
                if self.block_is_full() {
                    self.close_block(writer, false)?;
                }
            } else if self.match_available {
                self.push_literal(self.window[self.strstart - 1]);
                // Closing happens before the cursor moves: the byte it is
                // about to pass belongs to the next block, not this one.
                if self.block_is_full() {
                    self.close_block(writer, false)?;
                }
                self.strstart += 1;
                self.lookahead -= 1;
            } else {
                self.match_available = true;
                self.strstart += 1;
                self.lookahead -= 1;
            }
        }
        if flushing {
            self.flush_deferred_literal();
        }
        Ok(())
    }

    /// Emit the match the previous position found, and hash every position it
    /// covers so a later match can start inside it.
    fn take_match(&mut self) {
        let distance = self.strstart - 1 - self.prev_match;
        let length = self.prev_length;
        self.push_match(distance, length);
        let max_insert = (self.strstart + self.lookahead).saturating_sub(MIN_MATCH);
        self.lookahead -= length - 1;
        for _ in 0..length - 2 {
            self.strstart += 1;
            if self.strstart <= max_insert {
                self.insert_string(self.strstart);
            }
        }
        self.match_available = false;
        self.match_length = MIN_MATCH - 1;
        self.strstart += 1;
    }

    /// Tally a literal the lazy evaluation is still holding, so the tokens
    /// once again cover exactly the block's span.
    ///
    /// The match that made it a candidate is dropped with it; keeping it
    /// would emit those bytes twice.
    fn flush_deferred_literal(&mut self) {
        if self.match_available {
            self.push_literal(self.window[self.strstart - 1]);
            self.match_available = false;
            self.match_length = MIN_MATCH - 1;
        }
    }

    /// Whether the block must close now. One slot is always left free, so a
    /// deferred literal can still be tallied before a close.
    fn block_is_full(&self) -> bool {
        self.tok_count >= TOKENS - 1 || self.strstart - self.block_start >= BLOCK_SPAN_LIMIT
    }

    /// Record one literal byte.
    fn push_literal(&mut self, byte: u8) {
        let slot = self.tok_count;
        self.tok_lit[slot] = byte;
        self.tok_dist[slot] = 0;
        self.tok_count = slot + 1;
        self.lit_freq[usize::from(byte)] += 1;
    }

    /// Record one back-reference.
    fn push_match(&mut self, distance: usize, length: usize) {
        let slot = self.tok_count;
        self.tok_lit[slot] = u8::try_from(length - MIN_MATCH).unwrap_or(0);
        self.tok_dist[slot] = u16::try_from(distance).unwrap_or(1);
        self.tok_count = slot + 1;
        let (length_symbol, length_extra, _) = format::length_symbol(length);
        let (distance_symbol, distance_extra, _) = format::distance_symbol(distance);
        self.lit_freq[length_symbol] += 1;
        self.dist_freq[distance_symbol] += 1;
        self.extra_bits += u64::from(length_extra) + u64::from(distance_extra);
    }

    /// Begin a fresh block's tally. The end-of-block symbol is always sent,
    /// so it is counted before any token is.
    fn start_block(&mut self) {
        self.tok_count = 0;
        self.extra_bits = 0;
        self.lit_freq.fill(0);
        self.dist_freq.fill(0);
        self.lit_freq[END_OF_BLOCK] = 1;
    }
}

/// A canonical code read most-significant bit first, stored the other way
/// round so it goes out through the same least-significant-first writer as
/// every other field.
fn reverse_bits(mut value: u32, length: usize) -> u16 {
    let mut out = 0u32;
    for _ in 0..length {
        out = (out << 1) | (value & 1);
        value >>= 1;
    }
    u16::try_from(out).unwrap_or(0)
}

/// Fill `codes` with the canonical code for each symbol's length in
/// `lengths`, pre-reversed.
fn canonical_codes(lengths: &[u8], codes: &mut [u16]) {
    let mut per_length = [0u32; MAX_BITS + 2];
    for &length in lengths {
        per_length[usize::from(length)] += 1;
    }
    per_length[0] = 0;
    let mut next = [0u32; MAX_BITS + 2];
    let mut code = 0u32;
    for bits in 1..=MAX_BITS {
        code = (code + per_length[bits - 1]) << 1;
        next[bits] = code;
    }
    for (symbol, &length) in lengths.iter().enumerate() {
        if length == 0 {
            continue;
        }
        let bits = usize::from(length);
        let value = next[bits];
        next[bits] += 1;
        if let Some(slot) = codes.get_mut(symbol) {
            *slot = reverse_bits(value, bits);
        }
    }
}

/// Extra bits carried by a code-length repeat symbol.
fn repeat_extra_bits(symbol: usize) -> u32 {
    match symbol {
        REPEAT_PREVIOUS => 2,
        REPEAT_ZERO_SHORT => 3,
        REPEAT_ZERO_LONG => 7,
        _ => 0,
    }
}

impl Trees {
    /// Build a canonical Huffman code over `freqs` with no code longer than
    /// `max_length`, returning the highest symbol it covers.
    fn build(&mut self, freqs: &[u16], max_length: usize) -> usize {
        let elems = freqs.len();
        self.heap_len = 0;
        self.heap_max = TREE_NODES;
        let mut max_code: i32 = -1;
        for (symbol, &count) in freqs.iter().enumerate() {
            self.freq[symbol] = u32::from(count);
            self.len[symbol] = 0;
            if count != 0 {
                self.heap_len += 1;
                self.heap[self.heap_len] = u16::try_from(symbol).unwrap_or(0);
                self.depth[symbol] = 0;
                max_code = i32::try_from(symbol).unwrap_or(max_code);
            }
        }
        // A Huffman code needs two codes to be well formed. A block using
        // fewer borrows the lowest symbols, which cost nothing to describe.
        while self.heap_len < 2 {
            let node = if max_code < 2 {
                max_code += 1;
                usize::try_from(max_code).unwrap_or(0)
            } else {
                0
            };
            self.heap_len += 1;
            self.heap[self.heap_len] = u16::try_from(node).unwrap_or(0);
            self.freq[node] = 1;
            self.depth[node] = 0;
        }
        let max_code = usize::try_from(max_code).unwrap_or(0);
        for position in (1..=self.heap_len / 2).rev() {
            self.sift_down(position);
        }
        self.merge(elems);
        self.assign_lengths(max_code, max_length);
        canonical_codes(&self.len[..=max_code], &mut self.code[..=max_code]);
        max_code
    }

    /// Order two nodes by frequency, breaking a tie on subtree depth so the
    /// flatter tree wins and code lengths stay short.
    fn smaller(&self, left: u16, right: u16) -> bool {
        let (left, right) = (usize::from(left), usize::from(right));
        self.freq[left] < self.freq[right]
            || (self.freq[left] == self.freq[right] && self.depth[left] <= self.depth[right])
    }

    /// Restore the heap property from `start` downwards.
    fn sift_down(&mut self, start: usize) {
        let mut parent = start;
        let value = self.heap[parent];
        loop {
            let mut child = parent << 1;
            if child > self.heap_len {
                break;
            }
            if child < self.heap_len && self.smaller(self.heap[child + 1], self.heap[child]) {
                child += 1;
            }
            if self.smaller(value, self.heap[child]) {
                break;
            }
            self.heap[parent] = self.heap[child];
            parent = child;
        }
        self.heap[parent] = value;
    }

    /// Take the least frequent node off the heap.
    fn pop(&mut self) -> u16 {
        let top = self.heap[1];
        self.heap[1] = self.heap[self.heap_len];
        self.heap_len -= 1;
        self.sift_down(1);
        top
    }

    /// Combine the heap into one tree, recording each node in `heap` from the
    /// top down so the later passes see them parents-first.
    fn merge(&mut self, elems: usize) {
        let mut node = elems;
        loop {
            let left = self.pop();
            let right = self.heap[1];
            self.heap_max -= 1;
            self.heap[self.heap_max] = left;
            self.heap_max -= 1;
            self.heap[self.heap_max] = right;
            let (left, right) = (usize::from(left), usize::from(right));
            self.freq[node] = self.freq[left] + self.freq[right];
            self.depth[node] = self.depth[left].max(self.depth[right]).saturating_add(1);
            let parent = u16::try_from(node).unwrap_or(0);
            self.dad[left] = parent;
            self.dad[right] = parent;
            self.heap[1] = parent;
            node += 1;
            self.sift_down(1);
            if self.heap_len < 2 {
                break;
            }
        }
        self.heap_max -= 1;
        self.heap[self.heap_max] = self.heap[1];
    }

    /// Set each leaf's code length from its depth in the tree, then pull any
    /// code longer than `max_length` back into range.
    fn assign_lengths(&mut self, max_code: usize, max_length: usize) {
        self.bl_count.fill(0);
        self.len[usize::from(self.heap[self.heap_max])] = 0;
        let mut overflow = 0usize;
        for index in self.heap_max + 1..TREE_NODES {
            let node = usize::from(self.heap[index]);
            let mut bits = usize::from(self.len[usize::from(self.dad[node])]) + 1;
            if bits > max_length {
                bits = max_length;
                overflow += 1;
            }
            self.len[node] = u8::try_from(bits).unwrap_or(0);
            if node <= max_code {
                self.bl_count[bits] += 1;
            }
        }
        if overflow == 0 {
            return;
        }
        self.redistribute(max_code, max_length, overflow);
    }

    /// Move leaves out of the over-long band until the code set is exactly
    /// subscribed again, then re-derive every length from the new profile.
    fn redistribute(&mut self, max_code: usize, max_length: usize, mut overflow: usize) {
        // Some shorter length always has a leaf to push down: filling every
        // length with leaves would need more symbols than either alphabet has,
        // so the search below cannot come up empty while `overflow` remains.
        while let Some(bits) = (1..max_length).rev().find(|&bits| self.bl_count[bits] > 0) {
            self.bl_count[bits] -= 1;
            self.bl_count[bits + 1] += 2;
            self.bl_count[max_length] -= 1;
            if overflow <= 2 {
                break;
            }
            overflow -= 2;
        }
        let mut index = TREE_NODES;
        for bits in (1..=max_length).rev() {
            let mut remaining = self.bl_count[bits];
            while remaining != 0 && index > self.heap_max {
                index -= 1;
                let node = usize::from(self.heap[index]);
                if node > max_code {
                    continue;
                }
                self.len[node] = u8::try_from(bits).unwrap_or(0);
                remaining -= 1;
            }
        }
    }
}

impl Deflate {
    /// Emit the current block in whichever of the three forms is smallest,
    /// then begin the next.
    fn close_block(&mut self, writer: &mut Writer<'_>, last: bool) -> Result<(), Error> {
        if self.tok_count == 0 && !last {
            return Ok(());
        }
        let span = self.strstart - self.block_start;
        let fixed_lengths = format::fixed_literal_length_lengths();
        let fixed_bits = 3 + self.symbol_cost(&fixed_lengths, &FIXED_DISTANCE_LENGTHS);
        self.build_trees();
        let dynamic_bits = 3 + self.header_bits + self.symbol_cost(&self.lit_len, &self.dist_len);
        // A span the 16-bit `LEN` field cannot state is not a candidate at
        // all. The block-span cap already keeps that unreachable; this is
        // what makes it unreachable rather than silently mis-framed.
        let stored_bits = (span <= MAX_STORED)
            .then(|| 3 + writer.stored_padding() + 32 + 8 * u64::try_from(span).unwrap_or(0));

        if stored_bits.is_some_and(|bits| bits <= fixed_bits && bits <= dynamic_bits) {
            self.emit_stored(writer, last, span)?;
        } else if fixed_bits <= dynamic_bits {
            self.lit_len = fixed_lengths;
            canonical_codes(&fixed_lengths, &mut self.lit_code);
            self.dist_len
                .copy_from_slice(&FIXED_DISTANCE_LENGTHS[..DIST_SYMBOLS]);
            canonical_codes(&FIXED_DISTANCE_LENGTHS, &mut self.dist_code);
            writer.put(u32::from(last), 1)?;
            writer.put(1, 2)?;
            self.emit_tokens(writer)?;
        } else {
            self.emit_dynamic(writer, last)?;
        }

        self.block_start = self.strstart;
        self.start_block();
        Ok(())
    }

    /// Bits the block's tokens cost under the given code lengths, including
    /// the length and distance extra bits, which no choice of code changes.
    fn symbol_cost(&self, lit_lengths: &[u8], dist_lengths: &[u8]) -> u64 {
        let mut bits = self.extra_bits;
        for (symbol, &freq) in self.lit_freq.iter().enumerate() {
            bits += u64::from(freq) * u64::from(lit_lengths[symbol]);
        }
        for (symbol, &freq) in self.dist_freq.iter().enumerate() {
            bits += u64::from(freq) * u64::from(dist_lengths[symbol]);
        }
        bits
    }

    /// Build this block's dynamic literal/length, distance, and code-length
    /// trees, and cost the header that describes them.
    fn build_trees(&mut self) {
        self.lit_codes = self.trees.build(&self.lit_freq, MAX_BITS) + 1;
        self.lit_len.fill(0);
        self.lit_code.fill(0);
        self.lit_len[..LIT_CODED].copy_from_slice(&self.trees.len[..LIT_CODED]);
        self.lit_code[..LIT_CODED].copy_from_slice(&self.trees.code[..LIT_CODED]);

        self.dist_codes = self.trees.build(&self.dist_freq, MAX_BITS) + 1;
        self.dist_len
            .copy_from_slice(&self.trees.len[..DIST_SYMBOLS]);
        self.dist_code
            .copy_from_slice(&self.trees.code[..DIST_SYMBOLS]);

        self.build_length_sequence();
        self.trees.build(&self.cl_freq, MAX_CODE_LENGTH_BITS);
        self.cl_len
            .copy_from_slice(&self.trees.len[..CODE_LENGTH_SYMBOLS]);
        self.cl_code
            .copy_from_slice(&self.trees.code[..CODE_LENGTH_SYMBOLS]);

        self.hclen = CODE_LENGTH_ORDER
            .iter()
            .rposition(|&symbol| self.cl_len[symbol] != 0)
            .map_or(4, |index| (index + 1).max(4));

        let mut bits = 14 + 3 * u64::try_from(self.hclen).unwrap_or(0);
        for index in 0..self.seq_count {
            let symbol = usize::from(self.seq_sym[index]);
            bits += u64::from(self.cl_len[symbol]) + u64::from(repeat_extra_bits(symbol));
        }
        self.header_bits = bits;
    }

    /// The code length of the `index`th entry of the concatenated
    /// literal/length and distance length list the header transmits.
    fn length_at(&self, index: usize) -> u8 {
        if index < self.lit_codes {
            self.lit_len[index]
        } else {
            self.dist_len[index - self.lit_codes]
        }
    }

    /// Run-length encode the two trees' code lengths into the sequence the
    /// dynamic header carries (RFC 1951 §3.2.7).
    fn build_length_sequence(&mut self) {
        self.seq_count = 0;
        self.cl_freq.fill(0);
        let total = self.lit_codes + self.dist_codes;
        // No previous length yet: a value no code length can take.
        let mut previous = u8::MAX;
        let mut run = 0usize;
        let (mut longest, mut shortest) = if self.length_at(0) == 0 {
            (138usize, 3usize)
        } else {
            (7usize, 4usize)
        };
        for index in 0..total {
            let current = self.length_at(index);
            // A value no code length can take closes the final run.
            let next = if index + 1 < total {
                self.length_at(index + 1)
            } else {
                u8::MAX
            };
            run += 1;
            if run < longest && current == next {
                continue;
            }
            if run < shortest {
                for _ in 0..run {
                    self.push_sequence(usize::from(current), 0);
                }
            } else if current != 0 {
                if current != previous {
                    self.push_sequence(usize::from(current), 0);
                    run -= 1;
                }
                self.push_sequence(REPEAT_PREVIOUS, run - 3);
            } else if run <= 10 {
                self.push_sequence(REPEAT_ZERO_SHORT, run - 3);
            } else {
                self.push_sequence(REPEAT_ZERO_LONG, run - 11);
            }
            run = 0;
            previous = current;
            if next == 0 {
                longest = 138;
                shortest = 3;
            } else if current == next {
                longest = 6;
                shortest = 3;
            } else {
                longest = 7;
                shortest = 4;
            }
        }
    }

    /// Record one code-length-sequence entry.
    fn push_sequence(&mut self, symbol: usize, extra: usize) {
        let slot = self.seq_count;
        self.seq_sym[slot] = u8::try_from(symbol).unwrap_or(0);
        self.seq_extra[slot] = u8::try_from(extra).unwrap_or(0);
        self.seq_count = slot + 1;
        self.cl_freq[symbol] += 1;
    }

    /// Emit the block verbatim, which is the floor every other form is
    /// measured against.
    fn emit_stored(&self, writer: &mut Writer<'_>, last: bool, span: usize) -> Result<(), Error> {
        writer.put(u32::from(last), 1)?;
        writer.put(0, 2)?;
        writer.align()?;
        let len = u16::try_from(span).unwrap_or(0);
        writer.bytes(&len.to_le_bytes())?;
        writer.bytes(&(!len).to_le_bytes())?;
        writer.bytes(&self.window[self.block_start..self.block_start + span])
    }

    /// The empty stored block that ends a [`Flush::Sync`], leaving the stream
    /// byte-aligned with everything so far decodable.
    fn emit_empty_stored(writer: &mut Writer<'_>) -> Result<(), Error> {
        writer.put(0, 3)?;
        writer.align()?;
        writer.bytes(&[0x00, 0x00, 0xFF, 0xFF])
    }

    /// Emit the block under its own transmitted code lengths.
    fn emit_dynamic(&self, writer: &mut Writer<'_>, last: bool) -> Result<(), Error> {
        writer.put(u32::from(last), 1)?;
        writer.put(2, 2)?;
        writer.put(u32::try_from(self.lit_codes - 257).unwrap_or(0), 5)?;
        writer.put(u32::try_from(self.dist_codes - 1).unwrap_or(0), 5)?;
        writer.put(u32::try_from(self.hclen - 4).unwrap_or(0), 4)?;
        for &symbol in CODE_LENGTH_ORDER.iter().take(self.hclen) {
            writer.put(u32::from(self.cl_len[symbol]), 3)?;
        }
        for index in 0..self.seq_count {
            let symbol = usize::from(self.seq_sym[index]);
            writer.put(
                u32::from(self.cl_code[symbol]),
                u32::from(self.cl_len[symbol]),
            )?;
            writer.put(u32::from(self.seq_extra[index]), repeat_extra_bits(symbol))?;
        }
        self.emit_tokens(writer)
    }

    /// Write the block's tokens under whichever code is in `lit_code` and
    /// `dist_code`, closing with the end-of-block symbol.
    fn emit_tokens(&self, writer: &mut Writer<'_>) -> Result<(), Error> {
        for index in 0..self.tok_count {
            let distance = usize::from(self.tok_dist[index]);
            if distance == 0 {
                let symbol = usize::from(self.tok_lit[index]);
                writer.put(
                    u32::from(self.lit_code[symbol]),
                    u32::from(self.lit_len[symbol]),
                )?;
                continue;
            }
            let length = usize::from(self.tok_lit[index]) + MIN_MATCH;
            let (symbol, extra, value) = format::length_symbol(length);
            writer.put(
                u32::from(self.lit_code[symbol]),
                u32::from(self.lit_len[symbol]),
            )?;
            writer.put(value, extra)?;
            let (symbol, extra, value) = format::distance_symbol(distance);
            writer.put(
                u32::from(self.dist_code[symbol]),
                u32::from(self.dist_len[symbol]),
            )?;
            writer.put(value, extra)?;
        }
        writer.put(
            u32::from(self.lit_code[END_OF_BLOCK]),
            u32::from(self.lit_len[END_OF_BLOCK]),
        )
    }
}

#[cfg(test)]
#[path = "deflate_tests.rs"]
mod tests;
