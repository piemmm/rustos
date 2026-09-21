//! Hashing primitives: the SHA-2 family.
//!
//! SHA-256 is `abi-v1`'s hash (the syscall-table fingerprint embedded in
//! every manifest). SHA-384 and SHA-512 are the exchange hashes RFC 5656 and
//! RFC 8268 bind to the P-384/P-521 and larger finite-field key exchanges, so
//! they arrive with the same one-shot-plus-streaming surface rather than a
//! caller's slice of it.
//!
//! SHA-1 is deliberately absent: its one sanctioned use is the hashed
//! `known_hosts` index, which is keyed, so it is reachable only as
//! [`crate::mac::hmac_sha1`] and never as a bare digest.
//!
//! Streaming is exposed as the narrow `Sha*Stream` types below — never as a
//! re-export of the upstream `Digest`/`Update`/`Finalize` traits. Each
//! streaming type wraps the same audited core as its one-shot sibling, so the
//! two can never disagree.

use sha2::{Digest, Sha256, Sha384, Sha512};

/// Emit the one-shot function, digest alias, length constant, and streaming
/// type for one SHA-2 variant.
///
/// The four items are the complete surface of a hash and differ between
/// variants only in the upstream core and the output length, so they are
/// written once here rather than three times by hand.
macro_rules! sha2_variant {
    (
        $upstream:ty,
        $spec:literal,
        $len:ident = $bytes:literal,
        $alias:ident,
        $one_shot:ident,
        $stream:ident
    ) => {
        #[doc = concat!("Length, in bytes, of a ", $spec, " digest.")]
        pub const $len: usize = $bytes;

        #[doc = concat!("A ", $spec, " digest as raw bytes.")]
        pub type $alias = [u8; $len];

        #[doc = concat!("Compute the ", $spec, " digest of `data`.")]
        #[must_use]
        pub fn $one_shot(data: &[u8]) -> $alias {
            let mut stream = $stream::new();
            stream.update(data);
            stream.finalize()
        }

        #[doc = concat!("Incremental ", $spec, ": feed chunks with")]
        /// [`update`](Self::update), then take the digest with
        /// [`finalize`](Self::finalize).
        ///
        /// Exists so a caller hashing a large, piecewise message — a bundle's
        /// every file, or an SSH exchange hash over eight length-prefixed
        /// fields — streams it instead of first concatenating the whole
        /// message in memory.
        pub struct $stream {
            inner: $upstream,
        }

        impl $stream {
            #[doc = concat!("Start a new streaming ", $spec, " computation.")]
            #[must_use]
            pub fn new() -> Self {
                Self {
                    inner: <$upstream>::new(),
                }
            }

            /// Feed the next `chunk` of the message.
            pub fn update(&mut self, chunk: &[u8]) {
                self.inner.update(chunk);
            }

            /// Consume the stream and return the digest of everything fed so
            /// far.
            #[must_use]
            pub fn finalize(self) -> $alias {
                let out = self.inner.finalize();
                let mut digest = [0u8; $len];
                digest.copy_from_slice(out.as_slice());
                digest
            }
        }

        impl Default for $stream {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

sha2_variant!(
    Sha256,
    "SHA-256",
    SHA256_OUTPUT_LEN = 32,
    Sha256Digest,
    sha256,
    Sha256Stream
);
sha2_variant!(
    Sha384,
    "SHA-384",
    SHA384_OUTPUT_LEN = 48,
    Sha384Digest,
    sha384,
    Sha384Stream
);
sha2_variant!(
    Sha512,
    "SHA-512",
    SHA512_OUTPUT_LEN = 64,
    Sha512Digest,
    sha512,
    Sha512Stream
);

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
