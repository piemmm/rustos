//! Stream ciphers: `ChaCha12` keystream and 64-bit-nonce `ChaCha20`.
//!
//! `ChaCha12` — the twelve-round reduced variant of `ChaCha20` (RFC 8439) —
//! is the construction OpenBSD's `arc4random` and Linux's `get_random_u64`
//! expand their fast random output from. It backs `lib/rng`'s fast
//! generator; long-lived key material stays on the SP 800-90A DRBG there.
//!
//! The 64-bit-nonce `ChaCha20` below is djb's original construction, which
//! `chacha20-poly1305@openssh.com` uses: the nonce is the packet sequence
//! number and the block counter is chosen per call, because that cipher
//! takes its Poly1305 key from block 0 and encrypts the payload from block 1
//! under the same nonce. RFC 8439's 96-bit-nonce layout cannot express that,
//! so both are exposed here rather than one being bent into the other.
//!
//! As with the rest of this crate the wrappers are *narrower* than the
//! upstream crate: callers hand in fixed-size byte arrays and never see the
//! upstream `cipher` traits or `GenericArray`. Nothing here chooses a key or
//! a nonce — that discipline belongs to the caller, which is the only party
//! that knows whether its key is fresh per run.

use core::fmt;

use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::{ChaCha12, ChaCha20Legacy};

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

/// `ChaCha` block size in bytes: the granularity the block counter steps in.
const BLOCK_LEN: usize = 64;

/// Blocks one 64-bit-nonce `ChaCha20` `(key, nonce)` pair can emit.
///
/// djb's original construction counts in 64 bits, so one nonce covers the
/// whole address space and then some: no buffer a caller can hold reaches
/// the bound from block zero. Starting high enough still can, which is why
/// the run is checked rather than assumed.
pub const CHACHA20_MAX_KEYSTREAM_BLOCKS: u64 = u64::MAX;

/// Length, in bytes, of a `ChaCha20` key.
pub const CHACHA20_KEY_LEN: usize = 32;

/// Length, in bytes, of the 64-bit `ChaCha20` nonce of djb's original
/// construction. `chacha20-poly1305@openssh.com` puts the packet sequence
/// number here.
pub const CHACHA20_NONCE_LEN: usize = 8;

/// A 256-bit `ChaCha20` key as raw bytes.
pub type ChaCha20Key = [u8; CHACHA20_KEY_LEN];

/// A 64-bit `ChaCha20` nonce as raw bytes.
pub type ChaCha20Nonce = [u8; CHACHA20_NONCE_LEN];

/// A `ChaCha20` run did not fit one `(key, nonce)` pair's block counter.
///
/// A run that would carry past [`CHACHA20_MAX_KEYSTREAM_BLOCKS`] is refused
/// rather than silently restarting the keystream, which would reuse it. SSH
/// is nowhere near the bound — one packet is at most a few hundred
/// kibibytes under a nonce that is the packet's own sequence number — so the
/// error marks a caller misusing the primitive, never a working protocol.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct StreamError(());

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("chacha20 run exceeds one nonce's block counter")
    }
}

/// XOR the `ChaCha20` keystream under `(key, nonce)`, starting at block
/// `counter`, into `buffer`.
///
/// This is djb's original construction — 64-bit nonce, 64-bit counter — not
/// RFC 8439's. `counter` is explicit because
/// `chacha20-poly1305@openssh.com` derives its Poly1305 key from block 0 and
/// encrypts the payload from block 1 under one nonce.
///
/// # Errors
///
/// Returns [`StreamError`] if the run would carry the block counter past
/// [`CHACHA20_MAX_KEYSTREAM_BLOCKS`], which would restart — and so reuse —
/// the keystream.
pub fn chacha20_apply(
    key: &ChaCha20Key,
    nonce: &ChaCha20Nonce,
    counter: u64,
    buffer: &mut [u8],
) -> Result<(), StreamError> {
    let mut cipher = ChaCha20Legacy::new(key.into(), nonce.into());
    cipher
        .try_seek(u128::from(counter) * BLOCK_LEN as u128)
        .map_err(|_| StreamError(()))?;
    cipher
        .try_apply_keystream(buffer)
        .map_err(|_| StreamError(()))
}

/// Write the `ChaCha20` keystream under `(key, nonce)`, starting at block
/// `counter`, into `out`.
///
/// The keystream itself rather than a ciphertext: this is how
/// `chacha20-poly1305@openssh.com` obtains its per-packet Poly1305 key from
/// block 0. Equivalent to [`chacha20_apply`] over a zeroed buffer, so the
/// two can never disagree.
///
/// # Errors
///
/// As [`chacha20_apply`].
pub fn chacha20_keystream(
    key: &ChaCha20Key,
    nonce: &ChaCha20Nonce,
    counter: u64,
    out: &mut [u8],
) -> Result<(), StreamError> {
    out.fill(0);
    chacha20_apply(key, nonce, counter, out)
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
