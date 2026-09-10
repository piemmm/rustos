//! Streams the VP8L tests and the container's tests are written from.
//!
//! The writers follow the specification's own rules rather than inverting
//! the decoder beside them: the bit order, the canonical code assignment,
//! and the length and distance prefix mapping are each re-derived, so a
//! round trip tests the decoder against the format.

use alloc::vec::Vec;

use super::SIGNATURE;

/// A bit stream written least significant bit first, as the format packs
/// one.
pub(crate) struct Bits {
    pub(crate) bytes: Vec<u8>,
    pub(crate) pos: usize,
}

impl Bits {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            pos: 0,
        }
    }

    /// Write `count` bits of `value`, least significant first.
    pub(crate) fn put(&mut self, value: u32, count: u32) {
        for index in 0..count {
            if self.pos.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = (value >> index) & 1;
            let at = self.pos / 8;
            self.bytes[at] |= u8::try_from(bit).expect("one bit") << (self.pos % 8);
            self.pos += 1;
        }
    }

    /// Write one prefix code, most significant bit of the code first, which
    /// is how the format packs a code into its bit stream.
    pub(crate) fn code(&mut self, code: u32, len: u32) {
        for index in (0..len).rev() {
            self.put((code >> index) & 1, 1);
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Assign canonical codes to the symbols given, shortest length first and
/// by symbol within a length.
pub(crate) fn canonical(lengths: &[(u16, u8)]) -> Vec<(u16, u32, u32)> {
    let mut sorted: Vec<(u16, u8)> = lengths.iter().copied().filter(|&(_, l)| l > 0).collect();
    sorted.sort_by_key(|&(symbol, len)| (len, symbol));
    let mut out = Vec::new();
    let mut code = 0u32;
    let mut previous = 0u8;
    for (symbol, len) in sorted {
        code <<= u32::from(len - previous);
        previous = len;
        out.push((symbol, code, u32::from(len)));
        code += 1;
    }
    out
}

/// The order the code-length code's own lengths are written in.
pub(crate) const ORDER: [u8; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

/// A prefix code written in the general form: its symbol lengths coded by a
/// code of their own.
pub(crate) struct Prefix {
    /// Every symbol's code, by symbol.
    pub(crate) codes: Vec<(u16, u32, u32)>,
}

impl Prefix {
    /// Write a code assigning `lengths` and answer the codes it assigns.
    pub(crate) fn write(bits: &mut Bits, lengths: &[(u16, u8)], alphabet: u32) -> Self {
        let highest = lengths.iter().map(|&(symbol, _)| symbol).max().unwrap_or(0);
        let coded = u32::from(highest) + 1;
        assert!(
            (2..=alphabet).contains(&coded),
            "the alphabet holds every symbol"
        );
        // The length every symbol up to the highest carries, zero where the
        // code leaves one uncoded.
        let per_symbol: Vec<u8> = (0..=highest)
            .map(|symbol| {
                lengths
                    .iter()
                    .find(|&&(at, _)| at == symbol)
                    .map_or(0, |&(_, len)| len)
            })
            .collect();
        let mut used = per_symbol.clone();
        used.sort_unstable();
        used.dedup();
        // A complete code over the length values used: where their count is
        // not a power of two, the first few take one bit less.
        let width =
            u8::try_from(used.len().next_power_of_two().trailing_zeros()).expect("a small width");
        let short = (1usize << width) - used.len();
        let length_lengths: Vec<(u16, u8)> = used
            .iter()
            .enumerate()
            .map(|(index, &value)| {
                let len = if used.len() == 1 {
                    1
                } else if index < short {
                    width - 1
                } else {
                    width
                };
                (u16::from(value), len)
            })
            .collect();
        let length_codes = canonical(&length_lengths);

        bits.put(0, 1);
        let declared = ORDER
            .iter()
            .enumerate()
            .filter(|&(_, slot)| used.contains(slot))
            .map(|(index, _)| index + 1)
            .max()
            .unwrap_or(4)
            .max(4);
        bits.put(u32::try_from(declared - 4).expect("a small count"), 4);
        for &slot in ORDER.iter().take(declared) {
            let len = length_lengths
                .iter()
                .find(|&&(symbol, _)| symbol == u16::from(slot))
                .map_or(0, |&(_, len)| u32::from(len));
            bits.put(len, 3);
        }
        // Cap the symbols coded at the highest one carrying a length, so a
        // fixture need not spell out an alphabet of zeros. The widest count
        // field the format offers holds any alphabet here.
        bits.put(1, 1);
        bits.put(7, 3);
        bits.put(coded - 2, 16);
        for &len in &per_symbol {
            let (_, code, code_len) = length_codes
                .iter()
                .copied()
                .find(|&(at, _, _)| at == u16::from(len))
                .expect("every length used has a code");
            // A code-length code holding one symbol costs no bits, so a
            // writer that spent one would run ahead of the reader.
            if length_codes.len() > 1 {
                bits.code(code, code_len);
            }
        }
        Self {
            codes: canonical(lengths),
        }
    }

    /// Write one symbol of this code.
    pub(crate) fn emit(&self, bits: &mut Bits, symbol: u16) {
        let (_, code, len) = self
            .codes
            .iter()
            .copied()
            .find(|&(at, _, _)| at == symbol)
            .expect("the symbol is in the code");
        bits.code(code, len);
    }
}

/// The prefix symbol, extra-bit value, and extra-bit count that together
/// name `value` as a length or a distance, from the format's own mapping.
pub(crate) fn prefix_for(value: u32) -> (u16, u32, u32) {
    if value <= 4 {
        return (u16::try_from(value - 1).expect("a small symbol"), 0, 0);
    }
    for symbol in 4u32..40 {
        let extra = (symbol - 2) >> 1;
        let offset = (2 + (symbol & 1)) << extra;
        if (offset + 1..=offset + (1 << extra)).contains(&value) {
            return (
                u16::try_from(symbol).expect("a small symbol"),
                value - offset - 1,
                extra,
            );
        }
    }
    panic!("no prefix names {value}")
}

/// Write a simple code naming exactly one symbol, which costs no bits to
/// read.
pub(crate) fn simple_one(bits: &mut Bits, symbol: u32) {
    bits.put(1, 1);
    bits.put(0, 1);
    if symbol < 2 {
        bits.put(0, 1);
        bits.put(symbol, 1);
    } else {
        bits.put(1, 1);
        bits.put(symbol, 8);
    }
}

/// A stream's header: signature, geometry, and version.
pub(crate) fn header(bits: &mut Bits, width: u32, height: u32) {
    bits.put(u32::from(SIGNATURE), 8);
    bits.put(width - 1, 14);
    bits.put(height - 1, 14);
    bits.put(0, 1);
    bits.put(0, 3);
}

/// One prefix-code group whose five codes each name a single symbol, so
/// every pixel of the picture is that one colour.
pub(crate) fn flat_group(bits: &mut Bits, colour: [u8; 4]) {
    simple_one(bits, u32::from(colour[1]));
    simple_one(bits, u32::from(colour[0]));
    simple_one(bits, u32::from(colour[2]));
    simple_one(bits, u32::from(colour[3]));
    simple_one(bits, 0);
}

/// A whole stream of one colour, with no transforms.
pub(crate) fn flat(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
    let mut bits = Bits::new();
    header(&mut bits, width, height);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    flat_group(&mut bits, colour);
    bits.finish()
}
