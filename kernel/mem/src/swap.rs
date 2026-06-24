//! Encrypted swap layer.
//!
//! When the kernel pages anonymous, stack, or capability-bearing memory out
//! to a backing store, the bytes that leave RAM are sealed here first. This
//! preserves the zero-on-free guarantee of [`crate::sensitive`]: a swap
//! device read back off the platter — or tampered with in place — yields
//! neither plaintext nor an undetected forgery.
//!
//! # Fail-closed by construction
//!
//! the charter requires that "the kernel refuses to activate a swap
//! device that is not wrapped by the encrypted-swap layer, and fails closed
//! rather than falling back to plaintext". RustOS enforces this with the
//! type system rather than a runtime check: a [`SwapBackend`] (the raw,
//! slot-addressed device) is *useless on its own* — it exposes only opaque
//! fixed-size records — and the **only** way to read or write a page through
//! it is [`EncryptedSwap`], whose sole constructor
//! [`EncryptedSwap::activate`] takes a [`SwapKey`]. There is no plaintext
//! code path to fall back to, so a plaintext swap is unrepresentable
//! (illegal states unrepresentable).
//!
//! # Key lifetime
//!
//! The [`SwapKey`] is an **ephemeral per-boot** key drawn from the platform
//! RNG (the entropy source, injected here as [`EntropySource`] until
//! that source lands). It is zeroed on drop and never persisted: there is no
//! serialisation path and no accessor that copies the key bytes out of the
//! crate. A power cycle therefore destroys the key, so paged-out secrets
//! cannot be recovered at rest.
//!
//! # Nonce discipline
//!
//! ChaCha20-Poly1305 fails catastrophically on `(key, nonce)` reuse. Each
//! [`EncryptedSwap`] draws a random 32-bit salt at activation and appends a
//! 64-bit monotonic counter, giving a 96-bit nonce that cannot repeat for
//! the life of the (per-boot, unique) key. Counter exhaustion fails closed
//! ([`SwapError::NonceExhausted`]) rather than wrapping.

use rustos_crypto::aead::{self, AeadKey, AeadNonce, AeadTag, AEAD_NONCE_LEN, AEAD_TAG_LEN};
use zeroize::Zeroize;

use crate::frame::PAGE_SIZE;

/// Bytes of associated data binding a record to its slot: `slot.to_le_bytes()`.
const SLOT_AAD_LEN: usize = 8;

/// Offset of the nonce within an on-device swap record.
const NONCE_OFFSET: usize = 0;
/// Offset of the authentication tag within an on-device swap record.
const TAG_OFFSET: usize = NONCE_OFFSET + AEAD_NONCE_LEN;
/// Offset of the ciphertext within an on-device swap record.
const CIPHERTEXT_OFFSET: usize = TAG_OFFSET + AEAD_TAG_LEN;

/// Size, in bytes, of one on-device swap record: nonce ‖ tag ‖ ciphertext.
///
/// A [`SwapBackend`] stores and returns records of exactly this length; the
/// ciphertext region is one [`PAGE_SIZE`] page.
pub const SWAP_RECORD_LEN: usize = CIPHERTEXT_OFFSET + PAGE_SIZE;

/// A page of memory, the unit the swap layer seals and restores.
pub type SwapPage = [u8; PAGE_SIZE];

/// Reason a swap operation failed.
///
/// Every variant is a fail-closed outcome: the caller
/// must treat the page as unavailable, never as plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SwapError {
    /// The requested slot is at or beyond the backend's slot count.
    SlotOutOfRange,
    /// The per-boot nonce counter is exhausted; no further pages can be
    /// sealed under this key without risking nonce reuse.
    NonceExhausted,
    /// Authentication failed when restoring a page: the record was
    /// truncated, corrupted, forged, or moved to a different slot. The
    /// caller's buffer has been zeroed.
    Authentication,
    /// The backend reported an I/O fault or returned a malformed record.
    Backend,
    /// The injected entropy source could not supply random bytes.
    Entropy,
}

impl core::fmt::Display for SwapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let msg = match self {
            Self::SlotOutOfRange => "swap slot is outside the backing device",
            Self::NonceExhausted => "swap nonce counter exhausted for this boot key",
            Self::Authentication => "swap record failed authentication",
            Self::Backend => "swap backing device fault",
            Self::Entropy => "platform entropy source failed",
        };
        f.write_str(msg)
    }
}

/// Source of cryptographic randomness for keys and nonce salts.
///
/// This is the seam for the platform RNG. Until that subsystem lands,
/// the kernel injects a concrete implementation (mirroring the seam pattern
/// used by `init`'s `Spawner` and `login`'s `Authenticator`); the swap layer
/// itself never reaches for a global RNG.
pub trait EntropySource {
    /// Fill `out` with cryptographically random bytes.
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::Entropy`] if randomness is unavailable. The swap
    /// layer fails closed: no key or nonce salt is derived from a failed
    /// draw.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), SwapError>;
}

/// A slot-addressed raw swap device.
///
/// Implementations are storage drivers (a block device, a partition, a RAM
/// region). They move opaque [`SWAP_RECORD_LEN`]-byte records and make **no**
/// cryptographic decision — sealing and authentication are owned entirely by
/// [`EncryptedSwap`], so a backend can never expose plaintext.
pub trait SwapBackend {
    /// Number of records the device can hold. Valid slots are `0..count`.
    fn slot_count(&self) -> u64;

    /// Persist `record` (exactly [`SWAP_RECORD_LEN`] bytes) at `slot`.
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::Backend`] on an I/O fault.
    fn write_record(&mut self, slot: u64, record: &[u8]) -> Result<(), SwapError>;

    /// Read the record at `slot` into `record` (exactly [`SWAP_RECORD_LEN`]
    /// bytes).
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::Backend`] on an I/O fault.
    fn read_record(&self, slot: u64, record: &mut [u8]) -> Result<(), SwapError>;
}

/// An ephemeral per-boot swap-encryption key.
///
/// Zeroed on drop; never persisted, cloned, or copied out of the crate.
pub struct SwapKey {
    bytes: AeadKey,
}

impl SwapKey {
    /// Draw a fresh random key from `entropy`.
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::Entropy`] if the source cannot supply randomness;
    /// no key is constructed in that case.
    pub fn generate(entropy: &mut dyn EntropySource) -> Result<Self, SwapError> {
        let mut bytes: AeadKey = [0u8; rustos_crypto::aead::AEAD_KEY_LEN];
        entropy.fill(&mut bytes)?;
        Ok(Self { bytes })
    }

    /// Lend the key bytes to the in-crate cipher. Crate-private on purpose:
    /// the key never leaves `kernel/mem`.
    fn material(&self) -> &AeadKey {
        &self.bytes
    }
}

impl core::fmt::Debug for SwapKey {
    /// Never reveals the key bytes.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SwapKey")
            .field("bytes", &"<redacted>")
            .finish()
    }
}

impl Drop for SwapKey {
    fn drop(&mut self) {
        // SAFETY-INVARIANT: `Zeroize::zeroize` uses volatile writes the
        // compiler may not elide, so the ephemeral key is gone once the
        // `EncryptedSwap` that owns it is torn down (the
        // key is discarded, never persisted).
        self.bytes.zeroize();
    }
}

/// The encrypted-swap front end: the only way to use a [`SwapBackend`].
///
/// See the module docs for the fail-closed-by-construction guarantee.
pub struct EncryptedSwap<B: SwapBackend> {
    backend: B,
    key: SwapKey,
    nonce_salt: [u8; AEAD_NONCE_LEN - 8],
    counter: u64,
}

impl<B: SwapBackend> EncryptedSwap<B> {
    /// Activate encrypted swap over `backend` with `key`.
    ///
    /// Draws the per-activation nonce salt from `entropy`. This is the sole
    /// constructor: a swap device cannot be used any other way, which is how
    /// RustOS refuses plaintext swap.
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::Entropy`] if the nonce salt cannot be drawn.
    pub fn activate(
        backend: B,
        key: SwapKey,
        entropy: &mut dyn EntropySource,
    ) -> Result<Self, SwapError> {
        let mut nonce_salt = [0u8; AEAD_NONCE_LEN - 8];
        entropy.fill(&mut nonce_salt)?;
        Ok(Self {
            backend,
            key,
            nonce_salt,
            counter: 0,
        })
    }

    /// Number of slots the backing device offers.
    #[must_use]
    pub fn slot_count(&self) -> u64 {
        self.backend.slot_count()
    }

    /// Build the next unique nonce: `salt(4) ‖ counter_be(8)`.
    fn next_nonce(&mut self) -> Result<AeadNonce, SwapError> {
        let counter = self.counter;
        self.counter = counter.checked_add(1).ok_or(SwapError::NonceExhausted)?;
        let mut nonce = [0u8; AEAD_NONCE_LEN];
        nonce[..self.nonce_salt.len()].copy_from_slice(&self.nonce_salt);
        nonce[self.nonce_salt.len()..].copy_from_slice(&counter.to_be_bytes());
        Ok(nonce)
    }

    /// Seal `page` and write it to `slot`.
    ///
    /// The slot index is bound as associated data, so a record relocated to
    /// a different slot fails authentication on [`Self::load`].
    ///
    /// # Errors
    ///
    /// - [`SwapError::SlotOutOfRange`] if `slot >= slot_count()`.
    /// - [`SwapError::NonceExhausted`] if the per-boot counter is spent.
    /// - [`SwapError::Authentication`] if the cipher rejects the input.
    /// - [`SwapError::Backend`] on an I/O fault.
    pub fn store(&mut self, slot: u64, page: &SwapPage) -> Result<(), SwapError> {
        if slot >= self.backend.slot_count() {
            return Err(SwapError::SlotOutOfRange);
        }
        let nonce = self.next_nonce()?;
        let aad: [u8; SLOT_AAD_LEN] = slot.to_le_bytes();

        let mut record = [0u8; SWAP_RECORD_LEN];
        record[NONCE_OFFSET..TAG_OFFSET].copy_from_slice(&nonce);
        record[CIPHERTEXT_OFFSET..].copy_from_slice(page);
        let Ok(tag) = aead::seal(
            self.key.material(),
            &nonce,
            &aad,
            &mut record[CIPHERTEXT_OFFSET..],
        ) else {
            record.zeroize();
            return Err(SwapError::Authentication);
        };
        record[TAG_OFFSET..CIPHERTEXT_OFFSET].copy_from_slice(&tag);

        let result = self.backend.write_record(slot, &record);
        record.zeroize();
        result
    }

    /// Read and authenticate the record at `slot` into `out`.
    ///
    /// On any failure `out` is zeroed before the error is returned, so a
    /// caller can never observe forged or stale plaintext.
    ///
    /// # Errors
    ///
    /// - [`SwapError::SlotOutOfRange`] if `slot >= slot_count()`.
    /// - [`SwapError::Authentication`] if the record does not verify.
    /// - [`SwapError::Backend`] on an I/O fault.
    pub fn load(&self, slot: u64, out: &mut SwapPage) -> Result<(), SwapError> {
        if slot >= self.backend.slot_count() {
            return Err(SwapError::SlotOutOfRange);
        }
        let mut record = [0u8; SWAP_RECORD_LEN];
        if let Err(e) = self.backend.read_record(slot, &mut record) {
            out.zeroize();
            record.zeroize();
            return Err(e);
        }

        let mut nonce: AeadNonce = [0u8; AEAD_NONCE_LEN];
        nonce.copy_from_slice(&record[NONCE_OFFSET..TAG_OFFSET]);
        let mut tag: AeadTag = [0u8; AEAD_TAG_LEN];
        tag.copy_from_slice(&record[TAG_OFFSET..CIPHERTEXT_OFFSET]);
        out.copy_from_slice(&record[CIPHERTEXT_OFFSET..]);
        record.zeroize();

        let aad: [u8; SLOT_AAD_LEN] = slot.to_le_bytes();
        if aead::open(self.key.material(), &nonce, &aad, out, &tag).is_err() {
            out.zeroize();
            return Err(SwapError::Authentication);
        }
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
