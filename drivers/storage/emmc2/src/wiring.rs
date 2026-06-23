//! Driver-host wiring: discovered EMMC2 register window → [`Emmc2`].
//!
//! The aarch64 `FdtDiscovery` emits the `brcm,bcm2711-emmc2` node into
//! `rustos_abi::hwtree` as a Storage-class device whose one resource is
//! the SDHCI register window (base and length read from the device tree's
//! `reg`, translated through ancestor-bus `ranges` — never a compiled-in
//! constant, `AGENTS.md` §18.1 / `plans/PI.md` P8). `devmgr` matches the
//! node against the driver's bind table (`brcm,bcm2711-emmc2`, §18.3) and
//! the driver host calls [`open_discovered`] with the discovered window.
//!
//! This is the only seam that maps memory: [`open_discovered`] checks
//! [`CapabilityId::MMIO_MAP`], maps the window through the host's
//! [`MmioMapper`] (never a pointer the driver synthesises, `AGENTS.md`
//! §4), and brings the card up over it. Everything below — the SDHCI
//! command/response and block-transfer state machine — is the
//! host-provable [`Emmc2`] engine driven over the register seam.

use rustos_abi::driver::mmio::MmioMapError;
use rustos_abi::{CapabilityId, DriverError, DriverHost, MmioMapper};

use crate::{regs, BringUpFault, BringUpStage, CompletionWait, Emmc2, IrqSdhci};

/// Map the discovered EMMC2 register window and bring the card online.
///
/// `regs_phys` is the ARM-physical base of the SDHCI register block as
/// reported by the hardware-tree `brcm,bcm2711-emmc2` node. The window is
/// mapped read/write under [`CapabilityId::MMIO_MAP`] and handed to
/// [`Emmc2::open`], which runs the SD identification sequence.
///
/// # Errors
///
/// Returns a [`BringUpFault`] naming the [`BringUpStage`] that failed: the
/// window map and capability checks below report [`BringUpStage::MapWindow`],
/// and the SD identification reports its own per-command stage (see
/// [`Emmc2::open`]). The underlying [`DriverError`] is:
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::MMIO_MAP`].
/// * [`DriverError::Unsupported`] if the host exposes no [`MmioMapper`].
/// * [`DriverError::LengthOutOfRange`] / [`DriverError::DeviceFault`] if
///   the platform cannot map the window, plus any [`Emmc2::open`] error
///   (an unsupported card or a controller that never responds).
///
/// Convert to a bare [`DriverError`] with `?` / `DriverError::from`.
///
/// # Capabilities
///
/// Requires [`CapabilityId::MMIO_MAP`] in addition to the load-time
/// [`CapabilityId::DRV_LOAD`] [`crate::register`] checked.
///
/// `waiter` is the completion seam the engine parks on instead of
/// busy-spinning a status register (`AGENTS.md` §17.1): the metal kernel
/// supplies one that blocks the calling task on the controller's bound GIC
/// interrupt line, while a host test supplies a no-op (`AGENTS.md` §2.2).
pub fn open_discovered<W: CompletionWait>(
    host: &dyn DriverHost,
    regs_phys: u64,
    waiter: W,
) -> Result<Emmc2<IrqSdhci<W>>, BringUpFault> {
    if !host.has_capability(CapabilityId::MMIO_MAP) {
        return Err(BringUpFault {
            stage: BringUpStage::MapWindow,
            error: DriverError::PermissionDenied,
        });
    }
    let mapper: &dyn MmioMapper = host.mmio_mapper().ok_or(BringUpFault {
        stage: BringUpStage::MapWindow,
        error: DriverError::Unsupported,
    })?;
    let window = mapper
        .map_window(regs_phys, regs::REGS_LEN_BYTES)
        .map_err(|e| BringUpFault {
            stage: BringUpStage::MapWindow,
            error: MmioMapError::as_driver_error(e),
        })?;
    Emmc2::open(IrqSdhci::new(window, waiter))
}
