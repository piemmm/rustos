//! The live overview panel's lifecycle: opening, raising, refreshing, and
//! closing the one window this service ever shows, and applying the
//! [`Effect`] each reported [`SwitchboardAction`] implies.
//!
//! The service is a monitor first and a window host second: it keeps
//! sampling and publishing its tray summary whether or not a window is
//! open, and a window is only ever opened because the session asked for one
//! ([`Panel::open_section`]). At most one window exists at a time — a second
//! request raises the one already open rather than stacking another.
//!
//! Everything that touches the outside world — the window channel, the
//! session's request endpoint, the `signal` syscall, and the diagnostic
//! stream — is reached through the one [`ServiceHost`] seam, so this whole
//! lifecycle is exercised on the host against a recording fake, exactly as
//! the sampler is exercised against a fake `sysinfo` transport.

use alloc::format;
use alloc::string::String;

use tairix_abi::switchboard_ipc::{CommandSection, FrameReport, SeatReport, SwitchboardRequest};
use tairix_abi::window_ipc::WindowSizing;
use tairix_abi::{CapabilityQuery, Errno, Signal};
use tairix_controls::damage;
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Region, Scale};
use tairix_input::{InputEvent, Key};
use tairix_theme::Theme;
use tairix_window::Repaint;

use crate::model::{apply_action, map_section, signal_pid, Effect, PanelModel, SessionReport};
use crate::service::ServiceHost;
use crate::view::{Section, Switchboard, SwitchboardAction};

/// The overview panel: the live model, the window when one is open, what
/// the session has last reported about itself, and what the next present
/// owes the screen.
#[derive(Debug)]
pub struct Panel {
    own_pid: u64,
    session: SessionReport,
    model: PanelModel,
    view: Option<Switchboard>,
    /// Whether the next present owes the whole client, for a change no
    /// control round and no refresh could have described.
    whole: bool,
    /// The rectangles the rounds since the last present reported.
    damage: Region,
}

impl Panel {
    /// A closed panel over `model`, owned by the process `own_pid` — the id
    /// the panel names when it asks the session to raise *its own* window.
    #[must_use]
    pub fn new(own_pid: u64, model: PanelModel) -> Self {
        Self {
            own_pid,
            session: SessionReport::HEALTHY,
            model,
            view: None,
            whole: true,
            damage: damage::sink(),
        }
    }

    /// Whether a window is currently open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.view.is_some()
    }

    /// The panel's current model, for resolving a grouping outcome's
    /// indices back to stable identities.
    #[must_use]
    pub const fn model(&self) -> &PanelModel {
        &self.model
    }

    /// The section the open window is showing, or `None` while closed.
    #[must_use]
    pub fn section(&self) -> Option<Section> {
        self.view.as_ref().map(Switchboard::section)
    }

    /// What the session has last reported — its unresponsive owners and its
    /// last frame's cost — which the caller folds into the next model it
    /// builds.
    #[must_use]
    pub const fn session_report(&self) -> &SessionReport {
        &self.session
    }

    /// Route one pointer event into the open composition, accumulating the
    /// rectangles its controls repainted, and report the action it produced.
    ///
    /// Input routing needs the window's geometry, theme, and font, which only
    /// the hosting program has, so the caller feeds the event and hands any
    /// resulting [`SwitchboardAction`] back to [`Panel::act`]. It goes through
    /// the panel rather than the composition because the panel is what knows
    /// what is on screen, and so what the next present owes it.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<SwitchboardAction> {
        let view = self.view.as_mut()?;
        view.on_pointer(event, bounds, scale, theme, font, &mut self.damage)
    }

    /// Route one key into the open composition, on the same terms as
    /// [`Panel::on_pointer`].
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<SwitchboardAction> {
        let view = self.view.as_mut()?;
        view.on_key(key, bounds, scale, theme, font, &mut self.damage)
    }

    /// Mark the next present as covering the whole window.
    ///
    /// A change no control round described — a resize onto a fresh surface, a
    /// desktop appearance or density change, a fresh model — moves pixels the
    /// accumulated report says nothing about, so the report is dropped and the
    /// window is drawn whole rather than left partly stale.
    pub fn repaint_whole(&mut self) {
        self.whole = true;
        self.damage.clear();
    }

    /// Show the panel on the wire `section`, opening the window if none is
    /// open.
    pub fn open_section(&mut self, host: &mut dyn ServiceHost, section: CommandSection) {
        self.open(host, map_section(section));
    }

    /// Adopt the seat's latest unresponsive-owner report.
    ///
    /// The report changes which owners the recovery rows call out, so an
    /// *open* panel is rebuilt from the sample already in hand rather than
    /// left stale until the next cycle. Adopting it is all this does; the
    /// caller decides whether anything is on screen to rebuild for
    /// (`Service::rebuild_if_shown`).
    pub fn set_seat_report(&mut self, report: SeatReport) {
        self.session.seat = report;
    }

    /// Adopt what the session's last composited frame cost, on the same
    /// terms as the seat report: the Resources page reads it, so the caller
    /// rebuilds an open panel from the sample in hand — and a closed one not
    /// at all, since this is the one report the session's frame path can
    /// produce several times a second.
    pub fn set_frame_report(&mut self, report: FrameReport) {
        self.session.frame = Some(report);
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
            self.view = Some(Switchboard::new(&self.model.model));
        } else if let Err(refusal) = host.request(SwitchboardRequest::ActivateOwner {
            owner: self.own_pid,
        }) {
            host.report_refusal("raise the overview window", refusal);
        }

        if let Some(view) = self.view.as_mut() {
            let _ = view.select_section(section);
        }
        // Opening a window, raising it, or showing a different section is
        // never a control round, so nothing described what moved.
        self.repaint_whole();
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
    ///
    /// The composition adopts against the frame it will next be drawn in, so
    /// the readings that moved are what the next present carries — a monitor
    /// samples every couple of seconds, and re-presenting the whole client for
    /// a handful of moved digits costs a render, a whole-window encode, and a
    /// whole-frame decode on the session's own serve thread. A window with no
    /// bounds to adopt against holds none of the pixels a partial present
    /// would leave standing, so it is drawn whole instead.
    pub fn refresh(&mut self, host: &dyn ServiceHost, model: PanelModel) {
        if model == self.model {
            return;
        }
        self.model = model;
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(layout) = host.layout() else {
            view.adopt_unshown(&self.model.model);
            self.repaint_whole();
            return;
        };
        view.set_model(
            &self.model.model,
            layout.bounds,
            layout.scale,
            layout.theme,
            layout.font,
            &mut self.damage,
        );
    }

    /// Apply every effect `action` implies under `authority`, in order.
    ///
    /// A refusal on one entry is stated and the rest still run: one refusal
    /// must never abort the others.
    pub fn act(
        &mut self,
        host: &mut dyn ServiceHost,
        action: SwitchboardAction,
        authority: &dyn CapabilityQuery,
    ) {
        for effect in apply_action(&self.model, action, authority) {
            match effect {
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
                Effect::Signal { pid, signal } => {
                    Self::signal_one(host, pid, signal, "force that task to quit");
                }
                Effect::LowerPriority { pid } => Self::lower_priority(host, pid),
            }
        }
    }

    /// Send one owner-directed request, stating a refusal rather than
    /// ending the session over it.
    fn attempt(host: &mut dyn ServiceHost, action: &str, request: SwitchboardRequest) {
        if let Err(refusal) = host.request(request) {
            host.report_refusal(action, refusal);
        }
    }

    /// Deliver `signal` to a sampled task id, refusing an id that does not
    /// fit the syscall's signed width rather than truncating it into a
    /// different, arbitrary process. `action` names the attempted action in
    /// plain words for the refusal notice.
    fn signal_one(host: &mut dyn ServiceHost, pid: u64, signal: Signal, action: &str) {
        let Some(target) = signal_pid(pid) else {
            host.report_refusal(action, Errno::OutOfRange);
            return;
        };
        if let Err(refusal) = host.signal(target, signal) {
            host.report_refusal(action, refusal);
        }
    }

    /// Lower a sampled task id's scheduling priority, refusing an id that
    /// does not fit the syscall's signed width rather than truncating it.
    fn lower_priority(host: &mut dyn ServiceHost, pid: u64) {
        let Some(target) = signal_pid(pid) else {
            host.report_refusal("lower priority", Errno::OutOfRange);
            return;
        };
        if let Err(refusal) = host.lower_priority(target) {
            host.report_refusal("lower priority", refusal);
        }
    }

    /// Present whatever the open composition still owes the screen, stating a
    /// refusal rather than ending the session over it.
    ///
    /// The account is authoritative: every round that moves a pixel — a
    /// pointer or key routed into the controls, a fresh reading adopted into
    /// the section on show — reports the rectangle it repaints, and a wake
    /// that reports nothing presents nothing. A change no report could
    /// describe (a resize onto a fresh surface, a re-theme, a released region,
    /// a window just opened) marks the client whole through
    /// [`repaint_whole`](Self::repaint_whole).
    ///
    /// The account is cleared whether or not the present is accepted: a
    /// refusal is already reported once through
    /// [`ServiceHost::report_refusal`], and re-attempting it on every wake
    /// would storm the refusal path. The next genuine change reports again.
    ///
    /// A round that moved pixels and reported *nothing* leaves them stale,
    /// which is why reporting is each section's stated obligation; where the
    /// two pull against each other a round over-reports, and an over-reported
    /// rectangle costs one redundant repaint.
    pub fn flush(&mut self, host: &mut dyn ServiceHost) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let repaint = if self.whole {
            Repaint::Whole
        } else if self.damage.is_empty() {
            return;
        } else {
            Repaint::Reported
        };
        self.whole = false;
        if let Err(refusal) = host.present(view, repaint, &self.damage) {
            host.report_refusal("redraw the overview window", refusal);
        }
        self.damage.clear();
    }

    /// Destroy the window and return to headless sampling. Closing an
    /// already-closed panel does nothing.
    ///
    /// The account goes with the window: a reopened panel is a fresh surface
    /// and owes every pixel of it, never the rectangles the last one left.
    pub fn close(&mut self, host: &mut dyn ServiceHost) {
        if self.view.take().is_none() {
            return;
        }
        self.repaint_whole();
        if let Err(refusal) = host.close_window() {
            host.report_refusal("close the overview window", refusal);
        }
    }
}

/// The window title the panel opens under, used by the window the session
/// registers and decorates for it.
pub const PANEL_TITLE: &str = "Switchboard";

/// The overview window's initial client width in physical pixels.
///
/// The panel's size envelope lives beside the panel itself — the service
/// binary opens and resizes the window with it, and the QEMU vertical's
/// host-side scan-out assertion measures the panel's region against it —
/// so the drawn window and the pixels a test looks at cannot disagree.
///
/// Wider than it was by the navigation rail, so the window still opens with
/// the room a section's own anatomy had beside it.
pub const WIN_WIDTH: u32 = 760 + crate::view::RAIL_WIDTH;

/// The overview window's initial client height in physical pixels (see
/// [`WIN_WIDTH`]).
pub const WIN_HEIGHT: u32 = 560;

/// The sizing the overview window asks the window manager for: resizable,
/// down to the narrowest client its sections still seat (see [`WIN_WIDTH`]).
///
/// Resizable decoration widens the furniture band reserved around the
/// client.
pub const WIN_SIZING: WindowSizing = WindowSizing::Resizable {
    min_width_px: MIN_WIN_WIDTH,
    min_height_px: MIN_WIN_HEIGHT,
};

/// The narrowest client width the panel is laid out for, declared to the
/// window manager when the window opens so a drag simply stops here rather
/// than squeezing the sections into a box they cannot fit.
///
/// Declared, never self-imposed: an app that answered a resize by resizing
/// its own window back up would fight the drag once per pointer sample.
///
/// The floor is what every section's primary column must still seat — the
/// widest unshrinkable row-command strip any section declares — plus the
/// navigation rail, which is never shed because it is the only route between
/// subjects. The optional columns beside the primary (a detail pane, an
/// impact column, an action rail) are shed in the section frame's drop order
/// when they do not fit, so they do not set this floor; a row whose inline
/// commands would be pushed off its own edge has nothing left to shed and
/// does.
pub const MIN_WIN_WIDTH: u32 = 640 + crate::view::RAIL_WIDTH;

/// The shortest client height the panel is laid out for (see
/// [`MIN_WIN_WIDTH`]).
pub const MIN_WIN_HEIGHT: u32 = 240;

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
