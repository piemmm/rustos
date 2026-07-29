//! TAIRiX bus-agnostic `xHCI` USB host-controller protocol.
//!
//! This `lib/*` crate carries the host-provable, **bus-agnostic** `xHCI`
//! layers: the register vocabulary
//! ([`regs`]), the TRB vocabulary ([`trb`]), the ring state machines
//! ([`ring`]), the controller bring-up sequence ([`Xhci::open`] — `xHCI`
//! 1.2 §4.2: halt, reset, wait ready), and the single-device enumeration
//! engine ([`device::UsbDevice`]). It depends only on the stable ABI
//! types in `lib/abi`, so it builds for every Tier-1 target and is
//! identical across architectures — the USB protocol is the same on
//! `aarch64`, `x86_64`, and `riscv64`.
//!
//! It is the USB analogue of `lib/virtio`: the protocol lives here so a
//! concrete host-controller *driver* (`drivers/bus/usb`, which adds the
//! PCI discovery/BAR/DMA wiring and the `register` entry) and an
//! arch-neutral user-space keyboard driver can both consume it without
//! depending on each other (`drivers/* → lib/*`
//! only).
//!
//! # Layered seam
//!
//! Every controller access goes through the [`XhciHost`] register seam,
//! not a concrete memory mapping. Metal drives it over a
//! capability-gated [`RegisterWindow`] whose base the hardware tree
//! discovered (PCI BAR assignment — never a compiled-in constant); host tests drive it over a register-level mock
//! controller. This mirrors the `emmc2` `SdhciHost` seam: the bring-up and ring protocol is proven host-side, the
//! doorbell below it on metal.
//!
//! # Public surface
//!
//! [`Xhci`] is the controller engine a driver instantiates through
//! [`Xhci::open`] over the discovered register window; [`device::UsbDevice`]
//! is the single-device enumeration engine built over it. Neither holds any
//! capability of its own — authority is the consuming driver's
//! ([`RegisterWindow`] mapping is gated by `CAP_MMIO_MAP` in the wiring that
//! mints the window, the DMA carve by `CAP_MEM_DMA`).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// The enumeration engine's tables and the DMA bank's chunk bookkeeping
// grow with the devices actually served (fallibly — exhaustion is a typed
// error, never a panic).
extern crate alloc;

use tairix_abi::driver::mmio::WindowError;
use tairix_abi::{DriverError, RegisterWindow};

pub mod bank;
pub mod device;
pub mod regs;
pub mod ring;
pub mod transport;
pub mod trb;

pub use bank::SlabBank;

#[cfg(test)]
mod tests;

/// Upper bound on register polls while waiting for a controller event.
///
/// A bound on a *defence* against an unresponsive or absent controller,
/// not a scalable capacity: controller ready, halt,
/// and reset each complete within milliseconds on real silicon, so a
/// million polls is orders of magnitude past any honest completion.
/// Exceeding it fails closed with [`DriverError::DeviceFault`] rather
/// than spinning forever.
pub const DEFAULT_POLL_BUDGET: u32 = 1_000_000;

/// BAR slot carrying the xHCI register block (xHCI 1.2 §5.2.1: the memory
/// BAR at offset `0x10`, i.e. BAR0).
///
/// An xHCI-protocol fact, not a board or device identity: it lives here,
/// beside the controller engine, so every host that maps an xHCI
/// controller's register BAR — the PCI bus driver's wiring
/// (`drivers/bus/usb`) and the root-complex bus driver that resolves the
/// controller's BAR before publishing it (`drivers/bus/pcie_brcm`) — depends
/// on one definition.
pub const XHCI_BAR_INDEX: u8 = 0;

/// The device-tree-style `compatible` identity a discovered xHCI USB
/// host-controller node carries.
///
/// A bus driver that brings an xHCI controller up publishes the controller
/// into the hardware tree under this `compatible` string, and the
/// controller's driver binds it with a matching
/// [`HwMatchKey::compatible`](tairix_abi::HwMatchKey::compatible) bind key.
/// It is an xHCI-protocol identity (the controller class), not a board or
/// vendor name, so it lives here beside the controller engine as the single
/// definition both the emitting bus driver (`drivers/bus/usb/vl805`) and the
/// binding controller driver (`drivers/input/usb_kbd`) depend on, never a
/// copy in each.
pub const XHCI_COMPATIBLE: &[u8] = b"usb,xhci";

/// Highest doorbell target value (: endpoint IDs 1..=31 for device
/// doorbells; 0 is the command-ring target on doorbell 0).
const DOORBELL_TARGET_MAX: u32 = 31;

/// The `xHCI` register-access seam.
///
/// Every controller access the [`Xhci`] engine makes goes through this
/// trait, so the bring-up state machine is proven host-side against a
/// register-level mock. Both methods take
/// `&mut self` so a model can represent registers with side-effects
/// (self-clearing reset bits; write-1-to-clear status bits).
pub trait XhciHost {
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

impl XhciHost for RegisterWindow {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        self.read_u32(offset).map_err(WindowError::as_driver_error)
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.write_u32(offset, value)
            .map_err(WindowError::as_driver_error)
    }
}

/// Decoded view of one root-hub port's `PORTSC` value (§5.4.8).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PortStatus(u32);

impl PortStatus {
    /// Wrap a raw `PORTSC` value.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw `PORTSC` dword this view wraps, for one-shot diagnostics
    /// (a metal capture of every root-hub port's status).
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// A device is attached ([`regs::PORTSC_CCS`]).
    #[must_use]
    pub const fn connected(self) -> bool {
        self.0 & regs::PORTSC_CCS != 0
    }

    /// The port is enabled ([`regs::PORTSC_PED`]).
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.0 & regs::PORTSC_PED != 0
    }

    /// The port is powered ([`regs::PORTSC_PP`]).
    #[must_use]
    pub const fn powered(self) -> bool {
        self.0 & regs::PORTSC_PP != 0
    }

    /// A port reset is in progress ([`regs::PORTSC_PR`]).
    #[must_use]
    pub const fn resetting(self) -> bool {
        self.0 & regs::PORTSC_PR != 0
    }

    /// The connect status changed since last cleared
    /// ([`regs::PORTSC_CSC`]).
    #[must_use]
    pub const fn connect_changed(self) -> bool {
        self.0 & regs::PORTSC_CSC != 0
    }

    /// Protocol-defined port speed ID (`1` full, `2` low, `3` high,
    /// `4` super; `0` when no device is connected).
    #[must_use]
    pub const fn speed(self) -> u8 {
        let speed = (self.0 >> regs::PORTSC_SPEED_SHIFT) & regs::PORTSC_SPEED_MASK;
        speed.to_le_bytes()[0]
    }
}

/// The device-shared memory addresses [`Xhci::start`] programs.
///
/// Every address is device-visible and 64-byte aligned (the strictest
/// alignment any of the four structures requires); the memory
/// they point into is owned by the caller's DMA bank
/// ([`device::DmaBank`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaProgram {
    /// Device context base address array, for `DCBAAP`.
    pub dcbaap: u64,
    /// Command ring base, for `CRCR` (consumer cycle state starts 1).
    pub command_ring: u64,
    /// Event ring segment table (one entry), for `ERSTSZ`/`ERSTBA`.
    pub erst: u64,
    /// First event segment slot, the initial `ERDP`.
    pub event_segment: u64,
}

impl DmaProgram {
    /// `true` when every address is non-zero and 64-byte aligned.
    #[must_use]
    pub const fn is_plausible(&self) -> bool {
        let mut ok = true;
        let addrs = [
            self.dcbaap,
            self.command_ring,
            self.erst,
            self.event_segment,
        ];
        let mut i = 0;
        while i < addrs.len() {
            if addrs[i] == 0 || !addrs[i].is_multiple_of(64) {
                ok = false;
            }
            i += 1;
        }
        ok
    }
}

/// An `xHCI` controller brought to the halted, freshly reset state.
///
/// [`Xhci::open`] validates the capability block, then runs the
/// initialisation prologue: wait for Controller Not Ready to clear,
/// halt a running controller, and issue a Host Controller Reset. The
/// controller is left halted; [`Xhci::start`] programs the DMA
/// structures and starts it.
pub struct Xhci<H: XhciHost> {
    host: H,
    op_base: usize,
    db_base: usize,
    rt_base: usize,
    hci_version: u16,
    max_slots: u8,
    max_ports: u8,
    max_scratchpad: u32,
    page_size: usize,
    ac64: bool,
    csz: bool,
}

/// Stage of [`Xhci::open_diagnostic`] that refused the controller.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum XhciOpenStage {
    /// The capability block was malformed or unreadable.
    Capability,
    /// `USBSTS.HCHalted` did not assert after `Run/Stop` was cleared.
    HaltedBeforeReset,
    /// `USBCMD.HCRST` did not self-clear.
    ResetSelfClear,
    /// `USBSTS.CNR` stayed set after reset completion.
    ControllerReadyAfterReset,
}

impl XhciOpenStage {
    /// Stable diagnostic name for the failing stage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::HaltedBeforeReset => "halted_before_reset",
            Self::ResetSelfClear => "reset_self_clear",
            Self::ControllerReadyAfterReset => "controller_ready_after_reset",
        }
    }
}

/// Operational-register snapshot captured when [`Xhci::open_diagnostic`]
/// fails.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct XhciOpenRegisters {
    /// Last `USBCMD` value read at the failing stage, if readable.
    pub usbcmd: Option<u32>,
    /// Last `USBSTS` value read at the failing stage, if readable.
    pub usbsts: Option<u32>,
}

/// Rich failure returned by [`Xhci::open_diagnostic`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct XhciOpenError {
    /// Driver ABI error for the refusal.
    pub error: DriverError,
    /// The exact open stage that failed.
    pub stage: XhciOpenStage,
    /// Register values observed at the failing stage.
    pub registers: XhciOpenRegisters,
}

impl<H: XhciHost> Xhci<H> {
    /// Bring the controller to the halted, reset state with the
    /// default poll budget.
    ///
    /// # Errors
    ///
    /// See [`Xhci::open_with_budget`].
    pub fn open(host: H) -> Result<Self, DriverError> {
        Self::open_with_budget(host, DEFAULT_POLL_BUDGET)
    }

    /// [`Xhci::open`] with an explicit poll budget (tests use a small
    /// one so a stuck-controller path fails fast).
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the capability block is
    ///   implausible (`CAPLENGTH` below [`regs::CAPLENGTH_MIN`],
    ///   `HCIVERSION` below [`regs::HCIVERSION_MIN`] or all-ones, zero
    ///   `MaxSlots`/`MaxPorts`, zero `DBOFF`/`RTSOFF`) — the absent-
    ///   or broken-controller signatures — or if ready/halt/reset
    ///   never completes within `budget` polls.
    pub fn open_with_budget(host: H, budget: u32) -> Result<Self, DriverError> {
        Self::open_diagnostic_with_budget(host, budget).map_err(|err| err.error)
    }

    /// [`Xhci::open`] with a diagnostic error that names the failing
    /// reset stage and includes the operational-register values observed
    /// there.
    ///
    /// # Errors
    ///
    /// As [`Self::open_with_budget`], but wrapped in [`XhciOpenError`].
    pub fn open_diagnostic(host: H) -> Result<Self, XhciOpenError> {
        Self::open_diagnostic_with_budget(host, DEFAULT_POLL_BUDGET)
    }

    /// [`Xhci::open_diagnostic`] with an explicit poll budget.
    ///
    /// # Errors
    ///
    /// As [`Self::open_with_budget`], but wrapped in [`XhciOpenError`].
    pub fn open_diagnostic_with_budget(mut host: H, budget: u32) -> Result<Self, XhciOpenError> {
        let cap = host
            .read32(regs::CAPLENGTH_HCIVERSION)
            .map_err(|err| Self::open_error(err, XhciOpenStage::Capability, None, None))?;
        let caplength = regs::caplength(cap);
        let hci_version = regs::hciversion(cap);
        if caplength < regs::CAPLENGTH_MIN
            || hci_version < regs::HCIVERSION_MIN
            || hci_version == u16::MAX
        {
            return Err(Self::open_error(
                DriverError::DeviceFault,
                XhciOpenStage::Capability,
                None,
                None,
            ));
        }
        let structural = host
            .read32(regs::HCSPARAMS1)
            .map_err(|err| Self::open_error(err, XhciOpenStage::Capability, None, None))?;
        let max_slots = regs::hcsparams1_max_slots(structural);
        let max_ports = regs::hcsparams1_max_ports(structural);
        if max_slots == 0 || max_ports == 0 {
            return Err(Self::open_error(
                DriverError::DeviceFault,
                XhciOpenStage::Capability,
                None,
                None,
            ));
        }
        // Max Scratchpad Buffers: page-sized buffers the controller
        // requires software to reserve and point `DCBAA[0]` at before it
        // can run any command (xHCI §4.20). The VL805 reports 31; missing
        // them leaves the very first command without a completion event
        // (the Pi 4 `stage=2 completion=0` metal symptom).
        let structural2 = host
            .read32(regs::HCSPARAMS2)
            .map_err(|err| Self::open_error(err, XhciOpenStage::Capability, None, None))?;
        let max_scratchpad = regs::hcsparams2_max_scratchpad(structural2);
        let capability = host
            .read32(regs::HCCPARAMS1)
            .map_err(|err| Self::open_error(err, XhciOpenStage::Capability, None, None))?;
        let db_off = host
            .read32(regs::DBOFF)
            .map_err(|err| Self::open_error(err, XhciOpenStage::Capability, None, None))?
            & regs::DBOFF_MASK;
        let rt_off = host
            .read32(regs::RTSOFF)
            .map_err(|err| Self::open_error(err, XhciOpenStage::Capability, None, None))?
            & regs::RTSOFF_MASK;
        if db_off == 0 || rt_off == 0 {
            return Err(Self::open_error(
                DriverError::DeviceFault,
                XhciOpenStage::Capability,
                None,
                None,
            ));
        }
        let op_base = caplength as usize;

        let mut xhci = Self {
            host,
            op_base,
            db_base: db_off as usize,
            rt_base: rt_off as usize,
            hci_version,
            max_slots,
            max_ports,
            max_scratchpad,
            // Resolved from the operational `PAGESIZE` register below, once
            // the operational base (`CAPLENGTH`) is known.
            page_size: 0,
            ac64: regs::hccparams1_ac64(capability),
            csz: regs::hccparams1_csz(capability),
        };
        // The scratchpad buffers are each one controller page and
        // page-aligned; read the size now so [`Self::start`] can
        // lay them out. A malformed register (no supported size) fails
        // closed at the capability stage rather than assuming 4 KiB.
        let page_raw = xhci
            .read_op(regs::PAGESIZE)
            .map_err(|err| xhci.open_error_with_snapshot(err, XhciOpenStage::Capability))?;
        xhci.page_size = regs::pagesize_bytes(page_raw);
        if max_scratchpad > 0 && xhci.page_size == 0 {
            return Err(
                xhci.open_error_with_snapshot(DriverError::DeviceFault, XhciOpenStage::Capability)
            );
        }

        xhci.reset_to_ready(budget)?;
        Ok(xhci)
    }

    /// Run the initialisation prologue on a freshly-parsed
    /// controller: halt a running controller, clear any latched
    /// Host System Error / Port Change status firmware left, issue the
    /// Host Controller Reset, and wait for it to self-clear and for
    /// Controller Not Ready to clear before any further programming.
    pub(crate) fn reset_to_ready(&mut self, budget: u32) -> Result<(), XhciOpenError> {
        // Halt a running controller before resetting it (§5.4.1.1).
        let usbcmd = self
            .read_op(regs::USBCMD)
            .map_err(|err| self.open_error_with_snapshot(err, XhciOpenStage::HaltedBeforeReset))?;
        if usbcmd & regs::USBCMD_RUN != 0 {
            self.write_op(regs::USBCMD, usbcmd & !regs::USBCMD_RUN)
                .map_err(|err| {
                    self.open_error_with_snapshot(err, XhciOpenStage::HaltedBeforeReset)
                })?;
        }
        if let Err(err) = self.wait_status(regs::USBSTS_HCH, true, budget) {
            return Err(self.open_error_with_snapshot(err, XhciOpenStage::HaltedBeforeReset));
        }
        let latched_status = self
            .read_op(regs::USBSTS)
            .map_err(|err| self.open_error_with_snapshot(err, XhciOpenStage::ResetSelfClear))?
            & (regs::USBSTS_HSE | regs::USBSTS_PCD);
        if latched_status != 0 {
            self.write_op(regs::USBSTS, latched_status)
                .map_err(|err| self.open_error_with_snapshot(err, XhciOpenStage::ResetSelfClear))?;
            // Read back the status register so a posted bridge write cannot
            // leave stale error bits visible when the reset command arrives.
            self.read_op(regs::USBSTS)
                .map_err(|err| self.open_error_with_snapshot(err, XhciOpenStage::ResetSelfClear))?;
        }
        // Host Controller Reset: self-clearing on completion, after
        // which CNR must also clear before further programming.
        self.write_op(regs::USBCMD, regs::USBCMD_HCRST)
            .map_err(|err| self.open_error_with_snapshot(err, XhciOpenStage::ResetSelfClear))?;
        if let Err(err) = self.wait_op_clear(regs::USBCMD, regs::USBCMD_HCRST, budget) {
            return Err(self.open_error_with_snapshot(err, XhciOpenStage::ResetSelfClear));
        }
        if let Err(err) = self.wait_status(regs::USBSTS_CNR, false, budget) {
            return Err(
                self.open_error_with_snapshot(err, XhciOpenStage::ControllerReadyAfterReset)
            );
        }
        Ok(())
    }

    const fn open_error(
        error: DriverError,
        stage: XhciOpenStage,
        usbcmd: Option<u32>,
        usbsts: Option<u32>,
    ) -> XhciOpenError {
        XhciOpenError {
            error,
            stage,
            registers: XhciOpenRegisters { usbcmd, usbsts },
        }
    }

    fn open_error_with_snapshot(
        &mut self,
        error: DriverError,
        stage: XhciOpenStage,
    ) -> XhciOpenError {
        Self::open_error(
            error,
            stage,
            self.read_op(regs::USBCMD).ok(),
            self.read_op(regs::USBSTS).ok(),
        )
    }

    fn read_op(&mut self, offset: usize) -> Result<u32, DriverError> {
        self.host.read32(self.op_base + offset)
    }

    fn write_op(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.host.write32(self.op_base + offset, value)
    }

    /// Poll `USBSTS` until `mask` reads as `set`, within `budget`.
    fn wait_status(&mut self, mask: u32, set: bool, budget: u32) -> Result<(), DriverError> {
        for _ in 0..budget {
            let status = self.read_op(regs::USBSTS)?;
            if (status & mask != 0) == set {
                return Ok(());
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Poll an operational register until `mask` clears, within
    /// `budget`.
    fn wait_op_clear(&mut self, offset: usize, mask: u32, budget: u32) -> Result<(), DriverError> {
        for _ in 0..budget {
            if self.read_op(offset)? & mask == 0 {
                return Ok(());
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Interface version from `HCIVERSION` (BCD, e.g. `0x0110`).
    #[must_use]
    pub const fn hci_version(&self) -> u16 {
        self.hci_version
    }

    /// Device slots the controller supports (`HCSPARAMS1` `MaxSlots`).
    #[must_use]
    pub const fn max_slots(&self) -> u8 {
        self.max_slots
    }

    /// Root-hub ports the controller exposes (`HCSPARAMS1` `MaxPorts`).
    #[must_use]
    pub const fn max_ports(&self) -> u8 {
        self.max_ports
    }

    /// Page-sized scratchpad buffers the controller requires software to
    /// reserve (`HCSPARAMS2` Max Scratchpad Buffers; `0` when none are
    /// needed). [`device::UsbDevice::start`] reserves this many pages and
    /// points `DCBAA[0]` at their pointer array (xHCI §4.20).
    #[must_use]
    pub const fn max_scratchpad_buffers(&self) -> u32 {
        self.max_scratchpad
    }

    /// The controller page size in bytes the scratchpad buffers are sized
    /// and aligned to (`PAGESIZE`; `0` only when no scratchpad is
    /// required and the register was unreadable).
    #[must_use]
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    /// `true` if the controller addresses 64-bit DMA (`HCCPARAMS1`
    /// AC64).
    #[must_use]
    pub const fn ac64(&self) -> bool {
        self.ac64
    }

    /// `true` if device contexts are 64 bytes (`HCCPARAMS1` CSZ).
    #[must_use]
    pub const fn csz(&self) -> bool {
        self.csz
    }

    /// Read `USBCMD` for a one-shot bring-up diagnostic, or `None` if the
    /// register window faults the read.
    ///
    /// A driver wraps the engine's coarse [`DriverError`] with its own
    /// structured diagnostics (the engine holds no logging dependency); this exposes the operational command
    /// register so a stuck-controller capture names what the silicon
    /// reported.
    pub fn read_usbcmd(&mut self) -> Option<u32> {
        self.read_op(regs::USBCMD).ok()
    }

    /// Read `USBSTS` for a one-shot bring-up diagnostic, or `None` if the
    /// register window faults the read (companion of [`Self::read_usbcmd`]).
    pub fn read_usbsts(&mut self) -> Option<u32> {
        self.read_op(regs::USBSTS).ok()
    }

    /// Whether the controller has latched a fatal Host System Error
    /// (`USBSTS.HSE`) or halted (`USBSTS.HCHalted`).
    ///
    /// A host-system error self-clears only through a Host Controller Reset
    /// (xHCI §4.24.1), and a halted controller runs nothing, so once either
    /// bit is set the interrupter never re-asserts and no further interrupt is
    /// raised — a watched device's hot-plug and transfers go silent until the
    /// controller is reset and re-enumerated. The caller recovers by resetting
    /// and re-enumerating the engine. An unreadable `USBSTS` is reported as
    /// not-faulted so a transient register-read miss never triggers a spurious
    /// reset.
    #[must_use]
    pub fn controller_faulted(&mut self) -> bool {
        self.read_op(regs::USBSTS)
            .is_ok_and(|status| status & (regs::USBSTS_HSE | regs::USBSTS_HCH) != 0)
    }

    /// Byte offset of the runtime register block within the window
    /// (`RTSOFF`).
    #[must_use]
    pub const fn runtime_base(&self) -> usize {
        self.rt_base
    }

    /// Byte offset of the operational register block within the window
    /// (`CAPLENGTH` — the length of the capability registers).
    #[must_use]
    pub const fn caplength(&self) -> usize {
        self.op_base
    }

    /// Byte offset of the doorbell array within the window (`DBOFF`).
    #[must_use]
    pub const fn doorbell_base(&self) -> usize {
        self.db_base
    }

    fn write_ir0(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.host
            .write32(self.rt_base + regs::IR0_BASE + offset, value)
    }

    /// Program the DMA structures and start the controller (
    /// steps 5–7): `CONFIG` (all reported slots enabled), `DCBAAP`,
    /// `CRCR` (consumer cycle state 1), interrupter 0's single-entry
    /// event ring segment table and dequeue pointer, then Run/Stop.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if any `prog` address is zero or
    ///   not 64-byte aligned — the controller would fault or,
    ///   worse, DMA somewhere unintended (fail closed).
    /// * [`DriverError::DeviceFault`] if the controller never leaves
    ///   the halted state within `budget` polls.
    pub fn start(&mut self, prog: &DmaProgram, budget: u32) -> Result<(), DriverError> {
        if !prog.is_plausible() {
            return Err(DriverError::OutOfRange);
        }
        // The caller has already written the DMA structures (DCBAA, command
        // ring, event segment, ERST, device contexts) this programming points
        // the controller at. Order those Normal-Non-Cacheable writes ahead of
        // the register stores below so the controller — a separate,
        // possibly non-coherent bus master — never reads stale structures
        // once `RUN` is set (see `tairix_dma_barrier`).
        tairix_dma_barrier::dma_wmb();
        self.write_op(regs::CONFIG, u32::from(self.max_slots))?;
        self.write_op(regs::DCBAAP, low_dword(prog.dcbaap))?;
        self.write_op(regs::DCBAAP + 4, high_dword(prog.dcbaap))?;
        self.write_op(regs::CRCR, low_dword(prog.command_ring) | regs::CRCR_RCS)?;
        self.write_op(regs::CRCR + 4, high_dword(prog.command_ring))?;
        self.write_ir0(regs::IR_ERSTSZ, 1)?;
        self.write_ir0(regs::IR_ERSTBA, low_dword(prog.erst))?;
        self.write_ir0(regs::IR_ERSTBA + 4, high_dword(prog.erst))?;
        self.write_ir0(regs::IR_ERDP, low_dword(prog.event_segment))?;
        self.write_ir0(regs::IR_ERDP + 4, high_dword(prog.event_segment))?;
        let usbcmd = self.read_op(regs::USBCMD)?;
        self.write_op(regs::USBCMD, usbcmd | regs::USBCMD_RUN)?;
        self.wait_status(regs::USBSTS_HCH, false, budget)
    }

    /// Advance interrupter 0's event ring dequeue pointer to `erdp`
    /// (the device-visible address of the next unconsumed event slot),
    /// clearing Event Handler Busy (§5.5.2.3.3).
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the register window rejects
    ///   the write.
    pub fn ack_event(&mut self, erdp: u64) -> Result<(), DriverError> {
        self.write_ir0(regs::IR_ERDP, low_dword(erdp) | regs::ERDP_EHB)?;
        self.write_ir0(regs::IR_ERDP + 4, high_dword(erdp))
    }

    /// Enable interrupt generation on interrupter 0 so a posted event
    /// asserts the device's interrupt (the MSI write, on the PCIe VL805)
    /// rather than only landing on the event ring for a poller to find.
    ///
    /// Programs interrupt moderation to the 1 ms reset default
    /// ([`regs::IMODI_DEFAULT`]) so a device that streams a report every
    /// service interval — a mouse can post thousands per second — is coalesced
    /// to at most ~1000 interrupts/s instead of storming the CPU with one
    /// interrupt per report; a lone, sparse report is not delayed past the
    /// interval, so genuine input latency is unaffected. It then sets the
    /// per-interrupter Interrupt Enable while clearing any stale Interrupt
    /// Pending the firmware hand-off left latched, then sets the global
    /// `USBCMD.INTE`. Idempotent: re-enabling an already-enabled
    /// interrupter re-programs moderation and re-clears IP and re-sets the
    /// same bits.
    ///
    /// A driver that drives the controller interrupt-driven calls this once
    /// after [`Self::start`] and after its device's interrupt has been
    /// routed to it (the MSI capability programmed, the line bound), so an
    /// asserted interrupt has somewhere to be delivered. A poll-only
    /// consumer never calls it and the controller stays interrupt-silent.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register window rejects a write.
    pub fn enable_interrupter(&mut self) -> Result<(), DriverError> {
        let stale_status =
            self.read_op(regs::USBSTS)? & (regs::USBSTS_HSE | regs::USBSTS_EINT | regs::USBSTS_PCD);
        if stale_status != 0 {
            self.write_op(regs::USBSTS, stale_status)?;
            self.read_op(regs::USBSTS)?;
        }
        self.write_ir0(regs::IR_IMOD, regs::IMODI_DEFAULT)?;
        // Write IE together with IP: IP is write-1-to-clear, so this both
        // arms the interrupter and discards any pending bit a prior owner
        // (firmware) left set, without a read-modify-write that could race
        // the controller setting IP.
        self.write_ir0(regs::IR_IMAN, regs::IMAN_IE | regs::IMAN_IP)?;
        let usbcmd = self.read_op(regs::USBCMD)?;
        self.write_op(regs::USBCMD, usbcmd | regs::USBCMD_INTE)
    }

    /// Acknowledge interrupter 0's pending interrupt by clearing the xHCI
    /// global event status and `IMAN.IP`, keeping Interrupt Enable set.
    ///
    /// Called at the **start** of servicing a delivered interrupt, before
    /// the event ring is drained: clearing the global event latch before
    /// `IMAN.IP` follows the controller-defined ordering, and clearing IP
    /// before the drain means a completion the controller posts while the
    /// handler is draining re-sets IP and re-asserts the interrupt, so no
    /// event edge is lost (the drain then advances `ERDP` via
    /// [`Self::ack_event`]). Writing IP as 1 clears it; the companion IE bit
    /// is re-written set so the interrupter stays armed.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register window rejects the write.
    pub fn acknowledge_interrupt(&mut self) -> Result<(), DriverError> {
        self.write_op(regs::USBSTS, regs::USBSTS_EINT)?;
        self.write_ir0(regs::IR_IMAN, regs::IMAN_IE | regs::IMAN_IP)
    }

    /// Reset a root-hub port and wait for it to come back enabled
    /// (§4.19.5 — required before a USB2 device can be addressed).
    ///
    /// The read-modify-write masks the write-1-to-clear bits
    /// ([`regs::PORTSC_RW1C_MASK`]) so no pending change bit is
    /// consumed by accident.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `port` is zero or above
    ///   [`Self::max_ports`].
    /// * [`DriverError::DeviceFault`] if no device is connected, the
    ///   reset never completes within `budget` polls, or the port
    ///   does not come back enabled.
    pub fn reset_port(&mut self, port: u8, budget: u32) -> Result<PortStatus, DriverError> {
        let status = self.port_status(port)?;
        if !status.connected() {
            return Err(DriverError::DeviceFault);
        }
        let offset = regs::PORTSC_BASE + (usize::from(port) - 1) * regs::PORTSC_STRIDE;
        let raw = self.read_op(offset)?;
        self.write_op(offset, (raw & !regs::PORTSC_RW1C_MASK) | regs::PORTSC_PR)?;
        self.wait_op_clear(offset, regs::PORTSC_PR, budget)?;
        let status = self.port_status(port)?;
        if !(status.connected() && status.enabled()) {
            return Err(DriverError::DeviceFault);
        }
        Ok(status)
    }

    /// Read and decode one root-hub port's `PORTSC`.
    ///
    /// `port` is 1-based, as in the xHCI register layout (§5.4.8).
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `port` is zero or above
    ///   [`Self::max_ports`].
    pub fn port_status(&mut self, port: u8) -> Result<PortStatus, DriverError> {
        if port == 0 || port > self.max_ports {
            return Err(DriverError::OutOfRange);
        }
        let offset = regs::PORTSC_BASE + (port as usize - 1) * regs::PORTSC_STRIDE;
        let raw = self.read_op(offset)?;
        Ok(PortStatus::from_raw(raw))
    }

    /// Clear one root-hub port's latched Connect Status Change
    /// ([`regs::PORTSC_CSC`], write-1-to-clear), consuming the latch the
    /// root-port hot-plug scan keys on so the next connect/disconnect
    /// latches — and interrupts — anew.
    ///
    /// The write masks every *other* write-1-to-clear bit
    /// ([`regs::PORTSC_RW1C_MASK`]) so no unrelated pending change is
    /// consumed by accident.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `port` is zero or above
    ///   [`Self::max_ports`].
    /// * [`DriverError::DeviceFault`] if the register window rejects the
    ///   access.
    pub fn clear_port_connect_change(&mut self, port: u8) -> Result<(), DriverError> {
        if port == 0 || port > self.max_ports {
            return Err(DriverError::OutOfRange);
        }
        let offset = regs::PORTSC_BASE + (port as usize - 1) * regs::PORTSC_STRIDE;
        let raw = self.read_op(offset)?;
        self.write_op(offset, (raw & !regs::PORTSC_RW1C_MASK) | regs::PORTSC_CSC)
    }

    /// Assert Port Power ([`regs::PORTSC_PP`]) on root-hub `port`.
    ///
    /// The Host Controller Reset issued in [`Self::open`] clears every
    /// `PORTSC`, and a port-power-controlled controller (`HCCPARAMS1`
    /// PPC = 1 — the Pi 4's VL805) reports `PP` = 0 and never asserts
    /// Current Connect Status until software powers the port on
    /// (xHCI 1.2 §4.19.1.1 / §5.4.8). Without this an attached device
    /// is invisible to [`Self::port_status`]. The read-modify-write
    /// masks the write-1-to-clear bits ([`regs::PORTSC_RW1C_MASK`]) so
    /// no pending change bit is consumed (as [`Self::reset_port`]).
    /// Writing `PP` to an already-powered or non-controlled port is a
    /// no-op the hardware ignores, so this is safe to call on every
    /// reported port.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `port` is zero or above
    ///   [`Self::max_ports`].
    /// * [`DriverError::DeviceFault`] if the register window rejects
    ///   the access.
    pub fn set_port_power(&mut self, port: u8) -> Result<(), DriverError> {
        if port == 0 || port > self.max_ports {
            return Err(DriverError::OutOfRange);
        }
        let offset = regs::PORTSC_BASE + (usize::from(port) - 1) * regs::PORTSC_STRIDE;
        let raw = self.read_op(offset)?;
        if raw & regs::PORTSC_PP == 0 {
            self.write_op(offset, (raw & !regs::PORTSC_RW1C_MASK) | regs::PORTSC_PP)?;
        }
        Ok(())
    }

    /// Ring a doorbell: `index` 0 with `target` 0 notifies the
    /// command ring; `index` 1..=`MaxSlots` with `target` 1..=31
    /// notifies a device endpoint.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `index` exceeds
    ///   [`Self::max_slots`], if the command doorbell carries a
    ///   non-zero `target`, or if a device doorbell's `target` is zero
    ///   or above 31.
    pub fn ring_doorbell(&mut self, index: u8, target: u32) -> Result<(), DriverError> {
        if index > self.max_slots {
            return Err(DriverError::OutOfRange);
        }
        let valid = if index == 0 {
            target == 0
        } else {
            (1..=DOORBELL_TARGET_MAX).contains(&target)
        };
        if !valid {
            return Err(DriverError::OutOfRange);
        }
        // Order every TRB/ring write the caller published ahead of the
        // doorbell store that hands them to the controller. Without this the
        // controller can observe the doorbell before the just-written TRBs on
        // non-coherent DMA memory and act on stale ring contents — the metal
        // failure this fixes (see `tairix_dma_barrier`).
        tairix_dma_barrier::dma_wmb();
        self.host
            .write32(self.db_base + usize::from(index) * 4, target)
    }
}

/// Low 32 bits of a 64-bit register value.
const fn low_dword(value: u64) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// High 32 bits of a 64-bit register value.
const fn high_dword(value: u64) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]])
}
