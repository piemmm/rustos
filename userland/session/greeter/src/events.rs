//! Stable audit event ids for the graphical login screen.
//!
//! The reserved range is `19000..20000`. An id is never reused for a
//! different meaning once it has shipped, because a log reader keys off it.

use tairix_log::EventId;

/// First id reserved for this service.
pub const GREETER_RANGE_START: u32 = 19_000;

/// One past the last id reserved for this service.
pub const GREETER_RANGE_END: u32 = 20_000;

/// The seat is held, the mode is known, and the first frame is on screen.
pub const SCREEN_READY: EventId = EventId(19_001);

/// No screen could be brought up, so the greeter exits and the authority
/// falls back to a text login.
pub const SCREEN_UNAVAILABLE: EventId = EventId(19_002);

/// The account directory could not be read, so the chooser stands with the
/// typed-name tile alone.
pub const ACCOUNTS_UNAVAILABLE: EventId = EventId(19_003);

/// The authority answered a secret with a verdict.
pub const VERDICT_RECEIVED: EventId = EventId(19_004);

/// A secret was offered and no verdict came back.
pub const AUTHORITY_UNREACHABLE: EventId = EventId(19_005);

/// The shipped wallpaper could not be read or decoded, so the flat desktop
/// colour is drawn instead.
pub const WALLPAPER_UNAVAILABLE: EventId = EventId(19_006);

/// The pointer artwork would not rasterise, so the screen runs with a
/// working but undrawn pointer.
pub const POINTER_UNAVAILABLE: EventId = EventId(19_007);

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[EventId] = &[
        SCREEN_READY,
        SCREEN_UNAVAILABLE,
        ACCOUNTS_UNAVAILABLE,
        VERDICT_RECEIVED,
        AUTHORITY_UNREACHABLE,
        WALLPAPER_UNAVAILABLE,
        POINTER_UNAVAILABLE,
    ];

    #[test]
    fn every_id_is_inside_the_reserved_range() {
        for event in ALL {
            assert!(event.0 >= GREETER_RANGE_START, "{} too low", event.0);
            assert!(event.0 < GREETER_RANGE_END, "{} too high", event.0);
        }
    }

    #[test]
    fn every_id_is_distinct() {
        for (index, event) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert_ne!(event.0, other.0, "duplicate event id {}", event.0);
            }
        }
    }
}
