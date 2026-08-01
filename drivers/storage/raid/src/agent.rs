//! The member agent's lifecycle: what a member device's driver process should
//! do next, and when.
//!
//! Pure and event-timed. It holds no clock and issues no syscall: a caller
//! supplies its monotonic reading and acts on the returned [`AgentStep`],
//! parking on the absolute deadline the step names. Nothing here spins, sleeps
//! or polls, so the whole lifecycle is proven host-side.

use tairix_abi::raid_ipc::MembershipEnd;
use tairix_raid::{RetryCadence, RetryState};

/// How long the agent waits before its first re-offer.
///
/// This paces re-offering to a *service* rather than re-probing a device, so
/// it is a property of the composer's lifecycle and not of the member's
/// hardware: a composer that is merely late is late by the moment it takes
/// `devmgr` to load it, and one that has crashed will not be back sooner than
/// a fresh load either. A second absorbs the boot race in a single retry
/// without any device class needing to be consulted or guessed at.
pub const REOFFER_BASE_NS: u64 = 1_000_000_000;

/// How far the re-offer delay escalates.
///
/// Doubling from [`REOFFER_BASE_NS`] and stopping here means an agent whose
/// composer never appears costs one wake every half minute rather than one a
/// second, while a composer that does come back is picked up within a bounded
/// wait rather than an ever-receding one. Both ends matter on a machine with
/// many member disks, each holding its own agent.
pub const REOFFER_CEILING_NS: u64 = 32 * REOFFER_BASE_NS;

/// What the agent should do next.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AgentStep {
    /// Delegate this device's block endpoint and data window to the composer's
    /// rendezvous, then post a membership offer naming them.
    ///
    /// Delegation is repeated on every offer rather than done once: a composer
    /// that restarted is a new task holding none of the previous one's grants,
    /// and re-granting a resource a recipient already holds returns the handle
    /// it has rather than minting a second, so repeating costs nothing.
    Offer,
    /// An offer is outstanding. Park on its reply with no deadline — the
    /// membership lasts as long as the array holds the device, and the
    /// composer going away cancels the call and wakes the agent anyway.
    AwaitReply,
    /// Park until this absolute monotonic deadline, then offer again.
    Retry {
        /// The deadline to arm a one-shot wait to.
        deadline_ns: u64,
    },
    /// Stop: the composer read this device and will not compose it. Its
    /// verdict came from the device's own metadata, so re-offering the same
    /// unchanged device would only reach the same answer.
    Stop,
}

/// One member device's agent lifecycle.
///
/// Built once per matched member node and driven turn by turn: ask
/// [`next_step`](Self::next_step) what to do, do it, and report the result
/// back through [`note_offered`](Self::note_offered) or
/// [`note_end`](Self::note_end).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MemberAgent {
    cadence: RetryCadence,
    retry: RetryState,
    outstanding: bool,
    stopped: bool,
}

impl Default for MemberAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl MemberAgent {
    /// A fresh agent, ready to make its first offer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cadence: RetryCadence::new(REOFFER_BASE_NS, REOFFER_CEILING_NS),
            retry: RetryState::new(),
            outstanding: false,
            stopped: false,
        }
    }

    /// What the agent should do at `now_ns`.
    #[must_use]
    pub fn next_step(&self, now_ns: u64) -> AgentStep {
        if self.stopped {
            return AgentStep::Stop;
        }
        if self.outstanding {
            return AgentStep::AwaitReply;
        }
        match self.retry.due_ns() {
            Some(deadline_ns) if deadline_ns > now_ns => AgentStep::Retry { deadline_ns },
            _ => AgentStep::Offer,
        }
    }

    /// Record the result of the [`AgentStep::Offer`] just performed.
    ///
    /// `delivered` is whether the offer reached a composer at all — a
    /// rendezvous nobody has bound yet simply refuses the post. An undelivered
    /// offer escalates the cadence, so an agent whose composer never appears
    /// backs off instead of hammering the empty rendezvous.
    pub fn note_offered(&mut self, delivered: bool, now_ns: u64) {
        if delivered {
            self.outstanding = true;
            self.retry.disarm();
        } else {
            self.retry.note_failure(self.cadence, now_ns);
        }
    }

    /// Record how the outstanding membership ended.
    ///
    /// A refusal stops the agent. Every other ending paces the next offer on
    /// the shared cadence rather than re-offering at once: an agent that
    /// re-offered instantly could not tell a healthy composer from one that is
    /// releasing every member in a loop, and would turn the second into a spin
    /// across every member disk on the machine.
    pub fn note_end(&mut self, end: MembershipEnd, now_ns: u64) {
        self.outstanding = false;
        if end.should_reoffer() {
            self.retry.note_failure(self.cadence, now_ns);
        } else {
            self.stopped = true;
        }
    }
}

#[cfg(test)]
mod tests;
