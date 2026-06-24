//! RustOS Broadcom BCM2711 (Raspberry Pi 4) PCIe root-complex bring-up.
//!
//! The Pi 4's VL805 xHCI controller hangs off the BCM2711 PCIe root
//! complex, which ships with its link **down**: reaching the VL805's
//! config space requires resetting the root complex, powering its SerDes,
//! programming its address windows, and training its link (`plans/PI.md`
//! P10). This crate performs that bring-up.
//!
//! This is the `lib` target of the BCM2711 PCIe bus driver crate, **not** a
//! `lib/*` crate. It is board-specific single-device support, but it lives
//! *in the driver* rather than under `lib/*`: the §2.20 device-support
//! carve-out only permits a `lib/*` home when a charter-legal *non-driver*
//! consumer (a §18.6 bootstrap-floor path, or a driver of a different class)
//! shares it, and a BCM2711 PCIe bus driver has none. PCIe root-complex
//! bring-up sits **above** the §18.6 bootstrap floor (the kernel floor is the
//! storage path only — virtio-blk + EMMC2), so this engine's only consumer is
//! this crate's own `Run` binary (`src/main.rs`). It is therefore a
//! host-testable `lib` target the freestanding bin links, with no `lib/*`
//! device-support crate to keep in sync (`AGENTS.md` §2.22 / §2.2 / §2.14).
//! It names BCM2711 detail legitimately because that is its entire purpose;
//! nothing generic depends on it.
//!
//! # Layered seams
//!
//! The state machine ([`BrcmPcieRc`]) is written against two seams so it is
//! proven host-side (`AGENTS.md` §2.2):
//!
//! * [`PcieRegs`] — register access (a capability-gated [`RegisterWindow`]
//!   on metal, a register-level mock in tests).
//! * [`Delay`] — microsecond busy-delay for the link timing requirements
//!   (a generic-timer delay on metal, a no-op in tests).
//!
//! Once the link is up the register window is handed to
//! `rustos_pci::mechanism_brcm`; this crate never enumerates
//! (`AGENTS.md` §17.4).
//!
//! # Public surface & capabilities
//!
//! The bring-up engine ([`BrcmPcieRc`], instantiated through
//! [`wiring::open_discovered`] / [`wiring::bring_up_from_node`]) and the
//! [`BIND_KEYS`] match table are the surface the `Run` binary drives.
//! [`register`] is the thin `AGENTS.md` §8 driver entry that checks
//! [`CapabilityId::DRV_LOAD`] and touches no hardware. Mapping the
//! register window requires [`CapabilityId::MMIO_MAP`]. Runs in user
//! space (no `CAP_DRV_KERNEL`).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::mmio::WindowError;
use rustos_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey, RegisterWindow,
};

pub mod regs;
pub mod wiring;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"PCB"` (PCIe Brcm) with a version nibble, matching
/// the other drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5043_4200_0000_0001;

/// Default upper bound on 5 ms link-training polls: 100 ms (20 × 5 ms)
/// after `PERST#` deassert. A defence bound, not a capacity (`AGENTS.md`
/// §24.4): a link that never trains fails closed.
pub const DEFAULT_LINK_POLLS: u32 = 20;

/// Microseconds between link-up polls (5 ms).
const LINK_POLL_INTERVAL_US: u32 = 5_000;

/// The §18.3 bind priority [`BIND_KEYS`] carries. An exact
/// `compatible`-string match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table (`AGENTS.md` §18.3): the BCM2711 PCIe
/// root complex, matched by its device-tree `compatible` string
/// `brcm,bcm2711-pcie`. The single source of truth the signed-manifest
/// bind table is authored from and `devmgr` resolves a node against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(b"brcm,bcm2711-pcie") {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic (`AGENTS.md` §2.9).
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

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

/// A microsecond timing seam: a busy-delay plus a monotonic clock.
///
/// Re-exported from `lib/abi` (`rustos_abi::Delay`) — the single definition
/// shared with the bus-agnostic USB stack (`lib/usb`), so the two driver
/// crates depend on one trait rather than each declaring their own
/// (`AGENTS.md` §2.2). The kernel supplies a generic-timer implementation
/// (`CNTPCT_EL0`/`CNTFRQ_EL0`); host tests a deterministic stand-in.
pub use rustos_abi::Delay;

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
    /// `dma-ranges` (the inbound viewport's far-side base — the
    /// `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000` translation on the Pi 4).
    pub inbound_pcie_base: u64,
    /// Total system-memory size the inbound viewport must cover, from
    /// `dma-ranges`. Rounded up to a power of two when encoded.
    pub inbound_size: u64,
    /// Exclusive CPU-physical upper bound a device behind the bridge may
    /// reach through the inbound viewport, from `dma-ranges` (the discovered
    /// grant's `addr_limit`; `0x2_0000_0000` on the Pi 4). The viewport's
    /// CPU-side base is `inbound_cpu_top - inbound_size`, and the inbound
    /// translation maps that CPU base onto `inbound_pcie_base`. Forwarded
    /// verbatim onto the published xHCI node so the kernel's DMA-grant
    /// coverage check and `dma_alloc` translation agree on one aperture
    /// (`AGENTS.md` §18.1).
    pub inbound_cpu_top: u64,
    /// CPU-physical base of the outbound MMIO window (the high PCIe
    /// MMIO aperture, e.g. `0x6_0000_0000` on the Pi 4), from `ranges`.
    pub outbound_cpu_base: u64,
    /// PCIe-space base the outbound window maps to (e.g. `0xc000_0000`).
    pub outbound_pcie_base: u64,
    /// Size of the outbound MMIO window, from `ranges`.
    pub outbound_size: u64,
}

/// The controller's **inbound** (PCIe→system-memory) viewport registers,
/// captured by [`BrcmPcieRc::entry_inbound_window`] as the previous boot
/// stage left them (it seeds the capture during `bring_up`).
///
/// On the Raspberry Pi 4 the `VideoCore` co-processor loads the VL805's
/// xHCI firmware over PCIe **through an inbound DMA window** (the "xHCI
/// firmware window"): if that inbound translation is reprogrammed away
/// from what `VideoCore` set up at power-on, a `NOTIFY_XHCI_RESET`
/// firmware (re)load is *honoured* yet a no-op and the controller never
/// decodes (raspberrypi/firmware #1617). `bring_up` programs the inbound
/// viewport in `RC_BAR2` and disables the unused `RC_BAR1`/`RC_BAR3`
/// inbound windows, so capturing these as they actually read back lets a
/// metal run compare our inbound translation with the known-good
/// `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`.
///
/// Each field is the raw 32-bit register as it reads back on metal, or
/// the all-ones sentinel if the read faulted (`AGENTS.md` §2.9). The
/// caller logs them; the driver does not depend on a logging facility.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct InboundWindowReadback {
    /// `RC_BAR1_CONFIG_LO` ([`regs::MISC_RC_BAR1_CONFIG_LO`]): the
    /// PCIe→GISB inbound window, disabled by bring-up (size field 0).
    pub rc_bar1_lo: u32,
    /// Low 32 bits of the active PCIe→system-memory inbound viewport
    /// ([`regs::MISC_RC_BAR2_CONFIG_LO`]): offset bits plus the encoded
    /// size in the low field.
    pub rc_bar2_lo: u32,
    /// High 32 bits of that inbound viewport offset
    /// ([`regs::MISC_RC_BAR2_CONFIG_HI`]).
    pub rc_bar2_hi: u32,
    /// `RC_BAR3_CONFIG_LO` ([`regs::MISC_RC_BAR3_CONFIG_LO`]): the
    /// PCIe→SCB inbound window, disabled by bring-up (size field 0).
    pub rc_bar3_lo: u32,
    /// `MISC_MISC_CTRL` ([`regs::MISC_MISC_CTRL`]): the inbound-path
    /// control register, whose `SCB0_SIZE` field
    /// ([`regs::MISC_CTRL_SCB0_SIZE_MASK`], bits `[31:27]`) sizes the
    /// inbound SCB→system-memory decode window. Captured so a metal run
    /// confirms bring-up programmed `SCB0_SIZE` to match the inbound
    /// `RC_BAR2` size (an unprogrammed window silently drops `VideoCore`'s
    /// VL805 firmware-load DMA — see [`regs::MISC_CTRL_SCB0_SIZE_MASK`]).
    pub misc_ctrl: u32,
    /// Link/role status ([`regs::MISC_PCIE_STATUS`]) for correlation.
    pub pcie_status: u32,
}

/// Wall-time breakdown of one `BrcmPcieRc` bring-up, in microseconds, from
/// the [`Delay`] clock, exposed by [`BrcmPcieRc::bring_up_timing`]. The
/// reset step releases the bridge `sw_init` reset before touching MISC
/// (a MISC access before that stalls ~10.8 s on the `SoC` bus completion
/// timeout); the split lets a capture pin any residual stall to the exact
/// MMIO group. The four `*_us` spans sum to the whole `bring_up`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct BringUpTiming {
    /// Microseconds spent releasing the bridge `sw_init` reset on
    /// [`regs::RGR1_SW_INIT_1`] (`0x9210`), run **first** so the controller
    /// is out of reset before any MISC access. `PERST#` is left as the
    /// previous boot stage set it and deasserted later in `train_link`.
    pub reset_swinit_us: u64,
    /// Microseconds letting the controller settle after the `sw_init`
    /// de-reset, before the first MISC access. No SerDes `IDDQ` toggle
    /// (already powered); a multi-second value means a MISC access still
    /// stalls out of reset.
    pub reset_settle_us: u64,
    /// Microseconds in the configuration-programming phase: the `MISC_*`
    /// and type-1 bridge config writes, issued before the link is awaited
    /// with no coded delay of their own.
    pub config_us: u64,
    /// Microseconds in the link-wait phase: the 100 ms `PERST#`-deassert
    /// settle plus the bounded link-up poll loop.
    pub linkwait_us: u64,
    /// Link-up polls actually performed in the link-wait phase (`0` when
    /// the link came up on the first check).
    pub link_polls: u32,
    /// Raw [`regs::RGR1_SW_INIT_1`] value sampled at `bring_up` entry,
    /// before the reset cycles it. A set `RGR1_SW_INIT_1_PERST_MASK` bit
    /// means the previous boot stage already held the VL805 in fundamental
    /// reset (dropping its firmware); a clear bit means the firmware should
    /// still be resident and must not be dropped.
    pub entry_rgr1_sw_init: u32,
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

/// Encode an inbound system-memory region size into the 5-bit
/// `MISC_CTRL` `SCB0_SIZE` field: `ilog2(round_pow2(size)) - 15`,
/// as the BCM2711 PCIe bring-up sequence requires.
///
/// This sizes the inbound SCB (PCIe→system-memory) decode window to the
/// DMA region the [`regs::MISC_RC_BAR2_CONFIG_LO`] viewport exposes. The
/// `SCB0_SIZE` field has no 4 KiB‥32 KiB special case — unlike
/// [`encode_ibar_size`] — because the SCB window always covers system
/// RAM (≥ 64 KiB); the formula is a plain `ilog2 - 15` over the
/// representable 64 KiB‥64 GiB range, failing closed to `0` (the
/// smallest encoding) outside it (`AGENTS.md` §5.4).
#[must_use]
fn encode_scb_size(size: u64) -> u32 {
    if size == 0 {
        return 0;
    }
    let log2_in = size.next_power_of_two().ilog2();
    if (16..=36).contains(&log2_in) {
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
    /// Per-phase wall-time breakdown of the bring-up — see
    /// [`BrcmPcieRc::bring_up_timing`].
    bring_up_timing: BringUpTiming,
    /// The inbound (PCIe→system-memory) viewport as the previous boot
    /// stage left it, captured before bring-up reprograms `RC_BAR2` —
    /// see [`BrcmPcieRc::entry_inbound_window`].
    entry_inbound: InboundWindowReadback,
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
        let mut dev = Self {
            regs,
            bring_up_timing: BringUpTiming::default(),
            entry_inbound: InboundWindowReadback::default(),
        };
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

    /// The per-phase wall-time breakdown of the bring-up (reset / config /
    /// link-wait, in microseconds, plus link polls), measured from the
    /// [`Delay`] clock so a capture localises a stall to the exact phase.
    #[must_use]
    pub fn bring_up_timing(&self) -> BringUpTiming {
        self.bring_up_timing
    }

    /// The inbound (PCIe→system-memory) viewport registers as the previous
    /// boot stage left them, sampled after the controller leaves reset and
    /// **before** bring-up programs `RC_BAR2`. `VideoCore`'s firmware load
    /// assumes the `RC_BAR2` state it set at power-on, so this lets a
    /// capture tell a reprogramming divergence from a never-(re)loaded blob.
    #[must_use]
    pub fn entry_inbound_window(&self) -> InboundWindowReadback {
        self.entry_inbound
    }

    /// Read the inbound (PCIe→system-memory) viewport registers
    /// (`RC_BAR1`/`RC_BAR2`/`RC_BAR3` + `MISC_CTRL` + link status), used by
    /// `bring_up` to capture the viewport **as the previous boot stage left
    /// it** before reprogramming `RC_BAR2` (see [`Self::entry_inbound_window`]
    /// and the `firmware_inbound_configured` decision in `bring_up`).
    /// Read-only; a faulting read renders the all-ones sentinel.
    #[must_use]
    fn inbound_window_readback(&mut self) -> InboundWindowReadback {
        InboundWindowReadback {
            rc_bar1_lo: self.read_or_sentinel(regs::MISC_RC_BAR1_CONFIG_LO),
            rc_bar2_lo: self.read_or_sentinel(regs::MISC_RC_BAR2_CONFIG_LO),
            rc_bar2_hi: self.read_or_sentinel(regs::MISC_RC_BAR2_CONFIG_HI),
            rc_bar3_lo: self.read_or_sentinel(regs::MISC_RC_BAR3_CONFIG_LO),
            misc_ctrl: self.read_or_sentinel(regs::MISC_MISC_CTRL),
            pcie_status: self.read_or_sentinel(regs::MISC_PCIE_STATUS),
        }
    }

    /// Read a register, rendering a faulting read as the all-ones
    /// sentinel (`AGENTS.md` §2.9 — the diagnostic read never propagates).
    fn read_or_sentinel(&mut self, offset: usize) -> u32 {
        self.regs.read32(offset).unwrap_or(0xFFFF_FFFF)
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

    /// Bring the controller core out of reset without fundamentally
    /// resetting the downstream device, returning the wall-time split
    /// `(swinit_us, settle_us)` from the [`Delay`] clock.
    ///
    /// Order matters: the always-accessible [`regs::RGR1_SW_INIT_1`]
    /// `sw_init` reset (`0x9210`) is released **first**, then MISC
    /// (`0x4xxx`) is touched — a MISC access before that stalls ~10.8 s on
    /// the `SoC` bus completion timeout. The previous boot stage left
    /// `sw_init` and `PERST#` asserted with the VL805 firmware already
    /// loaded, so this only **releases** `sw_init` (no fundamental reset,
    /// no SerDes toggle, either of which could drop that firmware);
    /// [`Self::train_link`] deasserts `PERST#` later.
    fn reset_controller(&mut self, delay: &dyn Delay) -> Result<(u64, u64), DriverError> {
        let t_start = delay.now_us();

        // Release only the bridge `sw_init` (bring the core/MISC out of
        // reset) without re-asserting a fundamental reset or toggling the
        // SerDes, either of which could drop the resident VL805 firmware.
        self.modify(
            regs::RGR1_SW_INIT_1,
            0,
            regs::RGR1_SW_INIT_1_INIT_GENERIC_MASK,
        )?;
        let t_after_swinit = delay.now_us();

        // Let the core / MISC block settle after the de-reset before the
        // configuration phase issues its first MISC access. No SerDes IDDQ
        // toggle: the SerDes is already powered by the power-on bring-up.
        delay.delay_us(200);
        let t_after_settle = delay.now_us();

        Ok((
            t_after_swinit.wrapping_sub(t_start),
            t_after_settle.wrapping_sub(t_after_swinit),
        ))
    }

    fn bring_up(
        &mut self,
        delay: &dyn Delay,
        windows: &PcieWindows,
        link_polls: u32,
    ) -> Result<(), DriverError> {
        // Bring the core out of reset before any MISC access
        // (`reset_controller`): the MISC block is unreadable until the
        // bridge `sw_init` is released, else the first access stalls ~10.8 s
        // on the `SoC` bus completion timeout. The phase marks below split
        // the wall time (reset / config / link-wait) into
        // `self.bring_up_timing` so a capture localises any residual stall.
        // Sample RGR1 *before* the reset touches it, so `4117` shows whether
        // the previous boot stage left `PERST#` asserted.
        let entry_rgr1_sw_init = self.regs.read32(regs::RGR1_SW_INIT_1)?;
        let (reset_swinit_us, reset_settle_us) = self.reset_controller(delay)?;
        let t_after_reset = delay.now_us();

        // Capture the inbound viewport as the previous boot stage left it,
        // *before* we touch `RC_BAR2`: `VideoCore` assumes the `RC_BAR2`
        // state it set at power-on for the firmware load, so reprogramming
        // it away makes the reload a no-op. Read-only.
        self.entry_inbound = self.inbound_window_readback();

        // Misc control: enable SCB access + UR config reads, BCM2711 burst
        // encoding, RCB MPS + 64-byte mode, and size the inbound SCB decode
        // window (`SCB0_SIZE`) to the inbound region. `SCB0_SIZE` must be
        // programmed unconditionally: the reset default is undersized, so a
        // DMA past it is silently dropped while config/enumeration succeed.
        let mut ctrl = self.regs.read32(regs::MISC_MISC_CTRL)?;
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_SCB_ACCESS_EN_MASK);
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_CFG_READ_UR_MODE_MASK);
        ctrl = regs::replace_bits(ctrl, 0, regs::MISC_CTRL_MAX_BURST_SIZE_MASK);
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_RCB_MPS_MODE_MASK);
        ctrl = regs::replace_bits(ctrl, 1, regs::MISC_CTRL_RCB_64B_MODE_MASK);
        ctrl = regs::replace_bits(
            ctrl,
            encode_scb_size(windows.inbound_size),
            regs::MISC_CTRL_SCB0_SIZE_MASK,
        );
        self.regs.write32(regs::MISC_MISC_CTRL, ctrl)?;

        // Program the inbound viewport unless the previous boot stage
        // already configured it: `VideoCore`'s firmware load assumes the
        // `RC_BAR2` state it set at power-on, so overwriting a
        // firmware-configured window makes the reload a no-op. Honour a
        // non-zero size field (captured in `entry_inbound`); program from
        // the discovered window only when it was left unconfigured.
        let firmware_inbound_configured =
            self.entry_inbound.rc_bar2_lo & regs::RC_BAR_CONFIG_LO_SIZE_MASK != 0;
        if !firmware_inbound_configured {
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
        }

        // Disable the unused PCIe→GISB (BAR1) and PCIe→SCB (BAR3)
        // inbound windows by clearing their size fields. These are not the
        // system-memory viewport `VideoCore` assumes for the firmware load
        // (that is `RC_BAR2`, preserved above), so clearing them is safe.
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

        // Name the downstream bus so the root port forwards configuration
        // transactions to it; the BCM2711 ships the bus-number register
        // at 0, which leaves the VL805 (bus 1) unreachable until set.
        self.program_bridge_bus_numbers()?;

        self.program_bridge_mem_window(windows)?;

        self.program_outbound_window(windows)?;

        // PCIe→SCB endian mode for the inbound BAR path: little-endian.
        self.modify(
            regs::RC_CFG_VENDOR_VENDOR_SPECIFIC_REG1,
            regs::RC_CFG_VENDOR_REG1_LITTLE_ENDIAN,
            regs::RC_CFG_VENDOR_REG1_ENDIAN_MODE_BAR2_MASK,
        )?;
        let t_after_config = delay.now_us();

        let polls_used = self.train_link(delay, link_polls)?;
        let t_after_link = delay.now_us();
        // Record the per-phase wall-time split (`bring_up_timing`);
        // `wrapping_sub` is exact for a monotonic clock.
        self.bring_up_timing = BringUpTiming {
            reset_swinit_us,
            reset_settle_us,
            config_us: t_after_config.wrapping_sub(t_after_reset),
            linkwait_us: t_after_link.wrapping_sub(t_after_config),
            link_polls: polls_used,
            entry_rgr1_sw_init,
        };
        // Training just completed: confirm the link is live before
        // declaring the root complex up, failing closed otherwise
        // (`AGENTS.md` §5.4 / §2.9).
        if !self.link_up()? {
            return Err(DriverError::DeviceFault);
        }
        // Enable Memory Space + Bus Master on the root-port bridge, done
        // last so it latches against a live link: writing it with `PERST#`
        // still asserted did not stick on the integrated RC (the `4110`
        // symptom — bridge command read back `0x0000`).
        self.program_bridge_command()?;
        Ok(())
    }

    /// Deassert `PERST#`, wait the 100 ms link-training settle, then poll
    /// for link-up, returning the number of polls performed.
    ///
    /// Bounded by `link_polls` (`AGENTS.md` §2.1 — a link that never trains
    /// fails closed in the caller rather than hanging); the poll count is
    /// reported into the bring-up timing diagnostic
    /// ([`bring_up_timing`](Self::bring_up_timing)).
    fn train_link(&mut self, delay: &dyn Delay, link_polls: u32) -> Result<u32, DriverError> {
        self.modify(regs::RGR1_SW_INIT_1, 0, regs::RGR1_SW_INIT_1_PERST_MASK)?;
        delay.delay_us(100_000);
        let mut polls_used = 0;
        while polls_used < link_polls {
            if self.link_up()? {
                break;
            }
            delay.delay_us(LINK_POLL_INTERVAL_US);
            polls_used += 1;
        }
        Ok(polls_used)
    }

    /// Program the root port's type-1 bridge bus-number register so it
    /// forwards configuration downstream. The BCM2711 ships it at 0, so the
    /// VL805 on bus 1 never answers until the secondary
    /// ([`regs::RC_SECONDARY_BUS`]) and subordinate
    /// ([`regs::RC_SUBORDINATE_BUS`], kept equal) buses are named — the
    /// assignment a full enumerator would do, which the windowed `mech_brcm`
    /// accessor does not.
    fn program_bridge_bus_numbers(&mut self) -> Result<(), DriverError> {
        let cur = self.regs.read32(regs::RC_CFG_PRIMARY_BUS)?;
        let mut value = regs::replace_bits(cur, 0, regs::PRIMARY_BUS_PRIMARY_MASK);
        value = regs::replace_bits(
            value,
            u32::from(regs::RC_SECONDARY_BUS),
            regs::PRIMARY_BUS_SECONDARY_MASK,
        );
        value = regs::replace_bits(
            value,
            u32::from(regs::RC_SUBORDINATE_BUS),
            regs::PRIMARY_BUS_SUBORDINATE_MASK,
        );
        self.regs.write32(regs::RC_CFG_PRIMARY_BUS, value)
    }

    /// Program the root port's type-1 bridge Memory Base/Limit window
    /// ([`regs::RC_CFG_MEMORY_BASE_LIMIT`]) so it forwards CPU memory
    /// downstream: a PCI-PCI bridge forwards a memory transaction only when
    /// the PCIe address falls inside this window, and the BCM2711 ships it
    /// empty (so the VL805 BAR master-aborts to `0xdead_dead` until named).
    /// Set to the outbound PCIe range; the non-prefetchable register only
    /// decodes below 4 GiB, so a window reaching the 4 GiB line fails closed.
    fn program_bridge_mem_window(&mut self, windows: &PcieWindows) -> Result<(), DriverError> {
        let base = windows.outbound_pcie_base;
        let end = base
            .checked_add(windows.outbound_size)
            .ok_or(DriverError::OutOfRange)?;
        if windows.outbound_size == 0 || end > 0x1_0000_0000 {
            return Err(DriverError::OutOfRange);
        }
        let limit = end - 1;

        let base_field = low32(base >> regs::MEMORY_WINDOW_GRANULE_SHIFT);
        let limit_field = low32(limit >> regs::MEMORY_WINDOW_GRANULE_SHIFT);
        let cur = self.regs.read32(regs::RC_CFG_MEMORY_BASE_LIMIT)?;
        let mut value = regs::replace_bits(cur, base_field, regs::MEMORY_BASE_LIMIT_BASE_MASK);
        value = regs::replace_bits(value, limit_field, regs::MEMORY_BASE_LIMIT_LIMIT_MASK);
        self.regs.write32(regs::RC_CFG_MEMORY_BASE_LIMIT, value)
    }

    /// Enable Memory Space + Bus Master in the root port's Command register
    /// (the bridge enable a full enumerator does, gating downstream memory
    /// forwarding and upstream DMA). Issued **after the link is trained**:
    /// the integrated RC latches Memory Space Enable against a live link, so
    /// an earlier write does not stick. The high 16 bits (the RW1C Status
    /// register) are masked off so no latched status bit is cleared.
    fn program_bridge_command(&mut self) -> Result<(), DriverError> {
        let cur = self.regs.read32(regs::RC_CFG_COMMAND)?;
        let value = (cur & !regs::COMMAND_STATUS_MASK)
            | regs::COMMAND_MEMORY_SPACE_MASK
            | regs::COMMAND_BUS_MASTER_MASK;
        self.regs.write32(regs::RC_CFG_COMMAND, value)
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
