//! The bus-agnostic USB request-block (URB) transport IPC protocol
//! (`plans/USB.md` §1.3, U2).
//!
//! The modular USB stack splits the host-controller driver (HCD) from the
//! per-interface class drivers: the HCD owns one controller (its registers,
//! DMA rings, and root-hub ports) and serves a **URB transport call
//! endpoint** per USB interface it emits into the hardware tree; a class
//! driver (e.g. the HID boot keyboard) binds that emitted interface node and
//! submits URBs over the endpoint, touching no controller register and no
//! other interface's buffer.
//!
//! A **URB** is one queued USB transfer: an endpoint within the interface, a
//! transfer type (control / interrupt / bulk), a direction, a shared-memory
//! data buffer the class driver owns, and a length. A **completion** reports
//! the transfer's status and the bytes actually moved. Neither carries any
//! controller detail — the same class-driver binary works behind any host
//! controller that serves this ABI.
//!
//! This module is the wire contract for that endpoint, modelled on the
//! read-only driver store ([`crate::driver_store`]) and the firmware mailbox
//! ([`crate::mailbox_ipc`]): borrowed buffers, no allocation (the crate is
//! `no_std`), and a status-framed reply so a fail-closed refusal is delivered
//! in-band rather than as a truncated payload. The transfer's *data* travels
//! through the separately-mapped shared-memory buffer the request names, not
//! in the request frame; only the URB descriptor and the completion cross the
//! call endpoint.

use crate::le::{put_i32, put_u32, put_u64, read_i32, read_u32, read_u64};
use crate::Errno;

/// Highest USB endpoint *number* an interface can address (USB 2.0 §9.6.6:
/// `bEndpointAddress` carries a 4-bit endpoint number, so `1..=15` are
/// device endpoints and `0` is the shared control endpoint). A validation
/// bound on an untrusted field, not a scalable capacity.
pub const MAX_ENDPOINT: u8 = 15;

/// Direction of a URB's data stage, from the host's point of view.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UsbDirection {
    /// Host → device (an OUT transfer; the data buffer is read by the HCD).
    Out = 0,
    /// Device → host (an IN transfer; the data buffer is written by the HCD).
    In = 1,
}

impl UsbDirection {
    /// The wire byte for this direction.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a direction from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is neither [`Self::Out`] nor
    /// [`Self::In`] (fail closed on a malformed field).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Out),
            1 => Ok(Self::In),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// USB transfer type of a URB (USB 2.0 §9.6.6 `bmAttributes` transfer-type
/// field). Isochronous is deliberately absent — it is out of scope for the
/// boot-protocol stack (`plans/USB.md` §4) and a URB naming it is rejected.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum UsbTransferType {
    /// A control transfer on endpoint 0 (SETUP + optional data + status).
    Control = 0,
    /// An interrupt transfer on a device endpoint (e.g. an HID report).
    Interrupt = 1,
    /// A bulk transfer on a device endpoint.
    Bulk = 2,
}

impl UsbTransferType {
    /// The wire byte for this transfer type.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a transfer type from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known transfer type (fail
    /// closed; isochronous and any future value are refused here).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Control),
            1 => Ok(Self::Interrupt),
            2 => Ok(Self::Bulk),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// A USB request block: one queued transfer submitted to the HCD's URB
/// transport endpoint.
///
/// The transfer's payload lives in the shared-memory buffer named by
/// [`Self::buffer`]; the request carries only the transfer's shape. The HCD
/// validates every field against the interface before it touches a ring
/// (`plans/USB.md` §1.3) and fails closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UrbRequest {
    /// The endpoint *number* within the interface (`0` is the control
    /// endpoint; `1..=`[`MAX_ENDPOINT`] are device endpoints). The direction
    /// is carried explicitly in [`Self::direction`] rather than folded into a
    /// `bEndpointAddress` bit, so there is one source of truth for it.
    pub endpoint: u8,
    /// The transfer type.
    pub transfer_type: UsbTransferType,
    /// The data-stage direction.
    pub direction: UsbDirection,
    /// Handle of the shared-memory IPC object holding the transfer's data
    /// buffer (the class driver owns it; the HCD maps it for the transfer).
    pub buffer: u64,
    /// Number of bytes to transfer, never larger than the buffer's mapped
    /// length (the HCD re-checks this against the actual mapping).
    pub length: u32,
    /// The 8-byte SETUP packet, meaningful only for a
    /// [`UsbTransferType::Control`] transfer (zero-filled otherwise).
    pub setup: [u8; 8],
}

/// Encoded length of a [`UrbRequest`]: `endpoint(1) || transfer_type(1) ||
/// direction(1) || pad(1) || length(4) || buffer(8) || setup(8)`. Fixed —
/// every URB encodes to the same size, so this is both the encoding length
/// and the endpoint's maximum request size.
pub const URB_REQUEST_LEN: usize = 1 + 1 + 1 + 1 + 4 + 8 + 8;

impl UrbRequest {
    /// Encode `self` into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold [`URB_REQUEST_LEN`]
    /// bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.len() < URB_REQUEST_LEN {
            return Err(Errno::BufferTooSmall);
        }
        buf[0] = self.endpoint;
        buf[1] = self.transfer_type.as_u8();
        buf[2] = self.direction.as_u8();
        buf[3] = 0;
        put_u32(buf, 4, self.length);
        put_u64(buf, 8, self.buffer);
        buf[16..URB_REQUEST_LEN].copy_from_slice(&self.setup);
        Ok(URB_REQUEST_LEN)
    }

    /// Decode a URB request from `bytes`, validating every field.
    ///
    /// This rejects a malformed *encoding* (truncation, an unknown transfer
    /// type or direction, an endpoint number above [`MAX_ENDPOINT`]); the HCD
    /// performs the further *semantic* checks that need the live interface
    /// (the endpoint belongs to this interface, the length fits the mapped
    /// buffer) before it queues the transfer.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is shorter than
    ///   [`URB_REQUEST_LEN`] — a truncated request is never read past its
    ///   bytes.
    /// * [`Errno::OutOfRange`] if the transfer type or direction byte is
    ///   unknown, or the endpoint number exceeds [`MAX_ENDPOINT`].
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < URB_REQUEST_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let endpoint = bytes[0];
        if endpoint > MAX_ENDPOINT {
            return Err(Errno::OutOfRange);
        }
        let transfer_type = UsbTransferType::from_u8(bytes[1])?;
        let direction = UsbDirection::from_u8(bytes[2])?;
        let length = read_u32(bytes, 4);
        let buffer = read_u64(bytes, 8);
        let mut setup = [0u8; 8];
        setup.copy_from_slice(&bytes[16..URB_REQUEST_LEN]);
        Ok(Self {
            endpoint,
            transfer_type,
            direction,
            buffer,
            length,
            setup,
        })
    }
}

/// Fixed prefix of every completion frame: a status word (`0` on success,
/// else the negated [`Errno`] discriminant), mirroring [`crate::driver_store`].
const COMPLETION_STATUS_LEN: usize = 4;

/// Encoded length of a URB completion: the status word followed by the
/// `u32` byte count actually transferred. Also the endpoint's maximum reply
/// size.
pub const URB_COMPLETION_LEN: usize = COMPLETION_STATUS_LEN + 4;

/// Encode a successful URB completion carrying `transferred` bytes into
/// `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`URB_COMPLETION_LEN`]
/// bytes.
pub fn encode_completion(buf: &mut [u8], transferred: u32) -> Result<usize, Errno> {
    if buf.len() < URB_COMPLETION_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    put_u32(buf, COMPLETION_STATUS_LEN, transferred);
    Ok(URB_COMPLETION_LEN)
}

/// Encode a fail-closed error completion (status only) into `buf`.
///
/// Used both for a hard transfer failure and for the benign
/// [`Errno::WouldBlock`] a non-blocking interrupt-IN poll returns when no
/// report has arrived yet — the caller distinguishes them by the decoded
/// [`Errno`].
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` is shorter than the status word.
pub fn encode_error_completion(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < COMPLETION_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(COMPLETION_STATUS_LEN)
}

/// Decode a URB completion: the bytes transferred on success, else the
/// carried [`Errno`].
///
/// # Errors
///
/// The carried [`Errno`] for an error frame (e.g. [`Errno::WouldBlock`] for a
/// not-yet-arrived interrupt-IN report, or a hard transfer fault), or
/// [`Errno::BadMagic`] if a success frame is truncated or the status word is
/// neither `0` nor a known negated discriminant (wire corruption — fail
/// closed), or [`Errno::BufferTooSmall`] if `reply` is shorter than the
/// status word.
pub fn decode_completion(reply: &[u8]) -> Result<u32, Errno> {
    if reply.len() < COMPLETION_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(reply, 0) {
        0 => {
            if reply.len() < URB_COMPLETION_LEN {
                return Err(Errno::BadMagic);
            }
            Ok(read_u32(reply, COMPLETION_STATUS_LEN))
        }
        negative => Err(Errno::try_from_status(negative).unwrap_or(Errno::BadMagic)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UrbRequest {
        UrbRequest {
            endpoint: 1,
            transfer_type: UsbTransferType::Interrupt,
            direction: UsbDirection::In,
            buffer: 0xDEAD_BEEF_0000_0010,
            length: 8,
            setup: [0; 8],
        }
    }

    #[test]
    fn request_round_trips() {
        let req = UrbRequest {
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::In,
            endpoint: 0,
            setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00],
            ..sample()
        };
        let mut buf = [0u8; URB_REQUEST_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, URB_REQUEST_LEN);
        assert_eq!(UrbRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn request_encode_rejects_small_buffer() {
        let mut buf = [0u8; URB_REQUEST_LEN - 1];
        assert_eq!(sample().encode(&mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn request_decode_rejects_truncated() {
        let buf = [0u8; URB_REQUEST_LEN - 1];
        assert_eq!(UrbRequest::decode(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn request_decode_rejects_bad_fields_fail_closed() {
        // Endpoint number above the 4-bit maximum.
        let mut buf = [0u8; URB_REQUEST_LEN];
        sample().encode(&mut buf).expect("encodes");
        buf[0] = MAX_ENDPOINT + 1;
        assert_eq!(UrbRequest::decode(&buf), Err(Errno::OutOfRange));

        // Unknown transfer type (isochronous / future value).
        sample().encode(&mut buf).expect("encodes");
        buf[1] = 3;
        assert_eq!(UrbRequest::decode(&buf), Err(Errno::OutOfRange));

        // Unknown direction byte.
        sample().encode(&mut buf).expect("encodes");
        buf[2] = 2;
        assert_eq!(UrbRequest::decode(&buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn completion_round_trips() {
        let mut buf = [0u8; URB_COMPLETION_LEN];
        let n = encode_completion(&mut buf, 8).expect("encodes");
        assert_eq!(n, URB_COMPLETION_LEN);
        assert_eq!(decode_completion(&buf[..n]), Ok(8));
    }

    #[test]
    fn error_completion_surfaces_its_errno() {
        let mut buf = [0u8; URB_COMPLETION_LEN];
        let n = encode_error_completion(&mut buf, Errno::WouldBlock).expect("encodes");
        assert_eq!(decode_completion(&buf[..n]), Err(Errno::WouldBlock));
    }

    #[test]
    fn truncated_ok_completion_fails_closed() {
        // Status ok but no transferred-bytes body.
        let mut buf = [0u8; COMPLETION_STATUS_LEN];
        put_i32(&mut buf, 0, 0);
        assert_eq!(decode_completion(&buf), Err(Errno::BadMagic));
    }

    #[test]
    fn corrupt_status_fails_closed() {
        let mut buf = [0u8; URB_COMPLETION_LEN];
        put_i32(&mut buf, 0, -9_999);
        assert_eq!(decode_completion(&buf), Err(Errno::BadMagic));
    }

    #[test]
    fn i32_min_status_fails_closed_without_overflow() {
        // A hostile status word of `i32::MIN` cannot be negated in `i32`;
        // the decoder must fail closed rather than overflow-panic.
        let mut buf = [0u8; URB_COMPLETION_LEN];
        put_i32(&mut buf, 0, i32::MIN);
        assert_eq!(decode_completion(&buf), Err(Errno::BadMagic));
    }
}
