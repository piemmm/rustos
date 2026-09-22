//! The RFC 1951 symbol tables, shared by [`crate::inflate`] and
//! [`crate::deflate`].
//!
//! Both directions read the same alphabets, so they are defined once here
//! rather than once per direction, where an edit could reach only one of
//! them.

/// Largest Huffman code length RFC 1951 permits.
pub(crate) const MAX_BITS: usize = 15;

/// Shortest back-reference DEFLATE can encode.
pub(crate) const MIN_MATCH: usize = 3;

/// Longest back-reference DEFLATE can encode.
pub(crate) const MAX_MATCH: usize = 258;

/// Largest back-reference distance, and so the sliding window's span.
pub(crate) const WINDOW_SIZE: usize = 32_768;

/// Most bytes one stored (uncompressed) block may carry, bounded by the
/// 16-bit `LEN` field.
pub(crate) const MAX_STORED: usize = 65_535;

/// Symbols in the literal/length alphabet. 286 and 287 exist so the fixed
/// code set is complete but are never emitted.
pub(crate) const LIT_SYMBOLS: usize = 288;

/// Literal/length symbols an encoder may actually emit (0..=285).
pub(crate) const LIT_CODED: usize = 286;

/// Symbols in the distance alphabet.
pub(crate) const DIST_SYMBOLS: usize = 30;

/// The end-of-block literal/length symbol.
pub(crate) const END_OF_BLOCK: usize = 256;

/// Number of symbols in the code-length alphabet (RFC 1951 §3.2.7).
pub(crate) const CODE_LENGTH_SYMBOLS: usize = 19;

/// The order code-length code lengths are transmitted in (RFC 1951
/// §3.2.7) — deliberately not ascending, so the common case of a handful of
/// short lengths and many omitted (zero) ones front-loads the codes an
/// encoder is likely to actually use.
pub(crate) const CODE_LENGTH_ORDER: [usize; CODE_LENGTH_SYMBOLS] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Base length for length symbols 257..=285, indexed by `symbol - 257`
/// (RFC 1951 §3.2.5).
pub(crate) const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

/// Extra bits following each length symbol, same indexing as
/// [`LENGTH_BASE`].
pub(crate) const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Base distance for distance symbols 0..=29 (RFC 1951 §3.2.5).
pub(crate) const DIST_BASE: [u16; DIST_SYMBOLS] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// Extra bits following each distance symbol, same indexing as
/// [`DIST_BASE`].
pub(crate) const DIST_EXTRA: [u8; DIST_SYMBOLS] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The fixed distance code lengths (RFC 1951 §3.2.6): every one of the 32
/// codes is 5 bits, even though only 0..=29 are ever legally used.
pub(crate) const FIXED_DISTANCE_LENGTHS: [u8; 32] = [5; 32];

/// The fixed literal/length code lengths (RFC 1951 §3.2.6), used by
/// `BTYPE = 01` blocks.
pub(crate) fn fixed_literal_length_lengths() -> [u8; LIT_SYMBOLS] {
    let mut lengths = [0u8; LIT_SYMBOLS];
    lengths[0..144].fill(8);
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    lengths[280..288].fill(8);
    lengths
}

/// The length symbol, its extra-bit count, and its extra-bit value for a
/// match of `length` bytes (`MIN_MATCH..=MAX_MATCH`).
///
/// [`LENGTH_BASE`] ascends and starts at [`MIN_MATCH`], so the partition
/// point is never zero and the search is five comparisons rather than a
/// second copy of the table keyed the other way round.
pub(crate) fn length_symbol(length: usize) -> (usize, u32, u32) {
    let index = LENGTH_BASE
        .partition_point(|&base| usize::from(base) <= length)
        .saturating_sub(1);
    let base = usize::from(LENGTH_BASE[index]);
    let extra = u32::from(LENGTH_EXTRA[index]);
    let value = u32::try_from(length - base).unwrap_or(0);
    (257 + index, extra, value)
}

/// The distance symbol, its extra-bit count, and its extra-bit value for a
/// back-reference of `distance` bytes (`1..=WINDOW_SIZE`).
pub(crate) fn distance_symbol(distance: usize) -> (usize, u32, u32) {
    let index = DIST_BASE
        .partition_point(|&base| usize::from(base) <= distance)
        .saturating_sub(1);
    let base = usize::from(DIST_BASE[index]);
    let extra = u32::from(DIST_EXTRA[index]);
    let value = u32::try_from(distance - base).unwrap_or(0);
    (index, extra, value)
}
