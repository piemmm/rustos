//! RustOS memory-mapped-IO bus driver.
//!
//! Enumerates virtio-MMIO transport slots on `virt`-style platforms
//! (QEMU's `aarch64 -M virt` and `riscv64 -M virt` machines). The
//! flat device-tree blob — handed to the driver host by the kernel
//! boot capability — is the single source of truth for slot
//! addresses; the parser used to walk it lives in
//! [`rustos_util::dtb`] so the future platform-discovery code can
//! reuse it without copy-paste (`AGENTS.md` §2.3 / §6, satisfying
//! the two-caller rule today).
//!
//! Per the issue spec for Stage 4 the driver only enumerates; it
//! never enables a slot. Reading the small per-slot register window
//! (`MagicValue` / `Version` / `DeviceID` / `VendorID`) goes through
//! a `transport::MmioRead` trait so the unit tests substitute a
//! deterministic in-memory fake while the production wiring uses
//! the volatile reader defined in `transport::VolatileMmioRead`.
//!
//! # Capabilities
//!
//! [`register`] requires the host already grant
//! [`rustos_abi::CapabilityId::DRV_LOAD`]; enumeration through
//! [`rustos_abi::driver::bus::Bus::enumerate`] inherits the gate
//! through the driver handle issued at load time
//! (`AGENTS.md` §5.4 / §8).
//!
//! # Safety
//!
//! The volatile reader is the only `unsafe` site in the crate; it
//! sits behind the `transport::MmioRead` trait so callers above
//! receive a safe API. Every `unsafe` block carries a `// SAFETY:`
//! justification and is exercised by the
//! `transport::tests::volatile_reader_round_trips_against_fake`
//! unit test on hosts that publish a synthetic backing buffer.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

pub(crate) mod enumerate;
pub(crate) mod transport;

#[cfg(test)]
mod tests;

use enumerate::Mmio;
use transport::MmioRead;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention introduced by `drivers/bus/pci`:
/// the host re-issues a host-local handle when binding the driver
/// into its load table; this constant is the on-the-wire signal
/// that every load-time gate cleared.
const REGISTER_HANDLE_MARKER: u64 = 0x4D4D_4900_0000_0001;

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

impl<T: MmioRead> Bus for Mmio<'_, T> {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        self.enumerate_into(out)
    }
}
