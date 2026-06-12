//! Audited cryptographic primitives for RustOS.
//!
//! This crate exists for one purpose: to keep cryptography out of the rest
//! of the codebase. Per `AGENTS.md` §1 no hand-rolled primitives are allowed;
//! everything here is a thin wrapper over a vetted upstream implementation
//! ([`sha2`], [`ed25519_dalek`], and [`chacha20poly1305`]) selected so that
//! the audit footprint never exceeds a handful of crates.
//!
//! The wrappers intentionally expose a *narrower* API than the upstream
//! crates: callers receive fixed-size byte arrays, not opaque types whose
//! lifetimes they would have to manage. This makes the boundary between
//! `lib/crypto` and the rest of the system straightforward to audit.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod aead;
pub mod constant_time;
pub mod hash;
pub mod kdf;
pub mod mac;
pub mod sign;

pub use aead::{
    open, seal, AeadError, AeadKey, AeadNonce, AeadTag, AEAD_KEY_LEN, AEAD_NONCE_LEN, AEAD_TAG_LEN,
};
pub use constant_time::ct_eq;
pub use hash::{sha256, Sha256Digest, SHA256_OUTPUT_LEN};
pub use kdf::{
    derive_key, pbkdf2_sha256, pbkdf2_sha256_verify, DerivedKey, PasswordHash, DERIVED_KEY_LEN,
    PASSWORD_HASH_LEN,
};
pub use mac::{
    hmac_sha256, hmac_sha256_parts, hmac_sha256_verify, MacKey, MacTag, MAC_KEY_LEN, MAC_TAG_LEN,
};
pub use sign::{Ed25519PublicKey, Ed25519Signature, SignatureError};
