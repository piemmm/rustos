//! RustOS Raspberry Pi 4 (BCM2711) VL805 xHCI USB host-controller
//! device-support library.
//!
//! The Pi 4's USB-A ports hang off a VIA VL805 PCIe-to-USB3 xHCI host
//! controller behind the BCM2711 PCIe root complex. On boards without the
//! SPI EEPROM (Pi 4 rev 1.4 and later) the VL805 has **no resident
//! firmware**: the `VideoCore` co-processor loads it at power-on, and a
//! PCIe `PERST#` (which the root-complex bring-up asserts) drops it. Only
//! the `VideoCore` can (re)load it, over a firmware property-channel
//! `NOTIFY_XHCI_RESET` request (`plans/PI.md` P10).
//!
//! That firmware reload is the **one** thing specific to *this device*. It
//! is therefore its own driver, separate from the generic PCIe root-complex
//! driver (`drivers/bus/pcie_brcm` / `lib/pcie_brcm`, which trains the link)
//! and the generic xHCI host-controller engine (`lib/usb`, which brings the
//! controller up and enumerates devices). None of those is a part of
//! another: a different board may need the PCIe driver without USB at all,
//! or an xHCI controller that needs no firmware reload. Keeping them
//! separate is the correct modular shape (`AGENTS.md` §2.2 / §2.20 / §8 /
//! §17.4).
//!
//! # Why a `lib/*` crate
//!
//! This is the §2.20 single-device support carve-out (the `lib/vcmailbox` /
//! `lib/pcie_brcm` precedent): the firmware policy and the controller-node
//! wiring live here so two consumers depend on **one** definition
//! (`AGENTS.md` §2.2) — the autoloaded user-space VL805 bus driver
//! (`drivers/bus/usb/vl805`, which links the userland runtime `rustos-rt`)
//! and the transitional in-kernel keyboard scaffold (`rustos-kernel`). A
//! `rustos-rt`-linking bin cannot share a kernel-linked `drivers/*` crate
//! (the userland `_start`/allocator would enter the kernel graph), so the
//! shared logic lives in `lib/*` and both reach it without a
//! `drivers/*`→`drivers/*` edge (`AGENTS.md` §17.4).
//!
//! # Layering
//!
//! The crate may know the VL805/BCM2711, but it reaches the firmware mailbox
//! **only** through the board-neutral
//! [`MailboxChannel`] seam the
//! host exposes — never a doorbell address, a property-buffer carve, or a
//! `kernel/*` dependency (`AGENTS.md` §17.4). The board specifics (doorbell
//! window, DMA-aliased buffer, cache coherency) stay behind the host's
//! `MailboxChannel` implementation and the `VideoCore` client
//! (`lib/vcmailbox`). The property-message *layout* lives once in
//! `lib/vcmailbox` ([`encode_xhci_reset`] and friends); this crate only
//! sequences the policy (`AGENTS.md` §2.2).
//!
//! # Public surface & capabilities
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`]; the
//! firmware policy is exposed as [`reload_firmware`] and
//! [`probe_firmware_revision`], composed by the host over a
//! [`MailboxChannel`]. The
//! [`wiring`] module composes the firmware reload with publishing the
//! controller as an xHCI hardware-tree node. Loading requires
//! [`CapabilityId::DRV_LOAD`]; the mailbox doorbell/buffer access is gated
//! host-side by the `MailboxChannel` implementation (`AGENTS.md` §5.4). Runs
//! in user space (no `CAP_DRV_KERNEL`).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::mailbox::MailboxChannel;
use rustos_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};

use rustos_vcmailbox::{
    decode_firmware_revision_response, decode_xhci_reset_response, encode_firmware_revision_query,
    encode_xhci_reset, MailboxError,
};

pub mod wiring;

#[cfg(test)]
mod tests;

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"VL805"` (ASCII `V L 8 0 5`) with a version nibble,
/// matching the other drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x564C_3830_3500_0001;

/// The VL805's PCI vendor id (VIA Technologies).
pub const VL805_PCI_VENDOR: u16 = 0x1106;

/// The VL805's PCI device id (VL805 USB 3.0 host controller).
pub const VL805_PCI_DEVICE: u16 = 0x3483;

/// The 24-bit PCI class code of an xHCI USB host controller
/// (`base 0x0C` serial bus, `sub 0x03` USB, `prog-if 0x30` xHCI). The
/// VL805 presents its USB function with this class; the bind key fixes it
/// (the class is matched exactly, never wildcarded — see
/// [`HwMatchKey::matches`]) so an EHCI/OHCI function on the same device id
/// could not bind this driver.
pub const VL805_PCI_CLASS: u32 = 0x0C_03_30;

/// The §18.3 bind priority [`BIND_KEYS`] carries.
///
/// An exact vendor:device match ranks **above** the generic xHCI
/// class-wildcard driver (priority 5) so the VL805 is matched specifically
/// when both could bind a discovered node (`AGENTS.md` §18.3 — bind
/// specificity decides).
const BIND_PRIORITY: u16 = 20;

/// This driver's hardware bind table (`AGENTS.md` §18.3): the VL805 USB
/// host controller, matched by its exact PCI vendor:device id
/// ([`VL805_PCI_VENDOR`]`:`[`VL805_PCI_DEVICE`]). The single source of
/// truth the signed-manifest bind table is authored from and `devmgr`
/// resolves a discovered node against (`AGENTS.md` §2.2).
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::pci(VL805_PCI_VENDOR, VL805_PCI_DEVICE, VL805_PCI_CLASS),
)];

/// The Pi firmware's encoded VL805 PCI address
/// (`bus << 20 | slot << 15 | func << 12`) for the hardwired bus-1,
/// device-0, function-0 controller, carried as the `NOTIFY_XHCI_RESET`
/// request value.
pub const VL805_FIRMWARE_DEV_ADDR: u32 = 0x0010_0000;

/// Stable reason reported when the VL805 firmware reload is refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FirmwareResetFailure {
    /// The mailbox register or property-buffer window was not usable.
    Window,
    /// The firmware mailbox did not complete within the bounded poll budget.
    Timeout,
    /// The firmware returned its top-level error response.
    FirmwareError,
    /// The firmware returned a malformed or unhonoured tag response.
    MalformedResponse,
    /// The property buffer was outside the `VideoCore` DMA aperture.
    BadAperture,
    /// The discovered mailbox or buffer geometry was unusable.
    BadGeometry,
    /// A newer mailbox error reached an older firmware-reset mapper.
    Unknown,
}

impl FirmwareResetFailure {
    /// Stable, allocation-free name for the failure, for the host's
    /// diagnostic log (`AGENTS.md` §2.9 — the log path never allocates).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FirmwareResetFailure::Window => "window",
            FirmwareResetFailure::Timeout => "timeout",
            FirmwareResetFailure::FirmwareError => "firmware_error",
            FirmwareResetFailure::MalformedResponse => "malformed_response",
            FirmwareResetFailure::BadAperture => "bad_aperture",
            FirmwareResetFailure::BadGeometry => "bad_geometry",
            FirmwareResetFailure::Unknown => "unknown",
        }
    }

    /// Map a [`MailboxError`] returned by the property-message *decode*
    /// (driver-side, so the precise reason survives) to a stable reason.
    #[must_use]
    pub const fn from_mailbox_error(err: MailboxError) -> Self {
        match err {
            MailboxError::Window => FirmwareResetFailure::Window,
            MailboxError::Timeout => FirmwareResetFailure::Timeout,
            MailboxError::FirmwareError => FirmwareResetFailure::FirmwareError,
            MailboxError::MalformedResponse => FirmwareResetFailure::MalformedResponse,
            MailboxError::BadAperture => FirmwareResetFailure::BadAperture,
            MailboxError::BadGeometry => FirmwareResetFailure::BadGeometry,
            // `MailboxError` is `#[non_exhaustive]`: a future variant maps
            // to the catch-all rather than silently mismatching.
            _ => FirmwareResetFailure::Unknown,
        }
    }

    /// Map a [`DriverError`] returned by the board-neutral
    /// [`MailboxChannel::exchange`] *transport* to a stable reason.
    ///
    /// The host's channel reports transport failures as the board-neutral
    /// [`DriverError`] (`AGENTS.md` §17.4), so this re-derives the reason
    /// from it; it is the inverse of `MailboxError::as_driver_error` over
    /// the transport-level error set (a `Timeout` is the only thing that
    /// maps to [`DriverError::DeviceFault`] at this stage — a
    /// `FirmwareError` is detected later, in the driver-side decode).
    #[must_use]
    pub const fn from_driver_error(err: DriverError) -> Self {
        match err {
            DriverError::OutOfRange => FirmwareResetFailure::Window,
            DriverError::DeviceFault => FirmwareResetFailure::Timeout,
            DriverError::BadMagic => FirmwareResetFailure::MalformedResponse,
            DriverError::LengthOutOfRange => FirmwareResetFailure::BadAperture,
            _ => FirmwareResetFailure::Unknown,
        }
    }
}

/// Result of one VL805 firmware reload attempt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FirmwareResetOutcome {
    /// No firmware mailbox is available for this boot shape (the host
    /// reported [`None`] from
    /// [`DriverHost::mailbox`]).
    NotAvailable,
    /// The firmware honoured the tag and returned `response_value`
    /// (diagnostic only — a healthy firmware echoes the device address).
    Reloaded {
        /// Diagnostic response value written by the firmware.
        response_value: u32,
    },
    /// The mailbox transport or firmware refused the tag.
    Failed {
        /// Stable failure reason for the host's diagnostic log.
        reason: FirmwareResetFailure,
    },
}

/// Driver entry point (`AGENTS.md` §8).
///
/// Verifies the host already granted [`CapabilityId::DRV_LOAD`] and returns
/// the registration marker handle. No hardware is touched here; the
/// firmware policy runs in [`reload_firmware`] / [`probe_firmware_revision`]
/// over a host-supplied
/// [`MailboxChannel`].
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

/// Probe the runtime mailbox by asking the `VideoCore` for its firmware
/// revision over `channel`.
///
/// A *liveness probe*, not a feature: the firmware-revision tag reads a
/// constant and mutates no device state, so running it immediately before
/// the heavier [`reload_firmware`] localises a failure — a probe failure
/// means the mailbox path itself is broken, while a probe success followed
/// by a reload timeout means the firmware is specifically dropping the
/// reset tag (`AGENTS.md` §15.7 — measure, don't guess). The composing
/// host logs the outcome.
///
/// # Errors
///
/// A [`FirmwareResetFailure`] re-derived from the transport
/// ([`MailboxChannel::exchange`]) or the decode
/// ([`decode_firmware_revision_response`]).
pub fn probe_firmware_revision(channel: &dyn MailboxChannel) -> Result<u32, FirmwareResetFailure> {
    let mut message = encode_firmware_revision_query();
    channel
        .exchange(&mut message)
        .map_err(FirmwareResetFailure::from_driver_error)?;
    decode_firmware_revision_response(&message).map_err(FirmwareResetFailure::from_mailbox_error)
}

/// Ask the `VideoCore` to (re)load the VL805 xHCI controller's firmware
/// over `channel`, returning the [`FirmwareResetOutcome`].
///
/// This is the VL805's device-specific bring-up step: after the PCIe
/// root-complex driver trains the link (which asserts `PERST#` and so may
/// have dropped the VL805's firmware), this reload restores it before the
/// generic xHCI driver brings the controller up. It is fail-closed: an
/// unverified firmware ack is treated as a failure, never a success
/// (`AGENTS.md` §5.4 — the firmware is external input). The response value
/// is diagnostic only; an honoured tag is the success signal.
///
/// The buffer is encoded once via
/// [`encode_xhci_reset`], exchanged
/// over the board-neutral channel, and verified via
/// [`decode_xhci_reset_response`]
/// — the property layout is never re-derived here (`AGENTS.md` §2.2).
///
/// This is best-effort in the boot composition: the authoritative xHCI
/// liveness gate is the controller's capability block at `Xhci::open`
/// (`AGENTS.md` §2.9), so the host logs a failure but need not abort on it.
#[must_use]
pub fn reload_firmware(channel: &dyn MailboxChannel) -> FirmwareResetOutcome {
    let mut message = encode_xhci_reset(VL805_FIRMWARE_DEV_ADDR);
    if let Err(err) = channel.exchange(&mut message) {
        return FirmwareResetOutcome::Failed {
            reason: FirmwareResetFailure::from_driver_error(err),
        };
    }
    match decode_xhci_reset_response(&message) {
        Ok(response_value) => FirmwareResetOutcome::Reloaded { response_value },
        Err(err) => FirmwareResetOutcome::Failed {
            reason: FirmwareResetFailure::from_mailbox_error(err),
        },
    }
}
