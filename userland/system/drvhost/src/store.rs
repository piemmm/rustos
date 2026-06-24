//! Signed driver-store scan — turn the installed `/System/Drivers/`
//! bundles into `devmgr` autoload candidates.
//!
//! RustOS does not ship a compiled-in list of *which* drivers exist: the discovered driver set is found at runtime by
//! scanning the installed signed bundles under `/System/Drivers/` and
//! reading each bundle's manifest bind table. This module is that scan,
//! sitting beside the load gate that already owns image parsing
//! ([`crate::ParsedImage`]) and byte fetching ([`crate::ImageSource`]),
//! so the same envelope splitter feeds both the scan and the load — the
//! match data can never drift from the gate's view of the bytes.
//!
//! # What the scan does — and what it deliberately does not
//!
//! For each bundle path the caller enumerated (a VFS directory walk of
//! `/System/Drivers/` in production; a fixed slice in tests) the scan:
//!
//! 1. reads the bundle bytes through the [`ImageSource`];
//! 2. parses the `.rxe` manifest header structurally
//!    ([`ParsedImage::parse`]); and
//! 3. decodes the bind table fail-closed
//!    ([`ParsedImage::decode_bind_table`]),
//!    rejecting a malformed entry rather than guessing.
//!
//! A bundle that fails any step is **skipped and logged**, never fatal:
//! one malformed bundle cannot block the rest of the boot. The successful bundles become owned
//! [`ScannedDriver`] records whose borrowed [`DriverCandidate`] view feeds
//! `rustos_devmgr`'s `DeviceManager::autoload`.
//!
//! The scan is a **match** step only. Building a candidate from a bundle's
//! bind table is *necessary but never sufficient* to run it: the bundle's
//! Ed25519 signature, syscall-hash, capability set, and `kind` are verified
//! by the load gate ([`crate::Host::load`]) when — and only when — the
//! candidate wins a hardware-tree node. The scan
//! therefore never trusts the manifest beyond its structure; it has no
//! authority to grant and checks none.
//!
//! # Layering
//!
//! The scan produces canonical [`rustos_devmatch::DriverCandidate`] values
//! directly (the single definition both the kernel floor catalogue and the
//! user-space `devmgr` consume), so the bin-crate boot
//! wiring — the one layer that may name both `drvhost` and `devmgr` — hands the candidates straight to
//! `DeviceManager::autoload` without re-typing them.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rustos_abi::{DriverBindKey, HwMatchKey, DRIVER_MANIFEST_MAX_BIND_KEYS};
use rustos_devmatch::DriverCandidate;
use rustos_log::{log as log_event, Event, Field, Level, Sink};

use crate::events;
use crate::host::HandleBuf;
use crate::image::ParsedImage;
use crate::source::ImageSource;
use crate::HostError;

/// One bundle accepted by [`scan_store`]: its logical store path and the
/// bind table decoded fail-closed from its signed manifest.
///
/// The record **owns** its bytes (the path string and the decoded bind
/// keys) so the scratch buffers the scan reused across bundles can be
/// dropped, while [`Self::candidate`] lends the borrowed
/// [`DriverCandidate`] view the matcher consumes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedDriver {
    path: String,
    bind_keys: Vec<DriverBindKey>,
}

impl ScannedDriver {
    /// Logical `/System/Drivers/` image path — the driver's stable id,
    /// understood verbatim by [`crate::Host::load`].
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The bind table decoded from this bundle's manifest, at most
    /// [`DRIVER_MANIFEST_MAX_BIND_KEYS`] entries.
    #[must_use]
    pub fn bind_keys(&self) -> &[DriverBindKey] {
        &self.bind_keys
    }

    /// Borrow this record as a [`DriverCandidate`] for the matcher.
    #[must_use]
    pub fn candidate(&self) -> DriverCandidate<'_> {
        DriverCandidate {
            path: &self.path,
            bind_keys: &self.bind_keys,
        }
    }
}

/// The drivers discovered by scanning the signed store: the autoload
/// candidate source.
///
/// Records are held in the order the caller enumerated them, so the
/// resulting candidate slice — and therefore the deterministic match
/// outcome — depends only on the input order, never on
/// internal iteration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DriverStore {
    drivers: Vec<ScannedDriver>,
    skipped: u32,
}

impl DriverStore {
    /// The accepted bundles, in enumeration order.
    #[must_use]
    pub fn drivers(&self) -> &[ScannedDriver] {
        &self.drivers
    }

    /// The number of bundles skipped during the scan (unreadable,
    /// malformed manifest, or a bind table that failed to decode).
    #[must_use]
    pub fn skipped(&self) -> u32 {
        self.skipped
    }

    /// `true` when no bundle was accepted — a headless or driverless
    /// install simply autoloads nothing, never an
    /// error.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    /// The number of accepted bundles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.drivers.len()
    }

    /// The accepted bundles as borrowed [`DriverCandidate`] values for
    /// [`rustos_devmatch::resolve`] / `DeviceManager::autoload`.
    #[must_use]
    pub fn candidates(&self) -> Vec<DriverCandidate<'_>> {
        self.drivers.iter().map(ScannedDriver::candidate).collect()
    }
}

/// Scan the signed driver store, building an autoload candidate per
/// readable, well-formed bundle.
///
/// `paths` is the set of bundle image paths the caller enumerated from
/// `/System/Drivers/` (a VFS directory walk in production); `source`
/// fetches each bundle's bytes. Every accepted bundle is audited
/// [`events::DRIVER_STORE_CANDIDATE`]; every skipped bundle is audited
/// [`events::DRIVER_STORE_ENTRY_SKIPPED`] with the reason and the scan
/// continues (fail closed for that bundle
/// only, never abort the boot).
///
/// The scan allocates one owned [`ScannedDriver`] per accepted bundle (a
/// path string plus at most [`DRIVER_MANIFEST_MAX_BIND_KEYS`] bind keys)
/// and reuses a single read buffer and a single fixed decode buffer across
/// bundles (no per-bundle scratch allocation).
#[must_use]
pub fn scan_store(source: &dyn ImageSource, paths: &[&str], sink: &dyn Sink) -> DriverStore {
    let mut store = DriverStore::default();
    // One read buffer, reused across bundles; cleared before each read
    // because `ImageSource::read` appends.
    let mut image: Vec<u8> = Vec::new();
    for &path in paths {
        image.clear();
        match scan_one(source, path, &mut image) {
            Ok(bind_keys) => {
                audit_candidate(sink, path, bind_keys.len());
                store.drivers.push(ScannedDriver {
                    path: path.to_string(),
                    bind_keys,
                });
            }
            Err(err) => {
                store.skipped += 1;
                audit_skip(sink, path, skip_reason(err));
            }
        }
    }
    store
}

/// Read, parse, and bind-decode a single bundle, returning its owned bind
/// table on success.
fn scan_one(
    source: &dyn ImageSource,
    path: &str,
    image: &mut Vec<u8>,
) -> Result<Vec<DriverBindKey>, HostError> {
    source.read(path, image).map_err(HostError::SourceFailed)?;
    let parsed = ParsedImage::parse(image)?;
    // Decode into a fixed buffer sized to the ABI's bind-key ceiling
    // (a validation bound, not a scalable capacity):
    // a manifest claiming more than the maximum overruns it and is
    // rejected fail-closed by `decode_bind_table`.
    let mut buf =
        [DriverBindKey::new(0, HwMatchKey::virtio(0)); DRIVER_MANIFEST_MAX_BIND_KEYS as usize];
    let count = parsed.decode_bind_table(&mut buf)?;
    Ok(buf[..count].to_vec())
}

/// A stable, human-readable reason string for the skip audit record,
/// mirroring the load gate's terse `reason` fields.
fn skip_reason(err: HostError) -> &'static str {
    match err {
        HostError::SourceFailed(_) => "source read",
        HostError::ImageTruncated => "image truncated",
        HostError::ManifestInvalid(_) => "manifest decode",
        HostError::BindKeyInvalid(_) => "bind key decode",
        HostError::CapabilityOutOfRange => "capability decode",
        // The scan only parses + bind-decodes, so the remaining variants
        // (signature, capability-set, kind, handle, spawner) are
        // unreachable here; classify them honestly rather than guessing.
        _ => "rejected",
    }
}

fn audit_candidate(sink: &dyn Sink, path: &str, bind_key_count: usize) {
    let mut cbuf = HandleBuf::new();
    let count_str = cbuf.format(bind_key_count as u64);
    let fields = [
        Field {
            key: "path",
            value: path,
        },
        Field {
            key: "bind_keys",
            value: count_str,
        },
    ];
    log_event(
        sink,
        &Event {
            level: Level::Info,
            id: events::DRIVER_STORE_CANDIDATE,
            message: "signed-store bundle accepted as autoload candidate",
            fields: &fields,
        },
    );
}

fn audit_skip(sink: &dyn Sink, path: &str, reason: &str) {
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
        sink,
        &Event {
            level: Level::Warn,
            id: events::DRIVER_STORE_ENTRY_SKIPPED,
            message: "signed-store bundle skipped during scan",
            fields: &fields,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use core::cell::RefCell;
    use rustos_abi::{DriverError, DriverKind, DriverManifest, DRIVER_MANIFEST_MAGIC};

    /// In-memory [`ImageSource`] keyed by logical path.
    struct MemStore(BTreeMap<String, Vec<u8>>);

    impl MemStore {
        fn new() -> Self {
            Self(BTreeMap::new())
        }

        fn insert(&mut self, path: &str, bytes: Vec<u8>) {
            self.0.insert(path.to_string(), bytes);
        }
    }

    impl ImageSource for MemStore {
        fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), rustos_abi::Errno> {
            match self.0.get(path) {
                Some(bytes) => {
                    buf.extend_from_slice(bytes);
                    Ok(())
                }
                None => Err(rustos_abi::Errno::NotFound),
            }
        }
    }

    struct CapturedEvent {
        id: u32,
        fields: Vec<(String, String)>,
    }

    struct RecordingSink {
        events: RefCell<Vec<CapturedEvent>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
            }
        }

        fn ids(&self) -> Vec<u32> {
            self.events.borrow().iter().map(|e| e.id).collect()
        }

        fn field_of(&self, id: u32, key: &str) -> Option<String> {
            self.events
                .borrow()
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.fields.iter().find(|(k, _)| k == key))
                .map(|(_, v)| v.clone())
        }
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(CapturedEvent {
                id: event.id.0,
                fields: event
                    .fields
                    .iter()
                    .map(|f| (f.key.to_string(), f.value.to_string()))
                    .collect(),
            });
        }
    }

    fn compat(s: &[u8]) -> HwMatchKey {
        match HwMatchKey::compatible(s) {
            Ok(key) => key,
            Err(_) => unreachable!("test compatible strings fit HW_COMPATIBLE_MAX"),
        }
    }

    /// Build a `.rxe` image with the given capabilities, bind keys, and
    /// payload. The signature fields are left zeroed: the scan never
    /// verifies the signature (the load gate does), so a
    /// structurally valid but unsigned image is a valid candidate.
    fn build_image(caps: &[u16], bind_keys: &[DriverBindKey], payload: &[u8]) -> Vec<u8> {
        let m = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: u8::try_from(bind_keys.len()).expect("test bind keys fit u8"),
            capability_count: u16::try_from(caps.len()).expect("test caps fit u16"),
            syscall_table_hash: [0u8; 32],
            signer_pubkey: [0u8; 32],
            signature: [0u8; 64],
        };
        let mut out = Vec::new();
        out.extend_from_slice(&m.to_le_bytes());
        for c in caps {
            out.extend_from_slice(&c.to_le_bytes());
        }
        for k in bind_keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn well_formed_bundles_become_candidates_in_enumeration_order() {
        let usb_keys = [DriverBindKey::new(5, HwMatchKey::pci(0, 0, 0x0C_0330))];
        let emmc_keys = [DriverBindKey::new(5, compat(b"brcm,bcm2711-emmc2"))];
        let mut src = MemStore::new();
        src.insert(
            "/System/Drivers/bus_usb",
            build_image(&[2], &usb_keys, b"payload-usb"),
        );
        src.insert(
            "/System/Drivers/emmc2",
            build_image(&[2], &emmc_keys, b"payload-emmc"),
        );
        let sink = RecordingSink::new();
        let store = scan_store(
            &src,
            &["/System/Drivers/emmc2", "/System/Drivers/bus_usb"],
            &sink,
        );
        assert_eq!(store.len(), 2);
        assert_eq!(store.skipped(), 0);
        assert!(!store.is_empty());
        // Enumeration order is preserved.
        assert_eq!(store.drivers()[0].path(), "/System/Drivers/emmc2");
        assert_eq!(store.drivers()[1].path(), "/System/Drivers/bus_usb");
        assert_eq!(store.drivers()[0].bind_keys(), &emmc_keys);
        assert_eq!(store.drivers()[1].bind_keys(), &usb_keys);
        // The borrowed candidate view is index-aligned with `drivers()`.
        let candidates = store.candidates();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].path, "/System/Drivers/emmc2");
        assert_eq!(candidates[0].bind_keys, &emmc_keys);
        // Two accepted candidates audited, no skips.
        assert_eq!(
            sink.ids(),
            [
                events::DRIVER_STORE_CANDIDATE.0,
                events::DRIVER_STORE_CANDIDATE.0
            ]
        );
        assert_eq!(
            sink.field_of(events::DRIVER_STORE_CANDIDATE.0, "bind_keys")
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn unreadable_bundle_is_skipped_and_logged_and_scan_continues() {
        let keys = [DriverBindKey::new(5, compat(b"arm,pl011"))];
        let mut src = MemStore::new();
        src.insert("/System/Drivers/uart", build_image(&[], &keys, b""));
        let sink = RecordingSink::new();
        // The first path is absent from the source.
        let store = scan_store(
            &src,
            &["/System/Drivers/missing", "/System/Drivers/uart"],
            &sink,
        );
        assert_eq!(store.len(), 1);
        assert_eq!(store.skipped(), 1);
        assert_eq!(store.drivers()[0].path(), "/System/Drivers/uart");
        assert_eq!(
            sink.ids(),
            [
                events::DRIVER_STORE_ENTRY_SKIPPED.0,
                events::DRIVER_STORE_CANDIDATE.0
            ]
        );
        assert_eq!(
            sink.field_of(events::DRIVER_STORE_ENTRY_SKIPPED.0, "reason")
                .as_deref(),
            Some("source read")
        );
    }

    #[test]
    fn malformed_manifest_is_skipped() {
        let keys = [DriverBindKey::new(5, compat(b"arm,pl011"))];
        let mut img = build_image(&[], &keys, b"");
        img[0] ^= 0xFF; // corrupt the magic
        let mut src = MemStore::new();
        src.insert("/System/Drivers/bad", img);
        let sink = RecordingSink::new();
        let store = scan_store(&src, &["/System/Drivers/bad"], &sink);
        assert!(store.is_empty());
        assert_eq!(store.skipped(), 1);
        assert_eq!(
            sink.field_of(events::DRIVER_STORE_ENTRY_SKIPPED.0, "reason")
                .as_deref(),
            Some("manifest decode")
        );
    }

    #[test]
    fn invalid_bind_key_is_skipped() {
        let keys = [DriverBindKey::new(5, compat(b"arm,pl011"))];
        let mut img = build_image(&[], &keys, b"");
        // Corrupt the first bind entry's reserved field (header + no caps).
        let bind_start = ParsedImage::HEADER_LEN;
        img[bind_start + 2] = 1;
        let mut src = MemStore::new();
        src.insert("/System/Drivers/badbind", img);
        let sink = RecordingSink::new();
        let store = scan_store(&src, &["/System/Drivers/badbind"], &sink);
        assert!(store.is_empty());
        assert_eq!(store.skipped(), 1);
        assert_eq!(
            sink.field_of(events::DRIVER_STORE_ENTRY_SKIPPED.0, "reason")
                .as_deref(),
            Some("bind key decode")
        );
    }

    #[test]
    fn a_bundle_with_no_bind_keys_is_a_valid_zero_key_candidate() {
        // A driver that declares no bind table matches no node, but the
        // scan still accepts it structurally — `devmatch` simply never
        // resolves it.
        let mut src = MemStore::new();
        src.insert("/System/Drivers/none", build_image(&[1], &[], b"body"));
        let sink = RecordingSink::new();
        let store = scan_store(&src, &["/System/Drivers/none"], &sink);
        assert_eq!(store.len(), 1);
        assert!(store.drivers()[0].bind_keys().is_empty());
        assert_eq!(
            sink.field_of(events::DRIVER_STORE_CANDIDATE.0, "bind_keys")
                .as_deref(),
            Some("0")
        );
    }

    #[test]
    fn empty_store_yields_no_candidates() {
        let src = MemStore::new();
        let sink = RecordingSink::new();
        let store = scan_store(&src, &[], &sink);
        assert!(store.is_empty());
        assert_eq!(store.skipped(), 0);
        assert!(store.candidates().is_empty());
        assert!(sink.ids().is_empty());
    }

    #[test]
    fn skip_reason_classifies_each_scan_error() {
        assert_eq!(
            skip_reason(HostError::SourceFailed(rustos_abi::Errno::NotFound)),
            "source read"
        );
        assert_eq!(skip_reason(HostError::ImageTruncated), "image truncated");
        assert_eq!(
            skip_reason(HostError::ManifestInvalid(DriverError::BadMagic)),
            "manifest decode"
        );
        assert_eq!(
            skip_reason(HostError::BindKeyInvalid(DriverError::BadMagic)),
            "bind key decode"
        );
        assert_eq!(
            skip_reason(HostError::CapabilityOutOfRange),
            "capability decode"
        );
    }

    #[test]
    fn candidates_feed_the_matcher() {
        // End-to-end with the real `devmatch::resolve`: a scanned store's
        // candidates resolve a node exactly as a hand-built candidate set
        // would (one match definition).
        let usb_keys = [DriverBindKey::new(5, HwMatchKey::pci(0, 0, 0x0C_0330))];
        let mut src = MemStore::new();
        src.insert("/System/Drivers/bus_usb", build_image(&[], &usb_keys, b""));
        let sink = RecordingSink::new();
        let store = scan_store(&src, &["/System/Drivers/bus_usb"], &sink);
        let candidates = store.candidates();
        let vl805 = [HwMatchKey::pci(0x1106, 0x3483, 0x0C_0330)];
        assert_eq!(
            rustos_devmatch::resolve(&vl805, &candidates),
            rustos_devmatch::MatchResolution::Winner {
                candidate: 0,
                priority: 5,
            }
        );
    }
}
