//! Unit tests for the Tasks section: its rows' actions, and the group
//! popup that files a task into an activity.

use tairix_abi::ProcessState;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, CellAlign, ControlDisposition, ControlState, MetricLayout, MetricTile,
    PressureKind, PressureState, RecoveryState, StatusPill, Tab, TableCell,
};

use super::{
    TaskKind, TaskSummary, COLUMN_WEIGHTS, COL_ACTIVITY, COL_CPU, COL_LAST_ACTIVE, COL_MEMORY,
    COL_NETWORK,
};
use tairix_controls::testkit::high_contrast;

use crate::view::frame::resolve_section_frame;
use crate::view::test_support::{
    bounds, centre, click, focus_task_row, font, has_ink, model, task_action_rects,
};
use crate::view::{
    Section, SectionView, Switchboard, SwitchboardAction, SwitchboardModel, UNMEASURED_READING,
};

#[test]
fn allowed_task_action_activates() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    let b = bounds();
    let buttons = task_action_rects(&mut sb, b, Scale::ONE, &theme, 0);
    let (x, y) = centre(buttons[0]);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Task { index: 0 }));
}

#[test]
fn denied_task_action_fails_closed() {
    let theme = Theme::dark();
    let mut m = SwitchboardModel::new("Switchboard");
    m.tasks.push(TaskSummary {
        name: alloc::string::String::from("locked task"),
        pressure: PressureState::None,
        activity: ActivityState::Idle,
        recovery: RecoveryState::None,
        action: alloc::string::String::from("End"),
        action_allowed: false,
        group: None,
        ..TaskSummary::default()
    });
    let mut sb = Switchboard::new(&m);
    let b = bounds();
    let buttons = task_action_rects(&mut sb, b, Scale::ONE, &theme, 0);
    let (x, y) = centre(buttons[0]);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.is_empty(), "a denied action must not activate");
}

/// Open the Group popup on task row 0 by clicking its Group button.
fn open_group_popup_on_first_task(sb: &mut Switchboard, b: Rect, theme: &Theme) {
    let buttons = task_action_rects(sb, b, Scale::ONE, theme, 0);
    let (x, y) = centre(buttons[1]);
    assert!(
        click(sb, b, Scale::ONE, theme, x, y).is_empty(),
        "opening the popup emits nothing"
    );
    assert!(sb.tasks.popup.is_some(), "the Group popup must open");
}

/// A window point that hits row `index` of the open Group popup.
fn popup_row_point(sb: &Switchboard, b: Rect, theme: &Theme, index: usize) -> (i32, i32) {
    let layout = sb.compute_layout(b, Scale::ONE, theme);
    let popup = sb.tasks.popup.as_ref().expect("an open Group popup");
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, theme, font());
    let anchor = sb.tasks.anchor_rect(popup.task, ctx);
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, theme, font());
    let x = rect.left() + i32::try_from(rect.width).unwrap_or(0) / 2;
    for y in rect.top()..rect.bottom() {
        if popup.menu.row_at(rect, Scale::ONE, theme, Point::new(x, y)) == Some(index) {
            return (x, y);
        }
    }
    panic!("popup row {index} is not hit-testable");
}

#[test]
fn group_button_opens_the_popup_on_its_task() {
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
fn group_popup_anchors_below_its_button_inside_the_window() {
    let theme = Theme::dark();
    // Tall enough to hold the whole popup below its anchor: the flip-upward
    // path is a different case, covered by its own test.
    let b = Rect::new(0, 0, 600, 560);
    let mut sb = Switchboard::new(&model());
    open_group_popup_on_first_task(&mut sb, b, &theme);
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let popup = sb.tasks.popup.as_ref().expect("popup");
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    let anchor = sb.tasks.anchor_rect(popup.task, ctx);
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, &theme, font());
    assert_eq!(
        rect.top(),
        anchor.bottom(),
        "the popup opens below its anchor"
    );
    assert!(rect.left() >= b.left());
    assert!(rect.right() <= b.right());
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
    let popup = sb.tasks.popup.as_ref().expect("popup");
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    let anchor = sb.tasks.anchor_rect(popup.task, ctx);
    let rect = Switchboard::popup_rect(&popup.menu, anchor, b, Scale::ONE, &theme, font());
    // The location band sits above the row the popup anchors on, so a press
    // there is genuinely outside it.
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

#[test]
fn keyboard_group_flow_reaches_the_popup_and_activates() {
    let mut sb = Switchboard::new(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
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
fn keyboard_reaches_the_task_group_button() {
    let mut sb = Switchboard::new(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(
        sb.on_key(Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Task { index: 0 })
    );
    assert_eq!(sb.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(sb.on_key(Key::Named(NamedKey::Enter)), None);
    assert_eq!(
        sb.tasks.popup.as_ref().map(|p| p.task),
        Some(0),
        "the popup opens on the focused task"
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
    for spec in rows {
        m.tasks.push(TaskSummary {
            name: alloc::string::String::from(spec.name),
            kind: spec.kind,
            lifecycle: Some(ProcessState::Running),
            cpu_permille: spec.cpu,
            memory_bytes: spec.memory,
            recovery: spec.recovery,
            action: alloc::string::String::from("End"),
            action_allowed: true,
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
fn focus_footer_stop(sb: &mut Switchboard, stop: usize) {
    let target = sb.tasks.focus_index_for_row(usize::MAX) + 1 + stop;
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
/// under and the layout it is drawn in, which is stronger than reading a
/// figure out of it would be.
fn expected_census(counts: [usize; 4]) -> alloc::vec::Vec<MetricTile> {
    ["Processes", "Jobs", "Services", "Alerts"]
        .iter()
        .zip(counts)
        .map(|(label, count)| {
            MetricTile::new(*label, alloc::format!("{count}"), PressureKind::Cpu)
                .with_layout(MetricLayout::Stacked)
                .unplated()
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
fn the_cursor_reaches_every_header_and_footer_control() {
    let mut sb = Switchboard::new(&mixed_model());
    let span = sb.active().focus_span();
    assert_eq!(span, 3 + 5 + 2, "three header stops, five rows, two footer");

    let mut rows = alloc::vec::Vec::new();
    for stop in 0..span {
        if stop > 0 {
            assert_eq!(sb.on_key(Key::Named(NamedKey::Down)), None);
        }
        assert_eq!(sb.active().content_focus(), stop);
        rows.push(sb.active().focus_row(stop));
    }
    assert_eq!(
        rows,
        alloc::vec![
            None,
            None,
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            None,
            None
        ],
        "only the row band names a row to scroll to"
    );
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
