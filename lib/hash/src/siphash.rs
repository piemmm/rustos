//! Keyed SipHash-1-3 — the collision-flooding defence.
//!
//! `SipHash` (Aumasson & Bernstein, 2012) is a keyed pseudo-random function
//! built for short inputs, designed for exactly this job: without the key an
//! attacker cannot find inputs that collide, so a hash table over
//! attacker-chosen keys keeps its expected O(1) behaviour. The 1-3 round
//! reduction is the variant chosen for hash-table use — the same one Rust's
//! own default hasher uses — and is pinned here by the published reference
//! vectors.
//!
//! It is not a message-authentication code: a MAC, a digest, or a key
//! derivation is `lib/crypto`'s.

use core::fmt;
use core::hash::Hasher;

use crate::seed::HashSeed;

/// A keyed SipHash-1-3 hasher.
///
/// Feed it through [`Hasher`], or take the one-shot [`SipHash13::hash_bytes`]
/// when the whole input is one slice. Integer writes are little-endian and
/// pointer-sized values are widened to 64 bits, so a value hashes identically
/// on every port.
///
/// [`fmt::Debug`] redacts the state: its initial value is the key combined
/// with public constants, so printing it would hand the key to anyone reading
/// the log.
#[derive(Copy, Clone)]
pub struct SipHash13 {
    v0: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    /// Bytes not yet forming a whole 64-bit word, packed little-endian.
    tail: u64,
    /// How many of `tail`'s bytes are live (`0..8`).
    ntail: usize,
    /// Total bytes written; only its low eight bits reach the final block.
    length: u64,
}

impl SipHash13 {
    /// A hasher keyed with `seed`.
    #[must_use]
    pub const fn new(seed: HashSeed) -> Self {
        let (k0, k1) = seed.words();
        Self {
            v0: k0 ^ 0x736f_6d65_7073_6575,
            v1: k1 ^ 0x646f_7261_6e64_6f6d,
            v2: k0 ^ 0x6c79_6765_6e65_7261,
            v3: k1 ^ 0x7465_6462_7974_6573,
            tail: 0,
            ntail: 0,
            length: 0,
        }
    }

    /// Hash one slice under `seed`.
    #[must_use]
    pub fn hash_bytes(seed: HashSeed, bytes: &[u8]) -> u64 {
        let mut hasher = Self::new(seed);
        hasher.write(bytes);
        hasher.finish()
    }

    /// One `SipRound`.
    fn round(&mut self) {
        self.v0 = self.v0.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(13);
        self.v1 ^= self.v0;
        self.v0 = self.v0.rotate_left(32);

        self.v2 = self.v2.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(16);
        self.v3 ^= self.v2;

        self.v0 = self.v0.wrapping_add(self.v3);
        self.v3 = self.v3.rotate_left(21);
        self.v3 ^= self.v0;

        self.v2 = self.v2.wrapping_add(self.v1);
        self.v1 = self.v1.rotate_left(17);
        self.v1 ^= self.v2;
        self.v2 = self.v2.rotate_left(32);
    }

    /// Absorb one whole 64-bit message word (the single compression round).
    fn absorb(&mut self, word: u64) {
        self.v3 ^= word;
        self.round();
        self.v0 ^= word;
    }
}

impl fmt::Debug for SipHash13 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SipHash13(<redacted>)")
    }
}

impl Hasher for SipHash13 {
    fn write(&mut self, msg: &[u8]) {
        self.length = self.length.wrapping_add(msg.len() as u64);

        // Complete a partial word left by an earlier write before consuming
        // whole words, so a hash is independent of how the caller chunked it.
        let mut consumed = 0;
        if self.ntail != 0 {
            let needed = 8 - self.ntail;
            self.tail |= pack_le(msg, needed.min(msg.len())) << (8 * self.ntail);
            if msg.len() < needed {
                self.ntail += msg.len();
                return;
            }
            let word = self.tail;
            self.absorb(word);
            self.tail = 0;
            self.ntail = 0;
            consumed = needed;
        }

        let rest = msg.get(consumed..).unwrap_or(&[]);
        let (words, left) = rest.as_chunks::<8>();
        for word in words {
            self.absorb(u64::from_le_bytes(*word));
        }
        self.tail = pack_le(left, left.len());
        self.ntail = left.len();
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
        let mut state = *self;
        let final_word = ((self.length & 0xff) << 56) | self.tail;
        state.absorb(final_word);
        state.v2 ^= 0xff;
        state.round();
        state.round();
        state.round();
        state.v0 ^ state.v1 ^ state.v2 ^ state.v3
    }
}

/// Pack the first `len` bytes of `bytes` into the low bytes of a word,
/// little-endian. `len` is at most eight; a shorter slice contributes what it
/// has and the remaining bytes stay zero.
fn pack_le(bytes: &[u8], len: usize) -> u64 {
    let mut word = 0u64;
    for (i, &byte) in bytes.iter().take(len.min(8)).enumerate() {
        word |= u64::from(byte) << (8 * i);
    }
    word
}

#[cfg(test)]
#[path = "siphash_tests.rs"]
mod tests;
