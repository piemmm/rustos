//! Unit tests for the Tasks section: the selected task's command rail, and
//! the group popup that files a task into an activity.

use tairix_abi::ProcessState;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, ButtonContent, CellAlign, ControlDisposition, ControlRole, ControlState,
    MetricLayout, MetricTile, PointerState, PressureKind, RecoveryState, StatusPill, Tab,
    TableCell,
};

use super::{
    TaskAuthority, TaskControl, TaskKind, TaskSummary, COLUMN_WEIGHTS, COL_ACTIVITY, COL_CPU,
    COL_LAST_ACTIVE, COL_MEMORY, COL_NETWORK,
};
use tairix_controls::testkit::high_contrast;

use crate::panel::{WIN_HEIGHT, WIN_WIDTH};
use crate::view::frame::resolve_section_frame;
use crate::view::tasks::TasksSection;
use crate::view::test_support::{
    centre, click, focus_task_row, font, has_ink, model, moved, select_task_row, task_id,
    task_rail_rects, task_row_point, PRESS, RELEASE,
};
use crate::view::{
    ActionVerdict, Section, SectionView, Switchboard, SwitchboardAction, SwitchboardModel,
    UNMEASURED_READING,
};

/// The window the Switchboard actually opens at.
///
/// The rail seats as many whole commands as its region holds, so a test that
/// aims at a command must use a window the app really ships rather than a
/// smaller fixture that would clip the list.
fn bounds() -> Rect {
    Rect::new(0, 0, WIN_WIDTH, WIN_HEIGHT)
}

/// A one-task model whose sole task carries `authority`.
fn one_task(authority: TaskAuthority) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    m.tasks.push(TaskSummary {
        proc_id: task_id(0),
        name: alloc::string::String::from("locked task"),
        authority,
        ..TaskSummary::default()
    });
    m
}

/// Every command permitted.
fn all_ready() -> TaskAuthority {
    TaskAuthority {
        switch: ActionVerdict::Ready,
        pause: ActionVerdict::Ready,
        resume: ActionVerdict::Ready,
        lower_priority: ActionVerdict::Ready,
        force_quit: ActionVerdict::Ready,
    }
}

/// Click rail slot `slot` after selecting row `row`, returning what the
/// composition reported.
fn invoke_rail(
    sb: &mut Switchboard,
    b: Rect,
    theme: &Theme,
    row: usize,
    slot: usize,
) -> alloc::vec::Vec<SwitchboardAction> {
    select_task_row(sb, b, Scale::ONE, theme, row);
    let rects = task_rail_rects(sb, b, Scale::ONE, theme);
    let (x, y) = centre(rects[slot]);
    click(sb, b, Scale::ONE, theme, x, y)
}

#[test]
fn a_table_with_rows_selects_the_first_and_offers_its_commands() {
    let sb = Switchboard::new(&model());
    assert_eq!(
        sb.tasks.selected,
        Some(task_id(0)),
        "a table with something to show always has a subject"
    );
    assert_eq!(sb.tasks.rail.len(), 8, "so its commands are offered");
}

#[test]
fn an_empty_table_selects_nothing_and_offers_no_command() {
    let sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
    assert_eq!(sb.tasks.selected, None);
    assert!(
        sb.tasks.rail.is_empty(),
        "with no subject the rail offers no command"
    );
}

#[test]
fn choosing_a_row_gives_the_rail_its_whole_command_set() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    select_task_row(&mut sb, b, Scale::ONE, &theme, 1);

    assert_eq!(sb.tasks.selected, Some(task_id(1)));
    assert_eq!(sb.tasks.rail.len(), 8, "every command keeps its slot");
    let labels: alloc::vec::Vec<&str> = sb
        .tasks
        .rail
        .items()
        .iter()
        .map(|item| match item.content() {
            ButtonContent::IconLabel { label, .. } => label.as_str(),
            _ => panic!("every rail command carries an icon beside its label"),
        })
        .collect();
    assert_eq!(
        labels,
        [
            "Switch to",
            "Reveal window",
            "Pause",
            "Resume",
            "Lower priority",
            "Open logs",
            "Group\u{2026}",
            "Force quit",
        ]
    );
}

#[test]
fn each_rail_command_reports_its_own_control() {
    let theme = Theme::dark();
    let b = bounds();
    let wanted = [
        (0, TaskControl::Switch),
        (1, TaskControl::Reveal),
        (2, TaskControl::Pause),
        (3, TaskControl::Resume),
        (4, TaskControl::LowerPriority),
        (7, TaskControl::ForceQuit),
    ];
    for (slot, control) in wanted {
        let mut sb = Switchboard::new(&one_task(all_ready()));
        let actions = invoke_rail(&mut sb, b, &theme, 0, slot);
        assert!(
            actions.contains(&SwitchboardAction::Task { index: 0, control }),
            "slot {slot} must report {control:?}, got {actions:?}"
        );
    }
}

#[test]
fn a_denied_command_keeps_its_slot_and_fails_closed() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&one_task(TaskAuthority {
        switch: ActionVerdict::Ready,
        ..TaskAuthority::default()
    }));
    let actions = invoke_rail(&mut sb, b, &theme, 0, 7);
    assert!(
        actions.is_empty(),
        "a command the caller may not use must not activate"
    );
    assert_eq!(sb.tasks.rail.len(), 8, "it keeps its slot regardless");
    assert_eq!(
        sb.tasks.rail.items()[7].state().disposition(),
        ControlDisposition::DeniedByAuthority,
        "and wears the Authority Mark"
    );
}

#[test]
fn a_command_the_state_rules_out_is_plainly_disabled() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&one_task(TaskAuthority {
        resume: ActionVerdict::DisabledByState,
        ..all_ready()
    }));
    let actions = invoke_rail(&mut sb, b, &theme, 0, 3);
    assert!(actions.is_empty(), "a disabled command must not activate");
    assert_eq!(
        sb.tasks.rail.items()[3].state().disposition(),
        ControlDisposition::DisabledByState
    );
}

#[test]
fn open_logs_states_its_absence_rather_than_pretending_to_work() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&one_task(all_ready()));
    let actions = invoke_rail(&mut sb, b, &theme, 0, 5);
    assert!(actions.is_empty(), "no journal-read interface exists yet");
    assert_eq!(
        sb.tasks.rail.items()[5].state().disposition(),
        ControlDisposition::DisabledByState,
        "so the command is plainly disabled, never denied"
    );
}

#[test]
fn force_quit_carries_the_destructive_role() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    select_task_row(&mut sb, bounds(), Scale::ONE, &theme, 0);
    assert_eq!(sb.tasks.rail.items()[7].role(), ControlRole::Destructive);
    for slot in 0..7 {
        assert_eq!(sb.tasks.rail.items()[slot].role(), ControlRole::Neutral);
    }
}

#[test]
fn the_selection_follows_the_task_when_a_re_sort_moves_it() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    select_task_row(&mut sb, b, Scale::ONE, &theme, 2);
    let chosen = sb.tasks.selected.expect("a selected task");

    // Reverse the name order; the chosen task is now somewhere else.
    sb.tasks
        .header
        .set_sort(Some((0, super::SortOrder::Ascending)));
    sb.tasks.arrange();
    sb.tasks
        .header
        .set_sort(Some((0, super::SortOrder::Descending)));
    sb.tasks.arrange();

    assert_eq!(
        sb.tasks.selected,
        Some(chosen),
        "the selection names the task, never the row it sat in"
    );
    assert_eq!(sb.tasks.rail.len(), 8, "so its commands are still offered");
}

#[test]
fn hiding_the_selected_task_drops_the_selection_and_its_commands() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    select_task_row(&mut sb, b, Scale::ONE, &theme, 0);
    assert!(sb.tasks.selected.is_some());

    sb.tasks.search.set_text("nothing matches this");
    sb.tasks.arrange();

    assert_eq!(sb.tasks.selected, None);
    assert!(
        sb.tasks.rail.is_empty(),
        "commands with no visible subject are withdrawn, not left dangling"
    );
}

/// Open the Group popup on task row 0 through the rail's Group command.
fn open_group_popup_on_first_task(sb: &mut Switchboard, b: Rect, theme: &Theme) {
    let actions = invoke_rail(sb, b, theme, 0, TasksSection::group_slot());
    assert!(actions.is_empty(), "opening the popup emits nothing");
    assert!(sb.tasks.popup.is_some(), "the Group popup must open");
}

/// A window point that hits row `index` of the open Group popup.
fn popup_row_point(sb: &Switchboard, b: Rect, theme: &Theme, index: usize) -> (i32, i32) {
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, theme, font());
    let anchor = sb.tasks.anchor_rect(ctx);
    let popup = sb.tasks.popup.as_ref().expect("an open Group popup");
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, theme);
    let x = rect.left() + i32::try_from(rect.width).unwrap_or(0) / 2;
    for y in rect.top()..rect.bottom() {
        if popup.menu.row_at(rect, Scale::ONE, theme, Point::new(x, y)) == Some(index) {
            return (x, y);
        }
    }
    panic!("popup row {index} is not hit-testable");
}

#[test]
fn the_group_command_opens_the_popup_on_the_selected_task() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let popup = sb.tasks.popup.as_ref().expect("popup");
    assert_eq!(popup.task, 0);
    // One row per activity, then "New activity"; an ungrouped task gets no
    // "Remove from activity" row.
    assert_eq!(popup.menu.items().len(), 7);
    assert_eq!(popup.menu.items()[0].label(), "activity 0");
    assert_eq!(popup.menu.items()[6].label(), "New activity");
}

#[test]
fn group_popup_anchors_below_its_command_inside_the_window() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    let anchor = sb.tasks.anchor_rect(ctx);
    let expected = task_rail_rects(&sb, b, Scale::ONE, &theme)[TasksSection::group_slot()];
    assert_eq!(anchor, expected, "the anchor is the Group command itself");
    let popup = sb.tasks.popup.as_ref().expect("popup");
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, &theme);
    // The Group command sits low in the rail, so the popup opens upward from
    // it rather than off the bottom of the window — either way it meets its
    // anchor's edge and stays wholly inside the window.
    assert!(
        rect.bottom() == anchor.top() || rect.top() == anchor.bottom(),
        "the popup meets its anchor's edge"
    );
    assert!(rect.left() >= b.left());
    assert!(rect.right() <= b.right());
    assert!(rect.top() >= b.top());
    assert!(rect.bottom() <= b.bottom());
}

#[test]
fn group_popup_lists_activities_with_disable_reasons() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.tasks[0].group = Some(0);
    m.activities[1].can_accept_member = false;
    m.can_create_activity = false;
    let mut sb = Switchboard::new(&m);
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let items = sb.tasks.popup.as_ref().expect("popup").menu.items();
    assert_eq!(items.len(), 8);
    assert_eq!(
        items[0].state().disposition(),
        ControlDisposition::DisabledByState
    );
    assert_eq!(items[0].reason(), Some("Current activity"));
    assert_eq!(
        items[1].state().disposition(),
        ControlDisposition::DisabledByState
    );
    assert_eq!(items[1].reason(), Some("Activity is full"));
    assert_eq!(
        items[2].state().disposition(),
        ControlDisposition::Interactive
    );
    assert_eq!(items[6].label(), "New activity");
    assert_eq!(
        items[6].state().disposition(),
        ControlDisposition::DisabledByState
    );
    assert_eq!(items[6].reason(), Some("Activity limit reached"));
    assert_eq!(items[7].label(), "Remove from activity");
    assert_eq!(
        items[7].state().disposition(),
        ControlDisposition::Interactive
    );
}

#[test]
fn group_popup_groups_to_an_existing_activity() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let (x, y) = popup_row_point(&sb, b, &theme, 2);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::TaskGrouped {
        task: 0,
        activity: Some(2)
    }));
    assert!(sb.tasks.popup.is_none(), "activation closes the popup");
}

#[test]
fn group_popup_new_activity_groups_to_none() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let (x, y) = popup_row_point(&sb, b, &theme, 6);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::TaskGrouped {
        task: 0,
        activity: None
    }));
    assert!(sb.tasks.popup.is_none());
}

#[test]
fn group_popup_removes_a_grouped_task() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.tasks[0].group = Some(0);
    let mut sb = Switchboard::new(&m);
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let (x, y) = popup_row_point(&sb, b, &theme, 7);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::TaskUngrouped { task: 0 }));
    assert!(sb.tasks.popup.is_none());
}

#[test]
fn group_popup_refuses_a_disabled_row() {
    let theme = Theme::dark();
    let b = bounds();
    let mut m = model();
    m.tasks[0].group = Some(0);
    let mut sb = Switchboard::new(&m);
    open_group_popup_on_first_task(&mut sb, b, &theme);
    // Row 0 is the task's current activity, disabled with its reason.
    let (x, y) = popup_row_point(&sb, b, &theme, 0);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "a disabled popup row must not activate"
    );
    assert!(
        sb.tasks.popup.is_some(),
        "a refused activation leaves the popup open"
    );
}

#[test]
fn group_popup_escape_dismisses_without_emitting() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Escape)), None);
    assert!(sb.tasks.popup.is_none());
}

#[test]
fn group_popup_outside_press_dismisses_without_emitting() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    let anchor = sb.tasks.anchor_rect(ctx);
    let popup = sb.tasks.popup.as_ref().expect("popup");
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, &theme);
    // The location band sits above the command the popup anchors on, so a
    // press there is genuinely outside it.
    let (x, y) = centre(layout.location);
    assert!(
        !rect.contains(Point::new(x, y)),
        "the probe point must sit outside the popup"
    );
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty(),
        "an outside press dismisses without emitting"
    );
    assert!(sb.tasks.popup.is_none());
}

#[test]
fn group_popup_drops_on_refresh_and_section_change() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    sb.set_model(&model());
    assert!(
        sb.tasks.popup.is_none(),
        "a refresh supersedes the menu the popup was built from"
    );

    open_group_popup_on_first_task(&mut sb, b, &theme);
    sb.select_section(Section::Jobs);
    assert!(
        sb.tasks.popup.is_none(),
        "a section change invalidates the popup's anchor"
    );
}

/// Walk the content cursor down onto rail slot `slot` from wherever it is.
///
/// The rail's stops follow the rows, so a reader reaches a command by
/// carrying on down past the last row exactly as the cursor does.
fn walk_to_rail_slot(sb: &mut Switchboard, slot: usize) {
    let target = sb.tasks.rail_focus_index(slot);
    while sb.active().content_focus() < target {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    assert_eq!(sb.active().content_focus(), target);
}

#[test]
fn the_keyboard_selects_a_row_then_reaches_its_commands() {
    let mut sb = Switchboard::new(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        None,
        "choosing a row reports nothing of its own"
    );
    assert_eq!(sb.tasks.selected, Some(task_id(0)));

    walk_to_rail_slot(&mut sb, 0);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Task {
            index: 0,
            control: TaskControl::Switch
        })
    );
}

#[test]
fn keyboard_group_flow_reaches_the_popup_and_activates() {
    let mut sb = Switchboard::new(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    walk_to_rail_slot(&mut sb, TasksSection::group_slot());
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        None,
        "opening the popup emits nothing"
    );
    assert_eq!(sb.tasks.popup.as_ref().map(|p| p.task), Some(0));
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::TaskGrouped {
            task: 0,
            activity: Some(0)
        })
    );
    assert!(sb.tasks.popup.is_none());
}

#[test]
fn a_rail_command_takes_the_focus_ring_from_the_rows() {
    let mut sb = Switchboard::new(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    walk_to_rail_slot(&mut sb, 2);
    assert!(
        sb.tasks.rail.items()[2].state().focus.focused,
        "the focused command wears the ring"
    );
    assert!(
        sb.tasks
            .entries
            .iter()
            .all(|entry| !entry.row.state().focus.focused),
        "and no row keeps it"
    );
}

/// One row a table fixture is to build, spelled in the order the table
/// shows it: what it is called, what kind of task it is, the condition it
/// is in, and the two figures the census and sort tests read.
struct RowSpec {
    name: &'static str,
    kind: TaskKind,
    recovery: RecoveryState,
    cpu: Option<u16>,
    memory: Option<u64>,
}

/// One [`RowSpec`], so a fixture reads as a table of rows rather than as a
/// column of struct literals.
const fn row(
    name: &'static str,
    kind: TaskKind,
    recovery: RecoveryState,
    cpu: Option<u16>,
    memory: Option<u64>,
) -> RowSpec {
    RowSpec {
        name,
        kind,
        recovery,
        cpu,
        memory,
    }
}

/// A model of exactly `rows`, so a test can state the census, filter and
/// sort inputs it is about rather than filtering a generic fixture.
fn table_model(rows: &[RowSpec]) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for (index, spec) in rows.iter().enumerate() {
        m.tasks.push(TaskSummary {
            proc_id: task_id(index),
            name: alloc::string::String::from(spec.name),
            kind: spec.kind,
            lifecycle: Some(ProcessState::Running),
            cpu_permille: spec.cpu,
            memory_bytes: spec.memory,
            recovery: spec.recovery,
            authority: all_ready(),
            ..TaskSummary::default()
        });
    }
    m
}

/// The three processes / one job / one service / one faulted mix the census
/// and filter tests both read, so both are asserted against one arrangement.
fn mixed_model() -> SwitchboardModel {
    table_model(&[
        row(
            "alpha",
            TaskKind::Process,
            RecoveryState::None,
            Some(300),
            Some(2048),
        ),
        row(
            "Beta",
            TaskKind::Process,
            RecoveryState::None,
            Some(100),
            Some(1024),
        ),
        row(
            "gamma",
            TaskKind::Process,
            RecoveryState::Hung,
            Some(200),
            None,
        ),
        row(
            "delta",
            TaskKind::Job,
            RecoveryState::None,
            None,
            Some(4096),
        ),
        row(
            "epsilon",
            TaskKind::Service,
            RecoveryState::None,
            Some(50),
            Some(512),
        ),
    ])
}

/// The shown rows' names, in the order the table would draw them.
fn shown(sb: &Switchboard) -> alloc::vec::Vec<alloc::string::String> {
    sb.tasks
        .order
        .iter()
        .filter_map(|index| sb.tasks.tasks.get(*index))
        .map(|task| task.name.clone())
        .collect()
}

/// Put the content cursor on one of the section's header stops.
fn focus_header_stop(sb: &mut Switchboard, stop: usize) {
    for _ in 0..stop {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    assert_eq!(sb.active().content_focus(), stop);
}

/// Put the content cursor on one of the section's footer stops.
///
/// The footer's stops come after the rows *and* the rail's commands, so the
/// walk counts both rather than assuming the rows are the last band.
fn focus_footer_stop(sb: &mut Switchboard, stop: usize) {
    let target = sb.tasks.rail_focus_index(usize::MAX) + 1 + stop;
    for _ in 0..target {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    }
    assert_eq!(sb.active().content_focus(), target);
}

/// Walk the action cursor to `index` within the focused stop.
fn walk_action_to(sb: &mut Switchboard, index: usize) {
    for _ in 0..index {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    }
    assert_eq!(sb.active().row_action(), index);
}

/// The census tiles a section showing these counts must have composed.
///
/// A tile states no value back, so the test asserts the whole composed
/// instrument: the count is checked together with the label it is filed
/// under, the glyph that identifies it, the tint that glyph wears, and the
/// plated stacked layout it is drawn in — which is stronger than reading a
/// figure out of it would be.
fn expected_census(counts: [usize; 4]) -> alloc::vec::Vec<MetricTile> {
    [
        ("Processes", IconKind::Executable, PressureKind::Cpu),
        ("Jobs", IconKind::Job, PressureKind::Disk),
        ("Services", IconKind::ServiceBundle, PressureKind::Network),
        ("Alerts", IconKind::Bell, PressureKind::Thermal),
    ]
    .iter()
    .zip(counts)
    .map(|((label, icon, tint), count)| {
        MetricTile::new(*label, alloc::format!("{count}"), *tint)
            .with_layout(MetricLayout::Stacked)
            .with_icon(*icon)
    })
    .collect()
}

#[test]
fn each_census_tile_counts_what_the_model_carries() {
    let sb = Switchboard::new(&mixed_model());
    assert_eq!(
        sb.tasks.census,
        expected_census([3, 1, 1, 1]),
        "each tile counts rows the model genuinely carries"
    );
}

#[test]
fn a_census_tile_with_nothing_to_count_reads_zero_not_blank() {
    let sb = Switchboard::new(&table_model(&[row(
        "solo",
        TaskKind::Process,
        RecoveryState::None,
        None,
        None,
    )]));
    assert_eq!(
        sb.tasks.census,
        expected_census([1, 0, 0, 0]),
        "a source with nothing to report counts zero, never nothing"
    );
}

#[test]
fn every_filter_tab_carries_its_own_real_count() {
    let sb = Switchboard::new(&mixed_model());
    let labels: alloc::vec::Vec<&str> = sb.tasks.filters.tabs().iter().map(Tab::label).collect();
    assert_eq!(
        labels,
        alloc::vec!["All 5", "Processes 3", "Jobs 1", "Services 1", "Faults 1"],
        "a tab states the count its rows will deliver"
    );
}

#[test]
fn choosing_a_filter_shows_exactly_the_rows_it_counted() {
    for (stop, expected) in [
        (
            0usize,
            alloc::vec!["alpha", "Beta", "gamma", "delta", "epsilon"],
        ),
        (1, alloc::vec!["alpha", "Beta", "gamma"]),
        (2, alloc::vec!["delta"]),
        (3, alloc::vec!["epsilon"]),
        (4, alloc::vec!["gamma"]),
    ] {
        let mut sb = Switchboard::new(&mixed_model());
        focus_header_stop(&mut sb, 0);
        walk_action_to(&mut sb, stop);
        assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
        assert_eq!(shown(&sb), expected, "filter {stop} shows what it counted");
        assert_eq!(
            sb.tasks.entries.len(),
            expected.len(),
            "and builds one row per shown task"
        );
    }
}

#[test]
fn a_filter_that_admits_nothing_shows_no_rows_and_strands_no_cursor() {
    let mut sb = Switchboard::new(&table_model(&[row(
        "solo",
        TaskKind::Process,
        RecoveryState::None,
        None,
        None,
    )]));
    focus_header_stop(&mut sb, 0);
    walk_action_to(&mut sb, 2);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert!(sb.tasks.entries.is_empty(), "no job rows to show");
    assert!(
        sb.active().content_focus() < 3,
        "the cursor rests in the header, where the reader's next act is"
    );
}

#[test]
fn search_matches_on_the_task_name_ignoring_case() {
    let mut sb = Switchboard::new(&mixed_model());
    focus_header_stop(&mut sb, 1);
    for ch in "BET".chars() {
        assert_eq!(sb.on_key(Key::Char(ch)), None);
    }
    assert_eq!(shown(&sb), alloc::vec!["Beta"], "case is folded both ways");
    assert_eq!(sb.tasks.entries.len(), 1);
}

#[test]
fn search_matching_nothing_shows_nothing_rather_than_everything() {
    let mut sb = Switchboard::new(&mixed_model());
    focus_header_stop(&mut sb, 1);
    for ch in "zzz".chars() {
        assert_eq!(sb.on_key(Key::Char(ch)), None);
    }
    assert!(
        sb.tasks.entries.is_empty(),
        "an unmatched search fails closed"
    );
}

#[test]
fn clearing_the_search_restores_every_row() {
    let mut sb = Switchboard::new(&mixed_model());
    focus_header_stop(&mut sb, 1);
    for ch in "bet".chars() {
        assert_eq!(sb.on_key(Key::Char(ch)), None);
    }
    assert_eq!(sb.tasks.entries.len(), 1);
    for _ in 0..3 {
        assert_eq!(sb.on_key(Key::Named(NamedKey::Backspace)), None);
    }
    assert!(sb.tasks.search.text().is_empty());
    assert_eq!(sb.tasks.entries.len(), 5, "clearing restores every row");
}

/// Sort by the column at `column`, returning the shown names.
fn sorted_by(model: &SwitchboardModel, column: usize) -> alloc::vec::Vec<alloc::string::String> {
    let mut sb = Switchboard::new(model);
    focus_header_stop(&mut sb, 2);
    walk_action_to(&mut sb, column);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    shown(&sb)
}

#[test]
fn each_sortable_column_orders_by_the_value_its_cells_show() {
    let m = mixed_model();
    assert_eq!(
        sorted_by(&m, 0),
        alloc::vec!["Beta", "alpha", "delta", "epsilon", "gamma"],
        "Task sorts by name"
    );
    assert_eq!(
        sorted_by(&m, 1),
        alloc::vec!["delta", "alpha", "Beta", "gamma", "epsilon"],
        "Type sorts by kind, stably within a kind"
    );
    assert_eq!(
        sorted_by(&m, 4),
        alloc::vec!["epsilon", "Beta", "gamma", "alpha", "delta"],
        "CPU sorts by share, the unmeasured row last"
    );
    assert_eq!(
        sorted_by(&m, 5),
        alloc::vec!["epsilon", "Beta", "alpha", "delta", "gamma"],
        "Memory sorts by bytes, the unmeasured row last"
    );
}

#[test]
fn the_state_and_disk_columns_sort_by_their_own_readings() {
    let mut m = mixed_model();
    m.tasks[0].lifecycle = Some(ProcessState::Zombie);
    m.tasks[0].disk_bytes_per_sec = Some(90);
    m.tasks[1].disk_bytes_per_sec = Some(10);
    assert_eq!(
        sorted_by(&m, 2).first().map(alloc::string::String::as_str),
        Some("Beta"),
        "State sorts by its own text, so Running precedes Zombie"
    );
    let by_disk = sorted_by(&m, 6);
    assert_eq!(
        by_disk.first().map(alloc::string::String::as_str),
        Some("Beta"),
        "Disk sorts by rate"
    );
    assert_eq!(
        by_disk.get(1).map(alloc::string::String::as_str),
        Some("alpha")
    );
}

#[test]
fn a_second_press_reverses_the_sort_and_the_unmeasured_rows_stay_last() {
    let mut sb = Switchboard::new(&mixed_model());
    focus_header_stop(&mut sb, 2);
    walk_action_to(&mut sb, 4);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    let ascending = shown(&sb);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    let descending = shown(&sb);
    assert_ne!(ascending, descending, "a second press reverses the order");
    assert_eq!(
        descending.first().map(alloc::string::String::as_str),
        Some("delta"),
        "reversing puts the unmeasured row first, never a fabricated zero"
    );
}

#[test]
fn the_sort_is_stable_across_rows_it_cannot_separate() {
    // Four rows the Type sort cannot tell apart, in a deliberate order.
    let m = table_model(&[
        row("d", TaskKind::Process, RecoveryState::None, None, None),
        row("c", TaskKind::Process, RecoveryState::None, None, None),
        row("b", TaskKind::Process, RecoveryState::None, None, None),
        row("a", TaskKind::Process, RecoveryState::None, None, None),
    ]);
    assert_eq!(
        sorted_by(&m, 1),
        alloc::vec!["d", "c", "b", "a"],
        "rows the sort cannot separate keep the order the sample reported"
    );
}

#[test]
fn the_activity_column_plots_the_tasks_own_cpu_history() {
    let mut m = mixed_model();
    m.tasks[0].cpu_history = alloc::vec![100, 200, 300];
    let sb = Switchboard::new(&m);
    assert!(
        !sb.tasks.entries[0].spark.is_empty(),
        "a measured task plots its own readings"
    );
    assert!(
        sb.tasks.entries[1].spark.is_empty(),
        "a task with no history plots nothing rather than a flat fabricated line"
    );
}

#[test]
fn the_activity_sparkline_is_drawn_into_its_own_column() {
    let theme = Theme::dark();
    let mut m = mixed_model();
    m.tasks[0].cpu_history = alloc::vec![100, 900, 100, 900];
    let mut sb = Switchboard::new(&m);
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font());

    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let item = info.item_rect(0);
    let cells = sb.tasks.entries[0]
        .row
        .cell_rects(item, Scale::ONE, &theme, &COLUMN_WEIGHTS);
    let activity = cells[COL_ACTIVITY];
    assert!(
        has_ink(&surface, activity),
        "the sparkline draws inside the Activity column's own rect"
    );
}

#[test]
fn a_working_task_wears_no_activity_seam_under_its_row() {
    let mut m = mixed_model();
    for task in &mut m.tasks {
        task.activity = ActivityState::Working;
    }
    let sb = Switchboard::new(&m);
    for entry in &sb.tasks.entries {
        assert_eq!(
            entry.row.state().activity,
            ActivityState::Idle,
            "a row's activity would paint a Heat Seam along its whole lower \
             edge, which reads as a rule under the table rather than as a \
             reading about one task"
        );
    }
}

#[test]
fn a_tasks_activity_changes_nothing_the_table_draws() {
    let theme = Theme::dark();
    let b = bounds();

    let paint = |working: bool| {
        let mut m = mixed_model();
        for task in &mut m.tasks {
            task.activity = if working {
                ActivityState::Working
            } else {
                ActivityState::Idle
            };
            task.cpu_history = alloc::vec![100, 900, 100, 900];
        }
        let mut sb = Switchboard::new(&m);
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        surface
    };

    // A working task once painted a Heat Seam along its row's whole lower
    // edge, which read as an orange rule under the table. Every task working
    // must now draw exactly what every task idle draws: the trend belongs to
    // the Activity column, which plots the same readings either way.
    assert_eq!(
        paint(true).pixels(),
        paint(false).pixels(),
        "a row's activity must paint nothing of its own"
    );
}

#[test]
fn network_and_last_active_render_an_explicit_unmeasured_mark() {
    let sb = Switchboard::new(&mixed_model());
    let cells = sb.tasks.entries[0].row.cells();
    let unmeasured = TableCell::new(UNMEASURED_READING)
        .with_align(CellAlign::Trailing)
        .with_state(ControlState::disabled());
    for column in [COL_NETWORK, COL_LAST_ACTIVE] {
        assert_eq!(
            cells[column], unmeasured,
            "column {column} has no interface to read, so it says so — \
             and says it disabled, never as a small figure"
        );
    }
}

#[test]
fn an_unmeasured_figure_never_renders_as_a_zero() {
    let sb = Switchboard::new(&mixed_model());
    // `delta` has no CPU share and `gamma` no memory reading.
    let unmeasured = TableCell::new(UNMEASURED_READING)
        .with_align(CellAlign::Trailing)
        .with_state(ControlState::disabled());
    assert_eq!(sb.tasks.entries[3].row.cells()[COL_CPU], unmeasured);
    assert_eq!(sb.tasks.entries[2].row.cells()[COL_MEMORY], unmeasured);
    assert_eq!(
        sb.tasks.entries[0].row.cells()[COL_CPU],
        TableCell::numeric("30%").with_align(CellAlign::Trailing),
        "a measured share still reads as its own figure"
    );
}

#[test]
fn the_footer_counts_the_shown_rows_against_the_total() {
    let mut sb = Switchboard::new(&mixed_model());
    assert_eq!(sb.tasks.count, StatusPill::new("5 of 5 shown"));
    focus_header_stop(&mut sb, 0);
    walk_action_to(&mut sb, 1);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert_eq!(
        sb.tasks.count,
        StatusPill::new("3 of 5 shown"),
        "a filter changes what is shown, never the total"
    );
}

#[test]
fn the_grouping_control_arranges_the_same_rows() {
    let mut sb = Switchboard::new(&mixed_model());
    focus_footer_stop(&mut sb, 0);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert!(sb.tasks.grouping.is_expanded(), "the choices open");
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert_eq!(sb.tasks.grouping.selected_text(), Some("By type"));
    assert_eq!(
        shown(&sb),
        alloc::vec!["alpha", "Beta", "gamma", "delta", "epsilon"],
        "grouping arranges the same rows and drops none"
    );
    assert_eq!(sb.tasks.count, StatusPill::new("5 of 5 shown"));
}

#[test]
fn grouping_by_activity_puts_the_working_rows_first() {
    let mut m = mixed_model();
    m.tasks[4].activity = ActivityState::Working;
    let mut sb = Switchboard::new(&m);
    sb.tasks.grouping.set_selected(2);
    sb.tasks.adopt(&m);
    assert_eq!(
        shown(&sb).first().map(alloc::string::String::as_str),
        Some("epsilon"),
        "the working row leads its group"
    );
}

#[test]
fn auto_refresh_off_holds_the_rows_the_reader_was_reading() {
    let mut sb = Switchboard::new(&mixed_model());
    focus_footer_stop(&mut sb, 1);
    assert!(sb.tasks.auto_refresh.is_on(), "refreshing by default");
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert!(!sb.tasks.auto_refresh.is_on(), "the toggle turns it off");

    let mut later = mixed_model();
    later.tasks.truncate(1);
    sb.tasks.adopt(&later);
    assert_eq!(sb.tasks.entries.len(), 5, "a paused table holds its rows");

    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert!(sb.tasks.auto_refresh.is_on());
    sb.tasks.adopt(&later);
    assert_eq!(sb.tasks.entries.len(), 1, "resuming takes the new sample");
}

#[test]
fn the_cursor_reaches_every_header_rail_and_footer_control() {
    let mut sb = Switchboard::new(&mixed_model());
    let span = sb.active().focus_span();
    assert_eq!(
        span,
        3 + 5 + 8 + 2,
        "three header stops, five rows, eight commands, two footer"
    );

    let mut rows = alloc::vec::Vec::new();
    for stop in 0..span {
        if stop > 0 {
            assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
        }
        assert_eq!(sb.active().content_focus(), stop);
        rows.push(sb.active().focus_row(stop));
    }
    // Only the row band names a row to scroll to: the header's stops, the
    // rail's anchored commands and the footer's controls all sit outside the
    // scrolling list.
    let mut expected = alloc::vec![None, None, None];
    expected.extend((0..5).map(Some));
    expected.extend(core::iter::repeat_n(None, 8 + 2));
    assert_eq!(rows, expected);
}

#[test]
fn each_header_and_footer_control_takes_the_focus_ring_in_turn() {
    let mut sb = Switchboard::new(&mixed_model());
    let resting = Switchboard::new(&mixed_model());
    focus_header_stop(&mut sb, 1);
    assert_ne!(
        sb.tasks.search, resting.tasks.search,
        "the search field takes the ring"
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        sb.tasks.search, resting.tasks.search,
        "and gives it up again"
    );
    assert_ne!(
        sb.tasks.header, resting.tasks.header,
        "the column headings take it next"
    );

    let mut sb = Switchboard::new(&mixed_model());
    focus_footer_stop(&mut sb, 1);
    assert_ne!(
        sb.tasks.auto_refresh, resting.tasks.auto_refresh,
        "and the auto-refresh toggle is reachable in its turn"
    );
}

/// The pointer state the row in shown slot `row` is wearing.
fn row_pointer(sb: &Switchboard, row: usize) -> PointerState {
    sb.tasks.entries[row].row.state().pointer
}

/// Move the pointer onto shown row `row`, the way the compositor delivers it.
fn hover_row(sb: &mut Switchboard, b: Rect, theme: &Theme, row: usize) {
    let (x, y) = task_row_point(sb, b, Scale::ONE, theme, row);
    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, theme, font()),
        None,
        "moving onto a row asks for nothing"
    );
}

#[test]
fn a_refresh_keeps_the_hover_on_the_row_under_the_pointer() {
    let m = model();
    let mut sb = Switchboard::new(&m);
    let (b, theme) = (bounds(), Theme::dark());
    hover_row(&mut sb, b, &theme, 2);
    assert_eq!(row_pointer(&sb, 2), PointerState::Hover);

    sb.set_model(&m);

    assert_eq!(
        row_pointer(&sb, 2),
        PointerState::Hover,
        "a refresh moves neither the pointer nor the slot it is over"
    );
}

#[test]
fn a_refresh_drops_a_press_begun_on_a_row() {
    let m = model();
    let mut sb = Switchboard::new(&m);
    let (b, theme) = (bounds(), Theme::dark());
    hover_row(&mut sb, b, &theme, 2);
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, &theme, font()), None);
    assert_eq!(row_pointer(&sb, 2), PointerState::Pressed);

    sb.set_model(&m);

    assert_eq!(
        row_pointer(&sb, 2),
        PointerState::None,
        "the slot may now hold another task, so the press must not survive"
    );

    // A press latch holds wherever the pointer went, so it says nothing about
    // where the pointer is; the next motion states that.
    hover_row(&mut sb, b, &theme, 2);
    assert_eq!(row_pointer(&sb, 2), PointerState::Hover);
}

#[test]
fn a_refresh_that_drops_the_slot_carries_no_hover() {
    let m = model();
    let mut sb = Switchboard::new(&m);
    let (b, theme) = (bounds(), Theme::dark());
    hover_row(&mut sb, b, &theme, 2);

    let mut shorter = m.clone();
    shorter.tasks.truncate(1);
    sb.set_model(&shorter);

    assert_eq!(sb.tasks.entries.len(), 1);
    assert_eq!(
        row_pointer(&sb, 0),
        PointerState::None,
        "the pointer is over no row once the slot it was over has gone"
    );
}

/// Press rail command `index`, publishing `refresh` before the release when
/// one is given, and report what the release produced.
fn press_rail_command(
    sb: &mut Switchboard,
    b: Rect,
    theme: &Theme,
    index: usize,
    refresh: Option<&SwitchboardModel>,
) -> Option<SwitchboardAction> {
    let (x, y) = centre(task_rail_rects(sb, b, Scale::ONE, theme)[index]);
    assert_eq!(
        sb.on_pointer(&moved(x, y), b, Scale::ONE, theme, font()),
        None
    );
    assert_eq!(sb.on_pointer(&PRESS, b, Scale::ONE, theme, font()), None);
    if let Some(model) = refresh {
        sb.set_model(model);
    }
    sb.on_pointer(&RELEASE, b, Scale::ONE, theme, font())
}

#[test]
fn a_refresh_does_not_swallow_a_press_begun_on_a_rail_command() {
    let m = model();
    let (b, theme) = (bounds(), Theme::dark());

    let mut undisturbed = Switchboard::new(&m);
    select_task_row(&mut undisturbed, b, Scale::ONE, &theme, 0);
    let expected = press_rail_command(&mut undisturbed, b, &theme, 0, None);
    assert!(
        expected.is_some(),
        "a press and release on a rail command commands the selected task"
    );

    let mut refreshed = Switchboard::new(&m);
    select_task_row(&mut refreshed, b, Scale::ONE, &theme, 0);
    let across = press_rail_command(&mut refreshed, b, &theme, 0, Some(&m));

    assert_eq!(
        across, expected,
        "the refresh derived the same commands, so the press completes on the one it began on"
    );
}

#[test]
fn the_table_renders_in_both_themes_and_under_heavier_contrast() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut m = mixed_model();
        m.tasks[0].cpu_history = alloc::vec![100, 500, 900];
        let mut sb = Switchboard::new(&m);
        let b = bounds();
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        let layout = sb.compute_layout(b, Scale::ONE, &theme);
        let frame = resolve_section_frame(layout.content, sb.tasks.anatomy(), Scale::ONE, &theme);
        assert!(has_ink(&surface, layout.content), "the table draws");
        assert!(has_ink(&surface, frame.header), "so does its header band");
        assert!(has_ink(&surface, frame.footer), "and its footer band");
    }
}
