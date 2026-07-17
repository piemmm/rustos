//! `ARXFS` per-volume key hierarchy and at-rest encryption
//! (`docs/src/filesystem/arxfs-spec.md` §5, §7).
//!
//! `ARXFS` is encrypted by default and has no plaintext mode: there is no
//! code path that lays out an unencrypted volume. Every volume is created with
//! a caller-supplied [`VolumeKey`] (the installer's / recovery flow's key
//! material). From it `ARXFS` grows the key hierarchy the spec fixes:
//!
//! ```text
//! volume key (caller-supplied)
//!   -> wrapping key  (KDF) ----- unwraps -----> master key (on disk, AEAD-wrapped)
//!                                                  -> metadata-authentication key (HMAC)
//!                                                  -> filename key (AEAD)
//!                                                  -> content  key (AEAD)
//! ```
//!
//! The master key is never stored unwrapped: only its AEAD-sealed form lives
//! on disk, in the plaintext discovery region of every superblock slot (the
//! "minimal unlock header" the spec permits, §7). Opening derives the wrapping
//! key from the supplied [`VolumeKey`] and unseals the master key; a wrong key
//! fails the AEAD authentication and the mount is refused, fail-closed, never a panic.
//!
//! All primitives come through `lib/crypto`: the KDF
//! ([`tairix_crypto::derive_key`]), the metadata MAC (HMAC-SHA256), and the
//! AEAD (ChaCha20-Poly1305). Nothing here hand-rolls a primitive.
//!
//! # The key material is drawn from the platform RNG
//!
//! The per-volume master key, its wrapping salt, and the wrap nonce are drawn
//! at format time from an injected [`EntropySource`] — the seam onto the
//! platform RNG (`lib/rng`'s `CsRng`, the cryptographically secure generator
//! the charter mandates for `ARXFS` keys). The master key is therefore
//! independent of the caller's volume key (and re-wrappable on a future key
//! change) rather than derived from it. Only the wrapping key stays a
//! deterministic KDF of the volume key and the random salt, because [`unwrap`]
//! must recompute it from the supplied volume key to unseal the master key on
//! mount. A failed entropy draw never yields a volume with predictable key
//! material: provisioning fails closed.
//!
//! The driver itself never reaches for a global RNG; the concrete generator is
//! injected at the composition root, mirroring the seam `kernel/mem`'s
//! encrypted swap, `init`'s `Spawner`, and `login`'s `Authenticator` use. That
//! keeps the driver architecture-neutral.

use tairix_abi::driver::DriverError;
use tairix_crypto::{
    derive_key, open, seal, AeadError, AeadKey, AeadNonce, AeadTag, MacKey, AEAD_NONCE_LEN,
};

/// Length, in bytes, of a volume key — the caller-supplied key that unwraps a
/// `ARXFS` volume. A 256-bit key matches the AEAD and MAC key widths.
pub const VOLUME_KEY_LEN: usize = 32;

/// A caller-supplied volume key: the installer's, recovery flow's, or storage
/// policy service's key material that unlocks a `ARXFS` volume. `ARXFS`
/// never persists it; only the master key it unwraps is stored, and that only
/// in AEAD-sealed form.
pub type VolumeKey = [u8; VOLUME_KEY_LEN];

/// Source of cryptographic randomness for a fresh volume's key material (the
/// master key, the wrapping salt, and the wrap nonce) and its UUID.
///
/// This is `ARXFS`'s seam onto the platform RNG: the composition root
/// injects the cryptographically secure generator (`lib/rng`'s `CsRng`, the
/// generator the charter mandates for `ARXFS` keys), so the driver
/// itself never names or reaches for a global RNG and stays
/// architecture-neutral. It mirrors the injection seam `kernel/mem`'s
/// encrypted swap, `init`'s `Spawner`, and `login`'s `Authenticator` use.
pub trait EntropySource {
    /// Fill the whole of `out` with cryptographically secure random bytes.
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] (the implementation's fail-closed code) if
    /// randomness is unavailable. `ARXFS` fails closed: no
    /// key, salt, nonce, or UUID is derived from a failed draw, so a volume is
    /// never provisioned with predictable key material. On error the contents
    /// of `out` are unspecified and must not be used.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError>;
}

/// Bytes of per-block crypto trailer appended to every encrypted data and
/// directory block: a [`tairix_crypto::AEAD_NONCE_LEN`]-byte nonce followed by
/// a [`tairix_crypto::AEAD_TAG_LEN`]-byte authentication tag.
pub const CRYPTO_TRAILER: usize = tairix_crypto::AEAD_NONCE_LEN + tairix_crypto::AEAD_TAG_LEN;

/// On-disk size of the crypto discovery header stored in a superblock slot's
/// plaintext payload region: a 16-byte salt, the 32-byte AEAD-wrapped master
/// key, the wrap nonce, and the wrap tag.
pub const CRYPTO_HEADER_LEN: usize =
    SALT_LEN + VOLUME_KEY_LEN + tairix_crypto::AEAD_NONCE_LEN + tairix_crypto::AEAD_TAG_LEN;

/// Length, in bytes, of the per-volume wrapping salt.
const SALT_LEN: usize = 16;

// Domain-separating KDF context labels. Each derived key gets its own stable
// label so no two uses of a parent key ever collide (`tairix_crypto::kdf`).
const CTX_WRAP: &[u8] = b"arxfs/wrap-key";
const CTX_META: &[u8] = b"arxfs/meta-mac";
const CTX_FILENAME: &[u8] = b"arxfs/filename";
const CTX_CONTENT: &[u8] = b"arxfs/content";
const CTX_DEDUPE: &[u8] = b"arxfs/dedupe-domain";

/// The fully-derived set of working keys for a mounted volume.
///
/// Every field is a key, so the shared `key` suffix is intentional and names
/// each key's role rather than repeating the type (`clippy::struct_field_names`
/// is silenced for exactly that reason).
#[derive(Clone)]
#[allow(clippy::struct_field_names)]
pub struct VolumeKeys {
    /// Keyed authenticator for every metadata block (HMAC-SHA256).
    pub mac_key: MacKey,
    /// AEAD key encrypting directory-entry names at rest.
    pub filename_key: AeadKey,
    /// AEAD key encrypting file data at rest.
    pub content_key: AeadKey,
    /// Stable per-volume deduplication-domain identifier. Dedupe is
    /// allowed only within one domain; with a single volume key today there
    /// is exactly one domain, but it is carried in every chunk record and
    /// index key so the cross-domain rule holds once domains arrive. It is
    /// derived from the master key, not secret in itself, and is an
    /// identifier rather than a key.
    pub dedupe_domain: u64,
}

/// The plaintext crypto discovery header stored in every superblock slot.
#[derive(Copy, Clone)]
pub struct CryptoHeader {
    salt: [u8; SALT_LEN],
    wrapped_master: [u8; VOLUME_KEY_LEN],
    wrap_nonce: AeadNonce,
    wrap_tag: AeadTag,
}

/// Derive a 256-bit subkey from `secret`, a domain-separating `label`, and the
/// per-volume `salt`, all through the `lib/crypto` KDF.
fn derive_with_salt(secret: &[u8; 32], label: &[u8], salt: &[u8; SALT_LEN]) -> [u8; 32] {
    let mut ctx = [0u8; 64];
    let l = label.len();
    ctx[..l].copy_from_slice(label);
    ctx[l..l + SALT_LEN].copy_from_slice(salt);
    derive_key(secret, &ctx[..l + SALT_LEN])
}

/// The wrapping key that seals the master key on disk, derived from the
/// caller's volume key and the per-volume salt. It is a deterministic KDF of
/// the volume key so [`unwrap`] can recompute it on mount; the random salt
/// (stored in the clear) makes it unique per volume.
fn wrapping_key(volume_key: &VolumeKey, salt: &[u8; SALT_LEN]) -> [u8; 32] {
    derive_with_salt(volume_key, CTX_WRAP, salt)
}

/// Grow the working key set from the master key.
fn derive_volume_keys(master: &[u8; 32]) -> VolumeKeys {
    let domain_material = derive_key(master, CTX_DEDUPE);
    let mut domain_bytes = [0u8; 8];
    domain_bytes.copy_from_slice(&domain_material[..8]);
    VolumeKeys {
        mac_key: derive_key(master, CTX_META),
        filename_key: derive_key(master, CTX_FILENAME),
        content_key: derive_key(master, CTX_CONTENT),
        dedupe_domain: u64::from_le_bytes(domain_bytes),
    }
}

impl CryptoHeader {
    /// Encode the header into the first [`CRYPTO_HEADER_LEN`] bytes of `out`.
    ///
    /// # Errors
    ///
    /// [`AeadError::Authentication`] is never produced here; the function is
    /// infallible for a correctly-sized buffer and panics are avoided by the
    /// caller always passing a full payload region.
    pub fn encode(&self, out: &mut [u8]) {
        let mut off = 0;
        out[off..off + SALT_LEN].copy_from_slice(&self.salt);
        off += SALT_LEN;
        out[off..off + VOLUME_KEY_LEN].copy_from_slice(&self.wrapped_master);
        off += VOLUME_KEY_LEN;
        out[off..off + AEAD_NONCE_LEN].copy_from_slice(&self.wrap_nonce);
        off += AEAD_NONCE_LEN;
        out[off..off + self.wrap_tag.len()].copy_from_slice(&self.wrap_tag);
    }

    /// Decode a header from the first [`CRYPTO_HEADER_LEN`] bytes of `bytes`.
    /// Returns `None` if `bytes` is too short. The bytes are plaintext
    /// discovery material; their authenticity is established by a successful
    /// [`unwrap`].
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CRYPTO_HEADER_LEN {
            return None;
        }
        let mut off = 0;
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[off..off + SALT_LEN]);
        off += SALT_LEN;
        let mut wrapped_master = [0u8; VOLUME_KEY_LEN];
        wrapped_master.copy_from_slice(&bytes[off..off + VOLUME_KEY_LEN]);
        off += VOLUME_KEY_LEN;
        let mut wrap_nonce = [0u8; AEAD_NONCE_LEN];
        wrap_nonce.copy_from_slice(&bytes[off..off + AEAD_NONCE_LEN]);
        off += AEAD_NONCE_LEN;
        let mut wrap_tag = [0u8; tairix_crypto::AEAD_TAG_LEN];
        wrap_tag.copy_from_slice(&bytes[off..off + tairix_crypto::AEAD_TAG_LEN]);
        Some(Self {
            salt,
            wrapped_master,
            wrap_nonce,
            wrap_tag,
        })
    }
}

/// Provision a fresh key hierarchy for a new volume at format time.
///
/// Draws the per-volume wrapping salt, the master key, and the wrap nonce from
/// `entropy` (the platform RNG seam), derives the wrapping key from the
/// caller-supplied `volume_key` and the random salt, AEAD-seals the master key
/// under it, and returns the on-disk [`CryptoHeader`] plus the working
/// [`VolumeKeys`]. The master key is independent of `volume_key`, so the
/// volume can be re-wrapped on a key change without rewriting its data. No
/// plaintext key is ever returned for storage; the header carries only the
/// wrapped master key.
///
/// # Errors
///
/// * [`DriverError`] (the entropy source's fail-closed code) if any random
///   draw is unavailable: provisioning aborts before sealing, so a volume is
///   never created with predictable key material.
/// * [`DriverError::DeviceFault`] if sealing the master key fails (unreachable
///   for the fixed-size buffer, but surfaced rather than panicked).
pub fn provision(
    volume_key: &VolumeKey,
    entropy: &mut dyn EntropySource,
) -> Result<(CryptoHeader, VolumeKeys), DriverError> {
    let mut salt = [0u8; SALT_LEN];
    entropy.fill(&mut salt)?;
    let mut master = [0u8; 32];
    entropy.fill(&mut master)?;
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    entropy.fill(&mut nonce)?;

    let wrapping = wrapping_key(volume_key, &salt);
    let mut wrapped_master = master;
    let wrap_tag = seal(&wrapping, &nonce, &salt, &mut wrapped_master)
        .map_err(|_| DriverError::DeviceFault)?;
    let keys = derive_volume_keys(&master);
    Ok((
        CryptoHeader {
            salt,
            wrapped_master,
            wrap_nonce: nonce,
            wrap_tag,
        },
        keys,
    ))
}

/// Recover the working key set by unwrapping the master key with the
/// caller-supplied `volume_key` at mount time.
///
/// # Errors
///
/// [`AeadError::Authentication`] if `volume_key` is wrong (or the wrapped
/// blob is corrupt): the AEAD tag fails to verify, so the caller refuses the
/// mount, fail-closed.
pub fn unwrap(volume_key: &VolumeKey, header: &CryptoHeader) -> Result<VolumeKeys, AeadError> {
    let wrapping = wrapping_key(volume_key, &header.salt);
    let mut master = header.wrapped_master;
    open(
        &wrapping,
        &header.wrap_nonce,
        &header.salt,
        &mut master,
        &header.wrap_tag,
    )?;
    Ok(derive_volume_keys(&master))
}

/// The deterministic AEAD nonce for the block at physical address `phys`
/// written by transaction generation `gen`. Unique per persisted write
/// because copy-on-write gives a fresh `(phys, gen)` to every distinct stored
/// ciphertext, and the nonce is also stored in the block so reads never
/// recompute it.
fn block_nonce(key: &AeadKey, phys: u64, gen: u64) -> AeadNonce {
    let mut ctx = [0u8; 16];
    ctx[..8].copy_from_slice(&phys.to_le_bytes());
    ctx[8..].copy_from_slice(&gen.to_le_bytes());
    let full = derive_key(key, &ctx);
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    nonce.copy_from_slice(&full[..AEAD_NONCE_LEN]);
    nonce
}

/// Encrypt `region` in place under `key`, writing the nonce and tag into
/// `trailer` (which must be exactly [`CRYPTO_TRAILER`] bytes). `phys` and
/// `gen` make the nonce unique; `phys` is also bound as associated data so a
/// block decrypted at the wrong physical address fails.
///
/// # Errors
///
/// [`AeadError`] if the AEAD seal fails (unreachable for the fixed key/nonce
/// widths, surfaced rather than panicked).
pub fn encrypt_region(
    key: &AeadKey,
    region: &mut [u8],
    trailer: &mut [u8],
    phys: u64,
    gen: u64,
) -> Result<(), AeadError> {
    let nonce = block_nonce(key, phys, gen);
    let tag = seal(key, &nonce, &phys.to_le_bytes(), region)?;
    trailer[..AEAD_NONCE_LEN].copy_from_slice(&nonce);
    trailer[AEAD_NONCE_LEN..CRYPTO_TRAILER].copy_from_slice(&tag);
    Ok(())
}

/// Decrypt `region` in place under `key`, reading the nonce and tag from
/// `trailer`. Authenticates before it yields plaintext: a bit-flip in the
/// ciphertext, trailer, or `phys` binding fails the tag and the caller fails
/// closed, never returning mis-decrypted bytes.
///
/// # Errors
///
/// [`AeadError::Authentication`] if authentication fails.
pub fn decrypt_region(
    key: &AeadKey,
    region: &mut [u8],
    trailer: &[u8],
    phys: u64,
) -> Result<(), AeadError> {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    nonce.copy_from_slice(&trailer[..AEAD_NONCE_LEN]);
    let mut tag = [0u8; tairix_crypto::AEAD_TAG_LEN];
    tag.copy_from_slice(&trailer[AEAD_NONCE_LEN..CRYPTO_TRAILER]);
    open(key, &nonce, &phys.to_le_bytes(), region, &tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VK: VolumeKey = [0x11; VOLUME_KEY_LEN];

    /// A deterministic stand-in for the platform RNG: a byte counter starting
    /// at `seed`, so each fill is distinct and reproducible and different seeds
    /// produce different streams. It is test scaffolding, never a production
    /// entropy source.
    struct CountingEntropy {
        next: u8,
    }

    impl CountingEntropy {
        fn new(seed: u8) -> Self {
            Self { next: seed }
        }
    }

    impl EntropySource for CountingEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
            for byte in out.iter_mut() {
                *byte = self.next;
                self.next = self.next.wrapping_add(1);
            }
            Ok(())
        }
    }

    /// An entropy source that always fails, to drive the fail-closed path.
    struct DeadEntropy;

    impl EntropySource for DeadEntropy {
        fn fill(&mut self, _out: &mut [u8]) -> Result<(), DriverError> {
            Err(DriverError::DeviceFault)
        }
    }

    #[test]
    fn provision_then_unwrap_round_trips_keys() {
        let (header, keys) = provision(&VK, &mut CountingEntropy::new(1)).expect("provision");
        let recovered = unwrap(&VK, &header).expect("unwrap");
        assert_eq!(keys.mac_key, recovered.mac_key);
        assert_eq!(keys.filename_key, recovered.filename_key);
        assert_eq!(keys.content_key, recovered.content_key);
    }

    #[test]
    fn wrong_volume_key_fails_to_unwrap() {
        let (header, _) = provision(&VK, &mut CountingEntropy::new(2)).expect("provision");
        let mut wrong = VK;
        wrong[0] ^= 0x01;
        assert!(unwrap(&wrong, &header).is_err());
    }

    #[test]
    fn derived_keys_are_independent() {
        let (_, keys) = provision(&VK, &mut CountingEntropy::new(3)).expect("provision");
        assert_ne!(keys.mac_key, keys.filename_key);
        assert_ne!(keys.mac_key, keys.content_key);
        assert_ne!(keys.filename_key, keys.content_key);
    }

    #[test]
    fn distinct_entropy_yields_distinct_master_keys() {
        // Two volumes formatted with the same volume key but independent RNG
        // streams get independent key hierarchies: the master key is drawn
        // from the RNG, not derived from the volume key.
        let (_, a) = provision(&VK, &mut CountingEntropy::new(10)).expect("provision a");
        let (_, b) = provision(&VK, &mut CountingEntropy::new(20)).expect("provision b");
        assert_ne!(a.content_key, b.content_key);
        assert_ne!(a.mac_key, b.mac_key);
        assert_ne!(a.dedupe_domain, b.dedupe_domain);
    }

    #[test]
    fn provision_fails_closed_when_entropy_is_unavailable() {
        assert_eq!(
            provision(&VK, &mut DeadEntropy).err(),
            Some(DriverError::DeviceFault)
        );
    }

    #[test]
    fn header_encode_decode_round_trips() {
        let (header, _) = provision(&VK, &mut CountingEntropy::new(4)).expect("provision");
        let mut bytes = [0u8; CRYPTO_HEADER_LEN];
        header.encode(&mut bytes);
        let decoded = CryptoHeader::decode(&bytes).expect("decode");
        let keys = unwrap(&VK, &decoded).expect("unwrap decoded");
        let direct = unwrap(&VK, &header).expect("unwrap direct");
        assert_eq!(keys.content_key, direct.content_key);
    }

    #[test]
    fn region_round_trips_and_detects_tampering() {
        let (_, keys) = provision(&VK, &mut CountingEntropy::new(5)).expect("provision");
        let plain = *b"a directory entry name or file data block content!!";
        let mut region = plain;
        let mut trailer = [0u8; CRYPTO_TRAILER];
        encrypt_region(&keys.content_key, &mut region, &mut trailer, 42, 7).expect("encrypt");
        assert_ne!(region, plain, "ciphertext must differ from plaintext");
        let mut ok = region;
        decrypt_region(&keys.content_key, &mut ok, &trailer, 42).expect("decrypt");
        assert_eq!(ok, plain);
        // A flipped ciphertext byte is rejected.
        let mut bad = region;
        bad[0] ^= 0x01;
        assert!(decrypt_region(&keys.content_key, &mut bad, &trailer, 42).is_err());
        // The wrong physical-address binding is rejected.
        let mut moved = region;
        assert!(decrypt_region(&keys.content_key, &mut moved, &trailer, 43).is_err());
    }
}
