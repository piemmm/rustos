//! RustOS Broadcom BCM2711 (Raspberry Pi 4) PCIe root-complex bring-up.
//!
//! On the Raspberry Pi 4 the USB-A ports hang off a VL805 xHCI host
//! controller behind the BCM2711's PCIe root complex. That root complex
//! ships out of reset with its link **down**: nothing can reach the
//! VL805's configuration space until the root complex is reset, its
//! SerDes powered, its inbound/outbound address windows programmed, and
//! its link trained (`plans/PI.md` P10). This driver performs that
//! bring-up over the BCM2711 root-complex registers.
//!
//! # Layered seams
//!
//! The bring-up state machine ([`BrcmPcieRc`]) is written against two
//! seams so it is proven host-side (`AGENTS.md` §2.2):
//!
//! * [`PcieRegs`] — controller register access. Metal drives it over a
//!   capability-gated [`RegisterWindow`] (implemented below); host tests
//!   drive it over a register-level mock that can model link-up after a
//!   bounded number of status polls.
//! * [`Delay`] — microsecond busy-delay. The link bring-up has hard
//!   timing requirements (SerDes stabilisation, the post-`PERST#`
//!   settle, the 100 ms link-training window) that no register poll can
//!   substitute for. On metal the kernel supplies a generic-timer-backed
//!   delay; host tests pass a no-op.
//!
//! Once the link is up the same register window is handed to
//! `rustos_drv_bus_pci::mechanism_brcm` to reach downstream configuration
//! space; this crate never enumerates and so never depends on the PCI
//! core (`AGENTS.md` §17.4).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`BrcmPcieRc`] is a public *type* the driver host instantiates through
//! [`wiring::open_discovered`]; the host never reaches into it beyond
//! recovering the brought-up register window.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; mapping the discovered
//! register window additionally requires [`CapabilityId::MMIO_MAP`]
//! (checked in [`wiring`]). The driver runs in user space and does not
//! request `CAP_DRV_KERNEL` (`AGENTS.md` §4 / §8).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::mmio::WindowError;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, RegisterWindow};

pub mod regs;
pub mod wiring;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"PCB"` (PCIe Brcm) with a version nibble, matching
/// the other drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5043_4200_0000_0001;

/// Default upper bound on 5 ms link-training polls (`AGENTS.md` §2.1).
///
/// The link is given up to 100 ms to come up after `PERST#` is
/// deasserted; 20 polls of 5 ms each is that budget. A defence bound,
/// not a scalable capacity (`AGENTS.md` §24.4): a link that never trains
/// fails closed rather than hanging the bring-up.
pub const DEFAULT_LINK_POLLS: u32 = 20;

/// Microseconds between link-up polls (5 ms).
const LINK_POLL_INTERVAL_US: u32 = 5_000;

/// Low 32 bits of a 64-bit address, as a `u32`.
#[must_use]
const fn low32(value: u64) -> u32 {
    (value & 0xFFFF_FFFF) as u32
}

/// High 32 bits of a 64-bit address, as a `u32`.
#[must_use]
const fn high32(value: u64) -> u32 {
    ((value >> 32) & 0xFFFF_FFFF) as u32
}

/// The controller register-access seam.
///
/// Every controller access the [`BrcmPcieRc`] bring-up makes goes
/// through this trait, so the reset/SerDes/window/link sequence is
/// proven host-side against a register-level mock (`AGENTS.md` §2.2).
/// Both methods take `&mut self` so a mock can model a status register
/// whose value evolves as the bring-up polls it.
pub trait PcieRegs {
    /// Read the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `offset` is outside the mapped
    /// register window.
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError>;

    /// Write `value` to the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `offset` is outside the mapped
    /// register window.
    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError>;
}

impl PcieRegs for RegisterWindow {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        self.read_u32(offset).map_err(WindowError::as_driver_error)
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.write_u32(offset, value)
            .map_err(WindowError::as_driver_error)
    }
}

/// A microsecond busy-delay seam.
///
/// The link bring-up has hard timing requirements a register poll
/// cannot express; the kernel composition supplies a generic-timer
/// implementation, host tests a no-op.
pub trait Delay {
    /// Block for at least `us` microseconds.
    fn delay_us(&self, us: u32);
}

/// Driver entry point (`AGENTS.md` §8).
///
/// Verifies the host already granted [`CapabilityId::DRV_LOAD`] and
/// returns the registration marker handle. No hardware is touched here;
/// the link bring-up runs in [`wiring::open_discovered`].
///
/// # Errors
///
/// [`DriverError::PermissionDenied`] if the host did not grant
/// [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// The discovered address windows the root complex must be programmed
/// with, all read from the `brcm,bcm2711-pcie` device-tree node — never
/// compiled-in constants (`AGENTS.md` §18.1).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PcieWindows {
    /// Lowest PCIe-space address system memory is viewed at, from
    /// `dma-ranges` (the inbound viewport offset; `0` on the Pi 4).
    pub inbound_pcie_base: u64,
    /// Total system-memory size the inbound viewport must cover, from
    /// `dma-ranges`. Rounded up to a power of two when encoded.
    pub inbound_size: u64,
    /// CPU-physical base of the outbound MMIO window (the high PCIe
    /// MMIO aperture, e.g. `0x6_0000_0000` on the Pi 4), from `ranges`.
    pub outbound_cpu_base: u64,
    /// PCIe-space base the outbound window maps to (e.g. `0xc000_0000`).
    pub outbound_pcie_base: u64,
    /// Size of the outbound MMIO window, from `ranges`.
    pub outbound_size: u64,
}

/// Encode an inbound-viewport size into the 5-bit `RC_BARn` size field.
///
/// Returns `0` (the "disabled" encoding) for a size outside the
/// representable 4 KiB‥32 GiB range, failing closed (`AGENTS.md` §5.4).
#[must_use]
fn encode_ibar_size(size: u64) -> u32 {
    if size == 0 {
        return 0;
    }
    // Round up to the next power of two, then take its log2.
    let log2_in = size.next_power_of_two().ilog2();
    if (12..=15).contains(&log2_in) {
        (log2_in - 12) + 0x1c
    } else if (16..=35).contains(&log2_in) {
        log2_in - 15
    } else {
        0
    }
}

/// A BCM2711 PCIe root complex brought up over the register seam.
///
/// `R` is the register backing: a capability-gated [`RegisterWindow`] on
/// metal, a register-level mock in host tests. After [`BrcmPcieRc::open`]
/// returns the link is up; the caller recovers the window with
/// [`BrcmPcieRc::into_regs`] and builds the windowed configuration
/// accessor over it.
pub struct BrcmPcieRc<R: PcieRegs> {
    regs: R,
}

impl<R: PcieRegs> BrcmPcieRc<R> {
    /// Reset and bring the root complex up over `regs`, programming it
    /// with the discovered `windows` and bounding link-training by
    /// [`DEFAULT_LINK_POLLS`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the controller never reports
    ///   itself a root port, the link never trains within the poll
    ///   budget, or a register access faults.
    ///
    /// # Capabilities
    ///
    /// None beyond the window's own (mapped under
    /// [`CapabilityId::MMIO_MAP`] by the wiring).
    pub fn open(regs: R, delay: &dyn Delay, windows: &PcieWindows) -> Result<Self, DriverError> {
        Self::open_with_polls(regs, delay, windows, DEFAULT_LINK_POLLS)
    }

    /// As [`BrcmPcieRc::open`] but bounding link-training by
    /// `link_polls` (host tests assert the fail-closed timeout with a
    /// small budget).
    ///
    /// # Errors
    ///
    /// As [`BrcmPcieRc::open`].
    pub fn open_with_polls(
        regs: R,
        delay: &dyn Delay,
        windows: &PcieWindows,
        link_polls: u32,
    ) -> Result<Self, DriverError> {
        let mut dev = Self { regs };
        dev.bring_up(delay, windows, link_polls)?;
        Ok(dev)
    }

    /// Recover the brought-up register window (for the windowed config
    /// accessor the composition builds over it).
    pub fn into_regs(self) -> R {
        self.regs
    }

    /// Borrow the register backing (host-test inspection).
    #[must_use]
    pub fn regs(&self) -> &R {
        &self.regs
    }

    /// Read-modify-write the bits selected by `mask` at `offset`.
    fn modify(&mut self, offset: usize, value: u32, mask: u32) -> Result<(), DriverError> {
        let cur = self.regs.read32(offset)?;
        self.regs
            .write32(offset, regs::replace_bits(cur, value, mask))
    }

    fn link_up(&mut self) -> Result<bool, DriverError> {
        let status = self.regs.read32(regs::MISC_PCIE_STATUS)?;
        Ok(status & regs::PCIE_STATUS_DL_ACTIVE_MASK != 0
            && status & regs::PCIE_STATUS_PHYLINKUP_MASK != 0)
    }

    fn bring_up(
        &mut self,
        delay: &dyn Delay,
        windows: &PcieWindows,
        link_polls: u32,
    ) -> Result<(), DriverError> {
        // Hold the bridge in reset and assert PERST# (some firmware
        // leaves it deasserted), then release the bridge reset.
        self.modify(
            regs::RGR1_SW_INIT_1,
            1,
            regs::RGR1_SW_INIT_1_INIT_GENERIC_MASK,
        )?;
        self.modify(regs::RGR1_SW_INIT_1, 1, regs::RGR1_SW_INIT_1_PERST_MASK)?;
        delay.delay_us(200);
        self.modify(
            regs::RGR1_SW_INIT_1,
            0,
            regs::RGR1_SW_INIT_1_INIT_GENERIC_MASK,
        )?;

        // Power the SerDes up (clear IDDQ) and let it stabilise.
        self.modify(
            regs::MISC_HARD_PCIE_HARD_DEBUG,
            0,
            regs::HARD_DEBUG_SERDES_IDDQ_MASK,
        )?;
        delay.delay_us(200);

        // Misc control: enable SCB access + UR config reads, set the
        // BCM2711 burst encoding (0 = 128 bytes), RCB MPS + 64-byte mode.
        let mut ctrl = self.regs.read32(regs::MISC_MISC_CTRL)?;
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_SCB_ACCESS_EN_MASK);
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_CFG_READ_UR_MODE_MASK);
        ctrl = regs::replace_bits(ctrl, 0, regs::MISC_CTRL_MAX_BURST_SIZE_MASK);
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_RCB_MPS_MODE_MASK);
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_RCB_64B_MODE_MASK);
        self.regs.write32(regs::MISC_MISC_CTRL, ctrl)?;

        // Program the inbound (PCIe→system-memory) viewport.
        let ibar = encode_ibar_size(windows.inbound_size);
        let lo = regs::replace_bits(
            low32(windows.inbound_pcie_base),
            ibar,
            regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        );
        self.regs.write32(regs::MISC_RC_BAR2_CONFIG_LO, lo)?;
        self.regs.write32(
            regs::MISC_RC_BAR2_CONFIG_HI,
            high32(windows.inbound_pcie_base),
        )?;

        // Disable the unused PCIe→GISB (BAR1) and PCIe→SCB (BAR3)
        // inbound windows by clearing their size fields.
        self.modify(
            regs::MISC_RC_BAR1_CONFIG_LO,
            0,
            regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        )?;
        self.modify(
            regs::MISC_RC_BAR3_CONFIG_LO,
            0,
            regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        )?;

        // Confirm the controller is in root-port (not endpoint) mode
        // before advertising bridge config; fail closed otherwise.
        let status = self.regs.read32(regs::MISC_PCIE_STATUS)?;
        if status & regs::PCIE_STATUS_PORT_MASK == 0 {
            return Err(DriverError::DeviceFault);
        }

        // Advertise ASPM L0s + L1 and present the RC as a PCI-PCI bridge
        // so downstream config accesses behave.
        self.modify(
            regs::RC_CFG_PRIV1_LINK_CAPABILITY,
            regs::PCIE_LINK_STATE_L0S | regs::PCIE_LINK_STATE_L1,
            regs::RC_CFG_PRIV1_LINK_CAPABILITY_ASPM_SUPPORT_MASK,
        )?;
        self.modify(
            regs::RC_CFG_PRIV1_ID_VAL3,
            regs::PCI_CLASS_BRIDGE_PCI,
            regs::RC_CFG_PRIV1_ID_VAL3_CLASS_CODE_MASK,
        )?;

        self.program_outbound_window(windows)?;

        // PCIe→SCB endian mode for the inbound BAR path: little-endian.
        self.modify(
            regs::RC_CFG_VENDOR_VENDOR_SPECIFIC_REG1,
            regs::RC_CFG_VENDOR_REG1_LITTLE_ENDIAN,
            regs::RC_CFG_VENDOR_REG1_ENDIAN_MODE_BAR2_MASK,
        )?;

        // Deassert PERST# and wait the 100 ms link-training settle
        // before polling for link-up.
        self.modify(regs::RGR1_SW_INIT_1, 0, regs::RGR1_SW_INIT_1_PERST_MASK)?;
        delay.delay_us(100_000);

        let mut polls = 0;
        while polls < link_polls {
            if self.link_up()? {
                return Ok(());
            }
            delay.delay_us(LINK_POLL_INTERVAL_US);
            polls += 1;
        }
        if self.link_up()? {
            Ok(())
        } else {
            Err(DriverError::DeviceFault)
        }
    }

    fn program_outbound_window(&mut self, windows: &PcieWindows) -> Result<(), DriverError> {
        // PCIe-space base the window maps to.
        self.regs.write32(
            regs::MISC_CPU_2_PCIE_MEM_WIN0_LO,
            low32(windows.outbound_pcie_base),
        )?;
        self.regs.write32(
            regs::MISC_CPU_2_PCIE_MEM_WIN0_HI,
            high32(windows.outbound_pcie_base),
        )?;

        // CPU-side base and limit, expressed in MiB.
        let cpu_base_mb = windows.outbound_cpu_base / 0x10_0000;
        let cpu_limit_mb = (windows
            .outbound_cpu_base
            .saturating_add(windows.outbound_size)
            .saturating_sub(1))
            / 0x10_0000;

        let mut base_limit = self
            .regs
            .read32(regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT)?;
        base_limit = regs::replace_bits(
            base_limit,
            low32(cpu_base_mb),
            regs::MEM_WIN0_BASE_LIMIT_BASE_MASK,
        );
        base_limit = regs::replace_bits(
            base_limit,
            low32(cpu_limit_mb),
            regs::MEM_WIN0_BASE_LIMIT_LIMIT_MASK,
        );
        self.regs
            .write32(regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT, base_limit)?;

        // The high bits of base/limit: the base-limit register's base
        // field is 12 bits wide, so the upper MiB bits go here.
        let high_shift = regs::MEM_WIN0_BASE_LIMIT_BASE_MASK.count_ones();
        let base_hi = low32(cpu_base_mb >> high_shift);
        let limit_hi = low32(cpu_limit_mb >> high_shift);
        self.modify(
            regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI,
            base_hi,
            regs::MEM_WIN0_BASE_HI_BASE_MASK,
        )?;
        self.modify(
            regs::MISC_CPU_2_PCIE_MEM_WIN0_LIMIT_HI,
            limit_hi,
            regs::MEM_WIN0_LIMIT_HI_LIMIT_MASK,
        )?;
        Ok(())
    }
}
