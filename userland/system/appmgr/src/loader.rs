//! The [`AppLoader`] pipeline: validate a bundle's layout, verify its signed
//! manifest and contents, compute its capability ceiling, and resolve its
//! shared-library references under the policy.
//!
//! This is the one place a bundle is judged. The pipeline **fails closed**: the first deviation — a stray top-level entry, a
//! manifest that will not decode, a wrong interface hash, a bad signature, a
//! content-hash mismatch — refuses the whole bundle and nothing is launched.

use alloc::string::String;

use rustos_abi::{
    decode_capability_ids, resolve_library as resolve_library_policy, validate_bundle_layout,
    AppInfoHeader, CapabilityId, Errno, LibraryScope, LoadImage, APPINFO_MAX_CAPABILITIES,
    SYSCALL_TABLE_HASH_LEN,
};
use rustos_caps::CapabilitySet;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::bundle::{BundleStore, LoadedApp, ResolvedLibrary, Verifier};
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
    /// declared hash does not match is refused.
    pub syscall_table_hash: [u8; SYSCALL_TABLE_HASH_LEN],
    /// Seam that reads a bundle off storage.
    pub store: &'a dyn BundleStore,
    /// Seam that verifies a manifest signature.
    pub verifier: &'a dyn Verifier,
    /// Structured audit log sink.
    pub sink: &'a dyn Sink,
}

/// The application-bundle loader (Stage 6).
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
    /// ceiling is the manifest request intersected with it. The loader never widens a request.
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

        self.verify_manifest_signature(bundle, &bytes, &header)?;

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

        let libraries = self.validate_run_image(bundle)?;

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
            libraries,
        ))
    }

    /// Resolve a shared-library `reference` for the bundle rooted at
    /// `bundle` under the dynamic-loader policy.
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

    /// Read and validate the entry-point `Run` binary and resolve the shared
    /// libraries it declares it needs.
    ///
    /// `LoadImage::parse` enforces the hardening invariants (PIE, W^X,
    /// and the syscall-hash CFI tag) on the binary; a malformed image or a
    /// CFI-tag mismatch is refused (`AppError::RunImage`).
    ///
    /// # Errors
    ///
    /// [`AppError::Store`] if the binary cannot be read, [`AppError::RunImage`]
    /// if it is not a valid `rxe` image, or [`AppError::Library`] if a needed
    /// library violates the policy.
    fn validate_run_image(
        &self,
        bundle: &str,
    ) -> Result<alloc::vec::Vec<ResolvedLibrary>, AppError> {
        let run_bytes = self
            .cfg
            .store
            .read_run(bundle)
            .map_err(|e| self.store_error(bundle, e))?;
        let image = LoadImage::parse(&run_bytes, &self.cfg.syscall_table_hash).map_err(|e| {
            self.audit(
                events::APP_RUN_IMAGE_INVALID,
                Level::Warn,
                bundle,
                "run image rejected",
            );
            AppError::RunImage(e)
        })?;
        self.resolve_needed_libraries(bundle, &image)
    }

    /// Resolve every shared library the entry-point `image` declares it needs
    /// against the dynamic-loader policy, in declaration order.
    ///
    /// This is where the C-ABI runtime (the curated `ros_sys_*` /
    /// `/System/Libraries/` *System runtime / C ABI* library) and any bundle
    /// `Libraries/` reference is bound; an out-of-tree reference fails closed.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Library`] on the first reference that points
    /// outside the bundle's `Libraries/` or `/System/Libraries/`.
    fn resolve_needed_libraries(
        &self,
        bundle: &str,
        image: &LoadImage,
    ) -> Result<alloc::vec::Vec<ResolvedLibrary>, AppError> {
        let mut resolved = alloc::vec::Vec::new();
        for reference in image.needed_libraries() {
            let scope = self.resolve_library(bundle, reference)?;
            resolved.push(ResolvedLibrary {
                reference: reference.into(),
                scope,
            });
        }
        Ok(resolved)
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

    /// Verify the manifest's Ed25519 signature over the whole manifest
    /// except the signature field: the header prefix concatenated with the
    /// capability/MIME body, so a swapped capability id in the body breaks
    /// it rather than hiding behind a header-only signature.
    fn verify_manifest_signature(
        &self,
        bundle: &str,
        bytes: &[u8],
        header: &AppInfoHeader,
    ) -> Result<(), AppError> {
        let mut signed = alloc::vec::Vec::with_capacity(
            AppInfoHeader::signed_range().end + (bytes.len() - AppInfoHeader::WIRE_LEN),
        );
        signed.extend_from_slice(&bytes[AppInfoHeader::signed_range()]);
        signed.extend_from_slice(&bytes[AppInfoHeader::WIRE_LEN..]);
        if self
            .cfg
            .verifier
            .verify(&signed, &header.signature, &header.signer_pubkey)
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
        Ok(())
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
                        value: rustos_log::FieldValue::Str(bundle),
                    },
                    Field {
                        key: "detail",
                        value: rustos_log::FieldValue::Str(detail),
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
        events::APP_RUN_IMAGE_INVALID => "application run image invalid",
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
        LoadHeader, NeededLibrary, RxeError, RxePermission, Segment, ABI_VERSION_CURRENT,
        APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_VERSION_MAX, LOAD_FLAG_PIE,
        LOAD_MAGIC, MIME_TYPE_MAX, SYSCALL_TABLE_HASH_LEN,
    };
    use rustos_caps::CapabilitySet;
    use rustos_log::{Event, EventId, Sink};

    const KERNEL_HASH: [u8; SYSCALL_TABLE_HASH_LEN] = [0x11; SYSCALL_TABLE_HASH_LEN];
    const CONTENT_HASH: [u8; 32] = [0x22; 32];
    /// The curated System runtime / C ABI shared library a hosted C program
    /// dynamically links; it lives under
    /// `/System/Libraries/`.
    const RUNTIME_LIB: &str = "/System/Libraries/libros-sys.so";

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

    /// Build a minimal valid `rxe` entry-point image declaring `needed`
    /// shared libraries and carrying `cfi` as its CFI tag. This is the C
    /// program's `Run` binary as the dynamic loader sees it.
    fn build_run_image(needed: &[&str], cfi: [u8; SYSCALL_TABLE_HASH_LEN]) -> Vec<u8> {
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: 1,
            needed_count: u16::try_from(needed.len()).unwrap(),
            entry: 0x1000,
            cfi_tag: cfi,
        };
        let segment = Segment {
            vaddr: 0x1000,
            file_offset: 0,
            file_size: 0x1000,
            mem_size: 0x1000,
            permission: RxePermission::ReadExecute,
        };
        let mut out = header.to_le_bytes().to_vec();
        out.extend_from_slice(&segment.to_le_bytes());
        for reference in needed {
            out.extend_from_slice(
                &NeededLibrary::from_reference(reference)
                    .unwrap()
                    .to_le_bytes(),
            );
        }
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
            "Help",
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
        run: Vec<u8>,
        entries_fail: bool,
    }
    impl MockStore {
        fn good(caps: &[CapabilityId]) -> Self {
            Self {
                entries: full_layout(),
                appinfo: build_manifest(caps, KERNEL_HASH, CONTENT_HASH),
                content: CONTENT_HASH,
                run: build_run_image(&[RUNTIME_LIB], KERNEL_HASH),
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
        fn read_run(&self, _bundle: &str) -> Result<Vec<u8>, Errno> {
            Ok(self.run.clone())
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

    /// Records the exact byte stream the loader asked to be verified, so a
    /// test can pin what the signature covers.
    struct CapturingVerifier(core::cell::RefCell<alloc::vec::Vec<u8>>);
    impl Verifier for CapturingVerifier {
        fn verify(&self, signed: &[u8], _: &[u8; 64], _: &[u8; 32]) -> Result<(), Errno> {
            *self.0.borrow_mut() = signed.to_vec();
            Ok(())
        }
    }

    #[test]
    fn signature_covers_the_capability_body() {
        // Regression: the signed message must be the header prefix
        // concatenated with the capability/MIME body — a verifier fed the
        // header alone would let a tampered store swap capability ids
        // behind a valid signature.
        let store = MockStore::good(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let verifier = CapturingVerifier(core::cell::RefCell::new(alloc::vec::Vec::new()));
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        loader
            .load("/Apps/Example.app", &cap_set(&[CapabilityId::NET_RAW]))
            .expect("loads");

        let signed = verifier.0.borrow();
        let manifest = store.read_appinfo("/Apps/Example.app").expect("manifest");
        let body = &manifest[AppInfoHeader::WIRE_LEN..];
        assert!(!body.is_empty(), "fixture must carry a capability body");
        assert_eq!(signed.len(), AppInfoHeader::signed_range().end + body.len());
        assert_eq!(
            &signed[..AppInfoHeader::signed_range().end],
            &manifest[AppInfoHeader::signed_range()]
        );
        assert_eq!(&signed[AppInfoHeader::signed_range().end..], body);
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
            run: build_run_image(&[RUNTIME_LIB], KERNEL_HASH),
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
    fn resolves_c_runtime_from_system_libraries() {
        // A C-compiled bundle whose Run binary dynamically links the curated
        // System runtime / C ABI library plus one private bundle library.
        let mut store = MockStore::good(&[]);
        store.run = build_run_image(
            &[RUNTIME_LIB, "/Apps/Example.app/Libraries/private.so"],
            KERNEL_HASH,
        );
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));

        let app = loader
            .load("/Apps/Example.app", &CapabilitySet::empty())
            .expect("loads");
        let libs = app.libraries();
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].reference, RUNTIME_LIB);
        assert_eq!(libs[0].scope, LibraryScope::System);
        assert_eq!(libs[1].reference, "/Apps/Example.app/Libraries/private.so");
        assert_eq!(libs[1].scope, LibraryScope::Bundle);
        assert_eq!(sink.count(events::LIBRARY_RESOLVED), 2);
        assert_eq!(sink.count(events::APP_LOADED), 1);
    }

    #[test]
    fn intersects_capabilities_for_c_bundle() {
        // The launching user holds only NET_RAW; FS_MOUNT is dropped, and the
        // C runtime still resolves from /System/Libraries.
        let store = MockStore::good(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));

        let user = cap_set(&[CapabilityId::NET_RAW]);
        let app = loader.load("/Apps/Example.app", &user).expect("loads");
        assert!(app.granted().contains(CapabilityId::NET_RAW));
        assert!(!app.granted().contains(CapabilityId::FS_MOUNT));
        assert_eq!(app.granted().len(), 1);
        assert_eq!(app.libraries().len(), 1);
        assert_eq!(app.libraries()[0].scope, LibraryScope::System);
    }

    #[test]
    fn refuses_run_image_out_of_tree_library() {
        let mut store = MockStore::good(&[]);
        store.run = build_run_image(&["/System/Kernel/secret.so"], KERNEL_HASH);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::Library(LibraryError::OutsidePolicy))
        );
        assert_eq!(sink.count(events::LIBRARY_REFUSED), 1);
    }

    #[test]
    fn refuses_run_image_with_cfi_mismatch() {
        let mut store = MockStore::good(&[]);
        store.run = build_run_image(&[RUNTIME_LIB], [0x99; SYSCALL_TABLE_HASH_LEN]);
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert_eq!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::RunImage(RxeError::InterfaceHashMismatch))
        );
        assert_eq!(sink.count(events::APP_RUN_IMAGE_INVALID), 1);
    }

    #[test]
    fn refuses_malformed_run_image() {
        let mut store = MockStore::good(&[]);
        store.run = alloc::vec![0u8, 1, 2, 3];
        let verifier = AcceptVerifier;
        let sink = RecordingSink::new();
        let loader = AppLoader::new(cfg(&store, &verifier, &sink));
        assert!(matches!(
            loader.load("/Apps/Example.app", &CapabilitySet::empty()),
            Err(AppError::RunImage(_))
        ));
        assert_eq!(sink.count(events::APP_RUN_IMAGE_INVALID), 1);
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
