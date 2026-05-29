//! RustOS PCI/PCIe bus driver.
//!
//! Implements the bus enumeration class trait
//! ([`rustos_abi::driver::bus::Bus`]) on top of the x86_64
//! configuration-access **mechanism #1** (PCI Local Bus 3.0 §3.2.2.3.2):
//! the 32-bit configuration address word at I/O port `0xCF8` selects
//! a `(bus, device, function, register)` tuple and the 32-bit data
//! word at I/O port `0xCFC` reads or writes the corresponding
//! configuration dword.
//!
//! Per `AGENTS.md` §8 the only public surface of a driver crate is
//! `pub fn register(host) -> Result<DriverHandle, DriverError>`.
//! Everything below is intentionally `pub(crate)` and tested through
//! the in-crate `#[cfg(test)]` module against a mock
//! `ConfigSpace` fixture that mirrors QEMU's `q35` default PCI
//! topology (LPC bridge, `SMBus` controller, plus the virtio-net
//! function the driver-host integration test will attach later).
//!
//! Per the Stage 4 sub-bullet on bus drivers in `PLAN.md`, MSI / MSI-X
//! capabilities are *discovered* but never enabled here — actual
//! interrupt routing is the responsibility of the `virtio_blk` /
//! `virtio_net` drivers in Stage 4.D. The BAR walker likewise
//! produces `BarDescriptor` records but never invokes the kernel
//! memory capability: callers route the mapping request through the
//! driver host once 4.D wires up the host-side memory facility.
//!
//! # Safety
//!
//! The real-hardware `ConfigSpace` implementation
//! (`mech_one::PortIoConfigSpace`) issues `in`/`out` instructions
//! against I/O ports `0xCF8`/`0xCFC`. Every `unsafe` block carries a
//! `// SAFETY:` justification and is covered by a unit test against a
//! mock `mech_one::PortIo` implementation.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::virtio_pci::VirtioPciBus;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, MmioMapper, RegisterWindow};

pub(crate) mod config;
pub(crate) mod enumerate;
pub(crate) mod mech_one;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The driver host re-issues a host-local handle when binding this
/// driver into its load table; this constant is the on-the-wire
/// signal that `register` cleared every gate (`AGENTS.md` §8).
const REGISTER_HANDLE_MARKER: u64 = 0x5043_4900_0000_0001;

/// Driver entry point (`AGENTS.md` §8).
///
/// Verifies the host already granted [`CapabilityId::DRV_LOAD`] and
/// returns the registration marker handle. No hardware probe runs
/// here; enumeration is driven by the host once it dispatches into
/// [`Bus::enumerate`] on the per-driver [`Bus`] trait object.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
/// * [`DriverError::OutOfRange`] is impossible by construction: the
///   marker is non-zero.
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

// --- Public re-exports through the `Bus` trait ----------------------------
//
// The trait impl below is the only post-`register` surface a host may
// reach; it is reached through `&dyn Bus`, never through the concrete
// type, satisfying `AGENTS.md` §8.

use config::ConfigSpace;
use enumerate::Pci;

impl<C: ConfigSpace> Bus for Pci<C> {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        self.enumerate_into(out)
    }
}

// The frozen `abi-v1` virtio-PCI transport-provisioning seam
// (`AGENTS.md` §9). The ring-0 boot walk reaches the concrete `Pci`
// through `&dyn VirtioPciBus`, so the bus driver never leaks its
// concrete type across the crate boundary (`AGENTS.md` §8). Both
// methods forward to the inherent enumeration core; the inherent
// `Pci::map_virtio_window` wins method resolution, so the forward is
// not recursive.
impl<C: ConfigSpace> VirtioPciBus for Pci<C> {
    fn map_virtio_window(
        &self,
        bdf: u64,
        cfg_type: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        Pci::map_virtio_window(self, bdf, cfg_type, mapper)
    }

    fn notify_off_multiplier(&self, bdf: u64) -> Result<u32, DriverError> {
        self.virtio_notify_off_multiplier(bdf)
    }
}
