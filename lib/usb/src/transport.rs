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
//! * [`drive_urb`] is the controller-side server transformation: decode a URB
//!   frame, validate it fail-closed against the interface, and drive the
//!   engine. It is **asynchronous**: an interrupt-IN report that has not
//!   arrived yet returns `Ok(None)`, so the HCD holds the caller's URB call
//!   outstanding and re-drives it on its next controller interrupt rather than
//!   busy-polling or blocking one interface inside another's handler
//!   (`plans/USB.md` §1.1, the async event loop). [`frame_completion`] frames
//!   the outcome into the in-band completion the HCD replies with.
//! * [`UrbClient`] is the class-side client over a [`UrbCall`] transport
//!   (the IPC call the class driver issues): it builds the URB, submits it,
//!   and decodes the completion. The call blocks in the kernel until the HCD
//!   replies (when the report arrives), so the class driver parks rather than
//!   spinning.
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
/// it because [`drive_urb`] validates the URB first. The transfers the
/// served device classes need are present: a control-IN transfer (used
/// during enumeration and for class-IN requests), a **no-data** control-OUT
/// (a class request carrying its whole meaning in SETUP — the BOT Mass
/// Storage Reset, `plans/DEVICES.md` D2), a control-OUT **data stage** (a
/// class request carrying a payload — the CBI ADSC command channel,
/// `plans/DEVICES.md` D5), a non-blocking interrupt-IN poll (the HID report
/// and CBI completion paths), and non-blocking bulk IN/OUT (the
/// mass-storage data path, `plans/DEVICES.md` D1).
pub trait UrbEngine {
    /// Run a control-IN transfer (SETUP + IN data stage) into `data`,
    /// returning the bytes the device delivered.
    ///
    /// # Errors
    ///
    /// A [`DriverError`] from the controller/device (e.g.
    /// [`DriverError::DeviceFault`]).
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, DriverError>;

    /// Run a no-data control transfer (SETUP + status stage only, USB 2.0
    /// §9.3 `wLength == 0`): a class request whose whole meaning rides in
    /// `setup`, e.g. the BOT Bulk-Only Mass Storage Reset.
    ///
    /// # Errors
    ///
    /// A [`DriverError`] from the controller/device (e.g.
    /// [`DriverError::DeviceFault`]).
    fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), DriverError>;

    /// Run a control-OUT transfer (SETUP + OUT data stage carrying `data` +
    /// status stage): a class request with a payload, e.g. the CBI ADSC
    /// command block.
    ///
    /// # Errors
    ///
    /// * [`DriverError::EndpointStalled`] — the device refused the request
    ///   with a protocol STALL (the control endpoint recovers on the next
    ///   SETUP, USB 2.0 §8.5.3.4).
    /// * Any other [`DriverError`] from the controller/device.
    fn control_out(&mut self, setup: [u8; 8], data: &[u8]) -> Result<(), DriverError>;

    /// Poll the interface's interrupt-IN endpoint for one pending report into
    /// `data`. `Ok(Some(n))` if a report arrived, `Ok(None)` if none is
    /// pending yet (the caller retries).
    ///
    /// # Errors
    ///
    /// A [`DriverError`] from the controller/device.
    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<Option<usize>, DriverError>;

    /// Drive one bulk-IN transfer on device endpoint number `endpoint`
    /// reading into `data`: arm it if not yet armed, then reap its
    /// completion. `Ok(Some(n))` when the transfer finished (`n` bytes
    /// landed in `data`; a short packet yields `n < data.len()`),
    /// `Ok(None)` while it is still in flight (the caller re-drives on the
    /// next controller event).
    ///
    /// # Errors
    ///
    /// * [`DriverError::EndpointStalled`] — the device answered the
    ///   transfer with STALL; the engine has already recovered the
    ///   endpoint, so the caller may submit fresh transfers immediately.
    /// * [`DriverError::OutOfRange`] — `endpoint` is not the interface's
    ///   configured bulk-IN endpoint.
    /// * Any other [`DriverError`] for a hard controller/device fault.
    fn bulk_in(&mut self, endpoint: u8, data: &mut [u8]) -> Result<Option<usize>, DriverError>;

    /// Drive one bulk-OUT transfer on device endpoint number `endpoint`
    /// writing `data`: arm it if not yet armed, then reap its completion.
    /// `Ok(Some(n))` when the transfer finished (`n` bytes accepted by the
    /// device), `Ok(None)` while it is still in flight.
    ///
    /// # Errors
    ///
    /// As [`Self::bulk_in`].
    fn bulk_out(&mut self, endpoint: u8, data: &[u8]) -> Result<Option<usize>, DriverError>;
}

/// Decode `request`, validate it fail-closed against the interface, and drive
/// it on `engine` over the shared `data` buffer, returning the transfer
/// outcome.
///
/// This is the controller-side body the HCD runs after
/// [`call_recv`](rustos_abi::SyscallNumber::CALL_RECV). It is **asynchronous**:
///
/// * `Ok(Some(n))` — the transfer completed; `n` bytes landed in `data`. The
///   HCD frames a completion with [`frame_completion`] and replies now.
/// * `Ok(None)` — an interrupt-IN report has not arrived yet. The HCD leaves
///   the caller's URB call outstanding and re-drives this same `request` on
///   its next controller interrupt (the report path); it never busy-polls and
///   never blocks one interface inside another's handler.
/// * `Err(_)` — a malformed or illegal URB (a bad endpoint/direction/transfer
///   type, an oversize length), refused **before** the engine is touched, or a
///   controller fault. The HCD frames an error completion and replies now, so
///   the blocked caller always fails closed.
///
/// Re-decoding the stored `request` each time it is driven keeps the
/// validation in one place and costs only a fixed-size parse.
pub fn drive_urb<E: UrbEngine>(
    request: &[u8],
    data: &mut [u8],
    engine: &mut E,
) -> Result<Option<u32>, Errno> {
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
            // The served control-OUT shapes: the no-data form (SETUP only)
            // and the data-stage form carrying the shared buffer's bytes.
            if urb.direction == UsbDirection::Out {
                if urb.length == 0 {
                    engine
                        .control_no_data(urb.setup)
                        .map_err(DriverError::as_errno)?;
                    return Ok(Some(0));
                }
                engine
                    .control_out(urb.setup, slice)
                    .map_err(DriverError::as_errno)?;
                return Ok(Some(urb.length));
            }
            // A control transfer completes synchronously within the call.
            let transferred = engine
                .control_in(urb.setup, slice)
                .map_err(DriverError::as_errno)?;
            Ok(Some(
                u32::try_from(transferred).map_err(|_| Errno::LengthOutOfRange)?,
            ))
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
                Some(transferred) => Ok(Some(
                    u32::try_from(transferred).map_err(|_| Errno::LengthOutOfRange)?,
                )),
                // No report yet — hold the URB outstanding (Ok(None)), do not
                // fabricate a completion.
                None => Ok(None),
            }
        }
        UsbTransferType::Bulk => {
            // A bulk transfer targets a device endpoint, never the shared
            // control endpoint. Whether the endpoint is the interface's
            // configured bulk endpoint in that direction is the engine's
            // check (it owns the interface's endpoint map); both fail
            // closed before any ring is touched.
            if urb.endpoint == 0 {
                return Err(Errno::OutOfRange);
            }
            let outcome = match urb.direction {
                UsbDirection::In => engine.bulk_in(urb.endpoint, slice),
                UsbDirection::Out => engine.bulk_out(urb.endpoint, slice),
            }
            .map_err(DriverError::as_errno)?;
            match outcome {
                Some(transferred) => Ok(Some(
                    u32::try_from(transferred).map_err(|_| Errno::LengthOutOfRange)?,
                )),
                // Still in flight — hold the URB outstanding; the next
                // controller event re-drives it.
                None => Ok(None),
            }
        }
    }
}

/// Frame a completed transfer outcome into a URB completion in `reply`,
/// returning the reply length.
///
/// `Ok(n)` becomes a success completion carrying the bytes transferred; an
/// `Err` becomes a status-framed error completion, so the blocked caller is
/// always answered and fails closed. This is the wire transformation the HCD
/// runs immediately before
/// [`call_reply`](rustos_abi::SyscallNumber::CALL_REPLY).
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `reply` cannot hold a completion frame (it
/// must be at least [`URB_COMPLETION_LEN`]). The caller sizes it so.
pub fn frame_completion(reply: &mut [u8], result: Result<u32, Errno>) -> Result<usize, Errno> {
    match result {
        Ok(transferred) => encode_completion(reply, transferred),
        Err(err) => encode_error_completion(reply, err),
    }
}

/// The class-side transport: one synchronous URB call to the HCD's endpoint.
///
/// A class driver implements this over the kernel
/// [`ipc_call`](rustos_abi::SyscallNumber::IPC_CALL) surface (`plans/USB.md`
/// U4); a host test implements it by routing the bytes through [`drive_urb`]
/// and [`frame_completion`].
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

    /// Borrow the underlying call transport, so a class driver can observe
    /// transport-level state it records there (e.g. that the served
    /// interface's endpoint has vanished after a hot-unplug).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Submit `urb` and decode the completion, returning the bytes
    /// transferred.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`] (a controller/device fault, or a
    /// rejected URB), or an encode/transport error. The call blocks in the
    /// kernel until the HCD replies, so a not-yet-ready interrupt-IN report
    /// parks the caller rather than surfacing a retryable error.
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

    /// Submit a no-data control-OUT URB on endpoint 0 (SETUP + status stage
    /// only): a class request whose whole meaning rides in `setup`, e.g. the
    /// BOT Bulk-Only Mass Storage Reset.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`], or an encode/transport error.
    pub fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), Errno> {
        self.submit(&UrbRequest {
            endpoint: 0,
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::Out,
            buffer: 0,
            length: 0,
            setup,
        })
        .map(|_| ())
    }

    /// Submit a control-OUT URB on endpoint 0 whose OUT data stage carries
    /// `length` bytes from the shared `buffer`: a class request with a
    /// payload, e.g. the CBI ADSC command block.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`] — notably [`Errno::EndpointStalled`]
    /// when the device refused the request with a protocol STALL — or an
    /// encode/transport error. A zero `length` is refused
    /// ([`Errno::LengthOutOfRange`]): the no-data form is
    /// [`Self::control_no_data`], and the two must not be conflated.
    pub fn control_out(&mut self, setup: [u8; 8], buffer: u64, length: u32) -> Result<(), Errno> {
        if length == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        self.submit(&UrbRequest {
            endpoint: 0,
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::Out,
            buffer,
            length,
            setup,
        })
        .map(|_| ())
    }

    /// Submit an interrupt-IN URB for `endpoint` reading one report into the
    /// shared `buffer` of `length` bytes, returning the bytes transferred.
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`] (a controller/device fault), or an
    /// encode/transport error. The call blocks until a report arrives, so the
    /// class driver parks rather than busy-polling for the next report.
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

    /// Submit a bulk-IN URB for `endpoint` reading up to `length` bytes into
    /// the shared `buffer`, returning the bytes the device delivered (a
    /// short packet yields fewer than `length`).
    ///
    /// # Errors
    ///
    /// The carried completion [`Errno`] — notably
    /// [`Errno::EndpointStalled`] when the device answered the transfer
    /// with STALL (the endpoint is already recovered; the caller runs its
    /// class-level recovery and may submit again) — or an encode/transport
    /// error. The call blocks until the transfer completes.
    pub fn bulk_in(&mut self, endpoint: u8, buffer: u64, length: u32) -> Result<u32, Errno> {
        self.submit(&UrbRequest {
            endpoint,
            transfer_type: UsbTransferType::Bulk,
            direction: UsbDirection::In,
            buffer,
            length,
            setup: [0; 8],
        })
    }

    /// Submit a bulk-OUT URB for `endpoint` writing `length` bytes from the
    /// shared `buffer`, returning the bytes the device accepted.
    ///
    /// # Errors
    ///
    /// As [`Self::bulk_in`].
    pub fn bulk_out(&mut self, endpoint: u8, buffer: u64, length: u32) -> Result<u32, Errno> {
        self.submit(&UrbRequest {
            endpoint,
            transfer_type: UsbTransferType::Bulk,
            direction: UsbDirection::Out,
            buffer,
            length,
            setup: [0; 8],
        })
    }
}

#[cfg(test)]
mod tests;
