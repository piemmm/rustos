//! Shared sealing primitives for the kernel's encrypted memory tiers.
//!
//! Two subsystems seal pages before their bytes leave the caller's
//! control: the encrypted block-swap layer ([`crate::swap`]) and the
//! encrypted compressed anonymous-memory tier ([`crate::ramzip`],
//! `plans/SWAPSWAPSWAP.md`). Both need the same three primitives — an
//! injected entropy seam, an ephemeral per-boot AEAD key that is zeroed
//! on drop and never persisted, and a never-repeating nonce sequence —
//! so those primitives have exactly one definition here. The two tiers
//! each hold their **own** [`SealKey`] and [`NonceSequence`]: the plan
//! forbids either tier from relying on the other's key or metadata
//! format, and nothing here allows key material to be shared, copied,
//! or serialised.
//!
//! # Nonce discipline
//!
//! ChaCha20-Poly1305 fails catastrophically on `(key, nonce)` reuse. A
//! [`NonceSequence`] draws a random 32-bit salt at construction and
//! appends a 64-bit monotonic counter, giving a 96-bit nonce that
//! cannot repeat for the life of the (per-boot, unique) key it is
//! paired with. Counter exhaustion fails closed
//! ([`SealError::NonceExhausted`]) rather than wrapping.

use tairix_crypto::aead::{AeadKey, AeadNonce, AEAD_KEY_LEN, AEAD_NONCE_LEN};
use zeroize::Zeroize;

/// Reason a sealing primitive could not be constructed or advanced.
///
/// Both variants are fail-closed outcomes: no key, salt, or nonce is
/// produced on the error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealError {
    /// The injected entropy source could not supply random bytes.
    Entropy,
    /// The per-boot nonce counter is exhausted; no further payloads can
    /// be sealed under this key without risking nonce reuse.
    NonceExhausted,
}

impl core::fmt::Display for SealError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::Entropy => "platform entropy source failed",
            Self::NonceExhausted => "nonce counter exhausted for this boot key",
        };
        f.write_str(msg)
    }
}

/// Source of cryptographic randomness for keys and nonce salts.
///
/// This is the seam for the platform RNG. The kernel injects a concrete
/// implementation (mirroring the seam pattern used by `init`'s
/// `Spawner` and `login`'s `Authenticator`); the sealing layers never
/// reach for a global RNG.
pub trait EntropySource {
    /// Fill `out` with cryptographically random bytes.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::Entropy`] if randomness is unavailable. The
    /// sealing layers fail closed: no key or nonce salt is derived from
    /// a failed draw.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError>;
}

/// An ephemeral per-boot sealing key.
///
/// Zeroed on drop; never persisted, cloned, or copied out of the crate.
/// A power cycle destroys the key, so sealed bytes are unrecoverable at
/// rest.
pub struct SealKey {
    bytes: AeadKey,
}

impl SealKey {
    /// Draw a fresh random key from `entropy`.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::Entropy`] if the source cannot supply
    /// randomness; no key is constructed in that case.
    pub fn generate(entropy: &mut dyn EntropySource) -> Result<Self, SealError> {
        let mut bytes: AeadKey = [0u8; AEAD_KEY_LEN];
        entropy.fill(&mut bytes)?;
        Ok(Self { bytes })
    }

    /// Lend the key bytes to an in-crate cipher. Crate-private on
    /// purpose: the key never leaves `kernel/mem`.
    pub(crate) fn material(&self) -> &AeadKey {
        &self.bytes
    }
}

impl core::fmt::Debug for SealKey {
    /// Never reveals the key bytes.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SealKey")
            .field("bytes", &"<redacted>")
            .finish()
    }
}

impl Drop for SealKey {
    fn drop(&mut self) {
        // SAFETY-INVARIANT: `Zeroize::zeroize` uses volatile writes the
        // compiler may not elide, so the ephemeral key is gone once its
        // owning tier is torn down (the key is discarded, never
        // persisted).
        self.bytes.zeroize();
    }
}

/// Byte length of the random salt prefix of every nonce; the remaining
/// bytes carry the monotonic counter.
const SALT_LEN: usize = AEAD_NONCE_LEN - 8;

/// A never-repeating nonce sequence: `salt(4) ‖ counter_be(8)`.
///
/// Paired one-to-one with a [`SealKey`]; the salt is drawn once at
/// construction and the counter only ever increases, so no two nonces
/// from the same sequence are equal. Exhaustion fails closed.
pub struct NonceSequence {
    salt: [u8; SALT_LEN],
    counter: u64,
}

impl NonceSequence {
    /// Draw the per-sequence salt from `entropy` and start the counter
    /// at zero.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::Entropy`] if the salt cannot be drawn.
    pub fn new(entropy: &mut dyn EntropySource) -> Result<Self, SealError> {
        let mut salt = [0u8; SALT_LEN];
        entropy.fill(&mut salt)?;
        Ok(Self { salt, counter: 0 })
    }

    /// Produce the next unique nonce.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::NonceExhausted`] once the counter is spent;
    /// the sequence never wraps.
    pub fn next_nonce(&mut self) -> Result<AeadNonce, SealError> {
        let counter = self.counter;
        self.counter = counter.checked_add(1).ok_or(SealError::NonceExhausted)?;
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        nonce[..SALT_LEN].copy_from_slice(&self.salt);
        nonce[SALT_LEN..].copy_from_slice(&counter.to_be_bytes());
        Ok(nonce)
    }

    /// Test-only constructor placing the counter near exhaustion, so
    /// the fail-closed exhaustion path is exercisable without 2⁶⁴
    /// draws.
    #[cfg(test)]
    pub(crate) fn with_counter(salt: [u8; SALT_LEN], counter: u64) -> Self {
        Self { salt, counter }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    extern crate std;
    use std::collections::BTreeSet;
    use std::format;

    /// Deterministic counting entropy: distinct bytes per call.
    struct CountingEntropy {
        next: u8,
    }

    impl EntropySource for CountingEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
            for byte in out.iter_mut() {
                *byte = self.next;
                self.next = self.next.wrapping_add(1);
            }
            Ok(())
        }
    }

    /// Entropy source that always fails, for the fail-closed paths.
    struct DeadEntropy;

    impl EntropySource for DeadEntropy {
        fn fill(&mut self, _out: &mut [u8]) -> Result<(), SealError> {
            Err(SealError::Entropy)
        }
    }

    #[test]
    fn key_generation_fails_closed_without_entropy() {
        assert!(matches!(
            SealKey::generate(&mut DeadEntropy),
            Err(SealError::Entropy)
        ));
    }

    #[test]
    fn key_debug_never_reveals_material() {
        let key = SealKey::generate(&mut CountingEntropy { next: 1 }).expect("key");
        let rendered = format!("{key:?}");
        assert!(rendered.contains("<redacted>"));
        // No key byte leaks into the rendering: the counting source
        // yields 1, 2, 3, … so "1, 2" would betray the material.
        assert!(!rendered.contains("1, 2"));
    }

    #[test]
    fn nonce_sequence_fails_closed_without_entropy() {
        assert!(matches!(
            NonceSequence::new(&mut DeadEntropy),
            Err(SealError::Entropy)
        ));
    }

    #[test]
    fn nonces_are_unique_across_many_draws() {
        let mut sequence = NonceSequence::new(&mut CountingEntropy { next: 7 }).expect("sequence");
        let mut seen = BTreeSet::new();
        for _ in 0..10_000 {
            let nonce = sequence.next_nonce().expect("nonce");
            assert!(seen.insert(nonce), "nonce repeated");
        }
    }

    #[test]
    fn nonce_exhaustion_fails_closed_without_wrapping() {
        let mut sequence = NonceSequence::with_counter([0xAB; SALT_LEN], u64::MAX - 1);
        let last = sequence.next_nonce().expect("final nonce");
        assert_eq!(&last[SALT_LEN..], &(u64::MAX - 1).to_be_bytes());
        assert!(matches!(
            sequence.next_nonce(),
            Err(SealError::NonceExhausted)
        ));
        // Still exhausted on retry: the counter never wraps back.
        assert!(matches!(
            sequence.next_nonce(),
            Err(SealError::NonceExhausted)
        ));
    }

    #[test]
    fn distinct_sequences_carry_distinct_salts() {
        let mut entropy = CountingEntropy { next: 1 };
        let mut a = NonceSequence::new(&mut entropy).expect("a");
        let mut b = NonceSequence::new(&mut entropy).expect("b");
        assert_ne!(a.next_nonce().expect("a0"), b.next_nonce().expect("b0"));
    }
}
