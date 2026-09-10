//! A complete, fail-closed VP8L decoder (WebP Lossless Bitstream
//! Specification).
//!
//! VP8L codes a picture as a prefix-coded stream of literals, LZ77 backward
//! references, and colour-cache hits, over pixels a chain of up to four
//! reversible transforms has already decorrelated. Everything the format
//! defines is here: the meta-prefix arrangement that gives each region of
//! the picture its own five prefix codes, the colour cache, the distance
//! mapping that turns a code into a two-dimensional offset, and all four
//! transforms — predictor with each of its fourteen predictors, cross
//! colour, subtract green, and colour indexing with its pixel bundling.
//!
//! The stream is reached two ways. A `VP8L` chunk is a whole picture, and
//! carries its own signature byte and geometry. A compressed `ALPH` chunk is
//! the same coding over one plane, with the geometry supplied by the picture
//! it belongs to and the plane held in the green channel — so
//! [`decode_alpha`] takes the size rather than reading one.
//!
//! # Three readings the specification leaves to the decoder
//!
//! A prefix code holding exactly one symbol costs **no bits** to read: it
//! spends half the code space, so an encoder emits nothing for it and a
//! decoder that consumed a bit would read into the next field.
//!
//! An index past the end of a colour-indexing palette reads as transparent
//! black, which is what the specification asks for rather than a refusal.
//!
//! The top-right neighbour of the rightmost pixel of a row is the *leftmost
//! pixel of that same row*, which the specification states and which falls
//! out of addressing one contiguous buffer.
//!
//! Every other malformation is refused: an oversubscribed or incomplete
//! prefix code, a repeated or unknown transform, a transform nested inside
//! one, a backward reference reaching outside the pixels already produced,
//! and a stream that ends early.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::huffman::Canonical;
use crate::{DecodeError, DecodeLimits, RasterImage, RGBA_BYTES};

/// The byte a `VP8L` chunk opens with.
pub(crate) const SIGNATURE: u8 = 0x2F;

/// The longest prefix code the format permits, which is shorter than the
/// shared table's own capacity.
const MAX_PREFIX_BITS: usize = 15;

/// Literal green values, which are also the first codes of the green
/// alphabet.
const LITERAL_CODES: u32 = 256;

/// Backward-reference length prefixes, following the literals.
const LENGTH_CODES: u32 = 24;

/// Backward-reference distance prefixes.
const DISTANCE_CODES: u32 = 40;

/// Symbols of the code that codes the other codes' lengths.
const CODE_LENGTH_CODES: usize = 19;

/// The first code-length symbol that repeats rather than naming a length.
const FIRST_REPEAT: u8 = 16;

/// The code length a repeat-previous carries before any length has been
/// named.
const DEFAULT_CODE_LENGTH: u8 = 8;

/// Extra bits each repeat symbol reads, and the shortest repeat it means.
const REPEATS: [(u32, u32); 3] = [(2, 3), (3, 3), (7, 11)];

/// The order the code-length code's own lengths arrive in.
const CODE_LENGTH_ORDER: [usize; CODE_LENGTH_CODES] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

/// The widest colour cache the format permits.
const MAX_CACHE_BITS: u32 = 11;

/// The multiplier the colour cache hashes a pixel with.
const CACHE_HASH: u32 = 0x1E35_A7BD;

/// Transforms one stream may carry, each at most once.
const MAX_TRANSFORMS: usize = 4;

/// Prefix codes one meta-Huffman group holds: green (with the lengths and
/// the cache sharing its alphabet), red, blue, alpha, and distance.
const CODES_PER_GROUP: usize = 5;

/// The green code's position in a group, which is the one read first.
const GREEN: usize = 0;

/// The fewest bits a meta-Huffman group can occupy.
///
/// A containment bound on the group table, not a capacity: the group count
/// comes from the largest index an entropy image holds, and a sparse image
/// may name a high index while spending almost no bytes. The cheapest
/// prefix code is a simple one carrying a single one-bit symbol — four bits
/// — and a group holds five, so a stream cannot describe more groups than
/// this allows without carrying the bytes to pay for them.
const MIN_GROUP_BITS: usize = 20;

/// The most groups an entropy image can name, since a group index is that
/// image's red and green bytes together.
const MAX_GROUPS: u32 = 1 << 16;

/// Which reversible transform a stream declares.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Kind {
    Predictor,
    CrossColour,
    SubtractGreen,
    ColourIndexing,
}

impl Kind {
    fn from_bits(bits: u32) -> Result<Self, DecodeError> {
        match bits {
            0 => Ok(Self::Predictor),
            1 => Ok(Self::CrossColour),
            2 => Ok(Self::SubtractGreen),
            3 => Ok(Self::ColourIndexing),
            _ => Err(DecodeError::WebpLosslessInvalidTransform),
        }
    }
}

/// One declared transform, and the picture width its inverse produces.
struct Transform {
    kind: Kind,
    /// The width and height of this transform's *output*, which is the
    /// picture as it stood before the transform narrowed it.
    width: u32,
    height: u32,
    /// The block size the predictor and cross-colour transforms sample
    /// their own image at, or the pixel bundling colour indexing packs at.
    bits: u32,
    /// The transform's own image: predictor modes, colour multipliers, or
    /// the palette. Empty for subtract green.
    data: Vec<u32>,
}

/// A bit stream read least significant bit first, as the format packs one.
struct Bits<'a> {
    bytes: &'a [u8],
    /// The next bit's index from the start of `bytes`.
    pos: usize,
    /// How many bits `bytes` holds.
    len: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            len: bytes.len().saturating_mul(8),
        }
    }

    /// How many bits remain unread.
    fn remaining(&self) -> usize {
        self.len.saturating_sub(self.pos)
    }

    /// The next `count` bits without consuming them, zero-padded past the
    /// end of the stream. `count` may be no more than 24.
    fn peek(&self, count: usize) -> u32 {
        let byte = self.pos / 8;
        let shift = u32::try_from(self.pos % 8).unwrap_or(0);
        let mut window = 0u64;
        for (index, &value) in self.bytes.iter().skip(byte).take(5).enumerate() {
            window |= u64::from(value) << (8 * u32::try_from(index).unwrap_or(0));
        }
        let mask = (1u64 << count) - 1;
        u32::try_from((window >> shift) & mask).unwrap_or(0)
    }

    /// Read `count` bits, refusing a read the stream cannot cover. `count`
    /// may be no more than 24.
    fn read(&mut self, count: usize) -> Result<u32, DecodeError> {
        if count > self.remaining() {
            return Err(DecodeError::WebpLosslessTruncated);
        }
        let value = self.peek(count);
        self.pos += count;
        Ok(value)
    }

    fn flag(&mut self) -> Result<bool, DecodeError> {
        Ok(self.read(1)? != 0)
    }
}

/// One prefix code: its canonical assignment, and where its symbols sit in
/// the group store.
struct PrefixCode {
    table: Canonical,
    /// This code's symbols, in code order, as a range of the store.
    first: u32,
    /// Whether the code holds exactly one symbol, which costs no bits.
    lone: bool,
}

/// Every meta-Huffman group's prefix codes, and the symbols they select.
///
/// One flat store rather than a table per group: a stream may declare tens
/// of thousands of groups, and an allocation per code would cost more in
/// bookkeeping than the codes themselves.
struct Codes {
    codes: Vec<PrefixCode>,
    symbols: Vec<u16>,
}

impl Codes {
    /// The group at `index`'s prefix codes, or `None` where the entropy
    /// image named a group the stream did not describe.
    fn group(&self, index: u32) -> Option<&[PrefixCode]> {
        let first = usize::try_from(index).ok()?.checked_mul(CODES_PER_GROUP)?;
        self.codes.get(first..first.checked_add(CODES_PER_GROUP)?)
    }
}

/// Decode one symbol of `code`, whose symbols sit in `store`.
fn read_symbol(bits: &mut Bits<'_>, code: &PrefixCode, store: &[u16]) -> Result<u16, DecodeError> {
    let at = |index: u32| {
        store
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .copied()
            .ok_or(BAD_CODE)
    };
    if code.lone {
        return at(code.first);
    }
    let available = bits.remaining().min(MAX_PREFIX_BITS);
    let window = bits.peek(available);
    let mut walk = Canonical::walk();
    for taken in 0..available {
        if let Some(offset) = walk.push(&code.table, window >> taken) {
            bits.pos += walk.len();
            return at(code.first.wrapping_add(offset));
        }
    }
    Err(if available < MAX_PREFIX_BITS {
        DecodeError::WebpLosslessTruncated
    } else {
        BAD_CODE
    })
}

/// Scratch a prefix-code read reuses, so reading tens of thousands of codes
/// costs no allocation per code.
struct Scratch {
    /// Every symbol's code length, of which only the leading `used` entries
    /// are ever written and cleared.
    lengths: Vec<u8>,
    /// How many leading `lengths` entries the last read touched.
    used: usize,
}

impl Scratch {
    fn new(alphabet: usize) -> Result<Self, DecodeError> {
        Ok(Self {
            lengths: fallible::filled(alphabet, 0u8).ok_or(DecodeError::OutOfMemory)?,
            used: 0,
        })
    }

    /// Forget the last read, clearing only what it wrote.
    fn reset(&mut self) {
        if let Some(touched) = self.lengths.get_mut(..self.used) {
            touched.fill(0);
        }
        self.used = 0;
    }

    fn set(&mut self, symbol: usize, length: u8) -> Result<(), DecodeError> {
        let slot = self
            .lengths
            .get_mut(symbol)
            .ok_or(DecodeError::WebpLosslessInvalidCode)?;
        *slot = length;
        self.used = self.used.max(symbol + 1);
        Ok(())
    }
}

/// Read one prefix code, appending its symbols to `store` and answering the
/// code that indexes them.
fn read_prefix_code(
    bits: &mut Bits<'_>,
    alphabet: u32,
    scratch: &mut Scratch,
    store: &mut Vec<u16>,
) -> Result<PrefixCode, DecodeError> {
    scratch.reset();
    let alphabet_len = usize::try_from(alphabet).unwrap_or(usize::MAX);
    if bits.flag()? {
        read_simple_lengths(bits, scratch)?;
    } else {
        read_coded_lengths(bits, alphabet_len, scratch)?;
    }
    build_prefix_code(scratch, alphabet_len, store)
}

/// Read the one- or two-symbol form, whose symbols are named outright.
fn read_simple_lengths(bits: &mut Bits<'_>, scratch: &mut Scratch) -> Result<(), DecodeError> {
    let two = bits.flag()?;
    let wide = bits.flag()?;
    let first = bits.read(if wide { 8 } else { 1 })?;
    scratch.set(usize::try_from(first).unwrap_or(usize::MAX), 1)?;
    if two {
        // Naming the same symbol twice is inefficient rather than invalid,
        // and leaves a code holding one symbol.
        let second = bits.read(8)?;
        scratch.set(usize::try_from(second).unwrap_or(usize::MAX), 1)?;
    }
    Ok(())
}

/// Read the general form, whose lengths are themselves prefix-coded.
fn read_coded_lengths(
    bits: &mut Bits<'_>,
    alphabet: usize,
    scratch: &mut Scratch,
) -> Result<(), DecodeError> {
    let mut lengths = [0u32; CODE_LENGTH_CODES];
    let declared = usize::try_from(bits.read(4)?).unwrap_or(0) + 4;
    for &slot in CODE_LENGTH_ORDER.iter().take(declared) {
        let length = bits.read(3)?;
        *lengths
            .get_mut(slot)
            .ok_or(DecodeError::WebpLosslessInvalidCode)? = length;
    }
    let mut counts = [0u32; 8];
    for &length in &lengths {
        if let Some(count) = usize::try_from(length)
            .ok()
            .and_then(|length| length.checked_sub(1))
            .and_then(|index| counts.get_mut(index))
        {
            *count += 1;
        }
    }
    let table = Canonical::build(&counts).ok_or(DecodeError::WebpLosslessInvalidCode)?;
    if !table.complete() && !table.lone_symbol() {
        return Err(DecodeError::WebpLosslessInvalidCode);
    }
    let mut ordered = [0u16; CODE_LENGTH_CODES];
    order_symbols(&lengths, &table, &mut ordered)?;
    let lone = table.lone_symbol();
    let code = PrefixCode {
        table,
        first: 0,
        lone,
    };

    // A stream may cap how many symbols carry a length, leaving the rest
    // zero; without the cap every symbol of the alphabet is coded.
    let mut remaining = if bits.flag()? {
        let width = 2 + 2 * usize::try_from(bits.read(3)?).unwrap_or(0);
        let capped = usize::try_from(bits.read(width)?)
            .unwrap_or(usize::MAX)
            .saturating_add(2);
        if capped > alphabet {
            return Err(BAD_CODE);
        }
        capped
    } else {
        alphabet
    };
    let mut previous = DEFAULT_CODE_LENGTH;
    let mut symbol = 0usize;
    while symbol < alphabet && remaining > 0 {
        remaining -= 1;
        let length = u8::try_from(read_symbol(bits, &code, &ordered)?).map_err(|_| BAD_CODE)?;
        if length < FIRST_REPEAT {
            scratch.set(symbol, length)?;
            symbol += 1;
            if length != 0 {
                previous = length;
            }
            continue;
        }
        let slot = usize::from(length - FIRST_REPEAT);
        let &(extra, offset) = REPEATS
            .get(slot)
            .ok_or(DecodeError::WebpLosslessInvalidCode)?;
        let repeat = usize::try_from(bits.read(usize::try_from(extra).unwrap_or(0))? + offset)
            .unwrap_or(usize::MAX);
        if symbol.saturating_add(repeat) > alphabet {
            return Err(DecodeError::WebpLosslessInvalidCode);
        }
        let filled = if length == FIRST_REPEAT { previous } else { 0 };
        for _ in 0..repeat {
            scratch.set(symbol, filled)?;
            symbol += 1;
        }
    }
    Ok(())
}

/// Turn the lengths a read produced into a canonical code, appending its
/// symbols to `store` in code order.
fn build_prefix_code(
    scratch: &Scratch,
    alphabet: usize,
    store: &mut Vec<u16>,
) -> Result<PrefixCode, DecodeError> {
    let written = scratch
        .lengths
        .get(..scratch.used.min(alphabet))
        .ok_or(DecodeError::WebpLosslessInvalidCode)?;
    let mut counts = [0u32; MAX_PREFIX_BITS];
    for &length in written {
        if let Some(count) = usize::from(length)
            .checked_sub(1)
            .and_then(|index| counts.get_mut(index))
        {
            *count += 1;
        } else if length != 0 {
            return Err(DecodeError::WebpLosslessInvalidCode);
        }
    }
    let table = Canonical::build(&counts).ok_or(DecodeError::WebpLosslessInvalidCode)?;
    let lone = table.lone_symbol();
    if !table.complete() && !lone {
        return Err(DecodeError::WebpLosslessInvalidCode);
    }
    let total = usize::try_from(table.symbols()).unwrap_or(usize::MAX);
    if total == 0 {
        return Err(DecodeError::WebpLosslessInvalidCode);
    }
    let first = u32::try_from(store.len()).map_err(|_| DecodeError::OutOfMemory)?;
    if !fallible::reserve(store, total) {
        return Err(DecodeError::OutOfMemory);
    }
    let base = store.len();
    store.resize(base + total, 0);
    let ordered = store.get_mut(base..).ok_or(DecodeError::OutOfMemory)?;
    order_symbols(written, &table, ordered)?;
    Ok(PrefixCode { table, first, lone })
}

/// Place every symbol carrying a non-zero length into `ordered` in code
/// order: by length, and within a length by symbol.
fn order_symbols(
    lengths: &[impl Copy + Into<u32>],
    table: &Canonical,
    ordered: &mut [u16],
) -> Result<(), DecodeError> {
    let mut cursors = [0u32; MAX_PREFIX_BITS + 1];
    for (len, cursor) in cursors.iter_mut().enumerate() {
        *cursor = table.run(len).map_or(0, |run| run.first_symbol);
    }
    for (symbol, &length) in lengths.iter().enumerate() {
        let length = usize::try_from(length.into()).unwrap_or(usize::MAX);
        if length == 0 {
            continue;
        }
        let cursor = cursors
            .get_mut(length)
            .ok_or(DecodeError::WebpLosslessInvalidCode)?;
        let slot = ordered
            .get_mut(usize::try_from(*cursor).unwrap_or(usize::MAX))
            .ok_or(DecodeError::WebpLosslessInvalidCode)?;
        *slot = u16::try_from(symbol).map_err(|_| DecodeError::WebpLosslessInvalidCode)?;
        *cursor += 1;
    }
    Ok(())
}

/// A block-sampled image's extent: how many blocks cover `size` pixels.
fn blocks(size: u32, bits: u32) -> u32 {
    let step = 1u32 << bits.min(31);
    size.saturating_add(step - 1) >> bits.min(31)
}

/// Read the colour-cache width a stream declares, or zero for none.
fn read_cache_bits(bits: &mut Bits<'_>) -> Result<u32, DecodeError> {
    if !bits.flag()? {
        return Ok(0);
    }
    let cache_bits = bits.read(4)?;
    if !(1..=MAX_CACHE_BITS).contains(&cache_bits) {
        return Err(DecodeError::WebpLosslessInvalidCacheBits);
    }
    Ok(cache_bits)
}

/// The pixel count `width` by `height` occupies, refused when it overflows
/// the address space rather than wrapping.
fn pixel_count(width: u32, height: u32) -> Result<usize, DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::WebpLosslessInvalidGeometry);
    }
    usize::try_from(u64::from(width) * u64::from(height)).map_err(|_| DecodeError::OutOfMemory)
}

/// Read an entropy-coded image: a colour cache, one group of prefix codes,
/// and the pixels.
///
/// This is what a transform's own image and the meta-prefix entropy image
/// are. It carries no transforms and no meta-prefix of its own, which is
/// what keeps the recursion exactly one level deep.
fn entropy_coded_image(
    bits: &mut Bits<'_>,
    width: u32,
    height: u32,
) -> Result<Vec<u32>, DecodeError> {
    let count = pixel_count(width, height)?;
    let cache_bits = read_cache_bits(bits)?;
    let codes = read_groups(bits, cache_bits, 1)?;
    let mut pixels = fallible::filled(count, 0u32).ok_or(DecodeError::OutOfMemory)?;
    decode_pixels(bits, &codes, None, cache_bits, width, &mut pixels)?;
    Ok(pixels)
}

/// Read `groups` meta-Huffman groups of prefix codes.
fn read_groups(bits: &mut Bits<'_>, cache_bits: u32, groups: u32) -> Result<Codes, DecodeError> {
    let cache_entries = if cache_bits == 0 {
        0
    } else {
        1u32 << cache_bits.min(MAX_CACHE_BITS)
    };
    let green_alphabet = LITERAL_CODES + LENGTH_CODES + cache_entries;
    let alphabets = [
        green_alphabet,
        LITERAL_CODES,
        LITERAL_CODES,
        LITERAL_CODES,
        DISTANCE_CODES,
    ];
    let widest = usize::try_from(green_alphabet).unwrap_or(usize::MAX);
    let mut scratch = Scratch::new(widest)?;
    let total = usize::try_from(groups)
        .ok()
        .and_then(|groups| groups.checked_mul(CODES_PER_GROUP))
        .ok_or(DecodeError::OutOfMemory)?;
    let mut codes = Codes {
        codes: Vec::new(),
        symbols: Vec::new(),
    };
    if !fallible::reserve(&mut codes.codes, total) {
        return Err(DecodeError::OutOfMemory);
    }
    for _ in 0..groups {
        for &alphabet in &alphabets {
            let code = read_prefix_code(bits, alphabet, &mut scratch, &mut codes.symbols)?;
            codes.codes.push(code);
        }
    }
    Ok(codes)
}

/// Where each pixel's prefix codes come from: one group for the whole
/// picture, or a block-sampled image naming one per region.
struct Meta {
    /// One group index per block, from the entropy image's red and green.
    groups: Vec<u32>,
    /// The block size, as a shift.
    bits: u32,
    /// How many blocks one row of blocks holds.
    stride: u32,
}

/// Decode a stream's pixels into `pixels`, which is already sized.
fn decode_pixels(
    bits: &mut Bits<'_>,
    codes: &Codes,
    meta: Option<&Meta>,
    cache_bits: u32,
    width: u32,
    pixels: &mut [u32],
) -> Result<(), DecodeError> {
    let mut cache = ColourCache::new(cache_bits)?;
    let cache_limit = LITERAL_CODES + LENGTH_CODES + cache.len();
    let stride = usize::try_from(width).unwrap_or(usize::MAX);
    let mut written = 0usize;
    let mut x = 0usize;
    let mut y = 0u32;
    while written < pixels.len() {
        let index = match meta {
            Some(meta) => group_at(meta, u32::try_from(x).unwrap_or(u32::MAX), y),
            None => 0,
        };
        let group = codes.group(index).ok_or(BAD_CODE)?;
        let green = u32::from(read_symbol(
            bits,
            group.get(GREEN).ok_or(BAD_CODE)?,
            &codes.symbols,
        )?);
        let produced = if green < LITERAL_CODES {
            let red = read_symbol(bits, group.get(1).ok_or(BAD_CODE)?, &codes.symbols)?;
            let blue = read_symbol(bits, group.get(2).ok_or(BAD_CODE)?, &codes.symbols)?;
            let alpha = read_symbol(bits, group.get(3).ok_or(BAD_CODE)?, &codes.symbols)?;
            let pixel =
                (u32::from(alpha) << 24) | (u32::from(red) << 16) | (green << 8) | u32::from(blue);
            *pixels.get_mut(written).ok_or(BAD_CODE)? = pixel;
            1usize
        } else if green < LITERAL_CODES + LENGTH_CODES {
            let length =
                usize::try_from(read_extended(bits, green - LITERAL_CODES)?).unwrap_or(usize::MAX);
            let symbol = u32::from(read_symbol(
                bits,
                group.get(4).ok_or(BAD_CODE)?,
                &codes.symbols,
            )?);
            let code = read_extended(bits, symbol)?;
            let distance = plane_distance(width, code);
            copy_reference(pixels, written, distance, length)?;
            length
        } else if green < cache_limit {
            let key = green - LITERAL_CODES - LENGTH_CODES;
            *pixels.get_mut(written).ok_or(BAD_CODE)? = cache.get(key)?;
            1usize
        } else {
            return Err(DecodeError::WebpLosslessInvalidCode);
        };
        for index in written..written + produced {
            cache.insert(*pixels.get(index).ok_or(BAD_CODE)?);
        }
        written += produced;
        x += produced;
        while stride != 0 && x >= stride {
            x -= stride;
            y = y.saturating_add(1);
        }
    }
    Ok(())
}

/// The refusal a group index or symbol the stream did not describe raises.
const BAD_CODE: DecodeError = DecodeError::WebpLosslessInvalidCode;

/// The group index covering pixel (`x`, `y`).
fn group_at(meta: &Meta, x: u32, y: u32) -> u32 {
    let shift = meta.bits.min(31);
    let column = x >> shift;
    let row = y >> shift;
    let index = u64::from(row) * u64::from(meta.stride) + u64::from(column);
    usize::try_from(index)
        .ok()
        .and_then(|index| meta.groups.get(index))
        .copied()
        .unwrap_or(MAX_GROUPS)
}

/// A length or distance prefix's value, with the extra bits it carries.
///
/// Length and distance prefixes are coded identically: the first four name
/// one to four outright, and each pair after that doubles the span the
/// extra bits address.
fn read_extended(bits: &mut Bits<'_>, symbol: u32) -> Result<u32, DecodeError> {
    if symbol >= DISTANCE_CODES {
        return Err(BAD_CODE);
    }
    if symbol < 4 {
        return Ok(symbol + 1);
    }
    let extra = usize::try_from((symbol - 2) >> 1).unwrap_or(0);
    let offset = (2 + (symbol & 1)) << u32::try_from(extra).unwrap_or(0);
    Ok(offset + bits.read(extra)? + 1)
}

/// A distance code's two-dimensional offsets, as (x, y) pairs.
///
/// The first codes address near neighbours in a spiral, so a reference to
/// the pixel above costs one short code rather than a code as wide as the
/// picture. Codes past these are a plain pixel distance.
const NEAR_OFFSETS: [(i32, u32); 120] = [
    (0, 1),
    (1, 0),
    (1, 1),
    (-1, 1),
    (0, 2),
    (2, 0),
    (1, 2),
    (-1, 2),
    (2, 1),
    (-2, 1),
    (2, 2),
    (-2, 2),
    (0, 3),
    (3, 0),
    (1, 3),
    (-1, 3),
    (3, 1),
    (-3, 1),
    (2, 3),
    (-2, 3),
    (3, 2),
    (-3, 2),
    (0, 4),
    (4, 0),
    (1, 4),
    (-1, 4),
    (4, 1),
    (-4, 1),
    (3, 3),
    (-3, 3),
    (2, 4),
    (-2, 4),
    (4, 2),
    (-4, 2),
    (0, 5),
    (3, 4),
    (-3, 4),
    (4, 3),
    (-4, 3),
    (5, 0),
    (1, 5),
    (-1, 5),
    (5, 1),
    (-5, 1),
    (2, 5),
    (-2, 5),
    (5, 2),
    (-5, 2),
    (4, 4),
    (-4, 4),
    (3, 5),
    (-3, 5),
    (5, 3),
    (-5, 3),
    (0, 6),
    (6, 0),
    (1, 6),
    (-1, 6),
    (6, 1),
    (-6, 1),
    (2, 6),
    (-2, 6),
    (6, 2),
    (-6, 2),
    (4, 5),
    (-4, 5),
    (5, 4),
    (-5, 4),
    (3, 6),
    (-3, 6),
    (6, 3),
    (-6, 3),
    (0, 7),
    (7, 0),
    (1, 7),
    (-1, 7),
    (5, 5),
    (-5, 5),
    (7, 1),
    (-7, 1),
    (4, 6),
    (-4, 6),
    (6, 4),
    (-6, 4),
    (2, 7),
    (-2, 7),
    (7, 2),
    (-7, 2),
    (3, 7),
    (-3, 7),
    (7, 3),
    (-7, 3),
    (5, 6),
    (-5, 6),
    (6, 5),
    (-6, 5),
    (8, 0),
    (4, 7),
    (-4, 7),
    (7, 4),
    (-7, 4),
    (8, 1),
    (8, 2),
    (6, 6),
    (-6, 6),
    (8, 3),
    (5, 7),
    (-5, 7),
    (7, 5),
    (-7, 5),
    (8, 4),
    (6, 7),
    (-6, 7),
    (7, 6),
    (-7, 6),
    (8, 5),
    (7, 7),
    (-7, 7),
    (8, 6),
    (8, 7),
];

/// Turn a distance code into a pixel distance.
fn plane_distance(width: u32, code: u32) -> usize {
    let code = usize::try_from(code).unwrap_or(usize::MAX);
    let Some(&(x, y)) = code
        .checked_sub(1)
        .and_then(|index| NEAR_OFFSETS.get(index))
    else {
        return code.saturating_sub(NEAR_OFFSETS.len()).max(1);
    };
    // A near offset above and to the left of the first pixel of a narrow
    // picture lands before the start, which the format resolves to the
    // immediately preceding pixel rather than a refusal.
    let distance = i64::from(y) * i64::from(width) + i64::from(x);
    usize::try_from(distance).unwrap_or(1).max(1)
}

/// Copy `length` pixels from `distance` back, refusing a reference that
/// reaches outside the pixels already produced or past the end.
///
/// Overlapping copies are the point of the coding — a distance of one
/// repeats a pixel — so this walks one pixel at a time rather than copying
/// a slice.
fn copy_reference(
    pixels: &mut [u32],
    written: usize,
    distance: usize,
    length: usize,
) -> Result<(), DecodeError> {
    if distance == 0 || distance > written {
        return Err(DecodeError::WebpLosslessInvalidReference);
    }
    if length > pixels.len() - written {
        return Err(DecodeError::WebpLosslessInvalidReference);
    }
    for step in 0..length {
        let source = written + step - distance;
        let value = *pixels
            .get(source)
            .ok_or(DecodeError::WebpLosslessInvalidReference)?;
        *pixels
            .get_mut(written + step)
            .ok_or(DecodeError::WebpLosslessInvalidReference)? = value;
    }
    Ok(())
}

/// The recently-seen colours a cache hit names.
struct ColourCache {
    entries: Vec<u32>,
    shift: u32,
}

impl ColourCache {
    fn new(bits: u32) -> Result<Self, DecodeError> {
        if bits == 0 {
            return Ok(Self {
                entries: Vec::new(),
                shift: 0,
            });
        }
        let count = 1usize << bits.min(MAX_CACHE_BITS);
        Ok(Self {
            entries: fallible::filled(count, 0u32).ok_or(DecodeError::OutOfMemory)?,
            shift: u32::BITS - bits,
        })
    }

    fn len(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(0)
    }

    fn slot(&self, pixel: u32) -> usize {
        usize::try_from(pixel.wrapping_mul(CACHE_HASH) >> self.shift).unwrap_or(0)
    }

    fn insert(&mut self, pixel: u32) {
        if self.entries.is_empty() {
            return;
        }
        let slot = self.slot(pixel);
        if let Some(entry) = self.entries.get_mut(slot) {
            *entry = pixel;
        }
    }

    fn get(&self, key: u32) -> Result<u32, DecodeError> {
        self.entries
            .get(usize::try_from(key).unwrap_or(usize::MAX))
            .copied()
            .ok_or(DecodeError::WebpLosslessInvalidCode)
    }
}

/// Read the transform chain a level-zero stream opens with.
fn read_transforms(
    bits: &mut Bits<'_>,
    width: &mut u32,
    height: u32,
) -> Result<Vec<Transform>, DecodeError> {
    let mut transforms: Vec<Transform> = Vec::new();
    let mut seen = 0u8;
    while bits.flag()? {
        if transforms.len() >= MAX_TRANSFORMS {
            return Err(DecodeError::WebpLosslessInvalidTransform);
        }
        let kind = Kind::from_bits(bits.read(2)?)?;
        let mark = 1u8 << (kind as u8);
        if seen & mark != 0 {
            return Err(DecodeError::WebpLosslessInvalidTransform);
        }
        seen |= mark;
        let output = *width;
        let mut transform = Transform {
            kind,
            width: output,
            height,
            bits: 0,
            data: Vec::new(),
        };
        match kind {
            Kind::Predictor | Kind::CrossColour => {
                transform.bits = 2 + bits.read(3)?;
                transform.data = entropy_coded_image(
                    bits,
                    blocks(output, transform.bits),
                    blocks(height, transform.bits),
                )?;
            }
            Kind::ColourIndexing => {
                let colours = bits.read(8)? + 1;
                transform.bits = match colours {
                    0..=2 => 3,
                    3..=4 => 2,
                    5..=16 => 1,
                    _ => 0,
                };
                transform.data =
                    expand_palette(&entropy_coded_image(bits, colours, 1)?, transform.bits)?;
                *width = blocks(output, transform.bits);
            }
            Kind::SubtractGreen => {}
        }
        if !fallible::reserve(&mut transforms, 1) {
            return Err(DecodeError::OutOfMemory);
        }
        transforms.push(transform);
    }
    Ok(transforms)
}

/// Undo the palette's per-channel delta coding, and pad it to every index
/// the bundling can name so an index past its end reads transparent black.
fn expand_palette(stored: &[u32], bits: u32) -> Result<Vec<u32>, DecodeError> {
    let addressable = 1usize << (8u32 >> bits.min(3));
    let mut palette = fallible::filled(addressable, 0u32).ok_or(DecodeError::OutOfMemory)?;
    let mut previous = 0u32;
    for (slot, &delta) in palette.iter_mut().zip(stored) {
        let mut entry = 0u32;
        for shift in [0u32, 8, 16, 24] {
            let sum = ((delta >> shift) & 0xFF).wrapping_add((previous >> shift) & 0xFF) & 0xFF;
            entry |= sum << shift;
        }
        *slot = entry;
        previous = entry;
    }
    Ok(palette)
}

/// Apply every transform's inverse, in the reverse of the order they were
/// read, in place over `pixels`.
fn apply_transforms(transforms: &[Transform], pixels: &mut [u32]) -> Result<(), DecodeError> {
    for transform in transforms.iter().rev() {
        match transform.kind {
            Kind::SubtractGreen => add_green(pixels),
            Kind::Predictor => undo_predictor(transform, pixels)?,
            Kind::CrossColour => undo_cross_colour(transform, pixels)?,
            Kind::ColourIndexing => undo_colour_indexing(transform, pixels)?,
        }
    }
    Ok(())
}

/// Add the green channel back into red and blue.
fn add_green(pixels: &mut [u32]) {
    for pixel in pixels {
        let green = (*pixel >> 8) & 0xFF;
        let red = ((*pixel >> 16).wrapping_add(green) & 0xFF) << 16;
        let blue = (*pixel).wrapping_add(green) & 0xFF;
        *pixel = (*pixel & 0xFF00_FF00) | red | blue;
    }
}

/// The mean of two pixels, channel by channel.
fn average2(a: u32, b: u32) -> u32 {
    (((a ^ b) & 0xFEFE_FEFE) >> 1).wrapping_add(a & b)
}

/// Clamp a channel sum into eight bits, as the format's own arithmetic does.
fn clamp_channel(value: i32) -> u32 {
    u32::try_from(value.clamp(0, 255)).unwrap_or(0)
}

fn channel(pixel: u32, shift: u32) -> i32 {
    i32::try_from((pixel >> shift) & 0xFF).unwrap_or(0)
}

/// `a + b - c` per channel, each clamped.
fn add_subtract_full(a: u32, b: u32, c: u32) -> u32 {
    let mut out = 0u32;
    for shift in [0u32, 8, 16, 24] {
        out |= clamp_channel(channel(a, shift) + channel(b, shift) - channel(c, shift)) << shift;
    }
    out
}

/// `mean(a, b) + (mean(a, b) - c) / 2` per channel, each clamped.
fn add_subtract_half(a: u32, b: u32, c: u32) -> u32 {
    let mean = average2(a, b);
    let mut out = 0u32;
    for shift in [0u32, 8, 16, 24] {
        let value = channel(mean, shift);
        out |= clamp_channel(value + (value - channel(c, shift)) / 2) << shift;
    }
    out
}

/// Whichever of `a` and `b` the gradient through `c` favours.
fn select(a: u32, b: u32, c: u32) -> u32 {
    let mut difference = 0i32;
    for shift in [0u32, 8, 16, 24] {
        let (a, b, c) = (channel(a, shift), channel(b, shift), channel(c, shift));
        difference += (b - c).abs() - (a - c).abs();
    }
    if difference <= 0 {
        a
    } else {
        b
    }
}

/// The predictor a mode names, given the left, top, top-left, and top-right
/// neighbours.
fn predict(mode: u32, left: u32, top: u32, top_left: u32, top_right: u32) -> u32 {
    match mode {
        1 => left,
        2 => top,
        3 => top_right,
        4 => top_left,
        5 => average2(average2(left, top_right), top),
        6 => average2(left, top_left),
        7 => average2(left, top),
        8 => average2(top_left, top),
        9 => average2(top, top_right),
        10 => average2(average2(left, top_left), average2(top, top_right)),
        11 => select(top, left, top_left),
        12 => add_subtract_full(left, top, top_left),
        13 => add_subtract_half(left, top, top_left),
        // Mode zero, and every value past the fourteen the format defines,
        // predicts opaque black. The mode comes from a decoded pixel's low
        // nibble, so there is nothing to refuse: the format assigns the
        // whole nibble and the predictor image is data, not structure.
        _ => 0xFF00_0000,
    }
}

/// Undo the predictor transform in place.
///
/// In place is exact rather than an optimisation: a predictor reads only
/// pixels already restored, and the residual it adds to them is the pixel
/// it is about to overwrite.
fn undo_predictor(transform: &Transform, pixels: &mut [u32]) -> Result<(), DecodeError> {
    let width = usize::try_from(transform.width).unwrap_or(usize::MAX);
    let height = usize::try_from(transform.height).unwrap_or(usize::MAX);
    if width == 0 || pixels.len() < width * height {
        return Err(DecodeError::WebpLosslessInvalidGeometry);
    }
    let stride = usize::try_from(blocks(transform.width, transform.bits)).unwrap_or(usize::MAX);
    let block_shift = transform.bits.min(31);
    for y in 0..height {
        let row = y * width;
        let block_row = (y >> block_shift) * stride;
        for x in 0..width {
            let at = row + x;
            let mode = if y == 0 && x == 0 {
                0
            } else if y == 0 {
                1
            } else if x == 0 {
                2
            } else {
                let block = block_row + (x >> block_shift);
                (transform.data.get(block).copied().unwrap_or(0) >> 8) & 0x0F
            };
            let up = at.wrapping_sub(width);
            let neighbour = |index: usize| pixels.get(index).copied().unwrap_or(0);
            let (left, top, top_left, top_right) = if y == 0 {
                (neighbour(at.wrapping_sub(1)), 0, 0, 0)
            } else {
                (
                    neighbour(at.wrapping_sub(1)),
                    neighbour(up),
                    neighbour(up.wrapping_sub(1)),
                    // The rightmost pixel's top-right neighbour is the
                    // leftmost pixel of its own row, which the format
                    // states and one contiguous buffer already gives.
                    neighbour(up + 1),
                )
            };
            let predicted = predict(mode, left, top, top_left, top_right);
            let residual = *pixels
                .get(at)
                .ok_or(DecodeError::WebpLosslessInvalidGeometry)?;
            let mut restored = 0u32;
            for shift in [0u32, 8, 16, 24] {
                let sum =
                    ((residual >> shift) & 0xFF).wrapping_add((predicted >> shift) & 0xFF) & 0xFF;
                restored |= sum << shift;
            }
            *pixels
                .get_mut(at)
                .ok_or(DecodeError::WebpLosslessInvalidGeometry)? = restored;
        }
    }
    Ok(())
}

/// One block's cross-colour multipliers.
fn multipliers(code: u32) -> (i32, i32, i32) {
    let signed = |shift: u32| {
        i32::from(
            u8::try_from((code >> shift) & 0xFF)
                .unwrap_or(0)
                .cast_signed(),
        )
    };
    (signed(0), signed(8), signed(16))
}

/// Undo the cross-colour transform in place.
fn undo_cross_colour(transform: &Transform, pixels: &mut [u32]) -> Result<(), DecodeError> {
    let width = usize::try_from(transform.width).unwrap_or(usize::MAX);
    let height = usize::try_from(transform.height).unwrap_or(usize::MAX);
    if width == 0 || pixels.len() < width * height {
        return Err(DecodeError::WebpLosslessInvalidGeometry);
    }
    let stride = usize::try_from(blocks(transform.width, transform.bits)).unwrap_or(usize::MAX);
    let block_shift = transform.bits.min(31);
    for y in 0..height {
        let row = y * width;
        let block_row = (y >> block_shift) * stride;
        for x in 0..width {
            let block = block_row + (x >> block_shift);
            let (to_red, to_blue, red_into_blue) =
                multipliers(transform.data.get(block).copied().unwrap_or(0));
            let pixel = *pixels
                .get(row + x)
                .ok_or(DecodeError::WebpLosslessInvalidGeometry)?;
            let green = i32::from(u8::try_from((pixel >> 8) & 0xFF).unwrap_or(0).cast_signed());
            let red = (channel(pixel, 16) + ((to_red * green) >> 5)) & 0xFF;
            let red_signed = i32::from(u8::try_from(red).unwrap_or(0).cast_signed());
            let blue = (channel(pixel, 0)
                + ((to_blue * green) >> 5)
                + ((red_into_blue * red_signed) >> 5))
                & 0xFF;
            let byte = |value: i32| u32::try_from(value & 0xFF).unwrap_or(0);
            *pixels
                .get_mut(row + x)
                .ok_or(DecodeError::WebpLosslessInvalidGeometry)? =
                (pixel & 0xFF00_FF00) | (byte(red) << 16) | byte(blue);
        }
    }
    Ok(())
}

/// Undo the colour-indexing transform in place, widening each row from the
/// bundled form back to the picture's own width.
///
/// Rows and pixels are walked backwards because a bundled row is never
/// wider than the row it expands to, so writing from the end never
/// overwrites an index still to be read.
fn undo_colour_indexing(transform: &Transform, pixels: &mut [u32]) -> Result<(), DecodeError> {
    let width = usize::try_from(transform.width).unwrap_or(usize::MAX);
    let height = usize::try_from(transform.height).unwrap_or(usize::MAX);
    let bundled = usize::try_from(blocks(transform.width, transform.bits)).unwrap_or(usize::MAX);
    if width == 0 || bundled == 0 || pixels.len() < width * height {
        return Err(DecodeError::WebpLosslessInvalidGeometry);
    }
    let per_byte = 1usize << transform.bits.min(3);
    let index_bits = 8u32 >> transform.bits.min(3);
    let mask = (1u32 << index_bits) - 1;
    for y in (0..height).rev() {
        for x in (0..width).rev() {
            let packed = *pixels
                .get(y * bundled + x / per_byte)
                .ok_or(DecodeError::WebpLosslessInvalidGeometry)?;
            let index =
                ((packed >> 8) & 0xFF) >> (index_bits * u32::try_from(x % per_byte).unwrap_or(0));
            let colour = transform
                .data
                .get(usize::try_from(index & mask).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(0);
            *pixels
                .get_mut(y * width + x)
                .ok_or(DecodeError::WebpLosslessInvalidGeometry)? = colour;
        }
    }
    Ok(())
}

/// Read the geometry a `VP8L` chunk declares, decoding no pixels.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    header(&mut Bits::new(bytes))
}

/// Read the signature, geometry, and version a `VP8L` stream opens with.
fn header(bits: &mut Bits<'_>) -> Result<(u32, u32), DecodeError> {
    if bits.read(8)? != u32::from(SIGNATURE) {
        return Err(DecodeError::WebpLosslessBadSignature);
    }
    let width = bits.read(14)? + 1;
    let height = bits.read(14)? + 1;
    // Whether the picture uses its alpha channel is a hint for a consumer
    // choosing a buffer, not a constraint on the pixels, so it is read and
    // not acted on.
    let _alpha_used = bits.flag()?;
    if bits.read(3)? != 0 {
        return Err(DecodeError::WebpLosslessUnsupportedVersion);
    }
    Ok((width, height))
}

/// Decode a `VP8L` chunk into straight-alpha RGBA8.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let mut bits = Bits::new(bytes);
    let (width, height) = header(&mut bits)?;
    limits.check(width, height)?;
    let pixels = spatially_coded_image(&mut bits, width, height)?;
    let mut rgba = fallible::filled(
        pixels
            .len()
            .checked_mul(RGBA_BYTES)
            .ok_or(DecodeError::DimensionsOverflow)?,
        0u8,
    )
    .ok_or(DecodeError::OutOfMemory)?;
    for (pixel, out) in pixels.iter().zip(rgba.as_chunks_mut::<RGBA_BYTES>().0) {
        out[0] = u8::try_from((pixel >> 16) & 0xFF).unwrap_or(0);
        out[1] = u8::try_from((pixel >> 8) & 0xFF).unwrap_or(0);
        out[2] = u8::try_from(pixel & 0xFF).unwrap_or(0);
        out[3] = u8::try_from((pixel >> 24) & 0xFF).unwrap_or(0);
    }
    Ok(RasterImage::from_parts(width, height, rgba))
}

/// Decode a compressed `ALPH` plane, whose geometry its picture supplies and
/// whose samples are the green channel.
pub(crate) fn decode_alpha(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, DecodeError> {
    let mut bits = Bits::new(bytes);
    let pixels = spatially_coded_image(&mut bits, width, height)?;
    fallible::collected(
        pixels.len(),
        pixels
            .iter()
            .map(|pixel| u8::try_from((pixel >> 8) & 0xFF).unwrap_or(0)),
    )
    .ok_or(DecodeError::OutOfMemory)
}

/// Decode the transform chain, the prefix codes, and the pixels a level-zero
/// stream is, answering restored ARGB pixels.
fn spatially_coded_image(
    bits: &mut Bits<'_>,
    width: u32,
    height: u32,
) -> Result<Vec<u32>, DecodeError> {
    let count = pixel_count(width, height)?;
    let mut coded_width = width;
    let transforms = read_transforms(bits, &mut coded_width, height)?;
    let packed = pixel_count(coded_width, height)?;
    let cache_bits = read_cache_bits(bits)?;
    let meta = read_meta(bits, coded_width, height)?;
    let groups = meta.as_ref().map_or(Ok(1), |meta| {
        let highest = meta.groups.iter().copied().max().unwrap_or(0);
        if highest >= MAX_GROUPS {
            return Err(DecodeError::WebpLosslessInvalidCode);
        }
        let groups = highest + 1;
        // A sparse entropy image can name a high group while spending
        // almost nothing, so the count is held to what the remaining bits
        // could actually describe.
        if usize::try_from(groups)
            .ok()
            .and_then(|groups| groups.checked_mul(MIN_GROUP_BITS))
            .is_none_or(|needed| needed > bits.remaining())
        {
            return Err(DecodeError::WebpLosslessTruncated);
        }
        Ok(groups)
    })?;
    let codes = read_groups(bits, cache_bits, groups)?;
    // Sized to the restored picture rather than the coded one, so the
    // colour-indexing transform has the room to widen each row in place.
    let mut pixels = fallible::filled(count.max(packed), 0u32).ok_or(DecodeError::OutOfMemory)?;
    let bundled = pixels.get_mut(..packed).ok_or(DecodeError::OutOfMemory)?;
    decode_pixels(
        bits,
        &codes,
        meta.as_ref(),
        cache_bits,
        coded_width,
        bundled,
    )?;
    apply_transforms(&transforms, &mut pixels)?;
    pixels.truncate(count);
    Ok(pixels)
}

/// Read the meta-prefix arrangement, or `None` where one group covers the
/// whole picture.
fn read_meta(bits: &mut Bits<'_>, width: u32, height: u32) -> Result<Option<Meta>, DecodeError> {
    if !bits.flag()? {
        return Ok(None);
    }
    let block_bits = 2 + bits.read(3)?;
    let stride = blocks(width, block_bits);
    let rows = blocks(height, block_bits);
    let image = entropy_coded_image(bits, stride, rows)?;
    // A group index is the entropy image's red and green bytes together.
    let groups = fallible::collected(image.len(), image.iter().map(|pixel| (pixel >> 8) & 0xFFFF))
        .ok_or(DecodeError::OutOfMemory)?;
    Ok(Some(Meta {
        groups,
        bits: block_bits,
        stride,
    }))
}

#[cfg(test)]
#[path = "vp8l_fixture.rs"]
pub(crate) mod fixture;

#[cfg(test)]
#[path = "vp8l_tests.rs"]
mod tests;
