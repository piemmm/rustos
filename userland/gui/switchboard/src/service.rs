//! The service's outside world in one seam ([`ServiceHost`]) and the body
//! of its run loop ([`Service`]).
//!
//! One cycle is: sample the live system, derive the tray summary, record
//! the meter readings, rebuild the overview model, and publish the summary
//! when it is worth publishing. The panel is refreshed on every cycle and
//! the summary is published on every cycle — **whether or not a window is
//! open**. The window is an optional view onto a monitor that never stops
//! monitoring; closing it removes a view, never a duty.
//!
//! The loop *body* lives here rather than in the `Run` binary so it is
//! exercised on the host against the same fake `sysinfo` transport the
//! sampler already uses plus a recording [`ServiceHost`]. What is left in
//! the binary is wiring: learning its own identity, binding its mailboxes,
//! arming the one multiplexed wait, translating window events, and
//! painting.

use tairix_abi::switchboard_ipc::{
    SeatReport, SwitchboardCommand, SwitchboardRequest, TraySummary,
};
use tairix_abi::{CapabilityQuery, Errno, Signal};
use tairix_controls::Switchboard;
use tairix_procinfo::Transport;

use crate::derive::{derive_summary, Hysteresis};
use crate::model::{build_model, LiveMeters};
use crate::panel::{CommandOutcome, Panel, PANEL_TITLE};
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
/// hysteresis, the publish gate, the rolling meter readings, and the
/// optional overview panel.
pub struct Service {
    sampler: Sampler,
    hysteresis: Hysteresis,
    publisher: Publisher,
    meters: LiveMeters,
    last_sample: Sample,
    panel: Panel,
}

impl Service {
    /// A fresh service owned by the process `own_pid`, sampling within
    /// `scopes`, with a closed panel whose model already reflects what
    /// `authority` allows.
    #[must_use]
    pub fn new(own_pid: u64, scopes: ScopeVerdicts, authority: &dyn CapabilityQuery) -> Self {
        let last_sample = Sample::default();
        let meters = LiveMeters::new();
        let model = build_model(
            PANEL_TITLE,
            &last_sample,
            &SeatReport::HEALTHY,
            &meters,
            authority,
        );
        Self {
            panel: Panel::new(own_pid, model),
            sampler: Sampler::new(scopes),
            hysteresis: Hysteresis::new(),
            publisher: Publisher::new(),
            meters,
            last_sample,
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

    /// Run one cycle: sample, derive, record, refresh the panel, and
    /// publish when the gate says so.
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
        let sample = self.sampler.sample(transport, now_ns);
        for field in &sample.degradations {
            host.note_degradation(*field);
        }
        let summary = derive_summary(&sample, &mut self.hysteresis);
        self.meters.record(&sample, self.hysteresis);
        self.last_sample = sample;
        self.rebuild(host, authority);

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
            self.rebuild(host, authority);
        }
    }

    /// Rebuild the live model from the sample and meter state in hand and
    /// hand it to the panel, which re-renders only if it actually changed.
    fn rebuild(&mut self, host: &mut dyn ServiceHost, authority: &dyn CapabilityQuery) {
        let model = build_model(
            PANEL_TITLE,
            &self.last_sample,
            self.panel.seat_report(),
            &self.meters,
            authority,
        );
        self.panel.refresh(host, model);
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
