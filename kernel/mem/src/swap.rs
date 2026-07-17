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
//! rather than falling back to plaintext". TAIRiX enforces this with the
//! type system rather than a runtime check: a [`SwapBackend`] (the raw,
//! slot-addressed device) is *useless on its own* — it exposes only opaque
//! fixed-size records — and the **only** way to read or write a page through
//! it is [`EncryptedSwap`], whose sole constructor
//! [`EncryptedSwap::activate`] takes a [`SealKey`]. There is no plaintext
//! code path to fall back to, so a plaintext swap is unrepresentable
//! (illegal states unrepresentable).
//!
//! # Key and nonce discipline
//!
//! The key is an **ephemeral per-boot** [`SealKey`] drawn from the
//! platform RNG (the entropy source, injected as the shared
//! [`EntropySource`] seam): zeroed on drop, never persisted, so a power
//! cycle destroys it and paged-out secrets cannot be recovered at rest.
//! Nonces come from the shared [`NonceSequence`] (random salt plus
//! monotonic counter — one definition in [`crate::seal`], shared with
//! the compressed anonymous-memory tier). Counter exhaustion fails
//! closed ([`SwapError::NonceExhausted`]) rather than wrapping. The
//! swap key and nonce sequence are this layer's own; the RAM tier
//! ([`crate::ramzip`]) holds separate ones and neither depends on the
//! other's key or metadata format.

use tairix_crypto::aead::{self, AeadNonce, AeadTag, AEAD_NONCE_LEN, AEAD_TAG_LEN};
use zeroize::Zeroize;

use crate::frame::PAGE_SIZE;
use crate::seal::{EntropySource, NonceSequence, SealError, SealKey};

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

impl From<SealError> for SwapError {
    fn from(e: SealError) -> Self {
        match e {
            SealError::Entropy => Self::Entropy,
            SealError::NonceExhausted => Self::NonceExhausted,
        }
    }
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

/// The encrypted-swap front end: the only way to use a [`SwapBackend`].
///
/// See the module docs for the fail-closed-by-construction guarantee.
pub struct EncryptedSwap<B: SwapBackend> {
    backend: B,
    key: SealKey,
    nonces: NonceSequence,
}

impl<B: SwapBackend> EncryptedSwap<B> {
    /// Activate encrypted swap over `backend` with `key`.
    ///
    /// Draws the per-activation nonce salt from `entropy`. This is the sole
    /// constructor: a swap device cannot be used any other way, which is how
    /// TAIRiX refuses plaintext swap.
    ///
    /// # Errors
    ///
    /// Returns [`SwapError::Entropy`] if the nonce salt cannot be drawn.
    pub fn activate(
        backend: B,
        key: SealKey,
        entropy: &mut dyn EntropySource,
    ) -> Result<Self, SwapError> {
        let nonces = NonceSequence::new(entropy)?;
        Ok(Self {
            backend,
            key,
            nonces,
        })
    }

    /// Number of slots the backing device offers.
    #[must_use]
    pub fn slot_count(&self) -> u64 {
        self.backend.slot_count()
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
        let nonce = self.nonces.next_nonce()?;
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
