//! Handing one piece of slow work off an interactive loop, latest-wins.

/// A one-job-at-a-time hand-off between an interactive loop and a worker:
/// one request waiting, one in flight, one answer landed.
///
/// The loop *submits* and later *collects*; the worker *takes* and
/// *delivers*. Nothing here blocks, locks, or performs I/O — the embedder
/// supplies the exclusion and the parking — so every rule below is a host
/// test.
///
/// Two properties are the reason it exists rather than a queue:
///
/// - **Latest-wins.** A submission made while a job is in flight replaces any
///   earlier waiting one, so an interaction that settles repeatedly costs at
///   most one further job. A queue would make the loop's own responsiveness
///   the thing that generated the backlog.
/// - **At most one in flight.** Two concurrent writes to the same store would
///   race for what it ends up saying, so a job is handed out only once the
///   previous one has been answered — however many workers ask.
///
/// An answer that a newer submission has superseded is dropped rather than
/// delivered: adopting it would show a state the queued job is about to
/// replace. What a submission *displaced* is handed back to the submitter, so a
/// caller waiting on the displaced request can be told it was superseded rather
/// than left waiting for an answer that will never come.
pub struct JobDesk<Req, Ans> {
    /// The request waiting to be taken, replaced by each submission.
    pending: Option<Req>,
    /// Whether a worker has taken a job and not yet answered it.
    in_flight: bool,
    /// The answer, kept until the loop collects it.
    done: Option<Ans>,
    /// Set once the embedder is tearing down, so a parked worker leaves
    /// instead of looking for work.
    stopping: bool,
}

/// What submitting a request did.
#[derive(Debug, Eq, PartialEq)]
pub struct Submitted<Req> {
    /// Whether a worker should be woken: only when the request is takeable
    /// now. With one already in flight the worker looks again as soon as it
    /// has delivered, so a wake would buy nothing.
    pub wake: bool,
    /// The request this one replaced, if it displaced one that had not been
    /// taken. Nobody will ever answer it.
    pub displaced: Option<Req>,
}

impl<Req, Ans> Default for JobDesk<Req, Ans> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Req, Ans> JobDesk<Req, Ans> {
    /// A desk with nothing submitted, nothing in flight, and nothing answered.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending: None,
            in_flight: false,
            done: None,
            stopping: false,
        }
    }

    /// Submit `request`, replacing any submission not yet taken.
    ///
    /// A stopping desk accepts nothing and hands the request straight back as
    /// displaced, so a caller waiting on it is never left waiting.
    pub fn submit(&mut self, request: Req) -> Submitted<Req> {
        if self.stopping {
            return Submitted {
                wake: false,
                displaced: Some(request),
            };
        }
        Submitted {
            wake: !self.in_flight,
            displaced: self.pending.replace(request),
        }
    }

    /// Take the waiting request, or `None` when there is nothing to do.
    pub fn next_job(&mut self) -> Option<Req> {
        if self.stopping || self.in_flight {
            return None;
        }
        let request = self.pending.take()?;
        self.in_flight = true;
        Some(request)
    }

    /// Record `answer` for the job in flight.
    ///
    /// Answers `false` — and keeps nothing — when a newer request is already
    /// waiting, because that job's answer is the one the loop should adopt.
    /// The caller uses it to decide whether a wake is owed at all.
    pub fn deliver(&mut self, answer: Ans) -> bool {
        self.in_flight = false;
        if self.pending.is_some() {
            return false;
        }
        self.done = Some(answer);
        true
    }

    /// Take the landed answer, once.
    pub fn collect(&mut self) -> Option<Ans> {
        self.done.take()
    }

    /// Whether a worker holds a job it has not yet answered.
    #[must_use]
    pub const fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Whether a request is waiting for a worker to take it.
    #[must_use]
    pub const fn has_work(&self) -> bool {
        !self.stopping && self.pending.is_some() && !self.in_flight
    }

    /// Stop handing out work, so a parked worker leaves its loop.
    ///
    /// A job already in flight is still deliverable, so a worker mid-write
    /// finishes rather than abandoning a half-published document.
    pub fn stop(&mut self) {
        self.stopping = true;
        self.pending = None;
    }

    /// Whether the embedder has asked workers to leave.
    #[must_use]
    pub const fn stopping(&self) -> bool {
        self.stopping
    }
}

#[cfg(test)]
#[path = "defer_tests.rs"]
mod tests;
