//! Unit tests for the AES-CTR wrappers.
//!
//! The known answers are NIST SP 800-38A §F.5's CTR-AES{128,192,256}
//! vectors: the same key, initial counter block, and four-block plaintext
//! for each key size. They were produced by an implementation outside this
//! tree (OpenSSL) and the AES-128 answer matches the published F.5.1
//! ciphertext, so the oracle is anchored to the standard rather than to one
//! library's behaviour.

use super::{
    Aes128Ctr, Aes128Key, Aes192Ctr, Aes192Key, Aes256Ctr, Aes256Key, AesCtrIv, AES128_KEY_LEN,
    AES192_KEY_LEN, AES256_KEY_LEN, AES_BLOCK_LEN,
};

/// SP 800-38A §F.5: the initial counter block shared by all three sizes.
const IV: AesCtrIv = [
    0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff,
];

/// SP 800-38A §F.5: the four-block plaintext shared by all three sizes.
const PLAINTEXT: [u8; 64] = [
    0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17, 0x2a,
    0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac, 0x45, 0xaf, 0x8e, 0x51,
    0x30, 0xc8, 0x1c, 0x46, 0xa3, 0x5c, 0xe4, 0x11, 0xe5, 0xfb, 0xc1, 0x19, 0x1a, 0x0a, 0x52, 0xef,
    0xf6, 0x9f, 0x24, 0x45, 0xdf, 0x4f, 0x9b, 0x17, 0xad, 0x2b, 0x41, 0x7b, 0xe6, 0x6c, 0x37, 0x10,
];

/// SP 800-38A §F.5: the CTR-AES128 key.
const KEY128: Aes128Key = [
    0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c,
];

/// SP 800-38A §F.5: the CTR-AES128 ciphertext.
const CIPHERTEXT128: [u8; 64] = [
    0x87, 0x4d, 0x61, 0x91, 0xb6, 0x20, 0xe3, 0x26, 0x1b, 0xef, 0x68, 0x64, 0x99, 0x0d, 0xb6, 0xce,
    0x98, 0x06, 0xf6, 0x6b, 0x79, 0x70, 0xfd, 0xff, 0x86, 0x17, 0x18, 0x7b, 0xb9, 0xff, 0xfd, 0xff,
    0x5a, 0xe4, 0xdf, 0x3e, 0xdb, 0xd5, 0xd3, 0x5e, 0x5b, 0x4f, 0x09, 0x02, 0x0d, 0xb0, 0x3e, 0xab,
    0x1e, 0x03, 0x1d, 0xda, 0x2f, 0xbe, 0x03, 0xd1, 0x79, 0x21, 0x70, 0xa0, 0xf3, 0x00, 0x9c, 0xee,
];

/// SP 800-38A §F.5: the CTR-AES192 key.
const KEY192: Aes192Key = [
    0x8e, 0x73, 0xb0, 0xf7, 0xda, 0x0e, 0x64, 0x52, 0xc8, 0x10, 0xf3, 0x2b, 0x80, 0x90, 0x79, 0xe5,
    0x62, 0xf8, 0xea, 0xd2, 0x52, 0x2c, 0x6b, 0x7b,
];

/// SP 800-38A §F.5: the CTR-AES192 ciphertext.
const CIPHERTEXT192: [u8; 64] = [
    0x1a, 0xbc, 0x93, 0x24, 0x17, 0x52, 0x1c, 0xa2, 0x4f, 0x2b, 0x04, 0x59, 0xfe, 0x7e, 0x6e, 0x0b,
    0x09, 0x03, 0x39, 0xec, 0x0a, 0xa6, 0xfa, 0xef, 0xd5, 0xcc, 0xc2, 0xc6, 0xf4, 0xce, 0x8e, 0x94,
    0x1e, 0x36, 0xb2, 0x6b, 0xd1, 0xeb, 0xc6, 0x70, 0xd1, 0xbd, 0x1d, 0x66, 0x56, 0x20, 0xab, 0xf7,
    0x4f, 0x78, 0xa7, 0xf6, 0xd2, 0x98, 0x09, 0x58, 0x5a, 0x97, 0xda, 0xec, 0x58, 0xc6, 0xb0, 0x50,
];

/// SP 800-38A §F.5: the CTR-AES256 key.
const KEY256: Aes256Key = [
    0x60, 0x3d, 0xeb, 0x10, 0x15, 0xca, 0x71, 0xbe, 0x2b, 0x73, 0xae, 0xf0, 0x85, 0x7d, 0x77, 0x81,
    0x1f, 0x35, 0x2c, 0x07, 0x3b, 0x61, 0x08, 0xd7, 0x2d, 0x98, 0x10, 0xa3, 0x09, 0x14, 0xdf, 0xf4,
];

/// SP 800-38A §F.5: the CTR-AES256 ciphertext.
const CIPHERTEXT256: [u8; 64] = [
    0x60, 0x1e, 0xc3, 0x13, 0x77, 0x57, 0x89, 0xa5, 0xb7, 0xa7, 0xf5, 0x04, 0xbb, 0xf3, 0xd2, 0x28,
    0xf4, 0x43, 0xe3, 0xca, 0x4d, 0x62, 0xb5, 0x9a, 0xca, 0x84, 0xe9, 0x90, 0xca, 0xca, 0xf5, 0xc5,
    0x2b, 0x09, 0x30, 0xda, 0xa2, 0x3d, 0xe9, 0x4c, 0xe8, 0x70, 0x17, 0xba, 0x2d, 0x84, 0x98, 0x8d,
    0xdf, 0xc9, 0xc5, 0x8d, 0xb6, 0x7a, 0xad, 0xa6, 0x13, 0xc2, 0xdd, 0x08, 0x45, 0x79, 0x41, 0xa6,
];

#[test]
fn ctr_matches_the_nist_vectors() {
    let mut buf = PLAINTEXT;
    Aes128Ctr::new(&KEY128, &IV).apply(&mut buf).expect("fits");
    assert_eq!(buf, CIPHERTEXT128);

    let mut buf = PLAINTEXT;
    Aes192Ctr::new(&KEY192, &IV).apply(&mut buf).expect("fits");
    assert_eq!(buf, CIPHERTEXT192);

    let mut buf = PLAINTEXT;
    Aes256Ctr::new(&KEY256, &IV).apply(&mut buf).expect("fits");
    assert_eq!(buf, CIPHERTEXT256);
}

#[test]
fn ctr_decrypts_by_applying_the_same_keystream() {
    let mut buf = CIPHERTEXT256;
    Aes256Ctr::new(&KEY256, &IV).apply(&mut buf).expect("fits");
    assert_eq!(buf, PLAINTEXT);
}

/// The cipher is stateful on purpose: a message split across calls must give
/// the same ciphertext as one call over the whole of it, including at splits
/// that fall inside a block. A wrapper that restarted the counter per call
/// would reuse keystream and fail here.
#[test]
fn successive_calls_continue_one_keystream() {
    for split in [0usize, 1, 15, 16, 17, 31, 32, 63, 64] {
        let mut cipher = Aes256Ctr::new(&KEY256, &IV);
        let mut buf = PLAINTEXT;
        let (head, tail) = buf.split_at_mut(split);
        cipher.apply(head).expect("fits");
        cipher.apply(tail).expect("fits");
        assert_eq!(buf, CIPHERTEXT256, "split at {split}");
    }
}

/// A fresh cipher restarts the keystream, which is exactly why one object
/// must live for the whole connection rather than being made per packet.
#[test]
fn a_fresh_cipher_restarts_the_keystream() {
    let mut first = [0u8; 16];
    Aes256Ctr::new(&KEY256, &IV)
        .apply(&mut first)
        .expect("fits");

    let mut continued = [0u8; 32];
    Aes256Ctr::new(&KEY256, &IV)
        .apply(&mut continued)
        .expect("fits");
    assert_eq!(first, continued[..16]);
    assert_ne!(first, continued[16..]);
}

#[test]
fn a_different_key_or_iv_gives_a_different_keystream() {
    let mut base = [0u8; 32];
    Aes256Ctr::new(&KEY256, &IV).apply(&mut base).expect("fits");

    let mut other_key = KEY256;
    other_key[0] ^= 1;
    let mut under_other = [0u8; 32];
    Aes256Ctr::new(&other_key, &IV)
        .apply(&mut under_other)
        .expect("fits");
    assert_ne!(base, under_other);

    let mut other_iv = IV;
    other_iv[15] ^= 1;
    let mut at_other_iv = [0u8; 32];
    Aes256Ctr::new(&KEY256, &other_iv)
        .apply(&mut at_other_iv)
        .expect("fits");
    assert_ne!(base, at_other_iv);
}

/// CTR is a stream mode, so a message shorter than the block is encrypted
/// without padding and the counter advances by the partial block.
#[test]
fn a_partial_block_needs_no_padding() {
    let mut buf = [0xa5u8; 5];
    Aes128Ctr::new(&KEY128, &IV).apply(&mut buf).expect("fits");
    assert_eq!(buf.len(), 5);
    assert_ne!(buf, [0xa5u8; 5]);
}

#[test]
fn key_and_block_widths_are_the_declared_sizes() {
    assert_eq!(AES_BLOCK_LEN, 16);
    assert_eq!(AES128_KEY_LEN, 16);
    assert_eq!(AES192_KEY_LEN, 24);
    assert_eq!(AES256_KEY_LEN, 32);
}

/// Neither the key schedule nor the counter position may reach a log.
#[test]
fn debug_does_not_leak_cipher_state() {
    use core::fmt::Write as _;

    struct Sink {
        data: [u8; 128],
        len: usize,
    }
    impl core::fmt::Write for Sink {
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

    let cipher = Aes256Ctr::new(&KEY256, &IV);
    let mut sink = Sink {
        data: [0; 128],
        len: 0,
    };
    write!(&mut sink, "{cipher:?}").expect("fits");
    let rendered = core::str::from_utf8(&sink.data[..sink.len]).expect("ascii");
    assert!(rendered.contains("Aes256Ctr"));
    assert!(!rendered.contains("60"), "key bytes must not be rendered");
}
