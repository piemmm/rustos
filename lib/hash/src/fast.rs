//! A fast, non-cryptographic hash (XXH64).
//!
//! [`FastHash`] is XXH64 (Yann Collet), a published, widely-deployed
//! non-cryptographic hash with strong avalanche and word-at-a-time
//! throughput. It is **not keyed**: its seed is a distinguisher, not a
//! secret, and an attacker who can choose inputs can find collisions. Use it
//! for kernel-assigned keys, content fingerprints, and revision counters —
//! never for a key an attacker can influence, where [`crate::SipHash13`] is
//! the only correct choice.
//!
//! Rolling it here rather than taking a dependency is the same call
//! `lib/rng` makes for xoshiro256++: an ordinary, well-studied algorithm is
//! not a security primitive, and the implementation is pinned by the
//! reference implementation's published outputs.

use core::hash::Hasher;

/// XXH64's five round constants, fixed by the algorithm.
const PRIME1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME5: u64 = 0x27D4_EB2F_1656_67C5;

/// Bytes per stripe: four 64-bit lanes, one per accumulator.
const STRIPE: usize = 32;

/// A fast, non-cryptographic XXH64 hasher.
///
/// Feed it through [`Hasher`], or take the one-shot [`FastHash::hash_bytes`]
/// when the whole input is one slice. Integer writes are little-endian and
/// pointer-sized values are widened to 64 bits, so a value hashes identically
/// on every port.
#[derive(Copy, Clone, Debug)]
pub struct FastHash {
    seed: u64,
    acc: [u64; 4],
    /// Bytes not yet forming a whole stripe.
    buf: [u8; STRIPE],
    /// How many of `buf`'s bytes are live (`0..STRIPE`).
    buffered: usize,
    /// Total bytes written, which the digest mixes in.
    total: u64,
}

impl FastHash {
    /// An unseeded hasher — the right form for a fingerprint or a revision
    /// counter, where nothing distinguishes one stream from another.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_seed(0)
    }

    /// A hasher whose stream is distinguished by `seed`.
    #[must_use]
    pub const fn with_seed(seed: u64) -> Self {
        Self {
            seed,
            acc: lanes(seed),
            buf: [0; STRIPE],
            buffered: 0,
            total: 0,
        }
    }

    /// Hash one slice under `seed`.
    #[must_use]
    pub fn hash_bytes(seed: u64, bytes: &[u8]) -> u64 {
        let (stripes, tail) = bytes.as_chunks::<STRIPE>();
        let mut acc = lanes(seed);
        for stripe in stripes {
            absorb(&mut acc, stripe);
        }
        let converged = if stripes.is_empty() {
            seed.wrapping_add(PRIME5)
        } else {
            converge(acc)
        };
        let with_len = converged.wrapping_add(bytes.len() as u64);
        avalanche(consume_tail(with_len, tail))
    }
}

impl Default for FastHash {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher for FastHash {
    fn write(&mut self, msg: &[u8]) {
        self.total = self.total.wrapping_add(msg.len() as u64);

        // Complete a partial stripe left by an earlier write before consuming
        // whole ones, so a hash is independent of how the caller chunked it.
        let mut input = msg;
        if self.buffered != 0 {
            let take = (STRIPE - self.buffered).min(input.len());
            for (dst, &src) in self
                .buf
                .iter_mut()
                .skip(self.buffered)
                .zip(input.iter().take(take))
            {
                *dst = src;
            }
            self.buffered += take;
            input = input.get(take..).unwrap_or(&[]);
            if self.buffered < STRIPE {
                return;
            }
            let stripe = self.buf;
            absorb(&mut self.acc, &stripe);
            self.buffered = 0;
        }

        let (stripes, rest) = input.as_chunks::<STRIPE>();
        for stripe in stripes {
            absorb(&mut self.acc, stripe);
        }
        for (dst, &src) in self.buf.iter_mut().zip(rest.iter()) {
            *dst = src;
        }
        self.buffered = rest.len();
    }

    fn write_u8(&mut self, n: u8) {
        self.write(&n.to_le_bytes());
    }

    fn write_u16(&mut self, n: u16) {
        self.write(&n.to_le_bytes());
    }

    fn write_u32(&mut self, n: u32) {
        self.write(&n.to_le_bytes());
    }

    fn write_u64(&mut self, n: u64) {
        self.write(&n.to_le_bytes());
    }

    fn write_u128(&mut self, n: u128) {
        self.write(&n.to_le_bytes());
    }

    /// Widened to 64 bits so a `usize` key hashes identically on a 32-bit
    /// port and a 64-bit one.
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn write_i8(&mut self, n: i8) {
        self.write(&n.to_le_bytes());
    }

    fn write_i16(&mut self, n: i16) {
        self.write(&n.to_le_bytes());
    }

    fn write_i32(&mut self, n: i32) {
        self.write(&n.to_le_bytes());
    }

    fn write_i64(&mut self, n: i64) {
        self.write(&n.to_le_bytes());
    }

    fn write_i128(&mut self, n: i128) {
        self.write(&n.to_le_bytes());
    }

    /// Widened as [`Hasher::write_usize`] is, for the same reason.
    fn write_isize(&mut self, n: isize) {
        self.write_i64(n as i64);
    }

    fn finish(&self) -> u64 {
        let converged = if self.total >= STRIPE as u64 {
            converge(self.acc)
        } else {
            self.seed.wrapping_add(PRIME5)
        };
        let with_len = converged.wrapping_add(self.total);
        let tail = self.buf.get(..self.buffered).unwrap_or(&[]);
        avalanche(consume_tail(with_len, tail))
    }
}

/// The four accumulators' starting values for `seed`.
const fn lanes(seed: u64) -> [u64; 4] {
    [
        seed.wrapping_add(PRIME1).wrapping_add(PRIME2),
        seed.wrapping_add(PRIME2),
        seed,
        seed.wrapping_sub(PRIME1),
    ]
}

/// One accumulator's update from one 64-bit lane.
fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(PRIME2))
        .rotate_left(31)
        .wrapping_mul(PRIME1)
}

/// Fold one accumulator into the converged digest.
fn merge(acc: u64, lane: u64) -> u64 {
    (acc ^ round(0, lane))
        .wrapping_mul(PRIME1)
        .wrapping_add(PRIME4)
}

/// Absorb one whole stripe into the four accumulators.
fn absorb(acc: &mut [u64; 4], stripe: &[u8; STRIPE]) {
    let (words, _) = stripe.as_chunks::<8>();
    for (lane, bytes) in acc.iter_mut().zip(words) {
        *lane = round(*lane, u64::from_le_bytes(*bytes));
    }
}

/// Collapse the four accumulators into one digest.
fn converge(acc: [u64; 4]) -> u64 {
    let mut digest = acc[0]
        .rotate_left(1)
        .wrapping_add(acc[1].rotate_left(7))
        .wrapping_add(acc[2].rotate_left(12))
        .wrapping_add(acc[3].rotate_left(18));
    for lane in acc {
        digest = merge(digest, lane);
    }
    digest
}

/// Consume the trailing bytes that never formed a stripe: whole words, then
/// one half-word, then single bytes.
fn consume_tail(mut acc: u64, tail: &[u8]) -> u64 {
    let (words, rest) = tail.as_chunks::<8>();
    for bytes in words {
        let lane = u64::from_le_bytes(*bytes);
        acc = (acc ^ round(0, lane))
            .rotate_left(27)
            .wrapping_mul(PRIME1)
            .wrapping_add(PRIME4);
    }
    // `rest` is under eight bytes, so this is the algorithm's single
    // half-word step, then its byte steps.
    let (halves, singles) = rest.as_chunks::<4>();
    for bytes in halves {
        let half = u64::from(u32::from_le_bytes(*bytes));
        acc = (acc ^ half.wrapping_mul(PRIME1))
            .rotate_left(23)
            .wrapping_mul(PRIME2)
            .wrapping_add(PRIME3);
    }
    for &byte in singles {
        acc = (acc ^ u64::from(byte).wrapping_mul(PRIME5))
            .rotate_left(11)
            .wrapping_mul(PRIME1);
    }
    acc
}

/// The final bit mix.
fn avalanche(mut digest: u64) -> u64 {
    digest ^= digest >> 33;
    digest = digest.wrapping_mul(PRIME2);
    digest ^= digest >> 29;
    digest = digest.wrapping_mul(PRIME3);
    digest ^= digest >> 32;
    digest
}

#[cfg(test)]
#[path = "fast_tests.rs"]
mod tests;
