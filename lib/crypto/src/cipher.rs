//! Unauthenticated block-cipher modes: AES in counter mode.
//!
//! AES-CTR is the SSH transport's `aes{128,192,256}-ctr` (RFC 4344): the
//! whole 16-byte block is one big-endian counter, seeded from the IV the key
//! exchange derived and advanced across every packet of the connection.
//!
//! Unlike the rest of this crate these wrappers are **stateful**, and that is
//! the point: a one-shot would restart the counter on each call, and CTR
//! reuses its keystream the instant a counter value repeats. One cipher
//! object per direction per connection, advanced by the bytes it processes,
//! is the only shape that cannot do that.
//!
//! CTR provides confidentiality and **no** integrity. Every caller must
//! authenticate separately — SSH pairs these with an
//! [`crate::mac`] HMAC — and must verify the tag before acting on plaintext.
//! Where an AEAD will do, prefer [`crate::aead`].

use core::fmt;

use aes::cipher::{KeyIvInit, StreamCipher};
use aes::{Aes128, Aes192, Aes256};
use ctr::Ctr128BE;

/// AES block size in bytes, and so the width of a CTR initial counter.
pub const AES_BLOCK_LEN: usize = 16;

/// An AES-CTR initial counter block as raw bytes.
pub type AesCtrIv = [u8; AES_BLOCK_LEN];

/// An AES-CTR run did not fit the counter.
///
/// The counter is the full 128-bit block, so exhausting it takes `2^132`
/// bytes under one key: unreachable for any real message. The error exists
/// because silently wrapping the counter would reuse keystream, which is a
/// total loss of confidentiality, so the path refuses rather than assuming.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CipherError(());

impl fmt::Display for CipherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("aes-ctr run exceeds the counter")
    }
}

/// Emit the key type, length constant, and stateful cipher for one AES-CTR
/// key size.
///
/// The three sizes differ only in the underlying AES key schedule, so the
/// wrapper is written once rather than three times.
macro_rules! aes_ctr_variant {
    (
        $block:ty,
        $spec:literal,
        $key_len:ident = $width:literal,
        $key_ty:ident,
        $cipher:ident
    ) => {
        #[doc = concat!("Length, in bytes, of an ", $spec, " key.")]
        pub const $key_len: usize = $width;

        #[doc = concat!("An ", $spec, " key as raw bytes.")]
        pub type $key_ty = [u8; $key_len];

        #[doc = concat!($spec, " in counter mode, advanced across calls.")]
        ///
        /// Holds the expanded key schedule and the live counter. The
        /// upstream cipher wipes both on drop, but the caller's copy of the
        /// key is the caller's to zero.
        pub struct $cipher {
            inner: Ctr128BE<$block>,
        }

        impl $cipher {
            #[doc = concat!("Start ", $spec, "-CTR under `key` from the initial")]
            /// counter block `iv`.
            ///
            /// `(key, iv)` must never repeat: the keystream depends on
            /// nothing else, so a repeat XORs two plaintexts together. SSH
            /// derives both per direction from the key exchange and rekeys
            /// before the counter could revisit a block.
            #[must_use]
            pub fn new(key: &$key_ty, iv: &AesCtrIv) -> Self {
                Self {
                    inner: Ctr128BE::<$block>::new(key.into(), iv.into()),
                }
            }

            /// XOR the next keystream bytes into `buffer`, advancing the
            /// counter by the bytes consumed.
            ///
            /// Encryption and decryption are the same operation. Successive
            /// calls continue one keystream, so a packet split across calls
            /// gives the same result as one call over the whole of it.
            ///
            /// # Errors
            ///
            /// Returns [`CipherError`] if the run would carry the counter
            /// past its last block rather than wrapping onto keystream this
            /// key has already emitted.
            pub fn apply(&mut self, buffer: &mut [u8]) -> Result<(), CipherError> {
                self.inner
                    .try_apply_keystream(buffer)
                    .map_err(|_| CipherError(()))
            }
        }

        impl fmt::Debug for $cipher {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Neither the key schedule nor the counter position is
                // printed: both narrow a keystream recovery.
                f.debug_struct(stringify!($cipher)).finish_non_exhaustive()
            }
        }
    };
}

aes_ctr_variant!(Aes128, "AES-128", AES128_KEY_LEN = 16, Aes128Key, Aes128Ctr);
aes_ctr_variant!(Aes192, "AES-192", AES192_KEY_LEN = 24, Aes192Key, Aes192Ctr);
aes_ctr_variant!(Aes256, "AES-256", AES256_KEY_LEN = 32, Aes256Key, Aes256Ctr);

#[cfg(test)]
#[path = "cipher_tests.rs"]
mod tests;
