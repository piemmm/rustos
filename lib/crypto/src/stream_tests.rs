//! Unit tests for the stream-cipher wrappers.
//!
//! Both keystreams are pinned against a reference round function computed
//! outside this tree, so a dependency that changed its round count, block
//! layout, or counter placement would fail here rather than silently
//! producing a different cipher.

use super::{
    chacha12_keystream, chacha20_apply, chacha20_keystream, ChaCha20Key, ChaCha20Nonce, StreamKey,
    StreamNonce, STREAM_KEY_LEN,
};

/// RFC 8439 §2.4.2's test-vector key and nonce.
const KEY: StreamKey = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
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
    0x63, 0x1c, 0x0c, 0xea, 0xad, 0x4a, 0x39, 0x3c, 0x07, 0x0e, 0xd7, 0x0c, 0xa8, 0x05, 0x40, 0x9e,
    0x22, 0xaa, 0x63, 0xe5, 0x16, 0xc2, 0x6b, 0x9f, 0xd8, 0xf7, 0x70, 0xd1, 0xd5, 0x83, 0x56, 0x63,
    0x7f, 0x66, 0xba, 0xfb, 0x59, 0x5d, 0xdd, 0xa4, 0xc5, 0x16, 0x74, 0x2e, 0x0d, 0xbc, 0xca, 0x80,
    0xf6, 0x14, 0x84, 0x12, 0xa7, 0xf9, 0x41, 0x30, 0xc9, 0x90, 0x83, 0x7f, 0x9d, 0x82, 0xab, 0xee,
    0xc1, 0x26, 0x86, 0x3f, 0x95, 0x77, 0x55, 0x93, 0x08, 0x79, 0x6f, 0xf8, 0x1a, 0x44, 0x65, 0x5b,
    0xd3, 0x52, 0x63, 0x0c, 0x35, 0xbd, 0x4b, 0xec, 0xcb, 0xad, 0x4b, 0x6f, 0xdd, 0x7b, 0x60, 0x8f,
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

/// Key and nonce for the 64-bit-nonce `ChaCha20` vectors below.
const C20_KEY: ChaCha20Key = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const C20_NONCE: ChaCha20Nonce = [0x03, 0x02, 0x01, 0x04, 0x06, 0x05, 0x08, 0x07];

/// Block 0 of the `ChaCha20` keystream under [`C20_KEY`]/[`C20_NONCE`],
/// computed from djb's original round function with the 64-bit counter at
/// state words 12-13 and the nonce at 14-15 — independently of the upstream
/// crate, so this pins the layout rather than restating whatever the
/// dependency produces. The same reference reproduces the published
/// all-zero-key `ChaCha20` keystream, so the layout itself is not guesswork.
const C20_BLOCK0: [u8; 64] = [
    0xbe, 0xb2, 0xeb, 0xc3, 0xb9, 0xdd, 0xa5, 0xc4, 0xf6, 0x30, 0x6e, 0xf4, 0x24, 0xba, 0xa0, 0x2a,
    0x8b, 0x5d, 0x32, 0x4c, 0x1c, 0xa7, 0xc7, 0x04, 0x57, 0x20, 0x29, 0xda, 0x99, 0x4e, 0x57, 0xbf,
    0xa2, 0xd3, 0xe1, 0xc1, 0x6c, 0xc1, 0x0b, 0x13, 0x08, 0x36, 0x85, 0x4b, 0x84, 0x78, 0x07, 0x3e,
    0x28, 0x97, 0xd7, 0xe4, 0xc3, 0x30, 0xa3, 0xac, 0x67, 0xcc, 0x81, 0x30, 0x77, 0x1b, 0x28, 0x45,
];

/// Blocks 1..=2 of the same keystream: what
/// `chacha20-poly1305@openssh.com` encrypts a payload with after taking its
/// Poly1305 key from block 0.
const C20_FROM_BLOCK1: [u8; 96] = [
    0xa5, 0xa8, 0x09, 0x3c, 0x7f, 0x58, 0xdc, 0x2d, 0xac, 0xc3, 0xba, 0x8d, 0x27, 0xac, 0x2b, 0x8e,
    0x16, 0xd9, 0xf1, 0x96, 0x83, 0x7c, 0xa8, 0x98, 0x36, 0x0d, 0x9c, 0x13, 0x62, 0xc6, 0xab, 0xdc,
    0x05, 0xd9, 0x79, 0xdb, 0xc7, 0x57, 0x4b, 0xe0, 0x81, 0xdd, 0x8e, 0x85, 0x8e, 0x08, 0x5f, 0x47,
    0x97, 0x39, 0x1f, 0x04, 0xbd, 0x92, 0x35, 0xf9, 0xec, 0x1d, 0x19, 0x9b, 0x0a, 0x58, 0x65, 0xa2,
    0xd1, 0x4e, 0xa5, 0x89, 0x2f, 0x3b, 0x6b, 0xd8, 0x89, 0x63, 0x22, 0xb9, 0x60, 0x42, 0x53, 0x83,
    0x28, 0x9b, 0x3a, 0x0d, 0x1a, 0x6f, 0x82, 0xe7, 0x48, 0xa0, 0xdc, 0xcf, 0x4a, 0xf5, 0x1d, 0x27,
];

#[test]
fn chacha20_keystream_matches_the_reference_layout() {
    let mut out = [0u8; 64];
    chacha20_keystream(&C20_KEY, &C20_NONCE, 0, &mut out).expect("run fits the counter");
    assert_eq!(out, C20_BLOCK0);

    let mut from_one = [0u8; 96];
    chacha20_keystream(&C20_KEY, &C20_NONCE, 1, &mut from_one).expect("run fits the counter");
    assert_eq!(from_one, C20_FROM_BLOCK1);
}

#[test]
fn chacha20_apply_xors_and_round_trips() {
    let plaintext = *b"thirty bytes of plaintext here";
    let mut buf = plaintext;
    chacha20_apply(&C20_KEY, &C20_NONCE, 1, &mut buf).expect("run fits the counter");

    for (i, (c, p)) in buf.iter().zip(plaintext.iter()).enumerate() {
        assert_eq!(*c, p ^ C20_FROM_BLOCK1[i], "byte {i}");
    }

    chacha20_apply(&C20_KEY, &C20_NONCE, 1, &mut buf).expect("run fits the counter");
    assert_eq!(buf, plaintext);
}

#[test]
fn chacha20_counters_and_nonces_select_different_keystreams() {
    let mut at_zero = [0u8; 32];
    let mut at_one = [0u8; 32];
    chacha20_keystream(&C20_KEY, &C20_NONCE, 0, &mut at_zero).expect("fits");
    chacha20_keystream(&C20_KEY, &C20_NONCE, 1, &mut at_one).expect("fits");
    assert_ne!(at_zero, at_one);

    let mut under_other = [0u8; 32];
    let mut other_nonce = C20_NONCE;
    other_nonce[0] ^= 1;
    chacha20_keystream(&C20_KEY, &other_nonce, 0, &mut under_other).expect("fits");
    assert_ne!(at_zero, under_other);

    let mut other_key = C20_KEY;
    other_key[31] ^= 1;
    chacha20_keystream(&other_key, &C20_NONCE, 0, &mut under_other).expect("fits");
    assert_ne!(at_zero, under_other);
}

/// A run that would carry the 64-bit block counter past its last usable
/// block is refused, not silently wrapped onto keystream the same nonce
/// already emitted.
#[test]
fn chacha20_refuses_a_run_that_would_wrap_the_counter() {
    let mut one_block = [0u8; 64];
    assert!(chacha20_keystream(&C20_KEY, &C20_NONCE, u64::MAX - 1, &mut one_block).is_ok());

    let mut two_blocks = [0u8; 128];
    assert!(chacha20_keystream(&C20_KEY, &C20_NONCE, u64::MAX - 1, &mut two_blocks).is_err());
    assert!(chacha20_keystream(&C20_KEY, &C20_NONCE, u64::MAX, &mut one_block).is_err());
}

/// A counter above `2^32` is reachable — the original construction counts
/// in 64 bits, where RFC 8439's layout would have wrapped.
#[test]
fn chacha20_counts_beyond_thirty_two_bits() {
    let mut low = [0u8; 32];
    let mut high = [0u8; 32];
    chacha20_keystream(&C20_KEY, &C20_NONCE, 1, &mut low).expect("fits");
    chacha20_keystream(&C20_KEY, &C20_NONCE, 1 << 32, &mut high).expect("fits");
    assert_ne!(low, high);
}

#[test]
fn chacha20_keystream_overwrites_rather_than_xors() {
    let mut dirty = [0xffu8; 64];
    chacha20_keystream(&C20_KEY, &C20_NONCE, 0, &mut dirty).expect("fits");
    assert_eq!(dirty, C20_BLOCK0);
}

#[test]
fn stream_error_display_is_terse_ascii() {
    use core::fmt::Write as _;
    let mut sink = FmtSink::<64> {
        data: [0; 64],
        len: 0,
    };
    let refused = chacha20_keystream(&C20_KEY, &C20_NONCE, u64::MAX, &mut [0u8; 128])
        .expect_err("a wrapping run is refused");
    write!(&mut sink, "{refused}").expect("fits");
    assert!(sink.len > 0);
    assert!(sink.data[..sink.len].iter().all(u8::is_ascii));
}

/// Fixed-size `core::fmt::Write` sink so the Display test needs no allocator.
struct FmtSink<const N: usize> {
    data: [u8; N],
    len: usize,
}

impl<const N: usize> core::fmt::Write for FmtSink<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        if end > self.data.len() {
            return Err(core::fmt::Error);
        }
        self.data[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
