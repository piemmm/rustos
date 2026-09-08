//! Wait-set token bookkeeping: the one place the run loop's multiplexed
//! parking sources are named, so the tests can assert *which* sources are
//! armed for a given window state without touching a real wait-set.
//!
//! The run loop parks on exactly one `waitset_wait` call per iteration,
//! given the timeout until the next sample deadline. Three members are
//! permanent for the life of the process — the termination signal, the
//! session's command mailbox, and the machine's memory-pressure band — and a
//! fourth, the open window's own event mailbox, is present only while a
//! window is open, so a closed window's channel is never left idly armed.

use alloc::vec::Vec;

/// Which wait-set member woke a `waitset_wait` call.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitToken {
    /// The process's own termination-request signal.
    Signal,
    /// The session's per-instance command mailbox.
    Command,
    /// The open window's event mailbox (armed only while a window is open).
    WindowEvent,
    /// The machine's memory-pressure band moving.
    ///
    /// Permanent, and armed even with no window open: this process caches
    /// rendered glyphs, and a cache that is never told the band can neither
    /// retain anything nor give anything back.
    MemoryPressure,
    /// The artwork worker reporting that a batch of icon decodes has landed.
    ///
    /// Permanent, like the band: the reader answers the icons a paint asked
    /// for, and its wake is what turns those answers into the frame that
    /// shows them. Its readiness is a level peek, so the loop drains it.
    Artwork,
}

impl WaitToken {
    /// The wait-set token value (`waitset_ctl`'s `token` argument,
    /// `waitset_wait`'s output).
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::Signal => 1,
            Self::Command => 2,
            Self::WindowEvent => 3,
            Self::MemoryPressure => 4,
            Self::Artwork => 5,
        }
    }

    /// Decode a token value back to the member it names.
    ///
    /// Returns [`None`] for any value that is not one of the defined tokens —
    /// unreachable from a genuine `waitset_wait` reply given the members the
    /// run loop ever arms, but the run loop still treats it as a spurious
    /// wake rather than indexing on a guess (fail closed).
    #[must_use]
    pub const fn from_u64(value: u64) -> Option<Self> {
        match value {
            1 => Some(Self::Signal),
            2 => Some(Self::Command),
            3 => Some(Self::WindowEvent),
            4 => Some(Self::MemoryPressure),
            5 => Some(Self::Artwork),
            _ => None,
        }
    }
}

/// The wait-set members the run loop must have armed for the given window
/// state: [`WaitToken::Signal`], [`WaitToken::Command`],
/// [`WaitToken::MemoryPressure`] and [`WaitToken::Artwork`] are permanent;
/// [`WaitToken::WindowEvent`] is present only while `window_open`.
#[must_use]
pub fn required_members(window_open: bool) -> Vec<WaitToken> {
    let mut members = alloc::vec![
        WaitToken::Signal,
        WaitToken::Command,
        WaitToken::MemoryPressure,
        WaitToken::Artwork
    ];
    if window_open {
        members.push(WaitToken::WindowEvent);
    }
    members
}

#[cfg(test)]
mod tests {
    use super::{required_members, WaitToken};

    #[test]
    fn tokens_round_trip() {
        for token in [
            WaitToken::Signal,
            WaitToken::Command,
            WaitToken::WindowEvent,
            WaitToken::MemoryPressure,
            WaitToken::Artwork,
        ] {
            assert_eq!(WaitToken::from_u64(token.as_u64()), Some(token));
        }
    }

    #[test]
    fn an_unknown_token_value_decodes_to_none() {
        assert_eq!(WaitToken::from_u64(0), None);
        assert_eq!(WaitToken::from_u64(6), None);
        assert_eq!(WaitToken::from_u64(u64::MAX), None);
    }

    #[test]
    fn a_closed_window_still_watches_the_pressure_band_and_its_reader() {
        // The glyph and artwork caches outlive the window, so the band must
        // too, and a decode still in flight when the window closes has a wake
        // to land on.
        assert_eq!(
            required_members(false),
            alloc::vec![
                WaitToken::Signal,
                WaitToken::Command,
                WaitToken::MemoryPressure,
                WaitToken::Artwork
            ]
        );
    }

    #[test]
    fn windowed_membership_adds_the_window_event_source() {
        assert_eq!(
            required_members(true),
            alloc::vec![
                WaitToken::Signal,
                WaitToken::Command,
                WaitToken::MemoryPressure,
                WaitToken::Artwork,
                WaitToken::WindowEvent
            ]
        );
    }
}
