//! The realm's clock, and the defences that make a client's timestamps safe
//! to accept.
//!
//! The realm's tick counter is the only clock. Every cooldown, duration and
//! regeneration is measured in ticks the realm counted, so a client that
//! lies about how much time has passed is describing a window the realm will
//! not honour — which closes the whole speedhack family at the source rather
//! than by detecting it.
//!
//! What a client *may* say is where inside a tick it sampled an input, and
//! that is worth accepting: a fixed step otherwise quantises every action to
//! the step boundary, which is enough to make a dodge feel unreliable. The
//! sample time is therefore an ordering key, validated and clamped by
//! [`place`] before anything reads it, and never a duration.

use tairix_wintersun_net::bounds::MAX_TICK_HZ;
use tairix_wintersun_net::value::{TickInstant, TickPhase};

use crate::bounds::{DIMINISH_RESET_SECONDS, INTENT_LOOKBEHIND_TICKS};
use crate::error::{Refusal, RuleError};

/// The rate a realm's authoritative step runs at.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TickRate(u16);

impl TickRate {
    /// The default rate.
    ///
    /// Thirty rather than twenty because this is an action game with dodges
    /// and aimed shots: a twenty-hertz step quantises every input to fifty
    /// milliseconds, which no amount of client-side polish hides.
    pub const DEFAULT_HZ: u16 = 30;

    /// The default rate, as a constant a caller can hold.
    #[must_use]
    pub const fn default_rate() -> Self {
        Self(Self::DEFAULT_HZ)
    }

    /// A rate of `hz` steps a second.
    ///
    /// # Errors
    ///
    /// [`RuleError::TickRate`] for zero, or for a rate above the protocol's
    /// ceiling.
    pub const fn new(hz: u16) -> Result<Self, RuleError> {
        if hz == 0 || hz > MAX_TICK_HZ {
            return Err(RuleError::TickRate);
        }
        Ok(Self(hz))
    }

    /// Steps a second.
    #[must_use]
    pub const fn hz(self) -> u16 {
        self.0
    }

    /// Ticks in `seconds` seconds, rounded up so a window is never shorter
    /// than the duration it stands for.
    ///
    /// Saturating: a duration that would overflow the tick count is longer
    /// than any rule states, and clamping it keeps the conversion total.
    #[must_use]
    pub fn ticks_for_seconds(self, seconds: u32) -> u32 {
        seconds.saturating_mul(u32::from(self.0))
    }

    /// Ticks of quiet after which a status's diminishing returns reset.
    ///
    /// Derived rather than authored, so the fairness window is the same
    /// wall-clock length whatever rate a realm runs at.
    #[must_use]
    pub fn diminish_reset_ticks(self) -> u32 {
        self.ticks_for_seconds(DIMINISH_RESET_SECONDS)
    }
}

impl Default for TickRate {
    fn default() -> Self {
        Self::default_rate()
    }
}

/// Validate a client's sample time and reduce it to an ordering key inside
/// the current tick.
///
/// The returned instant is what the realm sorts admitted intents by, so an
/// input sampled earlier resolves earlier. Three outcomes, and each is the
/// answer to a way a client can lie:
///
/// * A sample ahead of the realm is refused. Acting in a tick that has not
///   happened is the one claim no generosity covers.
/// * A sample older than [`INTENT_LOOKBEHIND_TICKS`] is clamped to the
///   window's oldest edge, so back-dating further buys no further priority.
/// * Anything inside the window is taken as sent, which is the whole point
///   of accepting it.
///
/// # Errors
///
/// [`Refusal::SampledInTheFuture`] for a sample ahead of `current`.
pub const fn place(current: u64, sampled: TickInstant) -> Result<TickInstant, Refusal> {
    if sampled.tick > current {
        return Err(Refusal::SampledInTheFuture);
    }
    let oldest = current.saturating_sub(INTENT_LOOKBEHIND_TICKS);
    if sampled.tick < oldest {
        return Ok(TickInstant {
            tick: oldest,
            phase: TickPhase(0),
        });
    }
    Ok(sampled)
}

#[cfg(test)]
mod tests;
