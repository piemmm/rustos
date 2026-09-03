//! The Switchboard tray capsule at the taskbar's trailing end.
//!
//! [`SwitchboardTray`] is the model behind the always-right-most Switchboard
//! slot: one shared `lib/controls` [`TraySignal`] capsule showing the
//! desktop's live system posture at a glance. The session feeds it the
//! latest [`TraySummary`] the Switchboard service published
//! ([`set_summary`](SwitchboardTray::set_summary)) and its own count of
//! unresponsive applications
//! ([`set_unresponsive`](SwitchboardTray::set_unresponsive)); each change
//! rebuilds the capsule through the one pure derive (`derive_signal`), so
//! the mapping from facts to furniture is spelled exactly once.
//!
//! The derive picks one *dominant* state for the badge, label, and value —
//! in precedence order: hung applications, resource pressure, background
//! jobs, recovery candidates, then calm. The orthogonal furniture composes
//! regardless of which dominates: the working Heat Seam whenever jobs run,
//! the leading pressure rail whenever a pressure is named, and the recovery
//! posture (hung outranking recoverable) whenever either holds. The
//! hover readout previews the busiest task only in the *calm* state; in
//! every other state the value line keeps the dominant state's own figure —
//! the more urgent information — rather than the preview. With no summary at
//! all (the service absent or not yet published) the capsule derives calm
//! idle with no value: it never fabricates a reading it was not fed (fail
//! closed).
//!
//! The capsule wears the signed-in account rather than a system glyph: the
//! desktop's one system control point is the account it belongs to, and the
//! rows behind it (lock, switch user, log out, shut down) are that account's.
//! The picture is the account's circular identity disc
//! ([`identity_disc`](SwitchboardTray::identity_disc)) — the same mark the
//! login screen drew — so the mark a person signed in as is the mark they then
//! live with. An account has no picture of its own yet, so the disc is its
//! monogram; a name that yields no character still marks the disc, so there is
//! always a picture and it is always a circle.
//!
//! The readout always carries one safe action, "Open Switchboard": a click
//! that completes on it reports [`TraySignalAction::Activated`], which the
//! taskbar's input router (`crate::input`) turns into the response that
//! opens the Switchboard window. The router reaches the same destination
//! from the capsule itself: a primary press opens the running-task section,
//! and a press held past the long-press threshold opens Recovery instead —
//! the session asks the Switchboard service to open, or revive and open,
//! its window at that section.

use alloc::format;
use alloc::string::String;

use tairix_abi::switchboard_ipc::{TrayPermille, TrayPressureKind, TraySummary};
use tairix_controls::{
    ActivityState, Button, ControlState, FocusState, PointerState, PressureKind, PressureState,
    RecoveryState, TrayBadge, TrayBadgeContent, TrayBadgeTone, TraySignal, TraySignalAction,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{monogram_disc, monogram_of, IconKind};
use tairix_input::InputEvent;
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::repaint::TaskbarRepaint;

/// The Switchboard tray capsule: the shared [`TraySignal`] control plus the
/// facts it is derived from — the latest published [`TraySummary`] and the
/// session's count of unresponsive applications.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchboardTray {
    signal: TraySignal,
    summary: Option<TraySummary>,
    unresponsive: u16,
    monogram: char,
}

impl Default for SwitchboardTray {
    fn default() -> Self {
        Self::new()
    }
}

impl SwitchboardTray {
    /// A calm tray: no summary published yet, nothing unresponsive, and no
    /// account named — so the disc bears the mark a nameless account gets
    /// rather than a fabricated initial.
    #[must_use]
    pub fn new() -> Self {
        Self {
            signal: derive_signal(None, 0).into_signal(PointerState::None, FocusState::default()),
            summary: None,
            unresponsive: 0,
            monogram: monogram_of(""),
        }
    }

    /// Adopt the signed-in account's name, from which the disc takes its
    /// mark. Returns the surfaces whose pixels the new mark actually moved.
    ///
    /// Only the embedder knows who is signed in, and the bar holds nothing
    /// but the one character it draws: a name is not an identity the bar can
    /// act on, and keeping only the mark means a live update cannot leak a
    /// name onto a surface that never shows one.
    pub fn set_account(&mut self, name: &str) -> TaskbarRepaint {
        let monogram = monogram_of(name);
        if self.monogram == monogram {
            return TaskbarRepaint::NONE;
        }
        self.monogram = monogram;
        TaskbarRepaint {
            bar: true,
            ..TaskbarRepaint::NONE
        }
    }

    /// The account's `side`×`side` circular identity picture, in the theme's
    /// accent — the same disc the login screen drew for the same account.
    ///
    /// Produced at exactly the side the capsule draws at, so nothing scales
    /// or crops it, and always a circle. `None` only where a picture of that
    /// side cannot exist at all, which leaves the capsule on its own glyph
    /// rather than a blank slot.
    #[must_use]
    pub fn identity_disc(&self, side: u32, scale: Scale, theme: &Theme) -> Option<Surface> {
        let palette = theme.palette();
        monogram_disc(
            self.monogram,
            side,
            BitmapFont::for_role(theme.fonts(), TextRole::Heading, scale),
            (Color::from(palette.accent), Color::from(palette.on_accent)),
        )
    }

    /// Adopt the latest published summary — or its absence, when the
    /// Switchboard service is gone — rebuilding the capsule. Returns the
    /// surfaces whose pixels the new reading actually moved.
    pub fn set_summary(&mut self, summary: Option<TraySummary>) -> TaskbarRepaint {
        if self.summary == summary {
            return TaskbarRepaint::NONE;
        }
        self.summary = summary;
        self.rebuild()
    }

    /// Adopt the session's count of unresponsive applications, rebuilding
    /// the capsule. Returns the surfaces whose pixels the new count moved.
    pub fn set_unresponsive(&mut self, count: u16) -> TaskbarRepaint {
        if self.unresponsive == count {
            return TaskbarRepaint::NONE;
        }
        self.unresponsive = count;
        self.rebuild()
    }

    /// The capsule control, for painting and readout sizing.
    #[must_use]
    pub const fn signal(&self) -> &TraySignal {
        &self.signal
    }

    /// Whether the readout is expanded — the pointer is over the capsule or
    /// over the open readout itself.
    #[must_use]
    pub fn is_expanded(&self) -> bool {
        self.signal.is_expanded()
    }

    /// Whether the publishing service has attested that it can power this
    /// machine off or restart it.
    ///
    /// No summary at all — the service has not published yet, or has died —
    /// reads as `false`: silence is never permission, so the desktop's power
    /// rows stay refused until something actually claims the authority.
    #[must_use]
    pub fn power_capable(&self) -> bool {
        self.summary.is_some_and(|summary| summary.power_capable)
    }

    /// Feed a pointer `event` (motion, press, or release) to the capsule and
    /// its open readout — hover tracking plus the readout's "Open
    /// Switchboard" safe action — reporting whether the capsule's visual
    /// state changed and the action it reports, if the click completed on
    /// it. `capsule` and `readout` are the slot and open-readout rectangles
    /// at the current scale ([`Rect::EMPTY`] while the readout is
    /// collapsed).
    pub(crate) fn on_pointer(
        &mut self,
        event: &InputEvent,
        capsule: Rect,
        readout: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> (bool, Option<TraySignalAction>) {
        let before = self.signal.state();
        let action = self
            .signal
            .on_pointer(event, capsule, readout, scale, theme, damage);
        (self.signal.state() != before, action)
    }

    /// Track the pointer over the capsule and its readout, reporting whether
    /// the capsule's visual state changed.
    ///
    /// The shared control owns the hover/expansion rule, so this synthesises
    /// the motion event it consumes; motion alone never completes the
    /// readout's action, so the action this reports is always discarded.
    pub(crate) fn track(
        &mut self,
        point: Point,
        capsule: Rect,
        readout: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        self.on_pointer(
            &InputEvent::PointerMoved { to: point },
            capsule,
            readout,
            scale,
            theme,
            damage,
        )
        .0
    }

    /// Tell the capsule the pointer has left it and its readout, reporting
    /// whether its visual state changed.
    ///
    /// The counterpart of [`track`](Self::track) for the case a position
    /// cannot state: the pointer no longer rests on the bar at all, though it
    /// may still be at the capsule's own coordinates because a window was
    /// drawn over it. The shared control owns the hover/expansion rule, so it
    /// is told rather than re-derived here.
    pub(crate) fn pointer_left(
        &mut self,
        capsule: Rect,
        readout: Rect,
        damage: &mut Region,
    ) -> bool {
        self.signal.pointer_left(capsule, readout, damage)
    }

    /// Re-derive the capsule from the current facts, carrying over the
    /// pointer and focus interaction state so a live update never drops an
    /// open hover readout, and report which surfaces the new derive actually
    /// redraws.
    ///
    /// The two surfaces draw different parts of one signal, so they are
    /// latched separately. The bar's capsule is the glyph, the composed
    /// state, and the badge; the readout adds the label, the value line, and
    /// the action. A calm desktop republishes a fresh CPU reading every
    /// couple of seconds and *only* the value line moves — so gating the bar
    /// on the whole signal would repaint the full-width bar, on a cadence, to
    /// redraw pixels nobody can tell apart. The readout is latched only while
    /// it is expanded, because that is the only time it is on screen at all.
    fn rebuild(&mut self) -> TaskbarRepaint {
        let interaction = self.signal.state();
        let next = derive_signal(self.summary.as_ref(), self.unresponsive)
            .into_signal(interaction.pointer, interaction.focus);
        let parts = TaskbarRepaint {
            bar: !self.signal.draws_same_capsule(&next),
            readout: next.is_expanded() && self.signal != next,
            ..TaskbarRepaint::NONE
        };
        self.signal = next;
        parts
    }
}

/// What [`derive_signal`] computed for the capsule: the composed furniture
/// state and the dominant state's badge, label, and value line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivedSignal {
    /// The composed furniture state (activity, pressure, recovery).
    pub(crate) state: ControlState,
    /// The dominant state's badge, if any.
    pub(crate) badge: Option<TrayBadge>,
    /// The dominant state's name — the readout's first line.
    pub(crate) label: String,
    /// The readout's value line, if the state has one.
    pub(crate) value: Option<String>,
}

impl DerivedSignal {
    /// The [`TraySignal`] this derive describes, with the session-owned
    /// `pointer` and `focus` interaction state carried in.
    ///
    /// Every derive carries the same "Open Switchboard" safe action
    /// ([`OPEN_SWITCHBOARD_ACTION`]) — the readout's one routed action,
    /// regardless of which state dominates.
    fn into_signal(self, pointer: PointerState, focus: FocusState) -> TraySignal {
        let mut state = self.state;
        state.pointer = pointer;
        state.focus = focus;
        let mut signal = TraySignal::new(IconKind::User, self.label)
            .with_state(state)
            .with_action(Button::labelled(OPEN_SWITCHBOARD_ACTION));
        if let Some(value) = self.value {
            signal = signal.with_value(value);
        }
        if let Some(badge) = self.badge {
            signal = signal.with_badge(badge);
        }
        signal
    }
}

/// The readout's safe-action label — named once so the derive and its tests
/// never risk spelling it two different ways.
pub(crate) const OPEN_SWITCHBOARD_ACTION: &str = "Open Switchboard";

/// Derive the capsule from the latest `summary` and the session's count of
/// `unresponsive` applications.
///
/// One dominant state drives the badge, label, and value — in precedence
/// order: hung applications (alert badge), resource pressure (warning count
/// badge), background jobs (accent count badge), recovery candidates
/// (recovery count badge), then calm (no badge, the busiest-task preview or
/// the overall CPU figure as the value). The orthogonal furniture composes
/// regardless of which dominates. An absent summary derives the calm capsule
/// with no value rather than fabricating a reading (fail closed).
pub(crate) fn derive_signal(summary: Option<&TraySummary>, unresponsive: u16) -> DerivedSignal {
    let jobs = summary.map_or(0, |summary| summary.jobs);
    let recovery = summary.map_or(0, |summary| summary.recovery);
    let pressure = summary.and_then(|summary| summary.pressure);

    let mut state = ControlState::idle();
    if jobs > 0 {
        state = state.with_activity(ActivityState::Working);
    }
    if let Some(pressure) = pressure {
        state = state.with_pressure(PressureState::Under(pressure_kind(pressure.kind)));
    }
    if unresponsive > 0 {
        state = state.with_recovery(RecoveryState::Hung);
    } else if recovery > 0 {
        state = state.with_recovery(RecoveryState::Recoverable);
    }

    if unresponsive > 0 {
        return DerivedSignal {
            state,
            badge: Some(TrayBadge::new(
                TrayBadgeContent::Alert,
                TrayBadgeTone::Danger,
            )),
            label: String::from("Not responding"),
            value: Some(counted(unresponsive, "application")),
        };
    }
    if let Some(pressure) = pressure {
        return DerivedSignal {
            state,
            badge: Some(TrayBadge::new(
                TrayBadgeContent::Count(u16::from(pressure.count.as_u8())),
                TrayBadgeTone::Warning,
            )),
            label: format!("{} pressure", kind_name(pressure.kind)),
            value: Some(format!("{}%", percent(pressure.level))),
        };
    }
    if jobs > 0 {
        return DerivedSignal {
            state,
            badge: Some(TrayBadge::new(
                TrayBadgeContent::Count(jobs),
                TrayBadgeTone::Accent,
            )),
            label: String::from("Background work"),
            value: Some(counted(jobs, "job")),
        };
    }
    if recovery > 0 {
        return DerivedSignal {
            state,
            badge: Some(TrayBadge::new(
                TrayBadgeContent::Count(recovery),
                TrayBadgeTone::Recovery,
            )),
            label: String::from("Recovery available"),
            value: Some(counted(recovery, "task")),
        };
    }
    DerivedSignal {
        state,
        badge: None,
        label: String::from("System normal"),
        value: summary.map(calm_value),
    }
}

/// The calm state's value line: the busiest task when one is known, else
/// the overall CPU figure.
fn calm_value(summary: &TraySummary) -> String {
    match &summary.top_task {
        Some(task) => format!(
            "{} — {}% CPU",
            task.name.as_str(),
            percent(task.cpu_permille)
        ),
        None => format!("CPU {}%", percent(summary.cpu_busy_permille)),
    }
}

/// Map the wire pressure kind onto the shared control vocabulary's.
///
/// The two closed sets mirror each other deliberately — the ABI cannot
/// depend on `lib/controls` — so the pairing is spelled once, here, at the
/// one consumer that holds both.
fn pressure_kind(kind: TrayPressureKind) -> PressureKind {
    match kind {
        TrayPressureKind::Cpu => PressureKind::Cpu,
        TrayPressureKind::Memory => PressureKind::Memory,
        TrayPressureKind::Disk => PressureKind::Disk,
        TrayPressureKind::Network => PressureKind::Network,
        TrayPressureKind::Power => PressureKind::Power,
        TrayPressureKind::Thermal => PressureKind::Thermal,
    }
}

/// The display name of a pressure kind, as the state label spells it
/// ("Memory pressure").
fn kind_name(kind: TrayPressureKind) -> &'static str {
    match kind {
        TrayPressureKind::Cpu => "CPU",
        TrayPressureKind::Memory => "Memory",
        TrayPressureKind::Disk => "Disk",
        TrayPressureKind::Network => "Network",
        TrayPressureKind::Power => "Power",
        TrayPressureKind::Thermal => "Thermal",
    }
}

/// A permille reading as a whole percentage, rounded to nearest.
fn percent(value: TrayPermille) -> u16 {
    value.as_u16().saturating_add(5) / 10
}

/// `"1 thing"` / `"N things"` — the count spelled before its noun.
fn counted(count: u16, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}
