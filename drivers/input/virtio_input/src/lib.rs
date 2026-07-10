//! RustOS virtio-input driver (keyboard / pointer) — the entry.
//!
//! This crate is the thin driver shell: the only
//! public *function* is [`register`], and [`BIND_KEYS`] is the
//! bind table `devmgr` (or the in-kernel bootstrap-floor catalogue)
//! resolves a discovered virtio-input node against. The arch-neutral,
//! transport-agnostic open/poll/decode device logic lives in
//! `lib/virtio_input` (`rustos_virtio_input`) so both this driver and
//! the user-space input-driver process compose it without a
//! `drivers/*`→`drivers/*` dependency (the
//! virtio analogue of `lib/hid` ↔ `drivers/input/usb_kbd`).
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`].

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use rustos_virtio_input::VIRTIO_INPUT_DEVICE_ID;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_5054_0000_0001; // "VNPT" (Virtio iNPuT)

/// The bind priority [`BIND_KEYS`] carries.
///
/// A virtio device-id match is *exact* (the discovered node's probed
/// device id either is `virtio-input` or it is not — there is no
/// wildcard, see [`HwMatchKey::matches`]), so it ranks at the
/// exact-match tier alongside the other concrete-identity drivers
/// (higher matched priority binds; an unbroken tie
/// is a packaging defect).
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: a virtio input
/// device, matched by its virtio device id ([`VIRTIO_INPUT_DEVICE_ID`]).
///
/// The single source of truth the signed-manifest bind table is authored
/// from and `devmgr` (or the in-kernel bootstrap-floor catalogue)
/// resolves a discovered node against. The
/// match key carries no transport (PCI vs MMIO) detail: the same driver
/// binds a virtio-input device however it is attached, because the
/// bus-agnostic transport abstracts the bus.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID),
)];

/// Driver entry point.
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

#[cfg(test)]
mod tests;
