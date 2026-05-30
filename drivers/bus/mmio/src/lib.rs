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
use rustos_abi::driver::virtio_mmio::VirtioMmioBus;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, MmioMapper, RegisterWindow};
use rustos_util::dtb::Dtb;

pub(crate) mod enumerate;
pub(crate) mod transport;

#[cfg(test)]
mod tests;

use enumerate::{Mmio, VIRTIO_MMIO_COMPATIBLE};
use transport::{MmioRead, VolatileMmioRead};

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

// The frozen `abi-v1` virtio-MMIO transport-provisioning seam
// (`AGENTS.md` §9). The ring-0 device-tree walk (whose only sanctioned
// driver surface is `register`, §8) reaches the concrete `Mmio` through
// `&dyn VirtioMmioBus`, so the bus driver never leaks its concrete type
// across the crate boundary (`AGENTS.md` §8). The inherent
// `Mmio::map_slot_window` wins method resolution, so the forward is not
// recursive.
impl<T: MmioRead> VirtioMmioBus for Mmio<'_, T> {
    fn map_slot_window(
        &self,
        base: u64,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        Mmio::map_slot_window(self, base, mapper)
    }
}

// --- `virt`-board construction seam ---------------------------------------

/// The smallest physical span covering every `virtio,mmio` slot the
/// device tree describes: `(base, length)` from the lowest slot base to
/// the highest slot end.
///
/// Returns `Ok(None)` when the tree advertises no `virtio,mmio` node.
/// The bring-up scaffold uses the span to size one volatile reader that
/// covers every slot's identifier window during enumeration (the
/// per-slot driver-facing register window is mapped separately, through
/// the capability-gated [`MmioMapper`], by [`Mmio::map_slot_window`]).
///
/// # Errors
///
/// [`DriverError::DeviceFault`] if a `virtio,mmio` node carries a
/// malformed `reg` property — failing closed exactly as
/// [`enumerate::Mmio::enumerate_into`] does.
fn virtio_mmio_aperture(dtb: &Dtb<'_>) -> Result<Option<(u64, u64)>, DriverError> {
    let mut lo: Option<u64> = None;
    let mut hi: Option<u64> = None;
    for node in dtb.nodes() {
        let node = node.map_err(|_| DriverError::DeviceFault)?;
        if !node.is_compatible(VIRTIO_MMIO_COMPATIBLE) {
            continue;
        }
        let reg = node.property("reg").ok_or(DriverError::DeviceFault)?;
        let base = reg.read_be_u64(0).map_err(|_| DriverError::DeviceFault)?;
        let length = reg.read_be_u64(8).map_err(|_| DriverError::DeviceFault)?;
        let end = base.checked_add(length).ok_or(DriverError::DeviceFault)?;
        lo = Some(lo.map_or(base, |l| l.min(base)));
        hi = Some(hi.map_or(end, |h| h.max(end)));
    }
    match (lo, hi) {
        (Some(base), Some(end)) => Ok(Some((base, end - base))),
        _ => Ok(None),
    }
}

/// Construct the `virt`-board virtio-MMIO bus over a flattened
/// device-tree blob.
///
/// Parses `dtb`, computes the aperture spanning every `virtio,mmio`
/// transport slot, and returns a bus that
/// enumerates those slots and resolves per-slot register windows. The
/// returned value is reached only through the frozen `abi-v1`
/// [`VirtioMmioBus`] / [`Bus`] seams — the concrete `Mmio` type stays
/// crate-private (`AGENTS.md` §8) — so the ring-0
/// `provision_virtio_mmio` walk can drive it as `&dyn VirtioMmioBus`.
///
/// This is the MMIO analogue of `rustos_drv_bus_pci::x86_mechanism_one`:
/// it is the sanctioned way for a `virt`-board boot pipeline to obtain
/// the bus without naming the driver's internals.
///
/// # Errors
///
/// * [`DriverError::DeviceFault`] — the blob is not a valid device tree
///   or a `virtio,mmio` node has a malformed `reg`.
/// * [`DriverError::NotFound`] — the tree advertises no `virtio,mmio`
///   transport slot.
///
/// # Safety
///
/// The MMIO aperture the device tree describes must be identity-mapped
/// and readable for the lifetime of the returned bus, and nothing else
/// may alias it. On the `virt` board entered in S-mode with paging off
/// (`satp == 0`) this holds: physical addresses are accessed directly.
/// The single volatile reader the constructor mints is confined to that
/// aperture and performs only bounds-checked volatile loads.
pub unsafe fn virtio_mmio_bus_from_dtb(dtb: &[u8]) -> Result<impl VirtioMmioBus + '_, DriverError> {
    let parsed = Dtb::parse(dtb).map_err(|_| DriverError::DeviceFault)?;
    let (base, len) = virtio_mmio_aperture(&parsed)?.ok_or(DriverError::NotFound)?;
    // SAFETY: the function-level contract guarantees `[base, base+len)`
    // is the identity-mapped, exclusively-owned virtio-MMIO aperture the
    // device tree describes; `base` is therefore a valid `*const u32`
    // for bounds-checked volatile reads for the bus's lifetime.
    let reader = unsafe { VolatileMmioRead::new(base as *const u32, base, len) };
    Ok(Mmio::new(parsed, reader))
}
