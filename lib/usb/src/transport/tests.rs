//! Unit tests for the URB transport seam: a control-IN and an interrupt-IN
//! round-trip through the client → `serve_urb` → mock engine path, and the
//! fail-closed validation `serve_urb` applies before the engine is ever
//! touched.

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::{serve_urb, UrbCall, UrbClient, UrbEngine};
use rustos_abi::usb_urb::{
    UrbRequest, UsbDirection, UsbTransferType, URB_COMPLETION_LEN, URB_REQUEST_LEN,
};
use rustos_abi::{DriverError, Errno};

/// An arbitrary shared-buffer handle the URB names; the in-process transport
/// ignores it and uses its own shared buffer (it stands in for the mapped
/// shared memory both the class driver and the HCD would see).
const BUFFER_HANDLE: u64 = 0x0BAD_F00D_0000_0001;

/// A controllable [`UrbEngine`] double: a control-IN transfer copies a fixed
/// response into the caller's buffer, and interrupt-IN delivers queued
/// reports once each, then reports "nothing pending". Records its call count
/// so a test can prove a rejected URB never reaches it.
struct MockEngine {
    control_response: Vec<u8>,
    reports: Vec<Vec<u8>>,
    control_calls: usize,
    interrupt_calls: usize,
}

impl MockEngine {
    fn new() -> Self {
        Self {
            control_response: Vec::new(),
            reports: Vec::new(),
            control_calls: 0,
            interrupt_calls: 0,
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

    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<Option<usize>, DriverError> {
        self.interrupt_calls += 1;
        if self.reports.is_empty() {
            return Ok(None);
        }
        let report = self.reports.remove(0);
        let n = report.len().min(data.len());
        data[..n].copy_from_slice(&report[..n]);
        Ok(Some(n))
    }
}

/// An in-process [`UrbCall`] that routes a URB straight into [`serve_urb`]
/// over a shared buffer — the host stand-in for the kernel IPC call the class
/// driver issues. The shared buffer is the single memory both sides see.
struct DirectCall {
    engine: Rc<RefCell<MockEngine>>,
    buffer: Rc<RefCell<Vec<u8>>>,
}

impl UrbCall for DirectCall {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        let mut buffer = self.buffer.borrow_mut();
        let mut engine = self.engine.borrow_mut();
        serve_urb(request, &mut buffer, &mut *engine, reply)
    }
}

/// Serve one URB directly (no client), returning the decoded completion. Used
/// by the fail-closed tests, which assert on the in-band errno.
fn serve_one(urb: &UrbRequest, buffer_len: usize, engine: &mut MockEngine) -> Result<u32, Errno> {
    let mut request = [0u8; URB_REQUEST_LEN];
    let n = urb.encode(&mut request).expect("encodes");
    let mut buffer = vec![0u8; buffer_len];
    let mut reply = [0u8; URB_COMPLETION_LEN];
    let len = serve_urb(&request[..n], &mut buffer, engine, &mut reply).expect("frames a reply");
    rustos_abi::usb_urb::decode_completion(&reply[..len])
}

#[test]
fn control_in_round_trips_through_the_client() {
    let engine = Rc::new(RefCell::new(MockEngine::new()));
    let descriptor = vec![0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40];
    engine.borrow_mut().control_response = descriptor.clone();
    let buffer = Rc::new(RefCell::new(vec![0u8; 64]));

    let mut client = UrbClient::new(DirectCall {
        engine: engine.clone(),
        buffer: buffer.clone(),
    });

    // A GET_DESCRIPTOR(device) SETUP packet.
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
    let transferred = client
        .control_in(setup, BUFFER_HANDLE, 8)
        .expect("control-IN completes");
    assert_eq!(transferred, 8);
    // The device's bytes landed in the shared buffer the class driver reads.
    assert_eq!(&buffer.borrow()[..8], &descriptor[..]);
    assert_eq!(engine.borrow().control_calls, 1);
}

#[test]
fn interrupt_in_round_trips_and_then_reports_would_block() {
    let engine = Rc::new(RefCell::new(MockEngine::new()));
    let report = vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
    engine.borrow_mut().reports = vec![report.clone()];
    let buffer = Rc::new(RefCell::new(vec![0u8; 8]));

    let mut client = UrbClient::new(DirectCall {
        engine: engine.clone(),
        buffer: buffer.clone(),
    });

    // The first poll delivers the queued report.
    let transferred = client
        .interrupt_in(1, BUFFER_HANDLE, 8)
        .expect("interrupt-IN completes");
    assert_eq!(transferred, 8);
    assert_eq!(&buffer.borrow()[..8], &report[..]);

    // With nothing pending, a non-blocking poll fails closed with the
    // retryable `WouldBlock` rather than fabricating a report.
    assert_eq!(
        client.interrupt_in(1, BUFFER_HANDLE, 8),
        Err(Errno::WouldBlock)
    );
    assert_eq!(engine.borrow().interrupt_calls, 2);
}

#[test]
fn rejects_oversize_length_before_the_engine() {
    let mut engine = MockEngine::new();
    let urb = UrbRequest {
        endpoint: 1,
        transfer_type: UsbTransferType::Interrupt,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        // One byte past the 8-byte shared buffer.
        length: 9,
        setup: [0; 8],
    };
    assert_eq!(
        serve_one(&urb, 8, &mut engine),
        Err(Errno::LengthOutOfRange)
    );
    // The transfer never reached the engine.
    assert_eq!(engine.interrupt_calls, 0);
}

#[test]
fn rejects_interrupt_on_the_control_endpoint() {
    let mut engine = MockEngine::new();
    let urb = UrbRequest {
        endpoint: 0,
        transfer_type: UsbTransferType::Interrupt,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length: 8,
        setup: [0; 8],
    };
    assert_eq!(serve_one(&urb, 8, &mut engine), Err(Errno::OutOfRange));
    assert_eq!(engine.interrupt_calls, 0);
}

#[test]
fn rejects_control_on_a_device_endpoint() {
    let mut engine = MockEngine::new();
    let urb = UrbRequest {
        endpoint: 3,
        transfer_type: UsbTransferType::Control,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length: 8,
        setup: [0; 8],
    };
    assert_eq!(serve_one(&urb, 8, &mut engine), Err(Errno::OutOfRange));
    assert_eq!(engine.control_calls, 0);
}

#[test]
fn rejects_illegal_direction() {
    let mut engine = MockEngine::new();
    // An interrupt-OUT is not a boot-report transfer; refuse it.
    let interrupt_out = UrbRequest {
        endpoint: 1,
        transfer_type: UsbTransferType::Interrupt,
        direction: UsbDirection::Out,
        buffer: BUFFER_HANDLE,
        length: 8,
        setup: [0; 8],
    };
    assert_eq!(
        serve_one(&interrupt_out, 8, &mut engine),
        Err(Errno::OutOfRange)
    );
    assert_eq!(engine.interrupt_calls, 0);

    // A control-OUT is not served on this boot-protocol seam.
    let control_out = UrbRequest {
        endpoint: 0,
        transfer_type: UsbTransferType::Control,
        direction: UsbDirection::Out,
        buffer: BUFFER_HANDLE,
        length: 0,
        setup: [0; 8],
    };
    assert_eq!(
        serve_one(&control_out, 8, &mut engine),
        Err(Errno::NotImplemented)
    );
    assert_eq!(engine.control_calls, 0);
}

#[test]
fn rejects_bulk_transfers() {
    let mut engine = MockEngine::new();
    let urb = UrbRequest {
        endpoint: 2,
        transfer_type: UsbTransferType::Bulk,
        direction: UsbDirection::In,
        buffer: BUFFER_HANDLE,
        length: 8,
        setup: [0; 8],
    };
    assert_eq!(serve_one(&urb, 8, &mut engine), Err(Errno::NotImplemented));
    assert_eq!(engine.control_calls, 0);
    assert_eq!(engine.interrupt_calls, 0);
}

#[test]
fn malformed_request_is_framed_in_band() {
    // A truncated request never reaches the engine and is answered with a
    // status-framed error completion the client decodes.
    let mut engine = MockEngine::new();
    let short = [0u8; URB_REQUEST_LEN - 1];
    let mut buffer = [0u8; 8];
    let mut reply = [0u8; URB_COMPLETION_LEN];
    let len = serve_urb(&short, &mut buffer, &mut engine, &mut reply).expect("frames a reply");
    assert_eq!(
        rustos_abi::usb_urb::decode_completion(&reply[..len]),
        Err(Errno::LengthOutOfRange)
    );
    assert_eq!(engine.control_calls, 0);
    assert_eq!(engine.interrupt_calls, 0);
}
