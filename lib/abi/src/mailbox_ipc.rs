//! The firmware property-mailbox IPC protocol (`plans/PI.md` P10 D3).
//!
//! Under Design D the `VideoCore` mailbox moves out of the kernel into a
//! user-space **service driver** (`drivers/bus/mailbox/vcmailbox`): it owns
//! the discovered doorbell MMIO window and the DMA-backed property buffer and
//! answers *synchronous* property-channel exchanges from other user-space
//! drivers (e.g. the VL805 USB firmware reload, `drivers/bus/usb/vl805`). A
//! caller reaches the service through the kernel's synchronous call-endpoint
//! surface ([`SyscallNumber::IPC_CALL`](crate::SyscallNumber::IPC_CALL)) named
//! at the well-known [`MAILBOX_ENDPOINT`].
//!
//! This module is the wire contract for that endpoint, modelled on the
//! read-only driver store ([`crate::driver_store`]): borrowed buffers, no
//! allocation (the crate is `no_std`), and a status-framed reply so a
//! fail-closed refusal is delivered in-band rather than as a truncated
//! payload.
//!
//! The payload both directions carry is a fixed
//! [`MAILBOX_PROPERTY_WORDS`]-word `VideoCore` property buffer (the exact
//! buffer the board-neutral [`MailboxChannel`] seam exchanges); the request
//! is that buffer encoded little-endian, and a
//! successful reply is a status word of `0` followed by the firmware's
//! response buffer. The single buffer-shape definition lives with the seam.

use crate::driver::mailbox::{MailboxChannel, MAILBOX_PROPERTY_WORDS};
use crate::le::{put_i32, put_u32, read_i32, read_u32};
use crate::Errno;

/// Well-known kernel-owned call-endpoint id of the user-space `VideoCore`
/// mailbox service (`plans/PI.md` P10 D3).
///
/// The `vcmailbox` service creates one synchronous call endpoint under this
/// reserved id with [`SyscallNumber::CALL_CREATE`](crate::SyscallNumber::CALL_CREATE);
/// a driver that needs a firmware property exchange names it as the
/// `endpoint` argument to [`SyscallNumber::IPC_CALL`](crate::SyscallNumber::IPC_CALL).
/// A reserved well-known id (rather than a delegated handle) keeps the
/// driver/service rendezvous from needing a prior name-exchange step; the
/// endpoint's required send/receive capabilities still gate every call.
pub const MAILBOX_ENDPOINT: u64 = 0x4D42_5800;

/// Encoded length of a mailbox request: the [`MAILBOX_PROPERTY_WORDS`]-word
/// property buffer, little-endian. This is also the endpoint's maximum
/// request size.
pub const REQUEST_LEN: usize = MAILBOX_PROPERTY_WORDS * 4;

/// Fixed prefix of every reply frame: a status word (`0` on success, else
/// the negated [`Errno`] discriminant), mirroring [`crate::driver_store`].
const REPLY_STATUS_LEN: usize = 4;

/// Encoded length of a successful reply: the status word followed by the
/// firmware's [`MAILBOX_PROPERTY_WORDS`]-word response buffer. This is also
/// the endpoint's maximum reply size.
pub const REPLY_LEN: usize = REPLY_STATUS_LEN + MAILBOX_PROPERTY_WORDS * 4;

/// Encode a property-channel request into `buf`, returning the number of
/// bytes written.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold the [`REQUEST_LEN`]-byte
/// encoding.
pub fn encode_request(
    buf: &mut [u8],
    message: &[u32; MAILBOX_PROPERTY_WORDS],
) -> Result<usize, Errno> {
    if buf.len() < REQUEST_LEN {
        return Err(Errno::BufferTooSmall);
    }
    for (i, word) in message.iter().enumerate() {
        put_u32(buf, i * 4, *word);
    }
    Ok(REQUEST_LEN)
}

/// Decode a property-channel request from `bytes`.
///
/// # Errors
///
/// [`Errno::LengthOutOfRange`] if `bytes` is shorter than [`REQUEST_LEN`] —
/// a truncated request is rejected, never read past its bytes.
pub fn decode_request(bytes: &[u8]) -> Result<[u32; MAILBOX_PROPERTY_WORDS], Errno> {
    if bytes.len() < REQUEST_LEN {
        return Err(Errno::LengthOutOfRange);
    }
    let mut message = [0u32; MAILBOX_PROPERTY_WORDS];
    for (i, word) in message.iter_mut().enumerate() {
        *word = read_u32(bytes, i * 4);
    }
    Ok(message)
}

/// Encode a successful reply carrying the firmware's `response` buffer.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold the [`REPLY_LEN`]-byte
/// framed reply.
pub fn encode_reply(
    buf: &mut [u8],
    response: &[u32; MAILBOX_PROPERTY_WORDS],
) -> Result<usize, Errno> {
    if buf.len() < REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    for (i, word) in response.iter().enumerate() {
        put_u32(buf, REPLY_STATUS_LEN + i * 4, *word);
    }
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

/// Decode a reply, writing the firmware's response buffer into `out` on
/// success.
///
/// # Errors
///
/// The carried [`Errno`] for an error frame (e.g. the service's
/// [`DriverError`](crate::DriverError) mapped to an `Errno`), or
/// [`Errno::BadMagic`] if a success frame is truncated or the status word is
/// neither `0` nor a known negated discriminant (wire corruption — fail
/// closed), or [`Errno::BufferTooSmall`] if `reply` is
/// shorter than the status word.
pub fn decode_reply(reply: &[u8], out: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), Errno> {
    if reply.len() < REPLY_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(reply, 0) {
        0 => {
            if reply.len() < REPLY_LEN {
                return Err(Errno::BadMagic);
            }
            for (i, word) in out.iter_mut().enumerate() {
                *word = read_u32(reply, REPLY_STATUS_LEN + i * 4);
            }
            Ok(())
        }
        negative => Err(Errno::try_from_status(negative).unwrap_or(Errno::BadMagic)),
    }
}

/// Serve one request frame: decode it, run the property exchange over
/// `channel`, and encode the framed reply into `reply`, returning the number
/// of reply bytes written.
///
/// This is the wire-level server transformation the user-space `vcmailbox`
/// service runs between [`call_recv`](crate::SyscallNumber::CALL_RECV) and
/// [`call_reply`](crate::SyscallNumber::CALL_REPLY): the hardware mechanism
/// (the doorbell window + DMA property buffer) lives behind `channel`, so the
/// service keeps no protocol logic of its own. Every
/// failure — a malformed request or a [`DriverError`](crate::DriverError)
/// from the exchange — is turned into an in-band status-framed error reply,
/// so the blocked caller is always answered and fails closed.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `reply` cannot hold a status-framed reply
/// (it must be at least [`REPLY_LEN`] for a success and at least the status
/// word for an error). The caller sizes `reply` to [`REPLY_LEN`].
pub fn serve_request<C: MailboxChannel + ?Sized>(
    channel: &C,
    request: &[u8],
    reply: &mut [u8],
) -> Result<usize, Errno> {
    match decode_request(request) {
        Ok(mut message) => match channel.exchange(&mut message) {
            Ok(()) => encode_reply(reply, &message),
            Err(err) => encode_error_reply(reply, err.as_errno()),
        },
        Err(err) => encode_error_reply(reply, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DriverError;

    /// A [`MailboxChannel`] double: echoes a fixed response on success, or
    /// returns a programmed [`DriverError`].
    struct MockChannel {
        response: [u32; MAILBOX_PROPERTY_WORDS],
        result: Result<(), DriverError>,
    }

    impl MailboxChannel for MockChannel {
        fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError> {
            self.result?;
            *message = self.response;
            Ok(())
        }
    }

    fn sample() -> [u32; MAILBOX_PROPERTY_WORDS] {
        let mut words = [0u32; MAILBOX_PROPERTY_WORDS];
        for (i, word) in words.iter_mut().enumerate() {
            *word = 0x1000_0000 + u32::try_from(i).expect("index fits u32");
        }
        words
    }

    #[test]
    fn request_round_trips() {
        let message = sample();
        let mut buf = [0u8; REQUEST_LEN];
        let n = encode_request(&mut buf, &message).expect("encodes");
        assert_eq!(n, REQUEST_LEN);
        assert_eq!(decode_request(&buf[..n]), Ok(message));
    }

    #[test]
    fn request_encode_rejects_small_buffer() {
        let mut buf = [0u8; REQUEST_LEN - 1];
        assert_eq!(
            encode_request(&mut buf, &sample()),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn request_decode_rejects_truncated() {
        let buf = [0u8; REQUEST_LEN - 1];
        assert_eq!(decode_request(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn ok_reply_round_trips() {
        let response = sample();
        let mut buf = [0u8; REPLY_LEN];
        let n = encode_reply(&mut buf, &response).expect("encodes");
        assert_eq!(n, REPLY_LEN);
        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(decode_reply(&buf[..n], &mut out), Ok(()));
        assert_eq!(out, response);
    }

    #[test]
    fn error_reply_surfaces_its_errno() {
        let mut buf = [0u8; REPLY_LEN];
        let n = encode_error_reply(&mut buf, Errno::PermissionDenied).expect("encodes");
        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(
            decode_reply(&buf[..n], &mut out),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn truncated_ok_reply_fails_closed() {
        // Status ok but no response body.
        let mut buf = [0u8; REPLY_STATUS_LEN];
        put_i32(&mut buf, 0, 0);
        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(decode_reply(&buf, &mut out), Err(Errno::BadMagic));
    }

    #[test]
    fn corrupt_status_fails_closed() {
        let mut buf = [0u8; REPLY_LEN];
        // A negative status that is not any known negated discriminant.
        put_i32(&mut buf, 0, -9_999);
        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(decode_reply(&buf, &mut out), Err(Errno::BadMagic));
    }

    #[test]
    fn serve_request_round_trips_a_successful_exchange() {
        // The server transformation: a valid request runs the exchange and
        // the firmware's response is framed as a success reply the client
        // decodes back.
        let request = sample();
        let response = {
            let mut r = sample();
            r[0] = 0xAABB_CCDD;
            r
        };
        let channel = MockChannel {
            response,
            result: Ok(()),
        };
        let mut req_bytes = [0u8; REQUEST_LEN];
        encode_request(&mut req_bytes, &request).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&channel, &req_bytes, &mut reply).expect("serves");

        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(decode_reply(&reply[..n], &mut out), Ok(()));
        assert_eq!(out, response);
    }

    #[test]
    fn serve_request_frames_an_exchange_error_in_band() {
        // A `DriverError` from the exchange becomes a status-framed error
        // reply, so the blocked caller is answered and fails closed.
        let channel = MockChannel {
            response: sample(),
            result: Err(DriverError::DeviceFault),
        };
        let mut req_bytes = [0u8; REQUEST_LEN];
        encode_request(&mut req_bytes, &sample()).expect("encodes");
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&channel, &req_bytes, &mut reply).expect("serves");

        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        // `DeviceFault` maps to its distinct `Errno::DeviceFault` via
        // `as_errno`.
        assert_eq!(decode_reply(&reply[..n], &mut out), Err(Errno::DeviceFault));
    }

    #[test]
    fn serve_request_frames_a_malformed_request_in_band() {
        // A truncated request is never passed to the hardware: it is rejected
        // and answered with a status-framed error.
        let channel = MockChannel {
            response: sample(),
            result: Ok(()),
        };
        let short = [0u8; REQUEST_LEN - 1];
        let mut reply = [0u8; REPLY_LEN];
        let n = serve_request(&channel, &short, &mut reply).expect("serves");
        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(
            decode_reply(&reply[..n], &mut out),
            Err(Errno::LengthOutOfRange)
        );
    }
    #[test]
    fn the_most_negative_status_word_fails_closed_instead_of_aborting() {
        let mut reply = [0u8; REPLY_LEN];
        reply[..4].copy_from_slice(&i32::MIN.to_le_bytes());
        let mut out = [0u32; MAILBOX_PROPERTY_WORDS];
        assert_eq!(decode_reply(&reply, &mut out), Err(Errno::BadMagic));
    }
}
