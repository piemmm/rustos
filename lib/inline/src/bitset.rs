//! Fixed-size 256-bit set.
//!
//! [`BitSet256`] is backed by four 64-bit words and supports the operations
//! required by capability membership and the planned scheduler ready bitmap:
//! constant-time `insert` / `remove` / `contains`, `union`, `intersection`,
//! `difference`, subset comparison, popcount, and ordered iteration.

use core::iter::FusedIterator;

/// Number of bits in a [`BitSet256`].
pub const BITSET256_BITS: usize = 256;

/// Number of `u64` words backing a [`BitSet256`].
const WORDS: usize = BITSET256_BITS / 64;

/// 256-bit fixed-capacity bitset.
///
/// Stores one bit per integer in `0..=255`. The representation is four
/// little-endian `u64` words, lowest-indexed word first; bit `i` lives in
/// word `i / 64` at position `i % 64`. All operations are constant-time and
/// alloc-free.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct BitSet256 {
    words: [u64; WORDS],
}

impl BitSet256 {
    /// An empty set.
    pub const EMPTY: Self = Self { words: [0; WORDS] };

    /// Construct an empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Construct a set populated from raw backing words.
    ///
    /// Useful for deserialising a bitset that was written to disk or sent
    /// across an IPC boundary.
    #[must_use]
    pub const fn from_words(words: [u64; WORDS]) -> Self {
        Self { words }
    }

    /// Expose the raw backing words.
    #[must_use]
    pub const fn as_words(&self) -> &[u64; WORDS] {
        &self.words
    }

    /// Add `bit` to the set.
    ///
    /// `bit` must be in `0..256`; out-of-range bits are ignored so that the
    /// function is total and infallible. Callers that need to detect an
    /// out-of-range identifier should validate it before calling.
    pub fn insert(&mut self, bit: u16) {
        if (bit as usize) < BITSET256_BITS {
            let (w, b) = split(bit);
            self.words[w] |= 1u64 << b;
        }
    }

    /// Remove `bit` from the set.
    ///
    /// Out-of-range bits are ignored; see [`Self::insert`].
    pub fn remove(&mut self, bit: u16) {
        if (bit as usize) < BITSET256_BITS {
            let (w, b) = split(bit);
            self.words[w] &= !(1u64 << b);
        }
    }

    /// Return `true` if `bit` is in the set.
    #[must_use]
    pub const fn contains(&self, bit: u16) -> bool {
        if (bit as usize) >= BITSET256_BITS {
            return false;
        }
        let (w, b) = split(bit);
        (self.words[w] >> b) & 1 == 1
    }

    /// Return `true` if the set has no members.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        let mut i = 0;
        while i < WORDS {
            if self.words[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Count the number of members.
    #[must_use]
    pub const fn len(&self) -> u32 {
        let mut total: u32 = 0;
        let mut i = 0;
        while i < WORDS {
            total += self.words[i].count_ones();
            i += 1;
        }
        total
    }

    /// Bitwise union: every bit in `self` or `other`.
    #[must_use]
    pub const fn union(&self, other: &Self) -> Self {
        let mut out = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            out[i] = self.words[i] | other.words[i];
            i += 1;
        }
        Self { words: out }
    }

    /// Bitwise intersection: only bits in both `self` and `other`.
    #[must_use]
    pub const fn intersection(&self, other: &Self) -> Self {
        let mut out = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            out[i] = self.words[i] & other.words[i];
            i += 1;
        }
        Self { words: out }
    }

    /// Bitwise difference: bits in `self` that are not in `other`.
    #[must_use]
    pub const fn difference(&self, other: &Self) -> Self {
        let mut out = [0u64; WORDS];
        let mut i = 0;
        while i < WORDS {
            out[i] = self.words[i] & !other.words[i];
            i += 1;
        }
        Self { words: out }
    }

    /// `true` if every member of `self` is also a member of `other`.
    #[must_use]
    pub const fn is_subset_of(&self, other: &Self) -> bool {
        let mut i = 0;
        while i < WORDS {
            if (self.words[i] & !other.words[i]) != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Iterate over the bits set in `self`, in ascending order.
    #[must_use]
    pub fn iter(&self) -> BitSet256Iter {
        BitSet256Iter {
            words: self.words,
            word_index: 0,
        }
    }
}

impl IntoIterator for &BitSet256 {
    type Item = u16;
    type IntoIter = BitSet256Iter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

const fn split(bit: u16) -> (usize, u32) {
    let w = (bit as usize) >> 6;
    let b = (bit as u32) & 0x3F;
    (w, b)
}

/// Ascending iterator over the set bits of a [`BitSet256`].
///
/// Yields each member exactly once as a `u16` in `0..256`. The iterator is
/// fused: once exhausted it returns `None` for ever.
#[derive(Clone, Debug)]
pub struct BitSet256Iter {
    words: [u64; WORDS],
    word_index: usize,
}

impl Iterator for BitSet256Iter {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        while self.word_index < WORDS {
            let word = self.words[self.word_index];
            if word == 0 {
                self.word_index += 1;
                continue;
            }
            let lsb = word.trailing_zeros();
            // Clear the bit we are about to yield.
            self.words[self.word_index] = word & (word - 1);
            // `word_index < WORDS` (4) and `lsb < 64`, so the bit number is
            // below `BITSET256_BITS` and fits a `u16`.
            #[allow(clippy::cast_possible_truncation)]
            let bit = (self.word_index as u16) * 64 + (lsb as u16);
            return Some(bit);
        }
        None
    }
}

impl FusedIterator for BitSet256Iter {}

#[cfg(test)]
mod tests {
    use super::{BitSet256, BITSET256_BITS};

    #[test]
    fn empty_set_has_no_members() {
        let set = BitSet256::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert!(!set.contains(0));
        assert!(!set.contains(255));
    }

    #[test]
    fn insert_and_contains_round_trip() {
        let mut set = BitSet256::new();
        set.insert(0);
        set.insert(63);
        set.insert(64);
        set.insert(255);
        assert!(set.contains(0));
        assert!(set.contains(63));
        assert!(set.contains(64));
        assert!(set.contains(255));
        assert!(!set.contains(1));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn out_of_range_bits_are_ignored() {
        let mut set = BitSet256::new();
        // The set's bit count is 256, well inside a `u16` — the point of the
        // test is that the index is out of the set's range, not out of the
        // type's.
        #[allow(clippy::cast_possible_truncation)]
        let oversize = BITSET256_BITS as u16;
        set.insert(oversize);
        set.insert(oversize + 1);
        assert!(set.is_empty());
        assert!(!set.contains(oversize));
    }

    #[test]
    fn remove_clears_only_target_bit() {
        let mut set = BitSet256::new();
        set.insert(10);
        set.insert(11);
        set.remove(10);
        assert!(!set.contains(10));
        assert!(set.contains(11));
    }

    #[test]
    fn union_intersection_difference() {
        let mut a = BitSet256::new();
        a.insert(1);
        a.insert(2);
        a.insert(3);
        let mut b = BitSet256::new();
        b.insert(2);
        b.insert(3);
        b.insert(4);
        assert_eq!(a.union(&b).len(), 4);
        assert_eq!(a.intersection(&b).len(), 2);
        assert!(a.intersection(&b).contains(2));
        assert!(a.intersection(&b).contains(3));
        assert!(a.difference(&b).contains(1));
        assert!(!a.difference(&b).contains(2));
    }

    #[test]
    fn subset_relation() {
        let mut parent = BitSet256::new();
        parent.insert(5);
        parent.insert(6);
        parent.insert(7);
        let mut child = BitSet256::new();
        child.insert(5);
        child.insert(7);
        assert!(child.is_subset_of(&parent));
        assert!(!parent.is_subset_of(&child));
        assert!(BitSet256::new().is_subset_of(&parent));
    }

    #[test]
    fn iterator_is_ascending_and_complete() {
        let mut set = BitSet256::new();
        for bit in [0u16, 1, 63, 64, 65, 128, 200, 255] {
            set.insert(bit);
        }
        let collected: [u16; 8] = {
            let mut iter = set.iter();
            [
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
                iter.next().unwrap(),
            ]
        };
        assert_eq!(collected, [0, 1, 63, 64, 65, 128, 200, 255]);
        assert_eq!(set.iter().count(), 8);
    }

    #[test]
    fn from_words_round_trips() {
        let words = [0xDEAD_BEEFu64, 0, 0, 0x1u64 << 63];
        let set = BitSet256::from_words(words);
        assert_eq!(set.as_words(), &words);
        assert!(set.contains(0)); // 0xDEADBEEF has bit 0 set (lowest bit of 0xF == 1).
        assert!(set.contains(192 + 63));
    }
}
