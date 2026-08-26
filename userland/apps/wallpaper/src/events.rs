//! Stable log event ids for the wallpaper chooser.
//!
//! The reserved range is `22000..23000`. An id is never reused for a
//! different meaning once it has shipped, because a log reader keys off it.

use tairix_log::EventId;

/// First id reserved for this application.
pub const WALLPAPER_RANGE_START: u32 = 22_000;

/// One past the last id reserved for this application.
pub const WALLPAPER_RANGE_END: u32 = 23_000;

/// One wallpaper was placed: how long its file took to read and how long the
/// sandboxed render took, with the source byte count and the destination.
///
/// A gallery of 8.3-megapixel masters is the desktop's heaviest read-and-
/// decode path, and its two halves have very different causes when it is
/// slow — a store behind an SD card or a cold block cache on one side, the
/// pipe transfer and the decode on the other. Reporting them apart is what
/// lets a measurement name the culprit instead of a guess. Reported at `Info`,
/// since the level is a per-process default with no runtime knob and a record
/// below it would never reach the log at all.
pub const RENDER_TIMED: EventId = EventId(22_001);

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[EventId] = &[RENDER_TIMED];

    #[test]
    fn every_id_is_inside_the_reserved_range() {
        for event in ALL {
            assert!(event.0 >= WALLPAPER_RANGE_START, "{} too low", event.0);
            assert!(event.0 < WALLPAPER_RANGE_END, "{} too high", event.0);
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
