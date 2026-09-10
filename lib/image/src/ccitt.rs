//! CCITT Group 3 and Group 4 facsimile decoding (ITU-T T.4 and T.6), as
//! TIFF's compressions 2, 3, and 4 carry it.
//!
//! A fax codes *runs of white and black*, so what it produces is one bit per
//! pixel with a set bit meaning black — the arrangement TIFF's `WhiteIsZero`
//! photometric describes, and the one every fax declares. A file that pairs
//! it with `BlackIsZero` gets an inverted picture, because that is what it
//! asked for; nothing here second-guesses the photometric.
//!
//! Uncompressed mode (the `0000001111` extension of ITU-T T.4 §4.2.1.3) is
//! refused by name rather than half-read: it is a bypass of the run coding
//! rather than a part of it, no encoder in circulation emits it, and
//! guessing at it would be a fabricated picture.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::DecodeError;

/// Bits of lookahead one table entry is found by: the longest code either
/// table holds is thirteen bits.
const LOOKUP_BITS: u32 = 13;
const LOOKUP_LEN: usize = 1 << LOOKUP_BITS;

/// Bits of the packed table entry given to the code's own length, which
/// leaves the run in the rest. A zero-length entry is one no code fills.
const LENGTH_BITS: u32 = 4;
const LENGTH_MASK: u16 = (1 << LENGTH_BITS) - 1;

/// Leading zeros an end-of-line code carries before its one bit. No run or
/// mode code holds more than seven, so a run this long can only be an EOL
/// and whatever fill preceded it.
const EOL_ZEROS: u32 = 11;

/// White run-length codes: terminating runs 0..=63, then the makeup runs
/// (ITU-T T.4 tables 2 and 3), as (code, bit length, run).
const WHITE_CODES: &[(u16, u8, u16)] = &[
    (0b0011_0101, 8, 0),
    (0b00_0111, 6, 1),
    (0b0111, 4, 2),
    (0b1000, 4, 3),
    (0b1011, 4, 4),
    (0b1100, 4, 5),
    (0b1110, 4, 6),
    (0b1111, 4, 7),
    (0b10011, 5, 8),
    (0b10100, 5, 9),
    (0b00111, 5, 10),
    (0b01000, 5, 11),
    (0b00_1000, 6, 12),
    (0b00_0011, 6, 13),
    (0b11_0100, 6, 14),
    (0b11_0101, 6, 15),
    (0b10_1010, 6, 16),
    (0b10_1011, 6, 17),
    (0b010_0111, 7, 18),
    (0b000_1100, 7, 19),
    (0b000_1000, 7, 20),
    (0b001_0111, 7, 21),
    (0b000_0011, 7, 22),
    (0b000_0100, 7, 23),
    (0b010_1000, 7, 24),
    (0b010_1011, 7, 25),
    (0b001_0011, 7, 26),
    (0b010_0100, 7, 27),
    (0b001_1000, 7, 28),
    (0b0000_0010, 8, 29),
    (0b0000_0011, 8, 30),
    (0b0001_1010, 8, 31),
    (0b0001_1011, 8, 32),
    (0b0001_0010, 8, 33),
    (0b0001_0011, 8, 34),
    (0b0001_0100, 8, 35),
    (0b0001_0101, 8, 36),
    (0b0001_0110, 8, 37),
    (0b0001_0111, 8, 38),
    (0b0010_1000, 8, 39),
    (0b0010_1001, 8, 40),
    (0b0010_1010, 8, 41),
    (0b0010_1011, 8, 42),
    (0b0010_1100, 8, 43),
    (0b0010_1101, 8, 44),
    (0b0000_0100, 8, 45),
    (0b0000_0101, 8, 46),
    (0b0000_1010, 8, 47),
    (0b0000_1011, 8, 48),
    (0b0101_0010, 8, 49),
    (0b0101_0011, 8, 50),
    (0b0101_0100, 8, 51),
    (0b0101_0101, 8, 52),
    (0b0010_0100, 8, 53),
    (0b0010_0101, 8, 54),
    (0b0101_1000, 8, 55),
    (0b0101_1001, 8, 56),
    (0b0101_1010, 8, 57),
    (0b0101_1011, 8, 58),
    (0b0100_1010, 8, 59),
    (0b0100_1011, 8, 60),
    (0b0011_0010, 8, 61),
    (0b0011_0011, 8, 62),
    (0b0011_0100, 8, 63),
    (0b11011, 5, 64),
    (0b10010, 5, 128),
    (0b01_0111, 6, 192),
    (0b011_0111, 7, 256),
    (0b0011_0110, 8, 320),
    (0b0011_0111, 8, 384),
    (0b0110_0100, 8, 448),
    (0b0110_0101, 8, 512),
    (0b0110_1000, 8, 576),
    (0b0110_0111, 8, 640),
    (0b0_1100_1100, 9, 704),
    (0b0_1100_1101, 9, 768),
    (0b0_1101_0010, 9, 832),
    (0b0_1101_0011, 9, 896),
    (0b0_1101_0100, 9, 960),
    (0b0_1101_0101, 9, 1024),
    (0b0_1101_0110, 9, 1088),
    (0b0_1101_0111, 9, 1152),
    (0b0_1101_1000, 9, 1216),
    (0b0_1101_1001, 9, 1280),
    (0b0_1101_1010, 9, 1344),
    (0b0_1101_1011, 9, 1408),
    (0b0_1001_1000, 9, 1472),
    (0b0_1001_1001, 9, 1536),
    (0b0_1001_1010, 9, 1600),
    (0b01_1000, 6, 1664),
    (0b0_1001_1011, 9, 1728),
];

/// Black run-length codes, as [`WHITE_CODES`].
const BLACK_CODES: &[(u16, u8, u16)] = &[
    (0b00_0011_0111, 10, 0),
    (0b010, 3, 1),
    (0b11, 2, 2),
    (0b10, 2, 3),
    (0b011, 3, 4),
    (0b0011, 4, 5),
    (0b0010, 4, 6),
    (0b00011, 5, 7),
    (0b00_0101, 6, 8),
    (0b00_0100, 6, 9),
    (0b000_0100, 7, 10),
    (0b000_0101, 7, 11),
    (0b000_0111, 7, 12),
    (0b0000_0100, 8, 13),
    (0b0000_0111, 8, 14),
    (0b0_0001_1000, 9, 15),
    (0b00_0001_0111, 10, 16),
    (0b00_0001_1000, 10, 17),
    (0b00_0000_1000, 10, 18),
    (0b000_0110_0111, 11, 19),
    (0b000_0110_1000, 11, 20),
    (0b000_0110_1100, 11, 21),
    (0b000_0011_0111, 11, 22),
    (0b000_0010_1000, 11, 23),
    (0b000_0001_0111, 11, 24),
    (0b000_0001_1000, 11, 25),
    (0b0000_1100_1010, 12, 26),
    (0b0000_1100_1011, 12, 27),
    (0b0000_1100_1100, 12, 28),
    (0b0000_1100_1101, 12, 29),
    (0b0000_0110_1000, 12, 30),
    (0b0000_0110_1001, 12, 31),
    (0b0000_0110_1010, 12, 32),
    (0b0000_0110_1011, 12, 33),
    (0b0000_1101_0010, 12, 34),
    (0b0000_1101_0011, 12, 35),
    (0b0000_1101_0100, 12, 36),
    (0b0000_1101_0101, 12, 37),
    (0b0000_1101_0110, 12, 38),
    (0b0000_1101_0111, 12, 39),
    (0b0000_0110_1100, 12, 40),
    (0b0000_0110_1101, 12, 41),
    (0b0000_1101_1010, 12, 42),
    (0b0000_1101_1011, 12, 43),
    (0b0000_0101_0100, 12, 44),
    (0b0000_0101_0101, 12, 45),
    (0b0000_0101_0110, 12, 46),
    (0b0000_0101_0111, 12, 47),
    (0b0000_0110_0100, 12, 48),
    (0b0000_0110_0101, 12, 49),
    (0b0000_0101_0010, 12, 50),
    (0b0000_0101_0011, 12, 51),
    (0b0000_0010_0100, 12, 52),
    (0b0000_0011_0111, 12, 53),
    (0b0000_0011_1000, 12, 54),
    (0b0000_0010_0111, 12, 55),
    (0b0000_0010_1000, 12, 56),
    (0b0000_0101_1000, 12, 57),
    (0b0000_0101_1001, 12, 58),
    (0b0000_0010_1011, 12, 59),
    (0b0000_0010_1100, 12, 60),
    (0b0000_0101_1010, 12, 61),
    (0b0000_0110_0110, 12, 62),
    (0b0000_0110_0111, 12, 63),
    (0b00_0000_1111, 10, 64),
    (0b0000_1100_1000, 12, 128),
    (0b0000_1100_1001, 12, 192),
    (0b0000_0101_1011, 12, 256),
    (0b0000_0011_0011, 12, 320),
    (0b0000_0011_0100, 12, 384),
    (0b0000_0011_0101, 12, 448),
    (0b0_0000_0110_1100, 13, 512),
    (0b0_0000_0110_1101, 13, 576),
    (0b0_0000_0100_1010, 13, 640),
    (0b0_0000_0100_1011, 13, 704),
    (0b0_0000_0100_1100, 13, 768),
    (0b0_0000_0100_1101, 13, 832),
    (0b0_0000_0111_0010, 13, 896),
    (0b0_0000_0111_0011, 13, 960),
    (0b0_0000_0111_0100, 13, 1024),
    (0b0_0000_0111_0101, 13, 1088),
    (0b0_0000_0111_0110, 13, 1152),
    (0b0_0000_0111_0111, 13, 1216),
    (0b0_0000_0101_0010, 13, 1280),
    (0b0_0000_0101_0011, 13, 1344),
    (0b0_0000_0101_0100, 13, 1408),
    (0b0_0000_0101_0101, 13, 1472),
    (0b0_0000_0101_1010, 13, 1536),
    (0b0_0000_0101_1011, 13, 1600),
    (0b0_0000_0110_0100, 13, 1664),
    (0b0_0000_0110_0101, 13, 1728),
];

/// Extended makeup codes, which both colours share (ITU-T T.4 table 4).
const EXTENDED_CODES: &[(u16, u8, u16)] = &[
    (0b000_0000_1000, 11, 1792),
    (0b000_0000_1100, 11, 1856),
    (0b000_0000_1101, 11, 1920),
    (0b0000_0001_0010, 12, 1984),
    (0b0000_0001_0011, 12, 2048),
    (0b0000_0001_0100, 12, 2112),
    (0b0000_0001_0101, 12, 2176),
    (0b0000_0001_0110, 12, 2240),
    (0b0000_0001_0111, 12, 2304),
    (0b0000_0001_1100, 12, 2368),
    (0b0000_0001_1101, 12, 2432),
    (0b0000_0001_1110, 12, 2496),
    (0b0000_0001_1111, 12, 2560),
];

/// The white and black run-length codes, expanded into direct lookup tables.
///
/// Built once per image rather than compiled in: only a fax reaches this
/// module at all, so every other consumer of the crate would otherwise carry
/// 32 KiB of tables it never reads.
pub(crate) struct Codes {
    white: Vec<u16>,
    black: Vec<u16>,
}

impl Codes {
    pub(crate) fn new() -> Option<Self> {
        Some(Self {
            white: expand_codes(WHITE_CODES)?,
            black: expand_codes(BLACK_CODES)?,
        })
    }

    const fn table(&self, black: bool) -> &Vec<u16> {
        if black {
            &self.black
        } else {
            &self.white
        }
    }
}

/// Fill every lookahead value a code prefixes with that code's run and
/// length.
fn expand_codes(codes: &[(u16, u8, u16)]) -> Option<Vec<u16>> {
    let mut table = fallible::filled(LOOKUP_LEN, 0u16)?;
    for &(code, len, run) in codes.iter().chain(EXTENDED_CODES) {
        let shift = LOOKUP_BITS - u32::from(len);
        let from = usize::from(code) << shift;
        let entry = (run << LENGTH_BITS) | u16::from(len);
        for slot in table.get_mut(from..from + (1 << shift))? {
            *slot = entry;
        }
    }
    Some(table)
}

/// Which of the three codings a strip carries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Coding {
    /// TIFF compression 2: one-dimensional runs, each row starting on a byte
    /// boundary, with no end-of-line codes.
    ModifiedHuffman,
    /// TIFF compression 3 (ITU-T T.4), whose rows carry end-of-line codes
    /// and, where two-dimensional coding was permitted, the tag bit after
    /// each that says how the row that follows is coded.
    Group3 { two_dimensional: bool },
    /// TIFF compression 4 (ITU-T T.6): every row two-dimensional against its
    /// predecessor, with no end-of-line codes.
    Group4,
}

/// A forward bit reader over the coded data.
///
/// `lsb_first` is TIFF's `FillOrder` 2, where the first bit of the stream is
/// a byte's least significant rather than its most.
struct Bits<'a> {
    data: &'a [u8],
    at: u64,
    lsb_first: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], lsb_first: bool) -> Self {
        Self {
            data,
            at: 0,
            lsb_first,
        }
    }

    fn total(&self) -> u64 {
        self.data.len() as u64 * 8
    }

    fn remaining(&self) -> u64 {
        self.total().saturating_sub(self.at)
    }

    fn bit_at(&self, at: u64) -> u32 {
        let Ok(index) = usize::try_from(at / 8) else {
            return 0;
        };
        let Some(&byte) = self.data.get(index) else {
            return 0;
        };
        let within = (at % 8) as u32;
        let shift = if self.lsb_first { within } else { 7 - within };
        u32::from(byte >> shift) & 1
    }

    /// The next `count` bits, most significant first and zero-padded past
    /// the end of the data.
    fn peek(&self, count: u32) -> u32 {
        let mut value = 0u32;
        for offset in 0..u64::from(count) {
            value = value << 1 | self.bit_at(self.at + offset);
        }
        value
    }

    fn skip(&mut self, count: u32) {
        self.at = self.at.saturating_add(u64::from(count));
    }

    fn align_to_byte(&mut self) {
        self.at = self.at.next_multiple_of(8);
    }

    /// Consume an end-of-line code, and whatever zero fill precedes it, if
    /// one is next; answer whether one was.
    fn take_eol(&mut self) -> bool {
        let start = self.at;
        let mut zeros = 0u64;
        while self.at < self.total() && self.bit_at(self.at) == 0 {
            self.at += 1;
            zeros += 1;
        }
        if zeros >= u64::from(EOL_ZEROS) && self.at < self.total() {
            self.at += 1;
            return true;
        }
        self.at = start;
        false
    }
}

/// The two-dimensional mode codes (ITU-T T.4 table 4).
enum Mode {
    Pass,
    Horizontal,
    /// The current row's changing element sits this far from the reference
    /// row's.
    Vertical(i32),
}

/// Read the next two-dimensional mode code.
fn mode(bits: &mut Bits<'_>) -> Result<Mode, DecodeError> {
    let peeked = bits.peek(7);
    let (mode, len) = match peeked {
        _ if peeked >> 6 == 0b1 => (Mode::Vertical(0), 1),
        _ if peeked >> 4 == 0b011 => (Mode::Vertical(1), 3),
        _ if peeked >> 4 == 0b010 => (Mode::Vertical(-1), 3),
        _ if peeked >> 4 == 0b001 => (Mode::Horizontal, 3),
        _ if peeked >> 3 == 0b0001 => (Mode::Pass, 4),
        _ if peeked >> 1 == 0b00_0011 => (Mode::Vertical(2), 6),
        _ if peeked >> 1 == 0b00_0010 => (Mode::Vertical(-2), 6),
        0b000_0011 => (Mode::Vertical(3), 7),
        0b000_0010 => (Mode::Vertical(-3), 7),
        0b000_0001 => return Err(DecodeError::TiffFaxUncompressedMode),
        // No mode code opens with seven zeros, so this is an end-of-line, an
        // end-of-facsimile block, or padding: the coded rows have run out.
        _ => return Err(DecodeError::TiffFaxTruncated),
    };
    if bits.remaining() < u64::from(len) {
        return Err(DecodeError::TiffFaxTruncated);
    }
    bits.skip(len);
    Ok(mode)
}

/// Read one complete run of `black`, following makeup codes to the
/// terminating code that ends the run.
fn run(bits: &mut Bits<'_>, codes: &Codes, black: bool) -> Result<u32, DecodeError> {
    let table = codes.table(black);
    let mut total = 0u32;
    loop {
        let entry = table
            .get(bits.peek(LOOKUP_BITS) as usize)
            .copied()
            .unwrap_or(0);
        let len = u32::from(entry & LENGTH_MASK);
        if len == 0 {
            return Err(DecodeError::TiffFaxBadCode);
        }
        if bits.remaining() < u64::from(len) {
            return Err(DecodeError::TiffFaxTruncated);
        }
        bits.skip(len);
        let length = u32::from(entry >> LENGTH_BITS);
        total = total
            .checked_add(length)
            .ok_or(DecodeError::TiffFaxRowOverflow)?;
        // Only a makeup code is followed by more of the same run, and every
        // makeup is a multiple of 64 at or above it.
        if length < 64 {
            return Ok(total);
        }
    }
}

/// Set bits `from`..`to` of a row, which is where its black pixels are.
fn fill(row: &mut [u8], from: u32, to: u32) {
    if to <= from {
        return;
    }
    let (first, last) = ((from / 8) as usize, ((to - 1) / 8) as usize);
    let head = 0xFFu8 >> (from % 8);
    let tail = 0xFFu8 << (7 - (to - 1) % 8);
    if first == last {
        if let Some(byte) = row.get_mut(first) {
            *byte |= head & tail;
        }
        return;
    }
    if let Some(byte) = row.get_mut(first) {
        *byte |= head;
    }
    if let Some(whole) = row.get_mut(first + 1..last) {
        whole.fill(0xFF);
    }
    if let Some(byte) = row.get_mut(last) {
        *byte |= tail;
    }
}

/// Record a changing element, refusing a row that produces more than the
/// two per mode its strictly-advancing walk can.
///
/// The reservation is what makes the push infallible, so a row that somehow
/// reached the ceiling is refused rather than growing the buffer.
fn record(changes: &mut Vec<u32>, at: u32) -> Result<(), DecodeError> {
    if changes.len() >= changes.capacity() {
        return Err(DecodeError::TiffFaxRowOverflow);
    }
    changes.push(at);
    Ok(())
}

/// Decode one one-dimensionally coded row, recording its changing elements.
fn row_1d(
    bits: &mut Bits<'_>,
    codes: &Codes,
    columns: u32,
    row: &mut [u8],
    changes: &mut Vec<u32>,
) -> Result<(), DecodeError> {
    let mut at = 0u32;
    let mut black = false;
    while at < columns {
        let length = run(bits, codes, black)?;
        let end = at
            .checked_add(length)
            .filter(|end| *end <= columns)
            .ok_or(DecodeError::TiffFaxRowOverflow)?;
        if black {
            fill(row, at, end);
        }
        record(changes, end)?;
        at = end;
        black = !black;
    }
    Ok(())
}

/// Decode one two-dimensionally coded row against `reference`, recording its
/// own changing elements.
///
/// `a0` only ever advances, so the reference row's cursor only ever advances
/// with it: the element the next mode is coded against is found by carrying
/// that cursor forward rather than by searching the row again.
fn row_2d(
    bits: &mut Bits<'_>,
    codes: &Codes,
    columns: u32,
    reference: &[u32],
    row: &mut [u8],
    changes: &mut Vec<u32>,
) -> Result<(), DecodeError> {
    let mut a0 = -1i64;
    let mut black = false;
    let mut cursor = 0usize;
    let at_or_end = |slot: usize| reference.get(slot).copied().unwrap_or(columns);
    while a0 < i64::from(columns) {
        let start = u32::try_from(a0.max(0)).unwrap_or(u32::MAX);
        while reference.get(cursor).is_some_and(|&at| i64::from(at) <= a0) {
            cursor += 1;
        }
        // An even-indexed changing element is one the reference row turns
        // black at and an odd-indexed one is where it turns white, so the
        // parity the cursor lands on decides whether it names the element of
        // the colour opposite `black` or the one after it does.
        let index = if cursor % 2 == usize::from(black) {
            cursor
        } else {
            cursor + 1
        };
        let next = match mode(bits)? {
            Mode::Pass => {
                let b2 = at_or_end(index + 1);
                if black {
                    fill(row, start, b2);
                }
                i64::from(b2)
            }
            Mode::Horizontal => {
                let first = run(bits, codes, black)?;
                let second = run(bits, codes, !black)?;
                let a1 = start
                    .checked_add(first)
                    .filter(|a1| *a1 <= columns)
                    .ok_or(DecodeError::TiffFaxRowOverflow)?;
                let a2 = a1
                    .checked_add(second)
                    .filter(|a2| *a2 <= columns)
                    .ok_or(DecodeError::TiffFaxRowOverflow)?;
                if black {
                    fill(row, start, a1);
                } else {
                    fill(row, a1, a2);
                }
                record(changes, a1)?;
                record(changes, a2)?;
                i64::from(a2)
            }
            Mode::Vertical(delta) => {
                let a1 = i64::from(at_or_end(index)) + i64::from(delta);
                let a1 = u32::try_from(a1)
                    .ok()
                    .filter(|a1| *a1 >= start && *a1 <= columns)
                    .ok_or(DecodeError::TiffFaxRowOverflow)?;
                if black {
                    fill(row, start, a1);
                }
                record(changes, a1)?;
                black = !black;
                i64::from(a1)
            }
        };
        // Every mode advances past the element it just placed, so a stream
        // that fails to is malformed rather than merely unusual — and
        // refusing it is what bounds this loop.
        if next <= a0 {
            return Err(DecodeError::TiffFaxRowOverflow);
        }
        a0 = next;
    }
    Ok(())
}

/// Decode `rows` rows of `columns` pixels into `out`, which the caller has
/// sized `rows * ceil(columns / 8)` and zeroed.
///
/// A set bit is a black pixel. Data left after the last row — an end-of-facsimile
/// block, fill, or a further page's worth — is not read.
pub(crate) fn decode(
    data: &[u8],
    codes: &Codes,
    coding: Coding,
    columns: u32,
    rows: u32,
    lsb_first: bool,
    out: &mut [u8],
) -> Result<(), DecodeError> {
    let stride =
        usize::try_from(columns.div_ceil(8)).map_err(|_| DecodeError::DimensionsOverflow)?;
    // A mode places at most two changing elements and must advance past the
    // last of them, so a row of `columns` pixels holds no more than this and
    // the reservation is what lets a record be infallible.
    let elements = usize::try_from(columns)
        .ok()
        .and_then(|columns| columns.checked_mul(2))
        .and_then(|elements| elements.checked_add(4))
        .ok_or(DecodeError::DimensionsOverflow)?;
    let mut bits = Bits::new(data, lsb_first);
    let mut reference: Vec<u32> = Vec::new();
    let mut changes: Vec<u32> = Vec::new();
    if !fallible::reserve(&mut reference, elements) || !fallible::reserve(&mut changes, elements) {
        return Err(DecodeError::OutOfMemory);
    }
    let mut two_dimensional = matches!(coding, Coding::Group4);
    for index in 0..rows {
        match coding {
            Coding::ModifiedHuffman => bits.align_to_byte(),
            Coding::Group3 {
                two_dimensional: allowed,
            } => {
                let synced = bits.take_eol();
                if allowed {
                    if synced {
                        if bits.remaining() == 0 {
                            return Err(DecodeError::TiffFaxTruncated);
                        }
                        // The tag bit after an end-of-line says how the row
                        // that follows is coded: set for one-dimensional.
                        two_dimensional = bits.peek(1) == 0;
                        bits.skip(1);
                    } else if index > 0 {
                        // T.4's first line is one-dimensional whether or not
                        // its end-of-line was written, but a later row with
                        // no tag names no coding at all.
                        return Err(DecodeError::TiffFaxMissingSync);
                    }
                }
            }
            Coding::Group4 => {}
        }
        if bits.remaining() == 0 {
            return Err(DecodeError::TiffFaxTruncated);
        }
        let from = (index as usize)
            .checked_mul(stride)
            .ok_or(DecodeError::DimensionsOverflow)?;
        let row = from
            .checked_add(stride)
            .and_then(|end| out.get_mut(from..end))
            .ok_or(DecodeError::DimensionsOverflow)?;
        changes.clear();
        if two_dimensional {
            row_2d(&mut bits, codes, columns, &reference, row, &mut changes)?;
        } else {
            row_1d(&mut bits, codes, columns, row, &mut changes)?;
        }
        core::mem::swap(&mut reference, &mut changes);
    }
    Ok(())
}

#[cfg(test)]
#[path = "ccitt_tests.rs"]
mod tests;
