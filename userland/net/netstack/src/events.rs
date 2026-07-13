//! Stable [`rustos_log::EventId`] constants emitted by `netstack`.
//!
//! Per `lib/log` convention every subsystem owns a 1 000-wide reserved
//! range. The network-stack service occupies `16000..17000` (adjacent to
//! the display service's `15000..16000`). Once shipped the numeric values
//! must never be re-used or re-numbered — external audit-log consumers
//! rely on them.

use rustos_log::EventId;

/// Range start (inclusive) reserved for `netstack` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const NETSTACK_RANGE_START: u32 = 16_000;
/// Range end (exclusive) reserved for `netstack` event identifiers.
pub const NETSTACK_RANGE_END: u32 = 17_000;

/// An admin mutation (address or route add) was applied.
///
/// Recorded at `Info`: interface configuration changes are rare,
/// security-relevant state transitions that must always surface.
pub const ADMIN_APPLIED: EventId = EventId(16_001);
/// A request was refused because the caller lacks its required
/// capability (`CAP_NET_ADMIN` for the admin surface,
/// `CAP_SYSINFO_INTROSPECT` for the broker reads).
///
/// A denial is a security-relevant decision in its own right and is
/// always recorded, at `Warn`.
pub const REQUEST_DENIED: EventId = EventId(16_002);
/// A request was rejected before dispatch: the frame failed to decode.
pub const REQUEST_MALFORMED: EventId = EventId(16_003);
/// An admin mutation named an interface the stack does not manage, or
/// the engine refused the new configuration (bad prefix, table full).
pub const ADMIN_REFUSED: EventId = EventId(16_004);

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_APPLIED, ADMIN_REFUSED, NETSTACK_RANGE_END, NETSTACK_RANGE_START, REQUEST_DENIED,
        REQUEST_MALFORMED,
    };

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in [
            ADMIN_APPLIED,
            REQUEST_DENIED,
            REQUEST_MALFORMED,
            ADMIN_REFUSED,
        ] {
            assert!(id.0 >= NETSTACK_RANGE_START && id.0 < NETSTACK_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = [
            ADMIN_APPLIED.0,
            REQUEST_DENIED.0,
            REQUEST_MALFORMED.0,
            ADMIN_REFUSED.0,
        ];
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate netstack EventId");
        }
    }
}
