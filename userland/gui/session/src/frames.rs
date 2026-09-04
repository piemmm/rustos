//! Publishing what the desktop's frames cost to the System Information API,
//! so anything on the machine can ask (`plans/FIX-DESKTOP-SPEEDUP.md` A.4).
//!
//! The session already tells its own monitor what the *last* frame cost, on
//! the mailbox that carries the seat report ([`crate::switchboard`]). That is
//! a push to one reader and reaches nothing else: a system monitor, a
//! regression gate, or a shell asking "what is the desktop repainting" has no
//! way in. This module is the pull side — the compositor's cumulative
//! accounting submitted to `sysinfod`, which retains it against this
//! process's kernel-attested identity and serves it under the same
//! capability gate as any other cross-principal query.
//!
//! Publishing is a submission a process makes about itself: it grants
//! nothing, reads nothing, and needs no capability. The rate limit is the
//! whole of the policy — the counters move on every composited frame, and a
//! reader is served from the retained value rather than by waking the
//! desktop, so a send per frame would buy a reader nothing.
//!
//! # It never waits, because the compositor owes a frame
//!
//! The submission is *handed over* and its verdict collected on a later pass
//! (`tairix_rt::submit::Submission`). A blocking `ipc_call` here parked the
//! compositor off the run queue for a full cross-process round trip four
//! times a second — measured at 5–11 ms a time in the aarch64 QEMU vertical,
//! with every application blocked in a window call behind it — which the user
//! saw as a stutter through every drag.

use tairix_abi::sysinfo::{
    encode_request, DesktopFrameTotals, SysinfoQueryId, SYSINFO_MAX_REQUEST,
};
use tairix_abi::Errno;
use tairix_wm::Compositor;

use crate::switchuser::park_within;

/// Minimum time between two publish attempts.
///
/// Deliberately the same order as the runtime's cache-report limit and the
/// switchboard frame report's, because all three answer the same question —
/// how often is it worth restating a figure that moves every frame — but each
/// is a separate policy over a separate channel, documented where its own
/// gate lives. A quarter of a second bounds a continuous redraw storm to four
/// round trips a second while leaving a reader a figure that is never more
/// than that stale.
pub const MIN_FRAME_PUBLISH_INTERVAL_NS: u64 = 250_000_000;

/// Carries this session's encoded submissions to `sysinfod`, without ever
/// waiting for one.
///
/// A seam rather than a direct call so the decisions here — what is worth
/// sending, and when — are host-tested with no kernel. A refusal is the
/// caller's to interpret, never retried in place.
pub trait FrameStatsSink {
    /// Hand `request`, an already-framed `sysinfo-v1` request, to the service
    /// and return at once.
    ///
    /// # Errors
    ///
    /// Whatever the hand-off raised, propagated verbatim — including
    /// [`Errno::WouldBlock`] when a submission is still outstanding.
    fn submit(&mut self, request: &[u8]) -> Result<(), Errno>;

    /// The service's verdict on the submission handed over earlier, or `None`
    /// while it is unanswered and when there is none.
    fn settle(&mut self) -> Option<Result<(), Errno>>;
}

/// The session's publish gate: what it last told the service, when it last
/// tried, and whether a change is being held back.
///
/// One per session, driven from the run loop's frame path beside the
/// switchboard report. The two gates are separate because their rules differ
/// in kind: the monitor must not be told about the frame in which it drew
/// itself, whereas the retained accounting is a truthful count of every frame
/// the desktop composed, the monitor's own included. Nothing here reads the
/// monitor's liveness either — a publish is worth making whether or not any
/// reader exists yet.
///
/// The gate holds no dirty flag: comparing freshly read totals against the
/// ones already accepted *is* the change detection, one struct comparison on
/// a frame path.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameStatsPublisher {
    /// The totals the last *accepted* submission carried, never one merely
    /// attempted: a refused submission is not what the service holds.
    last_sent: Option<DesktopFrameTotals>,
    /// The totals a handed-over submission carries while its verdict is
    /// outstanding. They become [`last_sent`](Self::last_sent) once the
    /// service has accepted them, and are dropped if it refused.
    in_flight: Option<DesktopFrameTotals>,
    /// When the last attempt was made, successful or not. `None` before the
    /// first attempt, so that attempt is never held back.
    last_attempt_ns: Option<u64>,
    /// Whether anything is still owed — a change suppressed by the limit, a
    /// refused hand-off, or a submission whose verdict has yet to be
    /// collected. Drives [`Self::park_deadline_ns`], so each gets one wake.
    pending: bool,
}

impl FrameStatsPublisher {
    /// A session that has published nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_sent: None,
            in_flight: None,
            last_attempt_ns: None,
            pending: false,
        }
    }

    /// Publish `compositor`'s accounting when it differs from what this gate
    /// last had accepted and the minimum interval has elapsed since the last
    /// attempt.
    ///
    /// A desktop composing the same frames repeatedly still moves the frame
    /// counter, so change detection alone cannot quieten a redraw storm and
    /// the interval is what makes this a report rather than a stream. Nothing
    /// here waits on the service: the frame path pays one comparison and at
    /// most one submission.
    ///
    /// A refused submission is dropped rather than retried in place — a
    /// retry loop would turn a busy or absent service into the spin this
    /// design exists to avoid. It leaves the change pending and advances the
    /// limiter exactly as a suppressed send does, so the next attempt rides
    /// the loop's ordinary next pass.
    pub fn maybe_publish(
        &mut self,
        compositor: &Compositor,
        now_ns: u64,
        sink: &mut dyn FrameStatsSink,
    ) {
        // What the service made of the last submission decides what it now
        // holds, so it is collected before fresh totals are compared against
        // that. A refusal drops the figures it carried rather than adopting
        // them, leaving the change to be restated.
        if let Some(outcome) = sink.settle() {
            let carried = self.in_flight.take();
            if outcome.is_ok() {
                self.last_sent = carried;
            }
        }
        let totals = compositor.frame_totals();
        if self.last_sent == Some(totals) {
            // Nothing new to say, but a submission still outstanding is owed
            // a collection, and that is what arms the wake for it.
            self.pending = self.in_flight.is_some();
            return;
        }
        if totals == DesktopFrameTotals::ZERO && self.last_sent.is_none() {
            // A desktop that has composed no frame has nothing to say, and
            // the empty epoch is what the service reads as a withdrawal — so
            // sending it before there is an entry to withdraw would spend a
            // round trip to say nothing. Once something *has* been accepted,
            // an empty epoch is a real withdrawal and is published.
            return;
        }
        if self
            .last_attempt_ns
            .is_some_and(|last| now_ns.saturating_sub(last) < MIN_FRAME_PUBLISH_INTERVAL_NS)
        {
            self.pending = true;
            return;
        }
        self.last_attempt_ns = Some(now_ns);
        let mut request = [0u8; SYSINFO_MAX_REQUEST];
        let Ok(len) = encode_request(
            SysinfoQueryId::DESKTOP_FRAME_REPORT,
            &totals.to_le_bytes(),
            &mut request,
        ) else {
            // The one payload this ever frames is a fixed-size record the
            // request bound is asserted to hold at compile time, so this is
            // unreachable; treating it as a refusal keeps the frame path
            // free of a panic either way.
            self.pending = true;
            return;
        };
        // A handed-over submission is not yet what the service holds, so the
        // change stays owed until its verdict lands — one wake, which collects
        // it. A refused hand-off is dropped rather than retried in place.
        self.in_flight = sink.submit(&request[..len]).is_ok().then_some(totals);
        self.pending = true;
    }

    /// `park_ns` shortened to the moment a held-back change may be published,
    /// or left exactly as it is when nothing is held back.
    ///
    /// This is what keeps the rate limit from costing a reader the desktop's
    /// final figures: a pointer that stops mid-motion leaves one change
    /// suppressed, and without a deadline the retained accounting would sit a
    /// gesture behind until some unrelated wake. With it the session wakes
    /// once, publishes, and the next pass compares equal and folds the park
    /// back to indefinite. An idle desktop therefore arms no timer at all,
    /// and a busy one arms exactly one.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(
            park_ns,
            self.pending.then(|| {
                // `pending` is only ever set alongside a recorded attempt on
                // the suppressed, handed-over and refused paths above. Failing closed to
                // "due now" if it somehow is not costs one extra pass of a
                // loop that is already awake, never a missed publish.
                let elapsed = self
                    .last_attempt_ns
                    .map_or(MIN_FRAME_PUBLISH_INTERVAL_NS, |last| {
                        now_ns.saturating_sub(last)
                    });
                MIN_FRAME_PUBLISH_INTERVAL_NS.saturating_sub(elapsed)
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameStatsPublisher, FrameStatsSink, MIN_FRAME_PUBLISH_INTERVAL_NS};
    use alloc::vec::Vec;
    use tairix_abi::driver::display::{DamageRect, Display, DisplayMode};
    use tairix_abi::sysinfo::{
        DesktopFrameTotals, SysinfoQueryId, SysinfoRequestHeader, SYSINFO_MAX_REQUEST,
    };
    use tairix_abi::{DriverError, Errno};
    use tairix_geometry::Point;
    use tairix_wm::{Compositor, Surface};

    /// A sink that records every submission it takes, refusing the hand-off
    /// or the submission itself however the test arms it. A taken submission
    /// is answered on the very next [`FrameStatsSink::settle`], which is the
    /// ordinary case.
    struct Recorder {
        submissions: Vec<Vec<u8>>,
        /// Refuse the hand-off itself, as an absent service does.
        refuse: bool,
        /// Take the hand-off but have the service refuse what it carried.
        service_refuses: bool,
        /// Hold the verdict back, as a service that has not answered yet.
        withhold: bool,
        /// The verdict a taken submission has yet to give.
        outstanding: Option<Result<(), Errno>>,
    }

    impl Recorder {
        const fn new() -> Self {
            Self {
                submissions: Vec::new(),
                refuse: false,
                service_refuses: false,
                withhold: false,
                outstanding: None,
            }
        }

        /// The totals the `n`th submission carried, decoded exactly as
        /// `sysinfod` decodes them — so a test that passes here is a
        /// submission the service would accept.
        fn totals(&self, n: usize) -> DesktopFrameTotals {
            let request = &self.submissions[n];
            let header = SysinfoRequestHeader::from_bytes(request).expect("a framed request");
            assert_eq!(header.query, SysinfoQueryId::DESKTOP_FRAME_REPORT);
            let payload = &request[SysinfoRequestHeader::WIRE_LEN..];
            assert_eq!(payload.len(), DesktopFrameTotals::WIRE_LEN);
            DesktopFrameTotals::from_bytes(payload).expect("the service accepts the submission")
        }
    }

    impl FrameStatsSink for Recorder {
        fn submit(&mut self, request: &[u8]) -> Result<(), Errno> {
            assert!(request.len() <= SYSINFO_MAX_REQUEST);
            if self.refuse {
                return Err(Errno::NotFound);
            }
            if self.outstanding.is_some() {
                return Err(Errno::WouldBlock);
            }
            self.submissions.push(request.to_vec());
            self.outstanding = Some(if self.service_refuses {
                Err(Errno::LengthOutOfRange)
            } else {
                Ok(())
            });
            Ok(())
        }

        fn settle(&mut self) -> Option<Result<(), Errno>> {
            if self.withhold {
                return None;
            }
            self.outstanding.take()
        }
    }

    /// An opaque red window, the cheapest way to give a frame some work.
    fn window(comp: &mut Compositor, x: i32, y: i32) {
        let mut surface = Surface::new(40, 30).expect("a small surface allocates");
        surface.fill(tairix_wm::Color::rgb(200, 0, 0));
        comp.add_window(Point::new(x, y), surface);
    }

    /// A display that accepts everything, so a test can drive the run loop's
    /// own `present` rather than `composite` — which is the pairing that
    /// decides whether an idle desktop keeps publishing.
    struct AcceptingDisplay {
        mode: DisplayMode,
    }

    impl Display for AcceptingDisplay {
        fn mode_info(&self) -> Result<DisplayMode, DriverError> {
            Ok(self.mode)
        }

        fn present(&mut self, _frame: &[u8]) -> Result<(), DriverError> {
            Ok(())
        }

        fn present_rects(
            &mut self,
            _frame: &[u8],
            _damage: &[DamageRect],
        ) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// The regression the counters exist to make impossible: an idle desktop
    /// publishes once and then goes quiet.
    ///
    /// The run loop presents on *every* wake, damaged or not. While an
    /// undamaged present still counted a frame, the totals never compared
    /// equal, so the gate stayed pending and spent a blocking round trip —
    /// whose service side walks the whole process table — on every wake a
    /// rate-limiting interval apart, for ever. The sibling tests drive the
    /// counters with `composite`, which is an explicit frame and cannot
    /// observe this; only `present` can.
    #[test]
    fn an_idle_desktop_stops_publishing() {
        let mut comp = crate::tests::compositor();
        let mut display = AcceptingDisplay { mode: comp.mode() };
        window(&mut comp, 10, 10);
        assert!(comp.present(&mut display).is_ok());

        let mut sink = Recorder::new();
        let mut gate = FrameStatsPublisher::new();
        gate.maybe_publish(&comp, 0, &mut sink);
        assert_eq!(sink.submissions.len(), 1, "the real frame is published");

        for wake in 1..=8u64 {
            assert!(comp.present(&mut display).is_ok());
            gate.maybe_publish(&comp, wake * 10 * MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        }
        assert_eq!(
            sink.submissions.len(),
            1,
            "wakes that composed nothing must cost no round trip"
        );
        assert_eq!(
            gate.park_deadline_ns(100 * MIN_FRAME_PUBLISH_INTERVAL_NS, u64::MAX),
            u64::MAX,
            "and nothing is left pending to wake the loop for"
        );
    }

    #[test]
    fn the_first_frame_is_published_and_carries_what_it_cost() {
        let mut comp = crate::tests::compositor();
        window(&mut comp, 10, 10);
        comp.composite();
        let mut sink = Recorder::new();
        let mut gate = FrameStatsPublisher::new();

        gate.maybe_publish(&comp, 0, &mut sink);
        assert_eq!(sink.submissions.len(), 1);
        assert_eq!(sink.totals(0), comp.frame_totals());
        assert_eq!(sink.totals(0).frames, 1);
    }

    #[test]
    fn a_desktop_whose_counts_have_not_moved_publishes_nothing() {
        let mut comp = crate::tests::compositor();
        comp.composite();
        let mut sink = Recorder::new();
        let mut gate = FrameStatsPublisher::new();
        gate.maybe_publish(&comp, 0, &mut sink);

        // No further composite: the totals are the ones already accepted.
        gate.maybe_publish(&comp, 10 * MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(sink.submissions.len(), 1);
        assert_eq!(
            gate.park_deadline_ns(0, u64::MAX),
            u64::MAX,
            "nothing held back arms no timer"
        );
    }

    #[test]
    fn a_change_inside_the_interval_is_held_back_and_wakes_the_loop_once() {
        let mut comp = crate::tests::compositor();
        comp.composite();
        let mut sink = Recorder::new();
        let mut gate = FrameStatsPublisher::new();
        gate.maybe_publish(&comp, 1_000, &mut sink);

        window(&mut comp, 20, 20);
        comp.composite();
        gate.maybe_publish(&comp, 1_000 + MIN_FRAME_PUBLISH_INTERVAL_NS / 2, &mut sink);
        assert_eq!(sink.submissions.len(), 1, "suppressed by the limit");
        assert_eq!(
            gate.park_deadline_ns(1_000 + MIN_FRAME_PUBLISH_INTERVAL_NS / 2, u64::MAX),
            MIN_FRAME_PUBLISH_INTERVAL_NS / 2,
            "the park shortens to the moment the held-back change may go"
        );

        // Once the interval has elapsed the held-back change is published,
        // and the following pass compares equal and folds the park back.
        gate.maybe_publish(&comp, 1_000 + MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(sink.submissions.len(), 2);
        assert_eq!(sink.totals(1), comp.frame_totals());
        // The verdict is still owed, so the loop stays armed to collect it;
        // the pass that does finds nothing more to say and folds the park
        // back to indefinite.
        assert!(gate.park_deadline_ns(1_000 + MIN_FRAME_PUBLISH_INTERVAL_NS, u64::MAX) > 0);
        gate.maybe_publish(&comp, 1_000 + 2 * MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(sink.submissions.len(), 2);
        assert_eq!(
            gate.park_deadline_ns(1_000 + 2 * MIN_FRAME_PUBLISH_INTERVAL_NS, u64::MAX),
            u64::MAX
        );
    }

    #[test]
    fn a_submission_the_service_refuses_is_not_adopted_and_is_restated() {
        let mut comp = crate::tests::compositor();
        comp.composite();
        let mut sink = Recorder::new();
        sink.service_refuses = true;
        let mut gate = FrameStatsPublisher::new();

        gate.maybe_publish(&comp, 0, &mut sink);
        assert_eq!(sink.submissions.len(), 1, "the hand-off was taken");

        // The verdict lands on the next pass. The figures it carried are not
        // what the service holds, so they are restated once the limit allows
        // rather than assumed recorded.
        sink.service_refuses = false;
        gate.maybe_publish(&comp, MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(sink.submissions.len(), 2);
        assert_eq!(sink.totals(1), comp.frame_totals());
    }

    #[test]
    fn a_submission_still_outstanding_is_never_replaced_by_a_second() {
        let mut comp = crate::tests::compositor();
        comp.composite();
        let mut sink = Recorder::new();
        let mut gate = FrameStatsPublisher::new();
        gate.maybe_publish(&comp, 0, &mut sink);
        assert_eq!(sink.submissions.len(), 1);

        // A fresh frame, the interval elapsed, but the service has yet to
        // answer: the hand-off is refused and the change stays owed. Nothing
        // waits, and nothing is lost.
        sink.withhold = true;
        window(&mut comp, 20, 20);
        comp.composite();
        gate.maybe_publish(&comp, MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(
            sink.submissions.len(),
            1,
            "the second is refused, not queued behind the first"
        );
        assert!(gate.park_deadline_ns(MIN_FRAME_PUBLISH_INTERVAL_NS, u64::MAX) > 0);

        // Once the service does answer, the withheld change goes out.
        sink.withhold = false;
        gate.maybe_publish(&comp, 2 * MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(sink.submissions.len(), 2);
        assert_eq!(sink.totals(1), comp.frame_totals());
    }

    #[test]
    fn a_refused_submission_is_not_retried_in_place_and_stays_pending() {
        let mut comp = crate::tests::compositor();
        comp.composite();
        let mut sink = Recorder::new();
        sink.refuse = true;
        let mut gate = FrameStatsPublisher::new();

        gate.maybe_publish(&comp, 0, &mut sink);
        assert!(sink.submissions.is_empty());
        assert_eq!(
            gate.park_deadline_ns(0, u64::MAX),
            MIN_FRAME_PUBLISH_INTERVAL_NS,
            "the refusal advances the limiter exactly as a suppressed send does"
        );

        // The refused figures were never accepted, so the next attempt after
        // the interval re-sends them rather than treating them as published.
        sink.refuse = false;
        gate.maybe_publish(&comp, MIN_FRAME_PUBLISH_INTERVAL_NS, &mut sink);
        assert_eq!(sink.submissions.len(), 1);
        assert_eq!(sink.totals(0), comp.frame_totals());
    }

    #[test]
    fn a_session_that_has_composed_nothing_publishes_nothing() {
        // The empty epoch is what the service reads as a withdrawal, so
        // sending it before there is an entry to withdraw spends a round trip
        // to say nothing.
        let mut comp = crate::tests::compositor();
        let mut sink = Recorder::new();
        let mut gate = FrameStatsPublisher::new();
        gate.maybe_publish(&comp, 0, &mut sink);
        assert!(sink.submissions.is_empty());
        assert_eq!(
            gate.park_deadline_ns(0, u64::MAX),
            u64::MAX,
            "and nothing is held back, so no timer is armed"
        );

        // The first composited frame is what makes a publisher.
        comp.composite();
        gate.maybe_publish(&comp, 1, &mut sink);
        assert_eq!(sink.submissions.len(), 1);
        assert_eq!(sink.totals(0).frames, 1);
    }
}
