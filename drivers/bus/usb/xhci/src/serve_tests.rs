//! Host tests for the HCD's per-interface URB-service state machine
//! ([`super::UrbService`]) and the interface-node grant builder
//! ([`super::attach_transport_grants`]).
//!
//! The state machine is driven over a mock [`UrbEngine`] and a heap "shared
//! buffer" standing in for the U3a2 region both the HCD and the class driver
//! map. The live wait-set loop that calls these on real `call_recv`/`irq_wait`
//! wake-ups is in `main.rs` and is the on-metal acceptance item.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use super::{attach_transport_grants, UrbOutcome, UrbService};
use rustos_abi::hwtree::{HwResourceKind, HW_NODE_ROOT};
use rustos_abi::usb_urb::{
    decode_completion, UrbRequest, UsbDirection, UsbTransferType, URB_REQUEST_LEN,
};
use rustos_abi::{DriverError, Errno, HwDeviceClass, HwMatchKey, HwNode};
use rustos_usb::transport::UrbEngine;

/// An arbitrary shared-buffer handle the URB names; the state machine uses the
/// `shm` slice it is handed, not this value.
const BUFFER_HANDLE: u64 = 0x0BAD_F00D_0000_0001;

/// A controllable [`UrbEngine`] double: control-IN copies a fixed response,
/// interrupt-IN delivers queued reports once each, then "nothing pending",
/// and the bulk pair mirrors the live engine's arm-then-reap shape (first
/// drive arms and returns `None`, a later drive completes from the queued
/// device data). Records its call counts so a test can prove a rejected URB
/// never reaches it.
struct MockEngine {
    control_response: Vec<u8>,
    reports: Vec<Vec<u8>>,
    interrupt_fault: Option<DriverError>,
    control_calls: usize,
    interrupt_calls: usize,
    /// Queued device responses for bulk-IN, delivered one per completed TD.
    bulk_in_data: Vec<Vec<u8>>,
    /// An armed bulk-IN TD's requested length, `None` when idle.
    bulk_in_armed: Option<usize>,
}

/// The mock interface's bulk-IN endpoint number.
const BULK_IN_ENDPOINT: u8 = 2;

impl MockEngine {
    fn new() -> Self {
        Self {
            control_response: Vec::new(),
            reports: Vec::new(),
            interrupt_fault: None,
            control_calls: 0,
            interrupt_calls: 0,
            bulk_in_data: Vec::new(),
            bulk_in_armed: None,
        }
    }
}

impl UrbEngine for MockEngine {
    fn control_in(&mut self, _setup: [u8; 8], data: &mut [u8]) -> Result<usize, DriverError> {
        self.control_calls += 1;
        let n = self.control_response.len().min(data.len());
        data[..n].copy_from_slice(&self.control_response[..n]);
        Ok(n)
    }

    fn control_no_data(&mut self, _setup: [u8; 8]) -> Result<(), DriverError> {
        // No serve-level test drives a no-data control-OUT through this
        // double yet; the seam-level round-trip lives in
        // `rustos_usb::transport::tests`.
        Err(DriverError::NotFound)
    }

    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<Option<usize>, DriverError> {
        self.interrupt_calls += 1;
        if let Some(err) = self.interrupt_fault.take() {
            return Err(err);
        }
        if self.reports.is_empty() {
            return Ok(None);
        }
        let report = self.reports.remove(0);
        let n = report.len().min(data.len());
        data[..n].copy_from_slice(&report[..n]);
        Ok(Some(n))
    }

    fn bulk_in(&mut self, endpoint: u8, data: &mut [u8]) -> Result<Option<usize>, DriverError> {
        if endpoint != BULK_IN_ENDPOINT {
            return Err(DriverError::OutOfRange);
        }
        if self.bulk_in_armed.is_none() {
            self.bulk_in_armed = Some(data.len());
            return Ok(None);
        }
        if self.bulk_in_data.is_empty() {
            return Ok(None);
        }
        self.bulk_in_armed = None;
        let response = self.bulk_in_data.remove(0);
        let n = response.len().min(data.len());
        data[..n].copy_from_slice(&response[..n]);
        Ok(Some(n))
    }

    fn bulk_out(&mut self, _endpoint: u8, _data: &[u8]) -> Result<Option<usize>, DriverError> {
        // No serve-level test drives bulk-OUT through this double yet; the
        // seam-level round-trip lives in `rustos_usb::transport::tests`.
        Err(DriverError::NotFound)
    }
}

/// Encode an interrupt-IN URB on `endpoint` reading `length` bytes.
fn interrupt_urb(endpoint: u8, length: u32) -> Vec<u8> {
    let urb = UrbRequest {
        endpoint,
        transfer_type: UsbTransferType::Interrupt,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length,
        setup: [0; 8],
    };
    let mut buf = [0u8; URB_REQUEST_LEN];
    let n = urb.encode(&mut buf).expect("encodes");
    buf[..n].to_vec()
}

/// Decode the completion carried by a [`UrbOutcome::Reply`].
fn reply_result(outcome: &UrbOutcome) -> Result<u32, Errno> {
    match outcome {
        UrbOutcome::Reply(reply) => decode_completion(&reply.bytes[..reply.len]),
        other => panic!("expected a Reply, got {other:?}"),
    }
}

#[test]
fn an_interrupt_in_is_held_until_a_controller_event_completes_it() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();
    let request = interrupt_urb(1, 8);

    // No report queued yet: the submit is held outstanding, not replied.
    let outcome = service.on_submit(true, 0x11, &request, &mut shm, &mut engine);
    assert_eq!(outcome, UrbOutcome::Held);
    assert!(service.is_busy());
    assert_eq!(engine.interrupt_calls, 1);

    // The report arrives; the next controller event completes the held URB,
    // landing the bytes in the shared buffer and replying to its ticket.
    let report = vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
    engine.reports = vec![report.clone()];
    let outcome = service.on_event(&mut shm, &mut engine);
    match outcome {
        UrbOutcome::Reply(reply) => {
            assert_eq!(reply.ticket, 0x11);
            assert_eq!(decode_completion(&reply.bytes[..reply.len]), Ok(8));
        }
        other => panic!("expected a Reply, got {other:?}"),
    }
    assert_eq!(&shm[..8], &report[..]);
    assert!(!service.is_busy());
}

/// Encode a bulk-IN URB on `endpoint` reading `length` bytes.
fn bulk_in_urb(endpoint: u8, length: u32) -> Vec<u8> {
    let urb = UrbRequest {
        endpoint,
        transfer_type: UsbTransferType::Bulk,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length,
        setup: [0; 8],
    };
    let mut buf = [0u8; URB_REQUEST_LEN];
    let n = urb.encode(&mut buf).expect("encodes");
    buf[..n].to_vec()
}

#[test]
fn a_bulk_in_is_held_until_a_controller_event_completes_it() {
    // Bulk completions are delivered asynchronously exactly like interrupt
    // completions: the submit arms the TD and is held, the controller event
    // reaps it and replies with the device's bytes in the shared buffer.
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 16];
    let mut service = UrbService::new();
    let request = bulk_in_urb(BULK_IN_ENDPOINT, 16);

    let outcome = service.on_submit(true, 0x21, &request, &mut shm, &mut engine);
    assert_eq!(outcome, UrbOutcome::Held);
    assert!(service.is_busy());

    let payload = vec![0xC3u8; 16];
    engine.bulk_in_data = vec![payload.clone()];
    let outcome = service.on_event(&mut shm, &mut engine);
    match outcome {
        UrbOutcome::Reply(reply) => {
            assert_eq!(reply.ticket, 0x21);
            assert_eq!(decode_completion(&reply.bytes[..reply.len]), Ok(16));
        }
        other => panic!("expected a Reply, got {other:?}"),
    }
    assert_eq!(&shm[..16], &payload[..]);
    assert!(!service.is_busy());
}

#[test]
fn a_control_in_is_replied_synchronously() {
    let mut engine = MockEngine::new();
    engine.control_response = vec![0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40];
    let mut shm = vec![0u8; 64];
    let mut service = UrbService::new();

    let urb = UrbRequest {
        endpoint: 0,
        transfer_type: UsbTransferType::Control,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length: 8,
        setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00],
    };
    let mut buf = [0u8; URB_REQUEST_LEN];
    let n = urb.encode(&mut buf).expect("encodes");

    let outcome = service.on_submit(true, 0x22, &buf[..n], &mut shm, &mut engine);
    assert_eq!(reply_result(&outcome), Ok(8));
    // A control transfer completes within the call — never left outstanding.
    assert!(!service.is_busy());
    assert_eq!(&shm[..8], &engine.control_response[..]);
    assert_eq!(engine.control_calls, 1);
}

#[test]
fn a_second_submit_while_one_is_outstanding_is_rejected_without_displacing_it() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();
    let request = interrupt_urb(1, 8);

    // First submit is held (no report yet).
    assert_eq!(
        service.on_submit(true, 0x11, &request, &mut shm, &mut engine),
        UrbOutcome::Held
    );
    // A second concurrent submit is fail-closed `AlreadyExists` and does not
    // touch the engine or the in-flight URB.
    let before = engine.interrupt_calls;
    let outcome = service.on_submit(true, 0x22, &request, &mut shm, &mut engine);
    assert_eq!(reply_result(&outcome), Err(Errno::AlreadyExists));
    assert_eq!(engine.interrupt_calls, before);
    assert!(service.is_busy());

    // The first URB's ticket — not the rejected one — is completed by the
    // event.
    engine.reports = vec![vec![1, 2, 3, 4, 5, 6, 7, 8]];
    match service.on_event(&mut shm, &mut engine) {
        UrbOutcome::Reply(reply) => assert_eq!(reply.ticket, 0x11),
        other => panic!("expected a Reply to the held ticket, got {other:?}"),
    }
}

#[test]
fn aborting_an_outstanding_urb_replies_and_unblocks_a_replugged_driver() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();
    let request = interrupt_urb(1, 8);

    assert_eq!(
        service.on_submit(true, 0x11, &request, &mut shm, &mut engine),
        UrbOutcome::Held
    );

    let outcome = service.abort_outstanding(Errno::NotFound);
    match outcome {
        UrbOutcome::Reply(reply) => {
            assert_eq!(reply.ticket, 0x11);
            assert_eq!(
                decode_completion(&reply.bytes[..reply.len]),
                Err(Errno::NotFound)
            );
        }
        other => panic!("expected a Reply to the aborted ticket, got {other:?}"),
    }
    assert!(!service.is_busy());

    assert_eq!(
        service.on_submit(true, 0x22, &request, &mut shm, &mut engine),
        UrbOutcome::Held
    );
}

#[test]
fn disconnect_abort_wins_over_a_stale_transfer_fault() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();
    let request = interrupt_urb(1, 8);

    assert_eq!(
        service.on_submit(true, 0x11, &request, &mut shm, &mut engine),
        UrbOutcome::Held
    );
    engine.interrupt_fault = Some(DriverError::DeviceFault);

    let outcome = service.abort_outstanding(Errno::NotFound);
    assert_eq!(reply_result(&outcome), Err(Errno::NotFound));
    assert!(!service.is_busy());
    assert_eq!(service.on_event(&mut shm, &mut engine), UrbOutcome::Idle);
    assert_eq!(
        engine.interrupt_calls, 1,
        "disconnect abort must not drain a stale transfer fault after replying NotFound"
    );
}

#[test]
fn submit_after_interface_removal_is_rejected_without_touching_the_engine() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();
    let request = interrupt_urb(1, 8);

    let outcome = service.on_submit(false, 0x33, &request, &mut shm, &mut engine);
    match outcome {
        UrbOutcome::Reply(reply) => {
            assert_eq!(reply.ticket, 0x33);
            assert_eq!(
                decode_completion(&reply.bytes[..reply.len]),
                Err(Errno::NotFound)
            );
        }
        other => panic!("expected a NotFound reply for an absent interface, got {other:?}"),
    }
    assert!(!service.is_busy());
    assert_eq!(engine.interrupt_calls, 0);
}

#[test]
fn an_illegal_urb_is_replied_fail_closed_without_reaching_the_engine() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();

    // A bulk transfer must target a device endpoint, never the shared
    // control endpoint.
    let urb = UrbRequest {
        endpoint: 0,
        transfer_type: UsbTransferType::Bulk,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length: 8,
        setup: [0; 8],
    };
    let mut buf = [0u8; URB_REQUEST_LEN];
    let n = urb.encode(&mut buf).expect("encodes");

    let outcome = service.on_submit(true, 0x33, &buf[..n], &mut shm, &mut engine);
    assert_eq!(reply_result(&outcome), Err(Errno::OutOfRange));
    assert!(!service.is_busy());
    assert_eq!(engine.control_calls, 0);
    assert_eq!(engine.interrupt_calls, 0);
}

#[test]
fn a_controller_event_with_nothing_outstanding_is_idle() {
    let mut engine = MockEngine::new();
    let mut shm = vec![0u8; 8];
    let mut service = UrbService::new();
    assert_eq!(service.on_event(&mut shm, &mut engine), UrbOutcome::Idle);
    assert_eq!(engine.interrupt_calls, 0);
}

#[test]
fn attach_transport_grants_adds_the_endpoint_and_shared_resources() {
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input);
    node.push_match_key(HwMatchKey::usb(0x046d, 0xc31c, 0x03_01_01))
        .expect("usb match key");

    let node = attach_transport_grants(node, 0xD012_5701, 0x5147).expect("attaches both grants");

    let kinds: Vec<Option<HwResourceKind>> = node
        .resources()
        .iter()
        .map(rustos_abi::HwResource::kind)
        .collect();
    assert!(kinds.contains(&Some(HwResourceKind::Endpoint)));
    assert!(kinds.contains(&Some(HwResourceKind::Shared)));
}
