//! Stream-cipher keystream generation (`ChaCha12`).
//!
//! The one stream cipher TAIRiX exposes is `ChaCha12` — the twelve-round
//! reduced variant of `ChaCha20` (RFC 8439), the construction OpenBSD's
//! `arc4random` and Linux's `get_random_u64` expand their fast random output
//! from. It backs `lib/rng`'s fast generator; long-lived key material stays
//! on the SP 800-90A DRBG there.
//!
//! As with the rest of this crate the wrapper is *narrower* than the upstream
//! crate: callers hand in fixed-size byte arrays and never see the upstream
//! `cipher` traits or `GenericArray`. Nothing here chooses a key or a nonce —
//! that discipline belongs to the caller, which is the only party that knows
//! whether its key is fresh per run.

use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha12;

/// Length, in bytes, of a `ChaCha12` key.
pub const STREAM_KEY_LEN: usize = 32;

/// Length, in bytes, of a `ChaCha12` nonce.
pub const STREAM_NONCE_LEN: usize = 12;

/// A 256-bit `ChaCha12` key as raw bytes.
pub type StreamKey = [u8; STREAM_KEY_LEN];

/// A 96-bit `ChaCha12` nonce as raw bytes.
pub type StreamNonce = [u8; STREAM_NONCE_LEN];

/// Bytes one `(key, nonce)` pair can emit before the 32-bit block counter of
/// the RFC 8439 layout would wrap: `2^32` blocks of 64 bytes.
pub const CHACHA12_MAX_KEYSTREAM_BYTES: u64 = 64 << 32;

/// Write `STREAM_KEY_LEN + N` contiguous `ChaCha12` keystream bytes under
/// `(key, nonce)`: the first [`STREAM_KEY_LEN`] into `prefix`, the following
/// `N` into `body`.
///
/// The run is split across two destinations so a caller that consumes the
/// head of its own keystream as a replacement key needs no scratch buffer
/// spanning the whole run — and therefore has no scratch copy of the output
/// to wipe afterwards. `N` is a const parameter so the total run is checked
/// against [`CHACHA12_MAX_KEYSTREAM_BYTES`] at compile time and the counter
/// can never wrap, which is what keeps this infallible.
pub fn chacha12_keystream<const N: usize>(
    key: &StreamKey,
    nonce: &StreamNonce,
    prefix: &mut StreamKey,
    body: &mut [u8; N],
) {
    const {
        assert!(
            (N as u64) <= CHACHA12_MAX_KEYSTREAM_BYTES - STREAM_KEY_LEN as u64,
            "a keystream run must fit one (key, nonce) pair's block counter"
        );
    }
    let mut cipher = ChaCha12::new(key.into(), nonce.into());
    // The upstream primitive XORs its keystream into the destination, so a
    // zeroed destination receives the keystream itself.
    prefix.fill(0);
    cipher.apply_keystream(prefix);
    body.fill(0);
    cipher.apply_keystream(body);
}

#[cfg(test)]
mod tests {
    use super::{chacha12_keystream, StreamKey, StreamNonce, STREAM_KEY_LEN};

    /// RFC 8439 §2.4.2's test-vector key and nonce.
    const KEY: StreamKey = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const NONCE: StreamNonce = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4a, 0x00, 0x00, 0x00, 0x00,
    ];

    /// The first 96 bytes of `ChaCha12` keystream under [`KEY`]/[`NONCE`] from
    /// block counter 0, computed from the RFC 8439 round function reduced to
    /// twelve rounds — independently of the upstream crate, so this pins both
    /// the round count and the split point rather than restating whatever the
    /// dependency happens to produce.
    const KEYSTREAM_96: [u8; 96] = [
        0x63, 0x1c, 0x0c, 0xea, 0xad, 0x4a, 0x39, 0x3c, 0x07, 0x0e, 0xd7, 0x0c, 0xa8, 0x05, 0x40,
        0x9e, 0x22, 0xaa, 0x63, 0xe5, 0x16, 0xc2, 0x6b, 0x9f, 0xd8, 0xf7, 0x70, 0xd1, 0xd5, 0x83,
        0x56, 0x63, 0x7f, 0x66, 0xba, 0xfb, 0x59, 0x5d, 0xdd, 0xa4, 0xc5, 0x16, 0x74, 0x2e, 0x0d,
        0xbc, 0xca, 0x80, 0xf6, 0x14, 0x84, 0x12, 0xa7, 0xf9, 0x41, 0x30, 0xc9, 0x90, 0x83, 0x7f,
        0x9d, 0x82, 0xab, 0xee, 0xc1, 0x26, 0x86, 0x3f, 0x95, 0x77, 0x55, 0x93, 0x08, 0x79, 0x6f,
        0xf8, 0x1a, 0x44, 0x65, 0x5b, 0xd3, 0x52, 0x63, 0x0c, 0x35, 0xbd, 0x4b, 0xec, 0xcb, 0xad,
        0x4b, 0x6f, 0xdd, 0x7b, 0x60, 0x8f,
    ];

    #[test]
    fn the_run_matches_the_reference_keystream_across_the_split() {
        // The prefix must be keystream bytes 0..32 and the body 32..96: a
        // wrapper that restarted the cipher, skipped the prefix, or ran
        // twenty rounds would disagree here.
        let (mut prefix, mut body) = ([0u8; STREAM_KEY_LEN], [0u8; 64]);
        chacha12_keystream(&KEY, &NONCE, &mut prefix, &mut body);
        assert_eq!(prefix, KEYSTREAM_96[..STREAM_KEY_LEN]);
        assert_eq!(body, KEYSTREAM_96[STREAM_KEY_LEN..]);
    }

    #[test]
    fn a_longer_body_extends_the_same_run() {
        let (mut prefix, mut short) = ([0u8; STREAM_KEY_LEN], [0u8; 32]);
        chacha12_keystream(&KEY, &NONCE, &mut prefix, &mut short);
        let (mut long_prefix, mut long) = ([0u8; STREAM_KEY_LEN], [0u8; 64]);
        chacha12_keystream(&KEY, &NONCE, &mut long_prefix, &mut long);
        assert_eq!(long_prefix, prefix);
        assert_eq!(long[..32], short[..]);
    }

    #[test]
    fn a_destination_is_overwritten_not_xored() {
        // The caller receives keystream, not keystream XOR whatever was
        // there: a dirty destination must give the same bytes as a clean one.
        let (mut prefix, mut body) = ([0xffu8; STREAM_KEY_LEN], [0xffu8; 48]);
        chacha12_keystream(&KEY, &NONCE, &mut prefix, &mut body);
        let (mut clean_prefix, mut clean_body) = ([0u8; STREAM_KEY_LEN], [0u8; 48]);
        chacha12_keystream(&KEY, &NONCE, &mut clean_prefix, &mut clean_body);
        assert_eq!(prefix, clean_prefix);
        assert_eq!(body, clean_body);
    }

    #[test]
    fn a_different_key_or_nonce_gives_a_different_run() {
        let (mut prefix, mut body) = ([0u8; STREAM_KEY_LEN], [0u8; 64]);
        chacha12_keystream(&KEY, &NONCE, &mut prefix, &mut body);

        let mut other_key = KEY;
        other_key[0] ^= 1;
        let (mut p2, mut b2) = ([0u8; STREAM_KEY_LEN], [0u8; 64]);
        chacha12_keystream(&other_key, &NONCE, &mut p2, &mut b2);
        assert_ne!(prefix, p2);
        assert_ne!(body, b2);

        let mut other_nonce = NONCE;
        other_nonce[0] ^= 1;
        let (mut p3, mut b3) = ([0u8; STREAM_KEY_LEN], [0u8; 64]);
        chacha12_keystream(&KEY, &other_nonce, &mut p3, &mut b3);
        assert_ne!(prefix, p3);
        assert_ne!(body, b3);
    }

    #[test]
    fn an_empty_body_still_yields_the_key_prefix() {
        let (mut prefix, mut body) = ([0u8; STREAM_KEY_LEN], [0u8; 0]);
        chacha12_keystream(&KEY, &NONCE, &mut prefix, &mut body);
        assert_ne!(prefix, [0u8; STREAM_KEY_LEN]);
    }

    #[test]
    fn the_keystream_is_not_the_key() {
        // A wrapper that forgot to run the cipher would hand the key back.
        let (mut prefix, mut body) = ([0u8; STREAM_KEY_LEN], [0u8; 32]);
        chacha12_keystream(&KEY, &NONCE, &mut prefix, &mut body);
        assert_ne!(prefix, KEY);
        assert_ne!(body, KEY);
    }
}
