//! The clock, date, and host name drawn above the panel.

use alloc::string::{String, ToString};

use tairix_abi::time::Time64;
use tairix_browse::format_datetime;
use tairix_greeter::Chrome;

/// How many characters of `YYYY-MM-DD HH:MM:SS` the clock line keeps.
const CLOCK_CHARS: usize = 5;

/// Build the backdrop's chrome from a wall-clock reading and a host name.
///
/// `now` is `None` when no trusted time is held, and `host` is empty when
/// the machine's name could not be read; either way that line is simply
/// absent. A login screen showing an invented time or an invented machine
/// name would be worse than one showing neither.
#[must_use]
pub fn chrome(now: Option<Time64>, host: &str) -> Chrome {
    let (date, clock) = now.map_or_else(
        || (String::new(), String::new()),
        |now| split_datetime(&format_datetime(now)),
    );
    Chrome {
        clock,
        date,
        host: host.to_string(),
    }
}

/// Split `YYYY-MM-DD HH:MM:SS` into its date and its `HH:MM`.
///
/// An empty reading — which is what the shared formatter answers for a time
/// it will not spell — yields two empty lines rather than a partial one.
fn split_datetime(stamp: &str) -> (String, String) {
    let Some((date, time)) = stamp.split_once(' ') else {
        return (String::new(), String::new());
    };
    let clock = match time.char_indices().nth(CLOCK_CHARS) {
        Some((end, _)) => &time[..end],
        None => time,
    };
    (date.to_string(), clock.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_becomes_a_date_and_a_minute() {
        let noon = Time64::from_secs(1_700_000_000);
        let built = chrome(Some(noon), "tairix");
        assert_eq!(built.date, "2023-11-14");
        assert_eq!(built.clock, "22:13");
        assert_eq!(built.host, "tairix");
    }

    #[test]
    fn no_trusted_time_leaves_the_clock_and_date_empty() {
        let built = chrome(None, "tairix");
        assert!(built.clock.is_empty());
        assert!(built.date.is_empty());
        assert_eq!(built.host, "tairix");
    }

    #[test]
    fn an_unspelled_reading_leaves_both_lines_empty() {
        let built = chrome(Some(Time64::from_secs(0)), "tairix");
        assert!(built.clock.is_empty());
        assert!(built.date.is_empty());
    }

    #[test]
    fn an_unreadable_host_name_leaves_that_line_empty() {
        let built = chrome(Some(Time64::from_secs(1_700_000_000)), "");
        assert!(built.host.is_empty());
        assert_eq!(built.clock, "22:13");
    }
}
