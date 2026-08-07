//! The Activities section: the grouped tasks that move, pause and close
//! together (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's activity view model ([`ActivitySummary`] and its
//! [`ActivityMember`]s), the [`ActivityControl`] vocabulary a row offers, the
//! inline rename [`TextField`], and the section's layout, painting and input.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, AuthorityState, Button, ButtonAction, ButtonContent, ControlRole, ControlState,
    Fact, FactList, ListRow, Panel, RowAction, TextAction, TextField,
};

use super::frame::{SectionAnatomy, SectionFrame, DETAIL_PANE_WIDTH};
use super::system_data::{reading_text, selection_prompt, Reading, Unmeasured};
use super::{
    resolve_selection, ActionVerdict, ListInfo, SectionCtx, SectionOutcome, SectionView,
    Switchboard, SwitchboardAction, SwitchboardModel,
};

/// An action a Switchboard activity header row can request (spec T12).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActivityControl {
    /// Switch to the activity.
    Switch,
    /// Pause every member of the activity.
    Pause,
    /// Resume every member of the activity.
    Resume,
    /// Close the activity and every member.
    Close,
}

/// One task grouped into an [`ActivitySummary`] (spec T12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityMember {
    /// The member's display name.
    pub name: String,
    /// A short trailing detail (e.g. owner, CPU%).
    pub detail: String,
    /// The member's live activity, drawn as its own Heat Seam.
    pub activity: ActivityState,
    /// Whether this sample found a task answering to the member.
    ///
    /// A group remembers the tasks put in it, so a member can outlive the
    /// task it named. Such a member has no readings at all and contributes
    /// nothing to the group's totals, and the detail pane says so rather
    /// than showing it as an idle task with nothing to report.
    pub joined: bool,
}

/// One activity: a named group of tasks that move, pause, and close together
/// (spec T12).
///
/// Rendered as a header [`ListRow`] plus one [`ListRow`] per
/// [`member`](Self::members), indented beneath it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivitySummary {
    /// A stable identity for this activity, independent of its position in
    /// the list, so an in-flight rename can survive a refresh that reorders
    /// or shortens [`SwitchboardModel::activities`](super::SwitchboardModel::activities).
    pub id: u64,
    /// The activity's display name.
    pub name: String,
    /// A short trailing detail (e.g. member count).
    pub detail: String,
    /// The activity's combined live activity, drawn as the header's Heat
    /// Seam.
    pub activity: ActivityState,
    /// Whether every member is currently paused.
    pub paused: bool,
    /// Whether the caller may pause/resume/close this activity.
    pub can_control: bool,
    /// Whether another task may still be grouped into this activity.
    pub can_accept_member: bool,
    /// The group's combined CPU share, totalled from its joined members'
    /// own measured shares, or why there is none.
    pub cpu: Reading,
    /// The group's combined resident memory, on the same terms as
    /// [`cpu`](Self::cpu).
    pub memory: Reading,
    /// The group's combined storage throughput, on the same terms as
    /// [`cpu`](Self::cpu).
    pub disk: Reading,
    /// The group's combined network throughput.
    ///
    /// There is no per-process network accounting anywhere in the System
    /// Information API, so there is nothing to total and this is always
    /// absent; it is carried rather than omitted because a reader comparing
    /// four resources must be told the fourth is unmeasured, not left to
    /// assume the group uses no network.
    pub network: Reading,
    /// The activity's member tasks.
    pub members: Vec<ActivityMember>,
}

/// One activity rendered as a header [`ListRow`] plus its Switch/Pause-or-
/// Resume/Rename/Close [`Button`]s, and one [`ListRow`] per member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActivityEntry {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) detail: String,
    pub(super) activity: ActivityState,
    pub(super) header: ListRow,
    pub(super) switch: Button,
    pub(super) pause_resume: Button,
    pub(super) rename: Button,
    pub(super) close: Button,
    pub(super) paused: bool,
    pub(super) can_control: bool,
    pub(super) can_accept_member: bool,
    pub(super) members: Vec<ListRow>,
}

/// Which row a flattened Activities-section list index names: an activity's
/// own header row, or one of its member rows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum ActivityRow {
    /// The header row of the activity at this index.
    Header(usize),
    /// A member row: the owning activity's index, then the member's index
    /// within it.
    Member(usize, usize),
}

/// An in-flight inline rename of an activity's header row.
///
/// `id` is the activity's stable identity (spec T12): a model refresh that
/// still has an activity with this `id` relocates `index` to match, so typing
/// survives a refresh unless the activity itself is gone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RenameEdit {
    pub(super) id: u64,
    pub(super) index: usize,
    pub(super) field: TextField,
}

/// The detail pane's caption with nothing selected.
const DETAIL_TITLE: &str = "ACTIVITY";

/// What the members list says for an activity that has none.
///
/// An activity with no members is an ordinary state — a group whose tasks
/// have all ended — not a missing reading, so it is stated plainly and
/// never wears the unmeasured mark.
const NO_MEMBERS: &str = "No members.";

/// What the detail pane says for a member no task answers to.
///
/// A group remembers the tasks put in it, so a member can outlive its task.
/// That is a fact the service knows rather than a reading it failed to
/// take, so it is stated plainly and does not wear the unmeasured mark —
/// which would imply a share exists that could not be read.
const MEMBER_NOT_RUNNING: &str = "Not running";

/// The rectangles the detail pane's parts occupy, resolved once so the
/// pane's parts cannot be laid out two different ways.
#[derive(Copy, Clone, Debug)]
struct DetailLayout {
    /// The activity's name and how many tasks it holds.
    identity: Rect,
    /// The group's four combined readings.
    totals: Rect,
    /// One line per member.
    members: Rect,
}

/// The Activities section: the activity headers with their member rows, the
/// selected activity's detail, the in-flight inline rename, and the
/// keyboard's place among the flattened rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActivitiesSection {
    /// The activities this sample reported, in model order, so the detail
    /// pane can state the selected group's members and combined readings.
    pub(super) items: Vec<ActivitySummary>,
    /// One header plus its members per activity, in the same order.
    pub(super) entries: Vec<ActivityEntry>,
    /// Which activity is selected, by its stable id rather than by its
    /// position, so a refresh that reorders or shortens the list keeps the
    /// reader on the group they chose and drops the selection only when
    /// that group has genuinely closed.
    pub(super) selected: Option<u64>,
    /// The in-flight inline rename, or `None` when nothing is being renamed.
    pub(super) rename: Option<RenameEdit>,
    /// The name the most recent rename committed, until the next sample.
    pub(super) submitted_name: Option<String>,
    /// Which flattened row the content cursor is on.
    pub(super) focus: usize,
    /// Which of the focused header's actions the cursor is on.
    pub(super) action: usize,
}

impl ActivitiesSection {
    /// The number of inline actions an activity header row carries: Switch,
    /// Pause-or-Resume, Rename, Close.
    const BUTTONS: u32 = 4;

    /// An empty Activities section: no groups, no rename, cursor at the top.
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            entries: Vec::new(),
            selected: None,
            rename: None,
            submitted_name: None,
            focus: 0,
            action: 0,
        }
    }

    /// The position of the selected activity in the current list, or [`None`]
    /// when nothing is selected (an empty list, or a selection whose group
    /// has closed).
    pub(super) fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.items.iter().position(|item| item.id == id)
    }

    /// The selected activity, or [`None`] when nothing is selected.
    pub(super) fn selected_item(&self) -> Option<&ActivitySummary> {
        self.items.get(self.selected_index()?)
    }

    /// Select the activity at `index`, if there is one, and mark its header.
    ///
    /// A member row selects the activity it belongs to: the pane describes
    /// whole groups, so the reader's place in the flattened list always
    /// resolves to one group rather than to nothing.
    fn select_activity(&mut self, index: usize) {
        if let Some(item) = self.items.get(index) {
            self.selected = Some(item.id);
            self.mark_selection();
        }
    }

    /// Put the selection mark on the selected activity's header row and take
    /// it off every other, so the row the detail pane describes is the one
    /// that looks chosen.
    fn mark_selection(&mut self) {
        let selected = self.selected;
        for entry in &mut self.entries {
            entry.header.set_selected(selected == Some(entry.id));
        }
    }

    /// The name committed by the most recent inline rename, until the next
    /// sample clears it.
    pub(super) fn submitted_name(&self) -> Option<&str> {
        self.submitted_name.as_deref()
    }

    /// Build an activity's header row + Switch/Pause-or-Resume/Rename/Close
    /// buttons, and one row per member.
    fn build(summary: &ActivitySummary) -> ActivityEntry {
        let header = Self::build_header(&summary.name, &summary.detail, summary.activity);
        let switch = Button::new(
            ButtonContent::Label(String::from("Switch")),
            ControlRole::Primary,
        );
        let gated = if summary.can_control {
            ActionVerdict::Ready
        } else {
            ActionVerdict::DeniedByAuthority
        };
        let mut pause_resume = Button::labelled(if summary.paused { "Resume" } else { "Pause" });
        pause_resume.set_state(gated.to_state());
        let rename = Button::labelled("Rename");
        let mut close = Button::new(
            ButtonContent::Label(String::from("Close")),
            ControlRole::Destructive,
        );
        close.set_state(if summary.can_control {
            ControlState::idle().with_authority(AuthorityState::NeedsConfirmation)
        } else {
            ActionVerdict::DeniedByAuthority.to_state()
        });
        let members = summary
            .members
            .iter()
            .map(|member| {
                ListRow::new(member.name.clone())
                    .with_trailing(member.detail.clone())
                    .with_state(ControlState::idle().with_activity(member.activity))
            })
            .collect();
        ActivityEntry {
            id: summary.id,
            name: summary.name.clone(),
            detail: summary.detail.clone(),
            activity: summary.activity,
            header,
            switch,
            pause_resume,
            rename,
            close,
            paused: summary.paused,
            can_control: summary.can_control,
            can_accept_member: summary.can_accept_member,
            members,
        }
    }

    /// Build (or rebuild, after a rename commit) an activity header row from
    /// its name, trailing detail, and live activity — the one place that
    /// composes a header [`ListRow`], so a rename can never drift from how
    /// [`build`](Self::build) first built it.
    fn build_header(name: &str, detail: &str, activity: ActivityState) -> ListRow {
        ListRow::new(name)
            .with_trailing(detail)
            .with_state(ControlState::idle().with_activity(activity))
    }

    /// The activity row a flattened index names — its owning activity's
    /// header, or one of its members — or `None` past the end of the
    /// flattened list.
    pub(super) fn row_at(&self, index: usize) -> Option<ActivityRow> {
        let mut remaining = index;
        for (ai, entry) in self.entries.iter().enumerate() {
            if remaining == 0 {
                return Some(ActivityRow::Header(ai));
            }
            remaining -= 1;
            if remaining < entry.members.len() {
                return Some(ActivityRow::Member(ai, remaining));
            }
            remaining -= entry.members.len();
        }
        None
    }

    /// A member row's rectangle: indented under its header by one control
    /// height, so the grouping reads as a hierarchy.
    fn member_rect(item: Rect, scale: Scale, theme: &Theme) -> Rect {
        let indent = scale.scale_length(theme.metrics().control_height);
        let indented = Rect::new(
            item.left() + to_i32(indent),
            item.top(),
            item.width.saturating_sub(indent),
            item.height,
        );
        let (row_rect, _) = Switchboard::split_row(indented, 0, scale, theme);
        row_rect
    }

    /// Begin an inline rename of the activity at `index`, pre-filled with its
    /// current name.
    fn begin_rename(&mut self, index: usize) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let mut field = TextField::new().with_text(&entry.name).with_max_len(48);
        field.set_focused(true);
        self.rename = Some(RenameEdit {
            id: entry.id,
            index,
            field,
        });
    }

    /// Where the detail pane's parts sit inside `content`, or [`None`] when
    /// the pane is too small to seat even its identity line.
    fn detail_layout(
        content: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<DetailLayout> {
        let gap = scale.scale_length(theme.metrics().control_gap);
        let line = font.line_height();
        let rows = FactList::row_height(scale, theme).saturating_mul(TOTALS);
        let mut top = content.top();
        let mut left = content.height;
        let mut take = |height: u32| -> Rect {
            let height = height.min(left);
            let rect = Rect::new(content.left(), top, content.width, height);
            top = top.saturating_add(to_i32(height));
            left = left.saturating_sub(height);
            let spent = gap.min(left);
            top = top.saturating_add(to_i32(spent));
            left = left.saturating_sub(spent);
            rect
        };
        let identity = take(line);
        if identity.is_empty() {
            return None;
        }
        let totals = take(rows);
        let members = Rect::new(content.left(), top, content.width, left);
        Some(DetailLayout {
            identity,
            totals,
            members,
        })
    }

    /// The plate the detail pane draws in: its caption is the selected
    /// activity's own name as the row currently spells it — so a rename the
    /// reader has just committed shows here at once, rather than waiting for
    /// the next sample to carry it back.
    fn detail_panel(&self) -> Panel {
        Panel::new(
            self.selected_index()
                .and_then(|index| self.entries.get(index))
                .map_or(DETAIL_TITLE, |entry| entry.name.as_str()),
        )
    }

    /// Paint the selected activity's detail pane.
    fn render_detail(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(rect) = ctx.frame.detail else {
            return;
        };
        let panel = self.detail_panel();
        panel.render(surface, rect, ctx.scale, ctx.theme);
        let Some(content) = panel.content_rect(rect, ctx.scale, ctx.theme) else {
            return;
        };
        let muted = Color::from(ctx.theme.palette().on_surface_muted);
        let Some(item) = self.selected_item() else {
            ctx.font.draw_text(
                surface,
                content.left(),
                content.top(),
                &selection_prompt("an activity"),
                muted,
            );
            return;
        };
        let Some(layout) = Self::detail_layout(content, ctx.scale, ctx.theme, ctx.font) else {
            return;
        };
        ctx.font.draw_text(
            surface,
            layout.identity.left(),
            layout.identity.top(),
            &identity_text(item),
            Color::from(ctx.theme.palette().on_surface),
        );
        combined_facts(item).render(surface, layout.totals, ctx.scale, ctx.theme);
        match member_facts(item) {
            Some(facts) => {
                facts.render(surface, layout.members, ctx.scale, ctx.theme);
            }
            None => {
                ctx.font.draw_text(
                    surface,
                    layout.members.left(),
                    layout.members.top(),
                    NO_MEMBERS,
                    muted,
                );
            }
        }
    }
}

/// How many combined readings the detail pane totals: CPU, memory, disk and
/// network, the same four resources every other screen compares.
const TOTALS: u32 = 4;

/// The detail pane's identity line: the activity and how many tasks it holds.
fn identity_text(item: &ActivitySummary) -> String {
    alloc::format!("{} · {}", item.name, item.detail)
}

/// The group's four combined readings, in the fixed order a reader learns
/// once: CPU, Memory, Disk, Network.
fn combined_facts(item: &ActivitySummary) -> FactList {
    FactList::new(alloc::vec![
        Fact::new("CPU", reading_text(&item.cpu)),
        Fact::new("Memory", reading_text(&item.memory)),
        Fact::new("Disk", reading_text(&item.disk)),
        Fact::new("Network", reading_text(&item.network)),
    ])
}

/// One fact per member — its name against its own reading — or [`None`] for
/// an activity with no members at all.
///
/// A member with no running task says so; a running member whose share this
/// sample could not measure wears the unmeasured mark instead, because the
/// two are different facts and an empty value would be read as nought.
fn member_facts(item: &ActivitySummary) -> Option<FactList> {
    if item.members.is_empty() {
        return None;
    }
    Some(FactList::new(
        item.members
            .iter()
            .map(|member| Fact::new(member.name.clone(), member_reading(member)))
            .collect(),
    ))
}

/// One member's own reading as the detail pane states it.
fn member_reading(member: &ActivityMember) -> String {
    if !member.joined {
        return String::from(MEMBER_NOT_RUNNING);
    }
    if member.detail.is_empty() {
        return reading_text(&Reading::Absent(Unmeasured::Unavailable));
    }
    member.detail.clone()
}

impl SectionView for ActivitiesSection {
    /// The flattened group list with the selected group's detail beside it.
    /// There is no action rail: an activity's commands live in its own header
    /// row, where the group that owns them is unambiguous.
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: None,
            sidebar_width: 0,
            header_height: 0,
            detail_width: DETAIL_PANE_WIDTH,
            impact_width: 0,
            rail_width: 0,
            footer_height: 0,
            primary_row_commands: Self::BUTTONS,
        }
    }

    /// Adopt a fresh sample, keeping the reader on the group they opened.
    ///
    /// The list is rebuilt every sample, so the selection is re-resolved
    /// against each group's stable id: a group that still exists stays
    /// selected however far it has moved, and only one that has closed loses
    /// the selection.
    fn adopt(&mut self, model: &SwitchboardModel) {
        self.items.clone_from(&model.activities);
        self.entries = model.activities.iter().map(Self::build).collect();
        self.selected = resolve_selection(self.selected, self.items.iter().map(|item| item.id));
        self.mark_selection();
        // An in-flight rename survives a refresh only as long as its activity
        // still exists, re-located by stable id — never by its old position,
        // which a refresh may have shifted or removed entirely (fail closed).
        self.rename = self.rename.take().and_then(|edit| {
            self.entries
                .iter()
                .position(|entry| entry.id == edit.id)
                .map(|index| RenameEdit {
                    id: edit.id,
                    index,
                    field: edit.field,
                })
        });
        self.submitted_name = None;
        self.focus = self.focus.min(self.item_count().saturating_sub(1));
        self.action = 0;
    }

    /// One header row per activity plus one row per member: the flattened list
    /// the cursor and the scrollbar both count in.
    fn item_count(&self) -> usize {
        self.entries.iter().map(|a| 1 + a.members.len()).sum()
    }

    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo {
        ListInfo::rows(frame.primary, self.item_count(), scale, theme)
    }

    fn row_buttons(&self) -> u32 {
        Self::BUTTONS
    }

    /// Only a header row carries buttons; a member row is display-only.
    fn focused_action_count(&self) -> usize {
        match self.row_at(self.focus) {
            Some(ActivityRow::Header(_)) => Self::BUTTONS as usize,
            Some(ActivityRow::Member(..)) | None => 0,
        }
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    /// Move the cursor, selecting the activity the row it lands on belongs
    /// to, so the detail pane always describes the group the reader is in.
    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
        if let Some(activity) = self.row_at(index).map(|row| match row {
            ActivityRow::Header(ai) | ActivityRow::Member(ai, _) => ai,
        }) {
            self.select_activity(activity);
        }
    }

    fn row_action(&self) -> usize {
        self.action
    }

    fn set_row_action(&mut self, index: usize) {
        self.action = index;
    }

    /// Activate the focused header's action-focused button (Switch,
    /// Pause-or-Resume, Rename, Close, in action-focus order). Member rows are
    /// display-only, so they activate nothing.
    fn activate_focused(&mut self, key: Key) -> Option<SectionOutcome> {
        let Some(ActivityRow::Header(index)) = self.row_at(self.focus) else {
            return None;
        };
        let action = self.action;
        let entry = self.entries.get_mut(index)?;
        let control = match action {
            0 => (entry.switch.on_key(key) == Some(ButtonAction::Activated))
                .then_some(ActivityControl::Switch),
            1 => {
                let control = if entry.paused {
                    ActivityControl::Resume
                } else {
                    ActivityControl::Pause
                };
                (entry.pause_resume.on_key(key) == Some(ButtonAction::Activated)).then_some(control)
            }
            2 => {
                if entry.rename.on_key(key) == Some(ButtonAction::Activated) {
                    self.begin_rename(index);
                }
                None
            }
            _ => (entry.close.on_key(key) == Some(ButtonAction::Activated))
                .then_some(ActivityControl::Close),
        }?;
        Some(SectionOutcome::Action(SwitchboardAction::Activity {
            index,
            control,
        }))
    }

    /// Paint the visible rows: a header row (with its four buttons, or an
    /// in-flight rename field in place of the header) followed by its indented
    /// member rows.
    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        for slot in 0..info.visible() {
            let Some(row) = self.row_at(ctx.start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            match row {
                ActivityRow::Header(ai) => {
                    let Some(entry) = self.entries.get(ai) else {
                        continue;
                    };
                    let (row_rect, buttons) =
                        Switchboard::split_row(item, Self::BUTTONS, ctx.scale, ctx.theme);
                    if let Some(edit) = self.rename.as_ref().filter(|e| e.index == ai) {
                        edit.field.render(surface, row_rect, ctx.scale, ctx.theme);
                    } else {
                        entry
                            .header
                            .render(surface, row_rect, ctx.scale, ctx.theme, None);
                    }
                    if let Some(rect) = buttons.first() {
                        entry.switch.render(surface, *rect, ctx.scale, ctx.theme);
                    }
                    if let Some(rect) = buttons.get(1) {
                        entry
                            .pause_resume
                            .render(surface, *rect, ctx.scale, ctx.theme);
                    }
                    if let Some(rect) = buttons.get(2) {
                        entry.rename.render(surface, *rect, ctx.scale, ctx.theme);
                    }
                    if let Some(rect) = buttons.get(3) {
                        entry.close.render(surface, *rect, ctx.scale, ctx.theme);
                    }
                }
                ActivityRow::Member(ai, mi) => {
                    let Some(member) = self.entries.get(ai).and_then(|e| e.members.get(mi)) else {
                        continue;
                    };
                    let row_rect = Self::member_rect(item, ctx.scale, ctx.theme);
                    member.render(surface, row_rect, ctx.scale, ctx.theme, None);
                }
            }
        }
        self.render_detail(surface, ctx);
    }

    /// Route a pointer event to the header rows (their four buttons) and the
    /// member rows (selection only).
    ///
    /// Whichever part of a header the press lands on — its name or any of its
    /// four commands — that group becomes the selected one, so the detail pane
    /// always describes the group the reader just acted on rather than one they
    /// left behind.
    fn on_pointer(&mut self, event: &InputEvent, ctx: SectionCtx<'_>) -> Option<SectionOutcome> {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        for slot in 0..info.visible() {
            let Some(row) = self.row_at(ctx.start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            match row {
                ActivityRow::Header(index) => {
                    let (row_rect, buttons) =
                        Switchboard::split_row(item, Self::BUTTONS, ctx.scale, ctx.theme);
                    let on_header = self.entries.get_mut(index).is_some_and(|entry| {
                        entry.header.on_pointer(event, row_rect) == Some(RowAction::Activated)
                    });
                    if on_header {
                        self.select_activity(index);
                        return None;
                    }
                    let Some(entry) = self.entries.get_mut(index) else {
                        continue;
                    };
                    if buttons.first().is_some_and(|rect| {
                        entry.switch.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        self.select_activity(index);
                        return Some(SectionOutcome::Action(SwitchboardAction::Activity {
                            index,
                            control: ActivityControl::Switch,
                        }));
                    }
                    if buttons.get(1).is_some_and(|rect| {
                        entry.pause_resume.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        let control = if entry.paused {
                            ActivityControl::Resume
                        } else {
                            ActivityControl::Pause
                        };
                        self.select_activity(index);
                        return Some(SectionOutcome::Action(SwitchboardAction::Activity {
                            index,
                            control,
                        }));
                    }
                    if buttons.get(2).is_some_and(|rect| {
                        entry.rename.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        self.select_activity(index);
                        self.begin_rename(index);
                        return None;
                    }
                    if buttons.get(3).is_some_and(|rect| {
                        entry.close.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        self.select_activity(index);
                        return Some(SectionOutcome::Action(SwitchboardAction::Activity {
                            index,
                            control: ActivityControl::Close,
                        }));
                    }
                }
                ActivityRow::Member(ai, mi) => {
                    let row_rect = Self::member_rect(item, ctx.scale, ctx.theme);
                    let Some(member) = self
                        .entries
                        .get_mut(ai)
                        .and_then(|entry| entry.members.get_mut(mi))
                    else {
                        continue;
                    };
                    if member.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                        if let Some(entry) = self.entries.get_mut(ai) {
                            for (i, row) in entry.members.iter_mut().enumerate() {
                                row.set_selected(i == mi);
                            }
                        }
                        self.select_activity(ai);
                    }
                }
            }
        }
        None
    }

    /// The flattened cursor marks a button only when it names a header row; a
    /// member row is a Focus Field of one.
    fn apply_focus_marks(&mut self, focused: bool) {
        let action = self.action;
        let row = focused.then(|| self.row_at(self.focus)).flatten();
        let header = row.and_then(|row| match row {
            ActivityRow::Header(ai) => Some(ai),
            ActivityRow::Member(..) => None,
        });
        let member = row.and_then(|row| match row {
            ActivityRow::Member(ai, mi) => Some((ai, mi)),
            ActivityRow::Header(..) => None,
        });
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let here = header == Some(i);
            entry.header.set_in_focus_field(here);
            entry.switch.set_focused(here && action == 0);
            entry.switch.set_in_focus_field(here);
            entry.pause_resume.set_focused(here && action == 1);
            entry.pause_resume.set_in_focus_field(here);
            entry.rename.set_focused(here && action == 2);
            entry.rename.set_in_focus_field(here);
            entry.close.set_focused(here && action == 3);
            entry.close.set_in_focus_field(here);
            for (m, row) in entry.members.iter_mut().enumerate() {
                row.set_in_focus_field(member == Some((i, m)));
            }
        }
    }

    /// The inline edit owns the keyboard while it is open, so no key reaches
    /// the regions beneath it until it commits or cancels.
    fn holds_keyboard(&self) -> bool {
        self.rename.is_some()
    }

    /// Route a key to the in-flight rename field: Enter commits (rebuilding
    /// the header row and reporting the rename), Escape cancels without
    /// emitting, and everything else edits the field.
    fn overlay_on_key(&mut self, key: Key) -> Option<SectionOutcome> {
        let action = self
            .rename
            .as_mut()?
            .field
            .on_key(key, Modifiers::default());
        match action {
            Some(TextAction::Submitted) => {
                let edit = self.rename.take()?;
                let index = edit.index;
                let entry = self.entries.get_mut(index)?;
                entry.name = String::from(edit.field.text());
                entry.header = Self::build_header(&entry.name, &entry.detail, entry.activity);
                self.submitted_name = Some(entry.name.clone());
                Some(SectionOutcome::Action(SwitchboardAction::ActivityRenamed {
                    index,
                }))
            }
            Some(TextAction::Cancelled) => {
                self.rename = None;
                None
            }
            Some(TextAction::Edited) | None => None,
        }
    }

    fn dismiss_overlay(&mut self) {
        self.rename = None;
    }
}

#[cfg(test)]
#[path = "activities_tests.rs"]
mod tests;
