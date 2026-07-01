//! Driver host state machine — `load`, `unload`, `reload`.
//!
//! Owns the verified-and-bound set of currently loaded driver modules.
//! Every state transition is audited through [`rustos_log`] (see
//! [`crate::events`]) and the per-record sensitive buffers are wiped
//! through [`crate::zeroize::secure_clear`] on drop.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::driver::VirtioHost;
use rustos_abi::{
    CapabilityId, DriverBindKey, DriverHandle, DriverHost, DriverKind, HwMatchKey, MmioMapper,
    DRIVER_MANIFEST_MAX_BIND_KEYS, DRIVER_MANIFEST_MAX_CAPABILITIES,
};
use rustos_caps::CapabilitySet;
use rustos_crypto::{Ed25519PublicKey, Ed25519Signature};
use rustos_log::{log as log_event, Event, EventId, Field, Level, Sink};
use rustos_virtio::VirtioHostFactory;

use crate::events;
use crate::image::ParsedImage;
use crate::source::ImageSource;
use crate::spawner::{DriverSpawner, SpawnContext, SpawnRegisterError};
use crate::zeroize::secure_clear;
use crate::HostError;

/// Snapshot of one loaded driver returned by [`Host::snapshot`].
///
/// Reading state out of the host this way (rather than exposing the
/// internal `LoadedRecord` directly) keeps the host's invariants
/// encapsulated.
#[derive(Clone, Debug)]
pub struct LoadedSnapshot {
    /// Handle issued for the driver instance.
    pub handle: DriverHandle,
    /// Logical path the host read the image from.
    pub path: String,
    /// Driver kind from the manifest.
    pub kind: DriverKind,
    /// Capability set granted to the driver (subset of the caller's
    /// set at load time).
    pub granted: CapabilitySet,
}

/// Configuration handed to [`Host::new`].
///
/// All fields are borrowed; the host outlives them via `'h`. The
/// caller is expected to construct one config per host process and
/// keep it alive for the host's lifetime.
pub struct HostConfig<'h> {
    /// Set of public keys whose signatures the host will accept.
    pub trusted_signers: &'h [Ed25519PublicKey],
    /// SHA-256 of the kernel syscall table this host was compiled
    /// against. Manifests must carry the same value or be refused.
    pub syscall_table_hash: [u8; 32],
    /// ABI version the host will accept; mismatched manifests are
    /// rejected with [`HostError::ManifestInvalid`] before this field
    /// is even consulted, but exposed here so an out-of-band tool can
    /// query the host's accepted version.
    pub accepted_abi_version: u32,
    /// Storage backend supplying `.rxe` image bytes.
    pub source: &'h dyn ImageSource,
    /// Spawner that completes a verified manifest+payload's registration
    /// in its own protection domain.
    pub spawner: &'h dyn DriverSpawner,
    /// Sink that receives every structured-log [`Event`] the host
    /// emits.
    pub sink: &'h dyn Sink,
    /// Optional factory minting a per-driver [`VirtioHost`].
    ///
    /// `None` for hosts that do not (yet) ship virtio-class plumbing
    /// — the [`DriverHost::virtio_host`] accessor on the driver
    /// view then reports `None` and the driver `register()` impl
    /// must fall back to a no-virtio path or refuse to load. The
    /// kernel binary wires a real implementation here that mints a
    /// `KernelVirtioHost` per driver (per-process
    /// heaps).
    pub virtio_host_factory: Option<&'h dyn VirtioHostFactory>,
    /// Optional MMIO mapper the driver reaches through
    /// [`DriverHost::mmio_mapper`].
    ///
    /// `None` for hosts that do not (yet) ship the MMIO-map facility
    /// — the [`DriverHost::mmio_mapper`] accessor on the driver view
    /// then reports `None` and a bus driver's `register()` impl must
    /// refuse to load. The kernel binary wires a
    /// `KernelMmioMapper` here so an in-kernel bus driver
    /// (`drivers/bus/pcie_brcm`, `drivers/bus/usb`) can map a
    /// device's register window through the capability-gated
    /// `rustos_kernel_sec::map_mmio` path (no
    /// pointer the driver synthesises). The mapper enforces
    /// [`CapabilityId::MMIO_MAP`] at every
    /// [`map_window`](MmioMapper::map_window) call; the host borrows
    /// it for the host's lifetime and lends it unchanged to every
    /// driver load (the mapper's own window bitmap is the per-load
    /// state, not the host's).
    pub mmio_mapper: Option<&'h dyn MmioMapper>,
}

/// One row in the host's loaded-driver table.
///
/// Holds the *original* image bytes so [`Host::reload`] can re-verify
/// without re-fetching, and so the wipe-on-drop pass covers the
/// signature and capability-body bytes regardless of which buffer the
/// source returned. The [`Drop`] impl runs `secure_clear` on the
/// whole vector before its allocator frees the storage.
struct LoadedRecord {
    handle: DriverHandle,
    path: String,
    kind: DriverKind,
    granted: CapabilitySet,
    /// Sensitive buffer; cleared on drop.
    image: Vec<u8>,
}

impl Drop for LoadedRecord {
    fn drop(&mut self) {
        // Wipe the signature + capability-body bytes the record held.
        // `secure_clear` overwrites the live allocation in place; the
        // subsequent `Vec` drop frees the (already-zeroed) storage.
        secure_clear(self.image.as_mut_slice());
    }
}

/// Userland driver host (Stage 4).
pub struct Host<'h> {
    cfg: HostConfig<'h>,
    next_handle: u64,
    loaded: Vec<LoadedRecord>,
}

impl<'h> Host<'h> {
    /// Construct a fresh host with no loaded drivers.
    #[must_use]
    pub fn new(cfg: HostConfig<'h>) -> Self {
        Self {
            cfg,
            next_handle: 1,
            loaded: Vec::new(),
        }
    }

    /// Number of currently-loaded drivers.
    #[must_use]
    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    /// Snapshot every loaded driver in load-order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<LoadedSnapshot> {
        self.loaded
            .iter()
            .map(|r| LoadedSnapshot {
                handle: r.handle,
                path: r.path.clone(),
                kind: r.kind,
                granted: r.granted,
            })
            .collect()
    }

    /// Read, verify, and bind a `.rxe` image at `path`, granting it the
    /// (necessarily narrower) intersection of its requested set with
    /// `caller_caps`.
    ///
    /// # Errors
    ///
    /// Returns the first failure surfaced by the verification pipeline
    /// (see crate-level rustdoc).
    pub fn load(
        &mut self,
        path: &str,
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, HostError> {
        // Hard gate: every load needs CAP_DRV_LOAD on the caller.
        if !caller_caps.contains(CapabilityId::DRV_LOAD) {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_DRV_LOAD,
                path,
                "caller lacks CAP_DRV_LOAD",
            );
            return Err(HostError::LoadCapabilityMissing);
        }
        let mut image: Vec<u8> = Vec::new();
        self.cfg.source.read(path, &mut image).map_err(|e| {
            self.audit_reject(events::DRIVER_LOAD_REJECTED_MANIFEST, path, "source read");
            HostError::SourceFailed(e)
        })?;
        let handle = self.verify_and_bind(path, &image, caller_caps)?;
        // `verify_and_bind` copied the image bytes into the
        // `LoadedRecord`; wipe the staging buffer before it is freed.
        secure_clear(image.as_mut_slice());
        Ok(handle)
    }

    /// Drop the driver previously issued `handle`.
    ///
    /// # Errors
    ///
    /// [`HostError::HandleNotFound`] if no driver carries `handle`.
    pub fn unload(&mut self, handle: DriverHandle) -> Result<(), HostError> {
        let idx = self
            .loaded
            .iter()
            .position(|r| r.handle == handle)
            .ok_or(HostError::HandleNotFound)?;
        // Remove the record first so its `Drop` runs (which secure-
        // clears the image). Cloning the path before the remove gives
        // the audit emitter a string to reference.
        let path_for_audit = self.loaded[idx].path.clone();
        let _record = self.loaded.remove(idx);
        // `_record`'s Drop wipes its sensitive buffer.
        self.audit_handle(events::DRIVER_UNLOADED, &path_for_audit, handle);
        Ok(())
    }

    /// Re-read, re-verify, and re-bind the driver at the handle's
    /// recorded path. Returns the new handle.
    ///
    /// The previous handle is invalidated even on the success path:
    /// a reload is logically `unload` followed by `load`. Callers must
    /// update any state keyed on the old handle.
    ///
    /// # Errors
    ///
    /// Surfaces any error from the underlying `load` pipeline. The old
    /// record is *not* removed if the reload fails (fail closed: a transient signature mismatch must not deprive
    /// the system of a working driver).
    pub fn reload(
        &mut self,
        handle: DriverHandle,
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, HostError> {
        let idx = self
            .loaded
            .iter()
            .position(|r| r.handle == handle)
            .ok_or(HostError::HandleNotFound)?;
        let path = self.loaded[idx].path.clone();
        // Re-read and verify a *new* record. If that succeeds, drop
        // the old one (Drop secure-clears its image).
        let new_handle = self.load(&path, caller_caps)?;
        // load() appended the new record to the end of `self.loaded`;
        // find the *old* record by its original handle (the index may
        // have shifted only when removing).
        let old_idx = self
            .loaded
            .iter()
            .position(|r| r.handle == handle)
            .ok_or(HostError::HandleNotFound)?;
        let _old = self.loaded.remove(old_idx);
        self.audit_handle(events::DRIVER_RELOADED, &path, new_handle);
        Ok(new_handle)
    }

    // ---- verification pipeline --------------------------------------

    fn verify_and_bind(
        &mut self,
        path: &str,
        image: &[u8],
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, HostError> {
        let parsed = self.parse_or_audit(path, image)?;
        self.check_syscall_hash(path, &parsed)?;
        self.verify_signature(path, &parsed)?;
        let requested = self.decode_and_check_caps(path, &parsed, caller_caps)?;
        self.check_bind_table(path, &parsed)?;
        // 8. Hand the verified image to the spawner for registration.
        // Construct the host view *before* the hand-off so the driver
        // sees the bitmap that is about to be installed.
        // The virtio host (if the deployment ships one) lives for
        // exactly the duration of the registration: drvhost owns the
        // box, the view borrows a `&dyn VirtioHost` from it, and the
        // box is dropped on fall-through (free path) or on the early
        // returns below.
        let virtio_host_owned = self
            .cfg
            .virtio_host_factory
            .and_then(|f| f.mint(&requested));
        let view = LoadedHostView {
            granted: requested,
            kind: parsed.manifest.kind,
            virtio_host: virtio_host_owned.as_deref(),
            mmio_mapper: self.cfg.mmio_mapper,
        };
        let spawn_ctx = SpawnContext {
            manifest: &parsed.manifest,
            payload: parsed.payload,
            host: &view,
            granted: requested,
        };
        match self.cfg.spawner.spawn_and_register(&spawn_ctx) {
            Ok(_reported) => {
                // The driver's reported handle is informational; the
                // host's own freshly-minted handle is the unforgeable
                // proof. We take the host-side handle to avoid trusting
                // a driver that might issue a colliding value.
            }
            Err(SpawnRegisterError::NoDriver) => {
                self.audit_reject(events::DRIVER_LOAD_REJECTED_SPAWN, path, "unknown driver");
                return Err(HostError::UnknownDriver);
            }
            Err(SpawnRegisterError::Register(e)) => {
                self.audit_reject(
                    events::DRIVER_LOAD_REJECTED_REGISTER,
                    path,
                    "driver register",
                );
                // The view borrows from `virtio_host_owned`; the
                // borrow ends at the function return below, after
                // which the boxed virtio host (if any) is dropped
                // and any per-driver `DmaPool` slots are reclaimed.
                return Err(HostError::DriverRegisterFailed(e));
            }
        }
        // 9. Issue the handle only after a successful registration, so a
        // refused load never consumes an identifier.
        let handle = self.next_handle();
        // Falling through, the view and the boxed virtio host are
        // both dropped at the end of this function: the view borrow
        // ends first (lexical order, view declared after the box),
        // then the box releases any per-driver DMA bookkeeping.
        // Explicit `drop()` calls would be redundant and clippy's
        // `drop_non_drop` lint flags them.
        let _ = view;
        let _ = &virtio_host_owned;
        let mut record_image = Vec::with_capacity(image.len());
        record_image.extend_from_slice(image);
        let record = LoadedRecord {
            handle,
            path: path.to_string(),
            kind: parsed.manifest.kind,
            granted: requested,
            image: record_image,
        };
        self.loaded.push(record);
        self.audit_handle(events::DRIVER_LOADED, path, handle);
        Ok(handle)
    }

    fn parse_or_audit<'a>(
        &self,
        path: &str,
        image: &'a [u8],
    ) -> Result<ParsedImage<'a>, HostError> {
        match ParsedImage::parse(image) {
            Ok(p) => Ok(p),
            Err(e) => {
                self.audit_reject(events::DRIVER_LOAD_REJECTED_MANIFEST, path, "parse");
                Err(e)
            }
        }
    }

    fn check_syscall_hash(&self, path: &str, parsed: &ParsedImage<'_>) -> Result<(), HostError> {
        if parsed.manifest.syscall_table_hash == self.cfg.syscall_table_hash {
            return Ok(());
        }
        self.audit_reject(
            events::DRIVER_LOAD_REJECTED_SYSCALL_HASH,
            path,
            "syscall hash",
        );
        Err(HostError::SyscallHashMismatch)
    }

    fn verify_signature(&self, path: &str, parsed: &ParsedImage<'_>) -> Result<(), HostError> {
        let Ok(signer_key) = Ed25519PublicKey::from_bytes(&parsed.manifest.signer_pubkey) else {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_TRUST,
                path,
                "signer key decode",
            );
            return Err(HostError::UntrustedSigner);
        };
        if !self
            .cfg
            .trusted_signers
            .iter()
            .any(|k| k.as_bytes() == signer_key.as_bytes())
        {
            self.audit_reject(events::DRIVER_LOAD_REJECTED_TRUST, path, "untrusted signer");
            return Err(HostError::UntrustedSigner);
        }
        // Compose `header[..signed_end] || cap_body || bind_table ||
        // payload` in a temporary buffer that is wiped before it leaves
        // scope (zero-on-free for any buffer that held
        // capability tokens).
        //
        // The payload is part of the signed message: for a `kind =
        // UserSpace` driver the payload *is* the program the load gate
        // hands the spawner to run as a fresh process, so leaving it
        // unsigned would let an attacker who can rewrite the on-disk image
        // (but not forge the signature) substitute arbitrary code while
        // passing the gate — an unsigned-code-execution hole. Covering it closes that hole; for an in-kernel
        // driver the payload is empty, so the coverage is a no-op.
        let mut signed_message: Vec<u8> = Vec::with_capacity(
            parsed.signed_bytes.len()
                + parsed.capability_body.len()
                + parsed.bind_table.len()
                + parsed.payload.len(),
        );
        signed_message.extend_from_slice(parsed.signed_bytes);
        signed_message.extend_from_slice(parsed.capability_body);
        signed_message.extend_from_slice(parsed.bind_table);
        signed_message.extend_from_slice(parsed.payload);
        let sig = Ed25519Signature::from_bytes(parsed.manifest.signature);
        let result = signer_key.verify(&signed_message, &sig);
        secure_clear(signed_message.as_mut_slice());
        if result.is_err() {
            self.audit_reject(events::DRIVER_LOAD_REJECTED_SIGNATURE, path, "ed25519");
            return Err(HostError::SignatureInvalid);
        }
        Ok(())
    }

    fn decode_and_check_caps(
        &self,
        path: &str,
        parsed: &ParsedImage<'_>,
        caller_caps: &CapabilitySet,
    ) -> Result<CapabilitySet, HostError> {
        let mut buf = [CapabilityId::DRV_LOAD; DRIVER_MANIFEST_MAX_CAPABILITIES as usize];
        let n = parsed.decode_capabilities(&mut buf).map_err(|e| {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_CAPABILITY,
                path,
                "capability decode",
            );
            e
        })?;
        let mut requested = CapabilitySet::empty();
        for cap in &buf[..n] {
            requested.insert(*cap);
        }
        if parsed.manifest.kind == DriverKind::InKernel
            && !caller_caps.contains(CapabilityId::DRV_KERNEL)
        {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_KERNEL_KIND,
                path,
                "InKernel without CAP_DRV_KERNEL",
            );
            return Err(HostError::KernelKindForbidden);
        }
        if !requested.is_subset_of(caller_caps) {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_CAPABILITY,
                path,
                "capability escalation",
            );
            return Err(HostError::CapabilityEscalation);
        }
        Ok(requested)
    }

    fn check_bind_table(&self, path: &str, parsed: &ParsedImage<'_>) -> Result<(), HostError> {
        // The decoded entries are not consumed here — matching is the
        // device manager's job — but every entry is
        // validated fail-closed at the load gate so a malformed table
        // never reaches a consumer.
        let mut buf =
            [DriverBindKey::new(0, HwMatchKey::virtio(0)); DRIVER_MANIFEST_MAX_BIND_KEYS as usize];
        parsed.decode_bind_table(&mut buf).map_err(|e| {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_BIND_KEY,
                path,
                "bind key decode",
            );
            e
        })?;
        Ok(())
    }

    fn next_handle(&mut self) -> DriverHandle {
        // `next_handle` is initialised to 1 and only ever increments by
        // 1; the field is `u64` and the host's lifetime is bounded by a
        // process restart long before 2^64 loads. The two corner cases
        // are nevertheless handled to keep the function total: a
        // hypothetical wrap to 0 (the `DriverHandle::NONE` sentinel)
        // skips to 1.
        if self.next_handle == 0 {
            self.next_handle = 1;
        }
        let raw = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        match DriverHandle::from_raw(raw) {
            Ok(h) => h,
            Err(_) => DriverHandle::NONE,
        }
    }

    fn audit_handle(&self, id: EventId, path: &str, handle: DriverHandle) {
        let mut hbuf = HandleBuf::new();
        let handle_str = hbuf.format(handle.as_u64());
        let fields = [
            Field {
                key: "path",
                value: rustos_log::FieldValue::Str(path),
            },
            Field {
                key: "handle",
                value: rustos_log::FieldValue::Str(handle_str),
            },
        ];
        log_event(
            self.cfg.sink,
            &Event {
                level: Level::Info,
                id,
                message: event_message(id),
                fields: &fields,
            },
        );
    }

    fn audit_reject(&self, id: EventId, path: &str, reason: &str) {
        let fields = [
            Field {
                key: "path",
                value: rustos_log::FieldValue::Str(path),
            },
            Field {
                key: "reason",
                value: rustos_log::FieldValue::Str(reason),
            },
        ];
        log_event(
            self.cfg.sink,
            &Event {
                level: Level::Warn,
                id,
                message: event_message(id),
                fields: &fields,
            },
        );
    }
}

/// Driver-visible view of a host. Only what the driver may legitimately
/// observe — the granted capability bitmap, the kind, and the
/// per-driver virtio host if one was minted — is exposed.
struct LoadedHostView<'v> {
    granted: CapabilitySet,
    kind: DriverKind,
    /// Borrowed [`VirtioHost`] minted by
    /// [`HostConfig::virtio_host_factory`] for this driver load.
    /// `None` whenever the host config has no factory or the factory
    /// declined to expose one to this driver (for example because
    /// `CAP_MEM_DMA` was not granted).
    virtio_host: Option<&'v dyn VirtioHost>,
    /// Borrowed MMIO mapper from [`HostConfig::mmio_mapper`], or
    /// `None` when the host config ships no MMIO-map facility.
    mmio_mapper: Option<&'v dyn MmioMapper>,
}

impl DriverHost for LoadedHostView<'_> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.granted.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn virtio_host(&self) -> Option<&dyn VirtioHost> {
        self.virtio_host
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        self.mmio_mapper
    }
}

fn event_message(id: EventId) -> &'static str {
    match id {
        x if x == events::DRIVER_LOADED => "driver loaded",
        x if x == events::DRIVER_UNLOADED => "driver unloaded",
        x if x == events::DRIVER_RELOADED => "driver reloaded",
        x if x == events::DRIVER_LOAD_REJECTED_MANIFEST => "driver load rejected: manifest",
        x if x == events::DRIVER_LOAD_REJECTED_SYSCALL_HASH => "driver load rejected: syscall hash",
        x if x == events::DRIVER_LOAD_REJECTED_TRUST => "driver load rejected: untrusted signer",
        x if x == events::DRIVER_LOAD_REJECTED_SIGNATURE => "driver load rejected: signature",
        x if x == events::DRIVER_LOAD_REJECTED_CAPABILITY => {
            "driver load rejected: capability escalation"
        }
        x if x == events::DRIVER_LOAD_REJECTED_KERNEL_KIND => {
            "driver load rejected: in-kernel without CAP_DRV_KERNEL"
        }
        x if x == events::DRIVER_LOAD_REJECTED_DRV_LOAD => {
            "driver load rejected: missing CAP_DRV_LOAD"
        }
        x if x == events::DRIVER_LOAD_REJECTED_SPAWN => "driver load rejected: unknown driver",
        x if x == events::DRIVER_LOAD_REJECTED_REGISTER => "driver load rejected: register()",
        _ => "drvhost event",
    }
}

/// Small stack buffer for rendering a `u64` (a [`DriverHandle`] or a
/// count) as decimal without an allocator. A u64 fits in 20 decimal
/// digits. Shared with [`crate::store`] so the crate has one decimal
/// formatter.
pub(crate) struct HandleBuf {
    bytes: [u8; 20],
    len: usize,
}

impl HandleBuf {
    pub(crate) fn new() -> Self {
        Self {
            bytes: [0u8; 20],
            len: 0,
        }
    }

    pub(crate) fn format(&mut self, value: u64) -> &str {
        // Render via `core::fmt::Write` into our fixed buffer. A 20-byte
        // buffer is more than enough for any u64 (max is 19 digits +
        // sign-less). On the unreachable overflow path we render "0".
        let mut writer = HandleWriter { buf: self };
        if write!(&mut writer, "{value}").is_err() {
            // Reset and fall back to "0".
            self.bytes[0] = b'0';
            self.len = 1;
        }
        // SAFETY: not used here — every byte we wrote comes from a
        // decimal digit, which is ASCII, and ASCII is valid UTF-8.
        // `core::str::from_utf8` performs the check explicitly so no
        // unsafe is needed at this site.
        match core::str::from_utf8(&self.bytes[..self.len]) {
            Ok(s) => s,
            Err(_) => "0",
        }
    }
}

struct HandleWriter<'a> {
    buf: &'a mut HandleBuf,
}

impl core::fmt::Write for HandleWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let remaining = self.buf.bytes.len() - self.buf.len;
        if s.len() > remaining {
            return Err(core::fmt::Error);
        }
        let start = self.buf.len;
        self.buf.bytes[start..start + s.len()].copy_from_slice(s.as_bytes());
        self.buf.len += s.len();
        Ok(())
    }
}
