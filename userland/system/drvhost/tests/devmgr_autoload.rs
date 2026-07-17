//! End-to-end Stage 4.HW autoload test: the device manager's match walk
//! drives this crate's load gate.
//!
//! `tairix-devmgr` owns matching *policy* and reaches the load
//! *mechanism* only through its `DriverLoader` seam. This test
//! closes the loop with the real pipeline: signed `.rxe` images carrying
//! bind tables are decoded fail-closed by [`tairix_drvhost::ParsedImage`],
//! matched against a hardware tree, and the winners are loaded through a
//! real [`tairix_drvhost::Host`] — signature verification, capability
//! gate, spawner hand-off and all. Both subsystems' audit records
//! (`7000`-range and `13000`-range) land on the same sink.

mod fixtures;

use fixtures::{
    build_signed_image_with_bind_keys, pubkey_of, test_signing_key, MemSource, RecordingSink,
    SingleSpawner,
};

use tairix_abi::{
    CapabilityId, DriverBindKey, DriverHandle, DriverKind, Errno, HwDeviceClass, HwMatchKey,
    HwNode, ABI_VERSION_CURRENT, DRIVER_MANIFEST_MAX_BIND_KEYS, HW_NODE_ROOT,
};
use tairix_caps::CapabilitySet;
use tairix_devmgr::{DeviceManager, DriverCandidate, DriverLoader};
use tairix_drvhost::{Host, HostConfig, ImageSource as _, ParsedImage};

const SYS_HASH: [u8; 32] = [0x11; 32];

/// Adapts the production `Host::load` pipeline to the device manager's
/// `DriverLoader` seam, mapping refusals onto the `abi-v1` error
/// surface exactly as a deployment integration point would.
struct HostLoader<'h, 'x> {
    host: &'x mut Host<'h>,
}

impl DriverLoader for HostLoader<'_, '_> {
    fn load(
        &mut self,
        path: &str,
        _resources: &[tairix_abi::hwtree::HwResource],
        caller_caps: &CapabilitySet,
    ) -> Result<DriverHandle, Errno> {
        // This adapter exercises the *verification* gate with an
        // in-process register spawner: the verified driver runs in this
        // host's own domain and reaches hardware through the host's own
        // capability-gated view, so the matched node's resource grants
        // are not minted here (they are minted by the process-spawning
        // loader that creates a fresh driver process). The argument is accepted to satisfy the seam.
        self.host
            .load(path, caller_caps)
            .map_err(tairix_drvhost::HostError::as_errno)
    }
}

fn compat(s: &[u8]) -> HwMatchKey {
    HwMatchKey::compatible(s).expect("test compatible strings fit HW_COMPATIBLE_MAX")
}

fn caller_with_drv_load() -> CapabilitySet {
    let mut set = CapabilitySet::empty();
    set.insert(CapabilityId::DRV_LOAD);
    set
}

/// Decode a stored image's bind table the same way the load gate does.
fn decode_bind_table(source: &MemSource, path: &str) -> Vec<DriverBindKey> {
    let mut bytes = Vec::new();
    source.read(path, &mut bytes).expect("fixture image exists");
    let parsed = ParsedImage::parse(&bytes).expect("fixture image parses");
    let mut buf =
        [DriverBindKey::new(0, HwMatchKey::virtio(0)); DRIVER_MANIFEST_MAX_BIND_KEYS as usize];
    let n = parsed
        .decode_bind_table(&mut buf)
        .expect("fixture bind table decodes");
    buf[..n].to_vec()
}

#[test]
fn autoload_matches_and_loads_through_the_real_gate() {
    // `NODE_UNBOUND` is a `Debug` record (filtered out on a default `Info`
    // boot); lower the threshold so the test observes it.
    tairix_log::set_max_level(tairix_log::Level::Trace);
    let sk = test_signing_key();
    let trusted = [pubkey_of(&sk)];

    let emmc_keys = [DriverBindKey::new(5, compat(b"brcm,bcm2711-emmc2"))];
    let uart_keys = [DriverBindKey::new(2, compat(b"arm,pl011"))];
    let mut source = MemSource::new();
    source.images.insert(
        "/System/Drivers/emmc2".into(),
        build_signed_image_with_bind_keys(
            &sk,
            DriverKind::UserSpace,
            SYS_HASH,
            &[],
            &emmc_keys,
            b"emmc2",
        ),
    );
    source.images.insert(
        "/System/Drivers/uart".into(),
        build_signed_image_with_bind_keys(
            &sk,
            DriverKind::UserSpace,
            SYS_HASH,
            &[],
            &uart_keys,
            b"uart",
        ),
    );

    let emmc_table = decode_bind_table(&source, "/System/Drivers/emmc2");
    let uart_table = decode_bind_table(&source, "/System/Drivers/uart");
    let candidates = [
        DriverCandidate {
            path: "/System/Drivers/emmc2",
            bind_keys: &emmc_table,
        },
        DriverCandidate {
            path: "/System/Drivers/uart",
            bind_keys: &uart_table,
        },
    ];

    let mut storage = HwNode::new(2, 1, HwDeviceClass::Storage);
    storage
        .push_match_key(compat(b"brcm,bcm2711-emmc2"))
        .expect("key fits");
    let mut serial = HwNode::new(3, 1, HwDeviceClass::Serial);
    serial
        .push_match_key(compat(b"arm,pl011"))
        .expect("key fits");
    // A display node nothing matches: a headless image leaves it
    // unbound and that is never an error.
    let mut display = HwNode::new(4, 1, HwDeviceClass::Display);
    display
        .push_match_key(HwMatchKey::virtio(16))
        .expect("key fits");
    let tree = [
        HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
        storage,
        serial,
        display,
    ];

    let spawner = SingleSpawner;
    let sink = RecordingSink::new();
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: SYS_HASH,
        accepted_abi_version: ABI_VERSION_CURRENT,
        source: &source,
        spawner: &spawner,
        sink: &sink,
        virtio_host_factory: None,
        mmio_mapper: None,
    });

    let report = DeviceManager::new(&sink).autoload(
        &tree,
        &candidates,
        &caller_with_drv_load(),
        &mut HostLoader { host: &mut host },
    );

    assert_eq!(report.bindings.len(), 2);
    assert_eq!(report.bindings[0].node, 2);
    assert_eq!(report.bindings[1].node, 3);
    assert_eq!(report.unbound, 1, "the display node stays unbound");
    assert_eq!(report.ties_rejected, 0);
    assert_eq!(report.load_failures, 0);
    assert_eq!(host.loaded_count(), 2, "both winners passed the gate");

    let ids = sink.ids();
    // The gate's own audit trail and the device manager's interleave on
    // the shared sink: two loads, two bindings, one unbound node.
    assert_eq!(ids.iter().filter(|&&id| id == 7_001).count(), 2);
    assert_eq!(ids.iter().filter(|&&id| id == 13_001).count(), 2);
    assert_eq!(ids.iter().filter(|&&id| id == 13_002).count(), 1);
}

#[test]
fn autoload_without_cap_drv_load_fails_closed_at_the_real_gate() {
    let sk = test_signing_key();
    let trusted = [pubkey_of(&sk)];

    let uart_keys = [DriverBindKey::new(2, compat(b"arm,pl011"))];
    let mut source = MemSource::new();
    source.images.insert(
        "/System/Drivers/uart".into(),
        build_signed_image_with_bind_keys(
            &sk,
            DriverKind::UserSpace,
            SYS_HASH,
            &[],
            &uart_keys,
            b"uart",
        ),
    );
    let uart_table = decode_bind_table(&source, "/System/Drivers/uart");
    let candidates = [DriverCandidate {
        path: "/System/Drivers/uart",
        bind_keys: &uart_table,
    }];

    let mut serial = HwNode::new(2, 1, HwDeviceClass::Serial);
    serial
        .push_match_key(compat(b"arm,pl011"))
        .expect("key fits");
    let tree = [serial];

    let spawner = SingleSpawner;
    let sink = RecordingSink::new();
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: SYS_HASH,
        accepted_abi_version: ABI_VERSION_CURRENT,
        source: &source,
        spawner: &spawner,
        sink: &sink,
        virtio_host_factory: None,
        mmio_mapper: None,
    });

    // The caller holds no CAP_DRV_LOAD: the gate refuses, the node
    // stays unbound, and nothing is loaded.
    let report = DeviceManager::new(&sink).autoload(
        &tree,
        &candidates,
        &CapabilitySet::empty(),
        &mut HostLoader { host: &mut host },
    );

    assert!(report.bindings.is_empty());
    assert_eq!(report.load_failures, 1);
    assert_eq!(host.loaded_count(), 0);

    let ids = sink.ids();
    assert!(ids.contains(&7_008), "gate refusal audited: {ids:?}");
    assert!(ids.contains(&13_004), "devmgr failure audited: {ids:?}");
    let events = sink.events.borrow();
    let failed = events
        .iter()
        .find(|e| e.id == 13_004)
        .expect("NODE_LOAD_FAILED present");
    assert!(
        failed.fields.iter().any(|(k, v)| k == "errno" && v == "6"),
        "PermissionDenied surfaces in the audit record: {failed:?}"
    );
}
