//! Key encapsulation: ML-KEM-768 (FIPS 203).
//!
//! The post-quantum half of SSH's `mlkem768x25519-sha256`, OpenSSH's current
//! default key exchange. It is a *hybrid*: the shared secret is derived from
//! this encapsulation and an X25519 agreement together ([`crate::agree`]),
//! so breaking either one alone does not break the session, and the
//! classical half still protects a session if the lattice assumption fails.
//!
//! Nothing here draws randomness. FIPS 203 defines key generation and
//! encapsulation as deterministic functions of caller-supplied random bytes
//! (`d ‖ z` and `m`), and those are the entry points exposed, so a caller
//! feeds the one kernel CSPRNG and a test feeds a fixture.
//!
//! # The encapsulated key is a key
//!
//! Unlike a Diffie-Hellman output, ML-KEM's shared key is already a uniform
//! 32-byte string. The hybrid construction still hashes it together with the
//! X25519 half before use, so no caller in TAIRiX consumes it directly.

use core::fmt;

use ml_kem::array::typenum::Unsigned;
use ml_kem::kem::{Decapsulate, Kem};
use ml_kem::{Ciphertext, EncapsulationKey, Key, KeyExport, KeyInit, MlKem768, Seed, B32};

/// Length, in bytes, of the seed an ML-KEM-768 key pair is derived from:
/// FIPS 203's `d ‖ z`.
pub const MLKEM768_SEED_LEN: usize = 64;

/// Length, in bytes, of an ML-KEM-768 encapsulation (public) key.
pub const MLKEM768_ENCAPSULATION_KEY_LEN: usize = 1184;

/// Length, in bytes, of an ML-KEM-768 ciphertext.
pub const MLKEM768_CIPHERTEXT_LEN: usize = 1088;

/// Length, in bytes, of an ML-KEM-768 shared key.
pub const MLKEM768_SHARED_KEY_LEN: usize = 32;

/// Length, in bytes, of the randomness one encapsulation consumes: FIPS
/// 203's `m`.
pub const MLKEM768_ENCAPS_RANDOMNESS_LEN: usize = 32;

/// An ML-KEM-768 key-pair seed (`d ‖ z`) as raw bytes.
///
/// This *is* the private key: FIPS 203 derives the whole decapsulation key
/// from it, so it is the only form worth storing and the caller owns wiping
/// its copy.
pub type MlKem768Seed = [u8; MLKEM768_SEED_LEN];

/// An ML-KEM-768 encapsulation key as raw bytes.
pub type MlKem768EncapsulationKey = [u8; MLKEM768_ENCAPSULATION_KEY_LEN];

/// An ML-KEM-768 ciphertext as raw bytes.
pub type MlKem768Ciphertext = [u8; MLKEM768_CIPHERTEXT_LEN];

/// An ML-KEM-768 shared key as raw bytes.
pub type MlKem768SharedKey = [u8; MLKEM768_SHARED_KEY_LEN];

// The declared widths must be the ones the parameter set actually uses: a
// dependency bump that changed a size would otherwise mis-copy silently
// rather than fail to build.
const _: () = {
    assert!(MLKEM768_CIPHERTEXT_LEN == <MlKem768 as Kem>::CiphertextSize::USIZE);
    assert!(MLKEM768_SHARED_KEY_LEN == <MlKem768 as Kem>::SharedKeySize::USIZE);
};

/// An ML-KEM-768 encapsulation key was not a valid one.
///
/// Opaque and single-variant, as elsewhere in this crate. Note that
/// *decapsulation* has no error: FIPS 203 requires implicit rejection, so a
/// forged ciphertext yields an unpredictable key rather than a detectable
/// failure, which is what stops a chosen-ciphertext attacker learning
/// anything from the distinction.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct KemError(());

impl fmt::Display for KemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ml-kem encapsulation key refused")
    }
}

/// An ML-KEM-768 decapsulation (private) key, wiped on drop.
pub struct MlKem768SecretKey {
    inner: ml_kem::ml_kem_768::DecapsulationKey,
}

impl MlKem768SecretKey {
    /// Derive a key pair from FIPS 203's 64-byte `d ‖ z` seed.
    ///
    /// Every 64-byte string is a valid seed, so there is nothing to reject.
    #[must_use]
    pub fn from_seed(seed: &MlKem768Seed) -> Self {
        Self {
            inner: <ml_kem::ml_kem_768::DecapsulationKey as KeyInit>::new(&Seed::from(*seed)),
        }
    }

    /// The encapsulation key a peer encapsulates to.
    #[must_use]
    pub fn encapsulation_key(&self) -> MlKem768EncapsulationKey {
        let encoded = self.inner.encapsulation_key().to_bytes();
        let mut out = [0u8; MLKEM768_ENCAPSULATION_KEY_LEN];
        out.copy_from_slice(&encoded);
        out
    }

    /// Recover the shared key from `ciphertext`.
    ///
    /// Infallible by design: FIPS 203 §7.3 specifies implicit rejection, so
    /// a ciphertext that was not produced for this key yields a key derived
    /// from the seed's rejection secret rather than an error. A caller
    /// learns a ciphertext was forged when the session that used the key
    /// fails to authenticate, never from this call.
    #[must_use]
    pub fn decapsulate(&self, ciphertext: &MlKem768Ciphertext) -> MlKem768SharedKey {
        // The widths agree by the compile-time assertion above, so this
        // copy has no reachable failure.
        let mut ct = Ciphertext::<MlKem768>::default();
        ct.copy_from_slice(ciphertext);
        let shared = self.inner.decapsulate(&ct);
        let mut out = [0u8; MLKEM768_SHARED_KEY_LEN];
        out.copy_from_slice(&shared);
        out
    }
}

impl fmt::Debug for MlKem768SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The decapsulation key is private material; it never reaches a log.
        f.debug_struct("MlKem768SecretKey").finish_non_exhaustive()
    }
}

/// Encapsulate a fresh shared key to `encapsulation_key`.
///
/// `randomness` is FIPS 203's `m`, the 32 bytes the encapsulation consumes;
/// the caller draws it from the one kernel CSPRNG. Reusing it against the
/// same key reproduces the same ciphertext and shared key, so it must be
/// fresh per encapsulation.
///
/// # Errors
///
/// Returns [`KemError`] if `encapsulation_key` is not a valid ML-KEM-768
/// key — its polynomial coefficients must all be canonically reduced, which
/// is what stops a malformed key steering the result.
pub fn mlkem768_encapsulate(
    encapsulation_key: &MlKem768EncapsulationKey,
    randomness: &[u8; MLKEM768_ENCAPS_RANDOMNESS_LEN],
) -> Result<(MlKem768Ciphertext, MlKem768SharedKey), KemError> {
    let encoded = Key::<EncapsulationKey<MlKem768>>::try_from(&encapsulation_key[..])
        .map_err(|_| KemError(()))?;
    let key = EncapsulationKey::<MlKem768>::new(&encoded).map_err(|_| KemError(()))?;
    let (ct, shared) = key.encapsulate_deterministic(&B32::from(*randomness));

    let mut ciphertext = [0u8; MLKEM768_CIPHERTEXT_LEN];
    ciphertext.copy_from_slice(&ct);
    let mut key_out = [0u8; MLKEM768_SHARED_KEY_LEN];
    key_out.copy_from_slice(&shared);
    Ok((ciphertext, key_out))
}

#[cfg(test)]
#[path = "kem_tests.rs"]
mod tests;
