//! The I²C transfer-endpoint protocol (`plans/TIMESYNC.md` TS-4).
//!
//! A bus driver (`drivers/bus/i2c/*`) owns the controller's registers and
//! serves **one endpoint per child the device tree declared**. The bus
//! address never crosses this wire: the server takes it from the
//! [`BusChild`](crate::hwtree::HwResourceKind::BusChild) duty grant paired
//! with the endpoint the request arrived on, so a chip driver has no field in
//! which it could name a neighbour and a compromised one still reaches only
//! its own part.
//!
//! The framing follows [`crate::rtc_ipc`]: borrowed buffers, no allocation,
//! one fixed request and reply size, and a status-framed reply so a
//! fail-closed refusal arrives in-band rather than as a truncated payload.
//!
//! # Which endpoint
//!
//! There is no well-known id here. Each child's id comes from
//! [`crate::hwtree::bus_child_endpoint`] over its hardware-tree node id, so
//! the bus driver learns the ids it must serve from its duty grants and the
//! chip driver learns its own from its endpoint grant. Neither guesses, and
//! the block is reserved ([`crate::ipc::is_reserved_endpoint`]) so a
//! bystander cannot bind a chip's endpoint first and feed its driver forged
//! registers.

use crate::driver::i2c::{I2cPort, MAX_TRANSFER_LEN};
use crate::le::{put_i32, put_u16, read_i32, read_u16};
use crate::Errno;

/// Fixed prefix of every reply frame: a status word (`0` on success, else the
/// negated [`Errno`] discriminant).
const REPLY_STATUS_LEN: usize = 4;

/// Offset of the write-phase length in a request.
const WRITE_LEN_AT: usize = 0;
/// Offset of the read-phase length in a request.
const READ_LEN_AT: usize = 2;
/// Offset of the write-phase payload in a request.
const WRITE_AT: usize = 4;

/// Encoded length of a request: both phase lengths and a fixed-size write
/// payload. One size for every request, so the endpoint needs one bound and a
/// short frame is a refusal rather than a reinterpreted transfer.
pub const REQUEST_LEN: usize = WRITE_AT + MAX_TRANSFER_LEN;

/// Encoded length of a reply: the status word and a fixed-size read payload.
pub const REPLY_LEN: usize = REPLY_STATUS_LEN + MAX_TRANSFER_LEN;

/// Encode a transfer request into `buf`, returning the bytes written.
///
/// The unused tail of the write payload is zeroed rather than left as it
/// was, so one client's stack bytes never reach the bus driver.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`REQUEST_LEN`] bytes, or
/// [`Errno::LengthOutOfRange`] if either phase exceeds [`MAX_TRANSFER_LEN`].
pub fn encode_request(buf: &mut [u8], write: &[u8], read_len: usize) -> Result<usize, Errno> {
    if buf.len() < REQUEST_LEN {
        return Err(Errno::BufferTooSmall);
    }
    if write.len() > MAX_TRANSFER_LEN || read_len > MAX_TRANSFER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    // Both lengths are bounded by `MAX_TRANSFER_LEN` above, so neither
    // narrowing can lose a bit.
    let write_len = u16::try_from(write.len()).map_err(|_| Errno::LengthOutOfRange)?;
    let read_len = u16::try_from(read_len).map_err(|_| Errno::LengthOutOfRange)?;
    buf[..REQUEST_LEN].fill(0);
    put_u16(buf, WRITE_LEN_AT, write_len);
    put_u16(buf, READ_LEN_AT, read_len);
    buf[WRITE_AT..WRITE_AT + write.len()].copy_from_slice(write);
    Ok(REQUEST_LEN)
}

/// One decoded transfer request: the bytes to write, and how many to read
/// back. The part is the endpoint, so the frame names none.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TransferRequest<'a> {
    /// The write phase's payload, exactly as long as the frame declared.
    pub write: &'a [u8],
    /// How many bytes the read phase must return.
    pub read_len: usize,
}

/// Decode a transfer request from `bytes`.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] if the frame is short or either declared phase
/// exceeds [`MAX_TRANSFER_LEN`] — both before the bus is touched.
pub fn decode_request(bytes: &[u8]) -> Result<TransferRequest<'_>, Errno> {
    if bytes.len() < REQUEST_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    let write_len = usize::from(read_u16(bytes, WRITE_LEN_AT));
    let read_len = usize::from(read_u16(bytes, READ_LEN_AT));
    if write_len > MAX_TRANSFER_LEN || read_len > MAX_TRANSFER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(TransferRequest {
        write: &bytes[WRITE_AT..WRITE_AT + write_len],
        read_len,
    })
}

/// Encode a successful reply carrying `read` into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`REPLY_LEN`] bytes, or
/// [`Errno::LengthOutOfRange`] if `read` is longer than [`MAX_TRANSFER_LEN`].
pub fn encode_reply(buf: &mut [u8], read: &[u8]) -> Result<usize, Errno> {
    if buf.len() < REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    if read.len() > MAX_TRANSFER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    buf[..REPLY_LEN].fill(0);
    put_i32(buf, 0, 0);
    buf[REPLY_STATUS_LEN..REPLY_STATUS_LEN + read.len()].copy_from_slice(read);
    Ok(REPLY_LEN)
}

/// Encode a fail-closed error reply (status only) into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` is shorter than the status word.
pub fn encode_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(REPLY_STATUS_LEN)
}

/// Decode a reply, copying the read phase into `read`.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame; [`Errno::BufferTooSmall`] if
/// `reply` is shorter than the status word; [`Errno::BadMagic`] if a success
/// frame is truncated; [`Errno::LengthOutOfRange`] if `read` is longer than
/// [`MAX_TRANSFER_LEN`]. Every one of them leaves `read` untouched, so a
/// caller never mistakes a refusal for data.
pub fn decode_reply(reply: &[u8], read: &mut [u8]) -> Result<(), Errno> {
    if reply.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(reply, 0) {
        0 => {}
        negative => return Err(Errno::try_from_status(negative).unwrap_or(Errno::BadMagic)),
    }
    if read.len() > MAX_TRANSFER_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    if reply.len() < REPLY_LEN {
        return Err(Errno::BadMagic);
    }
    read.copy_from_slice(&reply[REPLY_STATUS_LEN..REPLY_STATUS_LEN + read.len()]);
    Ok(())
}

/// Serve one request frame against `port`, encoding the framed reply into
/// `reply` and returning the bytes written.
///
/// The wire-level server transformation every I²C bus driver runs between
/// `call_recv` and `call_reply`, so the controller logic in a driver stays
/// register access and the protocol has one implementation. `port` is the
/// server's own view of the child that endpoint belongs to — the address
/// comes from there, never from the frame. Every failure — a malformed
/// request or a [`DriverError`](crate::DriverError) from the controller —
/// becomes an in-band status-framed error reply, so the blocked caller is
/// always answered and fails closed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `reply` cannot hold a status-framed reply;
/// the caller sizes it to [`REPLY_LEN`].
pub fn serve_request<P: I2cPort + ?Sized>(
    port: &P,
    request: &[u8],
    reply: &mut [u8],
) -> Result<usize, Errno> {
    let decoded = match decode_request(request) {
        Ok(parsed) => parsed,
        Err(err) => return encode_error_reply(reply, err),
    };
    let mut read = [0u8; MAX_TRANSFER_LEN];
    let read = &mut read[..decoded.read_len];
    match port.transfer(decoded.write, read) {
        Ok(()) => encode_reply(reply, read),
        Err(err) => encode_error_reply(reply, err.as_errno()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_reply, decode_request, encode_error_reply, encode_reply, encode_request,
        serve_request, REPLY_LEN, REQUEST_LEN, WRITE_AT, WRITE_LEN_AT,
    };
    use crate::driver::i2c::{I2cPort, MAX_TRANSFER_LEN};
    use crate::le::put_u16;
    use crate::{DriverError, Errno};

    /// An [`I2cPort`] double recording the last transfer and answering with a
    /// programmed result.
    struct Port {
        answer: Result<[u8; MAX_TRANSFER_LEN], DriverError>,
        seen: core::cell::RefCell<Option<([u8; MAX_TRANSFER_LEN], usize, usize)>>,
    }

    impl Port {
        fn ok(byte: u8) -> Self {
            Self {
                answer: Ok([byte; MAX_TRANSFER_LEN]),
                seen: core::cell::RefCell::new(None),
            }
        }

        fn failing(err: DriverError) -> Self {
            Self {
                answer: Err(err),
                seen: core::cell::RefCell::new(None),
            }
        }
    }

    impl I2cPort for Port {
        fn transfer(&self, write: &[u8], read: &mut [u8]) -> Result<(), DriverError> {
            let mut copy = [0u8; MAX_TRANSFER_LEN];
            copy[..write.len()].copy_from_slice(write);
            *self.seen.borrow_mut() = Some((copy, write.len(), read.len()));
            let answer = self.answer?;
            read.copy_from_slice(&answer[..read.len()]);
            Ok(())
        }
    }

    #[test]
    fn a_request_round_trips_with_both_phases() {
        let mut buf = [0u8; REQUEST_LEN];
        encode_request(&mut buf, &[0x00, 0x11], 7).expect("encodes");
        let decoded = decode_request(&buf).expect("decodes");
        assert_eq!(decoded.write, &[0x00, 0x11]);
        assert_eq!(decoded.read_len, 7);
    }

    #[test]
    fn an_empty_phase_round_trips_as_empty() {
        let mut buf = [0u8; REQUEST_LEN];
        encode_request(&mut buf, &[], 0).expect("encodes");
        let decoded = decode_request(&buf).expect("decodes");
        assert!(decoded.write.is_empty());
        assert_eq!(decoded.read_len, 0);
    }

    #[test]
    fn the_frame_carries_no_address_a_client_could_choose() {
        // The whole frame is two lengths and a payload: there is nowhere for
        // a chip driver to name a part other than its own, which is what
        // makes the per-child endpoint its complete authority.
        assert_eq!(WRITE_AT, 4);
        assert_eq!(REQUEST_LEN, 4 + MAX_TRANSFER_LEN);
    }

    #[test]
    fn an_over_long_phase_is_refused_on_both_sides() {
        let mut buf = [0u8; REQUEST_LEN];
        let long = [0u8; MAX_TRANSFER_LEN + 1];
        assert_eq!(
            encode_request(&mut buf, &long, 0),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            encode_request(&mut buf, &[], MAX_TRANSFER_LEN + 1),
            Err(Errno::LengthOutOfRange)
        );
        // A hostile frame declaring more than the payload can hold is
        // refused rather than read past its own buffer.
        encode_request(&mut buf, &[], 0).expect("encodes");
        put_u16(&mut buf, WRITE_LEN_AT, u16::MAX);
        assert_eq!(decode_request(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn a_short_frame_is_refused_before_the_bus() {
        assert_eq!(decode_request(&[0u8; 3]), Err(Errno::LengthOutOfRange));

        // The server answers such a frame in band, and never reaches the bus.
        let port = Port::ok(0xAB);
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&port, &[0u8; 3], &mut reply).expect("frames a refusal");
        assert_eq!(
            decode_reply(&reply[..n], &mut []),
            Err(Errno::LengthOutOfRange)
        );
        assert!(port.seen.borrow().is_none());
    }

    #[test]
    fn the_unused_write_tail_is_zeroed_rather_than_carried() {
        let mut buf = [0xFFu8; REQUEST_LEN];
        encode_request(&mut buf, &[0x42], 0).expect("encodes");
        assert!(buf[WRITE_AT + 1..].iter().all(|b| *b == 0));
    }

    #[test]
    fn serving_runs_the_transfer_and_returns_the_read_phase() {
        let port = Port::ok(0x5A);
        let mut request = [0u8; REQUEST_LEN];
        encode_request(&mut request, &[0x03], 4).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&port, &request, &mut reply).expect("frames a reply");

        let mut read = [0u8; 4];
        decode_reply(&reply[..n], &mut read).expect("succeeds");
        assert_eq!(read, [0x5A; 4]);

        let seen = port.seen.borrow();
        let (write, write_len, read_len) = seen.expect("the port was driven");
        assert_eq!(&write[..write_len], &[0x03]);
        assert_eq!(read_len, 4);
    }

    #[test]
    fn a_controller_refusal_reaches_the_caller_as_its_own_errno() {
        for (fault, expected) in [
            (DriverError::NotFound, Errno::NotFound),
            (DriverError::DeviceFault, Errno::DeviceFault),
            (DriverError::PermissionDenied, Errno::PermissionDenied),
        ] {
            let port = Port::failing(fault);
            let mut request = [0u8; REQUEST_LEN];
            encode_request(&mut request, &[0x03], 4).expect("encodes");
            let mut reply = [0u8; REPLY_LEN];
            let n = serve_request(&port, &request, &mut reply).expect("frames a refusal");
            let mut read = [0xEEu8; 4];
            assert_eq!(decode_reply(&reply[..n], &mut read), Err(expected));
            // A refusal leaves the caller's buffer alone: no half-read is
            // ever mistaken for data.
            assert_eq!(read, [0xEE; 4]);
        }
    }

    #[test]
    fn a_truncated_success_reply_is_refused_rather_than_half_read() {
        let mut reply = [0u8; REPLY_LEN];
        encode_reply(&mut reply, &[1, 2, 3]).expect("encodes");
        let mut read = [0u8; 3];
        assert_eq!(decode_reply(&reply[..8], &mut read), Err(Errno::BadMagic));
        assert_eq!(
            decode_reply(&reply[..2], &mut read),
            Err(Errno::BufferTooSmall)
        );
        // The whole frame does decode.
        decode_reply(&reply, &mut read).expect("succeeds");
        assert_eq!(read, [1, 2, 3]);
    }

    #[test]
    fn an_error_reply_needs_only_the_status_word() {
        let mut reply = [0u8; REPLY_LEN];
        let n = encode_error_reply(&mut reply, Errno::DeviceFault).expect("encodes");
        assert_eq!(decode_reply(&reply[..n], &mut []), Err(Errno::DeviceFault));
        assert_eq!(
            encode_error_reply(&mut [0u8; 2], Errno::DeviceFault),
            Err(Errno::BufferTooSmall)
        );
    }
}
