//! The sealed scope: the per-account master secret, the per-application key
//! derived from it, and the sealed document record on the volume.
//!
//! ```text
//! per-account master secret          (32 random bytes, drawn once per account)
//!   └─ derive_key(master, "tairix-appdata-secret/v1" ‖ 0x00 ‖ publisher ‖ bundle-id)
//!        └─ per-(account, application) AEAD key
//! ```
//!
//! Every primitive comes from `lib/crypto` — the single-block HKDF-Expand the
//! `ARXFS` key hierarchy derives its own subkeys through, and
//! ChaCha20-Poly1305. Nothing here is a new construction, only a new
//! domain-separating context label.
//!
//! The derivation binds the **publisher** rather than the signing key, so a
//! release signed with a fresh build key still opens the vault it wrote while a
//! different developer squatting the same bundle identifier derives a different
//! key. The AEAD authenticates the record, so a damaged or altered sealed
//! document is refused rather than parsed.
//!
//! The master secret is stored as drawn, in the gated store root, and is
//! deliberately **not** wrapped under a second key: there is no second secret
//! to wrap it with yet, so a wrap would use a key derivable by anyone who could
//! read the record. Hence no protector type and no keyslot here — the record
//! carries a version, and the stage that brings a login-passphrase protector or
//! TPM sealing (`plans/TPM.md`) reshapes it in place.
//!
//! What protects the record at rest, and what the sealing buys behind the gate,
//! are `docs/src/userland/confd.md`.

use alloc::vec::Vec;

use tairix_abi::appinfo::{PublisherId, PUBLISHER_ID_LEN};
use tairix_abi::{AppIdentity, Errno};
use tairix_appconf::{Document, MAX_DOCUMENT_LEN};
use tairix_crypto::{derive_key, open, seal, AeadNonce, AeadTag, AEAD_NONCE_LEN, AEAD_TAG_LEN};
use zeroize::Zeroize;

/// Source of cryptographic randomness for the sealed scope: the per-account
/// master secret and every seal's nonce.
///
/// The service's seam onto the kernel random subsystem, injected by the
/// composition root exactly as the store filesystem is — so the whole sealed
/// scope is exercised on the host, and the engine itself never reaches for a
/// generator of its own. It mirrors the seam `ARXFS` draws its volume key
/// material through.
pub trait Entropy {
    /// Fill the whole of `out` with cryptographically secure random bytes.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the generator reports — [`Errno::EntropyNotReady`]
    /// before the kernel generator is seeded. On an error the contents of
    /// `out` are unspecified and are never used: nothing is sealed and no
    /// master secret is created, so no key material is ever derived from a
    /// failed draw.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno>;
}

/// Length, in bytes, of a per-account app-data master secret. One AEAD key
/// width, so it drops straight into the derivation with no truncation.
pub const MASTER_SECRET_LEN: usize = tairix_crypto::DERIVED_KEY_LEN;

/// Domain-separating context label of the per-application key derivation.
///
/// The master secret derives nothing else, and the label makes sure it never
/// can be made to: a key derived for any other purpose under the same secret
/// can never coincide with an application's vault key.
const SECRET_CONTEXT: &[u8] = b"tairix-appdata-secret/v1";

/// Magic identifying a master-secret record (`"AVMK"` little-endian).
const MASTER_MAGIC: u32 = u32::from_le_bytes(*b"AVMK");

/// Version of the master-secret record layout.
const MASTER_VERSION: u16 = 1;

/// Byte offset of the master record's account uid.
const MASTER_UID_OFFSET: usize = HEADER_LEN;

/// Byte offset of the master record's secret.
const MASTER_SECRET_OFFSET: usize = MASTER_UID_OFFSET + 4;

/// Magic identifying a sealed document (`"AVLT"` little-endian).
const VAULT_MAGIC: u32 = u32::from_le_bytes(*b"AVLT");

/// Version of the sealed-document record layout.
const VAULT_VERSION: u16 = 1;

/// Byte length of a record's fixed header: a magic, a version, and a reserved
/// pair that must be zero.
///
/// Both records begin with one, and each carries its whole header as a single
/// constant ([`MASTER_HEADER`], [`VAULT_HEADER`]) rather than as three fields
/// to read and compare: every byte of it is pinned, so recognising a record is
/// one comparison against the constant the writer used.
const HEADER_LEN: usize = 8;

/// The master-secret record's fixed header.
const MASTER_HEADER: [u8; HEADER_LEN] = header(MASTER_MAGIC, MASTER_VERSION);

/// The sealed document's fixed header.
///
/// It is also the AEAD's associated data. The part of a record an AEAD does not
/// encrypt is the part its associated data exists for, and using the same
/// constant for both is what makes the structural check and the authentication
/// belt and braces rather than one covering for the other. Nothing else needs
/// binding: the account, the publisher, and the bundle identifier are already
/// bound by the key the record can only be opened with.
const VAULT_HEADER: [u8; HEADER_LEN] = header(VAULT_MAGIC, VAULT_VERSION);

/// `magic ‖ version ‖ 0u16`, the header both records begin with.
const fn header(magic: u32, version: u16) -> [u8; HEADER_LEN] {
    let magic = magic.to_le_bytes();
    let version = version.to_le_bytes();
    [
        magic[0], magic[1], magic[2], magic[3], version[0], version[1], 0, 0,
    ]
}

/// Byte offset of the sealed record's nonce.
const VAULT_NONCE_OFFSET: usize = HEADER_LEN;

/// Byte offset of the sealed record's authentication tag.
const VAULT_TAG_OFFSET: usize = VAULT_NONCE_OFFSET + AEAD_NONCE_LEN;

/// Byte length of the sealed record's header, before the ciphertext.
pub const VAULT_HEADER_LEN: usize = VAULT_TAG_OFFSET + AEAD_TAG_LEN;

/// Why a sealed-scope operation was refused.
///
/// Each variant is a distinct fact an operator investigates differently: a
/// missing or unreadable master secret is the account's key material gone, a
/// record that is not a vault at all is a truncated write or bit rot, a tag
/// that does not verify is tampering or the wrong key, and an entropy refusal
/// is the generator not being ready. None of them is ever answered as an empty
/// document: "your secrets are damaged" and "you have no secrets" must not
/// look alike.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VaultError {
    /// A sealed document exists but the account's master secret does not, or
    /// the record that should hold it attests nothing. Nothing is unsealed and
    /// **nothing is replaced**: drawing a fresh master here would make every
    /// existing vault permanently unreadable while looking like a clean start.
    MasterSecretRefused,
    /// The sealed document on the volume is not a well-formed record.
    VaultMalformed,
    /// The sealed document's authentication failed: it was altered, or the key
    /// that opens it is not this application's.
    VaultUnsealFailed,
    /// The random generator could not serve a draw, so no master secret was
    /// created and nothing was sealed.
    EntropyUnavailable,
}

impl VaultError {
    /// A stable one-line reason for the audit record.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::MasterSecretRefused => "the account's app-data master secret attests nothing",
            Self::VaultMalformed => "the sealed document is not a well-formed vault record",
            Self::VaultUnsealFailed => "the sealed document failed authentication",
            Self::EntropyUnavailable => "the random generator could not serve a draw",
        }
    }
}

/// One account's app-data master secret.
///
/// The bytes never leave the type except through [`Self::encode`], which the
/// one creating call site wipes as soon as the record is on the volume. It
/// implements no [`Debug`](core::fmt::Debug) and no rendering trait, so it
/// cannot reach a log or a panic message by construction, and it is wiped when
/// it goes out of scope — which is at the end of the request that read it,
/// because the service deliberately caches no master secret.
pub struct MasterSecret {
    bytes: [u8; MASTER_SECRET_LEN],
}

impl MasterSecret {
    /// Encoded size of the record: magic (4), version (2), a reserved pair
    /// (2), the account uid (4), then the secret.
    pub const WIRE_LEN: usize = MASTER_SECRET_OFFSET + MASTER_SECRET_LEN;

    /// Draw a fresh master secret for the account `uid` owns.
    ///
    /// # Errors
    ///
    /// [`VaultError::EntropyUnavailable`] when the generator cannot serve the
    /// draw, or when it answers all zeroes — which a working CSPRNG will not
    /// do in the lifetime of the universe, and a broken one might do on its
    /// first call. Either way nothing is created.
    pub fn draw<E: Entropy + ?Sized>(entropy: &mut E) -> Result<Self, VaultError> {
        let mut bytes = [0u8; MASTER_SECRET_LEN];
        if entropy.fill(&mut bytes).is_err() {
            bytes.zeroize();
            return Err(VaultError::EntropyUnavailable);
        }
        Self::from_bytes(&mut bytes).ok_or(VaultError::EntropyUnavailable)
    }

    /// Take raw secret bytes out of `bytes`, refusing the all-zero value.
    ///
    /// It **consumes** the caller's buffer — the bytes are copied in and the
    /// caller's copy wiped — which is why the parameter is `&mut`. An array of
    /// bytes is `Copy`, so a by-value parameter would leave a live copy of the
    /// secret on the caller's stack; doing the wipe here rather than at each
    /// call site is what makes that impossible to forget.
    ///
    /// The all-zero value is refused because a file allocated but never written
    /// reads as zeroes, and that must never be mistaken for a usable key: it
    /// would make two accounts share one derived key and turn a corruption into
    /// a silent cryptographic downgrade.
    fn from_bytes(bytes: &mut [u8; MASTER_SECRET_LEN]) -> Option<Self> {
        let taken = Self { bytes: *bytes };
        bytes.zeroize();
        if taken.bytes.iter().all(|byte| *byte == 0) {
            return None;
        }
        Some(taken)
    }

    /// Encode the record for the volume.
    ///
    /// The returned buffer holds the secret; the caller wipes it once the
    /// write has landed.
    #[must_use]
    pub fn encode(&self, uid: u32) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[..HEADER_LEN].copy_from_slice(&MASTER_HEADER);
        out[MASTER_UID_OFFSET..MASTER_SECRET_OFFSET].copy_from_slice(&uid.to_le_bytes());
        out[MASTER_SECRET_OFFSET..].copy_from_slice(&self.bytes);
        out
    }

    /// Decode the master secret of the account `uid` owns, or [`None`] for
    /// anything that is not exactly one.
    ///
    /// A wrong magic, an unknown version, a dirty reserved pair, a length that
    /// is not the record's, another account's uid, or the all-zero secret all
    /// refuse. Binding the uid is what stops a record copied between homes
    /// from silently giving two accounts one key hierarchy.
    #[must_use]
    pub fn decode(bytes: &[u8], uid: u32) -> Option<Self> {
        if bytes.len() != Self::WIRE_LEN {
            return None;
        }
        if bytes.get(..HEADER_LEN) != Some(&MASTER_HEADER[..]) {
            return None;
        }
        if read_u32(bytes, MASTER_UID_OFFSET)? != uid {
            return None;
        }
        let mut raw: [u8; MASTER_SECRET_LEN] = bytes[MASTER_SECRET_OFFSET..].try_into().ok()?;
        Self::from_bytes(&mut raw)
    }

    /// The AEAD key sealing `identity`'s vault in this account.
    #[must_use]
    pub fn app_key(&self, identity: &AppIdentity) -> VaultKey {
        VaultKey {
            bytes: derive_key(
                &self.bytes,
                &context(identity.publisher(), identity.bundle_id()),
            ),
        }
    }
}

impl Drop for MasterSecret {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// One application's vault key in one account.
///
/// Derived on demand and wiped when it goes out of scope. Like
/// [`MasterSecret`] it implements no rendering trait, so it cannot be logged.
pub struct VaultKey {
    bytes: [u8; tairix_crypto::DERIVED_KEY_LEN],
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// The domain-separated derivation context for one application in one account.
///
/// Every field before the identifier is fixed-width and the label is fixed, so
/// the concatenation is unambiguous: no two (publisher, identifier) pairs can
/// produce the same context.
fn context(publisher: PublisherId, bundle_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(SECRET_CONTEXT.len() + 1 + PUBLISHER_ID_LEN + bundle_id.len());
    out.extend_from_slice(SECRET_CONTEXT);
    out.push(0);
    out.extend_from_slice(publisher.as_bytes());
    out.extend_from_slice(bundle_id.as_bytes());
    out
}

/// Seal `document` under `key` as a sealed-document record.
///
/// The nonce is drawn fresh for every seal. A counter would need durable state
/// that a crash could rewind — and reusing a `(key, nonce)` pair under
/// ChaCha20-Poly1305 is catastrophic — where a 96-bit random nonce cannot
/// repeat in the handful of writes an application's secrets ever see.
///
/// The rendered text's own allocation is encrypted in place, so the plaintext
/// is overwritten by the ciphertext rather than freed beside a copy of itself,
/// and the only bytes that leave this function are ciphertext.
///
/// # Errors
///
/// [`VaultError::EntropyUnavailable`] when the nonce cannot be drawn, and
/// [`VaultError::VaultMalformed`] when the rendered document is past the
/// format's own byte bound or the cipher refuses the message — refused rather
/// than written, so a vault on the volume is always one this engine can open.
pub fn seal_document<E: Entropy + ?Sized>(
    key: &VaultKey,
    entropy: &mut E,
    document: &Document,
) -> Result<Vec<u8>, VaultError> {
    let mut text = document.render();
    if text.len() > MAX_DOCUMENT_LEN {
        text.zeroize();
        return Err(VaultError::VaultMalformed);
    }
    let mut record = alloc::vec![0u8; VAULT_HEADER_LEN];
    record[..HEADER_LEN].copy_from_slice(&VAULT_HEADER);
    let mut nonce: AeadNonce = [0u8; AEAD_NONCE_LEN];
    if entropy.fill(&mut nonce).is_err() {
        text.zeroize();
        return Err(VaultError::EntropyUnavailable);
    }
    record[VAULT_NONCE_OFFSET..VAULT_TAG_OFFSET].copy_from_slice(&nonce);
    let mut body = text.into_bytes();
    let Ok(tag) = seal(&key.bytes, &nonce, &VAULT_HEADER, &mut body) else {
        body.zeroize();
        return Err(VaultError::VaultMalformed);
    };
    record[VAULT_TAG_OFFSET..VAULT_HEADER_LEN].copy_from_slice(&tag);
    record.extend_from_slice(&body);
    Ok(record)
}

/// Open the sealed-document record `bytes` under `key`.
///
/// # Errors
///
/// [`VaultError::VaultMalformed`] when `bytes` is not a sealed record or the
/// document inside it is outside the format's bounds, and
/// [`VaultError::VaultUnsealFailed`] when the record's authentication fails.
/// Neither is ever answered as an empty document.
pub fn open_document(key: &VaultKey, bytes: &[u8]) -> Result<Document, VaultError> {
    if bytes.len() < VAULT_HEADER_LEN {
        return Err(VaultError::VaultMalformed);
    }
    if bytes.get(..HEADER_LEN) != Some(&VAULT_HEADER[..]) {
        return Err(VaultError::VaultMalformed);
    }
    if bytes.len() - VAULT_HEADER_LEN > MAX_DOCUMENT_LEN {
        return Err(VaultError::VaultMalformed);
    }
    let nonce: AeadNonce = bytes[VAULT_NONCE_OFFSET..VAULT_TAG_OFFSET]
        .try_into()
        .map_err(|_| VaultError::VaultMalformed)?;
    let tag: AeadTag = bytes[VAULT_TAG_OFFSET..VAULT_HEADER_LEN]
        .try_into()
        .map_err(|_| VaultError::VaultMalformed)?;
    let mut body = Vec::from(&bytes[VAULT_HEADER_LEN..]);
    if open(&key.bytes, &nonce, &VAULT_HEADER, &mut body, &tag).is_err() {
        body.zeroize();
        return Err(VaultError::VaultUnsealFailed);
    }
    // The buffer now holds plaintext, so it is wiped whichever way this goes:
    // a parse failure must not leave a secret in freed memory either.
    let parsed = core::str::from_utf8(&body)
        .ok()
        .and_then(|text| Document::parse(text).ok());
    body.zeroize();
    parsed.ok_or(VaultError::VaultMalformed)
}

/// Read a little-endian `u32` at `at`, or [`None`] when the buffer is short.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let field: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(field))
}

#[cfg(test)]
#[path = "vault_tests.rs"]
pub(crate) mod tests;
