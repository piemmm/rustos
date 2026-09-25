//! One request at a time on a split virtqueue.

use crate::host::{CompletionSignal, VirtioHost};
use crate::queue::{ChainSegment, SplitQueue, UsedToken};
use crate::transport::{Transport, VirtioError};
use tairix_abi::DriverError;

/// Most advisory completion wakes one request tolerates before failing
/// closed.
///
/// A healthy device posts its completion within a wake or two of the notify: an
/// early wake whose used-ring write is not yet visible, or a wake for a shared
/// line's other queue, costs one re-read. A count far above that turns a storm
/// of wakes with no completion — a stuck or mis-routed shared interrupt — into a
/// deterministic fault well before the request's deadline would.
pub const MAX_COMPLETION_WAKES: u32 = 1024;

/// A split virtqueue that carries one request at a time.
///
/// A request the device leaves unanswered is failed to its caller, but its
/// chain — and every buffer the chain names — stays the device's until the
/// device hands it back. Until [`Self::settle`] sees it come back no later
/// request is published, so a late completion is never taken for a later
/// request's, and no request reuses memory the device may still be using.
pub struct RequestQueue {
    queue: SplitQueue,
    abandoned: Option<Abandoned>,
}

/// A published chain whose completion was never seen.
#[derive(Clone, Copy)]
struct Abandoned {
    /// When the device was last notified of it, on the host's clock.
    notified_ns: u64,
    /// The budget its request was given, and so how often the device is
    /// reminded of it.
    budget_ns: u64,
}

impl RequestQueue {
    /// Carry requests on `queue`, which holds no chain yet.
    #[must_use]
    pub fn new(queue: SplitQueue) -> Self {
        Self {
            queue,
            abandoned: None,
        }
    }

    /// Whether a chain an earlier request abandoned is still with the device.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.abandoned.is_some()
    }

    /// Look for the completion of a chain an earlier request abandoned.
    ///
    /// Returns the completion when the chain has just come back, after which
    /// the buffers it named are the driver's again, and `None` when no chain
    /// was out. While the chain is still out the device is notified again, in
    /// case the first notify was the one it missed — at most once per the
    /// abandoned request's budget on `host`'s clock, however often this is
    /// called.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceOffline`] while the chain is still with the
    /// device, or the mapped queue error for a completion the device had no
    /// right to post.
    pub fn settle<T: Transport>(
        &mut self,
        transport: &mut T,
        host: &dyn VirtioHost,
    ) -> Result<Option<UsedToken>, DriverError> {
        let Some(abandoned) = self.abandoned.as_mut() else {
            return Ok(None);
        };
        // The abandoned chain is the only one out, and the queue answers only
        // for chains it holds.
        match self.queue.poll_used() {
            Ok(token) => {
                self.abandoned = None;
                transport.ack_interrupt();
                Ok(Some(token))
            }
            Err(VirtioError::NoCompletion) => {
                let now_ns = host.now_ns();
                if now_ns.saturating_sub(abandoned.notified_ns) >= abandoned.budget_ns {
                    abandoned.notified_ns = now_ns;
                    self.queue.kick(transport);
                }
                Err(DriverError::DeviceOffline)
            }
            Err(err) => Err(err.as_driver_error()),
        }
    }

    /// Publish `segments`, notify the device, and wait for that chain's own
    /// completion, for at most `budget_ns` in all.
    ///
    /// A wake is only advisory — the used-ring write and the interrupt can be
    /// observed in either order, and a shared line can wake the wait for
    /// another queue — so the ring is re-read after every wake, and each wait
    /// is given only what is left of the budget. A wait that times out, or
    /// could not be made at all, ends the request once the ring has been read
    /// again. [`MAX_COMPLETION_WAKES`] ends a storm of wakes sooner. Nothing
    /// is published while the ring holds a completion: with no chain out, it
    /// answers nothing. The device's interrupt is acknowledged whatever the
    /// outcome, so its line is clear before the next request re-arms it.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceOffline`] while an earlier chain is still out, or
    /// once a wait times out, or the budget is spent, with nothing in the
    /// ring; [`DriverError::DeviceFault`] for a completion posted with nothing
    /// out, or a wake storm; the mapped queue error for a chain the queue
    /// cannot take or a completion the device had no right to post. Every
    /// failure after the chain was published leaves it abandoned.
    pub fn submit_and_wait<T: Transport>(
        &mut self,
        transport: &mut T,
        host: &dyn VirtioHost,
        segments: &[ChainSegment],
        budget_ns: u64,
    ) -> Result<UsedToken, DriverError> {
        if self.abandoned.is_some() {
            return Err(DriverError::DeviceOffline);
        }
        if self.queue.poll_used() != Err(VirtioError::NoCompletion) {
            return Err(DriverError::DeviceFault);
        }
        let start_ns = host.now_ns();
        self.queue
            .add_chain(segments)
            .map_err(VirtioError::as_driver_error)?;
        self.queue.kick(transport);
        let outcome = self.await_completion(host, budget_ns, start_ns.saturating_add(budget_ns));
        if outcome.is_err() {
            self.abandoned = Some(Abandoned {
                notified_ns: start_ns,
                budget_ns,
            });
        }
        transport.ack_interrupt();
        outcome
    }

    fn await_completion(
        &mut self,
        host: &dyn VirtioHost,
        budget_ns: u64,
        deadline_ns: u64,
    ) -> Result<UsedToken, DriverError> {
        let mut left = budget_ns;
        let mut last_wait = None;
        let mut wakes = 0;
        loop {
            match self.queue.poll_used() {
                Ok(token) => return Ok(token),
                Err(VirtioError::NoCompletion) => {}
                Err(err) => return Err(err.as_driver_error()),
            }
            match last_wait {
                // Read only after a wake that brought nothing, so a request
                // answered on its first wake costs one clock reading.
                Some(CompletionSignal::Fired) => left = deadline_ns.saturating_sub(host.now_ns()),
                // The ring has now been read once more — a completion whose
                // interrupt was lost is already there — so a wait that timed
                // out, or could not be made, ends the request.
                Some(CompletionSignal::TimedOut) => left = 0,
                None => {}
            }
            if left == 0 {
                return Err(DriverError::DeviceOffline);
            }
            if wakes == MAX_COMPLETION_WAKES {
                return Err(DriverError::DeviceFault);
            }
            wakes += 1;
            last_wait = Some(host.notify_wait(self.queue.index(), left));
        }
    }

    /// Never return the rings to their pool: the device may still be using
    /// them.
    pub fn withhold(&mut self) {
        self.queue.withhold();
    }
}
