//! Canonical prefix-code assignment, shared by every format here whose
//! entropy coding is one.
//!
//! A JPEG Huffman table and a WEBP lossless stream's prefix codes are the
//! same construction: codes of one length are consecutive, assigned in
//! symbol order, and each length's first code is the previous length's last
//! plus one, doubled. Only what surrounds it differs — the bit source, the
//! alphabet's width, whether an incomplete code is legal, and the refusal
//! each format names — so the assignment and the walk that decodes one live
//! here, and the rest stays with the format.
//!
//! A table is its code-length counts and nothing else, because a lossless
//! WEBP holds one prefix code per channel per meta-Huffman group and a file
//! may declare tens of thousands of groups: at that count a table of
//! precomputed per-length values would cost hundreds of times the bytes the
//! stream declaring them occupies. [`Walk`] recovers each length's first
//! code and first symbol as it consumes bits, which is the same arithmetic
//! the assignment performed and costs a few operations per bit.

/// The longest prefix code any format here defines, which is JPEG's limit.
/// A WEBP lossless code never exceeds fifteen bits; holding sixteen does not
/// permit one, since the caller supplies its own length bound.
pub(crate) const MAX_CODE_BITS: usize = 16;

/// The codes of one length: where they start and which symbols they select.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Run {
    /// The first code value of this length.
    pub(crate) first_code: u32,
    /// How many codes this length holds.
    pub(crate) count: u32,
    /// The index, in the caller's code-ordered symbol list, that
    /// [`Self::first_code`] selects.
    pub(crate) first_symbol: u32,
}

/// A canonical prefix code, held as the code-length counts it was built
/// from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Canonical {
    /// How many codes have each length, `counts[0]` being length one.
    counts: [u16; MAX_CODE_BITS],
    /// Whether the assignment spends the whole code space. An incomplete
    /// code is a refusal in some formats and legal in others, so this is
    /// reported rather than enforced.
    complete: bool,
}

impl Canonical {
    /// Assign a canonical code to every symbol from `counts` — the number
    /// of codes of each length, `counts[0]` being length one — or `None`
    /// where the counts oversubscribe the code space, or hold more codes of
    /// one length than a 16-bit count, and so describe no assignment at all.
    ///
    /// `counts` may be no longer than [`MAX_CODE_BITS`].
    pub(crate) fn build(counts: &[u32]) -> Option<Self> {
        if counts.len() > MAX_CODE_BITS {
            return None;
        }
        // The share of the code space still unassigned, in units of the
        // shortest code: doubled per length, spent by the codes of it.
        let mut left = 1i64;
        let mut held = [0u16; MAX_CODE_BITS];
        for (slot, &count) in held.iter_mut().zip(counts) {
            left = left.checked_mul(2)?.checked_sub(i64::from(count))?;
            if left < 0 {
                return None;
            }
            *slot = u16::try_from(count).ok()?;
        }
        Some(Self {
            counts: held,
            complete: left == 0,
        })
    }

    /// A code holding exactly one symbol, whose only symbol index is zero.
    ///
    /// Such a code spends half the code space, so it is never
    /// [`Self::complete`]; a format that emits no bits at all for it reads
    /// this rather than walking.
    pub(crate) fn lone_symbol(&self) -> bool {
        self.counts
            .iter()
            .map(|&count| u32::from(count))
            .sum::<u32>()
            == 1
    }

    /// Whether the assignment spends the whole code space.
    pub(crate) const fn complete(&self) -> bool {
        self.complete
    }

    /// How many symbols the assignment covers.
    pub(crate) fn symbols(&self) -> u32 {
        self.counts.iter().map(|&count| u32::from(count)).sum()
    }

    /// The codes of `len` bits, or `None` where none has that length.
    ///
    /// Derived by replaying the assignment, so this is for a build-time
    /// pass rather than a per-symbol decode; [`Self::walk`] is that.
    pub(crate) fn run(&self, len: usize) -> Option<Run> {
        let mut walk = Walk::new();
        for (index, &count) in self.counts.iter().enumerate() {
            let count = u32::from(count);
            if index + 1 == len {
                return (count != 0).then_some(Run {
                    first_code: walk.first,
                    count,
                    first_symbol: walk.base,
                });
            }
            walk.next_length(count);
        }
        None
    }

    /// Begin decoding one code.
    pub(crate) const fn walk() -> Walk {
        Walk::new()
    }

    /// How many codes have length `len`.
    fn count(&self, len: usize) -> u32 {
        len.checked_sub(1)
            .and_then(|index| self.counts.get(index))
            .map_or(0, |&count| u32::from(count))
    }
}

/// One code being decoded, bit by bit.
///
/// The first bit taken is the code's most significant, which is how both
/// formats here pack a prefix code: JPEG's bit stream is most-significant
/// first outright, and a lossless WEBP's least-significant-first stream
/// carries each code's most significant bit first within it.
pub(crate) struct Walk {
    /// The code value assembled so far.
    value: u32,
    /// The first code value of the length reached.
    first: u32,
    /// How many symbols the shorter lengths covered.
    base: u32,
    /// How many bits have been taken.
    len: usize,
}

impl Walk {
    const fn new() -> Self {
        Self {
            value: 0,
            first: 0,
            base: 0,
            len: 0,
        }
    }

    /// How many bits have been taken.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Take one more bit, answering the symbol index once a code completes.
    ///
    /// A `None` means the code is longer than the bits taken so far; the
    /// caller stops at its own length bound.
    pub(crate) fn push(&mut self, table: &Canonical, bit: u32) -> Option<u32> {
        self.value = (self.value << 1) | (bit & 1);
        self.len += 1;
        let count = table.count(self.len);
        let offset = self.value.wrapping_sub(self.first);
        if offset < count {
            return Some(self.base + offset);
        }
        self.next_length(count);
        None
    }

    /// Move to the next length, which is where the assignment doubles.
    fn next_length(&mut self, count: u32) {
        self.base = self.base.wrapping_add(count);
        self.first = self.first.wrapping_add(count) << 1;
    }
}

#[cfg(test)]
#[path = "huffman_tests.rs"]
mod tests;
