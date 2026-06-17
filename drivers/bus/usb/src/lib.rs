//! RustOS `xHCI` USB host-controller driver.
//!
//! The Pi 4 reaches its USB-A ports through a `VL805` `PCIe` `xHCI`
//! controller (`plans/PI.md` P10). This crate is the concrete,
//! loadable host-controller *driver*: the §8 [`register`] entry, the
//! §18.3 [`BIND_KEYS`] bind table, and the PCI discovery / BAR / DMA
//! [`wiring`] that brings a discovered controller online.
//!
//! The bus-agnostic `xHCI` *protocol* (the [`rustos_usb`] crate — the
//! register vocabulary, TRB vocabulary, ring state machines, the
//! [`Xhci`](rustos_usb::Xhci) controller engine, and the
//! [`UsbDevice`](rustos_usb::device::UsbDevice) enumeration engine)
//! lives in `lib/usb` so this driver and an arch-neutral user-space
//! keyboard driver can both consume it without depending on each other
//! (`AGENTS.md` §17.4 — `drivers/* → lib/*` only; the USB analogue of
//! `lib/virtio` ↔ `drivers/bus/virtio`).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`]; the
//! [`wiring`] module brings the discovered controller online over the
//! [`rustos_usb`] protocol engine.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; mapping the discovered
//! register window additionally requires [`CapabilityId::MMIO_MAP`]
//! (checked by [`wiring`] when it mints the
//! [`RegisterWindow`](rustos_abi::RegisterWindow)). The driver runs in
//! user space and does not request `CAP_DRV_KERNEL` (`AGENTS.md` §4 /
//! §8).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};

pub mod wiring;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"XHCI"` with a version nibble, matching the other
/// drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5848_4349_0000_0001;

/// The 24-bit PCI class code of an xHCI USB host controller:
/// base class `0x0C` (serial bus), sub-class `0x03` (USB), prog-if
/// `0x30` (xHCI) — the prog-if is what distinguishes xHCI from the older
/// OHCI/UHCI/EHCI host classes, so it must be part of the bind key.
const XHCI_PCI_CLASS: u32 = 0x0C_03_30;

/// The §18.3 bind priority [`BIND_KEYS`] carries.
///
/// This driver binds *any* xHCI controller by class alone (vendor/device
/// wildcard, see [`HwMatchKey::matches`]), so it ranks below a
/// vendor-specific driver that names an exact device id, per §18.3.
const BIND_PRIORITY: u16 = 5;

/// This driver's hardware bind table (`AGENTS.md` §18.3).
///
/// A generic xHCI host driver: it binds any PCI function of the xHCI
/// class `0x0C_03_30` (`XHCI_PCI_CLASS`) regardless of vendor/device (the
/// wildcard match of [`HwMatchKey::matches`]), so the Pi 4's VL805 — and any other xHCI
/// controller — autoloads it without the device id being hard-coded
/// (`AGENTS.md` §2.2 / §18.3). This `const` is the single source of truth
/// the driver's signed-manifest bind table is authored from and the data
/// `devmgr` resolves a discovered VL805 node against once the bus-driver
/// enumeration emits it into the hardware tree (PLAN Stage 4.HW
/// increment 5).
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::pci(0, 0, XHCI_PCI_CLASS),
)];

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
