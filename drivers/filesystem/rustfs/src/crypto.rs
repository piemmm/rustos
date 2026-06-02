//! `RustFS` per-volume key hierarchy and at-rest encryption
//! (`docs/src/filesystem/rustfs-spec.md` §5, §7).
//!
//! `RustFS` is encrypted by default and has no plaintext mode: there is no
//! code path that lays out an unencrypted volume. Every volume is created with
//! a caller-supplied [`VolumeKey`] (the installer's / recovery flow's key
//! material). From it `RustFS` grows the key hierarchy the spec fixes:
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
//! fails the AEAD authentication and the mount is refused, fail-closed
//! (`AGENTS.md` §5.4), never a panic (§2.9).
//!
//! All primitives come through `lib/crypto` (`AGENTS.md` §2.12): the KDF
//! ([`rustos_crypto::derive_key`]), the metadata MAC (HMAC-SHA256), and the
//! AEAD (ChaCha20-Poly1305). Nothing here hand-rolls a primitive.
//!
//! # No platform RNG (yet)
//!
//! This driver has no entropy source of its own, so the per-volume master key
//! and its wrapping salt are derived deterministically from the supplied
//! volume key and the volume UUID at format time and wrapped on disk. The
//! security property the spec requires — that the volume cannot be read
//! without the volume key — holds regardless: the master key is recovered only
//! by unsealing the on-disk wrapped blob with a wrapping key derived from the
//! correct volume key. Sourcing the master key from the platform RNG (so it is
//! independent of the volume key and re-wrappable on key change) is a later
//! refinement, exactly as the random per-volume UUID is (`crate::derive_uuid`).

use rustos_crypto::{
    derive_key, open, seal, sha256, AeadError, AeadKey, AeadNonce, AeadTag, MacKey, AEAD_NONCE_LEN,
};

/// Length, in bytes, of a volume key — the caller-supplied key that unwraps a
/// `RustFS` volume. A 256-bit key matches the AEAD and MAC key widths.
pub const VOLUME_KEY_LEN: usize = 32;

/// A caller-supplied volume key: the installer's, recovery flow's, or storage
/// policy service's key material that unlocks a `RustFS` volume (§7). `RustFS`
/// never persists it; only the master key it unwraps is stored, and that only
/// in AEAD-sealed form.
pub type VolumeKey = [u8; VOLUME_KEY_LEN];

/// Bytes of per-block crypto trailer appended to every encrypted data and
/// directory block: a [`rustos_crypto::AEAD_NONCE_LEN`]-byte nonce followed by
/// a [`rustos_crypto::AEAD_TAG_LEN`]-byte authentication tag.
pub const CRYPTO_TRAILER: usize = rustos_crypto::AEAD_NONCE_LEN + rustos_crypto::AEAD_TAG_LEN;

/// On-disk size of the crypto discovery header stored in a superblock slot's
/// plaintext payload region: a 16-byte salt, the 32-byte AEAD-wrapped master
/// key, the wrap nonce, and the wrap tag.
pub const CRYPTO_HEADER_LEN: usize =
    SALT_LEN + VOLUME_KEY_LEN + rustos_crypto::AEAD_NONCE_LEN + rustos_crypto::AEAD_TAG_LEN;

/// Length, in bytes, of the per-volume wrapping salt.
const SALT_LEN: usize = 16;

// Domain-separating KDF context labels. Each derived key gets its own stable
// label so no two uses of a parent key ever collide (`rustos_crypto::kdf`).
const CTX_WRAP: &[u8] = b"rustfs/wrap-key";
const CTX_MASTER: &[u8] = b"rustfs/master-key";
const CTX_WRAP_NONCE: &[u8] = b"rustfs/wrap-nonce";
const CTX_META: &[u8] = b"rustfs/meta-mac";
const CTX_FILENAME: &[u8] = b"rustfs/filename";
const CTX_CONTENT: &[u8] = b"rustfs/content";

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
}

/// The plaintext crypto discovery header stored in every superblock slot.
#[derive(Copy, Clone)]
pub struct CryptoHeader {
    salt: [u8; SALT_LEN],
    wrapped_master: [u8; VOLUME_KEY_LEN],
    wrap_nonce: AeadNonce,
    wrap_tag: AeadTag,
}

/// Derive the per-volume wrapping salt from the volume UUID. The salt is
/// public (it is stored in the clear) and only needs to be stable and unique
/// per volume, so it is hashed from the UUID through `lib/crypto`.
fn derive_salt(uuid: u128) -> [u8; SALT_LEN] {
    let mut input = [0u8; 16 + 16];
    input[..16].copy_from_slice(b"rustfs/salt\0\0\0\0\0");
    input[16..].copy_from_slice(&uuid.to_le_bytes());
    let digest = sha256(&input);
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&digest[..SALT_LEN]);
    salt
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
/// caller's volume key and the per-volume salt.
fn wrapping_key(volume_key: &VolumeKey, salt: &[u8; SALT_LEN]) -> [u8; 32] {
    derive_with_salt(volume_key, CTX_WRAP, salt)
}

/// The deterministic wrap nonce: unique per volume because the salt is, so the
/// `(wrapping key, nonce)` pair never repeats across volumes.
fn wrap_nonce(volume_key: &VolumeKey, salt: &[u8; SALT_LEN]) -> AeadNonce {
    let full = derive_with_salt(volume_key, CTX_WRAP_NONCE, salt);
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    nonce.copy_from_slice(&full[..AEAD_NONCE_LEN]);
    nonce
}

/// Grow the working key set from the master key.
fn derive_volume_keys(master: &[u8; 32]) -> VolumeKeys {
    VolumeKeys {
        mac_key: derive_key(master, CTX_META),
        filename_key: derive_key(master, CTX_FILENAME),
        content_key: derive_key(master, CTX_CONTENT),
    }
}

impl CryptoHeader {
    /// Encode the header into the first [`CRYPTO_HEADER_LEN`] bytes of `out`.
    ///
    /// # Errors
    ///
    /// [`AeadError::Authentication`] is never produced here; the function is
    /// infallible for a correctly-sized buffer and panics are avoided by the
    /// caller always passing a full payload region (`AGENTS.md` §2.9).
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
        let mut wrap_tag = [0u8; rustos_crypto::AEAD_TAG_LEN];
        wrap_tag.copy_from_slice(&bytes[off..off + rustos_crypto::AEAD_TAG_LEN]);
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
/// Derives the per-volume salt, wrapping key, and master key from the
/// caller-supplied `volume_key` and the volume `uuid`, AEAD-seals the master
/// key, and returns the on-disk [`CryptoHeader`] plus the working
/// [`VolumeKeys`]. No plaintext key is ever returned for storage; the header
/// carries only the wrapped master key (§7).
///
/// # Errors
///
/// [`AeadError`] if sealing the master key fails (unreachable for the
/// fixed-size buffer, but surfaced rather than panicked, `AGENTS.md` §2.9).
pub fn provision(
    volume_key: &VolumeKey,
    uuid: u128,
) -> Result<(CryptoHeader, VolumeKeys), AeadError> {
    let salt = derive_salt(uuid);
    let wrapping = wrapping_key(volume_key, &salt);
    let nonce = wrap_nonce(volume_key, &salt);
    let master = derive_with_salt(volume_key, CTX_MASTER, &salt);
    let mut wrapped_master = master;
    let wrap_tag = seal(&wrapping, &nonce, &salt, &mut wrapped_master)?;
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
/// mount, fail-closed (`AGENTS.md` §5.4).
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
/// closed (`AGENTS.md` §5.4), never returning mis-decrypted bytes.
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
    let mut tag = [0u8; rustos_crypto::AEAD_TAG_LEN];
    tag.copy_from_slice(&trailer[AEAD_NONCE_LEN..CRYPTO_TRAILER]);
    open(key, &nonce, &phys.to_le_bytes(), region, &tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VK: VolumeKey = [0x11; VOLUME_KEY_LEN];
    const UUID: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;

    #[test]
    fn provision_then_unwrap_round_trips_keys() {
        let (header, keys) = provision(&VK, UUID).expect("provision");
        let recovered = unwrap(&VK, &header).expect("unwrap");
        assert_eq!(keys.mac_key, recovered.mac_key);
        assert_eq!(keys.filename_key, recovered.filename_key);
        assert_eq!(keys.content_key, recovered.content_key);
    }

    #[test]
    fn wrong_volume_key_fails_to_unwrap() {
        let (header, _) = provision(&VK, UUID).expect("provision");
        let mut wrong = VK;
        wrong[0] ^= 0x01;
        assert!(unwrap(&wrong, &header).is_err());
    }

    #[test]
    fn derived_keys_are_independent() {
        let (_, keys) = provision(&VK, UUID).expect("provision");
        assert_ne!(keys.mac_key, keys.filename_key);
        assert_ne!(keys.mac_key, keys.content_key);
        assert_ne!(keys.filename_key, keys.content_key);
    }

    #[test]
    fn header_encode_decode_round_trips() {
        let (header, _) = provision(&VK, UUID).expect("provision");
        let mut bytes = [0u8; CRYPTO_HEADER_LEN];
        header.encode(&mut bytes);
        let decoded = CryptoHeader::decode(&bytes).expect("decode");
        let keys = unwrap(&VK, &decoded).expect("unwrap decoded");
        let direct = unwrap(&VK, &header).expect("unwrap direct");
        assert_eq!(keys.content_key, direct.content_key);
    }

    #[test]
    fn region_round_trips_and_detects_tampering() {
        let (_, keys) = provision(&VK, UUID).expect("provision");
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
