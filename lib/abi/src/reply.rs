//! The shared IPC reply frames: a status-only frame, and a paged frame
//! built on top of it.
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
//!
//! Other services answer a request with a *list* of fixed-width records,
//! paged to bound both the reply's size and the work a single call does —
//! the network stack's interface/socket/route enumerations, and any
//! future broker with the same shape. Their reply is the status frame
//! above, followed by a record count, a reserved pair that must be zero,
//! then the records packed back-to-back. [`encode_page_reply`] and
//! [`decode_page_reply`] are that one paged codec, so every such service
//! shares the same wire shape and the same fail-closed decode instead of
//! each defining its own.
//!
//! The maximum records a page may carry is a **caller-supplied `limit`
//! parameter**, not a constant baked into this module. This module
//! answers to no one protocol: the limit is a per-protocol validation
//! bound (how large a page *that* protocol's transport and buffers are
//! willing to carry), and different protocols sharing this codec have no
//! reason to agree on one number. Hard-coding a single limit here would
//! either force every consumer to accept the first protocol's bound or
//! reintroduce a second, protocol-specific copy of the codec — exactly
//! the duplication this module exists to avoid.

use crate::le::{put_u16, read_u16};
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
    let errno = Errno::try_from_status(status).ok_or(Errno::OutOfRange)?;
    Err(errno)
}

/// Byte length of the page header following the status word: the
/// record count (2) and a reserved pair that must be zero (2).
pub const PAGE_HEADER_LEN: usize = 4;

/// Encode a paged reply: the status frame, the count, then `records`
/// packed back-to-back (each already encoded to its fixed width).
///
/// `limit` is the caller's own maximum records-per-page bound; it is a
/// validation bound the protocol chooses for itself, not a capacity this
/// codec imposes.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — more records than `limit`.
/// * [`Errno::BufferTooSmall`] — `out` cannot hold the reply.
pub fn encode_page_reply<const RECORD_LEN: usize>(
    records: &[[u8; RECORD_LEN]],
    limit: u16,
    out: &mut [u8],
) -> Result<usize, Errno> {
    if records.len() > limit as usize {
        return Err(Errno::LengthOutOfRange);
    }
    let total = STATUS_REPLY_LEN + PAGE_HEADER_LEN + records.len() * RECORD_LEN;
    if out.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    out[..STATUS_REPLY_LEN].copy_from_slice(&encode_status_reply(Ok(())));
    // Record count fits u16: bounded by `limit` above.
    let count = u16::try_from(records.len()).map_err(|_| Errno::LengthOutOfRange)?;
    put_u16(out, STATUS_REPLY_LEN, count);
    put_u16(out, STATUS_REPLY_LEN + 2, 0);
    let mut cursor = STATUS_REPLY_LEN + PAGE_HEADER_LEN;
    for record in records {
        out[cursor..cursor + RECORD_LEN].copy_from_slice(record);
        cursor += RECORD_LEN;
    }
    Ok(total)
}

/// Decode a paged reply's header, returning the record region.
///
/// `limit` is the same caller-chosen maximum records-per-page bound
/// [`encode_page_reply`] was given; a declared count above it is refused
/// as a corrupt or hostile frame rather than trusted.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the declared
///   records.
/// * [`Errno::BadMagic`] — a dirty reserved pair.
/// * [`Errno::LengthOutOfRange`] — a count beyond `limit`.
/// * The decoded [`Errno`] itself, when the service refused the
///   request.
pub fn decode_page_reply(
    bytes: &[u8],
    record_len: usize,
    limit: u16,
) -> Result<(u16, &[u8]), Errno> {
    decode_status_reply(&bytes[..bytes.len().min(STATUS_REPLY_LEN)])?;
    if bytes.len() < STATUS_REPLY_LEN + PAGE_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let count = read_u16(bytes, STATUS_REPLY_LEN);
    if read_u16(bytes, STATUS_REPLY_LEN + 2) != 0 {
        return Err(Errno::BadMagic);
    }
    if count > limit {
        return Err(Errno::LengthOutOfRange);
    }
    let body = &bytes[STATUS_REPLY_LEN + PAGE_HEADER_LEN..];
    let need = count as usize * record_len;
    if body.len() < need {
        return Err(Errno::BufferTooSmall);
    }
    Ok((count, &body[..need]))
}

#[cfg(test)]
mod tests {
    use super::{
        decode_page_reply, decode_status_reply, encode_page_reply, encode_status_reply,
        PAGE_HEADER_LEN, STATUS_REPLY_LEN,
    };
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

    /// A page reply large enough for every test below: the status frame,
    /// the header, and up to eight 4-byte records.
    const PAGE_BUF_LEN: usize = STATUS_REPLY_LEN + PAGE_HEADER_LEN + 8 * 4;

    fn record(tag: u8) -> [u8; 4] {
        [tag, tag, tag, tag]
    }

    #[test]
    fn page_reply_round_trips_and_fails_closed() {
        let records = [record(1), record(2)];
        let mut out = [0u8; PAGE_BUF_LEN];
        let len = encode_page_reply(&records, 8, &mut out).expect("encode");
        let (count, body) = decode_page_reply(&out[..len], 4, 8).expect("decode");
        assert_eq!(count, 2);
        assert_eq!(body, [1, 1, 1, 1, 2, 2, 2, 2]);
        // A truncated body fails closed.
        assert_eq!(
            decode_page_reply(&out[..len - 1], 4, 8),
            Err(Errno::BufferTooSmall)
        );
        // A dirty reserved pair fails closed.
        let mut dirty = out;
        dirty[STATUS_REPLY_LEN + 2] = 1;
        assert_eq!(decode_page_reply(&dirty[..len], 4, 8), Err(Errno::BadMagic));
        // A refusal decodes to its errno.
        let refusal = encode_status_reply(Err(Errno::PermissionDenied));
        assert_eq!(
            decode_page_reply(&refusal, 4, 8),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn page_at_exactly_the_limit_round_trips() {
        let records = [record(1), record(2), record(3)];
        let mut out = [0u8; PAGE_BUF_LEN];
        let len = encode_page_reply(&records, 3, &mut out).expect("encode at limit");
        let (count, body) = decode_page_reply(&out[..len], 4, 3).expect("decode at limit");
        assert_eq!(count, 3);
        assert_eq!(body.len(), 3 * 4);
    }

    #[test]
    fn one_more_than_the_limit_refuses_to_encode() {
        let records = [record(1), record(2), record(3), record(4)];
        let mut out = [0u8; PAGE_BUF_LEN];
        assert_eq!(
            encode_page_reply(&records, 3, &mut out),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn a_declared_count_above_the_decoder_limit_is_refused() {
        let records = [record(1), record(2), record(3), record(4)];
        let mut out = [0u8; PAGE_BUF_LEN];
        // Encoded against a generous limit, but decoded against a
        // stricter one — the frame's own count must still respect
        // whatever bound the decoder was given.
        let len = encode_page_reply(&records, 8, &mut out).expect("encode");
        assert_eq!(
            decode_page_reply(&out[..len], 4, 3),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn two_different_limits_do_not_interfere() {
        let small = [record(1)];
        let large = [record(1), record(2), record(3), record(4), record(5)];
        let mut small_out = [0u8; PAGE_BUF_LEN];
        let mut large_out = [0u8; PAGE_BUF_LEN];
        let small_len = encode_page_reply(&small, 1, &mut small_out).expect("encode small");
        let large_len = encode_page_reply(&large, 5, &mut large_out).expect("encode large");
        let (small_count, _) =
            decode_page_reply(&small_out[..small_len], 4, 1).expect("decode small");
        let (large_count, _) =
            decode_page_reply(&large_out[..large_len], 4, 5).expect("decode large");
        assert_eq!(small_count, 1);
        assert_eq!(large_count, 5);
        // Each frame is still bound by its own protocol's limit: the
        // small page's single record would also be refused against the
        // stricter limit of zero, independent of the large page's frame.
        assert_eq!(
            decode_page_reply(&small_out[..small_len], 4, 0),
            Err(Errno::LengthOutOfRange)
        );
    }
}
