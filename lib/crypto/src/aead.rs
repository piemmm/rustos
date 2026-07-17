//! Authenticated encryption with associated data (AEAD).
//!
//! The single AEAD exposed by TAIRiX is ChaCha20-Poly1305 (RFC 8439). It
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

use chacha20poly1305::aead::AeadInPlace;
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
        .encrypt_in_place_detached(Nonce::from_slice(nonce), aad, buffer)
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
        .decrypt_in_place_detached(Nonce::from_slice(nonce), aad, buffer, Tag::from_slice(tag))
        .map_err(|_| AeadError::Authentication)
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec::Vec;

    const KEY: AeadKey = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const NONCE: AeadNonce = [
        0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab,
    ];

    #[test]
    fn round_trip_recovers_plaintext() {
        let plaintext = b"page bytes paged out to swap".to_vec();
        let mut buf = plaintext.clone();
        let tag = seal(&KEY, &NONCE, b"slot-7", &mut buf).expect("seal");
        assert_ne!(buf, plaintext, "ciphertext must differ from plaintext");
        open(&KEY, &NONCE, b"slot-7", &mut buf, &tag).expect("open");
        assert_eq!(buf, plaintext);
    }

    #[test]
    fn empty_message_round_trips() {
        let mut buf: Vec<u8> = Vec::new();
        let tag = seal(&KEY, &NONCE, b"", &mut buf).expect("seal");
        open(&KEY, &NONCE, b"", &mut buf, &tag).expect("open");
        assert!(buf.is_empty());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut buf = b"secret".to_vec();
        let tag = seal(&KEY, &NONCE, b"", &mut buf).expect("seal");
        buf[0] ^= 0x01;
        assert_eq!(
            open(&KEY, &NONCE, b"", &mut buf, &tag),
            Err(AeadError::Authentication)
        );
    }

    #[test]
    fn tampered_tag_is_rejected() {
        let mut buf = b"secret".to_vec();
        let mut tag = seal(&KEY, &NONCE, b"", &mut buf).expect("seal");
        tag[0] ^= 0x01;
        assert_eq!(
            open(&KEY, &NONCE, b"", &mut buf, &tag),
            Err(AeadError::Authentication)
        );
    }

    #[test]
    fn wrong_associated_data_is_rejected() {
        let mut buf = b"secret".to_vec();
        let tag = seal(&KEY, &NONCE, b"slot-7", &mut buf).expect("seal");
        assert_eq!(
            open(&KEY, &NONCE, b"slot-8", &mut buf, &tag),
            Err(AeadError::Authentication)
        );
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let mut buf = b"secret".to_vec();
        let tag = seal(&KEY, &NONCE, b"", &mut buf).expect("seal");
        let mut other = NONCE;
        other[0] ^= 0x01;
        assert_eq!(
            open(&KEY, &other, b"", &mut buf, &tag),
            Err(AeadError::Authentication)
        );
    }

    #[test]
    fn rfc8439_test_vector() {
        // RFC 8439 §2.8.2 worked example.
        let key: AeadKey = [
            0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d,
            0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b,
            0x9c, 0x9d, 0x9e, 0x9f,
        ];
        let nonce: AeadNonce = [
            0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
        ];
        let aad = [
            0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
        ];
        let mut buf = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.".to_vec();
        let expected_tag: AeadTag = [
            0x1a, 0xe1, 0x0b, 0x59, 0x4f, 0x09, 0xe2, 0x6a, 0x7e, 0x90, 0x2e, 0xcb, 0xd0, 0x60,
            0x06, 0x91,
        ];
        let tag = seal(&key, &nonce, &aad, &mut buf).expect("seal");
        assert_eq!(tag, expected_tag, "tag must match the RFC 8439 vector");
        open(&key, &nonce, &aad, &mut buf, &tag).expect("open");
        assert_eq!(&buf[..6], b"Ladies");
    }
}
