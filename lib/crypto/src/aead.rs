//! Authenticated encryption with associated data (AEAD).
//!
//! TAIRiX's own AEAD is ChaCha20-Poly1305 (RFC 8439), and it is what the
//! unqualified [`seal`] and [`open`] below are: every first-party consumer
//! that gets to choose — encrypted swap, ARXFS, the app-data vault, the
//! realm session — uses it. AES-GCM is here for one reason only, and its
//! entry points say so in their names: a foreign SSH peer may offer
//! `aes{128,256}-gcm@openssh.com` and nothing else we accept. The two are
//! not peers, and the naming is the distinction.
//!
//! ChaCha20-Poly1305
//! backs the kernel's encrypted-swap layer: any page of
//! anonymous, stack, or capability-bearing memory the kernel writes to a
//! swap device is sealed here first, so a swap device read back off the
//! platter (or tampered with in place) yields neither plaintext nor an
//! undetected forgery.
//!
//! As with the rest of `lib/crypto`, the wrapper exposes a *narrower* API
//! than the upstream crate: callers hand in fixed-size byte arrays for the
//! key, nonce, and tag and a mutable buffer for the message, and never see
//! the upstream `aead` traits or `GenericArray` types. Encryption is
//! **detached and in place** — the ciphertext overwrites the plaintext in
//! the caller's buffer and the authentication tag is returned separately —
//! so the wrapper needs no allocator and stays `no_std`.
//!
//! # Nonce discipline
//!
//! ChaCha20-Poly1305 is catastrophically insecure if a `(key, nonce)` pair
//! is ever reused. This module does **not** generate nonces: that is the
//! caller's responsibility, because only the caller knows whether its key
//! is long-lived or — as for swap — an ephemeral per-boot key paired with a
//! monotonic counter that cannot repeat within the key's lifetime. See
//! `kernel/mem`'s `swap` module for the swap-side discipline.

use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce as AesGcmNonceArray, Tag as AesGcmTagArray};
use chacha20poly1305::aead::AeadInOut;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};

/// Length, in bytes, of a ChaCha20-Poly1305 key.
pub const AEAD_KEY_LEN: usize = 32;

/// Length, in bytes, of a ChaCha20-Poly1305 nonce.
pub const AEAD_NONCE_LEN: usize = 12;

/// Length, in bytes, of a Poly1305 authentication tag.
pub const AEAD_TAG_LEN: usize = 16;

/// A 256-bit ChaCha20-Poly1305 key as raw bytes.
pub type AeadKey = [u8; AEAD_KEY_LEN];

/// A 96-bit ChaCha20-Poly1305 nonce as raw bytes.
pub type AeadNonce = [u8; AEAD_NONCE_LEN];

/// A 128-bit Poly1305 authentication tag as raw bytes.
pub type AeadTag = [u8; AEAD_TAG_LEN];

/// Reason an AEAD operation failed.
///
/// The variant set is deliberately coarse: a caller never learns *why*
/// authentication failed, only that it did, so a forgery attempt leaks
/// nothing (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AeadError {
    /// Authentication failed: the ciphertext, tag, nonce, or associated
    /// data does not match what was sealed. On [`open`] the caller's
    /// buffer holds undefined plaintext and must be discarded.
    Authentication,
}

/// Seal `buffer` in place under `key` and `nonce`, binding `aad`.
///
/// On return `buffer` holds the ciphertext (same length as the plaintext)
/// and the returned [`AeadTag`] authenticates both the ciphertext and the
/// associated data `aad`. The caller must store the nonce and tag and
/// present the identical `aad` to [`open`].
///
/// # Errors
///
/// Returns [`AeadError::Authentication`] only if the upstream cipher
/// rejects the inputs (e.g. a message longer than the cipher's
/// `64 GiB`-per-nonce limit). For the page-sized buffers TAIRiX seals this
/// cannot occur in practice, but the path is fallible rather than panicking.
pub fn seal(
    key: &AeadKey,
    nonce: &AeadNonce,
    aad: &[u8],
    buffer: &mut [u8],
) -> Result<AeadTag, AeadError> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| AeadError::Authentication)?;
    let tag = cipher
        .encrypt_inout_detached(&Nonce::from(*nonce), aad, buffer.into())
        .map_err(|_| AeadError::Authentication)?;
    let mut out = [0u8; AEAD_TAG_LEN];
    out.copy_from_slice(tag.as_slice());
    Ok(out)
}

/// Open `buffer` in place under `key`, `nonce`, `aad`, and `tag`.
///
/// On success `buffer` holds the recovered plaintext. On failure the
/// buffer's contents are unspecified and the caller must not use them; the
/// swap layer zeroes the buffer before surfacing the error.
///
/// # Errors
///
/// Returns [`AeadError::Authentication`] if the tag does not verify — the
/// ciphertext, nonce, associated data, or tag was altered, or the key is
/// wrong.
pub fn open(
    key: &AeadKey,
    nonce: &AeadNonce,
    aad: &[u8],
    buffer: &mut [u8],
    tag: &AeadTag,
) -> Result<(), AeadError> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| AeadError::Authentication)?;
    cipher
        .decrypt_inout_detached(&Nonce::from(*nonce), aad, buffer.into(), &Tag::from(*tag))
        .map_err(|_| AeadError::Authentication)
}

/// Length, in bytes, of an AES-GCM nonce. RFC 5116 §5.1's 96-bit nonce is
/// the only width AES-GCM accepts without an extra GHASH derivation step,
/// and the one `aes*-gcm@openssh.com` (RFC 5647) uses.
pub const AES_GCM_NONCE_LEN: usize = 12;

/// Length, in bytes, of an AES-GCM authentication tag.
pub const AES_GCM_TAG_LEN: usize = 16;

/// An AES-GCM nonce as raw bytes.
pub type AesGcmNonce = [u8; AES_GCM_NONCE_LEN];

/// An AES-GCM authentication tag as raw bytes.
pub type AesGcmTag = [u8; AES_GCM_TAG_LEN];

/// Emit the detached in-place seal and open for one AES-GCM key size.
///
/// The two sizes differ only in the underlying key schedule, so the wrapper
/// is written once rather than twice.
macro_rules! aes_gcm_variant {
    ($cipher:ty, $spec:literal, $key_len:ident = $width:literal, $key_ty:ident, $seal:ident, $open:ident) => {
        #[doc = concat!("Length, in bytes, of an ", $spec, " key.")]
        pub const $key_len: usize = $width;

        #[doc = concat!("An ", $spec, " key as raw bytes.")]
        pub type $key_ty = [u8; $key_len];

        #[doc = concat!("Seal `buffer` in place with ", $spec, " under `key`")]
        /// and `nonce`, binding `aad`.
        ///
        /// On return `buffer` holds the ciphertext and the returned tag
        /// authenticates both it and `aad`.
        ///
        /// GCM fails catastrophically on `(key, nonce)` reuse — a repeat
        /// leaks the GHASH authentication key and so the ability to forge
        /// any message under that key. This wrapper generates no nonces;
        /// RFC 5647 gives SSH a fixed field plus a monotonic invocation
        /// counter, and the caller owns that discipline.
        ///
        /// # Errors
        ///
        /// Returns [`AeadError::Authentication`] if the upstream cipher
        /// refuses the inputs, which for AES-GCM means a message past its
        /// `~64 GiB` per-nonce limit.
        pub fn $seal(
            key: &$key_ty,
            nonce: &AesGcmNonce,
            aad: &[u8],
            buffer: &mut [u8],
        ) -> Result<AesGcmTag, AeadError> {
            let cipher = <$cipher>::new_from_slice(key).map_err(|_| AeadError::Authentication)?;
            let tag = cipher
                .encrypt_inout_detached(&AesGcmNonceArray::from(*nonce), aad, buffer.into())
                .map_err(|_| AeadError::Authentication)?;
            let mut out = [0u8; AES_GCM_TAG_LEN];
            out.copy_from_slice(tag.as_slice());
            Ok(out)
        }

        #[doc = concat!("Open `buffer` in place with ", $spec, " under `key`,")]
        /// `nonce`, `aad`, and `tag`.
        ///
        /// The tag is checked *before* anything is decrypted, so a rejected
        /// message leaves `buffer` holding the ciphertext it arrived as and
        /// never the plaintext — the failure CVE-2023-42811 was.
        ///
        /// # Errors
        ///
        /// Returns [`AeadError::Authentication`] if the tag does not verify.
        pub fn $open(
            key: &$key_ty,
            nonce: &AesGcmNonce,
            aad: &[u8],
            buffer: &mut [u8],
            tag: &AesGcmTag,
        ) -> Result<(), AeadError> {
            let cipher = <$cipher>::new_from_slice(key).map_err(|_| AeadError::Authentication)?;
            cipher
                .decrypt_inout_detached(
                    &AesGcmNonceArray::from(*nonce),
                    aad,
                    buffer.into(),
                    &AesGcmTagArray::from(*tag),
                )
                .map_err(|_| AeadError::Authentication)
        }
    };
}

aes_gcm_variant!(
    Aes128Gcm,
    "AES-128-GCM",
    AES128_GCM_KEY_LEN = 16,
    Aes128GcmKey,
    aes128gcm_seal,
    aes128gcm_open
);
aes_gcm_variant!(
    Aes256Gcm,
    "AES-256-GCM",
    AES256_GCM_KEY_LEN = 32,
    Aes256GcmKey,
    aes256gcm_seal,
    aes256gcm_open
);

#[cfg(test)]
#[path = "aead_tests.rs"]
mod tests;
