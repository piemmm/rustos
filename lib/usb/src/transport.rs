//! The bus-agnostic URB transport seam (`plans/USB.md` §1.3, U2).
//!
//! The host-controller driver (HCD) owns one controller and serves a URB
//! transport call endpoint per USB interface it emits; a class driver binds
//! that interface node and submits URBs over the endpoint. This module is the
//! protocol layer both sides share:
//!
//! * [`UrbEngine`] is the controller-side seam — the operations the HCD's
//!   real engine ([`crate::device::UsbDevice`]) performs to satisfy a URB. A
//!   host test drives it with a mock engine.
//! * [`serve_urb`] is the controller-side server transformation: decode a URB
//!   frame, validate it fail-closed against the interface, drive the engine,
//!   and frame the completion in band.
//! * [`UrbClient`] is the class-side client over a [`UrbCall`] transport
//!   (the synchronous IPC call the class driver issues): it builds the URB,
//!   submits it, and decodes the completion.
//!
//! Only the URB descriptor and the completion cross the endpoint; the
//! transfer's *data* lives in the separately-mapped shared-memory buffer the
//! URB names (the `data` slice the server is handed, and the buffer the
//! client reads back). No class driver ever sees a controller register or
//! another interface's buffer.

use rustos_abi::usb_urb::{
    decode_completion, encode_completion, encode_error_completion, UrbRequest, UsbDirection,
    UsbTransferType, URB_COMPLETION_LEN, URB_REQUEST_LEN,
};
use rustos_abi::{DriverError, Errno};

/// The controller-side operations the URB transport server drives.
///
/// The HCD's live engine implements this; a malformed transfer never reaches
/// it because [`serve_urb`] validates the URB first. Only the boot-protocol
/// transfers the keyboard stack needs are present: a control-IN transfer
/// (used during enumeration and for class-IN requests) and a non-blocking
/// interrupt-IN poll (the report path). Control-OUT and bulk are deliberately
/// absent — they are a later class-driver extension on this seam
/// (`plans/USB.md` §4); the server refuses them fail-closed rather than
/// pretending to perform them.
pub trait UrbEngine {
    /// Run a control-IN transfer (SETUP + IN data stage) into `data`,
    /// returning the bytes the device delivered.
    ///
    /// # Errors
    ///
    /// A [`DriverError`] from the controller/device (e.g.
    /// [`DriverError::DeviceFault`]).
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, DriverError>;

    /// Poll the interface's interrupt-IN endpoint for one pending report into
    /// `data`. `Ok(Some(n))` if a report arrived, `Ok(None)` if none is
    /// pending yet (the caller retries).
    ///
    /// # Errors
    ///
    /// A [`DriverError`] from the controller/device.
    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<Option<usize>, DriverError>;
}

/// Validate `request` against the interface, run it on `engine`, and return
/// the bytes transferred — the body behind [`serve_urb`].
fn run_urb<E: UrbEngine>(request: &[u8], data: &mut [u8], engine: &mut E) -> Result<u32, Errno> {
    let urb = UrbRequest::decode(request)?;
    let length = urb.length as usize;
    // The transfer may never run past the mapped shared buffer.
    if length > data.len() {
        return Err(Errno::LengthOutOfRange);
    }
    let slice = &mut data[..length];
    match urb.transfer_type {
        UsbTransferType::Control => {
            // A control transfer is the endpoint-0 protocol; any other
            // endpoint number is illegal.
            if urb.endpoint != 0 {
                return Err(Errno::OutOfRange);
            }
            // Only the IN data stage is served on this seam.
            if urb.direction != UsbDirection::In {
                return Err(Errno::NotImplemented);
            }
            let transferred = engine
                .control_in(urb.setup, slice)
                .map_err(DriverError::as_errno)?;
            u32::try_from(transferred).map_err(|_| Errno::LengthOutOfRange)
        }
        UsbTransferType::Interrupt => {
            // An interrupt transfer targets a device endpoint, never the
            // shared control endpoint, and the boot report path is IN.
            if urb.endpoint == 0 {
                return Err(Errno::OutOfRange);
            }
            if urb.direction != UsbDirection::In {
                return Err(Errno::OutOfRange);
            }
            match engine.interrupt_in(slice).map_err(DriverError::as_errno)? {
                Some(transferred) => {
                    u32::try_from(transferred).map_err(|_| Errno::LengthOutOfRange)
                }
                // No report yet — the benign, retryable outcome.
                None => Err(Errno::WouldBlock),
            }
        }
        // Bulk is out of scope for the boot-protocol stack (`plans/USB.md`
        // §4); refuse it rather than queue a transfer the engine cannot run.
        UsbTransferType::Bulk => Err(Errno::NotImplemented),
    }
}

/// Serve one URB frame: decode and validate it, drive `engine` over the
/// shared `data` buffer the URB names, and frame the completion into `reply`,
/// returning the reply length.
///
/// This is the wire-level transformation the HCD runs between
/// [`call_recv`](rustos_abi::SyscallNumber::CALL_RECV) and
/// [`call_reply`](rustos_abi::SyscallNumber::CALL_REPLY). Every failure — a
/// malformed URB, an endpoint/direction that does not belong to the
/// interface, an oversize length, or a controller fault — becomes an in-band
/// status-framed error completion, so the blocked caller is always answered
/// and fails closed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `reply` cannot hold a completion frame (it
/// must be at least [`URB_COMPLETION_LEN`]). The caller sizes it so.
pub fn serve_urb<E: UrbEngine>(
    request: &[u8],
    data: &mut [u8],
    engine: &mut E,
    reply: &mut [u8],
) -> Result<usize, Errno> {
    match run_urb(request, data, engine) {
        Ok(transferred) => encode_completion(reply, transferred),
        Err(err) => encode_error_completion(reply, err),
    }
}

/// The class-side transport: one synchronous URB call to the HCD's endpoint.
///
/// A class driver implements this over the kernel
/// [`ipc_call`](rustos_abi::SyscallNumber::IPC_CALL) surface (`plans/USB.md`
/// U4); a host test implements it by routing the bytes straight to
/// [`serve_urb`].
pub trait UrbCall {
    /// Send the encoded URB `request` to the HCD and read the framed
    /// completion into `reply`, returning its length.
    ///
    /// # Errors
    ///
    /// An [`Errno`] from the underlying call transport (a dead endpoint, a
    /// truncated reply).
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// The class-side URB transport client: builds URBs, submits them over a
/// [`UrbCall`] transport, and decodes the completions.
pub struct UrbClient<T: UrbCall> {
    transport: T,
}

impl<T: UrbCall> UrbClient<T> {
    /// Wrap a call transport.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Submit `urb` and decode the completion, returning the bytes
    /// transferred.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`] (e.g. [`Errno::WouldBlock`] when an
    /// interrupt-IN report has not arrived yet), or an encode/transport
    /// error.
    fn submit(&mut self, urb: &UrbRequest) -> Result<u32, Errno> {
        let mut request = [0u8; URB_REQUEST_LEN];
        let n = urb.encode(&mut request)?;
        let mut reply = [0u8; URB_COMPLETION_LEN];
        let len = self.transport.call(&request[..n], &mut reply)?;
        decode_completion(&reply[..len])
    }

    /// Submit a control-IN URB on endpoint 0 reading into the shared `buffer`
    /// of `length` bytes, returning the bytes the device delivered.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`], or an encode/transport error.
    pub fn control_in(&mut self, setup: [u8; 8], buffer: u64, length: u32) -> Result<u32, Errno> {
        self.submit(&UrbRequest {
            endpoint: 0,
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::In,
            buffer,
            length,
            setup,
        })
    }

    /// Submit an interrupt-IN URB for `endpoint` reading one report into the
    /// shared `buffer` of `length` bytes, returning the bytes transferred.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`] (or an encode/transport error) —
    /// notably [`Errno::WouldBlock`] when no report is pending yet, which the
    /// caller treats as "retry", not a fault.
    pub fn interrupt_in(&mut self, endpoint: u8, buffer: u64, length: u32) -> Result<u32, Errno> {
        self.submit(&UrbRequest {
            endpoint,
            transfer_type: UsbTransferType::Interrupt,
            direction: UsbDirection::In,
            buffer,
            length,
            setup: [0; 8],
        })
    }
}

#[cfg(test)]
mod tests;
