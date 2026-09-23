//! Audited cryptographic primitives for TAIRiX.
//!
//! This crate exists for one purpose: to keep cryptography out of the rest
//! of the codebase. No hand-rolled primitives are allowed; everything here
//! is a thin wrapper over a vetted upstream implementation, and **no other
//! crate in the workspace names a cryptographic dependency** — not in a
//! production path, a test, or a build script.
//!
//! Every upstream crate sits on one generation of the `RustCrypto` and
//! dalek-cryptography stacks, so the audit footprint is one set of
//! primitives rather than two of anything.
//!
//! The wrappers intentionally expose a *narrower* API than the upstream
//! crates: callers receive fixed-size byte arrays, not opaque types whose
//! lifetimes they would have to manage. This makes the boundary between
//! `lib/crypto` and the rest of the system straightforward to audit.
//!
//! Nothing here draws randomness. Every secret — a signing seed, an
//! agreement exponent, an encapsulation's `m` — is supplied by the caller,
//! which is the only party that knows whether it must come from the kernel
//! CSPRNG or from a fixture.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod aead;
pub mod agree;
pub mod backend;
pub mod cipher;
pub mod constant_time;
pub mod ffdh;
pub mod hash;
pub mod kdf;
pub mod kem;
pub mod mac;
pub mod nistp;
pub mod sign;
pub mod stream;

pub use aead::{
    aes128gcm_open, aes128gcm_seal, aes256gcm_open, aes256gcm_seal, open, seal, AeadError, AeadKey,
    AeadNonce, AeadTag, Aes128Gcm, Aes128GcmKey, Aes256Gcm, Aes256GcmKey, AesGcmNonce, AesGcmTag,
    AEAD_KEY_LEN, AEAD_NONCE_LEN, AEAD_TAG_LEN, AES128_GCM_KEY_LEN, AES256_GCM_KEY_LEN,
    AES_GCM_NONCE_LEN, AES_GCM_TAG_LEN,
};
pub use agree::{
    KeyAgreementError, X25519PublicKey, X25519SecretKey, X25519SharedSecret, X25519_PUBLIC_KEY_LEN,
    X25519_SECRET_LEN, X25519_SHARED_SECRET_LEN,
};
pub use backend::{self_test_passed, CryptoBackend};
pub use cipher::{
    Aes128Ctr, Aes128Key, Aes192Ctr, Aes192Key, Aes256Ctr, Aes256Key, AesCtrIv, CipherError,
    AES128_KEY_LEN, AES192_KEY_LEN, AES256_KEY_LEN, AES_BLOCK_LEN,
};
pub use constant_time::ct_eq;
pub use ffdh::{
    ffdh_group14_agree, ffdh_group14_public, ffdh_group16_agree, ffdh_group16_public,
    ffdh_group18_agree, ffdh_group18_public, Ffdh2048Value, Ffdh4096Value, Ffdh8192Value,
    FfdhError, FFDH_GROUP14_LEN, FFDH_GROUP16_LEN, FFDH_GROUP18_LEN,
};
pub use hash::{
    sha256, sha384, sha512, Sha256Digest, Sha256Stream, Sha384Digest, Sha384Stream, Sha512Digest,
    Sha512Stream, SHA256_OUTPUT_LEN, SHA384_OUTPUT_LEN, SHA512_OUTPUT_LEN,
};
pub use kdf::{
    bcrypt_pbkdf, bcrypt_pbkdf_scratch_len, derive_key, pbkdf2_sha256, pbkdf2_sha256_verify,
    BcryptPbkdfError, DerivedKey, PasswordHash, BCRYPT_PBKDF_MAX_OUTPUT_LEN, DERIVED_KEY_LEN,
    PASSWORD_HASH_LEN,
};
pub use kem::{
    mlkem768_encapsulate, KemError, MlKem768Ciphertext, MlKem768EncapsulationKey,
    MlKem768SecretKey, MlKem768Seed, MlKem768SharedKey, MLKEM768_CIPHERTEXT_LEN,
    MLKEM768_ENCAPSULATION_KEY_LEN, MLKEM768_ENCAPS_RANDOMNESS_LEN, MLKEM768_SEED_LEN,
    MLKEM768_SHARED_KEY_LEN,
};
pub use mac::{
    hmac_sha1, hmac_sha1_parts, hmac_sha1_verify, hmac_sha256, hmac_sha256_parts,
    hmac_sha256_verify, hmac_sha512, hmac_sha512_parts, hmac_sha512_verify, poly1305,
    poly1305_verify, HmacSha1Key, HmacSha1Tag, HmacSha256Key, HmacSha256Tag, HmacSha512Key,
    HmacSha512Tag, Poly1305Key, Poly1305Tag, HMAC_SHA1_KEY_LEN, HMAC_SHA1_TAG_LEN,
    HMAC_SHA256_KEY_LEN, HMAC_SHA256_TAG_LEN, HMAC_SHA512_KEY_LEN, HMAC_SHA512_TAG_LEN,
    POLY1305_KEY_LEN, POLY1305_TAG_LEN,
};
pub use nistp::{
    NistCurveError, P256Point, P256PublicKey, P256Scalar, P256SecretKey, P256SharedSecret,
    P384Point, P384PublicKey, P384Scalar, P384SecretKey, P384SharedSecret, P521Point,
    P521PublicKey, P521Scalar, P521SecretKey, P521SharedSecret, P256_POINT_LEN, P256_SCALAR_LEN,
    P384_POINT_LEN, P384_SCALAR_LEN, P521_POINT_LEN, P521_SCALAR_LEN,
};
pub use sign::{
    Ed25519PublicKey, Ed25519SecretKey, Ed25519Seed, Ed25519Signature, SignatureError,
    ED25519_PUBLIC_KEY_LEN, ED25519_SEED_LEN, ED25519_SIGNATURE_LEN,
};
pub use stream::{
    chacha12_keystream, chacha20_apply, chacha20_keystream, ChaCha20Key, ChaCha20Nonce,
    StreamError, StreamKey, StreamNonce, CHACHA12_MAX_KEYSTREAM_BYTES, CHACHA20_KEY_LEN,
    CHACHA20_MAX_KEYSTREAM_BLOCKS, CHACHA20_NONCE_LEN, STREAM_KEY_LEN, STREAM_NONCE_LEN,
};
