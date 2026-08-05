//! The System section: the resource, service and system-action overview
//! (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's overview view models ([`ResourceSummary`],
//! [`ServiceSummary`], [`SystemAction`]) and the section's layout, painting
//! and input. [`ResourceSummary`] also backs the window's header resource
//! band, which the frame module draws.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonAction, ButtonContent, Card, Chart, ControlRole, ControlState,
    ListRow, Meter, MeterValue, PanelAction, PressureKind, PressureState, ProgressValue,
    RecoveryState, RowAction, MAX_CHART_SAMPLES,
};

use super::{action_state, ListInfo, SbLayout, Section, Switchboard, SwitchboardAction};

/// One system resource reading (spec §17).
///
/// One fact drives two renderings that must never disagree: the Overview
/// section's resource [`Card`] (identity, numeric reading, and a semantic
/// Pressure Rail) and the always-visible header band's column (the same
/// identity and reading, the same rail tint, and one instrument — a [`Chart`]
/// of the bounded history where the caller supplies one, the [`Meter`]'s own
/// track where it does not). [`ResourceSummary::new`] alone leaves the meter
/// honestly quiet: an unmeasured value at no pressure, never a fabricated
/// reading; a host that can measure the resource adds
/// [`with_meter`](Self::with_meter).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceSummary {
    /// The resource's display name (e.g. "CPU", "Memory").
    pub name: String,
    /// The numeric reading as display text (e.g. "62%", "5.1 GiB").
    pub reading: String,
    /// Which resource this is, mapping to its semantic rail colour.
    pub kind: PressureKind,
    /// The resource's load, drawn as the Overview card's Heat Seam.
    pub activity: ActivityState,
    meter: MeterValue,
    meter_pressure: PressureState,
    history: [u16; MAX_CHART_SAMPLES],
    history_len: usize,
}

impl ResourceSummary {
    /// A resource reading named `name`, showing `reading` for `kind`, with
    /// the Overview card's Heat Seam driven by `activity`.
    ///
    /// The header band's meter starts honestly unmeasured, at no pressure,
    /// with no history — a host with no wired query or a denied capability
    /// for this resource stops here rather than fabricating a reading; add
    /// a real measurement with [`with_meter`](Self::with_meter).
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        reading: impl Into<String>,
        kind: PressureKind,
        activity: ActivityState,
    ) -> Self {
        Self {
            name: name.into(),
            reading: reading.into(),
            kind,
            activity,
            meter: MeterValue::Unmeasured,
            meter_pressure: PressureState::None,
            history: [0; MAX_CHART_SAMPLES],
            history_len: 0,
        }
    }

    /// This resource with the header band meter's measured `value`,
    /// `pressure` emphasis, and an oldest-to-newest history `samples` for the
    /// band's [`Chart`].
    ///
    /// Each sample is a permille fraction, clamped fail closed; the series
    /// is capped to the most recent [`MAX_CHART_SAMPLES`], dropping the
    /// oldest first, and held inline in this struct — never on the heap — so
    /// building the model never allocates on the instruments' account.
    #[must_use]
    pub fn with_meter(
        mut self,
        value: MeterValue,
        pressure: PressureState,
        samples: impl IntoIterator<Item = u16>,
    ) -> Self {
        self.meter = value;
        self.meter_pressure = pressure;
        self.history_len = 0;
        for sample in samples {
            if self.history_len == MAX_CHART_SAMPLES {
                self.history.copy_within(1.., 0);
                self.history_len -= 1;
            }
            self.history[self.history_len] = ProgressValue::new(sample).permille();
            self.history_len += 1;
        }
        self
    }
}

/// One system service row (spec §17).
///
/// Rendered as a [`ListRow`] with a state bead and one
/// capability-aware action [`Button`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSummary {
    /// The service's display name.
    pub name: String,
    /// A short trailing detail (e.g. its state).
    pub detail: String,
    /// The service's recovery posture, if any.
    pub recovery: RecoveryState,
    /// The action's label (e.g. "Restart", "Stop").
    pub action: String,
    /// Whether the caller may perform the action (fail closed when false).
    pub action_allowed: bool,
}

/// One system-level action shown in the Overview panel header (spec §17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemAction {
    /// The action's label (e.g. "Lock", "Shut Down").
    pub label: String,
    /// The action's role (e.g. [`ControlRole::System`] or
    /// [`ControlRole::Destructive`]); it drives the button's emphasis.
    pub role: ControlRole,
    /// Whether the caller may perform the action (fail closed when false).
    pub allowed: bool,
}

/// One resource rendered as an Overview [`Card`] and the header band's
/// [`Meter`], both built once from the same [`ResourceSummary`] rather than
/// re-derived per frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResourceEntry {
    pub(super) card: Card,
    pub(super) meter: Meter,
    pub(super) chart: Chart,
}

/// One service rendered as a [`ListRow`] plus its action [`Button`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ServiceEntry {
    pub(super) row: ListRow,
    pub(super) action: Button,
}

impl Switchboard {
    /// Build an Overview resource card and the header band's two instruments
    /// — the meter reading now, the chart of what it has been doing — for the
    /// same resource, from the one summary.
    pub(super) fn build_resource(res: ResourceSummary) -> ResourceEntry {
        let card = Card::new(res.name.clone())
            .with_body(res.reading.clone())
            .with_state(
                ControlState::idle()
                    .with_pressure(PressureState::Under(res.kind))
                    .with_activity(res.activity),
            );
        let chart =
            Chart::new(res.kind).with_samples(res.history[..res.history_len].iter().copied());
        let meter = Meter::new(res.name, res.reading, res.kind, res.meter)
            .with_pressure(res.meter_pressure);
        ResourceEntry { card, meter, chart }
    }

    /// Build an Overview service row + action button.
    pub(super) fn build_service(svc: ServiceSummary) -> ServiceEntry {
        let row = ListRow::new(svc.name)
            .with_trailing(svc.detail)
            .with_state(ControlState::idle().with_recovery(svc.recovery));
        let mut action = Button::labelled(svc.action);
        action.set_state(action_state(svc.action_allowed));
        ServiceEntry { row, action }
    }

    /// Build a system-action header button.
    pub(super) fn build_system_button(action: SystemAction) -> Button {
        let mut button = Button::new(ButtonContent::Label(action.label), action.role);
        button.set_state(action_state(action.allowed));
        button
    }

    /// Render the Overview panel: the panel chrome + system-action header, the
    /// fixed resource-card block, and the scrollable service rows below it.
    pub(super) fn render_overview(
        &self,
        surface: &mut Surface,
        layout: &SbLayout,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        self.panel
            .render(surface, layout.content, scale, theme, font);
        let Some(pc) = self.panel.content_rect(layout.content, scale, theme) else {
            return;
        };
        let gap = scale.scale_length(theme.metrics().control_gap);
        let card_h = Self::card_item_height(scale, theme);
        for (i, entry) in self.resources.iter().enumerate() {
            let top = pc.top() + to_i32(u32::try_from(i).unwrap_or(0).saturating_mul(card_h));
            if top + to_i32(card_h) > pc.bottom() {
                break;
            }
            let rect = Rect::new(pc.left(), top, pc.width, card_h.saturating_sub(gap));
            entry.card.render(surface, rect, scale, theme, font);
        }

        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(entry) = self.services.get(start + slot as usize) else {
                break;
            };
            let (row_rect, buttons) = Self::split_row(
                info.item_rect(slot),
                Self::row_actions(Section::Overview),
                scale,
                theme,
            );
            entry
                .row
                .render(surface, row_rect, scale, theme, font, None);
            if let Some(rect) = buttons.first() {
                entry.action.render(surface, *rect, scale, theme, font);
            }
        }
    }

    /// Route a pointer event to the Overview panel header (system actions) and
    /// its service rows.
    pub(super) fn overview_on_pointer(
        &mut self,
        event: &InputEvent,
        layout: &SbLayout,
        info: ListInfo,
        start: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        if let Some(PanelAction::HeaderActivated { index }) =
            self.panel.on_pointer(event, layout.content, scale, theme)
        {
            return Some(SwitchboardAction::System { index });
        }
        let mut selected = None;
        for slot in 0..info.visible() {
            let idx = start + slot as usize;
            let (row_rect, buttons) = Self::split_row(
                info.item_rect(slot),
                Self::row_actions(Section::Overview),
                scale,
                theme,
            );
            let Some(entry) = self.services.get_mut(idx) else {
                break;
            };
            if buttons.first().is_some_and(|rect| {
                entry.action.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Service { index: idx });
            }
            if entry.row.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                selected = Some(idx);
            }
        }
        if let Some(idx) = selected {
            for (i, entry) in self.services.iter_mut().enumerate() {
                entry.row.set_selected(i == idx);
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "system_tests.rs"]
mod tests;
