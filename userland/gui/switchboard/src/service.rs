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

use tairix_abi::switchboard_ipc::{SwitchboardCommand, SwitchboardRequest, TraySummary};
use tairix_abi::{CapabilityId, CapabilityQuery, Errno, PowerAction, Signal};
use tairix_geometry::Region;
use tairix_log::EventId;
use tairix_procinfo::Transport;
use tairix_window::Repaint;

use crate::activities::{Activities, Member};
use crate::derive::{derive_summary, Hysteresis};
use crate::model::{build_model, derive_self_uid, GroupingEdit, RollingMeters, SessionReport};
use crate::panel::{Panel, PanelOutcome, PANEL_TITLE};
use crate::publish::Publisher;
use crate::sample::{DegradedField, Sample, Sampler, ScopeVerdicts};
use crate::view::Switchboard;

/// Consecutive faulty publish attempts after which the service gives up
/// rather than retrying forever.
///
/// A session that has simply not drained its queue yet is not counted: see
/// [`Service::cycle`].
pub const MAX_CONSECUTIVE_PUBLISH_FAILURES: u32 = 5;

/// A cheap-to-compare snapshot of every render input besides the
/// composition value itself: the window's client bounds, the active
/// theme's identity, and the render scale.
///
/// [`crate::panel::Panel::flush`] keeps a record of the last one of these it
/// presented alongside its own composition, and skips a present entirely
/// when a fresh snapshot and the held composition both compare equal to
/// what is already on screen — reading a handful of fields is orders of
/// magnitude cheaper than the render-and-composite work a present performs.
///
/// The fields are plain integers rather than `tairix-geometry`'s
/// `Rect`/`Scale` or `tairix-theme`'s `Theme`: this crate's
/// sampler/derive/publish core links neither crate (only the freestanding
/// `Run` binary and the host tests do), so the one comparison that needs
/// them takes a value shape the host builds from its own real types rather
/// than this crate naming them. A theme's stable identity stands in for its
/// full value: a theme registry never mutates a registered theme in place
/// and refuses to register a duplicate id, so two snapshots that agree on
/// `theme_id` always agree on every pixel that theme would draw.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RenderInputs {
    /// The window's client-area left edge.
    pub bounds_left: i32,
    /// The window's client-area top edge.
    pub bounds_top: i32,
    /// The window's client-area width, in pixels.
    pub bounds_width: u32,
    /// The window's client-area height, in pixels.
    pub bounds_height: u32,
    /// The active theme's stable identity.
    pub theme_id: u32,
    /// The active render scale, as its whole-percent value.
    pub scale_percent: u32,
}

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

    /// Paint `panel` into the open window's retained surface and present
    /// what `repaint` and `damage` name.
    ///
    /// A [`Repaint::Reported`] present redraws and copies only the
    /// rectangle the round's controls reported; the surface is retained for
    /// the life of the window, so every pixel outside it is the one already
    /// on screen. An empty report, or [`Repaint::Whole`], covers the window.
    ///
    /// # Errors
    ///
    /// The session's typed refusal, or a surface that could not be built.
    fn present(
        &mut self,
        panel: &mut Switchboard,
        repaint: Repaint,
        damage: &Region,
    ) -> Result<(), Errno>;

    /// The render inputs a present would use right now — the window's
    /// client bounds, the active theme, and the render scale — or `None`
    /// while no window is open: with nothing to present there is nothing
    /// to compare.
    ///
    /// [`Panel::flush`] queries this once per flush, before touching the
    /// composition at all, so a wake that changed nothing a present would
    /// draw never renders or presents.
    fn render_inputs(&self) -> Option<RenderInputs>;

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
    /// the two the service stops cleanly on;
    /// [`Errno::WouldBlock`] (the session's queue is full) is back-pressure
    /// the next sample retries.
    fn publish(&mut self, summary: TraySummary) -> Result<(), Errno>;

    /// Deliver `signal` to the process `pid`.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — notably a target this service holds no
    /// authority over.
    fn signal(&mut self, pid: i64, signal: Signal) -> Result<(), Errno>;

    /// Lower the process `pid`'s time-shared scheduling priority.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — notably a target this service holds no
    /// authority over.
    fn lower_priority(&mut self, pid: i64) -> Result<(), Errno>;

    /// Ask the kernel to move the machine to the power state `action`
    /// names.
    ///
    /// The service checks its own authority before calling this, and the
    /// kernel checks the caller's again on the far side of the trap: this
    /// seam marshals the request, it never stands in for either check.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — a caller without the power capability,
    /// or a platform with no primitive for the requested transition. A
    /// successful power-off never returns at all.
    fn power(&mut self, action: PowerAction) -> Result<(), Errno>;

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
    /// Stop abnormally: the session is there and refused this instance's
    /// identity, so it is orphaned (a session restart left it behind).
    ///
    /// Unlike [`Self::SessionUnbound`] this is not a service running out of
    /// purpose — it has been told it is an impostor, which it cannot be if
    /// the session launched it. Ending quietly would hide a fault behind a
    /// window that simply disappears, so the loop states the reason and
    /// exits non-zero.
    SessionRefused,
    /// Stop: [`MAX_CONSECUTIVE_PUBLISH_FAILURES`] publish attempts in a row
    /// failed for a reason that is neither of the clean ones above nor mere
    /// back-pressure.
    PublishFailed,
}

/// Log event for [`CycleOutcome::SessionRefused`], from this service's own
/// reserved `21000..22000` range.
///
/// A desktop-launched service writes `stderr` to nowhere a user will look,
/// so the one place the reason for stopping can still be found is the log.
pub const SESSION_REFUSED: EventId = EventId(21_000);

/// The Switchboard service: the sampler, the derived tray summary's
/// hysteresis, the publish gate, the rolling meter readings, the
/// activity-grouping state, and the optional overview panel.
pub struct Service {
    self_pid: u64,
    sampler: Sampler,
    hysteresis: Hysteresis,
    publisher: Publisher,
    meters: RollingMeters,
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
        let meters = RollingMeters::new();
        let activities = Activities::new();
        let model = build_model(
            PANEL_TITLE,
            &last_sample,
            &SessionReport::HEALTHY,
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
        let mut summary = derive_summary(&sample, &mut self.hysteresis);
        // The desktop's Restart and Shut Down rows are only offered when a
        // process has said it can actually perform them, so the flag is a
        // live re-read of this service's own capability rather than a value
        // cached at start-up: an authority dropped since then stops being
        // advertised on the very next publish.
        summary.power_capable = authority.holds(CapabilityId::SYSTEM_POWER);
        // Folded in before the rows are built, so each row reads the disk
        // rate and CPU history this sample produced.
        self.meters
            .record(&sample, self.hysteresis, self.panel.session_report());
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
            // A full queue is the session not having drained it yet, not a
            // fault of this instance and not evidence there is no session:
            // the endpoint refuses a post at capacity so the caller can
            // come back, which is what the next sample does — the summary
            // still differs from the last one acknowledged, so the change
            // gate re-offers it. Counting this towards the give-up budget
            // let a desktop that was merely busy for a few seconds kill the
            // monitor watching it, and nothing restarts one.
            Err(Errno::WouldBlock) => CycleOutcome::Continue,
            Err(_) => {
                if self.publisher.record_failure() >= MAX_CONSECUTIVE_PUBLISH_FAILURES {
                    CycleOutcome::PublishFailed
                } else {
                    CycleOutcome::Continue
                }
            }
        }
    }

    /// The relative timeout, in nanoseconds, to park the service until its
    /// next sample is due, adopting the deadline that wait is taken against.
    ///
    /// The deadline [`cycle`](Self::cycle) set was anchored to the clock as it
    /// stood *before* that cycle's work, so a cycle whose own cost reached the
    /// sample period leaves it already spent. Re-anchoring here — against the
    /// reading the loop is actually about to park on — is what stops an
    /// expensive cycle from re-firing immediately and indefinitely.
    #[must_use]
    pub fn wait_timeout_ns(&mut self, now_ns: u64) -> u64 {
        let (deadline, timeout) = crate::schedule::park_until(self.next_sample_ns, now_ns);
        self.next_sample_ns = deadline;
        timeout
    }

    /// Apply one authenticated command from the desktop session.
    ///
    /// A fresh report from the session — which owners are unresponsive, or
    /// what its last frame cost — is rebuilt into the model from the sample
    /// already in hand rather than leaving the *open* panel stale until the
    /// next cycle. While the panel is closed the report is adopted and
    /// nothing is rebuilt for it: with no window there is nothing a rebuild
    /// could reach, and the `OpenPanel` arm below rebuilds before creating
    /// one, so the same page is shown for a fraction of the work.
    pub fn command(
        &mut self,
        host: &mut dyn ServiceHost,
        command: SwitchboardCommand,
        authority: &dyn CapabilityQuery,
    ) {
        match command {
            // Rebuilt *before* the window is created, because that creation
            // reads the model: this is where every report adopted while the
            // panel was closed is folded in, so a panel opening now shows
            // them rather than waiting for the next cycle.
            SwitchboardCommand::OpenPanel { section } => {
                self.rebuild(authority);
                self.panel.open_section(host, section);
            }
            SwitchboardCommand::SeatReport { report } => {
                self.panel.set_seat_report(report);
                self.rebuild_if_shown(authority);
            }
            SwitchboardCommand::FrameReport { report } => {
                self.panel.set_frame_report(report);
                self.rebuild_if_shown(authority);
            }
            SwitchboardCommand::Power { action } => Self::power(host, action, authority),
        }
    }

    /// Move the machine to the power state `action` names, under this
    /// service's own authority.
    ///
    /// The desktop session relays the user's confirmed choice but holds no
    /// power authority itself, so the check belongs here, before anything is
    /// asked of the kernel. A service that does not hold the capability
    /// states the refusal and leaves the machine running rather than
    /// attempting a call it knows will be denied, and the kernel checks
    /// again regardless. Either way the user is told which transition did
    /// not happen.
    ///
    /// A granted power-off or restart never returns; a refusal from the
    /// kernel (an unsupported platform primitive, say) is stated like any
    /// other and the service keeps running.
    fn power(host: &mut dyn ServiceHost, action: PowerAction, authority: &dyn CapabilityQuery) {
        let attempted = power_phrase(action);
        if !authority.holds(CapabilityId::SYSTEM_POWER) {
            host.report_refusal(attempted, Errno::PermissionDenied);
            return;
        }
        if let Err(refusal) = host.power(action) {
            host.report_refusal(attempted, refusal);
        }
    }

    /// Apply a grouping-related outcome the panel reported from a window
    /// action, then rebuild the panel's model so the change is on the
    /// composition the next flush compares against what is on screen.
    ///
    /// The edit is applied and presented once in this same wake, before the
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

    /// Rebuild the model only when a window is open to show it.
    ///
    /// The session's reports arrive whether or not anybody is looking: it
    /// sends what its last frame cost from its own frame path, and that path
    /// runs for every pointer motion across bare wallpaper. A rebuild is not
    /// a cheap thing to do per report — [`build_model`] walks every sampled
    /// process and allocates a row, a name, and a history for each — and with
    /// the panel closed it buys nothing at all: [`Panel::refresh`] renders
    /// nothing without a view, the model it stores is read only on paths that
    /// require an open window, and [`Service::cycle`] rebuilds it from a
    /// fresh sample within [`SAMPLE_PERIOD_NS`](crate::SAMPLE_PERIOD_NS)
    /// regardless.
    ///
    /// So the report itself is always adopted — that is a field write — and
    /// only the rebuild waits. Nothing is lost by waiting: the panel cannot
    /// open without going through [`Service::command`]'s `OpenPanel` arm,
    /// which rebuilds first, so the first frame a user sees already carries
    /// every report that arrived while they were not looking.
    fn rebuild_if_shown(&mut self, authority: &dyn CapabilityQuery) {
        if self.panel.is_open() {
            self.rebuild(authority);
        }
    }

    /// Rebuild the live model from the sample and meter state in hand and
    /// hand it to the panel, which re-renders only if it actually changed.
    fn rebuild(&mut self, authority: &dyn CapabilityQuery) {
        let self_uid = derive_self_uid(&self.last_sample, self.self_pid);
        let model = build_model(
            PANEL_TITLE,
            &self.last_sample,
            self.panel.session_report(),
            &self.meters,
            authority,
            &self.activities,
            self_uid,
        );
        self.panel.refresh(model);
    }
}

/// Name a power transition the way a refusal notice reads it: "could not
/// power the machine off", "could not restart the machine".
///
/// [`PowerAction::name`] is the stable log spelling, which does not fit an
/// English sentence; this is the sentence form and the only place it is
/// written.
const fn power_phrase(action: PowerAction) -> &'static str {
    match action {
        PowerAction::PowerOff => "power the machine off",
        PowerAction::Restart => "restart the machine",
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
