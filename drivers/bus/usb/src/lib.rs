//! RustOS `xHCI` USB host-controller driver (protocol layers).
//!
//! The Pi 4 reaches its USB-A ports through a `VL805` `PCIe` `xHCI`
//! controller (`plans/PI.md` P10). This crate carries the
//! host-provable `xHCI` layers: the register vocabulary ([`regs`]), the
//! TRB vocabulary ([`trb`]), the ring state machines ([`ring`]), and
//! the controller bring-up sequence ([`Xhci::open`] — `xHCI` 1.2 §4.2:
//! wait ready, halt, reset). Device enumeration (slots, control
//! transfers, HID endpoint wiring to `drivers/input/usb_hid`) builds on
//! these layers in the next P10 increments.
//!
//! # Layered seam
//!
//! Every controller access goes through the [`XhciHost`] register seam,
//! not a concrete memory mapping. Metal drives it over a
//! capability-gated [`RegisterWindow`] whose base the hardware tree
//! discovered (PCI BAR assignment — never a compiled-in constant,
//! `AGENTS.md` §18.1); host tests drive it over a register-level mock
//! controller. This mirrors the `emmc2` `SdhciHost` seam (`AGENTS.md`
//! §2.2): the bring-up and ring protocol is proven host-side, the
//! doorbell below it on metal.
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`Xhci`] is a public *type* the driver host instantiates through
//! [`Xhci::open`] over the discovered register window; the host never
//! reaches into it beyond the methods below.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; mapping the discovered
//! register window additionally requires [`CapabilityId::MMIO_MAP`]
//! (checked by the wiring that mints the [`RegisterWindow`]). The
//! driver runs in user space and does not request `CAP_DRV_KERNEL`
//! (`AGENTS.md` §4 / §8).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::mmio::WindowError;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, RegisterWindow};

pub mod regs;
pub mod ring;
pub mod trb;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"XHCI"` with a version nibble, matching the other
/// drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5848_4349_0000_0001;

/// Upper bound on register polls while waiting for a controller event.
///
/// A bound on a *defence* against an unresponsive or absent controller,
/// not a scalable capacity (`AGENTS.md` §24.4): controller ready, halt,
/// and reset each complete within milliseconds on real silicon, so a
/// million polls is orders of magnitude past any honest completion.
/// Exceeding it fails closed with [`DriverError::DeviceFault`] rather
/// than spinning forever (`AGENTS.md` §2.1).
pub const DEFAULT_POLL_BUDGET: u32 = 1_000_000;

/// Highest doorbell target value (§5.6: endpoint IDs 1..=31 for device
/// doorbells; 0 is the command-ring target on doorbell 0).
const DOORBELL_TARGET_MAX: u32 = 31;

/// Driver entry point (`AGENTS.md` §8).
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
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

/// The `xHCI` register-access seam.
///
/// Every controller access the [`Xhci`] engine makes goes through this
/// trait, so the bring-up state machine is proven host-side against a
/// register-level mock (`AGENTS.md` §2.2). Both methods take
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

/// An `xHCI` controller brought to the halted, freshly reset state.
///
/// [`Xhci::open`] validates the capability block, then runs the §4.2
/// initialisation prologue: wait for Controller Not Ready to clear,
/// halt a running controller, and issue a Host Controller Reset. The
/// controller is left halted; programming DCBAAP/CRCR and starting it
/// belongs to the enumeration increment that brings the DMA memory.
pub struct Xhci<H: XhciHost> {
    host: H,
    op_base: usize,
    db_base: usize,
    rt_base: usize,
    hci_version: u16,
    max_slots: u8,
    max_ports: u8,
    ac64: bool,
    csz: bool,
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
    pub fn open_with_budget(mut host: H, budget: u32) -> Result<Self, DriverError> {
        let cap = host.read32(regs::CAPLENGTH_HCIVERSION)?;
        let caplength = regs::caplength(cap);
        let hci_version = regs::hciversion(cap);
        if caplength < regs::CAPLENGTH_MIN
            || hci_version < regs::HCIVERSION_MIN
            || hci_version == u16::MAX
        {
            return Err(DriverError::DeviceFault);
        }
        let structural = host.read32(regs::HCSPARAMS1)?;
        let max_slots = regs::hcsparams1_max_slots(structural);
        let max_ports = regs::hcsparams1_max_ports(structural);
        if max_slots == 0 || max_ports == 0 {
            return Err(DriverError::DeviceFault);
        }
        let capability = host.read32(regs::HCCPARAMS1)?;
        let db_off = host.read32(regs::DBOFF)? & regs::DBOFF_MASK;
        let rt_off = host.read32(regs::RTSOFF)? & regs::RTSOFF_MASK;
        if db_off == 0 || rt_off == 0 {
            return Err(DriverError::DeviceFault);
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
            ac64: regs::hccparams1_ac64(capability),
            csz: regs::hccparams1_csz(capability),
        };

        // §4.2: wait until Controller Not Ready clears before touching
        // the operational registers.
        xhci.wait_status(regs::USBSTS_CNR, false, budget)?;
        // Halt a running controller before resetting it (§5.4.1.1).
        let usbcmd = xhci.read_op(regs::USBCMD)?;
        if usbcmd & regs::USBCMD_RUN != 0 {
            xhci.write_op(regs::USBCMD, usbcmd & !regs::USBCMD_RUN)?;
        }
        xhci.wait_status(regs::USBSTS_HCH, true, budget)?;
        // Host Controller Reset: self-clearing on completion, after
        // which CNR must also clear before further programming.
        xhci.write_op(regs::USBCMD, regs::USBCMD_HCRST)?;
        xhci.wait_op_clear(regs::USBCMD, regs::USBCMD_HCRST, budget)?;
        xhci.wait_status(regs::USBSTS_CNR, false, budget)?;
        Ok(xhci)
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

    /// Byte offset of the runtime register block within the window
    /// (`RTSOFF`), for the event-ring wiring that follows this slice.
    #[must_use]
    pub const fn runtime_base(&self) -> usize {
        self.rt_base
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

    /// Ring a doorbell (§5.6): `index` 0 with `target` 0 notifies the
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
        self.host
            .write32(self.db_base + usize::from(index) * 4, target)
    }
}
