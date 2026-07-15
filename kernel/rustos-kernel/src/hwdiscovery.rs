//! Arch-neutral virtio-MMIO hardware-discovery observers.
//!
//! These walks probe an enumerated virtio-MMIO bus and emit each populated
//! slot into an [`HwNodeSink`] as a discovered [`rustos_abi::HwNode`] —
//! block disks, and the interrupt-driven input/network devices a
//! user-space driver autoloads against. They are **pure discovery**: they
//! reach the bus only through the frozen [`Bus`] / [`VirtioMmioBus`] ABI
//! seams, name no concrete `drivers/bus/*` type, and never read, mount, or
//! bind a driver.
//!
//! They live here, apart from the root-block *catalogue resolution*
//! ([`crate::root_storage`], which links the in-kernel `driver_catalog` /
//! `drvhost`), so that discovering hardware never drags the driver-signing
//! trust anchor in with it: an architecture whose boot path builds a
//! hardware tree (over its own FDT/ACPI source) reuses these observers
//! without linking the catalogue. Input and network devices are
//! discovered by one shared core (`observe_virtio_mmio_interrupt_devices`,
//! §2.2); the block probe differs (a different resource shape) and stays
//! separate.

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::virtio_mmio::VirtioMmioBus;
use rustos_abi::hwtree::HwResource;
use rustos_abi::{DriverError, HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT_ID};
use rustos_arch_api::{DiscoveryError, HwNodeSink};
use rustos_drv_storage_virtio_blk::VIRTIO_BLK_DEVICE_ID;
use rustos_kernel_virtio::MAX_SLOTS;
use rustos_log::{Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;
use rustos_virtio_input::VIRTIO_INPUT_DEVICE_ID;
use rustos_virtio_net::VIRTIO_NET_DEVICE_ID;

use crate::hwtree_node_ids::{
    VIRTIO_BLOCK_PROBE_NODE_BASE_ID, VIRTIO_INPUT_PROBE_NODE_BASE_ID, VIRTIO_NET_PROBE_NODE_BASE_ID,
};

/// Enumerate the virtio-MMIO `bus` and emit each populated **block** slot
/// into `sink` as a probed child node.
///
/// The raw `virtio,mmio` firmware node the discovery walk emits carries
/// only its `compatible` string, which no floor block driver binds — the
/// virtio-blk bind key is the device id *read from the transport*, not a
/// string. This is the bootstrap-floor bus enumeration that closes that
/// gap: it reads each slot's `DeviceID` register through the MMIO bus
/// driver and, for a virtio-block device ([`VIRTIO_BLK_DEVICE_ID`]),
/// synthesises the probed child node keyed by [`HwMatchKey::virtio`] — the
/// genuine probed identity (never a fabricated key), exactly the node
/// shape the root-storage gate models. The bring-up
/// ([`crate::unlock_service`]) derives the slot's register window and GIC
/// SPI from the same device tree by base, so the probed child carries only
/// its bind identity.
///
/// The probed child is **emitted into the same [`HwNodeSink`] the platform
/// discovery walk writes to**, so it becomes part of the one buffered
/// hardware tree the boot path both resolves the root binding from
/// ([`crate::root_storage::resolve_root_block_driver`]) and stashes for the
/// unlock kthread's `devmgr` autoload — a discovered node, never a side
/// channel.
///
/// Driver-agnostic: it reaches the bus only through the frozen [`Bus`] ABI
/// seam, so the boot path never names a concrete `drivers/bus/*` type. The
/// enumeration is bounded by [`MAX_SLOTS`]; an over-full bus fails closed
/// rather than under-enumerating.
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
    let mut next_id = VIRTIO_BLOCK_PROBE_NODE_BASE_ID;
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
/// a virtio keyboard/pointer is driven entirely from user space, so unlike
/// the in-kernel bootstrap-floor block path (whose bring-up re-derives the
/// slot window from the device tree by base) the input node **must** carry
/// both its MMIO window and a DMA constraint as [`HwResource`]s — a
/// user-space virtio driver maps its registers and drives its split
/// virtqueues out of driver-allocated DMA memory, so a node that requested
/// no DMA would be discovered yet fail its queue bring-up closed. The
/// privileged driver-spawn path mints exactly one device-resource grant per
/// resource the matched node requested ([`crate::driver_spawn_loader`]), so
/// the autoloaded driver is handed a window grant of precisely the slot it
/// owns plus a DMA grant for its virtqueues — and nothing more (no ambient
/// authority).
///
/// Each populated slot whose `DeviceID` register equals
/// [`VIRTIO_INPUT_DEVICE_ID`] (the genuine probed identity read from the
/// transport, never a fabricated key) is emitted as an
/// [`HwDeviceClass::Input`] node keyed by [`HwMatchKey::virtio`], carrying
/// [`HwResource::mmio`] over the slot's discovered base and the extent
/// [`VirtioMmioBus::slot_window`] reports from the device tree (a discovered
/// value, never a literal) plus a coherent [`HwResource::dma`] (the QEMU
/// `virt` virtio interconnect is cache-coherent with no IOMMU, so the device
/// addresses all of RAM — no address limit, never a board constant). The
/// node is parented to the tree root id ([`HW_NODE_ROOT_ID`]), not the
/// `HW_NODE_ROOT` parent sentinel, so the autoload walk treats it as a
/// device rather than skipping it as the root ([`HwNode::is_root`]). It is
/// emitted into the same buffered hardware tree the discovery walk and the
/// block probe write to, so the unlock kthread's `devmgr` autoload sees one
/// faithful tree.
///
/// Driver-agnostic: it reaches the bus only through the frozen
/// [`VirtioMmioBus`] / [`Bus`] ABI seams, so the boot path never names a
/// concrete `drivers/bus/*` type. The Raspberry Pi 4 firmware tree describes
/// no `virtio,mmio` node, so this is a no-op there — it is the QEMU
/// `virt`-board path, additive and metal-neutral.
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
        // *discovered* value, never a board constant). A user-space
        // virtio-input driver is interrupt-driven: it parks on `irq_wait`
        // rather than busy-polling its event queue, so a slot whose IRQ
        // cannot be resolved is left undiscovered rather than emitted
        // without the line its driver needs (fail closed).
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
        // grant each for. A virtio device drives its split virtqueues out of
        // driver-allocated DMA memory, so without a DMA resource the
        // autoloaded driver is granted no DMA region and its queue bring-up
        // fails closed — the input node must request DMA exactly as it
        // requests its register window. The QEMU `virt` virtio interconnect
        // is cache-coherent with no IOMMU, so the device can address all of
        // RAM: `addr_limit = 0` declares no upper bound and `max_len = 0` no
        // per-buffer cap (the driver's DMA footprint is bounded by its fixed
        // queue sizes and the kernel's deterministic OOM) — a discovered
        // property of the coherent transport, never a board constant. The
        // IRQ resource carries the discovered line so the driver can
        // `irq_bind` it (it requests `CAP_IRQ_BIND`). All four fit a fresh
        // node by the ABI's capacity; a node that somehow could not hold
        // them is dropped rather than emitted on a partial identity.
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
    use rustos_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource};

    use crate::discovery_test_bus::FakeBus;

    /// A deterministic interrupt line the interrupt-probe tests hand the
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

    /// Collects every node a discovery probe emits, so the tests can assert
    /// the emitted node's class, bind key, and resource directly. Unbounded,
    /// so emit never fails.
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
