//! Arch-neutral virtio-MMIO hardware-discovery observers.
//!
//! These walks probe an enumerated virtio-MMIO bus and emit each populated
//! slot into an [`HwNodeSink`] as a discovered [`tairix_abi::HwNode`] —
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

use tairix_abi::driver::bus::{Bus, BusDevice};
use tairix_abi::driver::virtio_mmio::VirtioMmioBus;
use tairix_abi::driver::virtio_pci::{
    virtio_pci_window_resource, VirtioPciBus, VIRTIO_PCI_CFG_COMMON, VIRTIO_PCI_CFG_DEVICE,
    VIRTIO_PCI_CFG_ISR, VIRTIO_PCI_CFG_NOTIFY, VIRTIO_PCI_VENDOR_ID,
};
use tairix_abi::hwtree::HwResource;
use tairix_abi::{DriverError, HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT_ID};
use tairix_arch_api::{DiscoveryError, HwNodeSink};
use tairix_drv_storage_virtio_blk::VIRTIO_BLK_DEVICE_ID;
use tairix_kernel_virtio::MAX_SLOTS;
use tairix_log::{Event, EventId, Field, Level, Sink};
use tairix_util::fmt::format_hex_u64;
use tairix_virtio_input::VIRTIO_INPUT_DEVICE_ID;
use tairix_virtio_net::VIRTIO_NET_DEVICE_ID;

use crate::hwtree_node_ids::{
    VIRTIO_BLOCK_PROBE_NODE_BASE_ID, VIRTIO_INPUT_PROBE_NODE_BASE_ID,
    VIRTIO_NET_PROBE_NODE_BASE_ID, VIRTIO_PCI_NET_PROBE_NODE_BASE_ID,
};

/// PCI device-ID base of a **modern** virtio function: the device ID is
/// `0x1040 + virtio_device_type` (virtio 1.1 §4.1.2), so a virtio-net
/// function (type [`VIRTIO_NET_DEVICE_ID`] = 1) reports `0x1041`. The PCI
/// probe translates a function's PCI device ID back to the virtio *type*
/// so it emits the *same* [`HwMatchKey::virtio`]`(type)` node the
/// MMIO probe does — one signed driver bundle binds on either bus.
const VIRTIO_PCI_MODERN_DEVICE_ID_BASE: u32 = 0x1040;

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
                    value: tairix_log::FieldValue::Str(format_hex_u64(
                        u64::from(device_id),
                        &mut want_buf,
                    )),
                },
                Field {
                    key: "got",
                    value: tairix_log::FieldValue::Str(format_hex_u64(
                        u64::from(device.device),
                        &mut got_buf,
                    )),
                },
                Field {
                    key: "base",
                    value: tairix_log::FieldValue::Str(format_hex_u64(
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

/// Enumerate the virtio-**PCI** `bus` and emit each virtio-net function as
/// a [`HwDeviceClass::Network`] node carrying the four role-tagged config
/// windows, a coherent DMA constraint, and its routed interrupt line — the
/// PCI-bus analogue of [`observe_virtio_mmio_network_devices`]
/// (`plans/NETWORK.md` N4e-x86_64).
///
/// A modern virtio-net PCI function reports vendor
/// [`VIRTIO_PCI_VENDOR_ID`] and device ID
/// `0x1040 + `[`VIRTIO_NET_DEVICE_ID`]. Unlike a single-aperture MMIO
/// device, its register blocks are scattered across BARs at
/// capability-referenced offsets that only PCI-configuration-space access
/// can resolve — which a user-space driver cannot do. So the kernel
/// resolves the four windows here (through the frozen [`VirtioPciBus`]
/// seam, never naming a concrete `drivers/bus/*` type) and emits each as a
/// role-tagged [`virtio_pci_window_resource`] grant, so the autoloaded
/// driver receives pre-resolved `(base, len)` windows it maps in its own
/// address space (`AGENTS.md` §4 — no ambient authority, the driver never
/// touches config space). The node is keyed by the shared
/// [`HwMatchKey::virtio`]`(`[`VIRTIO_NET_DEVICE_ID`]`)` — the *virtio
/// type*, not the PCI device ID — so the same signed bundle binds on the
/// MMIO and PCI buses alike (§2.2).
///
/// `dev_irq(bdf)` resolves the interrupt line the platform routes the
/// function to (arch-specific: the x86_64 port allocates a vector and
/// routes MSI-X; the line is a discovered value, never a board constant).
/// A function whose windows, notify multiplier, or interrupt line cannot
/// be resolved is left undiscovered rather than emitted on a partial
/// identity (fail closed).
///
/// # Errors
///
/// As [`observe_virtio_mmio_network_devices`]: propagates the bus
/// enumeration error and surfaces a full sink as
/// [`DriverError::BufferTooSmall`] (fail closed).
pub fn observe_virtio_pci_network_devices(
    bus: &dyn VirtioPciBus,
    dev_irq: &dyn Fn(u64) -> Option<u32>,
    sink: &mut dyn HwNodeSink,
    log: &dyn Sink,
) -> Result<(), DriverError> {
    observe_virtio_pci_devices(
        bus,
        dev_irq,
        sink,
        log,
        VIRTIO_NET_DEVICE_ID,
        HwDeviceClass::Network,
        VIRTIO_PCI_NET_PROBE_NODE_BASE_ID,
    )
}

/// Shared core of the virtio-PCI class probes: enumerate the bus, and for
/// every modern virtio function of the requested `virtio_type` resolve its
/// four config windows and emit a role-tagged node of `class` (numbered
/// from `node_base_id`). Written once so a future virtio-PCI block/input
/// probe reuses it (§2.2); the MMIO probes stay separate because their
/// window shape (a single aperture) differs.
fn observe_virtio_pci_devices(
    bus: &dyn VirtioPciBus,
    dev_irq: &dyn Fn(u64) -> Option<u32>,
    sink: &mut dyn HwNodeSink,
    log: &dyn Sink,
    virtio_type: u32,
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
    // The PCI device ID a modern virtio function of this type reports.
    let want_device_id = VIRTIO_PCI_MODERN_DEVICE_ID_BASE + virtio_type;
    let mut next_id = node_base_id;
    for device in &table[..count] {
        // Only modern virtio functions are candidates.
        if device.vendor != u32::from(VIRTIO_PCI_VENDOR_ID) {
            continue;
        }
        // Diagnostic audit: every virtio-vendor function the walk sees,
        // with its PCI device ID and bus address — the PCI counterpart of
        // the MMIO probe's slot audit, so an unexpected function is visible
        // in the boot log rather than silent.
        log_virtio_pci_function(log, want_device_id, device);
        if device.device != want_device_id {
            continue;
        }
        let bdf = device.address;
        // Resolve the four config windows to CPU-physical `(base, len)`
        // without mapping (the driver maps them in its own space), plus the
        // notification multiplier. A function whose capability layout the
        // kernel cannot resolve is skipped rather than granted a guessed
        // window (fail closed).
        let (Ok(common), Ok(notify), Ok(isr), Ok(devcfg), Ok(multiplier)) = (
            bus.virtio_window_region(bdf, VIRTIO_PCI_CFG_COMMON),
            bus.virtio_window_region(bdf, VIRTIO_PCI_CFG_NOTIFY),
            bus.virtio_window_region(bdf, VIRTIO_PCI_CFG_ISR),
            bus.virtio_window_region(bdf, VIRTIO_PCI_CFG_DEVICE),
            bus.notify_off_multiplier(bdf),
        ) else {
            continue;
        };
        // The interrupt line the platform routes this function to. A
        // virtio-net driver parks its serve loop on this line (it never
        // busy-polls), so a function whose interrupt cannot be resolved is
        // left undiscovered rather than emitted without the line its driver
        // needs (fail closed).
        let Some(line) = dev_irq(bdf) else {
            continue;
        };
        let mut node = HwNode::new(next_id, HW_NODE_ROOT_ID, class);
        next_id = next_id.wrapping_add(1);
        // The shared virtio-type bind key, the four role-tagged config
        // windows (the notify window alone carrying the multiplier), a
        // coherent DMA constraint (the modern virtio PCI transport DMAs to
        // driver-allocated memory with no IOMMU limit — a discovered
        // property, never a board constant), and the routed interrupt line
        // — six grant requests the spawn path mints one grant each for. All
        // fit a fresh node by the ABI's capacity; a node that somehow could
        // not hold them is dropped rather than emitted on a partial
        // identity.
        if node.push_match_key(HwMatchKey::virtio(virtio_type)).is_ok()
            && node
                .push_resource(virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_COMMON,
                    common.0,
                    common.1 as u64,
                    0,
                ))
                .is_ok()
            && node
                .push_resource(virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_NOTIFY,
                    notify.0,
                    notify.1 as u64,
                    multiplier,
                ))
                .is_ok()
            && node
                .push_resource(virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_ISR,
                    isr.0,
                    isr.1 as u64,
                    0,
                ))
                .is_ok()
            && node
                .push_resource(virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_DEVICE,
                    devcfg.0,
                    devcfg.1 as u64,
                    0,
                ))
                .is_ok()
            && node.push_resource(HwResource::dma(0, 0)).is_ok()
            && node
                .push_resource(HwResource::irq(u64::from(line), 1))
                .is_ok()
        {
            sink.emit(node)
                .map_err(|_: DiscoveryError| DriverError::BufferTooSmall)?;
        }
    }
    Ok(())
}

/// Emit the per-function virtio-PCI discovery diagnostic: the wanted PCI
/// device ID, the one the function reports, and its bus address — the PCI
/// counterpart of the MMIO probe's slot audit, so a mis-probed or
/// unexpected function is visible in the boot log rather than silent.
fn log_virtio_pci_function(log: &dyn Sink, want_device_id: u32, device: &BusDevice) {
    let mut want_buf = [0u8; 16];
    let mut got_buf = [0u8; 16];
    let mut addr_buf = [0u8; 16];
    log.write_event(&Event {
        level: Level::Debug,
        id: EventId(4138),
        message: "virtio-pci function probed",
        fields: &[
            Field {
                key: "want",
                value: tairix_log::FieldValue::Str(format_hex_u64(
                    u64::from(want_device_id),
                    &mut want_buf,
                )),
            },
            Field {
                key: "got",
                value: tairix_log::FieldValue::Str(format_hex_u64(
                    u64::from(device.device),
                    &mut got_buf,
                )),
            },
            Field {
                key: "bdf",
                value: tairix_log::FieldValue::Str(format_hex_u64(device.address, &mut addr_buf)),
            },
        ],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::{HwDeviceClass, HwMatchKey, HwNode, HwResource};

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
        let bus = FakeBus::with(&[tairix_virtio_net::VIRTIO_NET_DEVICE_ID]);
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
            &[HwMatchKey::virtio(tairix_virtio_net::VIRTIO_NET_DEVICE_ID)]
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

    // --- virtio-PCI probe ------------------------------------------------
    //
    // The `VirtioPciBus` trait, the `VIRTIO_PCI_CFG_*` roles, the vendor
    // id, and `virtio_pci_window_resource` are all in scope via
    // `super::*` (the module imports them for the probe itself).

    /// Deterministic interrupt line the PCI-probe tests hand `dev_irq` for
    /// every function; the value is the test's own, never a production
    /// constant.
    const TEST_PCI_INTID: u32 = 40;

    /// Notification multiplier the fake PCI bus advertises, so a test can
    /// assert it flows onto the notify window's grant.
    const TEST_NOTIFY_MULTIPLIER: u32 = 4;

    /// A fake virtio-PCI bus enumerating a fixed function list and
    /// resolving each virtio config window to a synthetic `(base, len)`
    /// keyed by `cfg_type`, so a test can assert the exact windows the
    /// probe grants without any real config-space access.
    struct FakePciBus {
        functions: alloc::vec::Vec<BusDevice>,
    }

    impl FakePciBus {
        /// A bus carrying one function per `(device_id, bdf)`, all
        /// reporting the virtio vendor id.
        fn with(functions: &[(u16, u64)]) -> Self {
            Self {
                functions: functions
                    .iter()
                    .map(|&(device, address)| BusDevice {
                        vendor: u32::from(VIRTIO_PCI_VENDOR_ID),
                        device: u32::from(device),
                        class: 0x0200,
                        reserved0: 0,
                        address,
                    })
                    .collect(),
            }
        }
    }

    impl Bus for FakePciBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.len() < self.functions.len() {
                return Err(DriverError::BufferTooSmall);
            }
            out[..self.functions.len()].copy_from_slice(&self.functions);
            Ok(self.functions.len())
        }
    }

    impl VirtioPciBus for FakePciBus {
        fn virtio_window_region(
            &self,
            bdf: u64,
            cfg_type: u8,
        ) -> Result<(u64, usize), DriverError> {
            // Base encodes the function (bdf) and structure (cfg_type) so
            // distinct windows are distinguishable in assertions.
            let len = match cfg_type {
                VIRTIO_PCI_CFG_COMMON => 0x38,
                VIRTIO_PCI_CFG_NOTIFY => 0x10,
                VIRTIO_PCI_CFG_ISR => 0x4,
                VIRTIO_PCI_CFG_DEVICE => 0x8,
                _ => return Err(DriverError::NotFound),
            };
            let base = 0xC000_0000 + (bdf << 16) + (u64::from(cfg_type) << 8);
            Ok((base, len))
        }

        fn notify_off_multiplier(&self, _bdf: u64) -> Result<u32, DriverError> {
            Ok(TEST_NOTIFY_MULTIPLIER)
        }
    }

    /// The modern virtio-net PCI device id (`0x1040 + 1`).
    const VIRTIO_NET_PCI_DEVICE_ID: u16 = 0x1041;

    #[test]
    fn a_virtio_net_pci_function_is_discovered_with_role_tagged_windows() {
        // A modern virtio-net PCI function is emitted as a `Network` node
        // keyed by the shared virtio *type* (1), carrying its four
        // role-tagged config windows (the notify window alone carrying the
        // multiplier), a coherent DMA constraint, and its routed interrupt
        // line — the exact grant set the autoloaded user-space driver's
        // `virtio_pci_windows` resolver consumes.
        let bus = FakePciBus::with(&[(VIRTIO_NET_PCI_DEVICE_ID, 0x0000_0800)]);
        let mut sink = CollectingSink::default();
        observe_virtio_pci_network_devices(&bus, &|_| Some(TEST_PCI_INTID), &mut sink, &DiscardLog)
            .expect("enumerate");
        assert_eq!(sink.nodes.len(), 1);
        let node = &sink.nodes[0];
        assert_eq!(node.class(), Some(HwDeviceClass::Network));
        assert_eq!(node.id(), VIRTIO_PCI_NET_PROBE_NODE_BASE_ID);
        // The bind key is the virtio *type*, identical to the MMIO probe's,
        // so one signed bundle binds on both buses.
        assert_eq!(
            node.match_keys(),
            &[HwMatchKey::virtio(tairix_virtio_net::VIRTIO_NET_DEVICE_ID)]
        );
        let base = 0xC000_0000 + (0x0000_0800u64 << 16);
        assert_eq!(
            node.resources(),
            &[
                virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_COMMON,
                    base + (u64::from(VIRTIO_PCI_CFG_COMMON) << 8),
                    0x38,
                    0,
                ),
                virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_NOTIFY,
                    base + (u64::from(VIRTIO_PCI_CFG_NOTIFY) << 8),
                    0x10,
                    TEST_NOTIFY_MULTIPLIER,
                ),
                virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_ISR,
                    base + (u64::from(VIRTIO_PCI_CFG_ISR) << 8),
                    0x4,
                    0,
                ),
                virtio_pci_window_resource(
                    VIRTIO_PCI_CFG_DEVICE,
                    base + (u64::from(VIRTIO_PCI_CFG_DEVICE) << 8),
                    0x8,
                    0,
                ),
                HwResource::dma(0, 0),
                HwResource::irq(u64::from(TEST_PCI_INTID), 1),
            ]
        );
        // The emitted windows round-trip through the driver-side resolver.
        let windows =
            tairix_abi::driver::virtio_pci::virtio_pci_windows(node.resources()).expect("resolve");
        assert_eq!(windows.notify_off_multiplier, TEST_NOTIFY_MULTIPLIER);
        assert_eq!(
            windows.common,
            (base + (u64::from(VIRTIO_PCI_CFG_COMMON) << 8), 0x38)
        );
    }

    #[test]
    fn a_non_net_virtio_pci_function_emits_no_network_node() {
        // A virtio-blk PCI function (0x1042) and a non-virtio device id are
        // not virtio-net, so the network probe emits nothing.
        let bus = FakePciBus::with(&[(0x1042, 0x0000_0800), (0x1050, 0x0000_0900)]);
        let mut sink = CollectingSink::default();
        observe_virtio_pci_network_devices(&bus, &|_| Some(TEST_PCI_INTID), &mut sink, &DiscardLog)
            .expect("enumerate");
        assert!(sink.nodes.is_empty());
    }

    #[test]
    fn two_virtio_net_pci_functions_get_distinct_ids() {
        // Two virtio-net functions each become a distinct node from the PCI
        // network base, so a machine with two NICs never merges them.
        let bus = FakePciBus::with(&[
            (VIRTIO_NET_PCI_DEVICE_ID, 0x0000_0800),
            (VIRTIO_NET_PCI_DEVICE_ID, 0x0000_1000),
        ]);
        let mut sink = CollectingSink::default();
        observe_virtio_pci_network_devices(&bus, &|_| Some(TEST_PCI_INTID), &mut sink, &DiscardLog)
            .expect("enumerate");
        assert_eq!(sink.nodes.len(), 2);
        assert_eq!(sink.nodes[0].id(), VIRTIO_PCI_NET_PROBE_NODE_BASE_ID);
        assert_eq!(sink.nodes[1].id(), VIRTIO_PCI_NET_PROBE_NODE_BASE_ID + 1);
    }

    #[test]
    fn a_virtio_net_pci_function_without_an_irq_is_skipped_fail_closed() {
        // The driver parks on its interrupt, so a function whose line the
        // arch resolver cannot route is left undiscovered rather than
        // emitted without it.
        let bus = FakePciBus::with(&[(VIRTIO_NET_PCI_DEVICE_ID, 0x0000_0800)]);
        let mut sink = CollectingSink::default();
        observe_virtio_pci_network_devices(&bus, &|_| None, &mut sink, &DiscardLog)
            .expect("enumerate");
        assert!(sink.nodes.is_empty());
    }
}
