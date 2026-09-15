//! Best times: the one thing this game keeps between sessions.
//!
//! A closed registry of dotted keys in the OS app-data store, reached through
//! [`tairix_appdata`] — so it is private to this application, gated on the
//! kernel-attested bundle identity, and readable or writable by no other app
//! the user launches. Nothing here performs I/O of its own; the caller supplies
//! the handle, and the write itself is handed to a worker rather than run on
//! the event loop.
//!
//! Only the three preset boards have a best time. A custom board is a size the
//! player invented, so two custom games are not the same game and a time on one
//! says nothing about a time on the other.
//!
//! A stored value the registry refuses — a time outside the bounds, or text
//! that is not a number — leaves that entry empty and is *named* to the caller,
//! which reports it. One corrupt entry therefore costs only itself and can
//! never become a time nobody played.

use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_appconf::ConfError;
use tairix_appdata::Settings;

use crate::board::Difficulty;

/// The longest time the store will keep, in seconds.
///
/// The readout shows three digits, so a longer game has no reading to display
/// and no claim to being a best time. A fixed bound on a value that may arrive
/// from a hand-edited document, not a capacity.
pub const MAX_TIME_SECS: u32 = 999;

/// The store key a preset's best time is kept under.
fn key(difficulty: Difficulty) -> Option<&'static str> {
    match difficulty {
        Difficulty::Beginner => Some("best.beginner"),
        Difficulty::Intermediate => Some("best.intermediate"),
        Difficulty::Expert => Some("best.expert"),
        Difficulty::Custom(_) => None,
    }
}

/// A stored entry that could not be read, so the caller can say which.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Refused {
    /// The key whose value was refused.
    pub key: &'static str,
    /// Why.
    pub reason: Reason,
}

/// Why a stored best time was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Reason {
    /// The value is not a number the store's format admits.
    Malformed,
    /// The value is a number, but outside the bounds a time may take.
    OutOfRange,
}

/// Why writing the best times failed.
///
/// Two distinct refusals rather than one, because they mean different things to
/// a player: a value the document's own format would not hold, and a service
/// that would not commit.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SaveError {
    /// The store's format refused a value.
    Staging(ConfError),
    /// The service refused the commit.
    Commit(Errno),
}

impl core::fmt::Display for SaveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Staging(err) => write!(f, "the settings store refused the time: {err}"),
            Self::Commit(err) => write!(f, "the settings service refused the write: {err:?}"),
        }
    }
}

/// The best time on each preset board.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BestTimes {
    beginner: Option<u32>,
    intermediate: Option<u32>,
    expert: Option<u32>,
}

impl BestTimes {
    /// Read the best times out of `settings`, naming every entry it refused.
    ///
    /// Never fails: an unreadable store simply has no best times in it, which
    /// is the same thing a new user has.
    #[must_use]
    pub fn load(settings: &Settings<'_>) -> (Self, Vec<Refused>) {
        let mut times = Self::default();
        let mut refused = Vec::new();
        for difficulty in Difficulty::PRESETS {
            let Some(key) = key(difficulty) else {
                continue;
            };
            match settings.u32(key) {
                Ok(None) => {}
                Ok(Some(secs)) if secs > 0 && secs <= MAX_TIME_SECS => {
                    times.set(difficulty, Some(secs));
                }
                Ok(Some(_)) => refused.push(Refused {
                    key,
                    reason: Reason::OutOfRange,
                }),
                Err(_) => refused.push(Refused {
                    key,
                    reason: Reason::Malformed,
                }),
            }
        }
        (times, refused)
    }

    /// The best time on `difficulty`, in seconds.
    #[must_use]
    pub const fn best(&self, difficulty: Difficulty) -> Option<u32> {
        match difficulty {
            Difficulty::Beginner => self.beginner,
            Difficulty::Intermediate => self.intermediate,
            Difficulty::Expert => self.expert,
            Difficulty::Custom(_) => None,
        }
    }

    /// Record `secs` on `difficulty`, reporting whether it is a new best.
    ///
    /// A custom board keeps no time, and a time past the bound is not a record
    /// — a game nobody could read the clock on is not one to beat.
    pub fn record(&mut self, difficulty: Difficulty, secs: u32) -> bool {
        if key(difficulty).is_none() || secs == 0 || secs > MAX_TIME_SECS {
            return false;
        }
        if self.best(difficulty).is_some_and(|best| best <= secs) {
            return false;
        }
        self.set(difficulty, Some(secs));
        true
    }

    /// Forget every best time.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether any preset has a time recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.beginner.is_none() && self.intermediate.is_none() && self.expert.is_none()
    }

    /// Write these times into `settings` and commit.
    ///
    /// Only keys whose value differs from what the store already implies are
    /// touched, so the user's own document holds what they actually achieved
    /// and nothing else; a forgotten time is removed rather than written as a
    /// zero the loader would then have to interpret.
    ///
    /// # Errors
    ///
    /// Whichever refusal stopped the write — the caller reports it.
    pub fn save(&self, settings: &mut Settings<'_>) -> Result<(), SaveError> {
        for difficulty in Difficulty::PRESETS {
            let Some(key) = key(difficulty) else {
                continue;
            };
            let stored = settings.u32(key).ok().flatten();
            match self.best(difficulty) {
                Some(secs) if stored != Some(secs) => {
                    settings.set_u32(key, secs).map_err(SaveError::Staging)?;
                }
                None if stored.is_some() => settings.unset(key),
                _ => {}
            }
        }
        if !settings.is_dirty() {
            return Ok(());
        }
        settings.commit().map_err(SaveError::Commit)
    }

    fn set(&mut self, difficulty: Difficulty, secs: Option<u32>) {
        match difficulty {
            Difficulty::Beginner => self.beginner = secs,
            Difficulty::Intermediate => self.intermediate = secs,
            Difficulty::Expert => self.expert = secs,
            Difficulty::Custom(_) => {}
        }
    }
}

#[cfg(test)]
#[path = "scores_tests.rs"]
mod tests;
