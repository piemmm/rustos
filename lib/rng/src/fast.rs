//! The fast **unpredictable** generator ([`FastRng`]): buffered `ChaCha12`
//! with fast key erasure.
//!
//! This is the generator to reach for whenever output should be
//! unpredictable but is not long-lived key material: the task-id draw, a
//! network payload, the kernel's userland-facing output reserve. It is
//! Bernstein's fast-key-erasure construction — the same shape as OpenBSD's
//! `arc4random` and Linux's `get_random_u64` — over `lib/crypto`'s audited
//! `ChaCha12`, so no cryptographic primitive is written here.
//!
//! # The construction
//!
//! One refill runs the cipher once under the current key and a fixed nonce,
//! producing [`FAST_REFILL_BYTES`] of keystream. The first
//! [`tairix_crypto::STREAM_KEY_LEN`] bytes *become the key*; the rest fill
//! the issue buffer. **The key that produced a buffer is destroyed before a
//! byte of that buffer is handed out**, and each byte is wiped from the
//! buffer as it is consumed.
//!
//! A constant zero nonce is correct and deliberate: the key is fresh on every
//! refill, so a `(key, nonce)` pair can never repeat. A counter would be
//! extra state buying nothing.
//!
//! # What it guarantees, and what it does not
//!
//! * **Indistinguishable from uniform** at 256-bit security, so an observer
//!   of any amount of output learns nothing about the rest of it.
//! * **Backtracking-resistant**: the key behind already-issued bytes exists
//!   nowhere, so even a full memory capture reveals no past output.
//! * **Deterministic from its seed**, so reproducible fixtures keep working.
//! * **Not prediction-resistant on its own.** Recovering from a state
//!   compromise needs fresh entropy, which this type cannot conjure: an owner
//!   that wants it calls [`FastRng::perturb`] on the cadence
//!   [`FastRng::perturb_due`] reports. A generator whose owner never perturbs
//!   stays forward-secure and stops there.

use zeroize::Zeroize;

use tairix_crypto::{chacha12_keystream, StreamKey, StreamNonce, STREAM_KEY_LEN};

use crate::noncrypto::SplitMix64;
use crate::rand::RandU64;

/// Keystream bytes one refill generates: exactly four 64-byte cipher blocks,
/// so no block is generated and thrown away.
pub const FAST_REFILL_BYTES: usize = 256;

/// Default issue-buffer size, in bytes — one refill's keystream less the
/// bytes that become the next key.
///
/// This is a containment bound, not a capacity: it bounds how much
/// unissued output is resident in memory at once. A kilobyte buffer would
/// amortise the cipher only marginally better for several times the resident
/// exposure.
pub const FAST_BUFFER_BYTES: usize = FAST_REFILL_BYTES - STREAM_KEY_LEN;

/// Bytes a generator may issue under one key before [`FastRng::perturb_due`]
/// asks its owner for fresh entropy.
///
/// Counted in bytes issued rather than refills, so the cadence does not
/// change with the buffer size. It bounds the output attributable to a single
/// key, which is what bounds how long a state compromise stays exploitable.
pub const PERTURB_INTERVAL_BYTES: u64 = 1 << 20;

/// The fixed nonce every refill uses. Sound because the key is fresh each
/// time, so no `(key, nonce)` pair recurs.
const FAST_NONCE: StreamNonce = [0u8; tairix_crypto::STREAM_NONCE_LEN];

/// A fast, unpredictable, backtracking-resistant generator.
///
/// `N` is the issue-buffer size in bytes; [`FAST_BUFFER_BYTES`] unless a
/// consumer has a reason to differ (the kernel output reserve holds a larger
/// one). See the module docs for the construction and its guarantees.
pub struct FastRng<const N: usize = FAST_BUFFER_BYTES> {
    key: StreamKey,
    /// Issued output. `buffer[..cursor]` has been consumed and wiped;
    /// `buffer[cursor..]` is live.
    buffer: [u8; N],
    cursor: usize,
    bytes_since_perturb: u64,
}

impl<const N: usize> FastRng<N> {
    /// Build a generator from a 256-bit key.
    ///
    /// The key is stored and nothing is generated yet, so this is `const` and
    /// a consumer can hold a generator in a `static`. Draw the key from
    /// [`crate::CsRng`] ([`crate::CsRng::fork_fast`]) or the kernel's random
    /// syscall for an unpredictable one.
    #[must_use]
    pub const fn from_key(key: &StreamKey) -> Self {
        const {
            assert!(N > 0, "a FastRng issue buffer must be non-empty");
        }
        Self {
            key: *key,
            buffer: [0u8; N],
            // Empty: the first draw refills.
            cursor: N,
            bytes_since_perturb: 0,
        }
    }

    /// Build a generator from a single `u64`, expanded to a 256-bit key with
    /// `SplitMix64`.
    ///
    /// For deterministic fixtures and for a `static` that has no key until
    /// the boot path supplies one. A `u64` carries 64 bits of entropy however
    /// wide the key it fills, so this is not a way to seed an unpredictable
    /// generator — use [`FastRng::from_key`] for that.
    #[must_use]
    pub const fn seed_from_u64(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        let mut key = [0u8; STREAM_KEY_LEN];
        let mut i = 0;
        while i < STREAM_KEY_LEN {
            let word = sm.next().to_le_bytes();
            let mut j = 0;
            while j < 8 {
                key[i + j] = word[j];
                j += 1;
            }
            i += 8;
        }
        Self::from_key(&key)
    }

    /// Run the cipher once: the head of the keystream replaces the key, the
    /// rest becomes the issue buffer.
    fn refill(&mut self) {
        let mut next_key = [0u8; STREAM_KEY_LEN];
        chacha12_keystream(&self.key, &FAST_NONCE, &mut next_key, &mut self.buffer);
        // Destroy the key that produced this buffer before any of it is
        // issued: that is what makes already-issued output unrecoverable.
        self.key = next_key;
        next_key.zeroize();
        self.cursor = 0;
    }

    /// Serve `out` from the buffer, refilling as often as needed and wiping
    /// each byte as it is handed over.
    ///
    /// The one place output leaves this type, so the zeroise-on-consume and
    /// perturbation accounting have a single definition.
    fn take(&mut self, out: &mut [u8]) {
        let mut written = 0;
        while written < out.len() {
            if self.cursor == N {
                self.refill();
            }
            let take = core::cmp::min(out.len() - written, N - self.cursor);
            let end = self.cursor + take;
            out[written..written + take].copy_from_slice(&self.buffer[self.cursor..end]);
            self.buffer[self.cursor..end].zeroize();
            self.cursor = end;
            written += take;
        }
        self.bytes_since_perturb = self
            .bytes_since_perturb
            .saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX));
    }

    /// Whether this generator has issued [`PERTURB_INTERVAL_BYTES`] under its
    /// current key and is due fresh entropy.
    ///
    /// Advisory: the generator keeps producing secure output either way. An
    /// owner that can supply entropy checks this and calls
    /// [`FastRng::perturb`]; one that cannot simply forgoes prediction
    /// resistance, which is a property no generator can manufacture alone.
    #[must_use]
    pub const fn perturb_due(&self) -> bool {
        self.bytes_since_perturb >= PERTURB_INTERVAL_BYTES
    }

    /// Fold 32 fresh entropy bytes into the key and discard buffered output.
    ///
    /// XOR is the whole point: a dead, stuck, or hostile source contributes
    /// zeros or garbage and can never *lower* the key's quality below what it
    /// already was. Buffered output is dropped because it belongs to the old
    /// key.
    pub fn perturb(&mut self, fresh: &StreamKey) {
        for (byte, patch) in self.key.iter_mut().zip(fresh) {
            *byte ^= *patch;
        }
        self.buffer.zeroize();
        self.cursor = N;
        self.bytes_since_perturb = 0;
    }

    /// Destroy buffered output **and** the key that produced it.
    ///
    /// One erasure step whose output is thrown away, so neither the bytes a
    /// snapshot could replay nor the key that would regenerate them survives.
    /// Rotating the key unconditionally is what keeps two copies of a
    /// generator — a suspend image, a cloned task — from continuing the same
    /// stream.
    pub fn discard(&mut self) {
        self.refill();
        self.buffer.zeroize();
        self.cursor = N;
    }

    /// Buffered, not-yet-consumed bytes. For introspection and tests.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        N - self.cursor
    }
}

impl<const N: usize> RandU64 for FastRng<N> {
    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.take(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    /// Copies straight out of the buffer instead of reassembling `u64`s, so
    /// bulk consumers (the kernel output reserve) pay one memcpy per refill.
    fn fill_bytes(&mut self, out: &mut [u8]) {
        self.take(out);
    }
}

impl<const N: usize> core::fmt::Debug for FastRng<N> {
    /// Reveals neither the key nor the buffer: both would hand an observer
    /// the stream.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FastRng")
            .field("capacity", &N)
            .field("buffered", &self.buffered())
            .finish_non_exhaustive()
    }
}

impl<const N: usize> Drop for FastRng<N> {
    fn drop(&mut self) {
        self.key.zeroize();
        self.buffer.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FastRng, FAST_BUFFER_BYTES, FAST_REFILL_BYTES, PERTURB_INTERVAL_BYTES, STREAM_KEY_LEN,
    };
    use crate::rand::RandU64;

    const KEY: [u8; STREAM_KEY_LEN] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];

    /// The refill run is exactly whole cipher blocks, so none is generated
    /// and discarded.
    #[test]
    fn the_refill_run_is_block_aligned() {
        assert_eq!(FAST_REFILL_BYTES % 64, 0);
        assert_eq!(FAST_BUFFER_BYTES + STREAM_KEY_LEN, FAST_REFILL_BYTES);
    }

    /// A generator can be built in a `static`, which is what the kernel's
    /// process-wide task-id generator needs.
    #[test]
    fn construction_is_const() {
        static FROM_SEED: FastRng = FastRng::seed_from_u64(1);
        static FROM_KEY: FastRng = FastRng::from_key(&KEY);
        assert_eq!(FROM_SEED.buffered(), 0);
        assert_eq!(FROM_KEY.buffered(), 0);
    }

    /// The load-bearing structural test: the first bytes a generator issues
    /// must be keystream bytes `32..` of its seed key's run, and its key must
    /// have become bytes `..32`. Pins the buffer split, the key derivation,
    /// the nonce, and the byte order all at once.
    #[test]
    fn the_first_issued_bytes_are_the_keystream_past_the_new_key() {
        let mut expected_key = [0u8; STREAM_KEY_LEN];
        let mut expected_body = [0u8; FAST_BUFFER_BYTES];
        tairix_crypto::chacha12_keystream(
            &KEY,
            &[0u8; tairix_crypto::STREAM_NONCE_LEN],
            &mut expected_key,
            &mut expected_body,
        );

        let mut g = FastRng::<FAST_BUFFER_BYTES>::from_key(&KEY);
        let mut issued = [0u8; FAST_BUFFER_BYTES];
        g.fill_bytes(&mut issued);
        assert_eq!(issued, expected_body);
        assert_eq!(g.key, expected_key);
    }

    #[test]
    fn the_same_seed_reproduces_the_same_stream() {
        let mut a = FastRng::<64>::seed_from_u64(7);
        let mut b = FastRng::<64>::seed_from_u64(7);
        let mut c = FastRng::<64>::seed_from_u64(8);
        let (mut oa, mut ob, mut oc) = ([0u8; 300], [0u8; 300], [0u8; 300]);
        a.fill_bytes(&mut oa);
        b.fill_bytes(&mut ob);
        c.fill_bytes(&mut oc);
        assert_eq!(oa, ob, "same seed must give the same stream");
        assert_ne!(oa, oc, "a different seed must give a different stream");
    }

    /// A draw that spans several refills must be one continuous stream, not a
    /// repeat of the first buffer.
    #[test]
    fn a_draw_larger_than_the_buffer_crosses_refills_continuously() {
        let mut bulk = FastRng::<64>::from_key(&KEY);
        let mut whole = [0u8; 256];
        bulk.fill_bytes(&mut whole);

        let mut piecemeal = FastRng::<64>::from_key(&KEY);
        let mut pieces = [0u8; 256];
        for chunk in pieces.chunks_mut(7) {
            piecemeal.fill_bytes(chunk);
        }
        assert_eq!(whole, pieces, "chunking must not change the stream");
        // Four distinct buffers, so no 64-byte window repeats.
        for (i, a) in whole.chunks(64).enumerate() {
            for b in whole.chunks(64).skip(i + 1) {
                assert_ne!(a, b, "a refill repeated its predecessor's output");
            }
        }
    }

    #[test]
    fn consumed_bytes_are_wiped_from_the_buffer() {
        let mut g = FastRng::<64>::from_key(&KEY);
        let mut out = [0u8; 20];
        g.fill_bytes(&mut out);
        assert_eq!(g.buffered(), 44);
        assert!(
            g.buffer[..20].iter().all(|&b| b == 0),
            "consumed bytes must be wiped"
        );
        assert!(
            g.buffer[20..].iter().any(|&b| b != 0),
            "live bytes must survive"
        );
    }

    /// Backtracking resistance: once a buffer has been issued, the key that
    /// produced it is gone and no state the generator still holds can
    /// reproduce it.
    #[test]
    fn a_captured_state_cannot_reproduce_already_issued_output() {
        let mut g = FastRng::<64>::from_key(&KEY);
        let mut first = [0u8; 64];
        g.fill_bytes(&mut first);
        // One more byte forces the next refill, so the generator's state is
        // fully past the buffer above.
        let mut probe = [0u8; 1];
        g.fill_bytes(&mut probe);
        assert_ne!(g.key, KEY, "the key that produced the buffer must be gone");

        // Driving the surviving state forward never re-emits that buffer.
        let mut ahead = [0u8; 4096];
        g.fill_bytes(&mut ahead);
        assert!(
            !ahead.windows(64).any(|w| w == first),
            "issued output reappeared from the post-capture state"
        );
    }

    #[test]
    fn perturbation_diverges_two_otherwise_identical_generators() {
        let mut plain = FastRng::<64>::from_key(&KEY);
        let mut perturbed = FastRng::<64>::from_key(&KEY);
        perturbed.perturb(&[0x5a; STREAM_KEY_LEN]);
        let (mut a, mut b) = ([0u8; 128], [0u8; 128]);
        plain.fill_bytes(&mut a);
        perturbed.fill_bytes(&mut b);
        assert_ne!(a, b);
    }

    /// XOR-folding means a source that supplies nothing cannot damage the
    /// key: the generator keeps its key and keeps producing.
    #[test]
    fn a_zero_source_cannot_degrade_the_key() {
        let mut g = FastRng::<64>::from_key(&KEY);
        g.perturb(&[0u8; STREAM_KEY_LEN]);
        assert_eq!(g.key, KEY, "an all-zero fold must leave the key alone");
        let mut out = [0u8; 64];
        g.fill_bytes(&mut out);
        assert_ne!(out, [0u8; 64], "the stream must still be live");
    }

    #[test]
    fn perturbation_discards_buffered_output_and_resets_the_cadence() {
        let mut g = FastRng::<64>::from_key(&KEY);
        let mut out = [0u8; 8];
        g.fill_bytes(&mut out);
        assert!(g.buffered() > 0);
        g.perturb(&[1u8; STREAM_KEY_LEN]);
        assert_eq!(g.buffered(), 0, "old-key output must not survive");
        assert_eq!(g.buffer, [0u8; 64], "discarded output must be wiped");
        assert!(!g.perturb_due());
    }

    #[test]
    fn the_perturbation_cadence_counts_bytes_issued() {
        let mut g = FastRng::<64>::from_key(&KEY);
        assert!(!g.perturb_due());
        let mut sink = [0u8; 4096];
        let mut issued = 0u64;
        while issued < PERTURB_INTERVAL_BYTES {
            g.fill_bytes(&mut sink);
            issued += u64::try_from(sink.len()).expect("a chunk length fits a u64");
            assert_eq!(g.perturb_due(), issued >= PERTURB_INTERVAL_BYTES);
        }
        g.perturb(&[2u8; STREAM_KEY_LEN]);
        assert!(!g.perturb_due());
    }

    /// A discard must move the key on even with nothing buffered, or a
    /// suspend image and its original would continue one stream.
    #[test]
    fn discard_rotates_the_key_and_drops_the_buffer() {
        let mut original = FastRng::<64>::from_key(&KEY);
        let mut clone = FastRng::<64>::from_key(&KEY);
        clone.discard();
        assert_eq!(clone.buffered(), 0);
        assert_eq!(clone.buffer, [0u8; 64]);
        assert_ne!(clone.key, KEY);
        let (mut a, mut b) = ([0u8; 128], [0u8; 128]);
        original.fill_bytes(&mut a);
        clone.fill_bytes(&mut b);
        assert_ne!(a, b, "a discarded copy must not continue the same stream");

        // A discard with live buffered output drops it rather than serving
        // it later: what an undiscarded generator would have served next
        // must not appear.
        let mut discarded = FastRng::<64>::from_key(&KEY);
        let mut reference = FastRng::<64>::from_key(&KEY);
        let mut head = [0u8; 8];
        discarded.fill_bytes(&mut head);
        reference.fill_bytes(&mut head);
        let mut would_have_been_next = [0u8; 56];
        reference.fill_bytes(&mut would_have_been_next);
        discarded.discard();
        let mut after = [0u8; 56];
        discarded.fill_bytes(&mut after);
        assert_ne!(
            after, would_have_been_next,
            "discarded output was served after all"
        );
    }

    #[test]
    fn debug_reveals_neither_key_nor_buffer() {
        extern crate alloc;
        use alloc::format;
        let mut g = FastRng::<64>::from_key(&KEY);
        let mut out = [0u8; 8];
        g.fill_bytes(&mut out);
        // Exactly the two size fields, so no key or output byte can be in
        // there at all.
        assert_eq!(
            format!("{g:?}"),
            "FastRng { capacity: 64, buffered: 56, .. }"
        );
    }
}
