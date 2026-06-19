//! Driver-host wiring: discovered BCM2711 PCIe controller → trained link.
//!
//! The aarch64 `FdtDiscovery` emits the `brcm,bcm2711-pcie` node into
//! `rustos_abi::hwtree` as a `Bus` node whose controller/ECAM-access
//! `reg` window and inbound-DMA aperture (`dma-ranges`) are
//! device-tree-discovered, never compiled-in (`AGENTS.md` §18.1 /
//! `plans/PI.md` P10). A `devmgr`/host composition maps that window and
//! calls [`open_discovered`] to reset the root complex and train its
//! link; once it returns, the same register window is handed to
//! `rustos_drv_bus_pci::mechanism_brcm` to enumerate the VL805 xHCI
//! controller behind the bridge.
//!
//! This is the only seam that maps memory: [`open_discovered`] checks
//! [`CapabilityId::MMIO_MAP`], maps the controller window through the
//! host's [`MmioMapper`] (never a pointer the driver synthesises,
//! `AGENTS.md` §4), and brings the link up over it. Everything below —
//! the reset/SerDes/window/link state machine — is the host-provable
//! [`BrcmPcieRc`] engine driven over the register seam.
//!
//! QEMU models no Pi PCIe link timing (`AGENTS.md` §0.4 / §2.1), so the
//! host tests prove the composition and its fail-closed paths up to the
//! root-port / link-up check; the live link training is the on-metal
//! acceptance item.

use rustos_abi::driver::mmio::MmioMapError;
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, HwNode, HwResourceKind, MmioMapper, RegisterWindow,
};

use crate::{regs, BrcmPcieRc, Delay, PcieWindows};

/// The discovered inputs the PCIe root-complex bring-up needs, all read
/// from the `brcm,bcm2711-pcie` [`HwNode`] (`AGENTS.md` §18.1) — never
/// compiled-in constants.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcieBringup {
    /// CPU-physical base of the PCIe controller register block (the
    /// translated `reg` MMIO window).
    pub regs_phys: u64,
    /// The inbound (`dma-ranges`) and outbound (`ranges`) address windows
    /// the root complex is programmed with.
    pub windows: PcieWindows,
}

/// Why a `brcm,bcm2711-pcie` [`HwNode`] could not be turned into a
/// [`PcieBringup`]: a required discovered resource is absent. Each is a
/// fail-closed refusal — the bring-up never invents a window
/// (`AGENTS.md` §2.9 / §18.5).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BringupError {
    /// The node carries no controller register (`Mmio`) window.
    NoControllerWindow,
    /// The node carries no inbound-DMA aperture (`Dma`) resource.
    NoInboundAperture,
    /// The node carries no outbound (`BusWindow`) resource.
    NoOutboundWindow,
}

impl BringupError {
    /// Map an incomplete-node refusal onto the bus-neutral
    /// [`DriverError`] the autonomous entry reports. A required
    /// discovered resource is missing, so the device cannot be reached:
    /// [`DriverError::NotFound`], failing closed (`AGENTS.md` §5.4).
    #[must_use]
    pub const fn as_driver_error(self) -> DriverError {
        DriverError::NotFound
    }
}

/// Assemble the bring-up inputs from a discovered `brcm,bcm2711-pcie`
/// [`HwNode`].
///
/// The node carries three resources the bring-up needs (all discovered by
/// the architecture port, `AGENTS.md` §18.1):
///
/// * the controller register window — the first [`Mmio`](HwResourceKind::Mmio)
///   resource, whose base is [`PcieBringup::regs_phys`];
/// * the inbound viewport — the [`Dma`](HwResourceKind::Dma) resource,
///   whose `length` is the viewport size and `translated_base` the
///   PCIe-space base the inbound BAR is programmed at; and
/// * the outbound window — the [`BusWindow`](HwResourceKind::BusWindow)
///   resource (`base` CPU aperture, `length` size, `translated_base` the
///   PCIe-space base it maps to).
///
/// # Errors
///
/// A [`BringupError`] naming the first missing resource; the inputs are
/// never partially assembled (`AGENTS.md` §5.4).
pub fn pcie_bringup_from_node(node: &HwNode) -> Result<PcieBringup, BringupError> {
    let resources = node.resources();
    let find = |kind| resources.iter().find(|r| r.kind() == Some(kind));

    let regs = find(HwResourceKind::Mmio).ok_or(BringupError::NoControllerWindow)?;
    let inbound = find(HwResourceKind::Dma).ok_or(BringupError::NoInboundAperture)?;
    let outbound = find(HwResourceKind::BusWindow).ok_or(BringupError::NoOutboundWindow)?;

    Ok(PcieBringup {
        regs_phys: regs.base(),
        windows: PcieWindows {
            inbound_pcie_base: inbound.translated_base(),
            inbound_size: inbound.length(),
            outbound_cpu_base: outbound.base(),
            outbound_pcie_base: outbound.translated_base(),
            outbound_size: outbound.length(),
        },
    })
}

/// Autonomous bootstrap-floor entry: bring the discovered BCM2711 PCIe
/// root complex up straight from its hardware-tree [`HwNode`].
///
/// This is the §18.6 floor-driver autonomous bring-up the kernel's
/// bootstrap-floor catalogue drives directly, talking to the kernel
/// solely through the [`DriverHost`] contract (no `kernel/*` dependency,
/// `AGENTS.md` §17.4). It reads the controller window + address windows
/// off the discovered node ([`pcie_bringup_from_node`]) — never a
/// compiled-in board constant (`AGENTS.md` §2.20 / §18.1) — then maps the
/// window and trains the link over it ([`open_discovered`]). On success
/// the returned [`BrcmPcieRc`] owns the mapped window with the link up,
/// ready for `rustos_drv_bus_pci::mechanism_brcm` to enumerate behind the
/// bridge.
///
/// # Errors
///
/// * [`DriverError::NotFound`] if the discovered node is missing a
///   required resource (via [`BringupError::as_driver_error`]).
/// * every error of [`open_discovered`] (no [`CapabilityId::MMIO_MAP`],
///   no [`MmioMapper`], a window that cannot be mapped, or a link that
///   never trains).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
/// [`CapabilityId::DRV_LOAD`] [`crate::register`] checked.
pub fn bring_up_from_node(
    host: &dyn DriverHost,
    node: &HwNode,
    delay: &dyn Delay,
) -> Result<BrcmPcieRc<RegisterWindow>, DriverError> {
    let bringup = pcie_bringup_from_node(node).map_err(BringupError::as_driver_error)?;
    open_discovered(host, bringup.regs_phys, &bringup.windows, delay)
}

/// Map the discovered PCIe controller window and train the root-complex
/// link.
///
/// `regs_phys` is the CPU-physical base of the PCIe controller register
/// block as reported by the hardware-tree `brcm,bcm2711-pcie` node;
/// `windows` carries the discovered inbound (`dma-ranges`) and outbound
/// (`ranges`) address windows the root complex is programmed with;
/// `delay` supplies the link bring-up's microsecond waits. The window is
/// mapped read/write under [`CapabilityId::MMIO_MAP`] and handed to
/// [`BrcmPcieRc::open`].
///
/// On success the returned [`BrcmPcieRc`] owns the mapped window with the
/// link up; the caller recovers the window with
/// [`BrcmPcieRc::into_regs`] and builds the windowed configuration
/// accessor (`rustos_drv_bus_pci::mechanism_brcm`) over it.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::Unsupported`] if the host exposes no [`MmioMapper`].
/// * [`DriverError::LengthOutOfRange`] / [`DriverError::DeviceFault`] if
///   the platform cannot map the window, plus any [`BrcmPcieRc::open`]
///   error (the controller is not a root port, or the link never
///   trains).
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
/// [`CapabilityId::DRV_LOAD`] [`crate::register`] checked.
pub fn open_discovered(
    host: &dyn DriverHost,
    regs_phys: u64,
    windows: &PcieWindows,
    delay: &dyn Delay,
) -> Result<BrcmPcieRc<RegisterWindow>, DriverError> {
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(DriverError::PermissionDenied);
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(DriverError::Unsupported)?;
    let window = mapper
        .map_window(regs_phys, regs::REGS_LEN_BYTES)
        .map_err(MmioMapError::as_driver_error)?;
    BrcmPcieRc::open(window, delay, windows)
}
