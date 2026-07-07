//! Stable [`rustos_log::EventId`] constants emitted by `seatmgr`.
//!
//! Per `lib/log` convention every subsystem owns a 1 000-wide reserved
//! range. The seat-manager service occupies `14_000..15_000` (adjacent to
//! the device manager's `13_000..14_000`). Once shipped the numeric values
//! must never be re-used or re-numbered — external audit-log consumers rely
//! on them.

use rustos_log::EventId;

/// Range start (inclusive) reserved for `seatmgr` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const SEATMGR_RANGE_START: u32 = 14_000;
/// Range end (exclusive) reserved for `seatmgr` event identifiers.
pub const SEATMGR_RANGE_END: u32 = 15_000;

/// A seat-administration request was authorised and forwarded to the
/// kernel (which audits the switch/revoke itself with the seat and evicted
/// identity).
pub const SEAT_ADMIN_APPLIED: EventId = EventId(14_001);
/// A seat-administration request was refused: the requester's attested
/// origin does not carry `CAP_SEAT_ADMIN`, or the kernel refused the
/// operation. A denial is a security-relevant decision in its own right.
pub const SEAT_ADMIN_DENIED: EventId = EventId(14_002);
/// A request was rejected before dispatch: it failed to decode against
/// `seatmgr-v1`.
pub const REQUEST_MALFORMED: EventId = EventId(14_003);

#[cfg(test)]
mod tests {
    use super::{
        REQUEST_MALFORMED, SEATMGR_RANGE_END, SEATMGR_RANGE_START, SEAT_ADMIN_APPLIED,
        SEAT_ADMIN_DENIED,
    };

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in [SEAT_ADMIN_APPLIED, SEAT_ADMIN_DENIED, REQUEST_MALFORMED] {
            assert!(id.0 >= SEATMGR_RANGE_START && id.0 < SEATMGR_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = [
            SEAT_ADMIN_APPLIED.0,
            SEAT_ADMIN_DENIED.0,
            REQUEST_MALFORMED.0,
        ];
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate seatmgr EventId");
        }
    }
}
