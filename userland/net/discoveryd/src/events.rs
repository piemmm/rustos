//! Stable [`tairix_log::EventId`] constants `discoveryd` emits.
//!
//! The service owns the `25000..26000` range. A decoder's crash and its
//! failed launches are the sandbox seam's own events (`6000`, `6001`); these
//! record what the front decides.

use tairix_log::EventId;

/// Range start (inclusive) reserved for `discoveryd` event identifiers.
pub const DISCOVERYD_RANGE_START: u32 = 25_000;

/// Range end (exclusive) reserved for `discoveryd` event identifiers.
pub const DISCOVERYD_RANGE_END: u32 = 26_000;

/// The front holds its multicast DNS sockets; carries each address family's
/// outcome — joined, or the step the stack refused and why.
pub const SERVICE_STARTED: EventId = EventId(25_001);

/// A decoder started and was keyed; carries its generation.
pub const DECODER_STARTED: EventId = EventId(25_002);

/// The service cannot serve and is exiting; carries the reason, and each
/// family's outcome when no socket could be opened.
pub const SERVICE_UNAVAILABLE: EventId = EventId(25_003);

/// A client's request was refused for want of authority; carries what the
/// caller lacked, its uid, and the service type it asked for, if any —
/// never whether another principal holds that type.
pub const REQUEST_DENIED: EventId = EventId(25_004);

/// The grant store could not be read or was refused whole, so no
/// application is granted any type; carries the reason.
pub const GRANTS_REFUSED: EventId = EventId(25_005);

#[cfg(test)]
mod tests {
    use super::{
        DECODER_STARTED, DISCOVERYD_RANGE_END, DISCOVERYD_RANGE_START, GRANTS_REFUSED,
        REQUEST_DENIED, SERVICE_STARTED, SERVICE_UNAVAILABLE,
    };
    use tairix_log::EventId;

    #[test]
    fn the_event_ids_are_frozen_and_in_range() {
        assert_eq!(SERVICE_STARTED, EventId(25_001));
        assert_eq!(DECODER_STARTED, EventId(25_002));
        assert_eq!(SERVICE_UNAVAILABLE, EventId(25_003));
        assert_eq!(REQUEST_DENIED, EventId(25_004));
        assert_eq!(GRANTS_REFUSED, EventId(25_005));
        for id in [
            SERVICE_STARTED,
            DECODER_STARTED,
            SERVICE_UNAVAILABLE,
            REQUEST_DENIED,
            GRANTS_REFUSED,
        ] {
            assert!((DISCOVERYD_RANGE_START..DISCOVERYD_RANGE_END).contains(&id.0));
        }
    }
}
