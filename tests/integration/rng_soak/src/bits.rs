//! The bit view every statistic reads its sequence through.
//!
//! A generator produces bytes; the battery's tests are defined over bits. One
//! view fixes the correspondence — most-significant bit of each byte first,
//! the conventional order for a binary sequence — so no two tests can
//! disagree about which bit is which.

/// A borrowed byte buffer read as a sequence of bits.
#[derive(Clone, Copy)]
pub struct BitSeq<'a> {
    bytes: &'a [u8],
}

impl<'a> BitSeq<'a> {
    /// View `bytes` as `8 * bytes.len()` bits.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Length of the sequence in bits.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len() * 8
    }

    /// Whether the sequence has no bits.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Bit `index` as `0` or `1`; `0` past the end, which no caller reaches
    /// because every test bounds its own loops by [`BitSeq::len`].
    #[must_use]
    #[inline]
    pub fn bit(&self, index: usize) -> u8 {
        match self.bytes.get(index / 8) {
            Some(byte) => (byte >> (7 - index % 8)) & 1,
            None => 0,
        }
    }

    /// Number of one bits in the whole sequence.
    #[must_use]
    pub fn ones(&self) -> usize {
        self.bytes.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// Number of one bits in `start..start + len`.
    ///
    /// Whole bytes inside the range are counted with a population count and
    /// only the ragged ends bit by bit, so a byte-aligned block — which every
    /// block test's is — costs a byte per eight bits rather than a shift and
    /// a mask per bit.
    #[must_use]
    pub fn ones_in(&self, start: usize, len: usize) -> usize {
        let end = (start + len).min(self.len());
        if start >= end {
            return 0;
        }
        // The first and last whole byte fully inside the range.
        let first_whole = start.div_ceil(8);
        let last_whole = end / 8;
        if first_whole >= last_whole {
            return (start..end).map(|i| usize::from(self.bit(i))).sum();
        }
        let head: usize = (start..first_whole * 8)
            .map(|i| usize::from(self.bit(i)))
            .sum();
        let middle: usize = self.bytes[first_whole..last_whole]
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum();
        let tail: usize = (last_whole * 8..end)
            .map(|i| usize::from(self.bit(i)))
            .sum();
        head + middle + tail
    }

    /// Bits `start..start + width` packed into a `u32`, first bit most
    /// significant. `width` is at most 32.
    #[must_use]
    #[inline]
    pub fn chunk(&self, start: usize, width: usize) -> u32 {
        let mut value = 0u32;
        for offset in 0..width.min(32) {
            value = (value << 1) | u32::from(self.bit(start + offset));
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::BitSeq;

    #[test]
    fn bits_read_most_significant_first() {
        let seq = BitSeq::new(&[0b1010_0001, 0b0000_0010]);
        let read: Vec<u8> = (0..16).map(|i| seq.bit(i)).collect();
        assert_eq!(read, vec![1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 0]);
        assert_eq!(seq.len(), 16);
        assert!(!seq.is_empty());
    }

    #[test]
    fn ones_counts_the_whole_sequence() {
        assert_eq!(BitSeq::new(&[0xff, 0x00, 0x0f]).ones(), 12);
        assert_eq!(BitSeq::new(&[]).ones(), 0);
        assert!(BitSeq::new(&[]).is_empty());
    }

    #[test]
    fn a_chunk_packs_bits_in_the_same_order() {
        let seq = BitSeq::new(&[0b1011_0010, 0b0100_0001]);
        assert_eq!(seq.chunk(0, 8), 0b1011_0010);
        assert_eq!(seq.chunk(4, 8), 0b0010_0100);
        assert_eq!(seq.chunk(0, 16), 0b1011_0010_0100_0001);
        assert_eq!(seq.chunk(3, 3), 0b100);
        assert_eq!(seq.chunk(0, 0), 0);
    }

    /// The popcount path and the bit-by-bit one must agree at every
    /// alignment and length, or a block test would silently measure a
    /// different range than it reads.
    #[test]
    fn a_range_count_matches_counting_bit_by_bit() {
        let bytes: Vec<u8> = (0..37u8).map(|i| i.wrapping_mul(61) ^ 0xa5).collect();
        let seq = BitSeq::new(&bytes);
        for start in 0..seq.len() {
            for len in [0usize, 1, 7, 8, 9, 15, 16, 31, 64, 100] {
                let naive: usize = (start..(start + len).min(seq.len()))
                    .map(|i| usize::from(seq.bit(i)))
                    .sum();
                assert_eq!(seq.ones_in(start, len), naive, "start {start} len {len}");
            }
        }
        assert_eq!(seq.ones_in(0, seq.len()), seq.ones());
        assert_eq!(seq.ones_in(seq.len(), 8), 0, "past the end counts nothing");
    }

    /// A chunk that runs past the end reads zeros rather than panicking, so a
    /// mis-sized test parameter fails an assertion instead of aborting the
    /// harness.
    #[test]
    fn reading_past_the_end_yields_zeros() {
        let seq = BitSeq::new(&[0xff]);
        assert_eq!(seq.bit(8), 0);
        assert_eq!(seq.chunk(4, 8), 0b1111_0000);
    }
}
