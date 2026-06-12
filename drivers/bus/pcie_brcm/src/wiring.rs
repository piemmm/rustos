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
use rustos_abi::{CapabilityId, DriverError, DriverHost, MmioMapper, RegisterWindow};

use crate::{regs, BrcmPcieRc, Delay, PcieWindows};

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
