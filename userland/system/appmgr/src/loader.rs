//! The [`AppLoader`] pipeline: validate a bundle's layout, verify its signed
//! manifest and contents, compute its capability ceiling, and resolve its
//! shared-library references under the §16.4 policy.
//!
//! This is the one place a bundle is judged. The pipeline **fails closed**
//! (`AGENTS.md` §5.4): the first deviation — a stray top-level entry, a
//! manifest that will not decode, a wrong interface hash, a bad signature, a
//! content-hash mismatch — refuses the whole bundle and nothing is launched.

use alloc::string::String;

use rustos_abi::{
    decode_capability_ids, resolve_library as resolve_library_policy, validate_bundle_layout,
    AppInfoHeader, CapabilityId, Errno, LibraryScope, APPINFO_MAX_CAPABILITIES,
    SYSCALL_TABLE_HASH_LEN,
};
use rustos_caps::CapabilitySet;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::bundle::{BundleStore, LoadedApp, Verifier};
use crate::error::AppError;
use crate::events;

/// Construction-time configuration for an [`AppLoader`].
///
/// All seams are borrowed for the loader's lifetime, mirroring `init`'s
/// host configuration: one loader per service process, alive for the run.
pub struct AppLoaderConfig<'a> {
    /// ABI version the loader accepts in an `AppInfo` manifest. A manifest
    /// targeting a different version is refused.
    pub accepted_abi_version: u32,
    /// The kernel's compiled-in syscall-table hash. A manifest whose
    /// declared hash does not match is refused (`AGENTS.md` §9 / §19.2).
    pub syscall_table_hash: [u8; SYSCALL_TABLE_HASH_LEN],
    /// Seam that reads a bundle off storage.
    pub store: &'a dyn BundleStore,
    /// Seam that verifies a manifest signature.
    pub verifier: &'a dyn Verifier,
    /// Structured audit log sink (`AGENTS.md` §19.4).
    pub sink: &'a dyn Sink,
}

/// The application-bundle loader (Stage 6 — `AGENTS.md` §16.4, §16.5).
pub struct AppLoader<'a> {
    cfg: AppLoaderConfig<'a>,
}

impl<'a> AppLoader<'a> {
    /// Create a loader over the given configuration.
    #[must_use]
    pub fn new(cfg: AppLoaderConfig<'a>) -> Self {
        Self { cfg }
    }

    /// Load and authorise the bundle rooted at `bundle`, returning the
    /// capability ceiling and entry point a caller may spawn it with.
    ///
    /// `user_grants` is the launching user's capability set; the granted
    /// ceiling is the manifest request intersected with it (`AGENTS.md`
    /// §5.2). The loader never widens a request.
    ///
    /// # Errors
    ///
    /// Returns the first [`AppError`] encountered: [`AppError::Store`] for an
    /// unreadable bundle, [`AppError::Layout`] for a layout deviation,
    /// [`AppError::Manifest`] for a bad manifest, [`AppError::InterfaceHashMismatch`]
    /// for a syscall-hash mismatch, [`AppError::Signature`] for a failed
    /// signature, or [`AppError::ContentHashMismatch`] for tampered contents.
    pub fn load(&self, bundle: &str, user_grants: &CapabilitySet) -> Result<LoadedApp, AppError> {
        let names = self
            .cfg
            .store
            .entries(bundle)
            .map_err(|e| self.store_error(bundle, e))?;
        let refs: alloc::vec::Vec<&str> = names.iter().map(String::as_str).collect();
        if let Err(e) = validate_bundle_layout(&refs) {
            self.audit(
                events::APP_LAYOUT_REJECTED,
                Level::Warn,
                bundle,
                "layout invalid",
            );
            return Err(AppError::Layout(e));
        }

        let bytes = self
            .cfg
            .store
            .read_appinfo(bundle)
            .map_err(|e| self.store_error(bundle, e))?;
        let header = match AppInfoHeader::from_bytes(&bytes) {
            Ok(header) => header,
            Err(e) => {
                self.audit(
                    events::APP_MANIFEST_INVALID,
                    Level::Warn,
                    bundle,
                    "manifest undecodable",
                );
                return Err(AppError::Manifest(e));
            }
        };
        if header.abi_version != self.cfg.accepted_abi_version {
            self.audit(
                events::APP_MANIFEST_INVALID,
                Level::Warn,
                bundle,
                "abi version unsupported",
            );
            return Err(AppError::Manifest(Errno::AbiVersionUnsupported));
        }

        if ct_ne(&header.syscall_table_hash, &self.cfg.syscall_table_hash) {
            self.audit(
                events::APP_INTERFACE_MISMATCH,
                Level::Warn,
                bundle,
                "syscall interface hash mismatch",
            );
            return Err(AppError::InterfaceHashMismatch);
        }

        let signed = &bytes[AppInfoHeader::signed_range()];
        if self
            .cfg
            .verifier
            .verify(signed, &header.signature, &header.signer_pubkey)
            .is_err()
        {
            self.audit(
                events::APP_SIGNATURE_INVALID,
                Level::Warn,
                bundle,
                "signature did not verify",
            );
            return Err(AppError::Signature);
        }

        let actual = self
            .cfg
            .store
            .content_hash(bundle)
            .map_err(|e| self.store_error(bundle, e))?;
        if ct_ne(&actual, &header.content_hash) {
            self.audit(
                events::APP_CONTENT_MISMATCH,
                Level::Warn,
                bundle,
                "content hash mismatch",
            );
            return Err(AppError::ContentHashMismatch);
        }

        let requested = self.requested_capabilities(&bytes, &header, bundle)?;
        let granted = requested.intersection(user_grants);

        let run_path = join(bundle, "Run");
        self.audit(
            events::APP_LOADED,
            Level::Info,
            bundle,
            header.bundle_name(),
        );
        Ok(LoadedApp::new(
            header.bundle_id().into(),
            header.bundle_name().into(),
            header.bundle_version().into(),
            run_path,
            granted,
        ))
    }

    /// Resolve a shared-library `reference` for the bundle rooted at
    /// `bundle` under the §16.4 dynamic-loader policy.
    ///
    /// A reference is accepted only if it lies inside the bundle's own
    /// `Libraries/` directory or `/System/Libraries/`.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Library`] if the reference points anywhere else or
    /// contains a `..` component.
    pub fn resolve_library(&self, bundle: &str, reference: &str) -> Result<LibraryScope, AppError> {
        let dir = join(bundle, "Libraries");
        match resolve_library_policy(reference, &dir) {
            Ok(scope) => {
                self.audit(events::LIBRARY_RESOLVED, Level::Info, bundle, reference);
                Ok(scope)
            }
            Err(e) => {
                self.audit(events::LIBRARY_REFUSED, Level::Warn, bundle, reference);
                Err(AppError::Library(e))
            }
        }
    }

    /// Decode the manifest's capability-request body into a [`CapabilitySet`].
    fn requested_capabilities(
        &self,
        bytes: &[u8],
        header: &AppInfoHeader,
        bundle: &str,
    ) -> Result<CapabilitySet, AppError> {
        let count = usize::from(header.capability_count);
        let body = bytes.get(AppInfoHeader::WIRE_LEN..).ok_or_else(|| {
            self.audit(
                events::APP_MANIFEST_INVALID,
                Level::Warn,
                bundle,
                "manifest body truncated",
            );
            AppError::Manifest(Errno::BufferTooSmall)
        })?;
        let expected = header.body_len().map_err(|e| {
            self.audit(
                events::APP_MANIFEST_INVALID,
                Level::Warn,
                bundle,
                "manifest body length overflow",
            );
            AppError::Manifest(e)
        })?;
        if body.len() < expected {
            self.audit(
                events::APP_MANIFEST_INVALID,
                Level::Warn,
                bundle,
                "manifest body truncated",
            );
            return Err(AppError::Manifest(Errno::BufferTooSmall));
        }
        let mut scratch = [CapabilityId::FS_MOUNT; APPINFO_MAX_CAPABILITIES as usize];
        let decoded = decode_capability_ids(body, count, &mut scratch).map_err(|e| {
            self.audit(
                events::APP_MANIFEST_INVALID,
                Level::Warn,
                bundle,
                "capability body invalid",
            );
            AppError::Manifest(e)
        })?;
        let mut set = CapabilitySet::empty();
        for cap in &scratch[..decoded] {
            set.insert(*cap);
        }
        Ok(set)
    }

    fn store_error(&self, bundle: &str, err: Errno) -> AppError {
        self.audit(events::APP_STORE_ERROR, Level::Warn, bundle, "store error");
        AppError::Store(err)
    }

    fn audit(&self, id: EventId, level: Level, bundle: &str, detail: &str) {
        log(
            self.cfg.sink,
            &Event {
                level,
                id,
                message: event_message(id),
                fields: &[
                    Field {
                        key: "bundle",
                        value: bundle,
                    },
                    Field {
                        key: "detail",
                        value: detail,
                    },
                ],
            },
        );
    }
}

/// Constant-time inequality over two 32-byte digests, so a hostile bundle
/// cannot probe the kernel's hashes byte-by-byte via timing.
fn ct_ne(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        != 0
}

/// Join a bundle root and a child entry into an absolute path, tolerating a
/// trailing `/` on the root.
fn join(bundle: &str, child: &str) -> String {
    let base = bundle.strip_suffix('/').unwrap_or(bundle);
    let mut out = String::with_capacity(base.len() + 1 + child.len());
    out.push_str(base);
    out.push('/');
    out.push_str(child);
    out
}

fn event_message(id: EventId) -> &'static str {
    match id {
        events::APP_LOADED => "application bundle loaded",
        events::APP_LAYOUT_REJECTED => "application bundle layout rejected",
        events::APP_MANIFEST_INVALID => "application manifest invalid",
        events::APP_INTERFACE_MISMATCH => "application syscall interface hash mismatch",
        events::APP_SIGNATURE_INVALID => "application signature invalid",
        events::APP_CONTENT_MISMATCH => "application content hash mismatch",
        events::APP_STORE_ERROR => "application bundle store error",
        events::LIBRARY_RESOLVED => "shared library resolved",
        events::LIBRARY_REFUSED => "shared library refused",
        _ => "appmgr event",
    }
}

#[cfg(test)]
mod tests {
    use super::{event_message, AppLoader, AppLoaderConfig};
    use crate::bundle::{BundleStore, Verifier};
    use crate::error::AppError;
    use crate::events;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::{
        AppInfoHeader, BundleLayoutError, CapabilityId, Errno, LibraryError, LibraryScope,
        ABI_VERSION_CURRENT, APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX,
        MIME_TYPE_MAX, SYSCALL_TABLE_HASH_LEN,
    };
    use rustos_caps::CapabilitySet;
    use rustos_log::{Event, EventId, Sink};

    const KERNEL_HASH: [u8; SYSCALL_TABLE_HASH_LEN] = [0x11; SYSCALL_TABLE_HASH_LEN];
    const CONTENT_HASH: [u8; 32] = [0x22; 32];

    fn inline_buf<const N: usize>(text: &str) -> [u8; N] {
        let mut buf = [0u8; N];
        buf[..text.len()].copy_from_slice(text.as_bytes());
        buf
    }

    /// Build an `AppInfo` manifest (header + capability list + one MIME
    /// entry) requesting `caps`, declaring `syscall_hash` and `content_hash`.
    fn build_manifest(
        caps: &[CapabilityId],
        syscall_hash: [u8; SYSCALL_TABLE_HASH_LEN],
        content_hash: [u8; 32],
    ) -> Vec<u8> {
        let header = AppInfoHeader {
            magic: APPINFO_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: u16::try_from(caps.len()).unwrap(),
            mime_count: 1,
            id_len: u8::try_from("com.example.app".len()).unwrap(),
            name_len: u8::try_from("Example".len()).unwrap(),
            version_len: u8::try_from("1.0".len()).unwrap(),
            reserved0: 0,
            id: inline_buf::<BUNDLE_ID_MAX>("com.example.app"),
            name: inline_buf::<BUNDLE_NAME_MAX>("Example"),
            version: inline_buf::<BUNDLE_VERSION_MAX>("1.0"),
            syscall_table_hash: syscall_hash,
            content_hash,
            signer_pubkey: [0xEF; 32],
            signature: [0x12; 64],
        };
        let mut out = header.to_le_bytes().to_vec();
        for cap in caps {
            out.extend_from_slice(&cap.as_u16().to_le_bytes());
        }
        let mime = b"text/plain";
        out.push(u8::try_from(mime.len()).unwrap());
        let mut buf = [0u8; MIME_TYPE_MAX];
        buf[..mime.len()].copy_from_slice(mime);
        out.extend_from_slice(&buf);
        out
    }

    fn full_layout() -> Vec<String> {
        [
            "AppInfo",
            "Run",
            "Code",
            "Libraries",
            "Resources",
            "DefaultSettings",
            "Documentation",
        ]
        .iter()
        .map(ToString::to_string)
        .collect()
    }

    fn cap_set(list: &[CapabilityId]) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for cap in list {
            set.insert(*cap);
        }
        set
    }

    struct MockStore {
        entries: Vec<String>,
        appinfo: Vec<u8>,
        content: [u8; 32],
        entries_fail: bool,
    }
    impl MockStore {
        fn good(caps: &[CapabilityId]) -> Self {
            Self {
                entries: full_layout(),
                appinfo: build_manifest(caps, KERNEL_HASH, CONTENT_HASH),
                content: CONTENT_HASH,
                entries_fail: false,
            }
        }
    }
    impl BundleStore for MockStore {
        fn entries(&self, _bundle: &str) -> Result<Vec<String>, Errno> {
            if self.entries_fail {
                return Err(Errno::NotFound);
            }
            Ok(self.entries.clone())
        }
        fn read_appinfo(&self, _bundle: &str) -> Result<Vec<u8>, Errno> {
            Ok(self.appinfo.clone())
        }
        fn content_hash(&self, _bundle: &str) -> Result<[u8; 32], Errno> {
            Ok(self.content)
        }
    }

    struct AcceptVerifier;
    impl Verifier for AcceptVerifier {
        fn verify(&self, _: &[u8], _: &[u8; 64], _: &[u8; 32]) -> Result<(), Errno> {
            Ok(())
        }
    }
    struct RejectVerifier;
    impl Verifier for RejectVerifier {
        fn verify(&self, _: &[u8], _: &[u8; 64], _: &[u8; 32]) -> Result<(), Errno> {
            Err(Errno::SignatureInvalid)
        }
    }

    struct RecordingSink {
        events: RefCell<Vec<EventId>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }
        fn count(&self, id: EventId) -> usize {
            self.events.borrow().iter().filter(|e| **e == id).count()
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(event.id);
        }
    }

    fn cfg<'a>(
        store: &'a dyn BundleStore,
        verifier: &'a dyn Verifier,
        sink: &'a RecordingSink,
    ) -> AppLoaderConfig<'a> {
        AppLoaderConfig {
            accepted_abi_version: ABI_VERSION_CURRENT,
            syscall_table_hash: KERNEL_HASH,
            store,
            verifier,
            sink,
        }
    }

    #[test]
    fn loads_valid_bundle_and_intersects_capabilities() {
        let store = MockStore::good(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));

        // The user holds only NET_RAW, so FS_MOUNT is dropped on intersect.
        let user = cap_set(&[CapabilityId::NET_RAW]);
        let app = loader.load("/Apps/Example.app", &user).expect("loads");

        assert_eq!(app.id(), "com.example.app");
        assert_eq!(app.name(), "Example");
        assert_eq!(app.version(), "1.0");
        assert_eq!(app.run_path(), "/Apps/Example.app/Run");
        assert!(app.granted().contains(CapabilityId::NET_RAW));
        assert!(!app.granted().contains(CapabilityId::FS_MOUNT));
        assert_eq!(app.granted().len(), 1);
        assert_eq!(sink.count(events::APP_LOADED), 1);
    }

    #[test]
    fn minimal_layout_loads() {
        let mut store = MockStore::good(&[]);
        store.entries = vec!["AppInfo".to_string(), "Run".to_string()];
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        let app = loader
            .load("/Apps/Example.app", &CapabilitySet::empty())
            .expect("loads");
        assert!(app.granted().is_empty());
    }

    #[test]
    fn rejects_unknown_layout_entry() {
        let mut store = MockStore::good(&[]);
        store.entries.push("Plugins".to_string());
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Layout(BundleLayoutError::UnknownEntry))
        );
        assert_eq!(sink.count(events::APP_LAYOUT_REJECTED), 1);
    }

    #[test]
    fn rejects_missing_run() {
        let mut store = MockStore::good(&[]);
        store.entries = vec!["AppInfo".to_string(), "Resources".to_string()];
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Layout(BundleLayoutError::MissingRun))
        );
    }

    #[test]
    fn rejects_undecodable_manifest() {
        let mut store = MockStore::good(&[]);
        store.appinfo[0] ^= 0xFF; // corrupt magic
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Manifest(Errno::BadMagic))
        );
        assert_eq!(sink.count(events::APP_MANIFEST_INVALID), 1);
    }

    #[test]
    fn rejects_unsupported_abi_version() {
        let store = MockStore::good(&[]);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let mut config = cfg(&store, &verifier, &sink);
        config.accepted_abi_version = ABI_VERSION_CURRENT + 1;
        let loader = AppLoader::new(config);
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Manifest(Errno::AbiVersionUnsupported))
        );
    }

    #[test]
    fn rejects_interface_hash_mismatch() {
        let store = MockStore {
            entries: full_layout(),
            appinfo: build_manifest(&[], [0x99; SYSCALL_TABLE_HASH_LEN], CONTENT_HASH),
            content: CONTENT_HASH,
            entries_fail: false,
        };
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::InterfaceHashMismatch)
        );
        assert_eq!(sink.count(events::APP_INTERFACE_MISMATCH), 1);
    }

    #[test]
    fn rejects_bad_signature() {
        let store = MockStore::good(&[]);
        let verifier = RejectVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Signature)
        );
        assert_eq!(sink.count(events::APP_SIGNATURE_INVALID), 1);
    }

    #[test]
    fn rejects_content_hash_mismatch() {
        let mut store = MockStore::good(&[]);
        store.content = [0x77; 32]; // does not match the manifest's CONTENT_HASH
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::ContentHashMismatch)
        );
        assert_eq!(sink.count(events::APP_CONTENT_MISMATCH), 1);
    }

    #[test]
    fn store_failure_is_reported() {
        let mut store = MockStore::good(&[]);
        store.entries_fail = true;
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Store(Errno::NotFound))
        );
        assert_eq!(sink.count(events::APP_STORE_ERROR), 1);
    }

    #[test]
    fn rejects_truncated_capability_body() {
        let mut store = MockStore::good(&[CapabilityId::FS_MOUNT]);
        // Drop the capability/MIME body, leaving only the header. The
        // declared capability_count (1) can no longer be satisfied.
        store.appinfo.truncate(AppInfoHeader::WIRE_LEN);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Manifest(Errno::BufferTooSmall))
        );
    }

    #[test]
    fn resolves_libraries_within_policy() {
        let store = MockStore::good(&[]);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));

        assert_eq!(
            loader.resolve_library("/Apps/Example.app", "/Apps/Example.app/Libraries/libui.so"),
            Ok(LibraryScope::Bundle)
        );
        assert_eq!(
            loader.resolve_library("/Apps/Example.app", "/System/Libraries/libtls.so"),
            Ok(LibraryScope::System)
        );
        assert_eq!(sink.count(events::LIBRARY_RESOLVED), 2);
    }

    #[test]
    fn refuses_libraries_outside_policy() {
        let store = MockStore::good(&[]);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));

        assert_eq!(
            loader.resolve_library("/Apps/Example.app", "/System/Kernel/secret"),
            Err(AppError::Library(LibraryError::OutsidePolicy))
        );
        assert_eq!(
            loader.resolve_library("/Apps/Example.app", "/System/Libraries/../Kernel/x"),
            Err(AppError::Library(LibraryError::Traversal))
        );
        assert_eq!(sink.count(events::LIBRARY_REFUSED), 2);
    }

    #[test]
    fn event_messages_are_total() {
        assert_eq!(
            event_message(events::APP_LOADED),
            "application bundle loaded"
        );
        assert_eq!(event_message(EventId(1)), "appmgr event");
    }
}
