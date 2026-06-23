//! Driver-host wiring: discovered VL805 `PCIe` xHCI → [`UsbDevice`].
//!
//! This is the `plans/PI.md` P10 metal-wiring seam. On the Raspberry
//! Pi 4 (BCM2711) the USB-A ports hang off a VL805 `PCIe` xHCI host
//! controller behind the `SoC`'s `PCIe` root complex. The aarch64
//! `FdtDiscovery` emits the `brcm,bcm2711-pcie` bridge into
//! `rustos_abi::hwtree` (a `Bus` node whose ECAM-access window and
//! inbound-DMA aperture are device-tree-discovered, never compiled-in,
//! `AGENTS.md` §18.1); a `devmgr`/host composition maps that window,
//! constructs the bus driver over it (`rustos_pci::mechanism_ecam`),
//! and hands the resulting [`PciBus`] to [`open_discovered`].
//!
//! [`open_discovered`] enumerates the bus for the USB-class function,
//! enables bus mastering on it, maps its register BAR under
//! [`CapabilityId::MMIO_MAP`], carves a DMA region under the host's
//! DMA facility bounded by the discovered inbound-DMA aperture, and
//! brings the controller up through [`Xhci::open`] + [`UsbDevice::start`].
//! The PCI walk lives in `lib/pci` and the controller protocol
//! in this crate; the wiring composes them through the `lib/abi`
//! [`PciBus`] seam so neither driver crate names the other
//! (`AGENTS.md` §8 / §17.4).
//!
//! No QEMU vertical exists — QEMU models no Pi USB timing (`AGENTS.md`
//! §0.4 / §2.1) — so the host tests prove the composition and its
//! fail-closed paths up to the controller hand-off; the live
//! controller bring-up is the on-metal acceptance item.

use rustos_abi::driver::dma::{DmaHost, DmaSlab};
use rustos_abi::{
    CapabilityId, Delay, DriverError, DriverHost, HwNode, HwResource, MmioMapper, PciBus,
    RegisterWindow,
};

use rustos_pci::{assign_and_map_bar, find_function_by_class, USB_CONTROLLER_CLASS};
use rustos_usb::device::UsbDevice;
use rustos_usb::{Xhci, DEFAULT_POLL_BUDGET, XHCI_BAR_INDEX};

/// Bytes carved for the controller's device-shared DMA structures.
///
/// Re-exported from `lib/usb` ([`rustos_usb::XHCI_DMA_BYTES`]), the single
/// definition shared with the arch-neutral keyboard driver that also
/// carves a controller's DMA region (`AGENTS.md` §2.2).
pub use rustos_usb::XHCI_DMA_BYTES;

/// Bring the discovered xHCI controller online from `bus`.
///
/// `bus` is the PCI bus driver built over the discovered ECAM-access
/// window (`rustos_pci::mechanism_ecam`). `dma_aperture_top` is
/// the *exclusive* upper bound, in the **device-visible** (PCIe-space)
/// address space, of the inbound window the bridge lets devices behind
/// it reach (`inbound_pcie_base + inbound_size`, derived from the
/// `dma-ranges` aperture the hardware tree discovered, `AGENTS.md`
/// §18.1): the carved region's device-visible address
/// ([`DmaSlab::phys`](rustos_abi::driver::dma::DmaSlab::phys), the
/// address the controller's DMA descriptors carry) must lie entirely
/// below it or the controller could not reach its own rings. It is
/// **not** the CPU-physical aperture top — on the Pi 4 the inbound
/// viewport lifts the device address far above the CPU window
/// (`AGENTS.md` §5.4 — the bound must match the address space it guards).
///
/// `outbound_window` is the host bridge's outbound (CPU→PCIe) window
/// as a `(pcie_base, size)` pair, in the **PCIe-bus** address space the
/// downstream function's BARs decode (`AGENTS.md` §18.1). Firmware
/// normally assigns the controller's BAR, but when the OS resets and
/// re-enumerates the root complex the VL805's BAR0 address bits read
/// zero (unassigned); [`PciBus::assign_bar`] places it at a
/// size-aligned address inside this window before the BAR is mapped, so
/// the bridge can translate it to CPU-physical. An already-assigned BAR
/// is left untouched.
///
/// On success the returned [`UsbDevice`] owns the mapped register
/// window and DMA region with the controller halted, reset, and
/// running; the caller scans the root-hub ports and calls
/// [`UsbDevice::enumerate_hid`] for a connected device.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::Unsupported`] if the host exposes no
///   [`MmioMapper`] or no DMA facility.
/// * [`DriverError::NotFound`] if the bus carries no USB-class
///   function.
/// * [`DriverError::OutOfRange`] if the carved DMA region does not lie
///   below `dma_aperture_top`, or the BAR cannot be assigned inside
///   `outbound_window` (fail closed, `AGENTS.md` §5.4), plus any error
///   of [`PciBus::assign_bar`], [`PciBus::map_bar_window`], the DMA
///   allocation, [`Xhci::open`], or [`UsbDevice::start`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (to map the register BAR) in
/// addition to the load-time [`CapabilityId::DRV_LOAD`]
/// [`crate::register`] checked; the DMA carve is gated on the host's
/// own DMA capability (`CAP_MEM_DMA`) at allocation time.
pub fn open_discovered(
    host: &dyn DriverHost,
    bus: &dyn PciBus,
    dma_aperture_top: u64,
    outbound_window: (u64, u64),
) -> Result<UsbDevice<RegisterWindow, DmaSlab>, DriverError> {
    let mapped = map_controller(host, bus, dma_aperture_top, outbound_window)?;
    let xhci = Xhci::open(mapped.window)?;
    UsbDevice::start(xhci, mapped.dma, DEFAULT_POLL_BUDGET)
}

/// The outcome of a successful [`bring_up_boot_input`]: the enumerated
/// controller and the discovered child node published into the tree.
///
/// The [`UsbDevice`] is left pointed at the enumerated device's slot — it
/// is a [`rustos_abi::driver::input::ReportSource`] the composing host
/// drains (in the in-kernel transition, the report-pump kthread; after the
/// `plans/PI.md` B5 flip, the user-space `usb_kbd` driver that binds the
/// emitted `node`). The `node` is the same value already handed to
/// [`DriverHost::emit_node`], returned so the host can re-match it against
/// the driver catalogue and admit the matched HID driver through the
/// signed load gate before any input flows (`plans/PI.md` P10 5c-ii).
pub struct EnumeratedBootInput {
    /// The started controller, pointed at the enumerated device's slot.
    pub device: UsbDevice<RegisterWindow, DmaSlab>,
    /// The enumerated device as a discovered child [`HwNode`], carrying
    /// its register-window and DMA `HwResource` grant requests.
    pub node: HwNode,
}

/// Autonomous bootstrap-floor entry: bring the discovered xHCI controller
/// online, enumerate the connected boot input device, and publish it into
/// the hardware tree as a bindable child [`HwNode`] (`AGENTS.md` §18.6 /
/// `plans/PI.md` Increment C / driver-traits "Autonomous floor bring-up
/// entry").
///
/// `register` is *reactive* (`AGENTS.md` §8): the host instantiates the
/// driver against an already-discovered node. This entry is the
/// *proactive* floor counterpart — the kernel's bootstrap-floor catalogue
/// drives it before any node for the device behind the controller exists,
/// so it must itself enumerate that device and emit it. It runs the full
/// chain over the [`DriverHost`] contract alone (no `kernel/*` dependency,
/// `AGENTS.md` §17.4): [`map_controller`] (mapping the BAR through
/// [`DriverHost::mmio_mapper`] and carving DMA through
/// [`DriverHost::dma_host`]), [`Xhci::open`] + [`UsbDevice::start`],
/// [`UsbDevice::enumerate_boot_keyboard`] (which transparently descends one
/// tier through an onboard hub, `AGENTS.md` §2.2), and finally
/// [`DriverHost::emit_node`] of the enumerated device.
///
/// This crate stays board-neutral (`AGENTS.md` §2.20): it never names the
/// VL805/BCM2711. The PCIe link training and the device-specific firmware
/// reload are the PCIe driver's job (`drivers/bus/pcie_brcm`); the caller
/// hands this entry an already-trained [`PciBus`] and the discovered
/// `dma_aperture_top` / `outbound_window` (see [`open_discovered`]).
///
/// # Emitted grants — no ambient authority
///
/// The emitted `node` carries exactly the two [`HwResource`] grant
/// *requests* the matched downstream driver needs and no more
/// (`AGENTS.md` §4 / §18.3):
///
/// * an [`HwResource::mmio`] of the controller's **already-assigned** xHCI
///   register BAR (the window [`map_controller`] mapped — its CPU-physical
///   base and length), so the matched user-space driver re-maps the live
///   BAR rather than re-training the bus; and
/// * an [`HwResource::dma`] declaring the device-visible reachability bound
///   (`dma_aperture_top`) and the [`XHCI_DMA_BYTES`] region size the
///   matched driver carves for its own rings.
///
/// `parent_id`/`node_id` place the child under the controller's discovered
/// hardware-tree node; the node↔driver bind resolves on the node's match
/// keys, not its ids (`AGENTS.md` §18.3).
///
/// # Errors
///
/// Every failure is fail-closed (`AGENTS.md` §5.4); nothing is left
/// half-configured or half-published:
///
/// * any error of [`map_controller`] (capability/facility checks, the DMA
///   carve and its aperture bound, BAR assignment and mapping),
/// * any error of [`Xhci::open`] / [`UsbDevice::start`] (the controller
///   does not decode or never runs),
/// * [`DriverError::NotFound`] from [`UsbDevice::enumerate_boot_keyboard`]
///   for an empty hub, or [`UsbDevice::describe_device`] if no HID
///   interface was enumerated,
/// * [`DriverError::NoSpace`] if the node cannot carry its grant requests,
///   and
/// * any error of [`DriverHost::emit_node`] (the host fails the tree
///   mutation closed).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] (the BAR) and the host's DMA
/// capability (the carve), both re-checked host-side at each
/// map/allocation; emitting the node is gated by the host's own tree
/// mutation check (`AGENTS.md` §5.4).
pub fn bring_up_boot_input(
    host: &dyn DriverHost,
    bus: &dyn PciBus,
    dma_aperture_top: u64,
    outbound_window: (u64, u64),
    delay: &dyn Delay,
    parent_id: u32,
    node_id: u32,
) -> Result<EnumeratedBootInput, DriverError> {
    let mapped = map_controller(host, bus, dma_aperture_top, outbound_window)?;
    // Capture the controller's BAR window span before the window is moved
    // into the engine: it becomes the matched downstream driver's register
    // grant (the live, already-assigned BAR, not a fresh assignment).
    let bar_base = mapped.window.phys_base();
    let bar_len = mapped.window.len() as u64;
    let xhci = Xhci::open(mapped.window)?;
    let mut device = UsbDevice::start(xhci, mapped.dma, DEFAULT_POLL_BUDGET)?;
    device.enumerate_boot_keyboard(delay)?;
    let mut node = device.describe_device(parent_id, node_id)?;
    node.push_resource(HwResource::mmio(bar_base, bar_len))
        .map_err(|_| DriverError::NoSpace)?;
    node.push_resource(HwResource::dma(dma_aperture_top, XHCI_DMA_BYTES as u64))
        .map_err(|_| DriverError::NoSpace)?;
    host.emit_node(node)?;
    Ok(EnumeratedBootInput { device, node })
}

/// The discovered xHCI controller's mapped register BAR and its carved,
/// in-aperture device-shared DMA region — the inputs [`Xhci::open`] and
/// [`UsbDevice::start`] consume.
///
/// Produced by [`map_controller`] so a composing host (the in-kernel
/// keyboard service) can read the controller's capability block and log
/// its geometry between the map and the bring-up without re-mapping the
/// BAR (`AGENTS.md` §2.2 — one window per device).
pub struct MappedXhci {
    /// The controller's mapped register BAR window.
    pub window: RegisterWindow,
    /// The carved, zeroed, in-aperture device-shared DMA region.
    pub dma: DmaSlab,
}

/// Locate the VL805 on `bus`, carve its DMA region, assign and map its
/// register BAR, and enable bus mastering — every step up to (but not
/// including) the controller bring-up.
///
/// This is the prefix [`open_discovered`] runs before [`Xhci::open`].
/// It is split out so the caller can inspect the result (read the
/// capability block, log the carve and geometry) before handing it to
/// [`Xhci::open`] + [`UsbDevice::start`]; the bring-up is a thin
/// composition over it ([`open_discovered`]).
///
/// See [`open_discovered`] for the meaning of `dma_aperture_top` and
/// `outbound_window`.
///
/// # Errors
///
/// As [`open_discovered`], for every step it performs (capability and
/// facility checks, controller discovery, the DMA carve and its
/// aperture bound, BAR assignment, and the BAR map).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`]; the DMA carve is gated on the
/// host's own DMA capability at allocation time.
pub fn map_controller(
    host: &dyn DriverHost,
    bus: &dyn PciBus,
    dma_aperture_top: u64,
    outbound_window: (u64, u64),
) -> Result<MappedXhci, DriverError> {
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(DriverError::PermissionDenied);
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
    // Carve DMA through the bus-neutral `dma_host()` facility, not the
    // virtio-shaped `virtio_host()`: an xHCI host controller is not a
    // virtio device, so it allocates its device-shared rings through the
    // generic allocation contract (`AGENTS.md` §2.2 — the C-1 split that
    // gave a non-virtio driver a DMA seam of its own).
    let dma_host: &dyn DmaHost = host.dma_host().ok_or(DriverError::Unsupported)?;

    // Locate the single USB-class function behind the bridge through the
    // shared bus-driver scan (`AGENTS.md` §2.2 — one definition in
    // `lib/pci`, reused by the root-complex bus driver too).
    let bdf = find_function_by_class(bus, USB_CONTROLLER_CLASS)?;

    // Carve the device-shared DMA region first and verify it lies
    // wholly below the discovered inbound-DMA aperture before any
    // hardware is touched: a region the controller cannot reach is a
    // fail-closed refusal, never a silent truncation (`AGENTS.md`
    // §5.4). The slab is dropped (reclaimed) on the early return.
    let dma = dma_host.alloc_dma_zeroed(XHCI_DMA_BYTES)?;
    let end = dma
        .phys()
        .checked_add(dma.len() as u64)
        .ok_or(DriverError::OutOfRange)?;
    if end > dma_aperture_top {
        return Err(DriverError::OutOfRange);
    }

    // Assign (when firmware left it unassigned), enable bus-mastering on,
    // and map the controller's register BAR. Firmware normally assigns
    // the BAR, but after the OS resets and re-enumerates the root complex
    // the VL805's BAR0 reads unassigned (address bits zero); the shared
    // primitive places it inside the bridge's outbound PCIe window first
    // (a no-op if firmware already based it) and sets the Bus Master
    // Enable bit the controller's upstream DMA needs, so the map resolves
    // to a real CPU address and the controller can reach its rings
    // (`AGENTS.md` §5.4 / §2.2 — one definition in `lib/pci`).
    let window = assign_and_map_bar(bus, bdf, XHCI_BAR_INDEX, outbound_window, mapper)?;

    Ok(MappedXhci { window, dma })
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
