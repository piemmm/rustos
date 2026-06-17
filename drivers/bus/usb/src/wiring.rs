//! Driver-host wiring: discovered VL805 `PCIe` xHCI → [`UsbDevice`].
//!
//! This is the `plans/PI.md` P10 metal-wiring seam. On the Raspberry
//! Pi 4 (BCM2711) the USB-A ports hang off a VL805 `PCIe` xHCI host
//! controller behind the `SoC`'s `PCIe` root complex. The aarch64
//! `FdtDiscovery` emits the `brcm,bcm2711-pcie` bridge into
//! `rustos_abi::hwtree` (a `Bus` node whose ECAM-access window and
//! inbound-DMA aperture are device-tree-discovered, never compiled-in,
//! `AGENTS.md` §18.1); a `devmgr`/host composition maps that window,
//! constructs the bus driver over it (`rustos_drv_bus_pci::mechanism_ecam`),
//! and hands the resulting [`PciBus`] to [`open_discovered`].
//!
//! [`open_discovered`] enumerates the bus for the USB-class function,
//! enables bus mastering on it, maps its register BAR under
//! [`CapabilityId::MMIO_MAP`], carves a DMA region under the host's
//! DMA facility bounded by the discovered inbound-DMA aperture, and
//! brings the controller up through [`Xhci::open`] + [`UsbDevice::start`].
//! The PCI walk lives in `drivers/bus/pci` and the controller protocol
//! in this crate; the wiring composes them through the `lib/abi`
//! [`PciBus`] seam so neither driver crate names the other
//! (`AGENTS.md` §8 / §17.4).
//!
//! No QEMU vertical exists — QEMU models no Pi USB timing (`AGENTS.md`
//! §0.4 / §2.1) — so the host tests prove the composition and its
//! fail-closed paths up to the controller hand-off; the live
//! controller bring-up is the on-metal acceptance item.

use rustos_abi::driver::bus::BusDevice;
use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::{CapabilityId, DriverError, DriverHost, MmioMapper, PciBus, RegisterWindow};

use rustos_usb::device::UsbDevice;
use rustos_usb::{Xhci, DEFAULT_POLL_BUDGET};

/// PCI base-class + sub-class identifying a USB host controller
/// (PCI Local Bus 3.0 Appendix D: base `0x0C` Serial Bus Controller,
/// sub-class `0x03` USB). The VL805 exposes its xHCI as the single USB
/// function behind the Pi 4's `PCIe` bridge.
pub const USB_CONTROLLER_CLASS: u16 = 0x0C03;

/// BAR slot carrying the xHCI register block (xHCI 1.2 §5.2.1: the
/// memory BAR at offset `0x10`, i.e. BAR0).
pub const XHCI_BAR_INDEX: u8 = 0;

/// Bytes carved for the controller's device-shared DMA structures.
///
/// Re-exported from `lib/usb` ([`rustos_usb::XHCI_DMA_BYTES`]), the single
/// definition shared with the arch-neutral keyboard driver that also
/// carves a controller's DMA region (`AGENTS.md` §2.2).
pub use rustos_usb::XHCI_DMA_BYTES;

/// Upper bound on functions scanned while locating the USB controller.
///
/// A defence bound (`AGENTS.md` §24.4), not a capacity: the VL805 sits
/// alone behind the Pi 4 bridge, so the controller is found in the
/// first handful of entries; the cap stops a malfunctioning bus from
/// driving an unbounded scan.
const MAX_ENUMERATION: usize = 32;

/// Bring the discovered xHCI controller online from `bus`.
///
/// `bus` is the PCI bus driver built over the discovered ECAM-access
/// window (`rustos_drv_bus_pci::mechanism_ecam`). `dma_aperture_top` is
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
    let dma_host = host.virtio_host().ok_or(DriverError::Unsupported)?;

    let bdf = find_usb_controller(bus)?;

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

    // Firmware normally assigns the controller's BAR, but after the OS
    // resets and re-enumerates the root complex the VL805's BAR0 reads
    // unassigned (address bits zero); mapping it would target physical
    // address 0 and be refused. Place it inside the bridge's outbound
    // PCIe window first (a no-op if firmware already based it), so the
    // map resolves to a real CPU address (`AGENTS.md` §5.4).
    let (outbound_base, outbound_size) = outbound_window;
    bus.assign_bar(bdf, XHCI_BAR_INDEX, outbound_base, outbound_size)?;

    // The controller issues upstream DMA into the region above, so its
    // Bus Master Enable bit must be set before it runs (firmware leaves
    // it clear, `AGENTS.md` — PCI Local Bus 3.0 §6.2.2).
    bus.enable_bus_master(bdf)?;
    let window = bus.map_bar_window(bdf, XHCI_BAR_INDEX, mapper)?;

    Ok(MappedXhci { window, dma })
}

/// Locate the bus-local address of the first USB-class function on
/// `bus`.
///
/// Enumerates into a bounded buffer and matches
/// [`USB_CONTROLLER_CLASS`]; a [`DriverError::BufferTooSmall`] from the
/// bus still fills the buffer, so the populated entries are searched
/// either way.
fn find_usb_controller(bus: &dyn PciBus) -> Result<u64, DriverError> {
    let mut devices = [BusDevice {
        vendor: 0,
        device: 0,
        class: 0,
        reserved0: 0,
        address: 0,
    }; MAX_ENUMERATION];
    let found = match bus.enumerate(&mut devices) {
        Ok(n) => n,
        // The bus filled the buffer before reporting the overflow; the
        // controller is in the first handful of functions on the Pi 4,
        // so the populated prefix is searched rather than failing the
        // whole bring-up on an oversized bus.
        Err(DriverError::BufferTooSmall) => devices.len(),
        Err(other) => return Err(other),
    };
    devices[..found]
        .iter()
        .find(|d| d.class == USB_CONTROLLER_CLASS)
        .map(|d| d.address)
        .ok_or(DriverError::NotFound)
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod tests;
