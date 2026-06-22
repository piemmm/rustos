//! Build-time signed `.rxe` driver-bundle composer shared by the QEMU
//! integration fixtures that lay a kernel-trusted driver into the system
//! (`AGENTS.md` §2.2).
//!
//! A signed driver bundle is a [`rustos_abi::DriverManifest`] header, the manifest's
//! capability body, its bind table, and the program payload, with the
//! manifest's Ed25519 signature taken over
//! `header[..WIRE_LEN-64] || cap_body || bind_table || payload` — exactly
//! what `rustos_drvhost::Host::verify_signature` reconstructs (`AGENTS.md`
//! §8 / §9 / §2.17 — the payload program is authenticated, not just the
//! header). Several build scripts need to emit such a bundle (the
//! driver-spawn vertical, the autoload-root image fixture), so the one
//! definition of how a bundle is assembled and signed lives here rather
//! than re-rolled in each (`AGENTS.md` §2.2). It is feature-gated
//! (`driver-image`) so the crypto dependency is pulled in only by the
//! build scripts that actually sign a bundle, not by every harness
//! consumer.

use ed25519_dalek::{Signer, SigningKey};

use rustos_abi::{
    CapabilityId, DriverBindKey, DriverKind, DriverManifest, ABI_VERSION_CURRENT,
    DRIVER_MANIFEST_MAGIC,
};

/// A signed driver bundle plus the public key it was signed with.
pub struct SignedDriverImage {
    /// The wire bytes: `manifest || cap_body || bind_table || payload`.
    pub image: Vec<u8>,
    /// The Ed25519 public key the bundle was signed with — the trust
    /// anchor a verifier must hold for the bundle to be admitted.
    pub signer_pubkey: [u8; 32],
}

/// Assemble and Ed25519-sign a `.rxe` driver bundle from `seed`.
///
/// The bundle declares `kind`, requests `caps`, carries `bind_keys` as its
/// §18.3 bind table, stamps `syscall_table_hash` (the gate refuses a
/// mismatch, §9), and wraps `payload` as the loadable program image. The
/// signature covers `header[..WIRE_LEN-64] || cap_body || bind_table ||
/// payload` so the *payload* is authenticated and cannot be substituted
/// after signing (`AGENTS.md` §2.17).
///
/// # Panics
///
/// Panics if `caps`/`bind_keys` exceed the manifest's count fields — a
/// programming error in the fixture, never reachable for the small,
/// fixed sets the verticals pass.
#[must_use]
pub fn build_signed_driver_image(
    seed: &[u8; 32],
    kind: DriverKind,
    caps: &[CapabilityId],
    bind_keys: &[DriverBindKey],
    syscall_table_hash: [u8; 32],
    payload: &[u8],
) -> SignedDriverImage {
    let signing_key = SigningKey::from_bytes(seed);
    let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();

    let mut manifest = DriverManifest {
        magic: DRIVER_MANIFEST_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        kind,
        bind_key_count: u8::try_from(bind_keys.len()).expect("bind keys fit u8"),
        capability_count: u16::try_from(caps.len()).expect("caps fit u16"),
        syscall_table_hash,
        signer_pubkey,
        signature: [0u8; 64],
    };

    let mut cap_body = Vec::with_capacity(caps.len() * 2);
    for c in caps {
        cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
    }
    let mut bind_body = Vec::new();
    for k in bind_keys {
        bind_body.extend_from_slice(&k.to_le_bytes());
    }

    let header = manifest.to_le_bytes();
    let signed_end = DriverManifest::WIRE_LEN - 64;
    let mut signed_message = Vec::new();
    signed_message.extend_from_slice(&header[..signed_end]);
    signed_message.extend_from_slice(&cap_body);
    signed_message.extend_from_slice(&bind_body);
    signed_message.extend_from_slice(payload);
    manifest.signature = signing_key.sign(&signed_message).to_bytes();

    let mut image = Vec::new();
    image.extend_from_slice(&manifest.to_le_bytes());
    image.extend_from_slice(&cap_body);
    image.extend_from_slice(&bind_body);
    image.extend_from_slice(payload);

    SignedDriverImage {
        image,
        signer_pubkey,
    }
}
