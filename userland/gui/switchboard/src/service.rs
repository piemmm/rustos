//! The service's outside world in one seam ([`ServiceHost`]) and the body
//! of its run loop ([`Service`]).
//!
//! One cycle is: sample the live system, derive the tray summary, record
//! the meter readings, prune and refresh the activity-grouping state,
//! rebuild the overview model, and publish the summary when it is worth
//! publishing. The panel is refreshed on every cycle and the summary is
//! published on every cycle — **whether or not a window is open**. The
//! window is an optional view onto a monitor that never stops monitoring;
//! closing it removes a view, never a duty.
//!
//! The loop *body* lives here rather than in the `Run` binary so it is
//! exercised on the host against the same fake `sysinfo` transport the
//! sampler already uses plus a recording [`ServiceHost`]. What is left in
//! the binary is wiring: learning its own identity, binding its mailboxes,
//! arming the one multiplexed wait, translating window events, and
//! painting.

use alloc::collections::BTreeSet;
use alloc::string::String;

use tairix_abi::switchboard_ipc::{
    SeatReport, SwitchboardCommand, SwitchboardRequest, TraySummary,
};
use tairix_abi::{CapabilityQuery, Errno, Signal};
use tairix_controls::Switchboard;
use tairix_procinfo::Transport;

use crate::activities::{Activities, Member};
use crate::derive::{derive_summary, Hysteresis};
use crate::model::{build_model, derive_self_uid, GroupingEdit, LiveMeters};
use crate::panel::{CommandOutcome, Panel, PanelOutcome, PANEL_TITLE};
use crate::publish::Publisher;
use crate::sample::{DegradedField, Sample, Sampler, ScopeVerdicts};

/// Consecutive non-clean publish failures after which the service gives up
/// rather than retrying forever.
pub const MAX_CONSECUTIVE_PUBLISH_FAILURES: u32 = 5;

/// Everything outside this process the service reaches for, in one seam.
///
/// The production implementation lives in the service's `Run` binary; the
/// host tests drive a recording fake. Every method reports its own typed
/// refusal rather than ending the program: a refused optional action is an
/// answer, not a fatal error, and only [`Service::cycle`] decides that a
/// refusal is terminal.
pub trait ServiceHost {
    /// Create the overview window and arm its event mailbox in the
    /// service's single multiplexed wait, so a window event wakes the very
    /// park the sample deadline and the command mailbox already share.
    ///
    /// # Errors
    ///
    /// The session's or the kernel's typed refusal. The panel stays closed
    /// and the service keeps sampling headlessly.
    fn open_window(&mut self) -> Result<(), Errno>;

    /// Destroy the open window and disarm its event mailbox from that wait,
    /// so a closed window's channel is never left armed.
    ///
    /// # Errors
    ///
    /// The session's or the kernel's typed refusal.
    fn close_window(&mut self) -> Result<(), Errno>;

    /// Paint `panel` into the open window's surface and present it.
    ///
    /// # Errors
    ///
    /// The session's typed refusal, or a surface that could not be built.
    fn present(&mut self, panel: &mut Switchboard) -> Result<(), Errno>;

    /// Send one owner-directed request to the desktop session's Switchboard
    /// endpoint.
    ///
    /// # Errors
    ///
    /// The session's typed refusal (it validates the named owner against
    /// its own live window registry) or a transport failure.
    fn request(&mut self, request: SwitchboardRequest) -> Result<(), Errno>;

    /// Publish `summary` to the desktop session's Switchboard endpoint.
    ///
    /// # Errors
    ///
    /// The session's typed refusal or a transport failure.
    /// [`Errno::NotFound`] (nothing bound the endpoint) and
    /// [`Errno::PermissionDenied`] (the session refused this instance) are
    /// the two the service stops cleanly on.
    fn publish(&mut self, summary: TraySummary) -> Result<(), Errno>;

    /// Deliver `signal` to the process `pid`.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — notably a target this service holds no
    /// authority over.
    fn signal(&mut self, pid: i32, signal: Signal) -> Result<(), Errno>;

    /// Lower the process `pid`'s time-shared scheduling priority.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — notably a target this service holds no
    /// authority over.
    fn lower_priority(&mut self, pid: i32) -> Result<(), Errno>;

    /// State, in plain words, that `action` was refused with `refusal`.
    ///
    /// The service keeps running: the user is told which action did not
    /// happen and why, and the panel is left showing what is still true.
    fn report_refusal(&mut self, action: &str, refusal: Errno);

    /// State, once, that `field` has degraded to its honest empty value.
    fn note_degradation(&mut self, field: DegradedField);
}

/// What the run loop must do after one [`Service::cycle`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CycleOutcome {
    /// Keep running: park until the next thing that must happen.
    Continue,
    /// Stop cleanly: nothing is bound to the session's Switchboard
    /// endpoint, so there is no session to report to.
    SessionUnbound,
    /// Stop cleanly: the session refused this instance's identity, so it is
    /// orphaned (a session restart left it behind).
    SessionRefused,
    /// Stop: [`MAX_CONSECUTIVE_PUBLISH_FAILURES`] publish attempts in a row
    /// failed for a reason that is neither of the clean ones above.
    PublishFailed,
}

/// The Switchboard service: the sampler, the derived tray summary's
/// hysteresis, the publish gate, the rolling meter readings, the
/// activity-grouping state, and the optional overview panel.
pub struct Service {
    self_pid: u64,
    sampler: Sampler,
    hysteresis: Hysteresis,
    publisher: Publisher,
    meters: LiveMeters,
    last_sample: Sample,
    activities: Activities,
    panel: Panel,
    next_sample_ns: u64,
}

impl Service {
    /// A fresh service owned by the process `self_pid`, sampling within
    /// `scopes`, with no activities grouped and a closed panel whose model
    /// already reflects what `authority` allows.
    #[must_use]
    pub fn new(self_pid: u64, scopes: ScopeVerdicts, authority: &dyn CapabilityQuery) -> Self {
        let last_sample = Sample::default();
        let meters = LiveMeters::new();
        let activities = Activities::new();
        let model = build_model(
            PANEL_TITLE,
            &last_sample,
            &SeatReport::HEALTHY,
            &meters,
            authority,
            &activities,
            derive_self_uid(&last_sample, self_pid),
        );
        Self {
            self_pid,
            panel: Panel::new(self_pid, model),
            sampler: Sampler::new(scopes),
            hysteresis: Hysteresis::new(),
            publisher: Publisher::new(),
            meters,
            last_sample,
            activities,
            next_sample_ns: 0,
        }
    }

    /// The overview panel, for the run loop to route window input into.
    pub fn panel_mut(&mut self) -> &mut Panel {
        &mut self.panel
    }

    /// The overview panel.
    #[must_use]
    pub const fn panel(&self) -> &Panel {
        &self.panel
    }

    /// Run one cycle: sample, derive, record, prune and refresh the
    /// activity-grouping state, rebuild the panel, and publish when the
    /// gate says so.
    ///
    /// A cycle before the next sample is due (less than
    /// [`SAMPLE_PERIOD_NS`](crate::SAMPLE_PERIOD_NS) since the last one) is
    /// a no-op that returns immediately, so an input or command wake never
    /// re-queries the system.
    ///
    /// Every step happens whether or not a window is open — the panel
    /// refresh simply draws nothing while closed — so a closed window never
    /// stops the tray summary from being published.
    pub fn cycle(
        &mut self,
        host: &mut dyn ServiceHost,
        transport: &dyn Transport,
        now_ns: u64,
        authority: &dyn CapabilityQuery,
    ) -> CycleOutcome {
        if now_ns < self.next_sample_ns {
            return CycleOutcome::Continue;
        }

        let sample = self.sampler.sample(transport, now_ns);
        for field in &sample.degradations {
            host.note_degradation(*field);
        }
        let summary = derive_summary(&sample, &mut self.hysteresis);
        self.meters.record(&sample, self.hysteresis);
        // A process list that degraded to its honest empty form this cycle
        // is a query failure, not "every process exited" — pruning against
        // it would wipe every activity on a transient `sysinfo` hiccup, so
        // the grouping state is only ever pruned against a sample whose
        // process list actually succeeded.
        if !sample.degradations.contains(&DegradedField::ProcessList) {
            let live: BTreeSet<_> = sample
                .processes
                .iter()
                .map(|process| process.proc_id)
                .collect();
            self.activities.retain_live(&live);
            self.activities.refresh_names(&sample.processes);
        }
        self.last_sample = sample;
        self.rebuild(authority);

        self.next_sample_ns = crate::schedule::advance_deadline(self.next_sample_ns, now_ns);

        let Some(offered) = self.publisher.offer(summary, now_ns) else {
            return CycleOutcome::Continue;
        };
        match host.publish(offered) {
            Ok(()) => {
                self.publisher.record_ack(offered);
                CycleOutcome::Continue
            }
            Err(Errno::NotFound) => CycleOutcome::SessionUnbound,
            Err(Errno::PermissionDenied) => CycleOutcome::SessionRefused,
            Err(_) => {
                if self.publisher.record_failure() >= MAX_CONSECUTIVE_PUBLISH_FAILURES {
                    CycleOutcome::PublishFailed
                } else {
                    CycleOutcome::Continue
                }
            }
        }
    }

    /// The relative timeout, in nanoseconds, to park the service until
    /// its next sample is due.
    #[must_use]
    pub fn wait_timeout_ns(&self, now_ns: u64) -> u64 {
        crate::schedule::wait_timeout_ns(self.next_sample_ns, now_ns)
    }

    /// Apply one authenticated command from the desktop session.
    ///
    /// A fresh seat report changes which owners are unresponsive, so the
    /// model is rebuilt from the sample already in hand rather than leaving
    /// the panel stale until the next cycle.
    pub fn command(
        &mut self,
        host: &mut dyn ServiceHost,
        command: SwitchboardCommand,
        authority: &dyn CapabilityQuery,
    ) {
        if self.panel.command(host, command) == CommandOutcome::Rebuild {
            self.rebuild(authority);
        }
    }

    /// Apply a grouping-related outcome the panel reported from a window
    /// action, then mark the panel for re-presentation.
    ///
    /// The edit is marked and presented once in this same wake, before the
    /// service parks again — so the popup or rename the user just committed
    /// is visible now, not at the next sample.
    ///
    /// Every edit resolves the index it carries through the *current*
    /// [`crate::model::PanelModel`] to a stable activity id before touching
    /// [`Activities`], so a stale index from an action queued before a
    /// refresh can never edit the wrong group; an id that has since vanished
    /// (its activity closed or dissolved meanwhile) is a silent no-op, not a
    /// guess. A validation refusal (an invalid rename) is stated and the
    /// grouping state is left unchanged.
    pub fn apply_grouping(
        &mut self,
        host: &mut dyn ServiceHost,
        outcome: PanelOutcome,
        authority: &dyn CapabilityQuery,
    ) {
        match outcome {
            PanelOutcome::Edit(GroupingEdit::Assign { task, activity }) => {
                let Some((proc_id, pid, name)) = self.panel.model().task_ident(task) else {
                    return;
                };
                let member = Member {
                    proc_id,
                    pid,
                    name: String::from(name),
                };
                match activity {
                    Some(activity_index) => {
                        let Some(activity_id) = self.panel.model().activity_id(activity_index)
                        else {
                            return;
                        };
                        let Some(current_index) = self.group_index_of_id(activity_id) else {
                            return;
                        };
                        if let Err(refusal) = self.activities.assign(current_index, member) {
                            host.report_refusal("group that task", refusal);
                        }
                    }
                    None => {
                        if let Err(refusal) = self.activities.create(member) {
                            host.report_refusal("group that task", refusal);
                        }
                    }
                }
            }
            PanelOutcome::Edit(GroupingEdit::Unassign { task }) => {
                let Some((proc_id, _, _)) = self.panel.model().task_ident(task) else {
                    return;
                };
                let _ = self.activities.unassign(proc_id);
            }
            PanelOutcome::Edit(GroupingEdit::SetPaused { activity, paused }) => {
                let Some(activity_id) = self.panel.model().activity_id(activity) else {
                    return;
                };
                if let Some(current_index) = self.group_index_of_id(activity_id) {
                    let _ = self.activities.set_paused(current_index, paused);
                }
            }
            PanelOutcome::Edit(GroupingEdit::Close { activity }) => {
                let Some(activity_id) = self.panel.model().activity_id(activity) else {
                    return;
                };
                if let Some(current_index) = self.group_index_of_id(activity_id) {
                    let _ = self.activities.close(current_index);
                }
            }
            PanelOutcome::Edit(GroupingEdit::Rename { activity }) => {
                // The widget never reports `ActivityRenamed` without a
                // committed name in hand, so `Renamed` is the only shape a
                // rename outcome takes; this arm exists only so the match
                // stays exhaustive against `GroupingEdit`'s other variants.
                let _ = activity;
            }
            PanelOutcome::Renamed { activity, name } => {
                let Some(activity_id) = self.panel.model().activity_id(activity) else {
                    return;
                };
                if let Some(current_index) = self.group_index_of_id(activity_id) {
                    if let Err(refusal) = self.activities.rename(current_index, &name) {
                        host.report_refusal("rename that activity", refusal);
                    }
                }
            }
        }

        self.rebuild(authority);
    }

    /// The current index of the activity `id` still names, or `None` when
    /// it has since closed or dissolved (fail closed — never guess at a
    /// position).
    fn group_index_of_id(&self, id: u64) -> Option<usize> {
        self.activities.iter().position(|group| group.id == id)
    }

    /// Rebuild the live model from the sample and meter state in hand and
    /// hand it to the panel, which re-renders only if it actually changed.
    fn rebuild(&mut self, authority: &dyn CapabilityQuery) {
        let self_uid = derive_self_uid(&self.last_sample, self.self_pid);
        let model = build_model(
            PANEL_TITLE,
            &self.last_sample,
            self.panel.seat_report(),
            &self.meters,
            authority,
            &self.activities,
            self_uid,
        );
        self.panel.refresh(model);
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
