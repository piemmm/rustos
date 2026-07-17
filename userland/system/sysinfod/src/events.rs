//! Stable [`tairix_log::EventId`] constants emitted by `sysinfod`.
//!
//! Per `lib/log` convention every subsystem owns a
//! 1 000-wide reserved range. The System Information service occupies
//! `8000..9000` (adjacent to the driver host's `7000..8000`). Once shipped
//! the numeric values must never be re-used or re-numbered — external
//! audit-log consumers rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `sysinfod` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const SYSINFOD_RANGE_START: u32 = 8_000;
/// Range end (exclusive) reserved for `sysinfod` event identifiers.
pub const SYSINFOD_RANGE_END: u32 = 9_000;

/// An audited query was invoked under a granted capability set.
///
/// Emitted for every invocation of a query whose
/// [`SysinfoQuerySpec::audit`](tairix_abi::SysinfoQuerySpec) flag is set —
/// the cross-principal, kernel, and hardware queries.
/// Self-scoped observers are deliberately not recorded, to avoid drowning
/// the audit log. Recorded at `Debug`: a monitor polling privileged
/// queries emits this allow record continuously, so at `Info` it floods
/// the default console filter; lowering the filter recovers it for
/// forensics. Denials ([`QUERY_DENIED`]) stay at `Warn` and always
/// surface.
pub const QUERY_SERVED: EventId = EventId(8_001);
/// A query was refused because the caller lacks its required capability.
///
/// Recorded for *any* query, audited or not: a denial is a
/// security-relevant decision in its own right.
pub const QUERY_DENIED: EventId = EventId(8_002);
/// A request was rejected before dispatch: the header failed to decode, or
/// its declared payload was truncated.
pub const REQUEST_MALFORMED: EventId = EventId(8_003);
/// A request named a query identifier that is reserved but unassigned in
/// `sysinfo-v1`.
pub const QUERY_UNAVAILABLE: EventId = EventId(8_004);

#[cfg(test)]
mod tests {
    use super::{
        QUERY_DENIED, QUERY_SERVED, QUERY_UNAVAILABLE, REQUEST_MALFORMED, SYSINFOD_RANGE_END,
        SYSINFOD_RANGE_START,
    };

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in [
            QUERY_SERVED,
            QUERY_DENIED,
            REQUEST_MALFORMED,
            QUERY_UNAVAILABLE,
        ] {
            assert!(id.0 >= SYSINFOD_RANGE_START && id.0 < SYSINFOD_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut ids = [
            QUERY_SERVED.0,
            QUERY_DENIED.0,
            REQUEST_MALFORMED.0,
            QUERY_UNAVAILABLE.0,
        ];
        ids.sort_unstable();
        for w in ids.windows(2) {
            assert_ne!(w[0], w[1], "duplicate sysinfod EventId");
        }
    }
}
