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

/// The front holds its multicast DNS sockets; carries which address families
/// joined their group.
pub const SERVICE_STARTED: EventId = EventId(25_001);

/// A decoder started and was keyed; carries its generation.
pub const DECODER_STARTED: EventId = EventId(25_002);

/// The service cannot serve and is exiting; carries the reason.
pub const SERVICE_UNAVAILABLE: EventId = EventId(25_003);

#[cfg(test)]
mod tests {
    use super::{
        DECODER_STARTED, DISCOVERYD_RANGE_END, DISCOVERYD_RANGE_START, SERVICE_STARTED,
        SERVICE_UNAVAILABLE,
    };
    use tairix_log::EventId;

    #[test]
    fn the_event_ids_are_frozen_and_in_range() {
        assert_eq!(SERVICE_STARTED, EventId(25_001));
        assert_eq!(DECODER_STARTED, EventId(25_002));
        assert_eq!(SERVICE_UNAVAILABLE, EventId(25_003));
        for id in [SERVICE_STARTED, DECODER_STARTED, SERVICE_UNAVAILABLE] {
            assert!((DISCOVERYD_RANGE_START..DISCOVERYD_RANGE_END).contains(&id.0));
        }
    }
}
