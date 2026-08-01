//! The shared escalating, event-timed retry cadence.
//!
//! Two independent parts of the RAID stack must re-attempt an action that can
//! legitimately keep failing for a while, and must do so without hammering
//! whatever is not ready and without ever spinning: the maintenance scheduler
//! re-probing a faulted member ([`crate::ArrayMaintenance`]), and the member
//! agent re-offering its device to the array registry after the composer went
//! away (`plans/FIX-IO.md` `IO6c`). The arithmetic is the same in both — wait the
//! base delay, double it on each refusal, stop doubling at a ceiling so a
//! device that returns after a long absence still rejoins within a bounded
//! wait — so it is defined once here and driven by both.
//!
//! The cadence holds **no clock**: a caller supplies its monotonic reading and
//! parks on the absolute deadline [`RetryState::due_ns`] returns, exactly as
//! the per-device health machine and the fault domain hand out
//! `blkio::BlkHealth::grace_deadline_ns`. Nothing here polls, sleeps, or spins.

use tairix_abi::blkio::BlkDeviceClass;

/// How far a retry may escalate, as a multiple of its base delay.
///
/// The delay doubles on every refused attempt so a dead device is not
/// re-probed at the cadence of a merely-slow one, and stops doubling here so
/// one that comes back after a long absence still rejoins within a bounded
/// wait rather than an ever-receding one.
const BACKOFF_CEILING_STEPS: u64 = 32;

/// The escalation envelope of a retryable action: its first delay and the
/// ceiling its doubling stops at.
///
/// Derived from the device class in play through [`Self::for_class`] rather
/// than written as a scalar, so the cadence tracks the hardware the retry is
/// aimed at instead of a developer's machine.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RetryCadence {
    base_ns: u64,
    ceiling_ns: u64,
}

impl RetryCadence {
    /// The cadence for a device of `class`.
    ///
    /// The first delay **is** that class's recovery grace window
    /// (`IoBudget::grace_ns`): re-attempting sooner is pointless, because the
    /// device is still inside the window its own driver gives it to come back,
    /// so the attempt would only ask something that has not yet been given up
    /// on. Reading it from the class's own budget keeps this cadence and the
    /// grace window one definition rather than two that could drift apart.
    #[must_use]
    pub const fn for_class(class: BlkDeviceClass) -> Self {
        let grace_ns = class.budget().grace_ns;
        Self {
            base_ns: grace_ns,
            ceiling_ns: grace_ns.saturating_mul(BACKOFF_CEILING_STEPS),
        }
    }

    /// A cadence with an explicit base and ceiling.
    ///
    /// A `ceiling_ns` below `base_ns` is raised to it, so a mis-set envelope
    /// can never make an escalated wait shorter than the first one.
    #[must_use]
    pub const fn new(base_ns: u64, ceiling_ns: u64) -> Self {
        Self {
            base_ns,
            ceiling_ns: if ceiling_ns < base_ns {
                base_ns
            } else {
                ceiling_ns
            },
        }
    }

    /// The delay before the first attempt, and the floor no recovery signal
    /// may pull a later attempt below.
    #[must_use]
    pub const fn base_ns(&self) -> u64 {
        self.base_ns
    }

    /// The ceiling the doubling delay stops at.
    #[must_use]
    pub const fn ceiling_ns(&self) -> u64 {
        self.ceiling_ns
    }
}

/// One retryable action's live position in its [`RetryCadence`].
///
/// The record is the caller's to store — inline for a single action, or one
/// per slot in a caller-owned slice for a wide array — so escalating a retry
/// allocates nothing and imposes no fixed ceiling on how many actions are
/// tracked.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct RetryState {
    /// Monotonic time the next attempt may run.
    due_ns: u64,
    /// The current delay, doubling towards the cadence's ceiling.
    step_ns: u64,
    /// Monotonic time of the last attempt (or of arming), the floor a recovery
    /// signal may not pull an attempt below.
    last_attempt_ns: u64,
    /// Whether an attempt is currently outstanding at all.
    armed: bool,
}

impl RetryState {
    /// A fresh, unarmed record, usable in a `const` array initialiser.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            due_ns: 0,
            step_ns: 0,
            last_attempt_ns: 0,
            armed: false,
        }
    }

    /// Whether an attempt is outstanding.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.armed
    }

    /// The absolute monotonic deadline of the next attempt, or [`None`] when
    /// no attempt is outstanding. This is the value a caller arms its one-shot
    /// wait to.
    #[must_use]
    pub const fn due_ns(&self) -> Option<u64> {
        if self.armed {
            Some(self.due_ns)
        } else {
            None
        }
    }

    /// Whether the outstanding attempt is due at `now_ns`.
    #[must_use]
    pub const fn is_due(&self, now_ns: u64) -> bool {
        self.armed && self.due_ns <= now_ns
    }

    /// Arm a first attempt, due one base delay from `now_ns`.
    ///
    /// Arming an already-armed record leaves it alone, so re-observing the
    /// same unfinished condition cannot restart its escalation.
    pub fn arm(&mut self, cadence: RetryCadence, now_ns: u64) {
        if self.armed {
            return;
        }
        *self = Self {
            due_ns: now_ns.saturating_add(cadence.base_ns()),
            step_ns: cadence.base_ns(),
            last_attempt_ns: now_ns,
            armed: true,
        };
    }

    /// Clear the record: the action succeeded, or is no longer wanted.
    pub fn disarm(&mut self) {
        *self = Self::new();
    }

    /// Record a refused attempt at `now_ns`, doubling the delay towards the
    /// cadence's ceiling.
    ///
    /// A record that was not armed is armed by the refusal, so an action whose
    /// very first attempt fails still escalates from the base delay rather
    /// than retrying immediately.
    pub fn note_failure(&mut self, cadence: RetryCadence, now_ns: u64) {
        let step = self
            .step_ns
            .max(cadence.base_ns())
            .saturating_mul(2)
            .min(cadence.ceiling_ns());
        *self = Self {
            due_ns: now_ns.saturating_add(step),
            step_ns: step,
            last_attempt_ns: now_ns,
            armed: true,
        };
    }

    /// Bring the outstanding attempt forward on a demonstrated recovery
    /// signal, without ever scheduling it sooner than one base delay after the
    /// last attempt.
    ///
    /// That floor is what stops a flapping device — or a signal source that
    /// repeats — from turning the signal into an attempt storm. A record with
    /// no outstanding attempt is left alone.
    pub fn note_signal(&mut self, cadence: RetryCadence, now_ns: u64) {
        if !self.armed {
            return;
        }
        let floor = self.last_attempt_ns.saturating_add(cadence.base_ns());
        self.due_ns = self.due_ns.min(now_ns.max(floor));
    }
}

#[cfg(test)]
mod tests;
