//! Host tests for the VL805 controller-node wiring: building `node B` from
//! the forwarded grants ([`build_xhci_node`]) and the
//! reload-then-publish composition ([`reload_firmware_and_publish`]).
//!
//! QEMU models no `VideoCore` mailbox or Pi USB timing,
//! so these prove the composition and its fail-closed paths against doubles;
//! the live firmware reload is the on-metal acceptance item.

use core::cell::{Cell, RefCell};

use rustos_abi::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
use rustos_abi::{
    CapabilityId, DriverError, DriverHost, DriverKind, HwDeviceClass, HwMatchKey, HwNode,
    HwResource, HwResourceKind,
};
use rustos_usb::{XHCI_COMPATIBLE, XHCI_DMA_BYTES};

use rustos_vcmailbox::mock::MockFirmware;

use super::{build_xhci_node, reload_firmware_and_publish};
use crate::{FirmwareResetFailure, FirmwareResetOutcome, VL805_FIRMWARE_DEV_ADDR};

/// The CPU-physical BAR base + length and the inbound-DMA aperture top the
/// PCIe root-complex driver published on `node A` and the kernel granted
/// this driver. Representative Pi 4 values (the BAR inside the bridge's
/// outbound window, the aperture top the low 3 GiB of SDRAM).
const BAR_CPU_PHYS: u64 = 0x6_0000_0000;
const BAR_LEN: u64 = 0x1000;
const DMA_APERTURE_TOP: u64 = 0xC000_0000;

/// The two grants `node A` carried, in an arbitrary order (the parse must
/// not depend on ordering).
fn node_a_grants() -> [HwResource; 2] {
    [
        HwResource::dma(DMA_APERTURE_TOP, XHCI_DMA_BYTES as u64),
        HwResource::mmio(BAR_CPU_PHYS, BAR_LEN),
    ]
}

/// A [`MailboxChannel`] backed by the protocol-faithful mock firmware.
struct MockChannel(MockFirmware);

impl MailboxChannel for MockChannel {
    fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
        self.0.respond(message);
        Ok(())
    }
}

/// A mock [`DriverHost`] for the VL805 bus driver: the `CAP_MAILBOX` /
/// `CAP_HW_EMIT` capability set, an optional mailbox channel, a programmable
/// `emit_node` result, and a record of the last node published.
struct MockHost {
    caps: &'static [CapabilityId],
    mailbox: Option<MockChannel>,
    emit_result: Result<(), DriverError>,
    emitted: RefCell<Option<HwNode>>,
    emit_calls: Cell<usize>,
}

impl MockHost {
    fn new(mailbox: Option<MockChannel>) -> Self {
        Self {
            caps: &[CapabilityId::MAILBOX, CapabilityId::HW_EMIT],
            mailbox,
            emit_result: Ok(()),
            emitted: RefCell::new(None),
            emit_calls: Cell::new(0),
        }
    }
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.caps.contains(&cap)
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
    fn mailbox(&self) -> Option<&dyn MailboxChannel> {
        self.mailbox.as_ref().map(|c| c as &dyn MailboxChannel)
    }
    fn emit_node(&self, node: HwNode) -> Result<(), DriverError> {
        self.emit_calls.set(self.emit_calls.get() + 1);
        self.emit_result?;
        *self.emitted.borrow_mut() = Some(node);
        Ok(())
    }
}

#[test]
fn build_xhci_node_forwards_the_bar_and_dma_under_the_xhci_compatible_key() {
    let node = build_xhci_node(node_a_grants().iter()).expect("node B builds");

    // Bound by the shared xHCI `compatible` identity the controller driver
    // matches, classed as a bus to further USB devices.
    assert_eq!(node.class(), Some(HwDeviceClass::Bus));
    let key = HwMatchKey::compatible(XHCI_COMPATIBLE).expect("fits");
    assert_eq!(node.match_keys(), &[key]);

    // The BAR + DMA grants are forwarded verbatim (no ambient authority: the
    // bound driver receives exactly what this driver held).
    let bar = node
        .resources()
        .iter()
        .find(|r| r.kind() == Some(HwResourceKind::Mmio))
        .expect("BAR forwarded");
    assert_eq!(bar.base(), BAR_CPU_PHYS);
    assert_eq!(bar.length(), BAR_LEN);
    let dma = node
        .resources()
        .iter()
        .find(|r| r.kind() == Some(HwResourceKind::Dma))
        .expect("DMA forwarded");
    assert_eq!(dma.base(), DMA_APERTURE_TOP);
    assert_eq!(dma.length(), XHCI_DMA_BYTES as u64);
}

#[test]
fn build_xhci_node_fails_closed_without_the_bar_grant() {
    // Only the DMA grant: the controller's register window is missing, so no
    // node is fabricated.
    let dma = [HwResource::dma(DMA_APERTURE_TOP, XHCI_DMA_BYTES as u64)];
    assert_eq!(build_xhci_node(dma.iter()), Err(DriverError::NotFound));
}

#[test]
fn build_xhci_node_fails_closed_without_the_dma_grant() {
    // Only the BAR grant: the inbound-DMA constraint is missing, fail closed.
    let bar = [HwResource::mmio(BAR_CPU_PHYS, BAR_LEN)];
    assert_eq!(build_xhci_node(bar.iter()), Err(DriverError::NotFound));
}

#[test]
fn reload_then_publish_reloads_over_a_healthy_mailbox_and_emits_node_b() {
    let host = MockHost::new(Some(MockChannel(MockFirmware::healthy())));
    let node = build_xhci_node(node_a_grants().iter()).expect("node B builds");
    let outcome = reload_firmware_and_publish(&host, node).expect("publish succeeds");

    // A healthy firmware honoured the reset tag, echoing the device address.
    assert_eq!(
        outcome,
        FirmwareResetOutcome::Reloaded {
            response_value: VL805_FIRMWARE_DEV_ADDR
        }
    );
    // Node B was published exactly once, after the reload (firmware before
    // bring-up holds by construction).
    assert_eq!(host.emit_calls.get(), 1);
    assert!(host.emitted.borrow().is_some());
}

#[test]
fn reload_then_publish_emits_node_b_even_with_no_mailbox() {
    // A boot shape with no VideoCore mailbox: the reload is `NotAvailable`,
    // but the publish still happens — the authoritative liveness gate is the
    // controller driver's `Xhci::open`, not this best-effort reload.
    let host = MockHost::new(None);
    let node = build_xhci_node(node_a_grants().iter()).expect("node B builds");
    let outcome = reload_firmware_and_publish(&host, node).expect("publish succeeds");
    assert_eq!(outcome, FirmwareResetOutcome::NotAvailable);
    assert_eq!(host.emit_calls.get(), 1);
}

#[test]
fn reload_then_publish_fails_closed_when_the_emit_is_refused() {
    // The kernel refuses the publish (e.g. a forwarded resource is not
    // covered by a grant, or `CAP_HW_EMIT` is missing): the composition
    // surfaces the refusal rather than reporting success.
    let mut host = MockHost::new(Some(MockChannel(MockFirmware::healthy())));
    host.emit_result = Err(DriverError::PermissionDenied);
    let node = build_xhci_node(node_a_grants().iter()).expect("node B builds");
    assert_eq!(
        reload_firmware_and_publish(&host, node),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn reload_then_publish_surfaces_a_firmware_failure_but_still_emits() {
    // A mailbox present but the firmware drops the reset tag: the outcome is
    // `Failed`, yet node B is still published (best-effort reload).
    struct SilentChannel;
    impl MailboxChannel for SilentChannel {
        fn exchange(
            &self,
            _message: &mut [u32; MAILBOX_PROPERTY_WORDS],
        ) -> Result<(), DriverError> {
            Ok(())
        }
    }
    struct SilentHost {
        emitted: Cell<usize>,
    }
    impl DriverHost for SilentHost {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            matches!(cap, CapabilityId::MAILBOX | CapabilityId::HW_EMIT)
        }
        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }
        fn mailbox(&self) -> Option<&dyn MailboxChannel> {
            Some(&SilentChannel)
        }
        fn emit_node(&self, _node: HwNode) -> Result<(), DriverError> {
            self.emitted.set(self.emitted.get() + 1);
            Ok(())
        }
    }

    let host = SilentHost {
        emitted: Cell::new(0),
    };
    let node = build_xhci_node(node_a_grants().iter()).expect("node B builds");
    let outcome = reload_firmware_and_publish(&host, node).expect("publish succeeds");
    assert_eq!(
        outcome,
        FirmwareResetOutcome::Failed {
            reason: FirmwareResetFailure::MalformedResponse
        }
    );
    assert_eq!(host.emitted.get(), 1);
}
