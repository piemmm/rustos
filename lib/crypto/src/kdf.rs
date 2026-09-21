//! Key derivation.
//!
//! TAIRiX derives subkeys with HMAC-SHA256 used as a pseudo-random function,
//! the single-block case of HKDF-Expand (RFC 5869): a 256-bit secret keys the
//! MAC and a caller-chosen, domain-separating `context` string is the message.
//! The 256-bit output is itself a 256-bit key, so no expansion past one block
//! is ever required and the construction stays a thin wrapper over the audited
//! [`crate::mac`] primitive rather than a hand-rolled KDF.
//!
//! This is what `ARXFS` uses to grow its per-volume key hierarchy
//! (`docs/src/filesystem/arxfs-spec.md` §7): one master key derives the
//! metadata-authentication, filename, and content keys, each under a distinct
//! `context`, so a derived key never collides with another use of the master.
//!
//! For **password** material TAIRiX uses PBKDF2-HMAC-SHA256 (RFC 8018 §5.2):
//! a deliberately slow, salted derivation that makes offline guessing of a
//! stolen `/System/Security/Users` record expensive ([`pbkdf2_sha256`]). It
//! is a standard *construction* over the same audited HMAC primitive — the
//! same shape as `tairix-rng`'s HMAC-DRBG — never a hand-rolled primitive. Verification goes through [`crate::ct_eq`]
//! ([`pbkdf2_sha256_verify`]) so a stored hash comparison cannot leak through
//! timing.

use core::fmt;
use core::num::NonZeroU32;

use hmac::{Hmac, KeyInit as _, Mac};
use sha2::Sha256;

use crate::constant_time::ct_eq;
use crate::mac::{hmac_sha256, HmacSha256Key};

/// Length, in bytes, of a derived key. Matches both [`crate::mac::HMAC_SHA256_KEY_LEN`]
/// and [`crate::aead::AEAD_KEY_LEN`], so a derived key drops straight into
/// either primitive without truncation or expansion.
pub const DERIVED_KEY_LEN: usize = 32;

/// A 256-bit derived key as raw bytes.
pub type DerivedKey = [u8; DERIVED_KEY_LEN];

/// Derive a 256-bit subkey from a 256-bit `secret` and a domain-separating
/// `context`.
///
/// Computes `HMAC-SHA256(secret, context)` — the single-block HKDF-Expand
/// case (RFC 5869) — through the audited [`crate::mac`] wrapper. Distinct
/// `context` values yield independent keys from the same `secret`, so callers
/// must give each derived key its own stable, unique context label.
///
/// The output is uniformly random under the PRF assumption on HMAC-SHA256 and
/// reveals nothing about `secret`.
#[must_use]
pub fn derive_key(secret: &HmacSha256Key, context: &[u8]) -> DerivedKey {
    hmac_sha256(secret, context)
}

/// Length, in bytes, of a PBKDF2-derived password hash: one SHA-256 block,
/// so the derivation is the single-block PBKDF2 case (`T_1` only).
pub const PASSWORD_HASH_LEN: usize = 32;

/// A PBKDF2-HMAC-SHA256 password hash as raw bytes.
pub type PasswordHash = [u8; PASSWORD_HASH_LEN];

/// Derive a [`PasswordHash`] from `password` and `salt` with `iterations`
/// rounds of PBKDF2-HMAC-SHA256 (RFC 8018 §5.2).
///
/// The output length equals the HMAC output, so exactly one PBKDF2 block is
/// computed: `U_1 = HMAC(password, salt ‖ INT(1))`, `U_i = HMAC(password,
/// U_{i-1})`, and the hash is the XOR of all `U_i`. `iterations` is
/// [`NonZeroU32`] because zero rounds is not a defined PBKDF2 input; the
/// type, not a runtime check, rules it out. Callers choose the cost; the
/// users-database format pins its own accepted range.
#[must_use]
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: NonZeroU32) -> PasswordHash {
    // SAFETY-INVARIANT: HMAC accepts a key of any length, so construction
    // from an arbitrary password slice can never return `InvalidLength`.
    let prf = Hmac::<Sha256>::new_from_slice(password).expect("HMAC accepts any key length");

    let mut mac = prf.clone();
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut block: PasswordHash = mac.finalize().into_bytes().into();

    let mut out = block;
    for _ in 1..iterations.get() {
        let mut mac = prf.clone();
        mac.update(&block);
        block = mac.finalize().into_bytes().into();
        for (acc, byte) in out.iter_mut().zip(block.iter()) {
            *acc ^= byte;
        }
    }
    out
}

/// Verify that `expected` is the PBKDF2-HMAC-SHA256 hash of `password` under
/// `salt` and `iterations`, in constant time with respect to the hash
/// contents.
///
/// The comparison goes through [`crate::ct_eq`], so it does not leak through
/// timing how many leading hash bytes matched.
#[must_use]
pub fn pbkdf2_sha256_verify(
    password: &[u8],
    salt: &[u8],
    iterations: NonZeroU32,
    expected: &PasswordHash,
) -> bool {
    ct_eq(&pbkdf2_sha256(password, salt, iterations), expected)
}

/// A `bcrypt-pbkdf` derivation was refused.
///
/// Opaque and single-variant, as elsewhere in this crate: a zero round
/// count, an empty passphrase or salt, an output length outside what the
/// construction defines, or a scratch buffer too small are all a malformed
/// key file or a caller error rather than something to branch on.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct BcryptPbkdfError(());

impl fmt::Display for BcryptPbkdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bcrypt-pbkdf derivation refused")
    }
}

/// Block size, in bytes, the `bcrypt-pbkdf` stride is counted in.
const BCRYPT_PBKDF_BLOCK_LEN: usize = 32;

/// Longest output the construction defines.
pub const BCRYPT_PBKDF_MAX_OUTPUT_LEN: usize = BCRYPT_PBKDF_BLOCK_LEN * BCRYPT_PBKDF_BLOCK_LEN;

/// Scratch bytes a `bcrypt_pbkdf` of `output_len` bytes needs: the output
/// length rounded up to the block.
#[must_use]
pub const fn bcrypt_pbkdf_scratch_len(output_len: usize) -> usize {
    output_len.div_ceil(BCRYPT_PBKDF_BLOCK_LEN) * BCRYPT_PBKDF_BLOCK_LEN
}

/// Derive `output.len()` bytes from `passphrase` and `salt` with `rounds`
/// of OpenBSD's `bcrypt_pbkdf`, using `scratch` as working space.
///
/// This is a *foreign* format's KDF, present for one reason: the OpenSSH v1
/// private-key container wraps a passphrase-encrypted key with it, so
/// reading a user's existing `id_ed25519` requires it exactly as specified.
/// It is not TAIRiX's password KDF — [`pbkdf2_sha256`] is, and the users
/// database uses that. The construction is deliberately memory-hard
/// relative to plain PBKDF2: each round runs a Blowfish key schedule, which
/// is what makes a stolen key file expensive to attack offline.
///
/// `scratch` is the caller's rather than a buffer allocated here, for the
/// same reason the keystream in [`crate::stream`] writes straight into the
/// caller's destinations: it ends the call holding derived key material,
/// and the holder is the only party that can wipe it. It must be at least
/// [`bcrypt_pbkdf_scratch_len`] bytes.
///
/// # Errors
///
/// Returns [`BcryptPbkdfError`] if `rounds` is zero, either input is empty,
/// `output` is empty or longer than [`BCRYPT_PBKDF_MAX_OUTPUT_LEN`], or
/// `scratch` is too small. Each is refused rather than silently derived
/// from a truncated input.
pub fn bcrypt_pbkdf(
    passphrase: &[u8],
    salt: &[u8],
    rounds: u32,
    output: &mut [u8],
    scratch: &mut [u8],
) -> Result<(), BcryptPbkdfError> {
    bcrypt_pbkdf::bcrypt_pbkdf_with_memory(passphrase, salt, rounds, output, scratch)
        .map_err(|_| BcryptPbkdfError(()))
}

#[cfg(test)]
#[path = "kdf_tests.rs"]
mod tests;
