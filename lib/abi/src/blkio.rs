//! The block-service transport IPC protocol (`plans/DEVICES.md` D2).
//!
//! A user-space block driver (the first is the USB mass-storage class
//! driver, `drivers/storage/usb_msd`) exposes each logical unit it brings
//! up as a **block-service call endpoint** plus a shared-memory data
//! window, both forwarded as grants on the storage-class hardware-tree
//! node it emits. A consumer (the volume manager, `plans/DEVICES.md` D3)
//! inherits those grants, maps the window, and drives the device with the
//! fixed-size request/completion frames defined here — the same
//! request-reply IPC shape as the URB transport ([`crate::usb_urb`]).
//!
//! Only the request and the completion cross the endpoint; the data being
//! read or written lives in the separately-mapped shared window
//! ([`BLK_DATA_LEN`] bytes, always at offset zero). The serving driver
//! validates every field against the live device geometry before touching
//! hardware and fails closed; a request larger than the window is refused,
//! and a consumer moves larger transfers as multiple bounded requests, so
//! the per-device cost is fixed rather than a function of request size.

use crate::le::{put_i32, put_u32, put_u64, read_i32, read_u32, read_u64};
use crate::Errno;

/// Length of the shared-memory data window a block-service endpoint
/// serves transfers through, and therefore the largest single transfer
/// one request may name. A multiple of every supported block size, sized
/// to amortise per-request cost without scaling per-device memory with
/// request length (the `virtio_blk` staging-window precedent).
pub const BLK_DATA_LEN: usize = 32 * 1024;

/// One block-service operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BlkOp {
    /// Query the device geometry (block size, block count, flags). Carries
    /// no data and names no blocks.
    Geometry = 0,
    /// Read [`BlkRequest::blocks`] blocks starting at [`BlkRequest::lba`]
    /// into the shared data window (offset zero).
    Read = 1,
    /// Write [`BlkRequest::blocks`] blocks from the shared data window
    /// (offset zero) starting at [`BlkRequest::lba`].
    Write = 2,
    /// Commit every completed write to the medium (the SCSI
    /// `SYNCHRONIZE CACHE` / virtio flush equivalent). Carries no data and
    /// names no blocks.
    Flush = 3,
}

impl BlkOp {
    /// The wire byte for this operation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover an operation from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known operation (fail
    /// closed; any future opcode is refused here).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Geometry),
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            3 => Ok(Self::Flush),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One block-service request: a single bounded operation against the
/// logical unit the endpoint serves.
///
/// The transfer's payload lives in the shared data window; the request
/// carries only the operation's shape. The serving driver validates every
/// field against the live geometry (range, alignment, the window bound)
/// before it touches the device and fails closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BlkRequest {
    /// The operation.
    pub op: BlkOp,
    /// First logical block of a [`BlkOp::Read`] / [`BlkOp::Write`]; zero
    /// for the data-less operations.
    pub lba: u64,
    /// Number of blocks a [`BlkOp::Read`] / [`BlkOp::Write`] covers
    /// (`blocks * block_size` bytes in the shared window, never more than
    /// [`BLK_DATA_LEN`]); zero for the data-less operations.
    pub blocks: u32,
}

/// Encoded length of a [`BlkRequest`]: `op(1) || pad(3) || blocks(4) ||
/// lba(8)`. Fixed — every request encodes to the same size, so this is
/// both the encoding length and the endpoint's maximum request size.
pub const BLK_REQUEST_LEN: usize = 1 + 3 + 4 + 8;

impl BlkRequest {
    /// Encode `self` into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold [`BLK_REQUEST_LEN`]
    /// bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.len() < BLK_REQUEST_LEN {
            return Err(Errno::BufferTooSmall);
        }
        buf[0] = self.op.as_u8();
        buf[1] = 0;
        buf[2] = 0;
        buf[3] = 0;
        put_u32(buf, 4, self.blocks);
        put_u64(buf, 8, self.lba);
        Ok(BLK_REQUEST_LEN)
    }

    /// Decode a block-service request from `bytes`, validating every field.
    ///
    /// This rejects a malformed *encoding* (truncation, an unknown opcode,
    /// a data-less operation naming blocks); the serving driver performs
    /// the further *semantic* checks that need the live device (range
    /// against the geometry, the window bound) before it queues anything.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is shorter than
    ///   [`BLK_REQUEST_LEN`] — a truncated request is never read past its
    ///   bytes.
    /// * [`Errno::OutOfRange`] if the opcode byte is unknown, or a
    ///   data-less operation ([`BlkOp::Geometry`] / [`BlkOp::Flush`])
    ///   carries a non-zero `lba` or `blocks` (fail closed on a frame that
    ///   cannot mean what it says).
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < BLK_REQUEST_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let op = BlkOp::from_u8(bytes[0])?;
        let blocks = read_u32(bytes, 4);
        let lba = read_u64(bytes, 8);
        if matches!(op, BlkOp::Geometry | BlkOp::Flush) && (lba != 0 || blocks != 0) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { op, lba, blocks })
    }
}

/// Fixed prefix of every completion frame: a status word (`0` on success,
/// else the negated [`Errno`] discriminant), mirroring [`crate::usb_urb`].
const COMPLETION_STATUS_LEN: usize = 4;

/// Encoded length of a block-service completion: the status word followed
/// by the geometry payload (`block_size(4) || block_count(8) || flags(4)`,
/// zero-filled for the non-geometry operations). Also the endpoint's
/// maximum reply size.
pub const BLK_COMPLETION_LEN: usize = COMPLETION_STATUS_LEN + 4 + 8 + 4;

/// [`BlkCompletion::flags`] bit: the logical unit is write-protected; a
/// [`BlkOp::Write`] will be refused [`Errno::PermissionDenied`].
pub const BLK_FLAG_READ_ONLY: u32 = 1;

/// A successful block-service completion.
///
/// Only a [`BlkOp::Geometry`] reply carries meaningful fields; the other
/// operations complete with a zero-filled payload (a read or write either
/// moved exactly the requested blocks or failed — there are no partial
/// completions on this seam).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BlkCompletion {
    /// Size of one logical block, in bytes.
    pub block_size: u32,
    /// Total number of logical blocks.
    pub block_count: u64,
    /// Device flags ([`BLK_FLAG_READ_ONLY`]).
    pub flags: u32,
}

impl BlkCompletion {
    /// Encode `self` as a success completion into `buf`, returning the
    /// number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold
    /// [`BLK_COMPLETION_LEN`] bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.len() < BLK_COMPLETION_LEN {
            return Err(Errno::BufferTooSmall);
        }
        put_i32(buf, 0, 0);
        put_u32(buf, COMPLETION_STATUS_LEN, self.block_size);
        put_u64(buf, COMPLETION_STATUS_LEN + 4, self.block_count);
        put_u32(buf, COMPLETION_STATUS_LEN + 12, self.flags);
        Ok(BLK_COMPLETION_LEN)
    }
}

/// Encode a fail-closed error completion (status only) into `buf`.
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

/// Decode a block-service completion: the payload on success, else the
/// carried [`Errno`].
///
/// # Errors
///
/// The carried [`Errno`] for an error frame (e.g. a device fault, or
/// [`Errno::PermissionDenied`] for a refused write), or
/// [`Errno::BadMagic`] if a success frame is truncated or the status word
/// is neither `0` nor a known negated discriminant (wire corruption —
/// fail closed), or [`Errno::BufferTooSmall`] if `reply` is shorter than
/// the status word.
pub fn decode_completion(reply: &[u8]) -> Result<BlkCompletion, Errno> {
    if reply.len() < COMPLETION_STATUS_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(reply, 0) {
        0 => {
            if reply.len() < BLK_COMPLETION_LEN {
                return Err(Errno::BadMagic);
            }
            Ok(BlkCompletion {
                block_size: read_u32(reply, COMPLETION_STATUS_LEN),
                block_count: read_u64(reply, COMPLETION_STATUS_LEN + 4),
                flags: read_u32(reply, COMPLETION_STATUS_LEN + 12),
            })
        }
        // `checked_neg` guards `i32::MIN`, whose negation overflows; such a
        // status is not a valid negated discriminant, so it fails closed.
        negative => Err(negative
            .checked_neg()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::BadMagic)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = BlkRequest {
            op: BlkOp::Read,
            lba: 0x1_0000_0001,
            blocks: 64,
        };
        let mut buf = [0u8; BLK_REQUEST_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, BLK_REQUEST_LEN);
        assert_eq!(BlkRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn request_encode_rejects_small_buffer() {
        let req = BlkRequest {
            op: BlkOp::Flush,
            lba: 0,
            blocks: 0,
        };
        let mut buf = [0u8; BLK_REQUEST_LEN - 1];
        assert_eq!(req.encode(&mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn request_decode_rejects_truncated() {
        let buf = [0u8; BLK_REQUEST_LEN - 1];
        assert_eq!(BlkRequest::decode(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn request_decode_rejects_unknown_op() {
        let mut buf = [0u8; BLK_REQUEST_LEN];
        buf[0] = 4;
        assert_eq!(BlkRequest::decode(&buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn request_decode_rejects_dataless_op_naming_blocks() {
        for op in [BlkOp::Geometry, BlkOp::Flush] {
            let req = BlkRequest {
                op,
                lba: 0,
                blocks: 1,
            };
            let mut buf = [0u8; BLK_REQUEST_LEN];
            req.encode(&mut buf).expect("encodes");
            assert_eq!(BlkRequest::decode(&buf), Err(Errno::OutOfRange));

            let req = BlkRequest {
                op,
                lba: 1,
                blocks: 0,
            };
            req.encode(&mut buf).expect("encodes");
            assert_eq!(BlkRequest::decode(&buf), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn completion_round_trips() {
        let completion = BlkCompletion {
            block_size: 512,
            block_count: 0x2_0000_0000,
            flags: BLK_FLAG_READ_ONLY,
        };
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        let n = completion.encode(&mut buf).expect("encodes");
        assert_eq!(n, BLK_COMPLETION_LEN);
        assert_eq!(decode_completion(&buf[..n]), Ok(completion));
    }

    #[test]
    fn completion_encode_rejects_small_buffer() {
        let mut buf = [0u8; BLK_COMPLETION_LEN - 1];
        assert_eq!(
            BlkCompletion::default().encode(&mut buf),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn error_completion_round_trips() {
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        let n = encode_error_completion(&mut buf, Errno::PermissionDenied).expect("encodes");
        assert_eq!(decode_completion(&buf[..n]), Err(Errno::PermissionDenied));
    }

    #[test]
    fn truncated_success_completion_fails_closed() {
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        BlkCompletion::default().encode(&mut buf).expect("encodes");
        assert_eq!(
            decode_completion(&buf[..BLK_COMPLETION_LEN - 1]),
            Err(Errno::BadMagic)
        );
        assert_eq!(decode_completion(&buf[..2]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn unknown_negative_status_fails_closed() {
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        put_i32(&mut buf, 0, i32::MIN);
        assert_eq!(decode_completion(&buf), Err(Errno::BadMagic));
        put_i32(&mut buf, 0, -30_000);
        assert_eq!(decode_completion(&buf), Err(Errno::BadMagic));
    }

    #[test]
    fn window_holds_whole_blocks_of_every_supported_size() {
        for block_size in [512usize, 4096] {
            assert_eq!(BLK_DATA_LEN % block_size, 0);
        }
    }
}
