//! Interior-node fault-domain recovery for the xHCI host controller.
//!
//! The xHCI controller is an **interior node** of the discovered hardware tree
//! (`plans/FIX-IO.md` IO4): every USB device it owns hangs beneath it, so a
//! controller-wide event — a latched Host System Error, a `HCHalted`, the
//! `USBCMD.HCRST` reset the driver performs to recover (xHCI §4.24.1) — is a
//! *single fault-domain event over the whole subtree*, not one spurious
//! failure per device below it. Modelling the controller as one
//! [`FaultDomain`] (the arch-neutral primitive the leaf devices and their
//! shared transports already use — one definition, never a divergent second
//! rule) lets the driver ride out a controller blip within a bounded grace
//! window and fail closed coherently only if the controller does not come
//! back, instead of the two failure modes the un-domained path had: a
//! *successful* reset was invisible past a one-shot log line, and a *failed*
//! reset silently left the controller faulted — and since a faulted controller
//! raises no further interrupts (xHCI §4.24.1) the event loop then parked
//! forever with no timer to retry it.
//!
//! [`ControllerHealth`] is the pure, allocation-free coordinator wrapping that
//! [`FaultDomain`]: it is driven by the freestanding serve loop
//! (`src/main.rs`, metal-only — QEMU models no Pi USB) around the existing
//! synchronous controller reset, and every transition it makes is proven
//! host-side here. It classifies each edge through the **shared**
//! [`BlkHealthTransition`] vocabulary (the same the leaf devices, their shared
//! transports, and the mount overlay use, `plans/FIX-IO.md` IO5), so a
//! controller recovery and a disk recovery can never be recorded as different
//! kinds of event — one definition, never a divergent copy.

use tairix_abi::blkio::{fault_domain_wait_timeout, FaultDomain, FaultDomainState};
use tairix_abi::sysinfo::BlkHealthTransition;

/// The controller's recovery **grace window** in nanoseconds: how long a
/// controller that has faulted is held [`FaultDomainState::Recovering`] —
/// retried on a bounded, event-timed schedule — before it is failed closed to
/// [`FaultDomainState::Offline`].
///
/// Sized to ride out a full controller reset (`USBCMD.HCRST`, which the
/// architecture bounds to complete promptly, xHCI §4.24.1) *plus*
/// re-enumeration of the attached topology after it, since a bus glitch that
/// forces a controller reset re-enumerates every device below it. It matches
/// the removable-storage grace window (`BlkDeviceClass::Removable`,
/// 20 s) the controller sits *above*, so one controller blip and the removable
/// storage beneath it are ridden out under one coherent budget rather than the
/// leaves timing out before their bus has finished coming back.
///
/// This is scaling *policy* for this specific controller class — the driver's
/// own device knowledge — documented and derived from the hardware's
/// reset/enumeration timing, never a security or validation bound and never a
/// fixed capacity ceiling.
pub const CONTROLLER_GRACE_NS: u64 = 20_000_000_000;

/// An auditable edge in the controller's interior fault-domain health, the
/// interior-node counterpart of the leaf devices' health events
/// (`plans/FIX-IO.md` IO5).
///
/// [`Recovering`](Self::Recovering) and [`Recovered`](Self::Recovered) are the
/// shared-vocabulary transitions [`BlkHealthTransition::for_fault_domain`]
/// classifies (an interior node has no degraded-but-serving state of its own,
/// so `Degraded` never occurs); [`FailedClosed`](Self::FailedClosed) is the
/// fail-closed edge — the grace window elapsing without the controller
/// returning — which is the fault-domain owner's own distinct event, exactly
/// as the leaf drivers log their grace-window expiry separately from the
/// Recovering/Recovered vocabulary.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ControllerDomainEvent {
    /// The controller faulted and the whole subtree entered its one shared
    /// grace window.
    Recovering,
    /// The controller demonstrably returned (a reset succeeded, or it began
    /// answering again) — from inside the window or from an already-failed
    /// state — and the subtree recovered with no reboot.
    Recovered,
    /// The grace window elapsed with the controller still faulted: it is
    /// failed closed, but stays *recoverable* — a later successful reset
    /// clears it.
    FailedClosed,
}

/// Classify a controller fault-domain edge from `before` to `after` as an
/// auditable event, or [`None`] when the edge carries no signal (an unchanged
/// state).
///
/// Recovering/Recovered come from the shared
/// [`BlkHealthTransition::for_fault_domain`] classifier so they cannot diverge
/// from how the leaf devices and the mount overlay classify the same edge; the
/// into-[`Offline`](FaultDomainState::Offline) edge (which that classifier
/// deliberately leaves to the owner) becomes the distinct
/// [`ControllerDomainEvent::FailedClosed`].
#[must_use]
fn classify(before: FaultDomainState, after: FaultDomainState) -> Option<ControllerDomainEvent> {
    if before == after {
        return None;
    }
    match BlkHealthTransition::for_fault_domain(before, after) {
        Some(BlkHealthTransition::Recovering) => Some(ControllerDomainEvent::Recovering),
        Some(BlkHealthTransition::Recovered) => Some(ControllerDomainEvent::Recovered),
        // An interior fault domain never reports itself degraded-but-serving.
        Some(BlkHealthTransition::Degraded) => None,
        // The only edge the shared classifier leaves to the owner is the
        // fail-closed one (into `Offline`); an unchanged state was already
        // handled above.
        None => matches!(after, FaultDomainState::Offline)
            .then_some(ControllerDomainEvent::FailedClosed),
    }
}

/// The controller's interior fault-domain health, driven by the serve loop
/// around the controller reset.
///
/// It owns exactly one [`FaultDomain`] (the controller node) and exposes the
/// recovery sequencing the loop needs: open the window when a fault is first
/// seen ([`begin_recovery`](Self::begin_recovery)), fold the outcome of each
/// reset attempt ([`note_reset`](Self::note_reset)), fail closed on the
/// event-timed one-shot ([`poll`](Self::poll) at the deadline
/// [`wait_timeout`](Self::wait_timeout) names), and never busy-wait (all
/// waiting is the loop's one-shot timer, never a spin). Every method is
/// pure given the caller's monotonic reading, so the machine is proven
/// host-side.
pub struct ControllerHealth {
    domain: FaultDomain,
}

impl ControllerHealth {
    /// A freshly-`Healthy` controller health owned by `owner` — the
    /// controller's own runtime-discovered identity (its URB endpoint block
    /// base, never a board constant, `AGENTS.md` §2.20) used only to name the
    /// owner in the audit log.
    #[must_use]
    pub const fn new(owner: u32) -> Self {
        Self {
            domain: FaultDomain::new(owner, CONTROLLER_GRACE_NS),
        }
    }

    /// The owner's opaque id (the controller node's discovered identity).
    #[must_use]
    pub const fn owner(&self) -> u32 {
        self.domain.owner()
    }

    /// The current fault-domain state.
    #[must_use]
    pub const fn state(&self) -> FaultDomainState {
        self.domain.state()
    }

    /// Whether the controller has been failed closed (the grace window elapsed
    /// without it returning).
    ///
    /// A failed-closed controller that raises no interrupt and will not reset
    /// is declared dead: the serve loop stops retrying it (fail closed) rather
    /// than re-opening its window forever, so a genuinely dead controller
    /// cannot masquerade as merely recovering. Only a demonstrated return
    /// ([`note_reset(true, …)`](Self::note_reset)) clears it.
    #[must_use]
    pub const fn is_failed_closed(&self) -> bool {
        matches!(self.domain.state(), FaultDomainState::Offline)
    }

    /// Record that a controller fault has been observed at monotonic `now_ns`,
    /// opening (or continuing) the shared grace window, and return the edge to
    /// audit.
    ///
    /// Continuing an already-open window keeps its original start, so a
    /// controller that keeps faulting cannot postpone its fail-closed
    /// indefinitely.
    pub fn begin_recovery(&mut self, now_ns: u64) -> Option<ControllerDomainEvent> {
        let before = self.domain.state();
        let after = self.domain.quiesce(now_ns);
        classify(before, after)
    }

    /// Fold the outcome of a reset attempt at monotonic `now_ns` and return the
    /// edge to audit.
    ///
    /// A successful reset (`ok`) is the controller demonstrably returning: the
    /// subtree recovers to `Healthy` (clearing an already-failed-closed
    /// controller with no reboot). A failed reset advances the grace window on
    /// this reading: while the window is still open the controller stays
    /// `Recovering` (the loop re-arms its one-shot to retry), and once the
    /// window has elapsed it fails closed to `Offline`.
    pub fn note_reset(&mut self, ok: bool, now_ns: u64) -> Option<ControllerDomainEvent> {
        let before = self.domain.state();
        let after = if ok {
            self.domain.resume()
        } else {
            self.domain.poll(now_ns)
        };
        classify(before, after)
    }

    /// Advance the grace window on a pure time tick at monotonic `now_ns`,
    /// returning the edge to audit: a `Recovering` controller whose window has
    /// closed fails closed to `Offline`.
    pub fn poll(&mut self, now_ns: u64) -> Option<ControllerDomainEvent> {
        let before = self.domain.state();
        let after = self.domain.poll(now_ns);
        classify(before, after)
    }

    /// The **relative** one-shot timeout (nanoseconds from `now_ns`) the serve
    /// loop should arm its wait to — exactly the value the wait-set wait takes
    /// — or [`None`] when nothing is recovering (the loop parks unbounded).
    /// Computed by the shared
    /// [`fault_domain_wait_timeout`] rule so the controller's window is timed
    /// identically to every other fault domain's, and it shrinks as `now_ns`
    /// advances toward the fixed grace deadline.
    #[must_use]
    pub fn wait_timeout(&self, now_ns: u64) -> Option<u64> {
        fault_domain_wait_timeout(core::iter::once(&self.domain), now_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grace window's midpoint and just past its end, for driving the
    /// window in tests.
    const MID: u64 = CONTROLLER_GRACE_NS / 2;
    const PAST: u64 = CONTROLLER_GRACE_NS + 1;

    #[test]
    fn a_fresh_controller_is_healthy_and_arms_no_timer() {
        let health = ControllerHealth::new(0x1234);
        assert_eq!(health.state(), FaultDomainState::Healthy);
        assert_eq!(health.owner(), 0x1234);
        assert!(!health.is_failed_closed());
        assert_eq!(health.wait_timeout(0), None);
    }

    #[test]
    fn a_first_fault_enters_recovery_and_arms_a_one_shot_deadline() {
        let mut health = ControllerHealth::new(0);
        assert_eq!(
            health.begin_recovery(1_000),
            Some(ControllerDomainEvent::Recovering)
        );
        assert_eq!(health.state(), FaultDomainState::Recovering);
        // The armed timeout is the whole window relative to the fault instant:
        // a one-shot in the future (event-timed, never a spin).
        assert_eq!(health.wait_timeout(1_000), Some(CONTROLLER_GRACE_NS));
    }

    #[test]
    fn a_continuing_fault_logs_once_and_keeps_the_original_deadline() {
        let mut health = ControllerHealth::new(0);
        assert_eq!(
            health.begin_recovery(1_000),
            Some(ControllerDomainEvent::Recovering)
        );
        // A second observed fault inside the window is not a fresh transition:
        // no duplicate event, and the deadline does not move — so the remaining
        // one-shot timeout has simply shrunk by the elapsed time (a controller
        // that keeps faulting cannot postpone its fail-closed).
        assert_eq!(health.begin_recovery(1_000 + MID), None);
        assert_eq!(
            health.wait_timeout(1_000 + MID),
            Some(CONTROLLER_GRACE_NS - MID)
        );
    }

    #[test]
    fn a_reset_that_succeeds_inside_the_window_recovers_the_controller() {
        let mut health = ControllerHealth::new(0);
        assert_eq!(
            health.begin_recovery(0),
            Some(ControllerDomainEvent::Recovering)
        );
        assert_eq!(
            health.note_reset(true, MID),
            Some(ControllerDomainEvent::Recovered)
        );
        assert_eq!(health.state(), FaultDomainState::Healthy);
        // Recovered: no window left, so the loop parks unbounded again.
        assert_eq!(health.wait_timeout(MID), None);
    }

    #[test]
    fn a_failed_reset_inside_the_window_keeps_recovering_with_no_event() {
        let mut health = ControllerHealth::new(0);
        health.begin_recovery(0);
        assert_eq!(health.note_reset(false, MID), None);
        assert_eq!(health.state(), FaultDomainState::Recovering);
        assert_eq!(health.wait_timeout(MID), Some(CONTROLLER_GRACE_NS - MID));
    }

    #[test]
    fn a_failed_reset_after_the_window_fails_the_controller_closed() {
        let mut health = ControllerHealth::new(0);
        health.begin_recovery(0);
        assert_eq!(
            health.note_reset(false, PAST),
            Some(ControllerDomainEvent::FailedClosed)
        );
        assert!(health.is_failed_closed());
        // Failed closed: no timer is armed, so the loop parks unbounded rather
        // than spinning a retry against a dead controller.
        assert_eq!(health.wait_timeout(PAST), None);
    }

    #[test]
    fn an_idle_poll_fails_a_stale_recovering_controller_closed() {
        let mut health = ControllerHealth::new(0);
        health.begin_recovery(0);
        // No reset attempt landed; the one-shot fires past the deadline.
        assert_eq!(health.poll(PAST), Some(ControllerDomainEvent::FailedClosed));
        assert!(health.is_failed_closed());
    }

    #[test]
    fn a_poll_inside_the_window_is_a_no_op() {
        let mut health = ControllerHealth::new(0);
        health.begin_recovery(0);
        assert_eq!(health.poll(MID), None);
        assert_eq!(health.state(), FaultDomainState::Recovering);
    }

    #[test]
    fn a_failed_closed_controller_recovers_on_a_later_successful_reset() {
        // Sticky-but-recoverable: even after failing closed, a demonstrated
        // return recovers the controller with no reboot.
        let mut health = ControllerHealth::new(0);
        health.begin_recovery(0);
        health.poll(PAST);
        assert!(health.is_failed_closed());
        assert_eq!(
            health.note_reset(true, PAST + 1),
            Some(ControllerDomainEvent::Recovered)
        );
        assert_eq!(health.state(), FaultDomainState::Healthy);
    }

    #[test]
    fn a_spurious_success_on_a_healthy_controller_is_silent() {
        // The loop may fold a "not faulted" observation as a return; on an
        // already-healthy controller that is not a transition and logs nothing.
        let mut health = ControllerHealth::new(0);
        assert_eq!(health.note_reset(true, 10), None);
        assert_eq!(health.state(), FaultDomainState::Healthy);
    }
}
