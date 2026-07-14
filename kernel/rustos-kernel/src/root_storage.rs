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

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::virtio_mmio::VirtioMmioBus;
use rustos_abi::hwtree::HwResource;
use rustos_abi::{DriverError, HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT_ID};
use rustos_arch_api::{DiscoveryError, HwNodeSink};
use rustos_devmatch::MatchResolution;
use rustos_drv_storage_virtio_blk::VIRTIO_BLK_DEVICE_ID;
use rustos_kernel_virtio::MAX_SLOTS;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;
use rustos_virtio_input::VIRTIO_INPUT_DEVICE_ID;
use rustos_virtio_net::VIRTIO_NET_DEVICE_ID;

use crate::driver_catalog;

/// First synthetic hardware-tree id for a probed virtio-MMIO child node
/// (the bind key is the virtio device id read from the
/// transport, attached as a *probed child* of the raw bus node).
///
/// [`RootBlockSelection`] uses a node id only to tell two distinct block
/// devices apart (the ambiguity check) and to dedupe a re-emitted node.
/// The discovery walk (`platform::FdtDiscovery`) numbers the raw firmware
/// nodes from `1` upward; the probed children take ids from this high base
/// so the two id spaces are obviously disjoint (a collision would be
/// harmless — only block bindings ever populate the selection's `found` —
/// but a disjoint range keeps the origin of each id unambiguous). One id
/// per enumerated block slot, so distinct disks stay distinct.
const VIRTIO_PROBE_NODE_BASE_ID: u32 = 0x8000_0000;

/// First synthetic hardware-tree id for a probed virtio-MMIO **input**
/// child node ([`observe_virtio_mmio_input_devices`]).
///
/// Kept disjoint from [`VIRTIO_PROBE_NODE_BASE_ID`] (the block-child base)
/// so a block node and an input node discovered on the same bus never
/// share an id — the two probe walks number independently, and a shared id
/// would make the leaked tree's node origins ambiguous. One id per
/// enumerated input slot, so distinct devices stay distinct.
const VIRTIO_INPUT_PROBE_NODE_BASE_ID: u32 = 0x8001_0000;

/// First synthetic hardware-tree id for a probed virtio-MMIO **network**
/// child node ([`observe_virtio_mmio_network_devices`]).
///
/// Kept disjoint from the block- (`0x8000_0000`), input- (`0x8001_0000`),
/// and boot-display ([`crate::boot_display::BOOT_DISPLAY_NODE_ID`],
/// `0x8002_0000`) bases so a block, input, network, *and* display node
/// discovered on the same bus never share an id — the probe walks number
/// independently, and a shared id would make one node silently overwrite
/// another in the leaked tree (a display world with a NIC hits exactly
/// this). One id per enumerated network slot, so distinct devices stay
/// distinct.
const VIRTIO_NET_PROBE_NODE_BASE_ID: u32 = 0x8003_0000;

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

/// Enumerate the virtio-MMIO `bus` and emit each populated **block** slot
/// into `sink` as a probed child node.
///
/// The raw `virtio,mmio` firmware node the discovery walk emits carries
/// only its `compatible` string, which no floor block driver binds — the
/// virtio-blk bind key is the device id *read from the transport*, not a
/// string (the gate's own tests prove an unprobed bus node stays
/// unbound). This is the bootstrap-floor bus enumeration that closes that
/// gap: it reads each slot's `DeviceID` register through the MMIO bus
/// driver and, for a virtio-block device ([`VIRTIO_BLK_DEVICE_ID`]),
/// synthesises the probed child node keyed by [`HwMatchKey::virtio`] — the
/// genuine probed identity (never a fabricated key),
/// exactly the node shape the gate models. The bring-up
/// ([`crate::unlock_service`]) derives the slot's register window and GIC
/// SPI from the same device tree by base, so the probed child carries only
/// its bind identity.
///
/// The probed child is **emitted into the same [`HwNodeSink`] the platform
/// discovery walk writes to**, so it becomes part of the one buffered
/// hardware tree the boot path both resolves the root
/// binding from ([`resolve_root_block_driver`]) and stashes for the unlock
/// kthread's `devmgr` autoload — a discovered node, never a side channel.
///
/// Driver-agnostic: it reaches the bus only through the frozen [`Bus`] ABI
/// seam, so the boot path never names a concrete
/// `drivers/bus/*` type. The enumeration is bounded by
/// [`MAX_SLOTS`]; an over-full bus fails closed rather than
/// under-enumerating.
///
/// # Errors
///
/// Propagates the bus enumeration error verbatim — [`DriverError::BufferTooSmall`]
/// when more than [`MAX_SLOTS`] slots respond, or a malformed-tree
/// [`DriverError::DeviceFault`]. A [`DiscoveryError::SinkFull`] from a full
/// sink is also surfaced as [`DriverError::BufferTooSmall`]. The caller
/// leaves the root unbound on any error (fail closed).
pub fn observe_virtio_mmio_block_devices(
    bus: &dyn Bus,
    sink: &mut dyn HwNodeSink,
) -> Result<(), DriverError> {
    let blank = BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    };
    let mut table = [blank; MAX_SLOTS];
    let count = bus.enumerate(&mut table)?;
    let mut next_id = VIRTIO_PROBE_NODE_BASE_ID;
    for device in &table[..count] {
        if device.device != VIRTIO_BLK_DEVICE_ID {
            continue;
        }
        // Parent the probed device to the tree root ([`HW_NODE_ROOT_ID`]),
        // not the `HW_NODE_ROOT` *parent sentinel*: a node whose parent is
        // the sentinel is the root itself and is skipped by the autoload
        // walk (`HwNode::is_root`), so a top-level discovered device must
        // name the root's id as its parent.
        let mut node = HwNode::new(next_id, HW_NODE_ROOT_ID, HwDeviceClass::Storage);
        next_id = next_id.wrapping_add(1);
        // One bind key always fits a fresh node; the capacity bound is the
        // ABI's, so a node that somehow could not hold it is dropped rather
        // than bound on a partial identity.
        if node
            .push_match_key(HwMatchKey::virtio(VIRTIO_BLK_DEVICE_ID))
            .is_ok()
        {
            // A full sink (`DiscoveryError::SinkFull`) is the only emit
            // failure a buffering sink raises; surface it as the same
            // bounded-capacity refusal an over-full bus does (fail closed).
            sink.emit(node)
                .map_err(|_: DiscoveryError| DriverError::BufferTooSmall)?;
        }
    }
    Ok(())
}

/// Enumerate the virtio-MMIO `bus` and emit each populated **virtio-input**
/// slot into `sink` as a discovered, user-space-autoloadable device node
/// carrying its register window **and** DMA constraint as capability-grant
/// requests.
///
/// This is the input-device analogue of [`observe_virtio_mmio_block_devices`],
/// and the discovery step the user-space input-driver autoload depends on:
/// a virtio keyboard/pointer is driven entirely from user space (drivers in user space), so unlike the in-kernel bootstrap-floor
/// block path (whose bring-up re-derives the slot window from the device
/// tree by base) the input node **must** carry both its MMIO window and a
/// DMA constraint as [`HwResource`]s — a user-space virtio driver maps its
/// registers and drives its split virtqueues out of driver-allocated DMA
/// memory, so a node that requested no DMA would be discovered yet fail its
/// queue bring-up closed. The privileged driver-spawn path mints exactly one
/// device-resource grant per resource the matched node requested
/// ([`crate::driver_spawn_loader`]), so the autoloaded driver is handed a
/// window grant of precisely the slot it owns plus a DMA grant for its
/// virtqueues — and nothing more (no ambient
/// authority).
///
/// Each populated slot whose `DeviceID` register equals
/// [`VIRTIO_INPUT_DEVICE_ID`] (the genuine probed identity read from the
/// transport, never a fabricated key) is emitted as an
/// [`HwDeviceClass::Input`] node keyed by [`HwMatchKey::virtio`], carrying
/// [`HwResource::mmio`] over the slot's discovered base and the extent
/// [`VirtioMmioBus::slot_window`] reports from the device tree (a discovered value, never a literal) plus a coherent
/// [`HwResource::dma`] (the QEMU `virt` virtio interconnect is cache-coherent
/// with no IOMMU, so the device addresses all of RAM — no address limit,
/// never a board constant). The node is parented to the tree root id
/// ([`HW_NODE_ROOT_ID`]), not the `HW_NODE_ROOT` parent sentinel, so the
/// autoload walk treats it as a device rather than skipping it as the root
/// ([`HwNode::is_root`]). It is emitted into the same buffered
/// hardware tree the discovery walk and the block probe write to, so the
/// unlock kthread's `devmgr` autoload sees one faithful tree.
///
/// Driver-agnostic: it reaches the bus only through the frozen
/// [`VirtioMmioBus`] / [`Bus`] ABI seams, so the boot path
/// never names a concrete `drivers/bus/*` type. The Raspberry Pi 4 firmware
/// tree describes no `virtio,mmio` node, so this is a no-op there — it is
/// the QEMU `virt`-board path, additive and metal-neutral.
///
/// A slot whose window extent cannot be resolved (a malformed `reg`), or a
/// fresh node that cannot hold its match key and both resources, is
/// **skipped** rather than emitted on a partial identity — a node the
/// kernel cannot mint a correct, bounded grant for is left undiscovered and
/// thus unbound, never half-described (fail closed).
///
/// # Errors
///
/// Propagates the bus enumeration error verbatim — [`DriverError::BufferTooSmall`]
/// when more than [`MAX_SLOTS`] slots respond, or a malformed-tree
/// [`DriverError::DeviceFault`]. A [`DiscoveryError::SinkFull`] from a full
/// sink is surfaced as [`DriverError::BufferTooSmall`]. The caller leaves
/// the affected node undiscovered on any error (fail closed).
pub fn observe_virtio_mmio_input_devices(
    bus: &dyn VirtioMmioBus,
    slot_irq: &dyn Fn(u64) -> Option<u32>,
    sink: &mut dyn HwNodeSink,
    log: &dyn Sink,
) -> Result<(), DriverError> {
    observe_virtio_mmio_interrupt_devices(
        bus,
        slot_irq,
        sink,
        log,
        VIRTIO_INPUT_DEVICE_ID,
        HwDeviceClass::Input,
        VIRTIO_INPUT_PROBE_NODE_BASE_ID,
    )
}

/// Discover every populated `virtio,mmio` slot whose `DeviceID` register
/// equals [`VIRTIO_NET_DEVICE_ID`] and emit each as a
/// [`HwDeviceClass::Network`] node keyed by [`HwMatchKey::virtio`],
/// carrying the same register-window + coherent-DMA + interrupt-line
/// grant requests as the input probe — the four things the autoloaded
/// user-space virtio-net driver process needs (`plans/NETWORK.md` N4e).
///
/// The virtio-net driver is interrupt-driven exactly like the input
/// driver — it parks its serve loop on the device interrupt rather than
/// busy-polling — so its discovery is the *same* walk with only the
/// probed device id and the emitted node class differing; both go through
/// the shared `observe_virtio_mmio_interrupt_devices` core (§2.2).
/// Node ids are drawn from a base disjoint from the block- and
/// input-probe bases so the leaked tree's node origins stay unambiguous.
///
/// # Errors
///
/// As [`observe_virtio_mmio_input_devices`]: propagates the bus
/// enumeration error and surfaces a full sink as
/// [`DriverError::BufferTooSmall`] (fail closed).
pub fn observe_virtio_mmio_network_devices(
    bus: &dyn VirtioMmioBus,
    slot_irq: &dyn Fn(u64) -> Option<u32>,
    sink: &mut dyn HwNodeSink,
    log: &dyn Sink,
) -> Result<(), DriverError> {
    observe_virtio_mmio_interrupt_devices(
        bus,
        slot_irq,
        sink,
        log,
        VIRTIO_NET_DEVICE_ID,
        HwDeviceClass::Network,
        VIRTIO_NET_PROBE_NODE_BASE_ID,
    )
}

/// The shared core of the interrupt-driven virtio-MMIO class probes
/// ([`observe_virtio_mmio_input_devices`],
/// [`observe_virtio_mmio_network_devices`]): enumerate the bus, and for
/// every populated slot whose `DeviceID` equals `device_id` emit a node of
/// `class` (numbered from `node_base_id`) carrying its register window, a
/// coherent DMA constraint, and its discovered interrupt line. Input and
/// network devices are identical here — both are autoloaded into a
/// user-space process that parks on the device interrupt — so the walk is
/// written once (§2.2); the block probe differs (a different resource
/// shape) and stays separate.
fn observe_virtio_mmio_interrupt_devices(
    bus: &dyn VirtioMmioBus,
    slot_irq: &dyn Fn(u64) -> Option<u32>,
    sink: &mut dyn HwNodeSink,
    log: &dyn Sink,
    device_id: u32,
    class: HwDeviceClass,
    node_base_id: u32,
) -> Result<(), DriverError> {
    let blank = BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    };
    let mut table = [blank; MAX_SLOTS];
    let count = bus.enumerate(&mut table)?;
    let mut next_id = node_base_id;
    for device in &table[..count] {
        // Diagnostic audit: every populated virtio-MMIO slot the walk sees,
        // with its probed `DeviceID` and register base — the discovery
        // counterpart of the block probe's bind audit, so a mis-probed or
        // unexpected device is visible in the boot log rather than silent.
        let mut want_buf = [0u8; 16];
        let mut got_buf = [0u8; 16];
        let mut addr_buf = [0u8; 16];
        log.write_event(&Event {
            level: Level::Debug,
            id: EventId(4137),
            message: "virtio-mmio slot probed",
            fields: &[
                Field {
                    key: "want",
                    value: rustos_log::FieldValue::Str(format_hex_u64(
                        u64::from(device_id),
                        &mut want_buf,
                    )),
                },
                Field {
                    key: "got",
                    value: rustos_log::FieldValue::Str(format_hex_u64(
                        u64::from(device.device),
                        &mut got_buf,
                    )),
                },
                Field {
                    key: "base",
                    value: rustos_log::FieldValue::Str(format_hex_u64(
                        device.address,
                        &mut addr_buf,
                    )),
                },
            ],
        });
        if device.device != device_id {
            continue;
        }
        // The window extent the device tree declares for this slot. A
        // malformed `reg` (or a base the bus cannot resolve) means the
        // kernel cannot size a correct grant, so skip the node rather than
        // grant a guessed window (fail closed).
        let Ok(len) = bus.slot_window(device.address) else {
            continue;
        };
        // The interrupt line the platform routes this slot to, resolved by
        // the arch-supplied `slot_irq` (the aarch64 port decodes the FDT
        // `interrupts` specifier through `gic_device_intid`; the line is a
        // *discovered* value, never a board constant). A user-space virtio-input driver is interrupt-driven: it
        // parks on `irq_wait` rather than busy-polling its event queue, so a slot whose IRQ cannot be resolved
        // is left undiscovered rather than emitted without the line its
        // driver needs (fail closed).
        let Some(intid) = slot_irq(device.address) else {
            continue;
        };
        // A top-level discovered device parents to the tree root
        // ([`HW_NODE_ROOT_ID`]), never the `HW_NODE_ROOT` parent sentinel
        // (which marks the root itself and is skipped by the autoload
        // walk, `HwNode::is_root`).
        let mut node = HwNode::new(next_id, HW_NODE_ROOT_ID, class);
        next_id = next_id.wrapping_add(1);
        // The probed bind identity, the register window, a coherent DMA
        // constraint, **and** the interrupt line — the four things the
        // autoloaded user-space driver needs and the spawn path mints one
        // grant each for. A virtio device drives its
        // split virtqueues out of driver-allocated DMA memory, so without a
        // DMA resource the autoloaded driver is granted no DMA region and its
        // queue bring-up fails closed — the input node must request DMA
        // exactly as it requests its register window. The QEMU `virt` virtio
        // interconnect is cache-coherent with no IOMMU, so the device can
        // address all of RAM: `addr_limit = 0` declares no upper bound and
        // `max_len = 0` no per-buffer cap (the driver's DMA footprint is
        // bounded by its fixed queue sizes and the kernel's deterministic
        // OOM) — a discovered property of the coherent transport, never a
        // board constant. The IRQ resource carries the discovered
        // line so the driver can `irq_bind` it (it requests `CAP_IRQ_BIND`). All four fit a fresh node by the ABI's capacity;
        // a node that somehow could not hold them is dropped rather than
        // emitted on a partial identity.
        if node.push_match_key(HwMatchKey::virtio(device_id)).is_ok()
            && node
                .push_resource(HwResource::mmio(device.address, len))
                .is_ok()
            && node.push_resource(HwResource::dma(0, 0)).is_ok()
            && node
                .push_resource(HwResource::irq(u64::from(intid), 1))
                .is_ok()
        {
            // A full sink (`DiscoveryError::SinkFull`) is the only emit
            // failure a buffering sink raises; surface it as the same
            // bounded-capacity refusal an over-full bus does (fail closed).
            sink.emit(node)
                .map_err(|_: DiscoveryError| DriverError::BufferTooSmall)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use rustos_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource};

    use crate::driver_catalog::{EMMC2_PATH, VIRTIO_BLK_PATH};

    /// A deterministic interrupt line the input-probe tests hand the
    /// `slot_irq` closure for every slot, so the emitted node carries a
    /// predictable [`HwResource::irq`] the assertions check. An arbitrary
    /// in-range GICv2 SPI; the value is the test's own and never a board
    /// constant the production path uses.
    const TEST_INPUT_INTID: u32 = 34;

    /// A discarding [`Sink`] for the discovery diagnostic in the probe tests
    /// (they assert on the emitted nodes, not the audit stream).
    struct DiscardLog;

    impl Sink for DiscardLog {
        fn write_event(&self, _event: &Event<'_>) {}
    }

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
    /// it exposes as a capability-grant request.
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

    /// A non-storage device (a USB HID keyboard child) that binds a floor
    /// driver, but not a *block* driver.
    fn hid_node(id: u32) -> HwNode {
        let mut node = HwNode::new(id, 0, HwDeviceClass::Input);
        node.push_match_key(HwMatchKey::usb(0x3434, 0x0E21, 0x03_01_01))
            .expect("one key");
        node
    }

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
    fn a_non_block_floor_node_alongside_the_disk_is_ignored() {
        // A HID node binds a floor driver too, but it is not a block driver:
        // the gate selects only the EMMC2 disk and is not made ambiguous by
        // the keyboard.
        let audit = RecordingSink::default();
        let tree = [hid_node(1), emmc2_node(2)];
        let binding = resolve_root_block_driver(&tree, &audit).expect("emmc2 binds");
        assert_eq!(binding.driver_path, EMMC2_PATH);
    }

    #[test]
    fn two_distinct_block_devices_fail_closed_as_ambiguous() {
        // Which volume is the root is a policy decision needing a boot
        // descriptor, not a guess: the gate binds nothing and audits the
        // ambiguity as an error.
        let audit = RecordingSink::default();
        let tree = [virtio_blk_node(4), emmc2_node(7)];
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
        let node = emmc2_node(9);
        selection.observe(&node);
        selection.observe(&node);
        let binding = selection.finish(&audit).expect("still bound");
        assert_eq!(binding.node.id(), 9);
        assert_eq!(audit.only().1, Level::Info);
    }

    /// A fake virtio-MMIO bus enumerating a fixed slot table, standing in
    /// for the live `drivers/bus/mmio` reader so the enumeration
    /// (`observe_virtio_mmio_block_devices`) is host-testable without MMIO
    /// (same shape as the `kernel/virtio` walk's fake).
    struct FakeBus {
        slots: alloc::vec::Vec<BusDevice>,
    }

    impl FakeBus {
        fn with(devices: &[u32]) -> Self {
            let slots = devices
                .iter()
                .enumerate()
                .map(|(i, &device)| BusDevice {
                    vendor: 0x554D_4551,
                    device,
                    class: 2,
                    reserved0: 0,
                    // A distinct, plausible per-slot base; unused by the
                    // gate (the probed child carries only its bind key).
                    address: 0x0A00_0000 + (i as u64) * 0x200,
                })
                .collect();
            Self { slots }
        }
    }

    impl Bus for FakeBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.len() < self.slots.len() {
                return Err(DriverError::BufferTooSmall);
            }
            out[..self.slots.len()].copy_from_slice(&self.slots);
            Ok(self.slots.len())
        }
    }

    /// Window extent the fake reports for every populated slot — the
    /// `virt` board's per-slot virtio-MMIO register-block size, matching
    /// the live `drivers/bus/mmio` reader.
    const FAKE_SLOT_WINDOW: u64 = 0x200;

    impl VirtioMmioBus for FakeBus {
        fn map_slot_window(
            &self,
            _base: u64,
            _mapper: &dyn rustos_abi::MmioMapper,
        ) -> Result<rustos_abi::RegisterWindow, DriverError> {
            // The input-discovery walk never maps a window (it only reads
            // the slot extent through `slot_window`), so the host tests
            // never reach this method; it exists only to satisfy the trait.
            Err(DriverError::NotFound)
        }

        fn slot_window(&self, base: u64) -> Result<u64, DriverError> {
            if self.slots.iter().any(|s| s.address == base) {
                Ok(FAKE_SLOT_WINDOW)
            } else {
                Err(DriverError::NotFound)
            }
        }
    }

    /// Collects every node a discovery probe emits, so the input-discovery
    /// tests can assert the emitted node's class, bind key, and resource
    /// directly. Unbounded, so emit never fails.
    #[derive(Default)]
    struct CollectingSink {
        nodes: alloc::vec::Vec<HwNode>,
    }

    impl HwNodeSink for CollectingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.nodes.push(node);
            Ok(())
        }
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
        assert_eq!(binding.node.id(), VIRTIO_PROBE_NODE_BASE_ID);
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
    fn a_probed_block_slot_beside_a_directly_described_emmc2_is_ambiguous() {
        // The probed virtio-block child and a directly-described EMMC2 disk
        // are two distinct block devices: ambiguous, fail closed. This also
        // proves the high probed-child id never collides with a low
        // firmware-node id in a way that hides the second device.
        let audit = RecordingSink::default();
        let mut selection = RootBlockSelection::new();
        selection.observe(&emmc2_node(3));
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID]);
        observe_virtio_mmio_block_devices(&bus, &mut SelectionSink(&mut selection))
            .expect("enumerate");
        assert!(selection.finish(&audit).is_none());
        assert_eq!(audit.only().1, Level::Error);
    }

    #[test]
    fn a_probed_virtio_input_slot_is_discovered_with_its_mmio_window() {
        // A populated virtio-input slot (DeviceID 18) is emitted as a
        // user-space-autoloadable `Input` node keyed by its probed virtio
        // device id and carrying its register window as a grant request —
        // the discovery the input-driver autoload binds against.
        let bus = FakeBus::with(&[VIRTIO_INPUT_DEVICE_ID]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_input_devices(
            &bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut sink,
            &DiscardLog,
        )
        .expect("enumerate");
        assert_eq!(sink.nodes.len(), 1);
        let node = &sink.nodes[0];
        assert_eq!(node.class(), Some(HwDeviceClass::Input));
        assert_eq!(node.id(), VIRTIO_INPUT_PROBE_NODE_BASE_ID);
        assert_eq!(
            node.match_keys(),
            &[HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID)]
        );
        // The grant requests are exactly the slot's discovered register
        // window, a coherent DMA constraint, and the discovered interrupt
        // line — the window of precisely the region it owns, the DMA region
        // its virtqueues need, and the IRQ it parks on.
        assert_eq!(
            node.resources(),
            &[
                HwResource::mmio(0x0A00_0000, 0x200),
                HwResource::dma(0, 0),
                HwResource::irq(u64::from(TEST_INPUT_INTID), 1)
            ]
        );
    }

    #[test]
    fn a_non_input_virtio_slot_emits_no_input_node() {
        // A virtio-blk slot (2) and a virtio-net slot (1) are not input
        // devices, so the input probe emits nothing.
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID, 1]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_input_devices(
            &bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut sink,
            &DiscardLog,
        )
        .expect("enumerate");
        assert!(sink.nodes.is_empty());
    }

    #[test]
    fn an_input_slot_beside_a_block_slot_emits_only_the_input_node() {
        // On a mixed bus the input probe emits exactly the input device and
        // ignores the block disk; its node id comes from the disjoint input
        // base, so it can never collide with a block probe child.
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID, VIRTIO_INPUT_DEVICE_ID]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_input_devices(
            &bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut sink,
            &DiscardLog,
        )
        .expect("enumerate");
        assert_eq!(sink.nodes.len(), 1);
        let node = &sink.nodes[0];
        assert_eq!(node.class(), Some(HwDeviceClass::Input));
        assert_eq!(node.id(), VIRTIO_INPUT_PROBE_NODE_BASE_ID);
        // The input device sits in slot 1 (base = 0x0A00_0000 + 0x200),
        // and carries its coherent DMA grant and IRQ alongside the window.
        assert_eq!(
            node.resources(),
            &[
                HwResource::mmio(0x0A00_0200, 0x200),
                HwResource::dma(0, 0),
                HwResource::irq(u64::from(TEST_INPUT_INTID), 1)
            ]
        );
    }

    #[test]
    fn two_input_slots_get_distinct_ids_and_windows() {
        // Two virtio-input devices each become a distinct node with its own
        // discovered window, so a keyboard and a pointer are never merged.
        let bus = FakeBus::with(&[VIRTIO_INPUT_DEVICE_ID, VIRTIO_INPUT_DEVICE_ID]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_input_devices(
            &bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut sink,
            &DiscardLog,
        )
        .expect("enumerate");
        assert_eq!(sink.nodes.len(), 2);
        assert_eq!(sink.nodes[0].id(), VIRTIO_INPUT_PROBE_NODE_BASE_ID);
        assert_eq!(sink.nodes[1].id(), VIRTIO_INPUT_PROBE_NODE_BASE_ID + 1);
        assert_eq!(
            sink.nodes[0].resources(),
            &[
                HwResource::mmio(0x0A00_0000, 0x200),
                HwResource::dma(0, 0),
                HwResource::irq(u64::from(TEST_INPUT_INTID), 1)
            ]
        );
        assert_eq!(
            sink.nodes[1].resources(),
            &[
                HwResource::mmio(0x0A00_0200, 0x200),
                HwResource::dma(0, 0),
                HwResource::irq(u64::from(TEST_INPUT_INTID), 1)
            ]
        );
    }

    #[test]
    fn probed_device_nodes_parent_to_the_root_id_not_the_sentinel() {
        // Regression: a probed device node must name the
        // tree root's id (`HW_NODE_ROOT_ID`) as its parent, never the
        // `HW_NODE_ROOT` *parent sentinel*. A node parented to the sentinel
        // satisfies `HwNode::is_root`, and the devmgr autoload walk skips
        // every root node — so a probed device parented to the sentinel
        // would be discovered yet never bind its driver. Guards both the
        // block and the input probe.
        let blk_bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID]);
        let mut blk = CollectingSink::default();
        observe_virtio_mmio_block_devices(&blk_bus, &mut blk).expect("enumerate");
        assert_eq!(blk.nodes.len(), 1);
        assert_eq!(blk.nodes[0].parent(), HW_NODE_ROOT_ID);
        assert!(
            !blk.nodes[0].is_root(),
            "a probed block node is a device, not the tree root"
        );

        let kbd_bus = FakeBus::with(&[VIRTIO_INPUT_DEVICE_ID]);
        let mut kbd = CollectingSink::default();
        observe_virtio_mmio_input_devices(
            &kbd_bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut kbd,
            &DiscardLog,
        )
        .expect("enumerate");
        assert_eq!(kbd.nodes.len(), 1);
        assert_eq!(kbd.nodes[0].parent(), HW_NODE_ROOT_ID);
        assert!(
            !kbd.nodes[0].is_root(),
            "a probed input node is a device, not the tree root"
        );
    }

    #[test]
    fn a_probed_virtio_net_slot_is_discovered_with_its_grants() {
        // A populated virtio-net slot (DeviceID 1) is emitted as a
        // user-space-autoloadable `Network` node keyed by its probed virtio
        // device id, carrying the same register-window + coherent-DMA + IRQ
        // grant requests as an input node (both are interrupt-driven
        // autoloaded drivers). Its node id comes from the network base,
        // disjoint from the block / input / boot-display bases.
        let bus = FakeBus::with(&[rustos_virtio_net::VIRTIO_NET_DEVICE_ID]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_network_devices(
            &bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut sink,
            &DiscardLog,
        )
        .expect("enumerate");
        assert_eq!(sink.nodes.len(), 1);
        let node = &sink.nodes[0];
        assert_eq!(node.class(), Some(HwDeviceClass::Network));
        assert_eq!(node.id(), VIRTIO_NET_PROBE_NODE_BASE_ID);
        assert_ne!(
            node.id(),
            crate::boot_display::BOOT_DISPLAY_NODE_ID,
            "the network probe base must not collide with the boot-display node id"
        );
        assert_eq!(
            node.match_keys(),
            &[HwMatchKey::virtio(rustos_virtio_net::VIRTIO_NET_DEVICE_ID)]
        );
        assert_eq!(
            node.resources(),
            &[
                HwResource::mmio(0x0A00_0000, 0x200),
                HwResource::dma(0, 0),
                HwResource::irq(u64::from(TEST_INPUT_INTID), 1)
            ]
        );
    }

    #[test]
    fn a_non_network_virtio_slot_emits_no_network_node() {
        // A virtio-blk slot (2) and a virtio-input slot (18) are not network
        // devices, so the network probe emits nothing — the exact
        // network-free case a display world without a NIC presents.
        let bus = FakeBus::with(&[VIRTIO_BLK_DEVICE_ID, VIRTIO_INPUT_DEVICE_ID]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_network_devices(
            &bus,
            &|_| Some(TEST_INPUT_INTID),
            &mut sink,
            &DiscardLog,
        )
        .expect("enumerate");
        assert!(sink.nodes.is_empty());
    }

    #[test]
    fn an_overfull_bus_fails_closed_for_input() {
        // More than `MAX_SLOTS` responding slots cannot be enumerated whole,
        // so the input probe surfaces the error and the caller leaves the
        // affected nodes undiscovered.
        let devices = alloc::vec![VIRTIO_INPUT_DEVICE_ID; MAX_SLOTS + 1];
        let bus = FakeBus::with(&devices);
        let mut sink = CollectingSink::default();
        assert_eq!(
            observe_virtio_mmio_input_devices(
                &bus,
                &|_| Some(TEST_INPUT_INTID),
                &mut sink,
                &DiscardLog
            ),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn an_input_slot_without_a_resolvable_irq_is_skipped_fail_closed() {
        // A user-space virtio-input driver is interrupt-driven, so a slot
        // whose interrupt line the arch `slot_irq` cannot resolve is left
        // undiscovered rather than emitted without the IRQ its driver parks
        // on.
        let bus = FakeBus::with(&[VIRTIO_INPUT_DEVICE_ID]);
        let mut sink = CollectingSink::default();
        observe_virtio_mmio_input_devices(&bus, &|_| None, &mut sink, &DiscardLog)
            .expect("enumerate");
        assert!(sink.nodes.is_empty());
    }
}
