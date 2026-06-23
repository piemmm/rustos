//! Unit tests for the VL805 firmware policy: the §8 `register` gate, the
//! §18.3 `BIND_KEYS` match table, and the firmware-reset policy run over a
//! [`MailboxChannel`].
//!
//! QEMU models no `VideoCore`, so the policy is proven against the
//! protocol-faithful `lib/vcmailbox` mock firmware behind a test channel
//! (`AGENTS.md` §2.1 / §2.2); the live doorbell is the on-metal acceptance
//! item (`plans/PI.md` Increment C).

use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
use rustos_abi::{CapabilityId, DriverError, DriverHost, DriverKind, HwMatchKey};

use rustos_vcmailbox::mock::MockFirmware;

use super::{
    probe_firmware_revision, register, reload_firmware, FirmwareResetFailure, FirmwareResetOutcome,
    BIND_KEYS, BIND_PRIORITY, VL805_FIRMWARE_DEV_ADDR, VL805_PCI_DEVICE, VL805_PCI_VENDOR,
};

/// Mock driver host modelling the load-time `CAP_DRV_LOAD` grant.
struct MockHost {
    drv_load: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => self.drv_load,
            _ => false,
        }
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

/// A channel backed by the protocol-faithful mock firmware: it answers
/// every property message exactly as a healthy `VideoCore` would.
struct MockChannel(MockFirmware);

impl MailboxChannel for MockChannel {
    fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        self.0.respond(message);
        Ok(())
    }
}

/// A channel whose transport always fails closed with `err`, modelling a
/// dead doorbell / timed-out exchange.
struct FailingChannel(DriverError);

impl MailboxChannel for FailingChannel {
    fn exchange(&self, _message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        Err(self.0)
    }
}

/// A channel that returns `Ok` but leaves the request buffer untouched, so
/// the response header is never stamped (modelling firmware that drops the
/// tag): the decode must reject it as malformed, fail closed.
struct SilentChannel;

impl MailboxChannel for SilentChannel {
    fn exchange(&self, _message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        Ok(())
    }
}

#[test]
fn register_requires_drv_load() {
    assert!(register(&MockHost { drv_load: true }).is_ok());
    assert_eq!(
        register(&MockHost { drv_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn bind_table_matches_the_vl805_by_exact_vendor_device() {
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    // The discovered VL805 node (class is the xHCI prog-if), matched by
    // the exact vendor:device id with a class wildcard.
    let vl805 = HwMatchKey::pci(VL805_PCI_VENDOR, VL805_PCI_DEVICE, 0x0C_03_30);
    assert!(BIND_KEYS[0].key.matches(&vl805));
    // A different vendor's xHCI controller must not match this
    // device-specific driver (it binds the generic xHCI driver instead).
    let other = HwMatchKey::pci(0x8086, 0x1111, 0x0C_03_30);
    assert!(!BIND_KEYS[0].key.matches(&other));
}

#[test]
fn reload_firmware_succeeds_over_a_healthy_mailbox() {
    let channel = MockChannel(MockFirmware::healthy());
    // The mock echoes the request value word, so a healthy firmware
    // reports the device address it was asked to act on.
    assert_eq!(
        reload_firmware(&channel),
        FirmwareResetOutcome::Reloaded {
            response_value: VL805_FIRMWARE_DEV_ADDR
        }
    );
}

#[test]
fn probe_firmware_revision_reads_the_revision_word() {
    let channel = MockChannel(MockFirmware::healthy());
    assert_eq!(
        probe_firmware_revision(&channel),
        Ok(MockFirmware::healthy().firmware_revision)
    );
}

#[test]
fn reload_firmware_fails_closed_on_a_transport_fault() {
    // A dead doorbell surfaces as `DeviceFault`, re-derived to a timeout.
    let channel = FailingChannel(DriverError::DeviceFault);
    assert_eq!(
        reload_firmware(&channel),
        FirmwareResetOutcome::Failed {
            reason: FirmwareResetFailure::Timeout
        }
    );
}

#[test]
fn probe_fails_closed_on_a_transport_fault() {
    let channel = FailingChannel(DriverError::OutOfRange);
    assert_eq!(
        probe_firmware_revision(&channel),
        Err(FirmwareResetFailure::Window)
    );
}

#[test]
fn reload_firmware_fails_closed_on_an_unhonoured_tag() {
    // The firmware "answered" but never stamped the response: the decode
    // must reject it rather than report a reload that never happened.
    let channel = SilentChannel;
    assert_eq!(
        reload_firmware(&channel),
        FirmwareResetOutcome::Failed {
            reason: FirmwareResetFailure::MalformedResponse
        }
    );
}
