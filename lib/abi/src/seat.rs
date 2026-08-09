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
//! shared [`SEATMGR_REPLY_LEN`]-byte status frame
//! ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]) carrying `0` or a negative
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

/// Seat id of the **boot seat** every Tier-1 image hosts — the seat that
/// always exists (a text-only seat on a headless build) and owns the
/// directly attached keyboard. Every further seat is minted by hardware
/// discovery, one per display-class node published into the live tree,
/// with monotonic, never-reused ids (`plans/DISPLAY.md` D6); the
/// seat-addressed syscalls validate a request's seat id against that live
/// topology and fail closed with `NotFound` for a seat that does not (or
/// no longer) exist.
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
    /// The seat the lease was granted on ([`SEAT_PRIMARY`], or a
    /// discovery-minted seat id from `SEAT_LIST`).
    pub seat_id: u64,
    /// The kernel-attested task id the seat recorded as its owner.
    pub owner_task: u64,
    /// The seat's monotonic grant counter at mint time; starts at 1 and
    /// never repeats for a given seat.
    pub generation: u64,
}

/// What a `display_release` says becomes of the seat's screen.
///
/// One scan-out has one presenter, so the instant a lease ends the kernel
/// must decide what the screen shows. Only the releasing owner knows why it
/// is giving the seat up, and the two answers want opposite things, so the
/// release states its intent rather than the kernel guessing.
///
/// Neither disposition is a licence: an owner that claims a handover and
/// never returns has not blanked the machine, because the text console takes
/// the screen back the moment a program writes to it — a text login, or a
/// stated failure. Only the kernel's own routine diagnostics stay off a
/// screen promised to an incoming presenter, and they are in the log.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ReleaseSurface {
    /// The seat is going back to its text console, which takes the screen
    /// back and repaints its whole retained screen — so a user leaving a
    /// graphical session finds the terminal they left.
    Text,
    /// Another graphical presenter is taking the seat. The screen is
    /// cleared and held cleared until that presenter's first frame, so the
    /// gap shows neither the outgoing session's pixels — one principal's
    /// screen is never shown to the next — nor a replay of a text console
    /// the user is not returning to.
    Handover,
}

impl ReleaseSurface {
    /// Syscall-argument value of [`Self::Text`].
    const TEXT: u64 = 0;
    /// Syscall-argument value of [`Self::Handover`].
    const HANDOVER: u64 = 1;

    /// This disposition as the second `display_release` argument.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::Text => Self::TEXT,
            Self::Handover => Self::HANDOVER,
        }
    }

    /// Decode the second `display_release` argument.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for anything outside the closed set: an
    /// unrecognised disposition is refused, never read as a default.
    pub const fn from_u64(value: u64) -> Result<Self, Errno> {
        match value {
            Self::TEXT => Ok(Self::Text),
            Self::HANDOVER => Ok(Self::Handover),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Magic number identifying a seat-administration request (`"STA1"`
/// little-endian).
pub const SEATMGR_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"STA1");

/// The `seatmgr-v1` protocol version.
pub const SEATMGR_VERSION_V1: u16 = 1;

/// Maximum request, in bytes, the [`SEATMGR_ENDPOINT`] accepts: exactly one
/// fixed-width [`SeatAdminRequest`].
pub const SEATMGR_MAX_REQUEST: usize = SeatAdminRequest::WIRE_LEN;

/// Reply length, in bytes: the shared status frame — `0` on success, a
/// negative [`Errno`] discriminant on refusal.
pub const SEATMGR_REPLY_LEN: usize = crate::reply::STATUS_REPLY_LEN;

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

#[cfg(test)]
mod tests {
    use super::{SeatAdminRequest, SEATMGR_REPLY_LEN, SEATMGR_REQUEST_MAGIC};
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
    fn reply_length_is_the_shared_status_frame() {
        assert_eq!(SEATMGR_REPLY_LEN, crate::reply::STATUS_REPLY_LEN);
    }
}
