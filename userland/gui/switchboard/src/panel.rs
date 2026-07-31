//! The live overview panel's lifecycle: opening, raising, refreshing, and
//! closing the one window this service ever shows, and applying the
//! [`Effect`] each reported [`SwitchboardAction`] implies.
//!
//! The service is a monitor first and a window host second: it keeps
//! sampling and publishing its tray summary whether or not a window is
//! open, and a window is only ever opened because the session asked for one
//! ([`SwitchboardCommand::OpenPanel`]). At most one window exists at a
//! time — a second request raises the one already open rather than stacking
//! another.
//!
//! Everything that touches the outside world — the window channel, the
//! session's request endpoint, the `signal` syscall, and the diagnostic
//! stream — is reached through the one [`ServiceHost`] seam, so this whole
//! lifecycle is exercised on the host against a recording fake, exactly as
//! the sampler is exercised against a fake `sysinfo` transport.

use alloc::format;
use alloc::string::String;

use tairix_abi::switchboard_ipc::{SeatReport, SwitchboardCommand, SwitchboardRequest};
use tairix_abi::{CapabilityQuery, Errno, Signal};
use tairix_controls::{Section, Switchboard, SwitchboardAction};

use crate::model::{apply_action, map_section, signal_pid, Effect, PanelModel};
use crate::service::ServiceHost;

/// What the caller must do after handing the panel a command.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    /// The panel's inputs are unchanged; nothing further is required.
    Unchanged,
    /// The panel's inputs changed (a fresh seat report). The caller must
    /// rebuild the live model and hand it to [`Panel::refresh`].
    Rebuild,
}

/// The overview panel: the live model, the window when one is open, and the
/// session's latest unresponsive-owner report.
#[derive(Debug)]
pub struct Panel {
    own_pid: u64,
    seat_report: SeatReport,
    model: PanelModel,
    view: Option<Switchboard>,
}

impl Panel {
    /// A closed panel over `model`, owned by the process `own_pid` — the id
    /// the panel names when it asks the session to raise *its own* window.
    #[must_use]
    pub fn new(own_pid: u64, model: PanelModel) -> Self {
        Self {
            own_pid,
            seat_report: SeatReport::HEALTHY,
            model,
            view: None,
        }
    }

    /// Whether a window is currently open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.view.is_some()
    }

    /// The section the open window is showing, or `None` while closed.
    #[must_use]
    pub fn section(&self) -> Option<Section> {
        self.view.as_ref().map(Switchboard::section)
    }

    /// The session's latest unresponsive-owner report, which the caller
    /// folds into the next model it builds.
    #[must_use]
    pub const fn seat_report(&self) -> &SeatReport {
        &self.seat_report
    }

    /// The open composition, for the caller to route one input event into.
    ///
    /// Input routing needs the window's geometry, theme, and font, which
    /// only the hosting program has, so the caller feeds the event and
    /// hands any resulting [`SwitchboardAction`] back to [`Panel::act`].
    pub fn view_mut(&mut self) -> Option<&mut Switchboard> {
        self.view.as_mut()
    }

    /// Apply one authenticated command from the desktop session.
    pub fn command(
        &mut self,
        host: &mut dyn ServiceHost,
        command: SwitchboardCommand,
    ) -> CommandOutcome {
        match command {
            SwitchboardCommand::OpenPanel { section } => {
                self.open(host, map_section(section));
                CommandOutcome::Unchanged
            }
            SwitchboardCommand::SeatReport { report } => {
                self.seat_report = report;
                CommandOutcome::Rebuild
            }
        }
    }

    /// Show the panel on `section`: create the window if none is open, or
    /// ask the session to raise the one that is.
    ///
    /// A raise is the session's to perform — it alone owns the window
    /// stack — so the panel names its own process and lets the session
    /// decide; a refusal is stated and the panel still switches to the
    /// requested section, since the window is on screen either way.
    fn open(&mut self, host: &mut dyn ServiceHost, section: Section) {
        if self.view.is_none() {
            if let Err(refusal) = host.open_window() {
                host.report_refusal("open the overview window", refusal);
                return;
            }
            self.view = Some(Switchboard::new(self.model.model.clone()));
        } else if let Err(refusal) = host.request(SwitchboardRequest::ActivateOwner {
            owner: self.own_pid,
        }) {
            host.report_refusal("raise the overview window", refusal);
        }

        if let Some(view) = self.view.as_mut() {
            let _ = view.select_section(section);
        }
        self.redraw(host);
    }

    /// Adopt a freshly built model, re-rendering only when it actually
    /// changed and only while a window is open.
    ///
    /// The new reading is shown in place, so the parts of the surface the
    /// user set survive a refresh: the section they were reading, every
    /// section's scroll offset, the keyboard focus, the pointer position,
    /// and any move, resize, or scroll drag in flight. Row selection,
    /// hover, and a half-finished press are dropped by the composition,
    /// because a row index names a position rather than a task and the
    /// rows are rebuilt from the new reading.
    pub fn refresh(&mut self, host: &mut dyn ServiceHost, model: PanelModel) {
        if model == self.model {
            return;
        }
        self.model = model;
        let Some(view) = self.view.as_mut() else {
            return;
        };
        view.set_model(self.model.model.clone());
        self.redraw(host);
    }

    /// Apply the effect `action` implies under `authority`.
    pub fn act(
        &mut self,
        host: &mut dyn ServiceHost,
        action: SwitchboardAction,
        authority: &dyn CapabilityQuery,
    ) {
        match apply_action(&self.model, action, authority) {
            Effect::None => {}
            Effect::CloseWindow => self.close(host),
            Effect::ActivateOwner { owner } => {
                Self::attempt(
                    host,
                    "switch to that task's window",
                    SwitchboardRequest::ActivateOwner { owner },
                );
            }
            Effect::RestartOwner { owner } => {
                Self::attempt(
                    host,
                    "restart that task",
                    SwitchboardRequest::RestartOwner { owner },
                );
            }
            Effect::Signal { pid, signal } => Self::force(host, pid, signal),
        }
    }

    /// Send one owner-directed request, stating a refusal rather than
    /// ending the session over it.
    fn attempt(host: &mut dyn ServiceHost, action: &str, request: SwitchboardRequest) {
        if let Err(refusal) = host.request(request) {
            host.report_refusal(action, refusal);
        }
    }

    /// Deliver a control signal to a sampled task id, refusing an id that
    /// does not fit the syscall's signed width rather than truncating it
    /// into a different, arbitrary process.
    fn force(host: &mut dyn ServiceHost, pid: u64, signal: Signal) {
        let Some(target) = signal_pid(pid) else {
            host.report_refusal("force that task to quit", Errno::OutOfRange);
            return;
        };
        if let Err(refusal) = host.signal(target, signal) {
            host.report_refusal("force that task to quit", refusal);
        }
    }

    /// Re-present the open composition, stating a refusal rather than
    /// ending the session over it. A closed panel draws nothing.
    pub fn redraw(&mut self, host: &mut dyn ServiceHost) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if let Err(refusal) = host.present(view) {
            host.report_refusal("redraw the overview window", refusal);
        }
    }

    /// Destroy the window and return to headless sampling. Closing an
    /// already-closed panel does nothing.
    pub fn close(&mut self, host: &mut dyn ServiceHost) {
        if self.view.take().is_none() {
            return;
        }
        if let Err(refusal) = host.close_window() {
            host.report_refusal("close the overview window", refusal);
        }
    }
}

/// The window title the panel opens under, shared by the composition's
/// title bar and the window the session registers for it.
pub const PANEL_TITLE: &str = "Switchboard";

/// The refusal notice for `action` refused with `refusal`, as one line
/// ready for the diagnostic stream.
///
/// It names the program, the action in plain words, and the refusal the
/// kernel or the session actually gave, and carries no capability token or
/// other secret.
#[must_use]
pub fn refusal_notice(action: &str, refusal: Errno) -> String {
    format!("switchboard: could not {action} ({refusal})\n")
}

#[cfg(test)]
#[path = "panel_tests.rs"]
mod tests;
