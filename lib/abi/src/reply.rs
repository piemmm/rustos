//! The shared single-status IPC reply frame.
//!
//! Several reserved-endpoint services answer a request whose only outcome
//! is success or a typed refusal — seat administration
//! ([`crate::seat`]), the display service's configure/present operations
//! ([`crate::display_ipc`]). They all share this one reply shape: a
//! [`STATUS_REPLY_LEN`]-byte little-endian `i32` status word, `0` on
//! success and the negative [`Errno`] discriminant on refusal. Defining
//! the frame once keeps every consumer's encode and fail-closed decode
//! identical; a protocol whose reply carries payload (e.g. the display
//! mode reply) defines its own richer frame and never reuses this one
//! for it.

use crate::Errno;

/// Length, in bytes, of a status-only reply frame: one little-endian
/// `i32` status word.
pub const STATUS_REPLY_LEN: usize = 4;

/// Encode an operation outcome as the [`STATUS_REPLY_LEN`]-byte status
/// frame: `0` for success, the negative [`Errno`] discriminant for a
/// refusal.
#[must_use]
pub fn encode_status_reply(result: Result<(), Errno>) -> [u8; STATUS_REPLY_LEN] {
    let status: i32 = match result {
        Ok(()) => 0,
        Err(err) => -err.as_i32(),
    };
    status.to_le_bytes()
}

/// Decode a status-only reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the status word.
/// * [`Errno::OutOfRange`] — a non-zero status that is not a defined
///   negative [`Errno`] discriminant (fail closed on a corrupt frame).
/// * The decoded [`Errno`] itself, when the service refused the request.
pub fn decode_status_reply(bytes: &[u8]) -> Result<(), Errno> {
    if bytes.len() < STATUS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let mut raw = [0u8; STATUS_REPLY_LEN];
    raw.copy_from_slice(&bytes[..STATUS_REPLY_LEN]);
    let status = i32::from_le_bytes(raw);
    if status == 0 {
        return Ok(());
    }
    let errno = status
        .checked_neg()
        .and_then(Errno::from_i32)
        .ok_or(Errno::OutOfRange)?;
    Err(errno)
}

#[cfg(test)]
mod tests {
    use super::{decode_status_reply, encode_status_reply, STATUS_REPLY_LEN};
    use crate::Errno;

    #[test]
    fn round_trips_ok_and_every_errno() {
        assert_eq!(decode_status_reply(&encode_status_reply(Ok(()))), Ok(()));
        for errno in [
            Errno::PermissionDenied,
            Errno::NotFound,
            Errno::SeatNotOwner,
            Errno::SeatRevoked,
        ] {
            assert_eq!(
                decode_status_reply(&encode_status_reply(Err(errno))),
                Err(errno)
            );
        }
    }

    #[test]
    fn fails_closed_on_short_and_corrupt_frames() {
        assert_eq!(
            decode_status_reply(&[0u8; STATUS_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // `i32::MIN` cannot be negated; an undefined discriminant and a
        // positive status word are both refused rather than guessed.
        assert_eq!(
            decode_status_reply(&i32::MIN.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            decode_status_reply(&1i32.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            decode_status_reply(&(-9999i32).to_le_bytes()),
            Err(Errno::OutOfRange)
        );
    }
}
