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

/// Default upper bound on 5 ms link-training polls (`AGENTS.md` §2.1).
///
/// The link is given up to 100 ms to come up after `PERST#` is
/// deasserted; 20 polls of 5 ms each is that budget. A defence bound,
/// not a scalable capacity (`AGENTS.md` §24.4): a link that never trains
/// fails closed rather than hanging the bring-up.
pub const DEFAULT_LINK_POLLS: u32 = 20;

/// Microseconds between link-up polls (5 ms).
const LINK_POLL_INTERVAL_US: u32 = 5_000;

/// The §18.3 bind priority [`BIND_KEYS`] carries.
///
/// An exact `compatible`-string match is highly specific (one device
/// family), so it ranks above a generic class-wildcard driver (e.g. the
/// xHCI host bound by class alone, `rustos_drv_bus_usb::BIND_KEYS`) should
/// they ever contend for one node — they do not here, but the ordering is
/// the deliberate §18.3 contract.
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table (`AGENTS.md` §18.3).
///
/// It binds the BCM2711 PCIe root complex, matched by its device-tree
/// `compatible` string `brcm,bcm2711-pcie` — the node
/// `kernel/arch/aarch64::platform` discovers on a Raspberry Pi 4
/// (`plans/PI.md` P10). This `const` is the single source of truth the
/// driver's signed-manifest bind table is authored from, and the data
/// `devmgr` resolves a discovered node against (PLAN Stage 4.HW
/// increment 5); the driver declares its own match data rather than the
/// match being hard-coded in a per-board composition module (`AGENTS.md`
/// §2.2 / §18.3).
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
/// The link bring-up has hard timing requirements a register poll
/// cannot express, and a readiness poll must be bounded by real elapsed
/// time; the kernel composition supplies a generic-timer implementation
/// (`CNTPCT_EL0`/`CNTFRQ_EL0`), host tests a deterministic stand-in.
pub trait Delay {
    /// Block for at least `us` microseconds.
    fn delay_us(&self, us: u32);

    /// A monotonically non-decreasing microsecond timestamp from the same
    /// time source `delay_us` blocks against.
    ///
    /// It lets a caller bound a poll loop by *elapsed wall time* rather
    /// than a fixed iteration count, so a single read that itself blocks
    /// — e.g. a PCIe master-abort completion timeout, which on the BCM2711
    /// stalls each access to an un-decoded BAR for tens of milliseconds —
    /// cannot inflate the loop's real duration far past its intended
    /// budget (`AGENTS.md` §2.16 — a poll-count budget silently assumes
    /// cheap reads). The epoch is unspecified; only differences are
    /// meaningful.
    fn now_us(&self) -> u64;
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

/// A read-back of the controller's outbound (CPU→PCIe) memory-window
/// registers and link status, produced by
/// [`BrcmPcieRc::outbound_window_readback`] for a bring-up diagnostic.
///
/// Each field is the raw 32-bit register as it reads back on metal, or
/// the all-ones sentinel if the read faulted (`AGENTS.md` §2.9). The
/// caller (the kernel image-assembly binary) logs them; the driver does
/// not depend on a logging facility.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OutboundWindowReadback {
    /// Low 32 bits of the PCIe-space base the window maps to
    /// ([`regs::MISC_CPU_2_PCIE_MEM_WIN0_LO`]).
    pub mem_win0_lo: u32,
    /// High 32 bits of that PCIe-space base
    /// ([`regs::MISC_CPU_2_PCIE_MEM_WIN0_HI`]).
    pub mem_win0_hi: u32,
    /// CPU-side base and limit, in MiB, packed into one register
    /// ([`regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT`]).
    pub mem_win0_base_limit: u32,
    /// High bits of the CPU-side base
    /// ([`regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI`]).
    pub mem_win0_base_hi: u32,
    /// High bits of the CPU-side limit
    /// ([`regs::MISC_CPU_2_PCIE_MEM_WIN0_LIMIT_HI`]).
    pub mem_win0_limit_hi: u32,
    /// Link/role status ([`regs::MISC_PCIE_STATUS`]): root-port,
    /// data-link-active and phy-link-up bits.
    pub pcie_status: u32,
}

/// A read-back of the controller's **inbound** (PCIe→system-memory)
/// viewport registers, produced by
/// [`BrcmPcieRc::inbound_window_readback`] for a bring-up diagnostic.
///
/// On the Raspberry Pi 4 the `VideoCore` co-processor loads the VL805's
/// xHCI firmware over PCIe **through an inbound DMA window** (the "xHCI
/// firmware window"): if that inbound translation is reprogrammed away
/// from what `VideoCore` set up at power-on, a `NOTIFY_XHCI_RESET`
/// firmware (re)load is *honoured* yet a no-op and the controller never
/// decodes (raspberrypi/firmware #1617). `bring_up` programs the inbound
/// viewport in `RC_BAR2` and disables the unused `RC_BAR1`/`RC_BAR3`
/// inbound windows, so capturing these as they actually read back lets a
/// metal run compare our inbound translation with the working-Linux
/// `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`.
///
/// Each field is the raw 32-bit register as it reads back on metal, or
/// the all-ones sentinel if the read faulted (`AGENTS.md` §2.9). The
/// caller logs them; the driver does not depend on a logging facility.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
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
    /// Link/role status ([`regs::MISC_PCIE_STATUS`]) for correlation.
    pub pcie_status: u32,
}

/// Wall-time breakdown of one `BrcmPcieRc` bring-up, in microseconds,
/// produced from the [`Delay`] clock and exposed by
/// [`BrcmPcieRc::bring_up_timing`] for a bring-up diagnostic.
///
/// The metal symptom was a multi-second `bring_up` whose coded delays
/// (the reset settles plus the ≤ 100 ms link-training wait) total only a
/// few hundred milliseconds — so the time was spent in the register work
/// itself. Splitting the reset phase pinned the ~10.8 s on the **first
/// access to the MISC register block** (`0x4xxx`): at OS entry the
/// controller core is held off, and a MISC read/write does not complete
/// until the always-accessible RGR1 bridge `sw_init` reset (`0x9210`) has
/// been cycled — so any MISC access before that reset stalls ~10.8 s on
/// the `SoC` bus completion timeout (the *same* [`regs::MISC_PCIE_STATUS`]
/// read costs microseconds once the controller is out of reset, as the
/// configuration phase confirms). So `BrcmPcieRc`'s reset step releases
/// the bridge `sw_init` reset **before** touching MISC, matching U-Boot's /
/// Linux's `pcie-brcmstb`; the split is retained so a metal capture pins
/// any residual stall to the exact MMIO group (`AGENTS.md` §15.7 —
/// measure, don't guess). The four `*_us` spans sum to the whole
/// `bring_up`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct BringUpTiming {
    /// Microseconds spent **releasing** the bridge `sw_init` reset on
    /// [`regs::RGR1_SW_INIT_1`] (`0x9210`, the always-accessible reset
    /// block). Run **first**, before any MISC access, so the controller
    /// core (and its MISC block) is out of reset before the MISC block is
    /// touched. The gentlest no-touch-probe bring-up does **not** re-assert
    /// `sw_init`/`PERST#`: the previous boot stage left both asserted with
    /// the VL805 firmware already loaded, so `PERST#` is left as-is and
    /// deasserted later in `train_link` (the single firmware-(re)load
    /// edge).
    pub reset_swinit_us: u64,
    /// Microseconds spent letting the controller core / MISC block settle
    /// after the `sw_init` de-reset, before the configuration phase issues
    /// its first MISC access. No SerDes `IDDQ` toggle is performed (the
    /// SerDes is already powered by the previous boot stage's power-on
    /// bring-up); a multi-second value here would mean a MISC access still
    /// stalls even with the controller out of reset.
    pub reset_settle_us: u64,
    /// Microseconds in the configuration-programming phase: the
    /// `MISC_*` control/inbound-window writes and the type-1 bridge
    /// configuration-space reads/writes (bus numbers, Memory Base/Limit,
    /// Command, link capability, class, endian, outbound window) — issued
    /// before the link is awaited, with no coded delay of their own.
    pub config_us: u64,
    /// Microseconds in the link-wait phase: the 100 ms `PERST#`-deassert
    /// settle plus the bounded link-up poll loop.
    pub linkwait_us: u64,
    /// Link-up polls actually performed in the link-wait phase (`0` when
    /// the link came up on the first check).
    pub link_polls: u32,
    /// Raw [`regs::RGR1_SW_INIT_1`] value sampled at `bring_up` entry,
    /// **before** the reset cycles it. The always-accessible RGR1 reset
    /// block is readable immediately (no link or MISC needed), so this
    /// captures the downstream-reset state the previous boot stage left
    /// behind: a set `RGR1_SW_INIT_1_PERST_MASK` bit means `PERST#` was
    /// already asserted at OS entry (the handoff held the VL805 in
    /// fundamental reset, dropping its bootloader-loaded firmware before
    /// any RustOS code ran — a `VideoCore` reload would then be the only
    /// way to restore it), while a clear bit means the firmware should
    /// still be resident and must not be dropped (`AGENTS.md` §15.7 —
    /// measure, don't guess).
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

    /// The per-phase wall-time breakdown of the bring-up that produced
    /// this root complex (reset / configuration-programming / link-wait,
    /// in microseconds, plus the link polls performed).
    ///
    /// Measured from the [`Delay`] clock across the three disjoint phases
    /// of the bring-up; the keyboard composition logs it so a metal
    /// capture localises a multi-second bring-up to the exact phase that
    /// stalls rather than guessing (`AGENTS.md` §15.7).
    #[must_use]
    pub fn bring_up_timing(&self) -> BringUpTiming {
        self.bring_up_timing
    }

    /// Read the outbound (CPU→PCIe) memory-window registers and the link
    /// status back, for a bring-up diagnostic.
    ///
    /// The metal symptom is that the VL805's mapped BAR returns the
    /// BCM2711 `0xdead_dead` master-abort poison even though
    /// *configuration* reads succeed and every PCI-config register
    /// (bus numbers, Memory Base/Limit, the device's BAR and command) reads
    /// back what bring-up wrote. Configuration and memory take different
    /// paths through the controller — configuration through the internal
    /// `EXT_CFG` window, memory through this CPU→PCIe outbound translation
    /// window (`program_outbound_window`) — so a
    /// memory access that aborts while configuration works isolates the
    /// fault to the outbound path. This reads the window registers
    /// ([`regs::MISC_CPU_2_PCIE_MEM_WIN0_LO`] and friends) and
    /// [`regs::MISC_PCIE_STATUS`] back so a metal capture shows whether the
    /// translation window holds the programmed CPU/PCIe bases and whether
    /// the data link is actually up, rather than guessing the next change
    /// (`AGENTS.md` §15.7).
    ///
    /// Read-only and fail-closed: a faulting register read renders the
    /// all-ones sentinel and is never propagated — the read-back is
    /// diagnostic, not a bring-up step (`AGENTS.md` §2.9).
    #[must_use]
    pub fn outbound_window_readback(&mut self) -> OutboundWindowReadback {
        OutboundWindowReadback {
            mem_win0_lo: self.read_or_sentinel(regs::MISC_CPU_2_PCIE_MEM_WIN0_LO),
            mem_win0_hi: self.read_or_sentinel(regs::MISC_CPU_2_PCIE_MEM_WIN0_HI),
            mem_win0_base_limit: self.read_or_sentinel(regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT),
            mem_win0_base_hi: self.read_or_sentinel(regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI),
            mem_win0_limit_hi: self.read_or_sentinel(regs::MISC_CPU_2_PCIE_MEM_WIN0_LIMIT_HI),
            pcie_status: self.read_or_sentinel(regs::MISC_PCIE_STATUS),
        }
    }

    /// Read back the **inbound** (PCIe→system-memory) viewport registers
    /// as they actually stand after bring-up, for a metal diagnostic.
    ///
    /// On the Raspberry Pi 4 the `VideoCore` VL805 firmware (re)load runs
    /// over an inbound DMA window; a mismatch between the inbound
    /// translation we program (`RC_BAR2`, with `RC_BAR1`/`RC_BAR3`
    /// disabled) and what `VideoCore` expects makes the
    /// `NOTIFY_XHCI_RESET` reload honoured-but-no-op
    /// (raspberrypi/firmware #1617). Capturing these lets a metal run
    /// compare against the working-Linux inbound translation rather than
    /// guessing the next change (`AGENTS.md` §15.7).
    ///
    /// Read-only and fail-closed: a faulting register read renders the
    /// all-ones sentinel and is never propagated — the read-back is
    /// diagnostic, not a bring-up step (`AGENTS.md` §2.9).
    #[must_use]
    pub fn inbound_window_readback(&mut self) -> InboundWindowReadback {
        InboundWindowReadback {
            rc_bar1_lo: self.read_or_sentinel(regs::MISC_RC_BAR1_CONFIG_LO),
            rc_bar2_lo: self.read_or_sentinel(regs::MISC_RC_BAR2_CONFIG_LO),
            rc_bar2_hi: self.read_or_sentinel(regs::MISC_RC_BAR2_CONFIG_HI),
            rc_bar3_lo: self.read_or_sentinel(regs::MISC_RC_BAR3_CONFIG_LO),
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

    /// Bring the controller core online and power the SerDes up **without
    /// fundamentally resetting the downstream device**, returning the
    /// wall-time split `(swinit_us, serdes_us)` measured from the [`Delay`]
    /// clock.
    ///
    /// The order matters and matches U-Boot's / Linux's `pcie-brcmstb`:
    /// the always-accessible [`regs::RGR1_SW_INIT_1`] bridge `sw_init`
    /// reset register (`0x9210`) is released **first**, and only then is
    /// the MISC register block (`0x4xxx`) touched. At OS entry the
    /// controller core is held off and a MISC read/write does not complete
    /// until the bridge reset is released; touching MISC first stalls the
    /// access ~10.8 s on the `SoC` bus completion timeout (measured on
    /// metal — the same accesses cost microseconds once the controller is
    /// out of reset, as the configuration phase confirms).
    ///
    /// **No-touch probe (gentlest bring-up).** The previous boot stage
    /// (`start4.elf`) hands off with the bridge `sw_init` reset *and*
    /// `PERST#` already asserted (metal `entry_rgr1_sw_init = 0x3`), having
    /// already trained the link and loaded the VL805 xHCI firmware over it
    /// at power-on. Re-asserting a fundamental reset (`sw_init`/`PERST#`)
    /// or re-toggling the SerDes `IDDQ` is therefore redundant and risks
    /// dropping that resident firmware — the failure this probe isolates
    /// (`AGENTS.md` §15.7). So this does the minimum: it **releases** the
    /// already-asserted bridge `sw_init` (bringing the core and its MISC
    /// block out of reset) and lets the block settle. It does **not**
    /// re-assert `sw_init`/`PERST#` and does **not** touch the SerDes.
    /// `PERST#` is left as the handoff left it; [`Self::train_link`]
    /// deasserts it, producing the single `PERST#`-deassert edge rather
    /// than a fresh fundamental-reset cycle.
    fn reset_controller(&mut self, delay: &dyn Delay) -> Result<(u64, u64), DriverError> {
        let t_start = delay.now_us();

        // Gentlest bring-up (no-touch probe): the previous boot stage left
        // the bridge `sw_init` reset and `PERST#` asserted with the VL805
        // firmware already loaded over the power-on link, so we only
        // RELEASE the bridge `sw_init` — bringing the core and its MISC
        // block out of reset — without re-asserting a fundamental reset or
        // toggling the SerDes, either of which could drop that resident
        // firmware. `train_link` later deasserts the already-asserted
        // `PERST#` (the single firmware-(re)load edge).
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
        // Bring the controller core out of reset before any MISC access
        // (`reset_controller`), matching U-Boot's / Linux's `pcie-brcmstb`:
        // the BCM2711 holds the controller core off at OS entry, so the
        // bridge `sw_init` reset (on the always-accessible RGR1 register)
        // must be released before the MISC register block is readable —
        // otherwise the first MISC access stalls ~10.8 s on the `SoC` bus
        // completion timeout. This is the gentlest no-touch-probe bring-up:
        // `reset_controller` only *releases* the bridge `sw_init` the
        // previous boot stage left asserted (it does **not** re-assert a
        // fundamental reset or toggle the SerDes, either of which could drop
        // the VL805 firmware `start4.elf` loaded over the power-on link), and
        // `train_link` below deasserts the already-asserted `PERST#` —
        // producing the single `PERST#`-deassert edge rather than a fresh
        // fundamental-reset cycle.
        //
        // The phase marks below split the bring-up's wall time into reset
        // (further split by `reset_controller` into the bridge `sw_init`
        // release and the post-de-reset MISC settle sub-spans),
        // configuration-programming, and link-wait, recorded in
        // `self.bring_up_timing` so a metal capture localises any residual
        // stall to the exact MMIO group (`AGENTS.md` §15.7 — measure, don't
        // guess). They read the same monotonic clock the settles block
        // against, so the spans are real wall time.
        // Sample the always-accessible RGR1 reset register *before* the
        // reset touches it, so the `4117` diagnostic shows whether the
        // previous boot stage left `PERST#` asserted (VL805 firmware
        // already dropped at OS entry) or deasserted (`AGENTS.md` §15.7).
        let entry_rgr1_sw_init = self.regs.read32(regs::RGR1_SW_INIT_1)?;
        let (reset_swinit_us, reset_settle_us) = self.reset_controller(delay)?;
        let t_after_reset = delay.now_us();

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
        // Record the per-phase wall-time split for the bring-up diagnostic
        // (`bring_up_timing`); `wrapping_sub` is exact for a monotonic
        // clock and never panics on a host stub whose epoch differs
        // (`AGENTS.md` §2.9).
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
        // Enable Memory Space + Bus Master on the root-port bridge now the
        // link is trained, mirroring Linux's `pci_enable_bridge` (which
        // enables the device only once the link is up). Done last so the
        // enable latches against a live link rather than while `PERST#` is
        // still asserted: writing it during the config phase did not stick
        // on the integrated RC (the metal `4110` symptom — the bridge
        // command read back `0x0000`, leaving the VL805 BAR master-aborting
        // to `0xdead_dead` despite a correct bus-number + Memory Base/Limit
        // window).
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
    /// forwards configuration transactions downstream.
    ///
    /// The BCM2711 ships the register at 0 (primary/secondary/subordinate
    /// all 0), so the root port forwards nothing and the VL805 on bus 1
    /// never answers a configuration read. Naming the secondary
    /// ([`regs::RC_SECONDARY_BUS`]) and subordinate
    /// ([`regs::RC_SUBORDINATE_BUS`], kept equal — the root port is a
    /// single-device link with no on-board switch) buses opens that path;
    /// this mirrors the bus-number assignment a full PCI enumerator would
    /// perform, which the windowed `mech_brcm` accessor does not, so the
    /// root-complex bring-up establishes the routing itself. The accessor
    /// in turn forwards configuration only to the single device on the
    /// secondary bus, so no transaction is ever issued to an absent
    /// downstream target (`rustos_drv_bus_pci::mechanism_brcm`).
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

    /// Program the root port's type-1 bridge memory window so it forwards
    /// CPU memory transactions to the outbound PCIe window downstream.
    ///
    /// [`program_outbound_window`](Self::program_outbound_window) sets the
    /// controller's CPU→PCIe address *translation*, and
    /// [`program_bridge_bus_numbers`](Self::program_bridge_bus_numbers)
    /// opens *configuration* forwarding, but a PCI-PCI bridge forwards a
    /// *memory* transaction downstream only when the (translated) PCIe
    /// address falls inside its Memory Base/Limit window
    /// ([`regs::RC_CFG_MEMORY_BASE_LIMIT`]). The BCM2711 ships that
    /// register at 0 — an empty window — so the root port master-aborts
    /// every access to the VL805's BAR (the metal symptom: config reads
    /// succeed yet BAR reads return the `0xdead_dead` abort poison) until
    /// it is named. This mirrors the bridge-window assignment a full PCI
    /// enumerator would perform (Linux's `pci_setup_bridge`), which the
    /// windowed `mech_brcm` accessor does not.
    ///
    /// The window is set to the host bridge's outbound PCIe range
    /// `[outbound_pcie_base, outbound_pcie_base + outbound_size)`, the same
    /// range BARs are assigned within. The non-prefetchable Memory
    /// Base/Limit register only decodes addresses below 4 GiB, so a window
    /// reaching at or above the 4 GiB line fails closed (`AGENTS.md` §5.4);
    /// the BCM2711's outbound window sits at `0xc000_0000`, well below it.
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

    /// Enable Memory Space + Bus Master in the root port's own Command
    /// register, the standard PCI-PCI bridge enable a full enumerator
    /// performs (Linux's `pci_enable_bridges`), which the windowed
    /// `mech_brcm` accessor does not.
    ///
    /// A PCI-PCI bridge forwards a downstream memory transaction only when
    /// Memory Space Enable is set in its Command register (Bus Master
    /// Enable likewise gates upstream DMA), in addition to the bus numbers
    /// ([`program_bridge_bus_numbers`](Self::program_bridge_bus_numbers))
    /// and the Memory Base/Limit window
    /// ([`program_bridge_mem_window`](Self::program_bridge_mem_window)).
    /// [`bring_up`](Self::bring_up) issues this **after the link is
    /// trained**, mirroring Linux, which enables the device (`pcieport
    /// 0000:00:00.0: enabling device (0000 -> 0002)`) only once the link is
    /// up: the integrated RC latches Memory Space Enable against a live
    /// link, so an earlier write (with `PERST#` still asserted) does not
    /// stick — the metal symptom that left the VL805 BAR master-aborting
    /// (`0xdead_dead`) despite a correct bus-number + Memory Base/Limit
    /// window (the `4110` read-back showing the bridge command at `0x0000`).
    ///
    /// The high 16 bits of the dword are the write-1-to-clear Status
    /// register; they are masked off before the write so the
    /// read-modify-write does not clear any latched status bit (writing 0
    /// to a `RW1C` bit is a no-op).
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
