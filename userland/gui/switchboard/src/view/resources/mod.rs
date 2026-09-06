//! The Resources section: one pane per resource *device*, instrument-led
//! (`plans/NEW-SWITCHBOARD.md` S4, `plans/switchboard/02`–`07`).
//!
//! A leading rail of the devices discovery found — grouped processor and
//! memory, then one entry per mounted volume, per managed interface, the
//! display path, and the machine's own fact panes — and the selected
//! device's pane beside it. The rail grows with the machine: twelve cores,
//! four volumes and three interfaces need no redesign and neither does the
//! fifth disk.
//!
//! # Selecting a device performs no I/O
//!
//! The rail's selection changes which pane is *drawn* from state the sampler
//! has already delivered. It issues no query, opens no store and waits on
//! nothing; a pane with no sample yet reads unavailable rather than blocking
//! for one.

use alloc::vec::Vec;

use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::{
    ActionRail, Button, ButtonContent, ComboBox, Panel, RailAction, StatusPill, Tab, Tabs,
    TabsAction, TabsOrientation,
};

use super::frame::{SectionAnatomy, SectionFrame, ACTION_RAIL_WIDTH};
use super::refresh::restate_rail;
use super::{
    resolve_selection, FocusSweep, ListInfo, SectionCtx, SectionOutcome, SectionView, Switchboard,
    SwitchboardAction, SwitchboardModel,
};

mod device;
mod pane;

pub use device::{
    DeviceAction, DeviceGroup, DeviceId, PressureBanner, ResourceControl, ResourceDevice,
    ResourceReport, TaskCostColumn,
};
pub use pane::{
    BlockBody, BlockSpan, CompositionPart, ConsumerRow, CoreCell, HeroInstrument, PaneBlock,
    PaneHero,
};

pub(super) use pane::PaneItem;

/// The rail's logical width: wide enough for the longest device name beside
/// its reading at the reference density, and narrow enough that the rail,
/// the pane and the action column all still seat in the smallest window the
/// panel allows.
const SIDEBAR_WIDTH: u32 = 168;

/// The footer band's logical height: the sampling cadence and the
/// auto-refresh toggle.
const FOOTER_HEIGHT: u32 = 28;

/// The band `ComboBox`'s logical width, which replaces the rail's *route*
/// when the frame sheds the sidebar.
const BAND_COMBO_WIDTH: u32 = 132;

/// The action rail's caption. The rail control carries no caption of its
/// own, so the section seats it in a [`Panel`], which already defines what a
/// titled container looks like.
const RAIL_TITLE: &str = "DEVICE ACTIONS";

/// Which of the section's cursor stops the keyboard is on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Stop {
    /// A device entry in the rail, by its index in the report.
    Device(usize),
    /// The pressure banner's relief command.
    Relief,
    /// A command in the trailing action rail, by its slot.
    Rail(usize),
}

/// The Resources section: the report it draws, the device rail, the selected
/// device's compiled pane, and that device's commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResourcesSection {
    /// The report every pane is drawn from, kept so a device switch needs no
    /// fresh sample.
    pub(super) report: ResourceReport,
    /// The device rail. Its entries are a *window* of the report, starting
    /// at [`Self::rail_offset`], so a hundred-core machine's rail scrolls
    /// rather than drawing past its own column.
    pub(super) rail: Tabs,
    /// The first device the rail's window shows.
    pub(super) rail_offset: usize,
    /// The selected device's own identity, so the selection survives a
    /// refresh rather than following whichever entry slid into its place.
    pub(super) selected: Option<DeviceId>,
    /// The selected device's pane, compiled to its drawable flow.
    pub(super) items: Vec<PaneItem>,
    /// The pane width and scale the flow was compiled for, so a resize
    /// recompiles it rather than leaving the scroll range describing a
    /// different layout.
    pub(super) compiled_for: (u32, Scale),
    /// The selected device's commands.
    pub(super) actions: ActionRail,
    /// The plate the commands are seated in, which carries their caption.
    pub(super) action_panel: Panel,
    /// The banner's relief command, when the selected device wears a banner.
    pub(super) relief: Option<Button>,
    /// The device chooser the band grows when the frame sheds the rail, so
    /// losing the rail never loses a pane.
    pub(super) band_combo: ComboBox,
    /// Where the content cursor is.
    pub(super) focus: usize,
    /// Which of the focused stop's actions the keyboard is on.
    pub(super) action: usize,
}

impl ResourcesSection {
    /// An empty section: no devices, nothing selected.
    pub(super) fn new() -> Self {
        let mut section = Self {
            report: ResourceReport::default(),
            rail: Tabs::new(Vec::new()).with_orientation(TabsOrientation::Vertical),
            rail_offset: 0,
            selected: None,
            items: Vec::new(),
            compiled_for: (0, Scale::ONE),
            actions: ActionRail::new(Vec::new()),
            action_panel: Panel::new(RAIL_TITLE),
            relief: None,
            band_combo: ComboBox::new(Vec::new()),
            focus: 0,
            action: 0,
        };
        section.rebuild();
        section
    }

    /// The selected device, or [`None`] when the report holds none.
    fn device(&self) -> Option<&ResourceDevice> {
        let id = self.selected?;
        self.report.devices.iter().find(|device| device.id == id)
    }

    /// The selected device's index in the report.
    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.report.devices.iter().position(|d| d.id == id)
    }

    /// Rebuild the rail, the chooser, the commands and the pane flow from
    /// the report and the current selection.
    fn rebuild(&mut self) {
        self.rail = build_rail(&self.report.devices, self.rail_offset, self.selected);
        self.band_combo = build_combo(&self.report.devices, self.selected_index());
        let commands = self
            .device()
            .map(|device| device.actions.iter().map(build_command).collect())
            .unwrap_or_default();
        restate_rail(&mut self.actions, commands);
        self.relief = self.device().and_then(|device| {
            device
                .banner
                .as_ref()
                .and_then(|banner| banner.relief.as_ref())
                .map(build_command)
        });
        // The flow is recompiled for the width it will be drawn at, which
        // `relayout` supplies; until then it is compiled for the width it
        // last had, so the scroll range always describes the flow on screen.
        let (width, scale) = self.compiled_for;
        self.compile(width, scale);
    }

    /// Compile the selected device's pane for a pane `width` at `scale`.
    fn compile(&mut self, width: u32, scale: Scale) {
        self.compiled_for = (width, scale);
        self.items = match self.device() {
            Some(device) => pane::compile(
                &device.hero,
                device.banner.is_some(),
                &device.blocks,
                device.kind,
                pane::cells_per_row(width, scale),
            ),
            None => Vec::new(),
        };
    }

    /// The cursor's stops, in the order Up/Down walks them: the rail's
    /// device entries, then the banner's relief, then the commands.
    fn stops(&self) -> Vec<Stop> {
        let mut stops: Vec<Stop> = (0..self.report.devices.len()).map(Stop::Device).collect();
        if self.relief.is_some() {
            stops.push(Stop::Relief);
        }
        stops.extend((0..self.actions.len()).map(Stop::Rail));
        stops
    }

    /// The stop at cursor `index`.
    fn stop_at(&self, index: usize) -> Option<Stop> {
        self.stops().get(index).copied()
    }

    /// Select the device at `index` in the report, keeping it in the rail's
    /// window and recompiling the pane.
    ///
    /// This is the whole of what selecting a device does: no query is
    /// issued, no store opened, nothing waited on.
    fn select(&mut self, index: usize) {
        let Some(device) = self.report.devices.get(index) else {
            return;
        };
        self.selected = Some(device.id);
        self.keep_in_window(index);
        self.rebuild();
    }

    /// Scroll the rail's window so the device at `index` is inside it.
    fn keep_in_window(&mut self, index: usize) {
        if index < self.rail_offset {
            self.rail_offset = index;
        }
    }

    /// The rail's rectangle, or the empty one when the frame seated no
    /// sidebar: drawn nowhere reports nothing.
    fn rail_rect(frame: &SectionFrame) -> Rect {
        frame.sidebar.unwrap_or(Rect::new(0, 0, 0, 0))
    }

    /// Where the pane's own flow draws: the primary column, below the
    /// banner when one is shown.
    fn pane_rect(frame: &SectionFrame) -> Rect {
        frame.primary
    }

    /// The banner's rectangle within the pane, and the rectangle its relief
    /// command occupies inside it.
    fn banner_layout(
        &self,
        frame: &SectionFrame,
        scale: Scale,
        theme: &Theme,
    ) -> Option<(Rect, Rect)> {
        self.device()?.banner.as_ref()?;
        let primary = frame.primary;
        let height = Switchboard::row_item_height(scale, theme).saturating_mul(2);
        if primary.height < height {
            return None;
        }
        let band = Rect::new(primary.left(), primary.top(), primary.width, height);
        let button_w = scale.scale_length(BAND_COMBO_WIDTH).min(band.width);
        let button_h = scale
            .scale_length(theme.metrics().control_height)
            .min(band.height);
        let button = Rect::new(
            band.left() + to_i32(band.width.saturating_sub(button_w)),
            band.top() + to_i32(band.height.saturating_sub(button_h) / 2),
            button_w,
            button_h,
        );
        Some((band, button))
    }
}

impl ResourcesSection {
    /// The typed action a command reports, or [`None`] for one this section
    /// resolves itself.
    ///
    /// A "sort tasks by" command is a *view* transition rather than a
    /// privileged operation: it shows the Tasks table ordered by the cost
    /// this device is about, so a busy device is traced to the tasks on it.
    /// Every other command is reported for the service to authorise and
    /// apply; the view performs no privileged work.
    fn command_outcome(&self, control: ResourceControl) -> Option<SectionOutcome> {
        let index = self.selected_index()?;
        match control {
            ResourceControl::SortTasksBy(column) => Some(SectionOutcome::ShowTasksBy { column }),
            _ => Some(SectionOutcome::Action(SwitchboardAction::Resource {
                index,
                control,
            })),
        }
    }

    /// Paint the pressure banner: its band pill, what has happened, and the
    /// relief the model recommends.
    fn render_banner(&self, surface: &mut Surface, band: Rect, button: Rect, ctx: SectionCtx<'_>) {
        let Some(banner) = self.device().and_then(|device| device.banner.as_ref()) else {
            return;
        };
        let palette = ctx.theme.palette();
        let pill =
            StatusPill::new(banner.band.clone()).with_tone(tairix_theme::SignalRole::Warning);
        let pill_w = pill.measured_width(ctx.scale, ctx.theme).min(band.width);
        let pill_h = StatusPill::measured_height(ctx.scale, ctx.theme).min(band.height);
        pill.render(
            surface,
            Rect::new(band.left(), band.top(), pill_w, pill_h),
            ctx.scale,
            ctx.theme,
        );
        let text_left = band.left()
            + to_i32(
                pill_w.saturating_add(
                    ctx.scale
                        .scale_length(ctx.theme.metrics().control_gap)
                        .max(1),
                ),
            );
        ctx.font.draw_text(
            surface,
            text_left,
            band.top(),
            &banner.summary,
            Color::from(palette.on_surface),
        );
        ctx.font.draw_text(
            surface,
            text_left,
            band.top() + to_i32(ctx.font.line_height()),
            &banner.detail,
            Color::from(palette.on_surface_muted),
        );
        if let Some(relief) = self.relief.as_ref() {
            relief.render(surface, button, ctx.scale, ctx.theme);
        }
    }

    /// Paint the footer: the cadence and window the readings are averaged
    /// over, so a rate a reader acts on states its own span.
    fn render_footer(surface: &mut Surface, ctx: SectionCtx<'_>) {
        let footer = ctx.frame.footer;
        if footer.height == 0 {
            return;
        }
        ctx.font.draw_text(
            surface,
            footer.left(),
            footer.top(),
            CADENCE,
            Color::from(ctx.theme.palette().on_surface_muted),
        );
    }
}

/// What the footer states about every reading on the pane.
///
/// A pane that states its own averaging window is the difference between a
/// rate a reader can act on and a number.
const CADENCE: &str = "Sampling every 1.0 s";

/// The rail's entries: a window of the report from `offset`, each carrying
/// its own reading and trace, and a group heading on the entry that *starts*
/// its group so a heading can never point at one that is not there.
fn build_rail(devices: &[ResourceDevice], offset: usize, selected: Option<DeviceId>) -> Tabs {
    let mut tabs = Vec::new();
    let mut previous: Option<DeviceGroup> = None;
    for (index, device) in devices.iter().enumerate() {
        let starts_group = previous != Some(device.group);
        previous = Some(device.group);
        if index < offset {
            continue;
        }
        let mut tab = Tab::new(device.name.clone())
            .with_reading(crate::view::reading::reading_text(&device.reading));
        if starts_group {
            tab = tab.with_group(device.group.heading());
        }
        if !device.trend.is_empty() {
            tab = tab.with_trend(
                tairix_controls::Chart::new(device.kind).with_samples(device.trend.iter().copied()),
            );
        }
        tabs.push(tab);
    }
    let mut rail = Tabs::new(tabs).with_orientation(TabsOrientation::Vertical);
    if let Some(id) = selected {
        if let Some(position) = devices
            .iter()
            .position(|device| device.id == id)
            .and_then(|index| index.checked_sub(offset))
        {
            rail.adopt_selected(position);
            rail.adopt_current(Some(position));
        }
    }
    rail
}

/// The band's device chooser, holding the same device set the rail does.
fn build_combo(devices: &[ResourceDevice], selected: Option<usize>) -> ComboBox {
    let mut combo =
        ComboBox::new(devices.iter().map(|d| d.name.clone()).collect()).with_placeholder("Device");
    if let Some(index) = selected {
        combo.set_selected(index);
    }
    combo
}

/// One command as a [`Button`], refused visibly when the caller cannot take
/// it so the reader learns before attempting it.
fn build_command(action: &DeviceAction) -> Button {
    let mut button = Button::new(ButtonContent::Label(action.label.clone()), action.role);
    button.set_state(action.verdict.to_state());
    button
}

impl SectionView for ResourcesSection {
    /// The rail is the sidebar and the pane is the primary column; the frame
    /// sheds the commands first, then the rail, whose *route* moves into the
    /// band so no destination is lost.
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: None,
            sidebar_width: SIDEBAR_WIDTH,
            header_height: 0,
            detail_width: 0,
            impact_width: 0,
            rail_width: ACTION_RAIL_WIDTH,
            footer_height: FOOTER_HEIGHT,
            primary_row_commands: 0,
        }
    }

    fn adopt(&mut self, model: &SwitchboardModel) {
        let previous = self.selected;
        let stop = self.stop_at(self.focus);
        self.report.clone_from(&model.resources);
        self.selected =
            resolve_selection(previous, self.report.devices.iter().map(|device| device.id));
        self.rail_offset = self
            .rail_offset
            .min(self.report.devices.len().saturating_sub(1));
        self.rebuild();
        // The cursor is put back on the same *kind* of stop, so a device
        // cursor follows the device it was on rather than staying on a
        // number that now names a different one.
        self.focus = match stop {
            Some(Stop::Device(_)) | None => self.selected_index().unwrap_or(0),
            Some(Stop::Relief) => self
                .stops()
                .iter()
                .position(|s| *s == Stop::Relief)
                .unwrap_or(0),
            Some(Stop::Rail(slot)) => self
                .stops()
                .iter()
                .position(|s| *s == Stop::Rail(slot))
                .unwrap_or(0),
        };
        self.focus = self.focus.min(self.focus_span().saturating_sub(1));
        self.action = 0;
    }

    /// Recompile the pane for the width it will be drawn at.
    ///
    /// The per-core grid re-wraps with the pane's width, so the flow's row
    /// spans depend on it; recompiling here — once per resize rather than
    /// per paint — is what keeps the scroll range describing the flow that
    /// is actually on screen.
    fn relayout(&mut self, frame: &SectionFrame, scale: Scale, _theme: &Theme) {
        let width = Self::pane_rect(frame).width;
        if self.compiled_for != (width, scale) {
            self.compile(width, scale);
        }
    }

    /// The pane's flow, in rows: the scroll range's content extent.
    fn item_count(&self) -> usize {
        pane::extent(&self.items)
    }

    fn focus_span(&self) -> usize {
        self.stops().len()
    }

    /// No stop is a scrollable row: the cursor walks the rail and the
    /// commands, and the pane's own flow is scrolled by the reader rather
    /// than by a cursor moving through it.
    fn focus_row(&self, _index: usize) -> Option<usize> {
        None
    }

    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo {
        ListInfo::rows(Self::pane_rect(frame), self.item_count(), scale, theme)
    }

    /// Zero: a device's commands live in the anchored rail beside the pane.
    fn row_buttons(&self) -> u32 {
        0
    }

    fn focused_action_count(&self) -> usize {
        1
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    /// Move the cursor, selecting the device a rail stop names so the pane
    /// and the commands always describe the entry the reader is on.
    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
        if let Some(Stop::Device(row)) = self.stop_at(index) {
            self.select(row);
        }
    }

    fn row_action(&self) -> usize {
        self.action
    }

    fn set_row_action(&mut self, index: usize, _sweep: &mut FocusSweep<'_, '_>) {
        self.action = index;
    }

    /// Commit the focused stop.
    ///
    /// A command stop hands the key to the button, which decides for itself
    /// whether it may fire: a disabled command, or one whose Authority Mark
    /// denies the caller, refuses the keyboard exactly as it refuses the
    /// pointer.
    fn activate_focused(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        match self.stop_at(self.focus)? {
            Stop::Device(row) => {
                self.select(row);
                None
            }
            Stop::Relief => {
                let _ = damage;
                // The button decides for itself whether it may fire, so a
                // disabled relief refuses the keyboard as it refuses the
                // pointer.
                self.relief.as_mut()?.on_key(key)?;
                self.command_outcome(ResourceControl::Relieve)
            }
            Stop::Rail(slot) => {
                let rect = ctx.frame.rail?;
                self.actions.set_focus(Some(slot), rect, damage);
                match self.actions.on_key(key, rect, damage)? {
                    RailAction::Activate { index } => {
                        let control = self.device()?.actions.get(index)?.control;
                        self.command_outcome(control)
                    }
                }
            }
        }
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        if let Some(rect) = ctx.frame.sidebar {
            self.rail.render(surface, rect, ctx.scale, ctx.theme);
        }
        let mut pane = Self::pane_rect(&ctx.frame);
        if let Some((band, button)) = self.banner_layout(&ctx.frame, ctx.scale, ctx.theme) {
            self.render_banner(surface, band, button, ctx);
            let used = band.height.min(pane.height);
            pane = Rect::new(
                pane.left(),
                pane.top() + to_i32(used),
                pane.width,
                pane.height.saturating_sub(used),
            );
        }
        let start = u32::try_from(ctx.start).unwrap_or(u32::MAX);
        pane::render(
            surface,
            &self.items,
            pane,
            start,
            ctx.scale,
            ctx.theme,
            ctx.font,
        );
        if let Some(rect) = ctx.frame.rail {
            self.action_panel
                .render(surface, rect, ctx.scale, ctx.theme);
            if let Some(content) = self.action_panel.content_rect(rect, ctx.scale, ctx.theme) {
                self.actions.render(surface, content, ctx.scale, ctx.theme);
            }
        }
        Self::render_footer(surface, ctx);
    }

    /// Route a pointer event to the rail, the banner's relief, or the
    /// commands.
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if let Some(rect) = ctx.frame.sidebar {
            if let Some(action) = self
                .rail
                .on_pointer(event, rect, ctx.scale, ctx.theme, damage)
            {
                match action {
                    TabsAction::Selected { index } => {
                        self.select(self.rail_offset.saturating_add(index));
                        self.focus = self.rail_offset.saturating_add(index);
                        return None;
                    }
                }
            }
        }
        if let Some((_, button)) = self.banner_layout(&ctx.frame, ctx.scale, ctx.theme) {
            if let Some(relief) = self.relief.as_mut() {
                if relief.on_pointer(event, button, damage).is_some() {
                    return self.command_outcome(ResourceControl::Relieve);
                }
            }
        }
        let rect = ctx.frame.rail?;
        let content = self.action_panel.content_rect(rect, ctx.scale, ctx.theme)?;
        match self
            .actions
            .on_pointer(event, content, ctx.scale, ctx.theme, damage)?
        {
            RailAction::Activate { index } => {
                let control = self.device()?.actions.get(index)?.control;
                self.command_outcome(control)
            }
        }
    }

    fn apply_focus_marks(&mut self, focused: bool, sweep: &mut FocusSweep<'_, '_>) {
        let stop = focused.then(|| self.stop_at(self.focus)).flatten();
        let slot = match stop {
            Some(Stop::Rail(slot)) => Some(slot),
            _ => None,
        };
        let rect = sweep.ctx.and_then(|ctx| {
            ctx.frame
                .rail
                .and_then(|rect| self.action_panel.content_rect(rect, ctx.scale, ctx.theme))
        });
        sweep.rail(&mut self.actions, slot, rect);
        for (index, button) in self.actions.items_mut().iter_mut().enumerate() {
            button.set_focused(slot == Some(index));
            button.set_in_focus_field(slot.is_some());
        }
        if let Some(relief) = self.relief.as_mut() {
            let on_relief = matches!(stop, Some(Stop::Relief));
            relief.set_focused(on_relief);
            relief.set_in_focus_field(on_relief);
        }
        // A rail entry the reader has navigated away from must not keep its
        // ring lit under content nobody is looking at.
        let device = match stop {
            Some(Stop::Device(row)) => row.checked_sub(self.rail_offset),
            _ => None,
        };
        match sweep.ctx {
            Some(ctx) => {
                let rail = Self::rail_rect(&ctx.frame);
                self.rail
                    .set_current(device, rail, ctx.scale, ctx.theme, sweep.damage);
            }
            None => self.rail.adopt_current(device),
        }
    }
}
