//! Mock-driver fixtures shared by every integration test in
//! `userland/system/drvhost/tests/`.
//!
//! The fixtures here build well-formed `.rxe` images on the fly so each
//! test can exercise exactly one verification gate without re-deriving
//! the on-wire layout. They also expose a minimal in-memory
//! [`ImageSource`] (`MemSource`), in-process [`DriverSpawner`]
//! implementations (`SingleSpawner`, `NoDriverSpawner`,
//! `FailingSpawner`), and a recording log [`Sink`] (`RecordingSink`)
//! that drive the host through its lifecycle without touching any real
//! filesystem.
//!
//! These fixtures are **test-only**. They never appear in production
//! builds (`#[cfg(test)]`-only via the `tests/` integration harness) and
//! they intentionally register a function-pointer driver in-process —
//! the spawner layer is the production seam at which a real `.rxe`
//! image is spawned into its own process and registered over IPC; that
//! mechanism is the Stage 4.HW process-spawn increment (`PLAN.md`).

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use ed25519_dalek::{Signer, SigningKey};
use rustos_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, DriverKind, DriverManifest,
    DRIVER_MANIFEST_MAGIC,
};
use rustos_crypto::Ed25519PublicKey;
use rustos_drvhost::{
    DriverSpawner, Event as LogEvent, Field, ImageSource, Sink, SpawnContext, SpawnRegisterError,
};

/// Build a `.rxe` image: signed manifest header + capability body +
/// optional payload bytes. The bind table is empty.
///
/// `caps` is the requested capability set in declaration order. The
/// signature is computed with `signing_key` over
/// `header[..WIRE_LEN-64] || cap_body || bind_table`, matching the
/// verifier in `crate::host`.
pub fn build_signed_image(
    signing_key: &SigningKey,
    kind: DriverKind,
    syscall_table_hash: [u8; 32],
    caps: &[CapabilityId],
    payload: &[u8],
) -> Vec<u8> {
    build_signed_image_with_bind_keys(signing_key, kind, syscall_table_hash, caps, &[], payload)
}

/// [`build_signed_image`] with an explicit bind table (`AGENTS.md`
/// §18.3) between the capability body and the payload.
pub fn build_signed_image_with_bind_keys(
    signing_key: &SigningKey,
    kind: DriverKind,
    syscall_table_hash: [u8; 32],
    caps: &[CapabilityId],
    bind_keys: &[DriverBindKey],
    payload: &[u8],
) -> Vec<u8> {
    let signer_pubkey: [u8; 32] = signing_key.verifying_key().to_bytes();
    let mut header_no_sig = Vec::with_capacity(DriverManifest::WIRE_LEN - 64);
    let count = u16::try_from(caps.len()).expect("caps fit in u16");
    let bind_key_count = u8::try_from(bind_keys.len()).expect("bind keys fit in u8");
    // Build a temporary manifest with a zero signature so we can encode
    // the prefix the signer must cover.
    let mut manifest = DriverManifest {
        magic: DRIVER_MANIFEST_MAGIC,
        abi_version: rustos_abi::ABI_VERSION_CURRENT,
        kind,
        bind_key_count,
        capability_count: count,
        syscall_table_hash,
        signer_pubkey,
        signature: [0u8; 64],
    };
    let encoded = manifest.to_le_bytes();
    header_no_sig.extend_from_slice(&encoded[..DriverManifest::WIRE_LEN - 64]);
    let mut cap_body = Vec::with_capacity(caps.len() * 2);
    for c in caps {
        cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
    }
    let mut bind_table = Vec::with_capacity(bind_keys.len() * DriverBindKey::WIRE_LEN);
    for k in bind_keys {
        bind_table.extend_from_slice(&k.to_le_bytes());
    }
    // The signed message covers the payload too (`host::verify_signature`):
    // for a user-space driver the payload is the program the gate spawns,
    // so it must be authenticated (`AGENTS.md` §8 / §2.17).
    let mut signed_message =
        Vec::with_capacity(header_no_sig.len() + cap_body.len() + bind_table.len() + payload.len());
    signed_message.extend_from_slice(&header_no_sig);
    signed_message.extend_from_slice(&cap_body);
    signed_message.extend_from_slice(&bind_table);
    signed_message.extend_from_slice(payload);
    let sig = signing_key.sign(&signed_message);
    manifest.signature = sig.to_bytes();
    let mut out = Vec::with_capacity(
        DriverManifest::WIRE_LEN + cap_body.len() + bind_table.len() + payload.len(),
    );
    out.extend_from_slice(&manifest.to_le_bytes());
    out.extend_from_slice(&cap_body);
    out.extend_from_slice(&bind_table);
    out.extend_from_slice(payload);
    out
}

/// A deterministic 32-byte test seed; the same seed yields the same
/// public key on every run so log fixtures remain stable.
pub const TEST_SEED: [u8; 32] = [
    0x42, 0x6e, 0x47, 0x2c, 0x90, 0x12, 0xd1, 0x35, 0x99, 0xa0, 0x77, 0x80, 0x6a, 0xa3, 0x21, 0x18,
    0x73, 0xd6, 0xc0, 0x55, 0xe2, 0xb1, 0x47, 0x83, 0x18, 0x44, 0x91, 0x55, 0xee, 0x66, 0x9c, 0x0a,
];

/// Build a [`SigningKey`] from `TEST_SEED`.
pub fn test_signing_key() -> SigningKey {
    SigningKey::from_bytes(&TEST_SEED)
}

/// Build a second, distinct signing key — used in tests that check the
/// trust-anchor gate refuses a key that is not on the host's list.
pub fn alternative_signing_key() -> SigningKey {
    let mut seed = TEST_SEED;
    seed[0] ^= 0xFF;
    SigningKey::from_bytes(&seed)
}

/// Convert a `SigningKey` into the `rustos_crypto::Ed25519PublicKey`
/// the host stores on its trust anchor list.
pub fn pubkey_of(sk: &SigningKey) -> Ed25519PublicKey {
    let bytes = sk.verifying_key().to_bytes();
    Ed25519PublicKey::from_bytes(&bytes).expect("verifying key bytes are well-formed")
}

/// In-memory image source. Maps a logical `&str` path to image bytes.
pub struct MemSource {
    pub images: BTreeMap<String, Vec<u8>>,
    /// Counts how many times `read` was called per path. Used by tests
    /// that assert reload re-reads.
    pub reads: RefCell<BTreeMap<String, u32>>,
}

impl MemSource {
    pub fn new() -> Self {
        Self {
            images: BTreeMap::new(),
            reads: RefCell::new(BTreeMap::new()),
        }
    }
}

impl ImageSource for MemSource {
    fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), rustos_abi::Errno> {
        *self.reads.borrow_mut().entry(path.to_string()).or_insert(0) += 1;
        match self.images.get(path) {
            Some(bytes) => {
                buf.extend_from_slice(bytes);
                Ok(())
            }
            None => Err(rustos_abi::Errno::NotFound),
        }
    }
}

thread_local! {
    /// Count of times the fixed `register` entry point has been called
    /// on the current thread. Thread-local rather than a process-global
    /// static so tests running in parallel (each on its own harness
    /// thread, with `register` invoked synchronously on that thread)
    /// never observe or reset each other's count.
    static REGISTER_CALLS: Cell<u64> = const { Cell::new(0) };
}

/// Reset this thread's register-call counter. Tests that observe the
/// counter call this at the top of the test to start from a known base.
pub fn reset_register_calls() {
    REGISTER_CALLS.with(|c| c.set(0));
}

/// Read this thread's register-call counter.
pub fn register_calls() -> u64 {
    REGISTER_CALLS.with(Cell::get)
}

/// Driver `register` entry point: bumps the counter and yields a fixed
/// non-zero handle. Returning `Ok` exercises the host's success path;
/// the test crate provides [`failing_register`] for the negative path.
pub fn mock_register(_host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    REGISTER_CALLS.with(|c| c.set(c.get() + 1));
    DriverHandle::from_raw(0x00C0_FFEE)
}

/// Driver `register` entry point that always fails. Surfaces
/// `HostError::DriverRegisterFailed`.
pub fn failing_register(_host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    Err(DriverError::DeviceFault)
}

/// Spawner that registers every verified manifest in-process through
/// [`mock_register`].
pub struct SingleSpawner;
impl DriverSpawner for SingleSpawner {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError> {
        mock_register(ctx.host).map_err(SpawnRegisterError::Register)
    }
}

/// Spawner that has no driver program for any manifest.
pub struct NoDriverSpawner;
impl DriverSpawner for NoDriverSpawner {
    fn spawn_and_register(
        &self,
        _ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError> {
        Err(SpawnRegisterError::NoDriver)
    }
}

/// Spawner that registers every verified manifest in-process through
/// [`failing_register`].
pub struct FailingSpawner;
impl DriverSpawner for FailingSpawner {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError> {
        failing_register(ctx.host).map_err(SpawnRegisterError::Register)
    }
}

/// One captured log record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedEvent {
    pub id: u32,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

/// Sink that copies every event into an internal `RefCell` for the
/// test to inspect after the host operation returns.
pub struct RecordingSink {
    pub events: RefCell<Vec<CapturedEvent>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self {
            events: RefCell::new(Vec::new()),
        }
    }
    pub fn last_id(&self) -> Option<u32> {
        self.events.borrow().last().map(|e| e.id)
    }
    pub fn ids(&self) -> Vec<u32> {
        self.events.borrow().iter().map(|e| e.id).collect()
    }
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &LogEvent<'_>) {
        let fields = event
            .fields
            .iter()
            .map(|f: &Field<'_>| (f.key.to_string(), f.value.to_string()))
            .collect();
        self.events.borrow_mut().push(CapturedEvent {
            id: event.id.0,
            message: event.message.to_string(),
            fields,
        });
    }
}
