//! The System section: the machine itself, seen through eight pages
//! (`plans/NEW-SWITCHBOARD.md` S3/S4).
//!
//! A sidebar of pages on the left, four header readings across the top, the
//! selected page's body in the middle, and one action rail on the right —
//! the section commands a single subject, the machine, so its actions
//! belong to the screen rather than to any row.
//!
//! Nothing here invents a figure. A reading the service could not take is
//! drawn as an explicit unmeasured mark followed by the reason, so "not
//! permitted", "unavailable" and "no interface" reach the reader as the
//! three different statements they are.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::{SignalRole, Theme};

use tairix_controls::{
    ActionRail, Button, ButtonContent, Chart, Fact, FactList, MeterValue, MetricInstrument,
    MetricLayout, MetricTile, Panel, PressureKind, PressureState, ProgressValue, RailAction, Tab,
    Tabs, TabsAction, TabsOrientation,
};

use super::frame::{SectionAnatomy, SectionFrame, ACTION_RAIL_WIDTH};
use super::refresh::restate_rail;
use super::system_data::{
    absence_statement, reading_text, HeadlineTile, HealthSeverity, LimitRow, NetworkInterface,
    Reading, SessionSeat, StorageVolume, SystemAction, SystemFact, SystemPage, SystemReport,
    TileInstrument, Unmeasured,
};
use super::{
    ActionVerdict, FocusSweep, ListInfo, SectionCtx, SectionOutcome, SectionView,
    SwitchboardAction, SwitchboardModel,
};

/// The rail's title. The rail control carries no caption of its own, so the
/// section seats it in a [`Panel`], which already defines what a titled
/// container looks like.
const RAIL_TITLE: &str = "SYSTEM ACTIONS";

/// The sidebar's logical width: wide enough for the longest page name at
/// the reference density, and narrow enough that the sidebar, the rail and
/// a usable page body all still seat in the smallest window the panel
/// allows.
const SIDEBAR_WIDTH: u32 = 116;

/// The header band's logical height: one row of four reading tiles.
const HEADER_HEIGHT: u32 = 76;

/// One line of a page's body.
///
/// Every page compiles down to this one ordered vocabulary, so the section
/// has a single layout, a single scroll range and a single paint loop
/// rather than eight of each.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PageLine {
    /// A group heading introducing the lines under it.
    Heading(String),
    /// A labelled reading.
    Fact(Fact),
    /// A statement that something is not shown, and why.
    Absence(String),
}

/// A statement that something is absent, as a page line.
///
/// The sentence itself is the product-wide one, so this page's Services
/// block and Recovery's Logs page cannot drift into two wordings for the
/// same refusal; only the wrapping into a page line is System's own.
pub(super) fn absence_line(subject: &str, reason: Unmeasured) -> PageLine {
    PageLine::Absence(absence_statement(subject, reason))
}

/// A labelled reading as a page line, toned so an absent value is visibly
/// not a measurement.
fn fact_line(label: &str, value: &Reading) -> PageLine {
    let fact = Fact::new(label, reading_text(value));
    PageLine::Fact(match value.absence() {
        Some(Unmeasured::NotPermitted) => fact.with_tone(SignalRole::Denied),
        Some(Unmeasured::Unavailable | Unmeasured::NoInterface) => {
            fact.with_tone(SignalRole::Warning)
        }
        None => fact,
    })
}

/// A fact whose value is already plain text, for the parts of a page that
/// state a name rather than a measurement.
fn text_line(label: &str, value: impl Into<String>) -> PageLine {
    PageLine::Fact(Fact::new(label, value))
}

/// The System section: its page sidebar, its header readings, the selected
/// page's compiled body, and the machine's action rail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SystemSection {
    /// The page selector down the left-hand side.
    pub(super) sidebar: Tabs,
    /// Which page is showing.
    pub(super) page: SystemPage,
    /// The four header reading tiles, in the report's fixed order.
    pub(super) tiles: Vec<MetricTile>,
    /// The selected page's body, one line per row.
    pub(super) lines: Vec<PageLine>,
    /// The machine's actions.
    pub(super) rail: ActionRail,
    /// The plate the rail is seated in, which carries its caption.
    pub(super) rail_panel: Panel,
    /// The report every page is compiled from, kept so a page switch needs
    /// no fresh sample.
    pub(super) report: SystemReport,
    /// Where the content cursor is: a sidebar page, then a rail action.
    pub(super) focus: usize,
}

impl SystemSection {
    /// An empty System section: no readings, the Overview page selected.
    pub(super) fn new() -> Self {
        let mut section = Self {
            sidebar: page_tabs(SystemPage::Overview),
            page: SystemPage::Overview,
            tiles: Vec::new(),
            lines: Vec::new(),
            rail: ActionRail::new(Vec::new()),
            rail_panel: Panel::new(RAIL_TITLE),
            report: SystemReport::default(),
            focus: 0,
        };
        section.compile();
        section
    }

    /// How many rail actions the cursor can reach after the pages.
    fn rail_len(&self) -> usize {
        self.report.actions.len()
    }

    /// Show `page` and recompile the body behind it.
    fn select_page(&mut self, page: SystemPage) {
        self.page = page;
        self.sidebar.set_selected(page.index());
        self.sidebar.set_current(Some(page.index()));
        self.compile();
    }

    /// Rebuild the header tiles and the selected page's lines from the
    /// report the service last handed over.
    fn compile(&mut self) {
        self.tiles = self.report.headline.iter().map(build_tile).collect();
        self.lines = compile_page(self.page, &self.report);
        let commands = self.report.actions.iter().map(build_action).collect();
        restate_rail(&mut self.rail, commands);
    }

    /// The rectangle the page body's lines are laid down.
    fn body_rect(frame: &SectionFrame) -> Rect {
        frame.primary
    }

    /// A reading as the text a header tile shows: the measurement, or the
    /// unmeasured mark followed by why there is none.
    pub(super) fn tile_text(value: &Reading) -> String {
        reading_text(value)
    }

    /// The pressure emphasis a reading carries: under only where the
    /// service's own latch says so, never inferred from the figure.
    pub(super) fn tile_pressure(tile: &HeadlineTile) -> PressureState {
        if tile.pressured {
            PressureState::Under(tile.kind)
        } else {
            PressureState::None
        }
    }

    /// A tile's instrument: a chart over its history, or a bar at its
    /// fraction.
    ///
    /// An absent fraction becomes an unmeasured meter rather than a bar at
    /// nought, which would read as "idle" when the truth is "unknown"; an
    /// empty history plots nothing at all for the same reason.
    pub(super) fn tile_instrument(
        instrument: &TileInstrument,
        kind: PressureKind,
    ) -> MetricInstrument {
        match instrument {
            TileInstrument::Trend(history) => {
                MetricInstrument::Trend(Chart::new(kind).with_samples(history.iter().copied()))
            }
            TileInstrument::Track(Some(permille)) => {
                MetricInstrument::Track(MeterValue::Measured(ProgressValue::new(*permille)))
            }
            TileInstrument::Track(None) => MetricInstrument::Track(MeterValue::Unmeasured),
        }
    }
}

/// The sidebar's tabs, with `page` selected.
fn page_tabs(page: SystemPage) -> Tabs {
    let mut tabs = Tabs::new(
        SystemPage::ALL
            .iter()
            .map(|entry| Tab::new(entry.title()))
            .collect(),
    )
    .with_orientation(TabsOrientation::Vertical);
    tabs.set_selected(page.index());
    tabs
}

/// One header reading as a [`MetricTile`], unplated because the header
/// band is the plate.
fn build_tile(tile: &HeadlineTile) -> MetricTile {
    let mut built = MetricTile::new(
        tile.name.clone(),
        SystemSection::tile_text(&tile.value),
        tile.kind,
    )
    .with_detail(SystemSection::tile_text(&tile.detail))
    .with_layout(MetricLayout::Stacked)
    .with_pressure(SystemSection::tile_pressure(tile))
    .with_instrument(SystemSection::tile_instrument(&tile.instrument, tile.kind))
    .unplated();
    if !tile.unit.is_empty() {
        built = built.with_unit(tile.unit.clone());
    }
    built
}

/// One rail action as a [`Button`], refused visibly when the caller cannot
/// take it so the reader learns before attempting it.
///
/// The refusal's own reason picks the mark: a withheld capability earns the
/// Authority Mark, because acquiring the authority would make the action
/// available, while an action with no endpoint behind it at all is plainly
/// disabled. Showing the Authority Mark for a missing interface would tell
/// a reader to go and ask for a grant that would change nothing.
fn build_action(action: &SystemAction) -> Button {
    let mut button = Button::new(ButtonContent::Label(action.label.clone()), action.role);
    button.set_state(action_verdict(action).to_state());
    button
}

/// The verdict a rail action renders and fails closed as.
pub(super) fn action_verdict(action: &SystemAction) -> ActionVerdict {
    match action.refusal {
        None if action.allowed => ActionVerdict::Ready,
        Some(Unmeasured::NotPermitted) => ActionVerdict::DeniedByAuthority,
        _ => ActionVerdict::DisabledByState,
    }
}

/// Compile `page`'s body from `report`.
///
/// One function owns every page's layout, so a page cannot grow a second
/// definition of itself somewhere else in the file.
fn compile_page(page: SystemPage, report: &SystemReport) -> Vec<PageLine> {
    match page {
        SystemPage::Overview => overview_page(report),
        SystemPage::Resources => resources_page(report),
        SystemPage::Storage => storage_page(report),
        SystemPage::Network => network_page(report),
        SystemPage::Session => session_page(report),
        SystemPage::Permissions => permissions_page(report),
        SystemPage::Services => alloc::vec![
            PageLine::Heading(String::from("Active Services")),
            absence_line("no service registry to enumerate", Unmeasured::NoInterface),
        ],
        SystemPage::Power => alloc::vec![
            PageLine::Heading(String::from("Power")),
            absence_line(
                "no power state to read and no power interface to drive",
                Unmeasured::NoInterface,
            ),
        ],
    }
}

/// The Overview page: the machine's own facts, the services it is running,
/// and what this session is permitted to see.
fn overview_page(report: &SystemReport) -> Vec<PageLine> {
    let mut lines = alloc::vec![PageLine::Heading(String::from("Machine"))];
    lines.extend(report.machine.iter().map(fact_of));
    lines.push(PageLine::Heading(String::from("Active Services")));
    lines.push(absence_line(
        "no service registry to enumerate",
        Unmeasured::NoInterface,
    ));
    lines.push(PageLine::Heading(String::from("Permissions")));
    lines.extend(report.authority.iter().map(fact_of));
    lines
}

/// The Resources page: per-core load, the memory and kernel-heap detail,
/// then what the desktop's last frame cost.
fn resources_page(report: &SystemReport) -> Vec<PageLine> {
    let mut lines = alloc::vec![PageLine::Heading(String::from("Processors"))];
    lines.extend(report.cores.iter().map(fact_of));
    lines.push(PageLine::Heading(String::from("Memory")));
    lines.extend(report.memory.iter().map(fact_of));
    lines.push(PageLine::Heading(String::from("Desktop")));
    lines.extend(report.compositor.iter().map(fact_of));
    lines
}

/// The Storage page: one block per mounted volume, each stating where it
/// came from, what it is, how full it is, and how healthy.
fn storage_page(report: &SystemReport) -> Vec<PageLine> {
    if let Some(reason) = report.volumes_absent {
        return alloc::vec![
            PageLine::Heading(String::from("Volumes")),
            absence_line("the mount table", reason),
        ];
    }
    if report.volumes.is_empty() {
        return alloc::vec![
            PageLine::Heading(String::from("Volumes")),
            PageLine::Absence(String::from("No volumes are mounted.")),
        ];
    }
    let mut lines = Vec::new();
    for volume in &report.volumes {
        lines.extend(volume_lines(volume));
    }
    lines
}

/// One mounted volume's block.
fn volume_lines(volume: &StorageVolume) -> Vec<PageLine> {
    alloc::vec![
        PageLine::Heading(volume.mount_point.clone()),
        text_line("Source", volume.source.clone()),
        text_line("Filesystem", volume.filesystem.clone()),
        text_line("Medium", volume.medium.clone()),
        text_line("Availability", volume.availability.clone()),
        fact_line("Capacity", &volume.capacity),
        health_line(volume),
    ]
}

/// A volume's health line, toned by the severity its availability implies
/// so a failing disk stands out from a healthy one.
fn health_line(volume: &StorageVolume) -> PageLine {
    let fact = Fact::new("Health", reading_text(&volume.health));
    PageLine::Fact(match tone_for(volume) {
        Some(tone) => fact.with_tone(tone),
        None => fact,
    })
}

/// The tone a volume's health reading is drawn in: a failing volume is a
/// recovery matter, a degraded one a caution, an unmeasured one a caution
/// too, and a healthy one takes the ordinary colour.
fn tone_for(volume: &StorageVolume) -> Option<SignalRole> {
    match volume.health_state {
        HealthSeverity::Failing => Some(SignalRole::Recovery),
        HealthSeverity::Degraded => Some(SignalRole::Warning),
        HealthSeverity::Healthy => volume.health.absence().map(|_| SignalRole::Warning),
    }
}

/// The Network page: one block per interface.
fn network_page(report: &SystemReport) -> Vec<PageLine> {
    if let Some(reason) = report.interfaces_absent {
        return alloc::vec![
            PageLine::Heading(String::from("Interfaces")),
            absence_line("the interface inventory", reason),
        ];
    }
    if report.interfaces.is_empty() {
        return alloc::vec![
            PageLine::Heading(String::from("Interfaces")),
            PageLine::Absence(String::from("No network interfaces are present.")),
        ];
    }
    let mut lines = Vec::new();
    for interface in &report.interfaces {
        lines.extend(interface_lines(interface));
    }
    lines
}

/// One interface's block: its facts, its link, its addresses and its
/// throughput.
fn interface_lines(interface: &NetworkInterface) -> Vec<PageLine> {
    let mut lines = alloc::vec![PageLine::Heading(interface.name.clone())];
    lines.extend(interface.facts.iter().map(fact_of));
    lines.push(fact_line("Link", &interface.link));
    match interface.addresses_absent {
        Some(reason) => lines.push(absence_line("its addresses", reason)),
        None if interface.addresses.is_empty() => lines.push(PageLine::Absence(String::from(
            "No address is configured on this interface.",
        ))),
        None => lines.extend(
            interface
                .addresses
                .iter()
                .map(|address| text_line("Address", address.clone())),
        ),
    }
    lines.push(fact_line("Receiving", &interface.rx));
    lines.push(fact_line("Transmitting", &interface.tx));
    lines
}

/// The Session page: the machine's seats, then the census the load reading
/// carries.
fn session_page(report: &SystemReport) -> Vec<PageLine> {
    let mut lines = alloc::vec![PageLine::Heading(String::from("Seats"))];
    match report.seats_absent {
        Some(reason) => lines.push(absence_line("the seat list", reason)),
        None if report.seats.is_empty() => lines.push(PageLine::Absence(String::from(
            "No seats are configured at this machine.",
        ))),
        None => {
            for seat in &report.seats {
                lines.extend(seat_lines(seat));
            }
        }
    }
    lines.push(PageLine::Heading(String::from("Logged in")));
    lines.extend(report.census.iter().map(fact_of));
    lines
}

/// One seat's lines.
fn seat_lines(seat: &SessionSeat) -> Vec<PageLine> {
    alloc::vec![
        PageLine::Heading(seat.name.clone()),
        fact_line("Owner", &seat.owner),
        fact_line("Foreground", &seat.console),
    ]
}

/// The Permissions page: what the service can attest about this session's
/// authority, and the limits it runs under.
fn permissions_page(report: &SystemReport) -> Vec<PageLine> {
    let mut lines = alloc::vec![PageLine::Heading(String::from("Authority"))];
    lines.extend(report.authority.iter().map(fact_of));
    lines.push(PageLine::Heading(String::from("Resource limits")));
    match report.limits_absent {
        Some(reason) => lines.push(absence_line("the limit report", reason)),
        None if report.limits.is_empty() => lines.push(PageLine::Absence(String::from(
            "No resource limits are in force.",
        ))),
        None => {
            for limit in &report.limits {
                lines.extend(limit_lines(limit));
            }
        }
    }
    lines
}

/// One limit's lines: the bounds and the usage measured against them.
fn limit_lines(limit: &LimitRow) -> Vec<PageLine> {
    alloc::vec![
        PageLine::Heading(limit.name.clone()),
        text_line("Soft bound", limit.soft.clone()),
        text_line("Hard bound", limit.hard.clone()),
        fact_line("In use", &limit.usage),
    ]
}

/// A report fact as a page line.
fn fact_of(fact: &SystemFact) -> PageLine {
    fact_line(&fact.label, &fact.value)
}

impl SectionView for SystemSection {
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: None,
            sidebar_width: SIDEBAR_WIDTH,
            header_height: HEADER_HEIGHT,
            detail_width: 0,
            impact_width: 0,
            rail_width: ACTION_RAIL_WIDTH,
            footer_height: 0,
            primary_row_commands: 0,
        }
    }

    fn adopt(&mut self, model: &SwitchboardModel) {
        self.report = model.system.clone();
        self.compile();
        self.focus = self.focus.min(self.focus_span().saturating_sub(1));
    }

    fn item_count(&self) -> usize {
        self.lines.len()
    }

    /// The cursor walks the sidebar's pages and then the rail's actions.
    /// The page body carries no cursor of its own: it is a readout, and
    /// every control on this screen is either a page or an action.
    fn focus_span(&self) -> usize {
        SystemPage::ALL.len().saturating_add(self.rail_len())
    }

    /// No cursor stop is a scrollable row, so the reader's scroll position
    /// is left where they put it when the cursor moves.
    fn focus_row(&self, _index: usize) -> Option<usize> {
        None
    }

    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo {
        ListInfo::rows(Self::body_rect(frame), self.lines.len(), scale, theme)
    }

    fn row_buttons(&self) -> u32 {
        0
    }

    fn focused_action_count(&self) -> usize {
        1
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
    }

    fn row_action(&self) -> usize {
        0
    }

    fn set_row_action(&mut self, _index: usize, _sweep: &mut FocusSweep<'_, '_>) {}

    /// Commit the focused stop: a sidebar stop shows its page, a rail stop
    /// reports its action for the service to authorise.
    ///
    /// A rail stop hands the key to the rail, which forwards it to the
    /// focused button so the button decides for itself whether it may fire.
    /// A disabled command, or one whose Authority Mark denies the caller,
    /// therefore refuses the keyboard exactly as it refuses the pointer;
    /// committing on the cursor's position alone would let the keyboard
    /// dispatch a command the screen is showing as refused.
    fn activate_focused(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if !matches!(key, Key::Named(NamedKey::Enter) | Key::Char(' ')) {
            return None;
        }
        if let Some(page) = SystemPage::from_index(self.focus) {
            self.select_page(page);
            return None;
        }
        let index = self.focus.checked_sub(SystemPage::ALL.len())?;
        if index >= self.rail_len() {
            return None;
        }
        let rail = self
            .rail_content(&ctx.frame, ctx.scale, ctx.theme)
            .unwrap_or(Rect::EMPTY);
        self.rail.set_focus(Some(index), rail, damage);
        self.rail
            .on_key(key, rail, damage)
            .map(|RailAction::Activate { index }| {
                SectionOutcome::Action(SwitchboardAction::System { index })
            })
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        if let Some(sidebar) = ctx.frame.sidebar {
            self.sidebar.render(surface, sidebar, ctx.scale, ctx.theme);
        }
        self.render_header(surface, ctx);
        self.render_body(surface, ctx);
        self.render_rail(surface, ctx);
    }

    fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if let Some(sidebar) = ctx.frame.sidebar {
            if let Some(TabsAction::Selected { index }) =
                self.sidebar.on_pointer(event, sidebar, damage)
            {
                if let Some(page) = SystemPage::from_index(index) {
                    self.select_page(page);
                    self.focus = index;
                }
                return None;
            }
        }
        let rail = self.rail_content(&ctx.frame, ctx.scale, ctx.theme)?;
        self.rail
            .on_pointer(event, rail, ctx.scale, ctx.theme, damage)
            .map(|RailAction::Activate { index }| {
                SectionOutcome::Action(SwitchboardAction::System { index })
            })
    }

    fn apply_focus_marks(&mut self, focused: bool, sweep: &mut FocusSweep<'_, '_>) {
        let page = focused
            .then_some(self.focus)
            .filter(|f| *f < SystemPage::ALL.len());
        self.sidebar.set_current(page.or(Some(self.page.index())));
        let action = focused
            .then(|| self.focus.checked_sub(SystemPage::ALL.len()))
            .flatten()
            .filter(|index| *index < self.rail_len());
        let rail = sweep
            .ctx
            .and_then(|ctx| self.rail_content(&ctx.frame, ctx.scale, ctx.theme));
        sweep.rail(&mut self.rail, action, rail);
        for (index, button) in self.rail.items_mut().iter_mut().enumerate() {
            button.set_focused(action == Some(index));
        }
    }
}

impl SystemSection {
    /// Paint the four header readings across the header band.
    fn render_header(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let band = ctx.frame.header;
        if self.tiles.is_empty() || band.width == 0 || band.height == 0 {
            return;
        }
        let gap = ctx.scale.scale_length(ctx.theme.metrics().control_gap);
        let count = u32::try_from(self.tiles.len()).unwrap_or(1).max(1);
        let spread = gap.saturating_mul(count.saturating_sub(1));
        let each = band.width.saturating_sub(spread) / count;
        for (index, tile) in self.tiles.iter().enumerate() {
            let offset = u32::try_from(index)
                .unwrap_or(0)
                .saturating_mul(each.saturating_add(gap));
            let rect = Rect::new(
                band.left().saturating_add(to_i32(offset)),
                band.top(),
                each,
                band.height,
            );
            tile.render(surface, rect, ctx.scale, ctx.theme);
        }
    }

    /// Paint the visible slice of the selected page's body.
    ///
    /// Facts are drawn through the shared record list, so a page's rows
    /// line up with every other fact list in the product; a heading and an
    /// absence statement are single lines of their own.
    fn render_body(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        let palette = ctx.theme.palette();
        let mut slot = 0u32;
        let visible = info.visible();
        let mut index = ctx.start;
        while slot < visible {
            let Some(line) = self.lines.get(index) else {
                break;
            };
            let rect = info.item_rect(slot);
            match line {
                PageLine::Heading(text) => {
                    ctx.font.draw_text(
                        surface,
                        rect.left(),
                        rect.top(),
                        text,
                        Color::from(palette.accent),
                    );
                }
                PageLine::Fact(fact) => {
                    FactList::new(alloc::vec![fact.clone()])
                        .render(surface, rect, ctx.scale, ctx.theme);
                }
                PageLine::Absence(text) => {
                    ctx.font.draw_text(
                        surface,
                        rect.left(),
                        rect.top(),
                        text,
                        Color::from(palette.on_surface_muted),
                    );
                }
            }
            slot = slot.saturating_add(1);
            index = index.saturating_add(1);
        }
    }

    /// Paint the action rail inside its titled plate.
    fn render_rail(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(rail) = ctx.frame.rail else {
            return;
        };
        self.rail_panel.render(surface, rail, ctx.scale, ctx.theme);
        if let Some(content) = self.rail_panel.content_rect(rail, ctx.scale, ctx.theme) {
            self.rail.render(surface, content, ctx.scale, ctx.theme);
        }
    }

    /// The rail's own content rectangle inside its plate, or [`None`] when
    /// the frame dropped the rail under width pressure.
    fn rail_content(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.rail_panel.content_rect(frame.rail?, scale, theme)
    }
}

#[cfg(test)]
#[path = "system_tests.rs"]
mod tests;
