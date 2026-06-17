//! Passphrase-derived volume-key unlock (`AGENTS.md` §11, `plans/PI.md`
//! P11).
//!
//! A [`RustFs`](crate::RustFs) volume is opened with a 32-byte
//! [`VolumeKey`] (the key-encryption key that unwraps the on-disk master
//! key, [`crate::crypto`]). That key is high-entropy random material no
//! human can type, so it cannot itself be the thing an operator supplies
//! at boot. This module is the standard LUKS-style indirection: the
//! operator supplies a *passphrase*, and the [`VolumeKey`] is derived from
//! it through a memory-hard-enough KDF over a per-volume random salt and a
//! tunable iteration count. Both the salt and the count travel with the
//! volume in a small, plaintext [`UnlockDescriptor`] (a few dozen bytes),
//! laid down beside the volume where the bootstrap can read it *before*
//! anything is decrypted — the FAT boot partition on a Pi SD image.
//!
//! ```text
//! passphrase + UnlockDescriptor{salt, iterations}
//!   --PBKDF2-HMAC-SHA256--> VolumeKey  --RustFs::open--> mounted root
//! ```
//!
//! The KDF is [`rustos_crypto::pbkdf2_sha256`] — the same audited
//! primitive that protects `/System/Security/Users` records (`AGENTS.md`
//! §2.12, never a hand-rolled KDF). Its 256-bit output is exactly a
//! [`VolumeKey`], so no truncation or expansion is involved.
//!
//! # The descriptor is not a secret; the passphrase is
//!
//! [`UnlockDescriptor`] carries only the salt and the iteration count.
//! Neither is secret: the salt exists to make precomputation attacks
//! per-volume, and the count to make each guess expensive. Storing the
//! descriptor in the clear on the boot partition is therefore safe and
//! deliberate — it is the analogue of a LUKS header. The passphrase is
//! never stored anywhere; only the operator (or, in future, a hardware
//! key store) holds it.
//!
//! # Hardware-backed key storage (future, `AGENTS.md` §19.9)
//!
//! Typing a passphrase at every boot is the *baseline*, available on any
//! board. A platform that has a hardware root of trust — a TPM with
//! measured boot / sealed storage, an Arm `TrustZone` secure world, an
//! Apple-style Secure Enclave, or the UEFI Secure Boot + TPM chain
//! Windows `BitLocker` uses — should instead **seal** the [`VolumeKey`] (or
//! the passphrase-derived wrapping key) to the platform's measured state
//! and release it automatically when the boot chain is unmodified,
//! falling back to the passphrase only on a recovery path. That hand-off
//! is out of scope for this module and tracked as future work: it is a
//! *source* of the [`VolumeKey`], slotting in beside this passphrase path,
//! and changes nothing about the on-disk volume. Physical attacks
//! (cold-boot, decap) remain explicitly out of the charter threat model
//! (`AGENTS.md` §19.9); sealing bounds the *remote/offline* attacker, not
//! the one with the silicon in a lab.

use core::num::NonZeroU32;

use rustos_abi::driver::DriverError;
use rustos_crypto::pbkdf2_sha256;

use crate::crypto::{EntropySource, VolumeKey, VOLUME_KEY_LEN};

/// Length, in bytes, of the per-volume unlock salt.
///
/// 128 bits is the conventional floor for a KDF salt: wide enough that a
/// precomputed table cannot be shared across volumes, narrow enough to
/// keep the descriptor tiny.
pub const UNLOCK_SALT_LEN: usize = 16;

/// Lowest PBKDF2 iteration count an [`UnlockDescriptor`] may carry.
///
/// A descriptor below this is refused on decode (`AGENTS.md` §5.4 — fail
/// closed): a volume whose key is cheap to brute-force is a security
/// defect, not a tuning choice. Mirrors the floor `lib/users` applies to
/// password records.
pub const UNLOCK_MIN_ITERATIONS: u32 = 100_000;

/// Highest PBKDF2 iteration count an [`UnlockDescriptor`] may carry.
///
/// Bounds the work a malformed or hostile descriptor can force the
/// bootstrap to perform (`AGENTS.md` §24.4 — a validation bound, not a
/// resource capacity). A real volume is provisioned far below this.
pub const UNLOCK_MAX_ITERATIONS: u32 = 10_000_000;

/// Default PBKDF2 iteration count a freshly provisioned volume carries.
///
/// Chosen to match the `/System/Security/Users` record default so a boot
/// derivation and a login derivation cost the same order of magnitude on
/// the same hardware.
pub const UNLOCK_DEFAULT_ITERATIONS: u32 = 600_000;

/// Magic identifying the unlock descriptor on disk (`"RUKx"`, *RustOS
/// Unlock*). A blob not beginning with it is not a descriptor and is
/// refused rather than misinterpreted (`AGENTS.md` §2.9).
const UNLOCK_MAGIC: [u8; 4] = *b"RUK1";

/// KDF identifier: PBKDF2-HMAC-SHA256. The only algorithm this version
/// defines; any other value is refused on decode.
const KDF_PBKDF2_SHA256: u8 = 1;

/// On-disk size of an encoded [`UnlockDescriptor`].
///
/// `magic(4) ‖ kdf(1) ‖ reserved(3) ‖ iterations(4, little-endian) ‖
/// salt(16)`. The reserved bytes pad the iteration count to a 4-byte
/// boundary and MUST be zero (a non-zero reserved byte is refused, so the
/// field cannot smuggle data past a future reader).
pub const UNLOCK_DESCRIPTOR_LEN: usize = 4 + 1 + 3 + 4 + UNLOCK_SALT_LEN;

/// File name the encoded [`UnlockDescriptor`] is stored under on the
/// plaintext boot partition (the FAT boot partition of a Pi SD image).
///
/// This is the on-storage contract between the *writer* — `tools/mkimage`
/// and the §11 installer, which plant the descriptor here — and the
/// *reader*, the boot path that reads it back *before* anything is
/// decrypted to turn the operator passphrase into the volume key. It lives
/// beside [`UnlockDescriptor`] so both ends share one definition rather
/// than each carrying a private copy of the literal (`AGENTS.md` §2.2).
pub const ROOT_UNLOCK_NAME: &str = "root.unlock";

/// The plaintext key-derivation descriptor stored beside an encrypted
/// volume (on a Pi SD image, in a file on the FAT boot partition).
///
/// It is **not** secret (see the module docs): it carries only the
/// per-volume salt and the PBKDF2 iteration count, the public parameters a
/// boot path needs to turn a typed passphrase into the volume's
/// [`VolumeKey`]. It is `Copy` and fixed-size, so a reader can hold it on
/// the stack with no allocation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UnlockDescriptor {
    iterations: NonZeroU32,
    salt: [u8; UNLOCK_SALT_LEN],
}

impl UnlockDescriptor {
    /// Provision a fresh descriptor: draw a random salt from `entropy` and
    /// record `iterations`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `iterations` is outside
    ///   `UNLOCK_MIN_ITERATIONS..=UNLOCK_MAX_ITERATIONS` — a volume is
    ///   never provisioned with a cheap-to-attack or absurd cost.
    /// * Whatever [`EntropySource::fill`] returns if the salt draw fails;
    ///   no descriptor is built from a failed draw (`AGENTS.md` §5.4), so
    ///   a fresh volume never carries a predictable salt.
    pub fn provision(
        iterations: u32,
        entropy: &mut dyn EntropySource,
    ) -> Result<Self, DriverError> {
        let iterations = Self::checked_iterations(iterations)?;
        let mut salt = [0u8; UNLOCK_SALT_LEN];
        entropy.fill(&mut salt)?;
        Ok(Self { iterations, salt })
    }

    /// Validate `iterations` against the policy bounds and lift it to a
    /// [`NonZeroU32`], the form the struct stores so the derivation path
    /// never has to reconsider zero (`AGENTS.md` §2.9 — no panic path).
    fn checked_iterations(iterations: u32) -> Result<NonZeroU32, DriverError> {
        if !(UNLOCK_MIN_ITERATIONS..=UNLOCK_MAX_ITERATIONS).contains(&iterations) {
            return Err(DriverError::OutOfRange);
        }
        // The lower bound is `>= UNLOCK_MIN_ITERATIONS >= 1`, so this is
        // always `Some`; mapping `None` to the same fail-closed error keeps
        // the function total without an `unwrap`.
        NonZeroU32::new(iterations).ok_or(DriverError::OutOfRange)
    }

    /// The PBKDF2 iteration count.
    #[must_use]
    pub fn iterations(&self) -> u32 {
        self.iterations.get()
    }

    /// The per-volume salt.
    #[must_use]
    pub fn salt(&self) -> &[u8; UNLOCK_SALT_LEN] {
        &self.salt
    }

    /// Derive the volume key from `passphrase` under this descriptor.
    ///
    /// The 256-bit PBKDF2 output is the [`VolumeKey`] verbatim. A wrong
    /// passphrase yields the wrong key, which [`RustFs::open`](crate::RustFs::open)
    /// then rejects through the AEAD authentication of the wrapped master
    /// key — there is no separate "passphrase correct?" oracle here, so a
    /// guess costs a full mount attempt (`AGENTS.md` §5.4).
    #[must_use]
    pub fn derive_volume_key(&self, passphrase: &[u8]) -> VolumeKey {
        let hash = pbkdf2_sha256(passphrase, &self.salt, self.iterations);
        let mut key = [0u8; VOLUME_KEY_LEN];
        key.copy_from_slice(&hash);
        key
    }

    /// Encode the descriptor into the first [`UNLOCK_DESCRIPTOR_LEN`] bytes
    /// of `out`.
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] if `out` is shorter than
    /// [`UNLOCK_DESCRIPTOR_LEN`].
    pub fn encode(&self, out: &mut [u8]) -> Result<(), DriverError> {
        if out.len() < UNLOCK_DESCRIPTOR_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        out[0..4].copy_from_slice(&UNLOCK_MAGIC);
        out[4] = KDF_PBKDF2_SHA256;
        out[5..8].fill(0);
        out[8..12].copy_from_slice(&self.iterations.get().to_le_bytes());
        out[12..12 + UNLOCK_SALT_LEN].copy_from_slice(&self.salt);
        Ok(())
    }

    /// Decode a descriptor from the first [`UNLOCK_DESCRIPTOR_LEN`] bytes of
    /// `bytes`, fail-closed.
    ///
    /// # Errors
    ///
    /// On any of: too few bytes ([`DriverError::BufferTooSmall`]); a wrong
    /// magic, an unknown KDF id, or a non-zero reserved byte
    /// ([`DriverError::BadMagic`]); or an iteration count outside
    /// `UNLOCK_MIN_ITERATIONS..=UNLOCK_MAX_ITERATIONS`
    /// ([`DriverError::OutOfRange`]). A blob that is not exactly a
    /// well-formed descriptor never yields one (`AGENTS.md` §2.9 / §5.4.3
    /// — validate every field).
    pub fn decode(bytes: &[u8]) -> Result<Self, DriverError> {
        if bytes.len() < UNLOCK_DESCRIPTOR_LEN {
            return Err(DriverError::BufferTooSmall);
        }
        if bytes[0..4] != UNLOCK_MAGIC {
            return Err(DriverError::BadMagic);
        }
        if bytes[4] != KDF_PBKDF2_SHA256 {
            return Err(DriverError::BadMagic);
        }
        if bytes[5..8].iter().any(|&b| b != 0) {
            return Err(DriverError::BadMagic);
        }
        let mut iter_bytes = [0u8; 4];
        iter_bytes.copy_from_slice(&bytes[8..12]);
        let iterations = Self::checked_iterations(u32::from_le_bytes(iter_bytes))?;
        let mut salt = [0u8; UNLOCK_SALT_LEN];
        salt.copy_from_slice(&bytes[12..12 + UNLOCK_SALT_LEN]);
        Ok(Self { iterations, salt })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic entropy stand-in for the tests: fills with a fixed,
    /// recognisable byte so a provisioned salt is predictable.
    struct FixedEntropy(u8);

    impl EntropySource for FixedEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
            out.fill(self.0);
            Ok(())
        }
    }

    /// Entropy source that always fails, to prove provisioning fails closed.
    struct DeadEntropy;

    impl EntropySource for DeadEntropy {
        fn fill(&mut self, _out: &mut [u8]) -> Result<(), DriverError> {
            Err(DriverError::DeviceFault)
        }
    }

    #[test]
    fn provision_records_iterations_and_random_salt() {
        let mut ent = FixedEntropy(0xAB);
        let desc = UnlockDescriptor::provision(UNLOCK_DEFAULT_ITERATIONS, &mut ent)
            .expect("provision succeeds");
        assert_eq!(desc.iterations(), UNLOCK_DEFAULT_ITERATIONS);
        assert_eq!(desc.salt(), &[0xAB; UNLOCK_SALT_LEN]);
    }

    #[test]
    fn provision_rejects_out_of_range_iterations() {
        let mut ent = FixedEntropy(0);
        assert_eq!(
            UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS - 1, &mut ent),
            Err(DriverError::OutOfRange)
        );
        assert_eq!(
            UnlockDescriptor::provision(UNLOCK_MAX_ITERATIONS + 1, &mut ent),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn provision_fails_closed_on_dead_entropy() {
        let mut ent = DeadEntropy;
        assert_eq!(
            UnlockDescriptor::provision(UNLOCK_DEFAULT_ITERATIONS, &mut ent),
            Err(DriverError::DeviceFault)
        );
    }

    #[test]
    fn encode_decode_round_trips() {
        let mut ent = FixedEntropy(0x5A);
        let desc = UnlockDescriptor::provision(250_000, &mut ent).expect("provision succeeds");
        let mut buf = [0u8; UNLOCK_DESCRIPTOR_LEN];
        desc.encode(&mut buf).expect("encode succeeds");
        let decoded = UnlockDescriptor::decode(&buf).expect("decode succeeds");
        assert_eq!(decoded, desc);
    }

    #[test]
    fn derive_is_deterministic_and_passphrase_sensitive() {
        let mut ent = FixedEntropy(0x11);
        let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut ent)
            .expect("provision succeeds");
        let a = desc.derive_volume_key(b"correct horse");
        let b = desc.derive_volume_key(b"correct horse");
        let c = desc.derive_volume_key(b"wrong horse");
        assert_eq!(a, b, "same passphrase + salt derives the same key");
        assert_ne!(a, c, "a different passphrase derives a different key");
    }

    #[test]
    fn derive_is_salt_sensitive() {
        let mut a_ent = FixedEntropy(1);
        let mut b_ent = FixedEntropy(2);
        let a = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut a_ent)
            .expect("provision")
            .derive_volume_key(b"same passphrase");
        let b = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut b_ent)
            .expect("provision")
            .derive_volume_key(b"same passphrase");
        assert_ne!(a, b, "a different salt derives a different key");
    }

    #[test]
    fn encode_rejects_short_buffer() {
        let mut ent = FixedEntropy(0);
        let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut ent).expect("provision");
        let mut buf = [0u8; UNLOCK_DESCRIPTOR_LEN - 1];
        assert_eq!(desc.encode(&mut buf), Err(DriverError::BufferTooSmall));
    }

    #[test]
    fn decode_rejects_short_buffer() {
        let buf = [0u8; UNLOCK_DESCRIPTOR_LEN - 1];
        assert_eq!(
            UnlockDescriptor::decode(&buf),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut ent = FixedEntropy(0);
        let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut ent).expect("provision");
        let mut buf = [0u8; UNLOCK_DESCRIPTOR_LEN];
        desc.encode(&mut buf).expect("encode");
        buf[0] ^= 0xFF;
        assert_eq!(UnlockDescriptor::decode(&buf), Err(DriverError::BadMagic));
    }

    #[test]
    fn decode_rejects_unknown_kdf() {
        let mut ent = FixedEntropy(0);
        let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut ent).expect("provision");
        let mut buf = [0u8; UNLOCK_DESCRIPTOR_LEN];
        desc.encode(&mut buf).expect("encode");
        buf[4] = 0xFF;
        assert_eq!(UnlockDescriptor::decode(&buf), Err(DriverError::BadMagic));
    }

    #[test]
    fn decode_rejects_nonzero_reserved() {
        let mut ent = FixedEntropy(0);
        let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut ent).expect("provision");
        let mut buf = [0u8; UNLOCK_DESCRIPTOR_LEN];
        desc.encode(&mut buf).expect("encode");
        buf[6] = 1;
        assert_eq!(UnlockDescriptor::decode(&buf), Err(DriverError::BadMagic));
    }

    #[test]
    fn decode_rejects_out_of_range_iterations() {
        let mut ent = FixedEntropy(0);
        let desc = UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut ent).expect("provision");
        let mut buf = [0u8; UNLOCK_DESCRIPTOR_LEN];
        desc.encode(&mut buf).expect("encode");
        // Below the floor.
        buf[8..12].copy_from_slice(&(UNLOCK_MIN_ITERATIONS - 1).to_le_bytes());
        assert_eq!(UnlockDescriptor::decode(&buf), Err(DriverError::OutOfRange));
        // Above the ceiling.
        buf[8..12].copy_from_slice(&(UNLOCK_MAX_ITERATIONS + 1).to_le_bytes());
        assert_eq!(UnlockDescriptor::decode(&buf), Err(DriverError::OutOfRange));
    }
}
