//! RustOS USB-HID boot-protocol input driver (keyboard + mouse).
//!
//! This crate is the **driver**: the loadable-module identity for a USB
//! HID boot keyboard or mouse — the single [`register`] entry point and the
//! [`BIND_KEYS`] bind table `devmgr` matches a discovered HID node
//! against. All the reusable protocol logic — the boot-report decoders, the
//! console-input producer, and the arch-neutral xHCI boot-keyboard
//! orchestration — lives in the `rustos_hid` library, so it is shared by
//! both the in-kernel keyboard scaffold (transitional) and the user-space
//! keyboard driver process (`drivers/input/usb_kbd`) without a
//! `drivers/*`→`drivers/*` dependency, exactly as
//! the bus-agnostic xHCI protocol lives in `rustos_usb` rather than the xHCI
//! driver.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; the device's reports are
//! decoded by the `rustos_hid` decoders the loader wires over the
//! interrupt-IN endpoint. The driver runs in user space and does not request
//! `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"UHID"` with a version nibble, matching the other
/// drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5548_4944_0000_0001;

/// The 24-bit USB class code of an HID **boot keyboard** interface:
/// class `0x03` (HID), sub-class `0x01` (boot), protocol `0x01`
/// (keyboard) — see [`HwMatchKey::usb`].
const HID_BOOT_KEYBOARD_CLASS: u32 = 0x03_01_01;

/// The 24-bit USB class code of an HID **boot mouse** interface:
/// class `0x03` (HID), sub-class `0x01` (boot), protocol `0x02` (mouse).
const HID_BOOT_MOUSE_CLASS: u32 = 0x03_01_02;

/// The bind priority [`BIND_KEYS`] carries.
///
/// This driver binds the HID boot classes regardless of vendor/product
/// (the wildcard match of [`HwMatchKey::matches`]), so it ranks below a
/// vendor-specific HID driver naming an exact device id,.
const BIND_PRIORITY: u16 = 5;

/// This driver's hardware bind table.
///
/// It binds any HID **boot-protocol** keyboard or mouse interface by
/// class alone (vendor/product wildcard), so any such device enumerated
/// behind a USB host autoloads this driver without its device id being
/// hard-coded. This `const` is the single
/// source of truth the driver's signed-manifest bind table is authored
/// from and the data `devmgr` resolves a discovered HID node against once
/// the USB enumeration emits it into the hardware tree (PLAN Stage 4.HW
/// increment 5).
pub const BIND_KEYS: &[DriverBindKey] = &[
    DriverBindKey::new(
        BIND_PRIORITY,
        HwMatchKey::usb(0, 0, HID_BOOT_KEYBOARD_CLASS),
    ),
    DriverBindKey::new(BIND_PRIORITY, HwMatchKey::usb(0, 0, HID_BOOT_MOUSE_CLASS)),
];

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
