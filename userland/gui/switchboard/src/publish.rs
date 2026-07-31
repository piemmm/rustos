//! Change-only, keepalive-backed publish gating: decide whether the latest
//! [`TraySummary`] is worth sending to the desktop session this cycle.

use tairix_abi::switchboard_ipc::TraySummary;

/// How long to wait, at most, between publish attempts even when the
/// summary has not changed.
///
/// The keepalive doubles as orphan detection: a Switchboard instance whose
/// desktop session died and was replaced discovers it, at the latest, on
/// its next keepalive attempt — the new session's `SWITCHBOARD_ENDPOINT`
/// bind does not recognise the stale instance as anything special, so
/// staleness is bounded to one keepalive interval either way (an
/// acknowledged publish, or the refusal `run.rs` treats as a clean exit).
pub const KEEPALIVE_NS: u64 = 10_000_000_000;

/// Change-only publish gating with a keepalive fallback.
///
/// Tracks the last **acknowledged** summary (the session replied success to
/// it) and the wall-clock time of the last publish *attempt* (successful or
/// not), so a summary is (re-)sent when it differs from the last
/// acknowledged one, or when the keepalive interval has elapsed since the
/// last attempt — whichever comes first.
#[derive(Clone, Debug, Default)]
pub struct Publisher {
    last_acknowledged: Option<TraySummary>,
    last_attempt_ns: Option<u64>,
    consecutive_failures: u32,
}

impl Publisher {
    /// A fresh publisher with no acknowledged summary and no attempt
    /// history: the first call to [`Self::offer`] always publishes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether `summary` should be published now, recording this as
    /// the latest attempt time when it is.
    ///
    /// Returns `Some(summary)` when the caller should publish it (because
    /// the summary changed since the last acknowledgement, or the
    /// keepalive interval elapsed since the last attempt); `None` when
    /// there is nothing new to say and the keepalive has not yet come due.
    #[must_use]
    pub fn offer(&mut self, summary: TraySummary, now_ns: u64) -> Option<TraySummary> {
        let changed = self.last_acknowledged != Some(summary);
        let keepalive_due = match self.last_attempt_ns {
            Some(last) => now_ns.saturating_sub(last) >= KEEPALIVE_NS,
            None => true,
        };
        if !changed && !keepalive_due {
            return None;
        }
        self.last_attempt_ns = Some(now_ns);
        Some(summary)
    }

    /// Record that the session acknowledged `summary`: it becomes the new
    /// baseline for [`Self::offer`]'s change detection, and the
    /// consecutive-failure counter clears.
    pub fn record_ack(&mut self, summary: TraySummary) {
        self.last_acknowledged = Some(summary);
        self.consecutive_failures = 0;
    }

    /// Record a publish attempt that did not succeed (and was not one of
    /// the clean-exit refusals `run.rs` handles separately), returning the
    /// new consecutive-failure count.
    pub fn record_failure(&mut self) -> u32 {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures
    }

    /// The current consecutive publish-failure count.
    #[must_use]
    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
