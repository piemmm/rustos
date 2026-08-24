//! [`RtBlkCall`]: the production blkio transport.
//!
//! The bounded, capability-checked async submit/reap seam on a granted
//! block-service endpoint (`plans/FIX-IO.md` IO1), shared by every
//! [`crate::RemoteBlock`] consumer so the volume-manager probe and the RAID
//! array composer issue the identical wire discipline. Each request is
//! `call_post`ed with the caller's per-request deadline, the reply awaited
//! on a `CallReply` wait-set, and reaped with `call_reap`, so a wedged
//! device fails this transfer closed at its deadline instead of parking the
//! caller forever. The deadline is the caller's, derived from the device's
//! own declared class, so this transport carries no deadline policy of its
//! own. The serving driver fills the shared window during the call, so the
//! window parameter is untouched here.
//!
//! # The caller owns the wait-set
//!
//! A transport parks on a wait-set the **caller** supplies rather than one it
//! mints for itself. The kernel reclaims a wait-set only when its owning
//! process exits, so a transport that minted its own would strand one per
//! instance — invisible in a run-to-completion program, but an unbounded
//! kernel-memory leak in a long-lived one that opens a transport per device or
//! per retry (the RAID composer reconnects a member on every assembly
//! attempt). Handing the set in makes that cost part of the process's own
//! one-time setup, whatever its shape.

use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::Errno;

use crate::client::BlkCall;

/// The production blkio transport: one call endpoint plus the caller's
/// wait-set, on which this endpoint's reply completions are observed.
pub struct RtBlkCall {
    endpoint: u64,
    /// The caller's wait-set, parked on for every reply.
    waitset: u64,
    /// Whether this endpoint's `CallReply` member has been placed in the set.
    /// Registered on first use rather than at construction, so building a
    /// transport cannot fail and the first transfer surfaces a refusal.
    registered: bool,
}

impl RtBlkCall {
    /// A fresh transport for `endpoint`, parking on the caller's `waitset`.
    ///
    /// The set must outlive this transport. It may hold any number of other
    /// members: readiness is level-triggered, so a wake caused by an unrelated
    /// member re-checks and parks again, and no wake is consumed away from the
    /// set's owner.
    #[must_use]
    pub const fn new(endpoint: u64, waitset: u64) -> Self {
        Self {
            endpoint,
            waitset,
            registered: false,
        }
    }

    /// Ensure this endpoint's replies are observable on the caller's set.
    ///
    /// A duplicate `(kind, id)` membership is refused by the kernel and is
    /// treated as success: another transport on the same endpoint has already
    /// made its replies observable, which is exactly the state needed. Every
    /// other refusal fails the transfer closed rather than parking on a set
    /// that will never report this endpoint.
    fn ensure_registered(&mut self) -> Result<u64, Errno> {
        if !self.registered {
            let ctl = tairix_rt::waitset_ctl(
                self.waitset,
                WaitSetOp::Add,
                WaitSourceKind::CallReply,
                self.endpoint,
                0,
            );
            if ctl < 0 && Errno::from_syscall(ctl) != Errno::AlreadyExists {
                return Err(Errno::from_syscall(ctl));
            }
            self.registered = true;
        }
        Ok(self.waitset)
    }
}

impl BlkCall for RtBlkCall {
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        _window: &mut [u8],
        deadline_ns: u64,
    ) -> Result<usize, Errno> {
        let set = self.ensure_registered()?;
        let ticket = tairix_rt::call_post(self.endpoint, request, deadline_ns)
            .map_err(Errno::from_syscall)?;
        loop {
            match tairix_rt::call_reap(self.endpoint, ticket, reply) {
                Ok(len) => return Ok(len),
                Err(neg) => {
                    let err = Errno::from_syscall(neg);
                    // Not ready yet: park on the reply wait-set until the
                    // reply lands or the per-request deadline elapses (which
                    // makes the member ready and the next reap `TimedOut`),
                    // never a busy poll. Every other outcome — a timeout, a
                    // vanished endpoint — fails closed.
                    if err == Errno::WouldBlock {
                        let mut token = 0u64;
                        let _ = tairix_rt::waitset_wait(set, deadline_ns, &mut token);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }
}
