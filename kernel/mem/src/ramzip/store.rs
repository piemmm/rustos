//! Sealed-page encode/decode for the compressed anonymous-memory tier
//! (`plans/SWAPSWAPSWAP.md` sections 8 and 9).
//!
//! A page enters the tier as *compress, then encrypt and authenticate*
//! — never the reverse: ciphertext is high-entropy and would not
//! compress. The stored form is an AEAD-sealed blob (nonce, tag,
//! ciphertext of the compressed page) whose associated data binds the
//! entry's identity (address space, page number, mapping flags), so a
//! blob replayed against a different page, space, or permission set
//! fails authentication.
//!
//! A page that does not compress below the acceptance bound is refused
//! ([`SealFailure::Incompressible`]) rather than stored raw: storing
//! raw pages would defeat the tier's purpose and add pressure.
//!
//! Every temporary that held plaintext (or the compressed plaintext)
//! is zeroed before it leaves scope, on the success and failure paths
//! alike. Authentication or decode failure returns a typed error and a
//! zeroed output buffer — never plaintext, never a guess.

use alloc::vec::Vec;

use tairix_compress as compress;
use tairix_crypto::aead::{self, AeadNonce, AeadTag, AEAD_NONCE_LEN, AEAD_TAG_LEN};
use zeroize::Zeroize;

use crate::frame::PAGE_SIZE;
use crate::seal::{NonceSequence, SealError, SealKey};

/// Fixed sealing overhead per stored entry: the nonce and the
/// authentication tag.
pub(crate) const SEAL_OVERHEAD: usize = AEAD_NONCE_LEN + AEAD_TAG_LEN;

/// One sealed page as held in the RAM pool: the unique nonce, the
/// authentication tag, and the ciphertext of the compressed page.
///
/// The ciphertext reveals nothing without the per-boot key, so the
/// blob itself needs no scrub on free; the plaintext forms it was
/// derived from are zeroed inside [`seal_page`].
#[derive(Debug)]
pub(crate) struct SealedBlob {
    /// The entry's unique nonce.
    pub nonce: AeadNonce,
    /// The authentication tag over ciphertext and associated data.
    pub tag: AeadTag,
    /// The ciphertext of the compressed page.
    pub ciphertext: Vec<u8>,
}

impl SealedBlob {
    /// Bytes this blob occupies in the RAM pool (ciphertext plus the
    /// fixed sealing overhead).
    pub(crate) fn stored_len(&self) -> usize {
        self.ciphertext.len().saturating_add(SEAL_OVERHEAD)
    }
}

/// Why a page could not be sealed. Every variant leaves the page
/// exactly where it was and every plaintext temporary zeroed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum SealFailure {
    /// The page did not compress below the acceptance bound; the tier
    /// refuses it rather than storing raw bytes.
    Incompressible,
    /// The blob allocation failed (deterministic OOM, not a panic).
    Alloc,
    /// The nonce sequence is exhausted; no further pages can be sealed
    /// under this boot key.
    NonceExhausted,
    /// The cipher refused the input (never expected for in-bound
    /// lengths; surfaced rather than trusted away).
    Seal,
}

impl From<SealError> for SealFailure {
    fn from(e: SealError) -> Self {
        match e {
            // A seal-time entropy failure cannot occur (the salt is
            // drawn at construction), but the conversion stays total
            // and fail-closed.
            SealError::Entropy => Self::Seal,
            SealError::NonceExhausted => Self::NonceExhausted,
        }
    }
}

/// Why a sealed page could not be opened. Every variant returns with
/// the caller's output buffer zeroed — no plaintext, no partial write.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum OpenFailure {
    /// The blob's metadata is out of bounds (a ciphertext longer than
    /// any the tier stores, or an empty one): refused before any
    /// cryptography runs.
    Corrupt,
    /// Authentication failed: the blob was tampered with, replayed
    /// against a different identity, or damaged.
    Authentication,
    /// The authenticated payload did not decompress to exactly one
    /// page: refused after authentication, still fail-closed.
    Decode,
}

/// Compress `page` and seal the result under `key`, binding `aad`.
///
/// `max_compressed` is the acceptance bound: a page whose compressed
/// form (before sealing overhead) exceeds it is refused as
/// incompressible. The caller derives the bound from its metadata
/// budget so that a stored entry is always strictly smaller than the
/// page it replaces.
///
/// # Errors
///
/// See [`SealFailure`]; on every error path the plaintext scratch is
/// zeroed and no nonce is consumed except after a successful
/// compression (a refused page costs no nonce).
pub(crate) fn seal_page(
    key: &SealKey,
    nonces: &mut NonceSequence,
    aad: &[u8],
    page: &[u8; PAGE_SIZE],
    max_compressed: usize,
) -> Result<SealedBlob, SealFailure> {
    let bound = max_compressed.min(PAGE_SIZE);
    let mut scratch = [0u8; PAGE_SIZE];
    let compressed_len = match compress::compress(page, &mut scratch[..bound]) {
        Ok(len) => len,
        Err(compress::Error::TooSmall) => {
            scratch.zeroize();
            return Err(SealFailure::Incompressible);
        }
        Err(compress::Error::Corrupt) => {
            // `compress` never reports corruption for a fresh encode;
            // fail closed rather than reasoning it away.
            scratch.zeroize();
            return Err(SealFailure::Seal);
        }
    };

    let mut ciphertext = Vec::new();
    if ciphertext.try_reserve_exact(compressed_len).is_err() {
        scratch.zeroize();
        return Err(SealFailure::Alloc);
    }

    let nonce = match nonces.next_nonce() {
        Ok(nonce) => nonce,
        Err(e) => {
            scratch.zeroize();
            return Err(e.into());
        }
    };
    let Ok(tag) = aead::seal(key.material(), &nonce, aad, &mut scratch[..compressed_len]) else {
        scratch.zeroize();
        return Err(SealFailure::Seal);
    };
    ciphertext.extend_from_slice(&scratch[..compressed_len]);
    // The scratch head is ciphertext now, but its tail may still hold
    // compressed plaintext beyond `compressed_len`; scrub it all.
    scratch.zeroize();
    Ok(SealedBlob {
        nonce,
        tag,
        ciphertext,
    })
}

/// Authenticate, decrypt, and decompress `blob` into `out`, verifying
/// the same `aad` identity it was sealed under.
///
/// # Errors
///
/// See [`OpenFailure`]. On every error `out` is fully zeroed before
/// returning, so a caller can never observe forged, stale, or partial
/// plaintext; the compressed-plaintext scratch is likewise zeroed on
/// all paths.
pub(crate) fn open_page(
    key: &SealKey,
    aad: &[u8],
    blob: &SealedBlob,
    out: &mut [u8; PAGE_SIZE],
) -> Result<(), OpenFailure> {
    let len = blob.ciphertext.len();
    // Metadata validation before any cryptography: a stored entry is
    // always non-empty and strictly smaller than a page.
    if len == 0 || len >= PAGE_SIZE {
        out.zeroize();
        return Err(OpenFailure::Corrupt);
    }

    let mut scratch = [0u8; PAGE_SIZE];
    scratch[..len].copy_from_slice(&blob.ciphertext);
    if aead::open(
        key.material(),
        &blob.nonce,
        aad,
        &mut scratch[..len],
        &blob.tag,
    )
    .is_err()
    {
        scratch.zeroize();
        out.zeroize();
        return Err(OpenFailure::Authentication);
    }

    if let Ok(PAGE_SIZE) = compress::decompress(&scratch[..len], out) {
        scratch.zeroize();
        Ok(())
    } else {
        scratch.zeroize();
        out.zeroize();
        Err(OpenFailure::Decode)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::seal::EntropySource;

    extern crate std;

    /// Deterministic counting entropy for keys and salts.
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

    fn key_and_nonces() -> (SealKey, NonceSequence) {
        let mut entropy = CountingEntropy { next: 1 };
        let key = SealKey::generate(&mut entropy).expect("key");
        let nonces = NonceSequence::new(&mut entropy).expect("nonces");
        (key, nonces)
    }

    /// A page that compresses well: long runs with a sprinkle of
    /// structure.
    fn compressible_page() -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        for (i, byte) in page.iter_mut().enumerate() {
            *byte = u8::try_from((i / 256) % 7).expect("small value");
        }
        page
    }

    /// A page of PRNG noise: incompressible by construction.
    fn incompressible_page() -> [u8; PAGE_SIZE] {
        let mut page = [0u8; PAGE_SIZE];
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        for byte in &mut page {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *byte = (state >> 33).to_le_bytes()[0];
        }
        page
    }

    #[test]
    fn round_trip_restores_exact_page_bytes() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        let blob = seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256).expect("seal");
        assert!(blob.stored_len() < PAGE_SIZE);
        let mut out = [0u8; PAGE_SIZE];
        open_page(&key, b"aad", &blob, &mut out).expect("open");
        assert_eq!(out, page);
    }

    #[test]
    fn incompressible_page_is_refused_not_stored_raw() {
        let (key, mut nonces) = key_and_nonces();
        let page = incompressible_page();
        assert!(matches!(
            seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256),
            Err(SealFailure::Incompressible)
        ));
        // A refused page consumed no nonce: the next seal still gets
        // the first counter value.
        let good = compressible_page();
        let blob = seal_page(&key, &mut nonces, b"aad", &good, PAGE_SIZE - 256).expect("seal");
        assert_eq!(&blob.nonce[4..], &0u64.to_be_bytes());
    }

    #[test]
    fn tampered_ciphertext_fails_authentication_and_zeroes_out() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        let mut blob = seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256).expect("seal");
        blob.ciphertext[0] ^= 0x01;
        let mut out = [0xFFu8; PAGE_SIZE];
        assert_eq!(
            open_page(&key, b"aad", &blob, &mut out),
            Err(OpenFailure::Authentication)
        );
        assert!(out.iter().all(|b| *b == 0), "output must be zeroed");
    }

    #[test]
    fn tampered_tag_and_nonce_fail_authentication() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        let blob = seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256).expect("seal");

        let mut bad_tag = SealedBlob {
            nonce: blob.nonce,
            tag: blob.tag,
            ciphertext: blob.ciphertext.clone(),
        };
        bad_tag.tag[0] ^= 0x01;
        let mut out = [0u8; PAGE_SIZE];
        assert_eq!(
            open_page(&key, b"aad", &bad_tag, &mut out),
            Err(OpenFailure::Authentication)
        );

        let mut bad_nonce = SealedBlob {
            nonce: blob.nonce,
            tag: blob.tag,
            ciphertext: blob.ciphertext,
        };
        bad_nonce.nonce[0] ^= 0x01;
        assert_eq!(
            open_page(&key, b"aad", &bad_nonce, &mut out),
            Err(OpenFailure::Authentication)
        );
    }

    #[test]
    fn replay_under_a_different_identity_fails_authentication() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        let blob = seal_page(
            &key,
            &mut nonces,
            b"space 1, page 5",
            &page,
            PAGE_SIZE - 256,
        )
        .expect("seal");
        let mut out = [0u8; PAGE_SIZE];
        assert_eq!(
            open_page(&key, b"space 2, page 5", &blob, &mut out),
            Err(OpenFailure::Authentication)
        );
        assert!(out.iter().all(|b| *b == 0));
    }

    #[test]
    fn out_of_bounds_metadata_is_refused_before_cryptography() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        let blob = seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256).expect("seal");

        let empty = SealedBlob {
            nonce: blob.nonce,
            tag: blob.tag,
            ciphertext: Vec::new(),
        };
        let mut out = [0xAAu8; PAGE_SIZE];
        assert_eq!(
            open_page(&key, b"aad", &empty, &mut out),
            Err(OpenFailure::Corrupt)
        );
        assert!(out.iter().all(|b| *b == 0));

        let oversized_bytes = alloc::vec![0u8; PAGE_SIZE];
        let oversized = SealedBlob {
            nonce: blob.nonce,
            tag: blob.tag,
            ciphertext: oversized_bytes,
        };
        assert_eq!(
            open_page(&key, b"aad", &oversized, &mut out),
            Err(OpenFailure::Corrupt)
        );
    }

    #[test]
    fn authenticated_garbage_that_fails_decode_is_refused() {
        // Seal a valid compressed stream, then seal a *shorter* bound
        // so the plaintext is a truncated stream: authentication
        // passes (it was sealed honestly) but decode must fail closed.
        let (key, mut nonces) = key_and_nonces();
        let mut not_a_stream = [0u8; PAGE_SIZE];
        not_a_stream[..4].copy_from_slice(b"RLZ1");
        // Declared length 4096 but no sequence bytes follow: corrupt.
        not_a_stream[4..8].copy_from_slice(&4096u32.to_le_bytes());
        let mut scratch = [0u8; 8];
        scratch.copy_from_slice(&not_a_stream[..8]);
        let nonce = nonces.next_nonce().expect("nonce");
        let mut sealed = scratch;
        let tag = aead::seal(key.material(), &nonce, b"aad", &mut sealed).expect("seal");
        let mut ciphertext = Vec::new();
        ciphertext.extend_from_slice(&sealed);
        let blob = SealedBlob {
            nonce,
            tag,
            ciphertext,
        };
        let mut out = [0x55u8; PAGE_SIZE];
        assert_eq!(
            open_page(&key, b"aad", &blob, &mut out),
            Err(OpenFailure::Decode)
        );
        assert!(out.iter().all(|b| *b == 0));
    }

    #[test]
    fn nonces_differ_across_sealed_entries() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        let a = seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256).expect("a");
        let b = seal_page(&key, &mut nonces, b"aad", &page, PAGE_SIZE - 256).expect("b");
        assert_ne!(a.nonce, b.nonce);
    }

    #[test]
    fn acceptance_bound_is_respected() {
        let (key, mut nonces) = key_and_nonces();
        let page = compressible_page();
        // An impossible bound refuses even a compressible page.
        assert!(matches!(
            seal_page(&key, &mut nonces, b"aad", &page, 8),
            Err(SealFailure::Incompressible)
        ));
    }
}
