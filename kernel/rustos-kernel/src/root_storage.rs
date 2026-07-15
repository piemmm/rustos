//! The root-storage bind gate (`plans/PI.md` §3 P11 root-mount increment,
//! Chunk B-2): resolve which **discovered** hardware-tree node carries the
//! bootstrap root block device, and which floor block driver binds it.
//!
//! The kernel does not *hunt* for a disk, it
//! binds a block driver because that driver's signed bind table matched a
//! discovered node's identity. The match is the
//! one shared `lib/devmatch` policy the user-space `devmgr` autoloader also
//! uses, applied against the bootstrap-floor catalogue
//! ([`crate::driver_catalog`]) — the only drivers compiled into the kernel,
//! the storage path that must exist before the signed driver store under
//! `/System/Drivers/` is reachable.
//!
//! The gate is **resolution only** — it never reads, mounts, or trusts a
//! volume (the signed load gate and the mount path do that). It is
//! the front half the root-mount composition ([`crate::root_mount`]) builds
//! on: once a block device is bound here, the bring-up maps its windows
//! through an in-kernel `DriverHost`, mounts the filesystem, and loads the
//! users database.
//!
//! # Discovery vs. bus enumeration
//!
//! A device that the firmware tree describes directly — the Raspberry Pi 4
//! EMMC2 SD host (`compatible = "brcm,bcm2711-emmc2"`) — binds straight from
//! the discovered tree. A device behind a bus that must be *probed* — a
//! virtio-blk disk, whose bind key is the virtio device id read from the
//! transport, not a `compatible` string — appears only after the bus driver
//! enumerates it and attaches the probed child node.
//! This gate resolves whatever the tree it is handed contains: the raw
//! firmware tree on a direct-attached board, or the post-enumeration tree
//! once a bus driver has attached its children.
//!
//! # Fail closed
//!
//! A tree with no block device leaves the root unbound (logged, never a
//! panic). A tree with **more than one** distinct block device is
//! ambiguous: which volume is the root is a policy decision that needs an
//! explicit boot descriptor, not a guess, so the gate fails closed (binds
//! nothing) rather than picking one. Every outcome is audited under a
//! stable `ROOT_STORAGE_AUTOLOAD` event id.

use rustos_abi::HwNode;
use rustos_devmatch::MatchResolution;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::driver_catalog;

/// Audit event id for the root-storage bind gate (a
/// stable id for the security-relevant bind decision). Sits beside the
/// root-mount audit ids (`4133`/`4134`, [`crate::root_mount`]) in the
/// `4000..5000` range `kernel/core` owns.
const ROOT_STORAGE_AUTOLOAD: EventId = EventId(4135);

/// A discovered root block device and the floor driver that binds it.
///
/// Carries the matched node by value (a `HwNode` is a fixed-size record)
/// so the bring-up that follows has the node's resource-grant requests
/// (a driver receives only the resources its matched
/// node requested) without re-walking the tree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RootBlockBinding {
    /// The winning floor driver's `/System/Drivers/` image path.
    pub driver_path: &'static str,
    /// The discovered hardware-tree node that bound it (with its
    /// capability-grant resource requests).
    pub node: HwNode,
    /// The bind priority the match resolved at.
    pub priority: u16,
}

/// Resolve one node against the floor catalogue, returning a binding only
/// when the node binds a bootstrap-floor **block** driver
/// ([`driver_catalog::is_root_block_driver`]).
///
/// A node that matches nothing, ties (a packaging defect — fail closed), or binds a floor driver that is not a block driver is not a
/// root block device and yields [`None`] — the [`is_root_block_driver`]
/// check is kept as defence-in-depth even though the floor is storage-only
/// today.
///
/// [`is_root_block_driver`]: driver_catalog::is_root_block_driver
#[must_use]
fn classify(node: &HwNode) -> Option<RootBlockBinding> {
    if let MatchResolution::Winner {
        candidate,
        priority,
    } = driver_catalog::resolve_driver(node.match_keys())
    {
        let path = driver_catalog::driver_candidates()[candidate].path;
        if driver_catalog::is_root_block_driver(path) {
            return Some(RootBlockBinding {
                driver_path: path,
                node: *node,
                priority,
            });
        }
    }
    None
}

/// The streaming accumulator that selects the single root block device as
/// discovered nodes are emitted.
///
/// Feeding nodes one at a time ([`Self::observe`]) lets the production
/// boot path resolve the root device straight off the discovery sink with
/// no intermediate `Vec` of the whole tree (allocation-free on the boot path; — no fixed-size tree buffer to
/// outgrow). [`Self::finish`] audits and yields the decision.
#[derive(Default)]
pub struct RootBlockSelection {
    found: Option<RootBlockBinding>,
    ambiguous: bool,
}

impl RootBlockSelection {
    /// A selection with no block device seen yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            found: None,
            ambiguous: false,
        }
    }

    /// Fold one discovered node into the selection.
    ///
    /// The first block device seen is provisionally the root; a second
    /// *distinct* one marks the selection ambiguous (root disambiguation
    /// needs an explicit boot descriptor, not a guess). A repeated
    /// emission of the same node id is idempotent.
    pub fn observe(&mut self, node: &HwNode) {
        let Some(binding) = classify(node) else {
            return;
        };
        match self.found.as_ref() {
            None => self.found = Some(binding),
            Some(existing) if existing.node.id() == binding.node.id() => {}
            Some(_) => self.ambiguous = true,
        }
    }

    /// Audit the decision and yield the bound root block device, or [`None`]
    /// when the root is left unbound (no block device, or an ambiguous set).
    #[must_use]
    pub fn finish(self, audit: &dyn Sink) -> Option<RootBlockBinding> {
        if self.ambiguous {
            log(
                audit,
                &Event {
                    level: Level::Error,
                    id: ROOT_STORAGE_AUTOLOAD,
                    message: "root-storage autoload: multiple block devices discovered; failing \
                              closed (root needs an explicit boot descriptor)",
                    fields: &[],
                },
            );
            return None;
        }
        let Some(binding) = self.found else {
            log(
                audit,
                &Event {
                    level: Level::Info,
                    id: ROOT_STORAGE_AUTOLOAD,
                    message: "root-storage autoload: no block device discovered; root unbound",
                    fields: &[],
                },
            );
            return None;
        };
        let mut id_buf = [0u8; 16];
        let mut prio_buf = [0u8; 16];
        log(
            audit,
            &Event {
                level: Level::Info,
                id: ROOT_STORAGE_AUTOLOAD,
                message: "root-storage autoload: discovered storage node bound to block driver",
                fields: &[
                    Field {
                        key: "driver",
                        value: rustos_log::FieldValue::Str(binding.driver_path),
                    },
                    Field {
                        key: "node_id_hex",
                        value: rustos_log::FieldValue::Str(format_hex_u64(
                            u64::from(binding.node.id()),
                            &mut id_buf,
                        )),
                    },
                    Field {
                        key: "priority_hex",
                        value: rustos_log::FieldValue::Str(format_hex_u64(
                            u64::from(binding.priority),
                            &mut prio_buf,
                        )),
                    },
                ],
            },
        );
        Some(binding)
    }
}

/// Resolve the root block device from an already-collected hardware tree.
///
/// The array-slice form of the streaming [`RootBlockSelection`]: it folds
/// every node and audits the decision, returning the bound root block
/// device or [`None`] (fail closed). The production aarch64 boot path
/// drives [`RootBlockSelection`] straight off the discovery sink instead,
/// so no whole-tree buffer is allocated; this entry serves callers that
/// already hold the tree (and the tests).
#[must_use]
pub fn resolve_root_block_driver(tree: &[HwNode], audit: &dyn Sink) -> Option<RootBlockBinding> {
    let mut selection = RootBlockSelection::new();
    for node in tree {
        selection.observe(node);
    }
    selection.finish(audit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use rustos_abi::{DriverError, HwDeviceClass, HwMatchKey, HwNode};
    // `HwResource` is used only by the aarch64-only `emmc2_node` fixture
    // (the directly-described EMMC2 disk carries an MMIO grant request); on
    // every other target the floor is virtio-blk alone and nothing here
    // constructs a resource.
    #[cfg(kernel_isa = "aarch64")]
    use rustos_abi::HwResource;
    use rustos_arch_api::{DiscoveryError, HwNodeSink};
    use rustos_drv_storage_virtio_blk::VIRTIO_BLK_DEVICE_ID;
    use rustos_kernel_virtio::MAX_SLOTS;

    use crate::discovery_test_bus::FakeBus;
    use crate::driver_catalog::VIRTIO_BLK_PATH;
    // EMMC2 is a floor block driver only on aarch64 (the Pi 4 SD host), so
    // its path const and the directly-described-EMMC2 fixtures below exist
    // only there; every other target's floor is virtio-blk alone.
    #[cfg(kernel_isa = "aarch64")]
    use crate::driver_catalog::EMMC2_PATH;
    use crate::hwdiscovery::observe_virtio_mmio_block_devices;
    use crate::hwtree_node_ids::VIRTIO_BLOCK_PROBE_NODE_BASE_ID;

    /// Captures every audited event so a test can assert the bind decision
    /// was logged with the right id and level. Host
    /// tests are single-threaded, so a `RefCell` is sufficient — the gate
    /// takes a plain `&dyn Sink`, not the `Sync` boot-sink bound.
    #[derive(Default)]
    struct RecordingSink {
        events: RefCell<alloc::vec::Vec<(u32, Level)>>,
    }

    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.id.0, event.level));
        }
    }

    impl RecordingSink {
        fn only(&self) -> (u32, Level) {
            let events = self.events.borrow();
            assert_eq!(events.len(), 1, "the gate audits exactly one decision");
            events[0]
        }
    }

    /// The EMMC2 SD host the Pi 4 firmware tree describes directly: a node
    /// carrying the driver's own `compatible` bind key plus the MMIO window
    /// it exposes as a capability-grant request. EMMC2 is a floor driver
    /// only on aarch64, so this fixture is aarch64-only.
    #[cfg(kernel_isa = "aarch64")]
    fn emmc2_node(id: u32) -> HwNode {
        let mut node = HwNode::new(id, 0, HwDeviceClass::Storage);
        node.push_match_key(HwMatchKey::compatible(b"brcm,bcm2711-emmc2").expect("fits"))
            .expect("one key");
        node.push_resource(HwResource::mmio(0xfe34_0000, 0x100))
            .expect("one resource");
        node
    }

    /// A virtio-blk disk as it appears **after** the bus driver probes the
    /// transport and attaches the child: the bind key is the virtio device
    /// id (`2`), not a `compatible` string.
    fn virtio_blk_node(id: u32) -> HwNode {
        let mut node = HwNode::new(id, 0, HwDeviceClass::Storage);
        node.push_match_key(HwMatchKey::virtio(2)).expect("one key");
        node
    }

    /// The raw virtio-mmio transport node the firmware tree describes
    /// *before* enumeration: a bus node keyed only by its `compatible`
    /// string, which no floor block driver binds yet.
    fn raw_virtio_mmio_node(id: u32) -> HwNode {
        let mut node = HwNode::new(id, 0, HwDeviceClass::Bus);
        node.push_match_key(HwMatchKey::compatible(b"virtio,mmio").expect("fits"))
            .expect("one key");
        node
    }

    /// A non-storage device (a USB HID keyboard child): a node beside the
    /// disk that is not a block device, so the root selection must ignore
    /// it rather than treat it as a candidate.
    fn hid_node(id: u32) -> HwNode {
        let mut node = HwNode::new(id, 0, HwDeviceClass::Input);
        node.push_match_key(HwMatchKey::usb(0x3434, 0x0E21, 0x03_01_01))
            .expect("one key");
        node
    }

    #[cfg(kernel_isa = "aarch64")]
    #[test]
    fn a_directly_described_emmc2_node_binds_the_emmc2_driver() {
        let audit = RecordingSink::default();
        let tree = [
            HwNode::new(0, rustos_abi::HW_NODE_ROOT, HwDeviceClass::Root),
            emmc2_node(3),
        ];
        let binding = resolve_root_block_driver(&tree, &audit).expect("emmc2 binds");
        assert_eq!(binding.driver_path, EMMC2_PATH);
        assert_eq!(binding.node.id(), 3);
        // The node's MMIO grant request travels with the binding.
        assert_eq!(binding.node.resources().len(), 1);
        let (id, level) = audit.only();
        assert_eq!(id, ROOT_STORAGE_AUTOLOAD.0);
        assert_eq!(level, Level::Info);
    }

    #[test]
    fn an_enumerated_virtio_blk_node_binds_the_virtio_blk_driver() {
        let audit = RecordingSink::default();
        let tree = [virtio_blk_node(5)];
        let binding = resolve_root_block_driver(&tree, &audit).expect("virtio-blk binds");
        assert_eq!(binding.driver_path, VIRTIO_BLK_PATH);
        assert_eq!(binding.node.id(), 5);
        assert_eq!(audit.only().1, Level::Info);
    }

    #[test]
    fn an_unprobed_virtio_mmio_bus_node_does_not_bind_a_block_driver() {
        // Before the bus is enumerated the transport node carries only its
        // `compatible` string; the virtio-blk bind key (device id `2`) does
        // not match it, so the root stays unbound until the probed child is
        // attached.
        let audit = RecordingSink::default();
        let tree = [raw_virtio_mmio_node(2)];
        assert!(resolve_root_block_driver(&tree, &audit).is_none());
        let (id, level) = audit.only();
        assert_eq!(id, ROOT_STORAGE_AUTOLOAD.0);
        assert_eq!(
            level,
            Level::Info,
            "an unbound root is informational, not an error"
        );
    }

    #[test]
    fn a_tree_with_no_storage_leaves_the_root_unbound() {
        let audit = RecordingSink::default();
        let tree = [
            HwNode::new(0, rustos_abi::HW_NODE_ROOT, HwDeviceClass::Root),
            hid_node(1),
        ];
        assert!(resolve_root_block_driver(&tree, &audit).is_none());
        assert_eq!(audit.only().1, Level::Info);
    }

    #[test]
    fn an_empty_tree_leaves_the_root_unbound() {
        let audit = RecordingSink::default();
        assert!(resolve_root_block_driver(&[], &audit).is_none());
        assert_eq!(audit.only().0, ROOT_STORAGE_AUTOLOAD.0);
    }

    #[test]
    fn a_non_block_node_alongside_the_disk_is_ignored() {
        // A non-block node (a HID keyboard) beside the block disk must not
        // make the selection ambiguous: only the block device is the root.
        let audit = RecordingSink::default();
        let tree = [hid_node(1), virtio_blk_node(2)];
        let binding = resolve_root_block_driver(&tree, &audit).expect("virtio-blk binds");
        assert_eq!(binding.driver_path, VIRTIO_BLK_PATH);
    }

    #[test]
    fn two_distinct_block_devices_fail_closed_as_ambiguous() {
        // Which volume is the root is a policy decision needing a boot
        // descriptor, not a guess: the gate binds nothing and audits the
        // ambiguity as an error. Two distinct block nodes (here two
        // virtio-blk disks) are ambiguous regardless of the driver.
        let audit = RecordingSink::default();
        let tree = [virtio_blk_node(4), virtio_blk_node(7)];
        assert!(resolve_root_block_driver(&tree, &audit).is_none());
        let (id, level) = audit.only();
        assert_eq!(id, ROOT_STORAGE_AUTOLOAD.0);
        assert_eq!(level, Level::Error);
    }

    #[test]
    fn the_same_node_emitted_twice_is_not_ambiguous() {
        // Idempotent re-emission of one device must not look like two.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        let node = virtio_blk_node(9);
        selection.observe(&node);
        selection.observe(&node);
        let binding = selection.finish(&audit).expect("still bound");
        assert_eq!(binding.node.id(), 9);
        assert_eq!(audit.only().1, Level::Info);
    }

    /// Folds each probed child a [`observe_virtio_mmio_block_devices`] call
    /// emits into a [`RootBlockSelection`], so the enumeration tests assert
    /// the binding decision through the same streaming accumulator the
    /// production resolve path uses while exercising the [`HwNodeSink`]
    /// emit contract. A selection is unbounded, so emit
    /// never fails.
    struct SelectionSink<'a>(&'a mut RootBlockSelection);

    impl HwNodeSink for SelectionSink<'_> {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.0.observe(&node);
            Ok(())
        }
    }

    #[test]
    fn a_probed_virtio_block_slot_binds_the_virtio_blk_driver() {
        // A populated virtio-block slot (DeviceID 2) is attached as a probed
        // child keyed by its virtio device id, which binds virtio-blk — the
        // bootstrap-floor enumeration that lets the `virt` boot find its
        // root.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID]);
        observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection))
            .expect("enumerate");
        let binding = selection.finish(&audit).expect("virtio-blk binds");
        assert_eq!(binding.driver_path, VIRTIO_BLK_PATH);
        assert_eq!(binding.node.id(), VIRTIO_BLOCK_PROBE_NODE_BASE_ID);
        assert_eq!(audit.only().1, Level::Info);
    }

    #[test]
    fn a_non_block_virtio_slot_is_not_a_root_block_device() {
        // A virtio-net slot (DeviceID 1) is enumerated but binds no floor
        // *block* driver, so it never becomes the root;
        // an empty slot (DeviceID 0 is filtered by the bus) likewise.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        let bus = FakeBus::with(&[1]);
        observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection))
            .expect("enumerate");
        assert!(selection.finish(&audit).is_none());
        assert_eq!(audit.only().1, Level::Info);
    }

    #[test]
    fn a_block_slot_beside_a_net_slot_binds_only_the_block_disk() {
        // The block disk is selected and the network device ignored — one
        // device of each class is the common `virt` topology.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        let bus = FakeBus::with(&[1, VIRTIO_BLK_DEVICE_ID]);
        observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection))
            .expect("enumerate");
        let binding = selection.finish(&audit).expect("virtio-blk binds");
        assert_eq!(binding.driver_path, VIRTIO_BLK_PATH);
    }

    #[test]
    fn two_probed_block_slots_fail_closed_as_ambiguous() {
        // Two distinct virtio-block disks get distinct probed-child ids, so
        // the gate fails closed rather than guess which is the root.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID, VIRTIO_BLK_DEVICE_ID]);
        observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection))
            .expect("enumerate");
        assert!(selection.finish(&audit).is_none());
        assert_eq!(audit.only().1, Level::Error);
    }

    #[test]
    fn an_overfull_bus_fails_closed() {
        // More than `MAX_SLOTS` responding slots cannot be enumerated whole,
        // so the probe surfaces the error and the caller leaves the root
        // unbound rather than under-enumerating.
        let mut selection = RootBlockSelection::new();
        let devices = alloc::vec![VIRTIO_BLK_DEVICE_ID; MAX_SLOTS + 1];
        let bus = FakeBus::with(&devices);
        assert_eq!(
            observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection)),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn a_probed_block_slot_beside_a_directly_described_disk_is_ambiguous() {
        // The probed virtio-block child and a directly-described block disk
        // are two distinct block devices: ambiguous, fail closed. This also
        // proves the high probed-child id never collides with a low
        // firmware-node id in a way that hides the second device.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        selection.observe(&virtio_blk_node(3));
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID]);
        observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection))
            .expect("enumerate");
        assert!(selection.finish(&audit).is_none());
        assert_eq!(audit.only().1, Level::Error);
    }
}
