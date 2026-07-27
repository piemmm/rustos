//! Stable [`tairix_log::EventId`] constants emitted by `fontd`.
//!
//! Per `lib/log` convention every subsystem owns a 1 000-wide reserved range.
//! The font service occupies `17000..18000` (adjacent to the network stack's
//! `16000..17000`). Once shipped the numeric values must never be re-used or
//! re-numbered — external audit-log consumers rely on them.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `fontd` event identifiers.
///
/// Exposed so audit consumers can filter by subsystem in O(1) instead of
/// matching on individual event identifiers.
pub const FONTD_RANGE_START: u32 = 17_000;
/// Range end (exclusive) reserved for `fontd` event identifiers.
pub const FONTD_RANGE_END: u32 = 18_000;

/// The service loaded its faces and bound `FONT_ENDPOINT` successfully — it is
/// serving glyph coverage. Recorded once at startup so an operator can see the
/// font service came up before the desktop.
pub const SERVICE_READY: EventId = EventId(17_001);

/// The service could not come up: a face failed to load or parse, or the
/// reserved endpoint could not be bound (already held, or no registry). A
/// security- and availability-relevant decision — the service fails closed and
/// exits rather than serving forged or absent coverage.
pub const SERVICE_UNAVAILABLE: EventId = EventId(17_002);

#[cfg(test)]
mod tests {
    use super::{FONTD_RANGE_END, FONTD_RANGE_START, SERVICE_READY, SERVICE_UNAVAILABLE};

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in [SERVICE_READY, SERVICE_UNAVAILABLE] {
            assert!(id.0 >= FONTD_RANGE_START && id.0 < FONTD_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(SERVICE_READY.0, SERVICE_UNAVAILABLE.0);
    }
}
