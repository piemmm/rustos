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

/// Message of [`SERVICE_READY`].
///
/// A consumer that must not act before the endpoint is answerable keys on
/// this line: the SVG-text QEMU vertical holds its scripted shell command
/// until it appears, so the fixture cannot race the bind. One definition, so
/// the line the service writes and the line a waiter matches cannot drift.
pub const SERVICE_READY_MESSAGE: &str = "fontd: serving FONT_ENDPOINT";

/// The service could not come up at all: the store could not be listed, or
/// not a single family in it was usable, or the reserved endpoint could not
/// be bound (already held, or no registry). A security- and
/// availability-relevant decision — the service fails closed and exits
/// rather than serving forged or absent coverage.
pub const SERVICE_UNAVAILABLE: EventId = EventId(17_002);

/// One `/System/Fonts` directory was skipped during discovery: its name is
/// not a valid family key, it carries no readable manifest, or its manifest
/// does not parse. Never fatal on its own — the store may still hold other
/// usable families — but recorded so an operator can see a family did not
/// come up.
pub const FAMILY_SKIPPED: EventId = EventId(17_003);

/// The service manager refused the readiness notice the service sent once it
/// had bound its endpoint.
///
/// The service keeps serving — it is answerable, whatever the manager
/// recorded — but a client the manager parked on this activation will be
/// refused rather than connected, so the refusal is stated rather than
/// swallowed.
pub const READINESS_REFUSED: EventId = EventId(17_004);

#[cfg(test)]
mod tests {
    use super::{
        FAMILY_SKIPPED, FONTD_RANGE_END, FONTD_RANGE_START, READINESS_REFUSED, SERVICE_READY,
        SERVICE_UNAVAILABLE,
    };
    extern crate alloc;
    use alloc::collections::BTreeSet;

    const ALL: [super::EventId; 4] = [
        SERVICE_READY,
        SERVICE_UNAVAILABLE,
        FAMILY_SKIPPED,
        READINESS_REFUSED,
    ];

    #[test]
    fn ids_are_inside_reserved_range() {
        for id in ALL {
            assert!(id.0 >= FONTD_RANGE_START && id.0 < FONTD_RANGE_END);
        }
    }

    #[test]
    fn ids_are_unique() {
        let distinct: BTreeSet<u32> = ALL.iter().map(|id| id.0).collect();
        assert_eq!(distinct.len(), ALL.len());
    }
}
