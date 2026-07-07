//! The seat-manager service ABI (`plans/DISPLAY.md` D3): the reserved
//! `seatmgr` rendezvous and the typed seat-administration request it serves.
//!
//! The seat manager (`userland/system/seatmgr`) is the sole holder of
//! `CAP_SEAT_ADMIN` — the seat-multiplexing authority. It binds the
//! reserved [`SEATMGR_ENDPOINT`] (squat-protected by
//! [`crate::ipc::is_reserved_endpoint`]) and serves the two administrative
//! operations over the kernel's `seat_switch` / `seat_revoke` syscalls,
//! gating every request on the *requester's* kernel-attested
//! `CAP_SEAT_ADMIN` — the same broker discipline `sysinfod` applies to its
//! privileged queries, so administrative seat policy flows through one
//! audited service rather than ad-hoc syscall callers.
//!
//! Requests are the fixed-width [`SeatAdminRequest`]; the reply is the
//! [`SEATMGR_REPLY_LEN`]-byte status frame carrying `0` or a negative
//! [`Errno`] discriminant. Every decode fails closed: an unknown magic,
//! version, operation, or a dirty reserved field refuses rather than
//! guessing.

use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::Errno;

/// Reserved well-known call-endpoint id of the seat-manager service
/// (`"ST"` prefix, mirroring [`crate::sysinfo::SYSINFO_ENDPOINT`]'s
/// convention). Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the
/// rendezvous first would receive seat-administration requests meant for
/// the service.
pub const SEATMGR_ENDPOINT: u64 = 0x5354_1001;

/// Seat id of the primary (and, until `plans/DISPLAY.md` D6 lands
/// multi-seat, only) seat every Tier-1 image hosts. The `seat_switch` /
/// `seat_revoke` syscalls validate a request's seat id against the live
/// topology; today that topology is exactly this seat.
pub const SEAT_PRIMARY: u64 = 0;

/// One granted seat hold, as the client-visible handle: which seat, the
/// kernel-attested owning task, and the per-seat monotonic generation the
/// grant was minted under (`display_acquire` returns it).
///
/// The generation is what makes the handle *revocation-proof in the right
/// direction*: after a `seat_revoke`, or a release-and-reacquire, the live
/// lease carries a newer generation, so a stale pre-revoke handle can
/// never be mistaken for the live grant. The kernel threads this handle
/// into a display driver's host as the
/// [`SeatGate`](crate::driver::display::SeatGate) it derives the present
/// right from — holding a framebuffer mapping does not imply owning the
/// screen.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SeatLease {
    /// The seat the lease was granted on ([`SEAT_PRIMARY`] today).
    pub seat_id: u64,
    /// The kernel-attested task id the seat recorded as its owner.
    pub owner_task: u64,
    /// The seat's monotonic grant counter at mint time; starts at 1 and
    /// never repeats for a given seat.
    pub generation: u64,
}

/// Magic number identifying a seat-administration request (`"STA1"`
/// little-endian).
pub const SEATMGR_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"STA1");

/// The `seatmgr-v1` protocol version.
pub const SEATMGR_VERSION_V1: u16 = 1;

/// Maximum request, in bytes, the [`SEATMGR_ENDPOINT`] accepts: exactly one
/// fixed-width [`SeatAdminRequest`].
pub const SEATMGR_MAX_REQUEST: usize = SeatAdminRequest::WIRE_LEN;

/// Reply length, in bytes: one little-endian `i32` status word — `0` on
/// success, a negative [`Errno`] discriminant on refusal.
pub const SEATMGR_REPLY_LEN: usize = 4;

/// One seat-administration operation (`plans/DISPLAY.md` D3).
///
/// Both variants act on a whole seat and are the `CAP_SEAT_ADMIN`
/// authority's alone; the kernel re-checks the capability and the seat and
/// console indices when the service issues the corresponding syscall, so a
/// compromised service still cannot exceed the syscall's own validation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SeatAdminRequest {
    /// Switch `seat_id`'s foreground to the installed text console
    /// `console` (the `chvt` analogue; `seat_switch`).
    Switch {
        /// The seat to retarget.
        seat_id: u64,
        /// Index of the installed text console that becomes the foreground.
        console: u32,
    },
    /// Forcibly revoke `seat_id`'s current lease (`seat_revoke`); the
    /// evicted owner's next owner-gated call fails closed with
    /// `SeatRevoked`.
    Revoke {
        /// The seat whose lease is revoked.
        seat_id: u64,
    },
}

/// Wire operation discriminant of [`SeatAdminRequest::Switch`].
const OP_SWITCH: u16 = 1;
/// Wire operation discriminant of [`SeatAdminRequest::Revoke`].
const OP_REVOKE: u16 = 2;

impl SeatAdminRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), seat id
    /// (8), console (4), reserved (4).
    pub const WIRE_LEN: usize = 24;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, SEATMGR_REQUEST_MAGIC);
        put_u16(&mut out, 4, SEATMGR_VERSION_V1);
        match *self {
            Self::Switch { seat_id, console } => {
                put_u16(&mut out, 6, OP_SWITCH);
                put_u64(&mut out, 8, seat_id);
                put_u32(&mut out, 16, console);
            }
            Self::Revoke { seat_id } => {
                put_u16(&mut out, 6, OP_REVOKE);
                put_u64(&mut out, 8, seat_id);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, a dirty reserved field, or a
    ///   `Revoke` smuggling a console value.
    /// * [`Errno::AbiVersionUnsupported`] — not `seatmgr-v1`.
    /// * [`Errno::OutOfRange`] — an operation outside the closed set.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != SEATMGR_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != SEATMGR_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if read_u32(bytes, 20) != 0 {
            return Err(Errno::BadMagic);
        }
        let seat_id = read_u64(bytes, 8);
        let console = read_u32(bytes, 16);
        match read_u16(bytes, 6) {
            OP_SWITCH => Ok(Self::Switch { seat_id, console }),
            OP_REVOKE => {
                // A revoke carries no console; a non-zero value is wire
                // corruption, refused rather than silently ignored.
                if console != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Revoke { seat_id })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Encode a seat-administration outcome as the [`SEATMGR_REPLY_LEN`]-byte
/// status frame: `0` for success, the negative [`Errno`] discriminant for a
/// refusal.
#[must_use]
pub fn encode_seat_reply(result: Result<(), Errno>) -> [u8; SEATMGR_REPLY_LEN] {
    let status: i32 = match result {
        Ok(()) => 0,
        Err(err) => -err.as_i32(),
    };
    status.to_le_bytes()
}

/// Decode a seat-administration reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold the status word.
/// * [`Errno::OutOfRange`] — a non-zero status that is not a defined
///   negative [`Errno`] discriminant (fail closed on a corrupt frame).
/// * The decoded [`Errno`] itself, when the service refused the request.
pub fn decode_seat_reply(bytes: &[u8]) -> Result<(), Errno> {
    if bytes.len() < SEATMGR_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let mut raw = [0u8; SEATMGR_REPLY_LEN];
    raw.copy_from_slice(&bytes[..SEATMGR_REPLY_LEN]);
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
    use super::{
        decode_seat_reply, encode_seat_reply, SeatAdminRequest, SEATMGR_REPLY_LEN,
        SEATMGR_REQUEST_MAGIC,
    };
    use crate::Errno;

    #[test]
    fn requests_round_trip() {
        for request in [
            SeatAdminRequest::Switch {
                seat_id: 0,
                console: 2,
            },
            SeatAdminRequest::Revoke { seat_id: 0 },
        ] {
            let bytes = request.to_le_bytes();
            assert_eq!(SeatAdminRequest::from_bytes(&bytes), Ok(request));
        }
    }

    #[test]
    fn decode_fails_closed_on_malformed_input() {
        let good = SeatAdminRequest::Switch {
            seat_id: 0,
            console: 1,
        }
        .to_le_bytes();

        // Short buffer.
        assert_eq!(
            SeatAdminRequest::from_bytes(&good[..SeatAdminRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // Corrupt magic.
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(
            SeatAdminRequest::from_bytes(&bad_magic),
            Err(Errno::BadMagic)
        );
        // Unsupported version.
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            SeatAdminRequest::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        // Unknown operation.
        let mut bad_op = good;
        bad_op[6] = 9;
        assert_eq!(
            SeatAdminRequest::from_bytes(&bad_op),
            Err(Errno::OutOfRange)
        );
        // Dirty reserved tail.
        let mut dirty = good;
        dirty[20] = 1;
        assert_eq!(SeatAdminRequest::from_bytes(&dirty), Err(Errno::BadMagic));
        // A revoke must not smuggle a console value.
        let mut revoke = SeatAdminRequest::Revoke { seat_id: 0 }.to_le_bytes();
        revoke[16] = 3;
        assert_eq!(SeatAdminRequest::from_bytes(&revoke), Err(Errno::BadMagic));
    }

    #[test]
    fn magic_is_the_ascii_tag() {
        assert_eq!(SEATMGR_REQUEST_MAGIC, u32::from_le_bytes(*b"STA1"));
    }

    #[test]
    fn replies_round_trip_ok_and_error() {
        assert_eq!(decode_seat_reply(&encode_seat_reply(Ok(()))), Ok(()));
        assert_eq!(
            decode_seat_reply(&encode_seat_reply(Err(Errno::PermissionDenied))),
            Err(Errno::PermissionDenied)
        );
        // Fail closed: a short frame and an undefined status word.
        assert_eq!(
            decode_seat_reply(&[0u8; SEATMGR_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(
            decode_seat_reply(&i32::MIN.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            decode_seat_reply(&1i32.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
    }
}
