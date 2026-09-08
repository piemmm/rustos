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
use core::mem;

use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_icon::IconArtwork;
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::{ActionRail, Button, ButtonContent, RailAction, StatusPill};

use super::frame::{SectionAnatomy, SectionFrame, ACTION_RAIL_WIDTH};
use super::refresh::restate_rail;
use super::{
    resolve_selection, ListInfo, SectionCtx, SectionOutcome, SectionView, Sweep, Switchboard,
    SwitchboardAction, SwitchboardModel,
};

mod device;
mod pane;

pub use device::{
    DeviceAction, DeviceId, PressureBanner, RailGroup, ResourceControl, ResourceDevice,
    ResourceReport, StorageId, TaskCostColumn,
};
pub use pane::{
    BlockBody, BlockSpan, CompositionPart, ConsumerRow, CoreCell, HeroInstrument, PaneBlock,
    PaneHero,
};

pub(super) use pane::PaneItem;

/// The pressure banner's relief-command width, wide enough at the reference
/// density for the longest relief a banner offers.
const RELIEF_BUTTON_WIDTH: u32 = 132;

/// The action rail's caption. The rail control carries no caption of its own,
/// so the section seats it in the surface's shared titled block.
const RAIL_TITLE: &str = "DEVICE ACTIONS";

/// Which of the section's cursor stops the keyboard is on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Stop {
    /// The pressure banner's relief command.
    Relief,
    /// A command in the trailing action rail, by its slot.
    Rail(usize),
}

/// What one [`ResourcesSection::rebuild`] changed, so its caller reports
/// exactly the regions the screen now owes.
struct Rebuilt {
    /// The selected device's commands moved: the action column owes one.
    rail_column: bool,
    /// The pane owes a repaint whole, rather than item by item.
    pane: bool,
    /// The flow the rebuild replaced, for the item-by-item comparison.
    retired: Vec<PaneItem>,
}

/// The Resources section: the report it draws, the device rail, the selected
/// device's compiled pane, and that device's commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ResourcesSection {
    /// The report every pane is drawn from, kept so a device switch needs no
    /// fresh sample.
    pub(super) report: ResourceReport,
    /// The session's own account root, which resolves the icon of a consumer
    /// loaded from this user's own program store.
    pub(super) home: Option<alloc::string::String>,
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
    /// The banner's relief command, when the selected device wears a banner.
    pub(super) relief: Option<Button>,
    /// Where the content cursor is.
    pub(super) focus: usize,
    /// Which of the focused stop's actions the keyboard is on.
    pub(super) action: usize,
}

impl ResourcesSection {
    /// An empty section: no devices, nothing selected.
    pub(super) fn new() -> Self {
        let mut section = Self {
            home: None,
            report: ResourceReport::default(),
            selected: None,
            items: Vec::new(),
            compiled_for: (0, Scale::ONE),
            actions: ActionRail::new(Vec::new()),
            relief: None,
            focus: 0,
            action: 0,
        };
        let _ = section.rebuild();
        section
    }

    /// The selected device, or [`None`] when the report holds none.
    pub(super) fn device(&self) -> Option<&ResourceDevice> {
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
    fn rebuild(&mut self) -> Rebuilt {
        let commands = self
            .device()
            .map(|device| device.actions.iter().map(build_command).collect())
            .unwrap_or_default();
        let mut rail_column = restate_rail(&mut self.actions, commands);
        let relief = self.device().and_then(|device| {
            device
                .banner
                .as_ref()
                .and_then(|banner| banner.relief.as_ref())
                .map(build_command)
        });
        // The relief command is drawn inside the banner at the head of the
        // pane, so the pane is what owes it.
        let mut pane_moved = relief != self.relief;
        self.relief = relief;
        // The flow is recompiled for the width it will be drawn at, which
        // `relayout` supplies; until then it is compiled for the width it
        // last had, so the scroll range always describes the flow on screen.
        let (width, scale) = self.compiled_for;
        let retired = mem::take(&mut self.items);
        self.compile(width, scale);
        // A flow of a different length has moved every item below the change,
        // and the commands beside a pane the reader is now on describe that
        // device instead.
        if retired.len() != self.items.len() {
            pane_moved = true;
            rail_column = true;
        }
        Rebuilt {
            rail_column,
            pane: pane_moved,
            retired,
        }
    }

    /// What one [`rebuild`](Self::rebuild) changed, so the caller can report
    /// exactly those regions.
    ///
    /// `retired` is the flow the rebuild replaced, kept so an item-by-item
    /// comparison costs the pane no clone of its own.
    fn report_refresh(&self, rebuilt: &Rebuilt, sweep: &mut Sweep<'_, '_>) {
        let Some(ctx) = sweep.ctx() else {
            return;
        };
        if rebuilt.rail_column {
            if let Some(rail) = ctx.frame.rail {
                sweep.report(rail);
            }
        }
        let primary = Self::pane_rect(&ctx.frame);
        if rebuilt.pane {
            sweep.report(primary);
            return;
        }
        let pitch = pane::pitch(ctx.scale, ctx.theme);
        let pad = crate::view::block::content_inset(ctx.scale, ctx.theme);
        let start = u32::try_from(ctx.start).unwrap_or(u32::MAX);
        for (was, now) in rebuilt.retired.iter().zip(&self.items) {
            if was == now {
                continue;
            }
            if let Some(rect) = pane::item_rect(now, primary, start, pitch, pad) {
                sweep.report(rect);
            }
        }
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
        let mut stops: Vec<Stop> = Vec::new();
        if self.relief.is_some() {
            stops.push(Stop::Relief);
        }
        stops.extend((0..self.actions.len()).map(Stop::Rail));
        stops
    }

    /// Select the device `id` names, wherever it sits in the report.
    ///
    /// The rail addresses a device by its own identity rather than by a row,
    /// so a sample that reorders the report cannot land the reader on a
    /// different device than the one they chose. A device the report no
    /// longer names changes nothing (fail closed).
    pub(super) fn select_device(&mut self, id: DeviceId, sweep: &mut Sweep<'_, '_>) {
        let Some(index) = self.report.devices.iter().position(|d| d.id == id) else {
            return;
        };
        self.select(index, sweep);
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
    fn select(&mut self, index: usize, sweep: &mut Sweep<'_, '_>) {
        let Some(device) = self.report.devices.get(index) else {
            return;
        };
        self.selected = Some(device.id);
        let rebuilt = self.rebuild();
        // The pane, its commands and the rail's own marks all describe the
        // device that is selected, so switching device owes every one of them
        // — a selection that reported only the strip's own lift would leave
        // the reader reading the previous device's pane.
        self.report_refresh(&rebuilt, sweep);
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
        let button_w = scale.scale_length(RELIEF_BUTTON_WIDTH).min(band.width);
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
        let gap = ctx
            .scale
            .scale_length(ctx.theme.metrics().control_gap)
            .max(1);
        let text_left = band.left() + to_i32(pill_w.saturating_add(gap));
        // Truncated to the room between the pill and the relief command:
        // text drawn past the band would land in the gap beside the pane and
        // over the action column, which no repaint of the pane can ever
        // clean up.
        let limit = match self.relief.as_ref() {
            Some(_) => button.left().saturating_sub(to_i32(gap)),
            None => band.right(),
        };
        let avail = u32::try_from(limit.saturating_sub(text_left)).unwrap_or(0);
        ctx.font.draw_text(
            surface,
            text_left,
            band.top(),
            ctx.font.truncate_to_width(&banner.summary, avail),
            Color::from(palette.on_surface),
        );
        ctx.font.draw_text(
            surface,
            text_left,
            band.top() + to_i32(ctx.font.line_height()),
            ctx.font.truncate_to_width(&banner.detail, avail),
            Color::from(palette.on_surface_muted),
        );
        if let Some(relief) = self.relief.as_ref() {
            relief.render(surface, button, ctx.scale, ctx.theme);
        }
    }
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
            sidebar_width: 0,
            header_height: 0,
            detail_width: 0,
            impact_width: 0,
            rail_width: ACTION_RAIL_WIDTH,
            footer_height: 0,
            primary_row_commands: 0,
        }
    }

    fn adopt(&mut self, model: &SwitchboardModel, sweep: &mut Sweep<'_, '_>) {
        let previous = self.selected;
        let stop = self.stop_at(self.focus);
        self.report.clone_from(&model.resources);
        self.home.clone_from(&model.home);
        self.selected =
            resolve_selection(previous, self.report.devices.iter().map(|device| device.id));
        let rebuilt = self.rebuild();
        self.report_refresh(&rebuilt, sweep);
        // The cursor is put back on the same *kind* of stop, so a device
        // cursor follows the device it was on rather than staying on a
        // number that now names a different one.
        self.focus = match stop {
            None => 0,
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

    fn set_content_focus(&mut self, index: usize, _sweep: &mut Sweep<'_, '_>) {
        self.focus = index;
    }

    fn row_action(&self) -> usize {
        self.action
    }

    fn set_row_action(&mut self, index: usize, _sweep: &mut Sweep<'_, '_>) {
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

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>, artwork: &mut dyn IconArtwork) {
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
            pane::PaneWindow {
                primary: pane,
                start,
                scale: ctx.scale,
                theme: ctx.theme,
                font: ctx.font,
                home: self.home.as_deref(),
            },
            artwork,
        );
        if let Some(rect) = ctx.frame.rail {
            if let Some(inner) = crate::view::block::plate(surface, rect, ctx.scale, ctx.theme) {
                crate::view::block::title(surface, inner, ctx.scale, ctx.theme, RAIL_TITLE);
            }
            if let Some(content) = crate::view::block::titled_content(rect, ctx.scale, ctx.theme) {
                self.actions.render(surface, content, ctx.scale, ctx.theme);
            }
        }
    }

    /// Route a pointer event to the rail, the banner's relief, or the
    /// commands.
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if let Some((_, button)) = self.banner_layout(&ctx.frame, ctx.scale, ctx.theme) {
            if let Some(relief) = self.relief.as_mut() {
                if relief.on_pointer(event, button, damage).is_some() {
                    return self.command_outcome(ResourceControl::Relieve);
                }
            }
        }
        let rect = ctx.frame.rail?;
        let content = crate::view::block::titled_content(rect, ctx.scale, ctx.theme)?;
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

    fn apply_focus_marks(&mut self, focused: bool, sweep: &mut Sweep<'_, '_>) {
        let stop = focused.then(|| self.stop_at(self.focus)).flatten();
        let slot = match stop {
            Some(Stop::Rail(slot)) => Some(slot),
            _ => None,
        };
        let rect = sweep.ctx.and_then(|ctx| {
            ctx.frame
                .rail
                .and_then(|rect| crate::view::block::titled_content(rect, ctx.scale, ctx.theme))
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
    }
}
