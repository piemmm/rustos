//! Hashing primitives.
//!
//! The only hash exposed in `abi-v1` is SHA-256 (used by the syscall-table
//! fingerprint embedded in every manifest). Streaming is exposed only as the
//! narrow [`Sha256Stream`] below — added for the kernel's bundle content
//! digest, which frames many on-disk files through
//! `rustos_abi::digest_bundle_contents` and must not buffer the whole
//! framing in kernel memory — never as a re-export of the upstream
//! `Default`/`Update`/`Finalize` traits.

use sha2::{Digest, Sha256};

/// Length, in bytes, of a SHA-256 digest.
pub const SHA256_OUTPUT_LEN: usize = 32;

/// SHA-256 digest as raw bytes.
pub type Sha256Digest = [u8; SHA256_OUTPUT_LEN];

/// Compute the SHA-256 digest of `data`.
///
/// Wraps [`sha2::Sha256`] so callers never see the upstream `Digest` /
/// `Update` traits; this keeps the surface area auditable.
#[must_use]
pub fn sha256(data: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut digest = [0u8; SHA256_OUTPUT_LEN];
    digest.copy_from_slice(out.as_slice());
    digest
}

/// Incremental SHA-256: feed chunks with [`update`](Self::update), then
/// take the digest with [`finalize`](Self::finalize).
///
/// Audit note: this wraps the same audited [`sha2::Sha256`] core as the
/// one-shot [`sha256`] — the two can never diverge — and exists so a caller
/// hashing a large, piecewise message (the kernel's bundle content digest
/// over every file of an on-disk `.app` bundle) streams it instead of
/// concatenating the whole message in memory first. The upstream `Digest`
/// traits stay unexported; this type is the whole streaming surface.
pub struct Sha256Stream {
    inner: Sha256,
}

impl Sha256Stream {
    /// Start a new streaming SHA-256 computation.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Sha256::new(),
        }
    }

    /// Feed the next `chunk` of the message.
    pub fn update(&mut self, chunk: &[u8]) {
        self.inner.update(chunk);
    }

    /// Consume the stream and return the digest of everything fed so far.
    #[must_use]
    pub fn finalize(self) -> Sha256Digest {
        let out = self.inner.finalize();
        let mut digest = [0u8; SHA256_OUTPUT_LEN];
        digest.copy_from_slice(out.as_slice());
        digest
    }
}

impl Default for Sha256Stream {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{sha256, Sha256Stream, SHA256_OUTPUT_LEN};

    #[test]
    fn empty_string_matches_nist_vector() {
        // FIPS 180-4 §A.1: SHA-256 of the empty message.
        let expected: [u8; SHA256_OUTPUT_LEN] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(sha256(b""), expected);
    }

    #[test]
    fn streaming_matches_the_one_shot_across_chunk_boundaries() {
        // The stream wraps the same core as the one-shot, so any chunking
        // of the same message must produce the identical digest.
        let message: alloc_free_msg::Msg = alloc_free_msg::build();
        let whole = sha256(&message);
        for split in [0usize, 1, 31, 32, 33, 63, 64, 65, message.len()] {
            let mut stream = Sha256Stream::new();
            let (a, b) = message.split_at(split);
            stream.update(a);
            stream.update(b);
            assert_eq!(stream.finalize(), whole, "split at {split}");
        }
    }

    /// A deterministic 96-byte test message spanning two SHA-256 blocks,
    /// built without an allocator so the test stays `no_std`-shaped.
    mod alloc_free_msg {
        pub type Msg = [u8; 96];
        pub fn build() -> Msg {
            let mut msg = [0u8; 96];
            for (i, byte) in msg.iter_mut().enumerate() {
                let i = u8::try_from(i).expect("96-byte test message index fits in u8");
                *byte = i.wrapping_mul(37).wrapping_add(11);
            }
            msg
        }
    }

    #[test]
    fn abc_matches_nist_vector() {
        // FIPS 180-4 §A.1: SHA-256 of "abc".
        let expected: [u8; SHA256_OUTPUT_LEN] = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(sha256(b"abc"), expected);
    }
}
