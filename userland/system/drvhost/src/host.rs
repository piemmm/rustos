//! Driver host state machine — `load`, `unload`, `reload`.
//!
//! Owns the verified-and-bound set of currently loaded driver modules.
//! Every state transition is audited through [`rustos_log`] (see
//! [`crate::events`]) and the per-record sensitive buffers are wiped
//! through [`crate::zeroize::secure_clear`] on drop.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_abi::{
    CapabilityId, DriverHandle, DriverHost, DriverKind, DRIVER_MANIFEST_MAX_CAPABILITIES,
};
use rustos_caps::CapabilitySet;
use rustos_crypto::{Ed25519PublicKey, Ed25519Signature};
use rustos_log::{log as log_event, Event, EventId, Field, Level, Sink};

use crate::events;
use crate::image::ParsedImage;
use crate::resolver::EntryResolver;
use crate::source::ImageSource;
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
    /// Resolver that turns a verified manifest+payload into a driver
    /// `register` entry point.
    pub resolver: &'h dyn EntryResolver,
    /// Sink that receives every structured-log [`Event`] the host
    /// emits.
    pub sink: &'h dyn Sink,
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

/// Userland driver host (Stage 4 — `AGENTS.md` §8).
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
    /// record is *not* removed if the reload fails (`AGENTS.md` §5.4 —
    /// fail closed: a transient signature mismatch must not deprive
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
        let Some(entry) = self.cfg.resolver.resolve(&parsed.manifest, parsed.payload) else {
            self.audit_reject(
                events::DRIVER_LOAD_REJECTED_RESOLVER,
                path,
                "unknown driver",
            );
            return Err(HostError::UnknownDriver);
        };
        // 9. Issue handle, register, audit.
        let handle = self.next_handle();
        // Construct the host view *before* calling register() so the
        // driver sees the bitmap that is about to be installed.
        let view = LoadedHostView {
            granted: requested,
            kind: parsed.manifest.kind,
        };
        match entry(&view) {
            Ok(_returned) => {
                // The driver's returned handle is informational; the
                // host's own freshly-minted handle is the unforgeable
                // proof. We take the host-side handle to avoid trusting
                // a driver that might issue a colliding value.
            }
            Err(e) => {
                self.audit_reject(
                    events::DRIVER_LOAD_REJECTED_REGISTER,
                    path,
                    "driver register",
                );
                return Err(HostError::DriverRegisterFailed(e));
            }
        }
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
        // Compose `header[..signed_end] || cap_body` in a temporary
        // buffer that is wiped before it leaves scope (`AGENTS.md` §4 —
        // zero-on-free for any buffer that held capability tokens).
        let mut signed_message: Vec<u8> =
            Vec::with_capacity(parsed.signed_bytes.len() + parsed.capability_body.len());
        signed_message.extend_from_slice(parsed.signed_bytes);
        signed_message.extend_from_slice(parsed.capability_body);
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
                value: path,
            },
            Field {
                key: "handle",
                value: handle_str,
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
                value: path,
            },
            Field {
                key: "reason",
                value: reason,
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
/// observe — the granted capability bitmap and the kind — is exposed.
struct LoadedHostView {
    granted: CapabilitySet,
    kind: DriverKind,
}

impl DriverHost for LoadedHostView {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.granted.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        self.kind
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
        x if x == events::DRIVER_LOAD_REJECTED_RESOLVER => "driver load rejected: unknown driver",
        x if x == events::DRIVER_LOAD_REJECTED_REGISTER => "driver load rejected: register()",
        _ => "drvhost event",
    }
}

/// Small stack buffer for rendering a [`DriverHandle`] without an
/// allocator. A u64 fits in 20 decimal digits.
struct HandleBuf {
    bytes: [u8; 20],
    len: usize,
}

impl HandleBuf {
    fn new() -> Self {
        Self {
            bytes: [0u8; 20],
            len: 0,
        }
    }

    fn format(&mut self, value: u64) -> &str {
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
