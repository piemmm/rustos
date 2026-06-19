//! Production driver-autoload boot wiring (`plans/PI.md` P10 5d-2-ii; PLAN
//! Stage 4.HW item 5).
//!
//! This is the one composition that turns the discovered hardware tree and
//! the installed signed driver store into running user-space drivers — the
//! "drivers in user space by discovery" steady state (`AGENTS.md` §4 / §18).
//! It threads the already-landed building blocks together, adding no policy
//! of its own:
//!
//! 1. [`rustos_drvhost::store::scan_store`] reads each installed
//!    `/System/Drivers/` bundle (paths discovered by
//!    [`rustos_kernel_core::enumerate_driver_store`]) through the supplied
//!    [`ImageSource`] and decodes its manifest bind table fail-closed. This
//!    is a **match** step only: it grants no authority and verifies no
//!    signature (`AGENTS.md` §18.6).
//! 2. [`rustos_devmgr::DeviceManager::autoload`] resolves every node of the
//!    discovered tree against those candidates through the shared
//!    [`rustos_devmatch`] policy (`AGENTS.md` §18.3), leaving an unmatched
//!    node unbound and logged (`AGENTS.md` §18.4).
//! 3. Each winning node's driver is loaded through
//!    [`crate::driver_spawn_loader::SpawnDriverLoader`], which runs the
//!    signed `drvhost::Host::load` gate (Ed25519 signature against
//!    `trusted`, the `CAP_DRV_LOAD` gate, the syscall-table-hash match,
//!    bind-table validation) and then **spawns** the verified payload into
//!    its own hardware-isolated process, minting it one device-resource
//!    grant per [`rustos_abi::hwtree::HwResource`] the matched node
//!    requested — and nothing more (`AGENTS.md` §18.3 / §4 — no ambient
//!    authority).
//!
//! Security is the floor (`AGENTS.md` §5.4 / §23.1): a candidate that fails
//! the signed gate fails *that node* closed and the walk continues, so one
//! bad bundle can never block the rest of the boot; a node matching nothing
//! is left unbound, never an error. The whole pipeline is fail-closed and
//! every outcome is audited through the supplied [`Sink`] (the `drvhost`
//! `7000`-range gate records and the `devmgr` `13000`-range match records
//! interleave on it).
//!
//! # Layering
//!
//! This function lives in the kernel binary — the one layer permitted to
//! name both `rustos_devmgr` and `rustos_drvhost` (`AGENTS.md` §17.4) — and
//! is the staged production entry point the boot path will drive once the
//! root volume that backs the store is mounted in production (`plans/PI.md`
//! P10 5d-2-ii "Remaining" / the P11 root-mount increment). Until then it is
//! exercised end to end by the `-M virt` autoload vertical, exactly as the
//! sibling [`crate::driver_spawn_loader::SpawnDriverLoader`] is.

use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::HwNode;
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_devmgr::{AutoloadReport, DeviceManager};
use rustos_drvhost::store::scan_store;
use rustos_drvhost::{ImageSource, Sink};
use rustos_kernel_core::VfsError;

use crate::driver_spawn_loader::{DriverProcessSpawn, SpawnDriverLoader};
use crate::system_files::SystemFileService;

/// Scan the installed signed driver store and autoload a user-space driver
/// for every discovered hardware-tree node that binds one.
///
/// * `tree` — the discovered hardware tree (`AGENTS.md` §18.1). On a real
///   boot the architecture port's [`rustos_kernel_core`] discovery and the
///   bootstrap-floor bus drivers build it; on `-M virt` a virtio device
///   stands in for the metal controller (§0.4 — no Pi-board QEMU vertical).
/// * `store_paths` — the `/System/Drivers/` bundle paths
///   [`rustos_kernel_core::enumerate_driver_store`] discovered on the
///   mounted root volume.
/// * `image_source` — reads a bundle's bytes by path (the
///   [`crate::system_files::SystemFileService`] over the mounted `/System`
///   volume in production).
/// * `trusted` — the driver-signing trust anchors the load gate verifies
///   every winning bundle against (`AGENTS.md` §8 / §9).
/// * `spawn` — the architecture process-creation mechanism the verified
///   payload is spawned through (`AGENTS.md` §2.2 — kept behind the seam so
///   this stays scheduler-agnostic, §17.1).
/// * `args` — the startup-argument vector handed to every spawned driver.
/// * `caller_caps` — the capability set the load gate intersects each
///   driver manifest's request with (`AGENTS.md` §5.2); it must hold
///   `CAP_DRV_LOAD` or every load fails closed (§5.4).
/// * `sink` — the audit sink every scan, match, load, and spawn decision is
///   logged through (`AGENTS.md` §18.3 / §19.4).
///
/// Returns the [`AutoloadReport`] summarising every bound node, unbound
/// node, refused packaging tie, and failed load — never a panic
/// (`AGENTS.md` §2.9).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn autoload_drivers(
    tree: &[HwNode],
    store_paths: &[&str],
    image_source: &dyn ImageSource,
    trusted: &[Ed25519PublicKey],
    spawn: &dyn DriverProcessSpawn,
    args: &[&[u8]],
    caller_caps: &CapabilitySet,
    sink: &dyn Sink,
) -> AutoloadReport {
    // Match-only store scan: parse each bundle's manifest bind table
    // fail-closed (`AGENTS.md` §18.6). A malformed/unreadable bundle is
    // skipped and logged inside `scan_store`, never fatal (§18.4).
    let store = scan_store(image_source, store_paths, sink);
    let candidates = store.candidates();
    // The signed-gate + process-spawn loader: every winner is verified and
    // spawned with exactly its matched node's resource grants (`AGENTS.md`
    // §18.3). The loader re-reads the winning bundle's bytes through the
    // same `image_source`, so a candidate whose bytes changed between the
    // scan and the load fails the signature, fail-closed (§5.4).
    let mut loader = SpawnDriverLoader::new(trusted, image_source, sink, spawn, args);
    DeviceManager::new(sink).autoload(tree, &candidates, caller_caps, &mut loader)
}

/// Enumerate the `/System/Drivers/` store off the **mounted root volume**
/// and autoload a user-space driver for every discovered node that binds
/// one — the single production entry the boot path drives once the root is
/// mounted (`plans/PI.md` P10 5d-2-ii; `AGENTS.md` §18.3 / §18.6).
///
/// This is the thin glue [`autoload_drivers`] is missing for the live boot
/// path: it sources the store paths and the bundle bytes from the just-
/// mounted root volume `fs` rather than from a caller-supplied list, then
/// defers entirely to [`autoload_drivers`] for the match + signed-gate +
/// spawn pipeline. It adds no policy of its own (`AGENTS.md` §2.2).
///
/// It builds one [`SystemFileService`] over `fs` and drives both the store
/// listing and the per-bundle reads through it. The service holds the one
/// `&mut` borrow of `fs`, but its [`list_store`](SystemFileService::list_store)
/// and [`ImageSource`] read are strictly sequential and single-threaded —
/// the store is listed once, then one bundle is read at a time — so the
/// borrow never overlaps.
///
/// * `fs` — the mounted volume's filesystem driver (rustfs on a real
///   installation), the §5.3-checked surface both the store walk and the
///   bundle-byte reads delegate through under the kernel's bootstrap
///   identity (`AGENTS.md` §5.1 — no ambient power). On the design-B path
///   this is the read-only `/System` volume scanned before unlock.
/// * `store_root` — the driver store's path relative to `fs`'s own root
///   (`rustos_kernel_core::DRIVER_STORE_PATH` on a whole-root volume,
///   `SYSTEM_VOLUME_STORE_PATH` on a `/System` volume); the one root the
///   enumeration and the bundle reader both use (`AGENTS.md` §2.2).
/// * `tree` — the discovered hardware tree (`AGENTS.md` §18.1).
/// * `trusted` — the driver-signing trust anchors the load gate verifies
///   every winning bundle against (`AGENTS.md` §8 / §9).
/// * `spawn` — the architecture process-creation mechanism, behind the
///   [`DriverProcessSpawn`] seam so this stays scheduler-agnostic (§17.1).
/// * `args` — the startup-argument vector handed to every spawned driver.
/// * `caller_caps` — the capability set the load gate intersects each
///   manifest request with; must hold `CAP_DRV_LOAD` or every load fails
///   closed (`AGENTS.md` §5.2 / §5.4).
/// * `sink` — the audit sink every scan, match, load, and spawn decision is
///   logged through (`AGENTS.md` §18.3 / §19.4).
///
/// # Errors
///
/// [`VfsError`] only if the kernel's private root mount cannot be built for
/// the file service ([`SystemFileService::open`]) — the one fail-closed
/// path that prevents any autoload (`AGENTS.md` §2.9). A store that is
/// missing, empty, or full of malformed bundles is **not** an error: it
/// simply yields no candidates and binds nothing (`AGENTS.md` §18.4), so
/// the [`AutoloadReport`] is returned in `Ok`.
#[allow(clippy::too_many_arguments)]
pub fn autoload_from_mounted_root<F>(
    fs: &mut F,
    store_root: &str,
    tree: &[HwNode],
    trusted: &[Ed25519PublicKey],
    spawn: &dyn DriverProcessSpawn,
    args: &[&[u8]],
    caller_caps: &CapabilitySet,
    sink: &dyn Sink,
) -> Result<AutoloadReport, VfsError>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    // 1. Open the read-only `/System` file service over the mounted volume,
    //    building its root-backed VFS once. A failure to build the private
    //    root mount is the sole hard refusal (fail closed, `AGENTS.md`
    //    §2.9).
    let service = SystemFileService::open(fs, store_root)?;

    // 2. Structural path discovery off the mounted store (`AGENTS.md`
    //    §18.6). This reads no bundle and trusts nothing; it audits its own
    //    `DriverStoreScanned` outcome and is fail-closed — a missing or
    //    unreadable store yields fewer (or zero) paths, never an error.
    let store_paths = service.list_store(sink);

    // 3. The match + signed-gate + spawn pipeline, verbatim. `scan_store`
    //    wants `&[&str]`, so borrow the owned paths; each winning bundle's
    //    bytes are read back through the *same* service (its `ImageSource`).
    let path_refs: Vec<&str> = store_paths
        .iter()
        .map(alloc::string::String::as_str)
        .collect();
    Ok(autoload_drivers(
        tree,
        &path_refs,
        &service,
        trusted,
        spawn,
        args,
        caller_caps,
        sink,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec;
    use alloc::vec::Vec;

    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::hwtree::HwResource;
    use rustos_abi::{
        CapabilityId, DriverBindKey, DriverKind, DriverManifest, Errno, HwDeviceClass, HwMatchKey,
        ABI_VERSION_CURRENT, DRIVER_MANIFEST_MAGIC, HW_NODE_ROOT,
    };
    use rustos_log::{Event, Sink as LogSink};

    use crate::test_support::MockRootFs;

    /// Deterministic driver-signing seed for the test trust anchor. A
    /// distinct key models an untrusted signer.
    const TEST_SEED: [u8; 32] = *b"rustos-autoload-test-signing/v1!";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&TEST_SEED)
    }

    fn untrusted_key() -> SigningKey {
        let mut seed = TEST_SEED;
        seed[0] ^= 0xFF;
        SigningKey::from_bytes(&seed)
    }

    fn pubkey_of(sk: &SigningKey) -> Ed25519PublicKey {
        Ed25519PublicKey::from_bytes(&sk.verifying_key().to_bytes()).expect("well-formed key")
    }

    /// The kernel syscall-table hash the gate matches a manifest against;
    /// the bundles are stamped with it so the hash check passes.
    fn sys_hash() -> [u8; 32] {
        rustos_kernel_syscall::SYSCALL_TABLE_HASH
    }

    /// Build a signed `kind = UserSpace` `.rxe` bundle the same way the
    /// production build glue and the `drvhost` fixtures do: the signature
    /// covers `header[..WIRE_LEN-64] || cap_body || bind_table || payload`
    /// (`AGENTS.md` §2.2 — one wire format, here mirrored as test glue,
    /// like the `virtio_boot` wiring test).
    fn build_signed_bundle(
        sk: &SigningKey,
        caps: &[CapabilityId],
        bind_keys: &[DriverBindKey],
        payload: &[u8],
    ) -> Vec<u8> {
        let signer_pubkey: [u8; 32] = sk.verifying_key().to_bytes();
        let mut manifest = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: u8::try_from(bind_keys.len()).expect("bind keys fit u8"),
            capability_count: u16::try_from(caps.len()).expect("caps fit u16"),
            syscall_table_hash: sys_hash(),
            signer_pubkey,
            signature: [0u8; 64],
        };
        let mut cap_body = Vec::new();
        for c in caps {
            cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
        }
        let mut bind_body = Vec::new();
        for k in bind_keys {
            bind_body.extend_from_slice(&k.to_le_bytes());
        }
        let header = manifest.to_le_bytes();
        let signed_end = DriverManifest::WIRE_LEN - 64;
        let mut message = Vec::new();
        message.extend_from_slice(&header[..signed_end]);
        message.extend_from_slice(&cap_body);
        message.extend_from_slice(&bind_body);
        message.extend_from_slice(payload);
        manifest.signature = sk.sign(&message).to_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(&manifest.to_le_bytes());
        out.extend_from_slice(&cap_body);
        out.extend_from_slice(&bind_body);
        out.extend_from_slice(payload);
        out
    }

    /// In-memory [`ImageSource`] mapping a `/System/Drivers/` path to bytes.
    struct MemSource {
        images: BTreeMap<String, Vec<u8>>,
    }

    impl MemSource {
        fn new() -> Self {
            Self {
                images: BTreeMap::new(),
            }
        }
        fn insert(&mut self, path: &str, bytes: Vec<u8>) {
            self.images.insert(path.to_string(), bytes);
        }
    }

    impl ImageSource for MemSource {
        fn read(&self, path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            match self.images.get(path) {
                Some(bytes) => {
                    buf.extend_from_slice(bytes);
                    Ok(())
                }
                None => Err(Errno::NotFound),
            }
        }
    }

    /// One recorded `spawn_driver` call: payload, granted caps, node grants.
    type RecordedSpawn = (Vec<u8>, CapabilitySet, Vec<HwResource>);

    /// Records every spawn so a test can assert what the gate forwarded.
    struct RecordingSpawn {
        calls: RefCell<Vec<RecordedSpawn>>,
        next_pid: RefCell<u64>,
    }

    impl RecordingSpawn {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                next_pid: RefCell::new(0x1000),
            }
        }
    }

    impl DriverProcessSpawn for RecordingSpawn {
        fn spawn_driver(
            &self,
            rxe: &[u8],
            granted: CapabilitySet,
            grants: &[HwResource],
            _args: &[&[u8]],
        ) -> Result<u64, Errno> {
            self.calls
                .borrow_mut()
                .push((rxe.to_vec(), granted, grants.to_vec()));
            let mut pid = self.next_pid.borrow_mut();
            *pid += 1;
            Ok(*pid)
        }
    }

    /// Sink that records every event id so audit coverage can be asserted.
    struct RecordingSink {
        ids: RefCell<Vec<u32>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                ids: RefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.ids.borrow().clone()
        }
    }

    impl LogSink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    fn caller_with_drv_load() -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        set.insert(CapabilityId::DRV_LOAD);
        set.insert(CapabilityId::MMIO_MAP);
        set.insert(CapabilityId::MEM_DMA);
        set
    }

    /// A keyboard node keyed by a virtio match key, carrying the register
    /// window + DMA constraint a USB-host driver is granted (`AGENTS.md`
    /// §18.3). Mirrors the `-M virt` autoload vertical's stand-in node.
    fn keyboard_node(match_key: HwMatchKey) -> HwNode {
        let mut node = HwNode::new(2, 1, HwDeviceClass::Input);
        node.push_match_key(match_key).expect("key fits");
        node.push_resource(HwResource::mmio(0x0a00_0000, 0x200))
            .expect("mmio resource fits");
        node.push_resource(HwResource::dma(0x3fff_ffff, 0x1000))
            .expect("dma resource fits");
        node
    }

    /// The discovered tree: a root plus one keyboard node keyed by
    /// `match_key`.
    fn tree_with_key(match_key: HwMatchKey) -> [HwNode; 2] {
        [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            keyboard_node(match_key),
        ]
    }

    #[test]
    fn a_matched_node_spawns_its_signed_driver_with_the_nodes_resources() {
        // §18.3: the discovered node binds the signed bundle whose bind
        // table matches it, and the spawn mechanism receives exactly the
        // verified payload plus the node's two resource requests.
        let key = HwMatchKey::virtio(0x1234);
        let bind_keys = [DriverBindKey::new(5, key)];
        let payload = b"the-usb-kbd-rxe-bytes";
        let sk = signing_key();
        let mut source = MemSource::new();
        source.insert(
            "/System/Drivers/usb_kbd",
            build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &bind_keys, payload),
        );
        let trusted = [pubkey_of(&sk)];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(key);
        let args: [&[u8]; 1] = [b"usb_kbd"];

        let report = autoload_drivers(
            &tree,
            &["/System/Drivers/usb_kbd"],
            &source,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        );

        assert_eq!(report.bindings.len(), 1, "the keyboard node binds");
        assert_eq!(report.bindings[0].node, 2);
        assert_eq!(report.unbound, 0);
        assert_eq!(report.ties_rejected, 0);
        assert_eq!(report.load_failures, 0);

        let calls = spawn.calls.borrow();
        assert_eq!(calls.len(), 1, "the verified driver is spawned once");
        assert_eq!(calls[0].0, payload, "the verified payload reaches spawn");
        assert_eq!(
            calls[0].2,
            vec![
                HwResource::mmio(0x0a00_0000, 0x200),
                HwResource::dma(0x3fff_ffff, 0x1000),
            ],
            "only the matched node's resources are granted (§18.3)"
        );
        // The devmgr bound-node audit (13_001) is emitted.
        assert!(sink.ids().contains(&13_001), "{:?}", sink.ids());
    }

    #[test]
    fn an_untrusted_signature_fails_the_node_closed_and_is_not_spawned() {
        // §5.4 / §23.1: a bundle signed by a key not on the trust-anchor
        // list is refused at the gate; the node fails closed (a load
        // failure) and the spawn mechanism is never reached.
        let key = HwMatchKey::virtio(0x1234);
        let bind_keys = [DriverBindKey::new(5, key)];
        let attacker = untrusted_key();
        let mut source = MemSource::new();
        source.insert(
            "/System/Drivers/usb_kbd",
            build_signed_bundle(&attacker, &[CapabilityId::MMIO_MAP], &bind_keys, b"evil"),
        );
        // The kernel trusts only the legitimate key.
        let trusted = [pubkey_of(&signing_key())];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(key);
        let args: [&[u8]; 1] = [b"usb_kbd"];

        let report = autoload_drivers(
            &tree,
            &["/System/Drivers/usb_kbd"],
            &source,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        );

        assert!(
            report.bindings.is_empty(),
            "no node binds an unsigned image"
        );
        assert_eq!(report.load_failures, 1, "the gate refuses the node");
        assert!(
            spawn.calls.borrow().is_empty(),
            "an unverified driver is never spawned (§5.4)"
        );
    }

    #[test]
    fn a_caller_without_cap_drv_load_loads_nothing() {
        // §5.4: the load gate requires CAP_DRV_LOAD of the caller; without
        // it every winning node fails closed and nothing is spawned.
        let key = HwMatchKey::virtio(0x1234);
        let bind_keys = [DriverBindKey::new(5, key)];
        let sk = signing_key();
        let mut source = MemSource::new();
        source.insert(
            "/System/Drivers/usb_kbd",
            build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &bind_keys, b"payload"),
        );
        let trusted = [pubkey_of(&sk)];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(key);
        let args: [&[u8]; 1] = [b"usb_kbd"];

        let report = autoload_drivers(
            &tree,
            &["/System/Drivers/usb_kbd"],
            &source,
            &trusted,
            &spawn,
            &args,
            &CapabilitySet::empty(),
            &sink,
        );

        assert!(report.bindings.is_empty());
        assert_eq!(report.load_failures, 1);
        assert!(spawn.calls.borrow().is_empty());
    }

    #[test]
    fn an_unmatched_node_is_left_unbound_not_an_error() {
        // §18.4: a node whose match keys bind no installed driver is left
        // unbound and logged — never an error, never a spawn.
        let store_key = HwMatchKey::virtio(0x1234);
        let bind_keys = [DriverBindKey::new(5, store_key)];
        let sk = signing_key();
        let mut source = MemSource::new();
        source.insert(
            "/System/Drivers/usb_kbd",
            build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &bind_keys, b"payload"),
        );
        let trusted = [pubkey_of(&sk)];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        // The discovered node advertises a *different* device id.
        let tree = tree_with_key(HwMatchKey::virtio(0x9999));
        let args: [&[u8]; 1] = [b"usb_kbd"];

        let report = autoload_drivers(
            &tree,
            &["/System/Drivers/usb_kbd"],
            &source,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        );

        assert!(report.bindings.is_empty());
        assert_eq!(report.unbound, 1);
        assert_eq!(report.load_failures, 0);
        assert!(spawn.calls.borrow().is_empty());
    }

    #[test]
    fn an_empty_store_binds_nothing() {
        // No installed bundles: every node is unbound and the boot proceeds
        // (`AGENTS.md` §18.4 — a missing driver is never fatal).
        let source = MemSource::new();
        let trusted = [pubkey_of(&signing_key())];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(HwMatchKey::virtio(0x1234));
        let args: [&[u8]; 1] = [b"usb_kbd"];

        let report = autoload_drivers(
            &tree,
            &[],
            &source,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        );

        assert!(report.bindings.is_empty());
        assert_eq!(report.unbound, 1);
        assert!(spawn.calls.borrow().is_empty());
    }

    // --- `autoload_from_mounted_root` (the live boot-path composition) ---
    //
    // These drive the production composition over a mock *mounted root
    // volume*: the store paths are discovered by `enumerate_driver_store`
    // walking the volume's `/System/Drivers/` tree (not a hand-supplied
    // list), and the bundle bytes are read back through the kernel's
    // root-backed VFS — the same two reads of the one `&mut` driver the
    // boot path performs.

    #[test]
    fn the_mounted_store_is_enumerated_and_a_matched_driver_is_spawned() {
        // End to end on a mounted root: the signed bundle planted under
        // `/System/Drivers/` is *discovered* by the store walk, matched to
        // the discovered node, verified, and spawned with exactly the
        // node's resources (`AGENTS.md` §18.3 / §18.6).
        let key = HwMatchKey::virtio(0x1234);
        let bind_keys = [DriverBindKey::new(5, key)];
        let payload = b"the-input-driver-rxe-bytes";
        let sk = signing_key();
        let bundle = build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &bind_keys, payload);

        let mut fs = MockRootFs::new();
        // The store tree is `<class>/<driver>` (§16.2); the walk finds the
        // bundle wherever it lives under the store root.
        fs.add_file("/System/Drivers/input/kbd", &bundle);

        let trusted = [pubkey_of(&sk)];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(key);
        let args: [&[u8]; 1] = [b"kbd"];

        let report = autoload_from_mounted_root(
            &mut fs,
            "/System/Drivers",
            &tree,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        )
        .expect("the private root mount builds");

        assert_eq!(report.bindings.len(), 1, "the discovered node binds");
        assert_eq!(report.bindings[0].node, 2);
        assert_eq!(report.load_failures, 0);

        let calls = spawn.calls.borrow();
        assert_eq!(calls.len(), 1, "the discovered driver is spawned once");
        assert_eq!(calls[0].0, payload, "the verified payload reaches spawn");
        assert_eq!(
            calls[0].2,
            vec![
                HwResource::mmio(0x0a00_0000, 0x200),
                HwResource::dma(0x3fff_ffff, 0x1000),
            ],
            "only the matched node's resources are granted (§18.3)"
        );
        // The store-scan candidate (drvhost 7030) and the devmgr bound-node
        // (13_001) audit records both appear on the shared sink.
        let ids = sink.ids();
        assert!(ids.contains(&7030), "store candidate audited: {ids:?}");
        assert!(ids.contains(&13_001), "bound node audited: {ids:?}");
    }

    #[test]
    fn an_empty_mounted_store_binds_nothing() {
        // A root with no `/System/Drivers/` tree is the headless / driverless
        // install: enumeration finds nothing, autoload binds nothing, and the
        // result is `Ok` — never an error (`AGENTS.md` §18.4 / §2.9).
        let mut fs = MockRootFs::new();
        let trusted = [pubkey_of(&signing_key())];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(HwMatchKey::virtio(0x1234));
        let args: [&[u8]; 1] = [b"kbd"];

        let report = autoload_from_mounted_root(
            &mut fs,
            "/System/Drivers",
            &tree,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        )
        .expect("the private root mount builds");

        assert!(report.bindings.is_empty());
        assert_eq!(report.unbound, 1, "the node is left unbound, not errored");
        assert!(spawn.calls.borrow().is_empty());
    }

    #[test]
    fn an_untrusted_bundle_on_the_mounted_root_fails_closed() {
        // A bundle discovered on the root but signed by a key not on the
        // trust-anchor list is refused at the load gate: the node fails
        // closed and nothing is spawned (`AGENTS.md` §5.4 / §23.1).
        let key = HwMatchKey::virtio(0x1234);
        let bind_keys = [DriverBindKey::new(5, key)];
        let attacker = untrusted_key();
        let bundle = build_signed_bundle(&attacker, &[CapabilityId::MMIO_MAP], &bind_keys, b"evil");

        let mut fs = MockRootFs::new();
        fs.add_file("/System/Drivers/input/kbd", &bundle);

        // The kernel trusts only the legitimate key.
        let trusted = [pubkey_of(&signing_key())];
        let spawn = RecordingSpawn::new();
        let sink = RecordingSink::new();
        let tree = tree_with_key(key);
        let args: [&[u8]; 1] = [b"kbd"];

        let report = autoload_from_mounted_root(
            &mut fs,
            "/System/Drivers",
            &tree,
            &trusted,
            &spawn,
            &args,
            &caller_with_drv_load(),
            &sink,
        )
        .expect("the private root mount builds");

        assert!(report.bindings.is_empty());
        assert_eq!(report.load_failures, 1, "the gate refuses the node");
        assert!(
            spawn.calls.borrow().is_empty(),
            "an unverified driver is never spawned (§5.4)"
        );
    }
}
