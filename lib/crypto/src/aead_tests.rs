//! Unit tests for the AEAD wrappers.
//!
//! The AES-GCM known answers come from the Wycheproof project's
//! `aes_gcm_test.json`, an implementation-independent vector set assembled
//! specifically to catch the edge cases a self-consistent round-trip test
//! agrees with itself about. Each vector's identifier is recorded so it can
//! be looked up.

use super::{
    aes128gcm_open, aes128gcm_seal, aes256gcm_open, aes256gcm_seal, open, seal, AeadError, AeadKey,
    AeadNonce, AeadTag, Aes128Gcm, Aes128GcmKey, Aes256Gcm, Aes256GcmKey, AesGcmNonce, AesGcmTag,
    AES128_GCM_KEY_LEN, AES256_GCM_KEY_LEN, AES_GCM_NONCE_LEN, AES_GCM_TAG_LEN,
};

extern crate alloc;
use alloc::vec::Vec;

const KEY: AeadKey = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
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
        0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e,
        0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d,
        0x9e, 0x9f,
    ];
    let nonce: AeadNonce = [
        0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
    ];
    let aad = [
        0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
    ];
    let mut buf = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.".to_vec();
    let expected_tag: AeadTag = [
        0x1a, 0xe1, 0x0b, 0x59, 0x4f, 0x09, 0xe2, 0x6a, 0x7e, 0x90, 0x2e, 0xcb, 0xd0, 0x60, 0x06,
        0x91,
    ];
    let tag = seal(&key, &nonce, &aad, &mut buf).expect("seal");
    assert_eq!(tag, expected_tag, "tag must match the RFC 8439 vector");
    open(&key, &nonce, &aad, &mut buf, &tag).expect("open");
    assert_eq!(&buf[..6], b"Ladies");
}

/// Wycheproof `aes_gcm_test.json` tcId 2: the AES-128-GCM key.
const GCM128_KEY: Aes128GcmKey = [
    0x5b, 0x96, 0x04, 0xfe, 0x14, 0xea, 0xdb, 0xa9, 0x31, 0xb0, 0xcc, 0xf3, 0x48, 0x43, 0xda, 0xb9,
];

/// Wycheproof tcId 2: the 96-bit nonce.
const GCM128_NONCE: AesGcmNonce = [
    0x92, 0x1d, 0x25, 0x07, 0xfa, 0x80, 0x07, 0xb7, 0xbd, 0x06, 0x7d, 0x34,
];

/// Wycheproof tcId 2: the associated data.
const GCM128_AAD: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

/// Wycheproof tcId 2: the plaintext.
const GCM128_PLAINTEXT: [u8; 16] = [
    0x00, 0x1d, 0x0c, 0x23, 0x12, 0x87, 0xc1, 0x18, 0x27, 0x84, 0x55, 0x4c, 0xa3, 0xa2, 0x19, 0x08,
];

/// Wycheproof tcId 2: the expected ciphertext.
const GCM128_CIPHERTEXT: [u8; 16] = [
    0x49, 0xd8, 0xb9, 0x78, 0x3e, 0x91, 0x19, 0x13, 0xd8, 0x70, 0x94, 0xd1, 0xf6, 0x3c, 0xc7, 0x65,
];

/// Wycheproof tcId 2: the expected tag.
const GCM128_TAG: AesGcmTag = [
    0x1e, 0x34, 0x8b, 0xa0, 0x7c, 0xca, 0x2c, 0xf0, 0x4c, 0x61, 0x8c, 0xb4, 0xd4, 0x3a, 0x5b, 0x92,
];

/// Wycheproof `aes_gcm_test.json` tcId 91: the AES-256-GCM key.
const GCM256_KEY: Aes256GcmKey = [
    0x92, 0xac, 0xe3, 0xe3, 0x48, 0xcd, 0x82, 0x10, 0x92, 0xcd, 0x92, 0x1a, 0xa3, 0x54, 0x63, 0x74,
    0x29, 0x9a, 0xb4, 0x62, 0x09, 0x69, 0x1b, 0xc2, 0x8b, 0x87, 0x52, 0xd1, 0x7f, 0x12, 0x3c, 0x20,
];

/// Wycheproof tcId 91: the 96-bit nonce.
const GCM256_NONCE: AesGcmNonce = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
];

/// Wycheproof tcId 91: the associated data.
const GCM256_AAD: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff];

/// Wycheproof tcId 91: the plaintext.
const GCM256_PLAINTEXT: [u8; 10] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];

/// Wycheproof tcId 91: the expected ciphertext.
const GCM256_CIPHERTEXT: [u8; 10] = [0xe2, 0x7a, 0xbd, 0xd2, 0xd2, 0xa5, 0x3d, 0x2f, 0x13, 0x6b];

/// Wycheproof tcId 91: the expected tag.
const GCM256_TAG: AesGcmTag = [
    0x9a, 0x4a, 0x25, 0x79, 0x52, 0x93, 0x01, 0xbc, 0xfb, 0x71, 0xc7, 0x8d, 0x40, 0x60, 0xf5, 0x2c,
];

#[test]
fn aes_gcm_matches_the_wycheproof_vectors() {
    let mut buf = GCM128_PLAINTEXT;
    let tag = aes128gcm_seal(&GCM128_KEY, &GCM128_NONCE, &GCM128_AAD, &mut buf).expect("seal");
    assert_eq!(buf, GCM128_CIPHERTEXT);
    assert_eq!(tag, GCM128_TAG);

    let mut buf = GCM256_PLAINTEXT;
    let tag = aes256gcm_seal(&GCM256_KEY, &GCM256_NONCE, &GCM256_AAD, &mut buf).expect("seal");
    assert_eq!(buf, GCM256_CIPHERTEXT);
    assert_eq!(tag, GCM256_TAG);
}

#[test]
fn a_keyed_aes_gcm_matches_the_wycheproof_vectors() {
    let cipher = Aes128Gcm::new(&GCM128_KEY);
    let mut buf = GCM128_PLAINTEXT;
    let tag = cipher
        .seal(&GCM128_NONCE, &GCM128_AAD, &mut buf)
        .expect("seal");
    assert_eq!((buf, tag), (GCM128_CIPHERTEXT, GCM128_TAG));
    cipher
        .open(&GCM128_NONCE, &GCM128_AAD, &mut buf, &tag)
        .expect("open");
    assert_eq!(buf, GCM128_PLAINTEXT);

    let cipher = Aes256Gcm::new(&GCM256_KEY);
    let mut buf = GCM256_PLAINTEXT;
    let tag = cipher
        .seal(&GCM256_NONCE, &GCM256_AAD, &mut buf)
        .expect("seal");
    assert_eq!((buf, tag), (GCM256_CIPHERTEXT, GCM256_TAG));
    cipher
        .open(&GCM256_NONCE, &GCM256_AAD, &mut buf, &tag)
        .expect("open");
    assert_eq!(buf, GCM256_PLAINTEXT);
}

#[test]
fn one_keyed_aes_gcm_serves_many_messages_as_the_one_shots_would() {
    let key128: Aes128GcmKey = core::array::from_fn(|at| u8::try_from(at * 3).expect("small"));
    let key256: Aes256GcmKey =
        core::array::from_fn(|at| u8::try_from(at * 5 % 251).expect("small"));
    let (short, long) = (Aes128Gcm::new(&key128), Aes256Gcm::new(&key256));
    for counter in 0..64u8 {
        let nonce: AesGcmNonce =
            core::array::from_fn(|at| counter ^ u8::try_from(at).expect("small"));
        let message: Vec<u8> = (0..usize::from(counter) * 7)
            .map(|at| at.to_le_bytes()[0])
            .collect();
        let (mut a, mut b) = (message.clone(), message.clone());
        let keyed = short.seal(&nonce, b"aad", &mut a).expect("seal");
        let one_shot = aes128gcm_seal(&key128, &nonce, b"aad", &mut b).expect("seal");
        assert_eq!((&a, keyed), (&b, one_shot));
        aes128gcm_open(&key128, &nonce, b"aad", &mut a, &keyed).expect("the one-shot opens it");
        assert_eq!(a, message);
        let (mut a, mut b) = (message.clone(), message.clone());
        let keyed = long.seal(&nonce, b"aad", &mut a).expect("seal");
        let one_shot = aes256gcm_seal(&key256, &nonce, b"aad", &mut b).expect("seal");
        assert_eq!((&a, keyed), (&b, one_shot));
        assert_eq!(
            long.open(&nonce, b"other", &mut a, &keyed),
            Err(AeadError::Authentication)
        );
    }
}

#[test]
fn aes_gcm_round_trips() {
    let mut buf = GCM256_CIPHERTEXT;
    aes256gcm_open(
        &GCM256_KEY,
        &GCM256_NONCE,
        &GCM256_AAD,
        &mut buf,
        &GCM256_TAG,
    )
    .expect("open");
    assert_eq!(buf, GCM256_PLAINTEXT);
}

/// Every one of the four inputs is authenticated: altering the ciphertext,
/// the associated data, the nonce, the key, or the tag must be refused.
#[test]
fn aes_gcm_rejects_every_alteration() {
    /// One alteration to make, and the label the failure reports.
    type Alteration = (&'static str, fn() -> Result<(), AeadError>);

    let cases: [Alteration; 5] = [
        ("ciphertext", || {
            let mut buf = GCM256_CIPHERTEXT;
            buf[0] ^= 0x01;
            aes256gcm_open(
                &GCM256_KEY,
                &GCM256_NONCE,
                &GCM256_AAD,
                &mut buf,
                &GCM256_TAG,
            )
        }),
        ("aad", || {
            let mut buf = GCM256_CIPHERTEXT;
            let mut aad = GCM256_AAD;
            aad[0] ^= 0x01;
            aes256gcm_open(&GCM256_KEY, &GCM256_NONCE, &aad, &mut buf, &GCM256_TAG)
        }),
        ("nonce", || {
            let mut buf = GCM256_CIPHERTEXT;
            let mut nonce = GCM256_NONCE;
            nonce[0] ^= 0x01;
            aes256gcm_open(&GCM256_KEY, &nonce, &GCM256_AAD, &mut buf, &GCM256_TAG)
        }),
        ("key", || {
            let mut buf = GCM256_CIPHERTEXT;
            let mut key = GCM256_KEY;
            key[0] ^= 0x01;
            aes256gcm_open(&key, &GCM256_NONCE, &GCM256_AAD, &mut buf, &GCM256_TAG)
        }),
        ("tag", || {
            let mut buf = GCM256_CIPHERTEXT;
            let mut tag = GCM256_TAG;
            tag[0] ^= 0x01;
            aes256gcm_open(&GCM256_KEY, &GCM256_NONCE, &GCM256_AAD, &mut buf, &tag)
        }),
    ];
    for (what, case) in cases {
        assert_eq!(case(), Err(AeadError::Authentication), "altered {what}");
    }
}

/// A rejected message must leave the caller's buffer holding the ciphertext
/// it arrived as, never the decrypted plaintext. Exposing the plaintext on
/// tag-verification failure was CVE-2023-42811, fixed upstream in 0.10.3;
/// this pins the behaviour so a future bump cannot quietly undo it.
#[test]
fn a_rejected_aes_gcm_message_never_exposes_plaintext() {
    let mut buf = GCM256_CIPHERTEXT;
    let mut tag = GCM256_TAG;
    tag[0] ^= 0x01;
    assert_eq!(
        aes256gcm_open(&GCM256_KEY, &GCM256_NONCE, &GCM256_AAD, &mut buf, &tag),
        Err(AeadError::Authentication)
    );
    assert_eq!(buf, GCM256_CIPHERTEXT, "buffer must be left untouched");
    assert_ne!(buf, GCM256_PLAINTEXT, "plaintext must not be exposed");
}

/// An empty message and empty associated data are still authenticated: the
/// tag covers the lengths, so a forged empty message is refused.
#[test]
fn aes_gcm_authenticates_an_empty_message() {
    let tag = aes128gcm_seal(&GCM128_KEY, &GCM128_NONCE, &[], &mut []).expect("seal");
    assert!(aes128gcm_open(&GCM128_KEY, &GCM128_NONCE, &[], &mut [], &tag).is_ok());

    let mut forged = tag;
    forged[0] ^= 0x01;
    assert_eq!(
        aes128gcm_open(&GCM128_KEY, &GCM128_NONCE, &[], &mut [], &forged),
        Err(AeadError::Authentication)
    );
}

#[test]
fn aes_gcm_widths_are_the_declared_sizes() {
    assert_eq!(AES_GCM_NONCE_LEN, 12);
    assert_eq!(AES_GCM_TAG_LEN, 16);
    assert_eq!(AES128_GCM_KEY_LEN, 16);
    assert_eq!(AES256_GCM_KEY_LEN, 32);
}
