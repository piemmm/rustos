//! Unit tests for the Tasks section: the selected task's command rail, and
//! the group popup that files a task into an activity.

use tairix_abi::ProcessState;
use tairix_geometry::{Rect, Scale};
use tairix_icon::{IconKind, NoArtwork};
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, ButtonContent, CellAlign, ControlDisposition, ControlRole, ControlState,
    PointerState, RecoveryState, StatusPill, TableCell,
};

use super::{
    TaskAuthority, TaskControl, TaskOwner, TaskSummary, COLUMN_WEIGHTS, COL_ACTIVITY, COL_CORE,
    COL_CPU, COL_MEMORY, COL_NETWORK,
};
use tairix_controls::damage;
use tairix_controls::testkit::high_contrast;

use crate::panel::{WIN_HEIGHT, WIN_WIDTH};
use crate::view::frame::resolve_section_frame;
use crate::view::test_support::{
    centre, click, focus_task_row, font, has_ink, key, model, moved, pointer, refresh,
    select_task_row, task_id, task_rail_rects, task_row_point, PRESS, RELEASE,
};
use crate::view::Sweep;
use crate::view::{
    ActionVerdict, SectionView, Switchboard, SwitchboardAction, SwitchboardModel,
    UNMEASURED_READING,
};

/// A screen showing `model` on the Tasks section.
///
/// The surface opens on Resources — what the machine is doing is the question
/// a monitor is opened to answer — so a suite about the task list says so
/// rather than relying on whichever section happens to lead the rail.
fn on_tasks(model: &SwitchboardModel) -> Switchboard {
    let mut sb = Switchboard::new(model);
    sb.select_section(crate::view::Section::Tasks);
    sb
}

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
    let sb = on_tasks(&model());
    assert_eq!(
        sb.tasks.selected,
        Some(task_id(0)),
        "a table with something to show always has a subject"
    );
    assert_eq!(sb.tasks.rail.len(), 7, "so its commands are offered");
}

#[test]
fn an_empty_table_selects_nothing_and_offers_no_command() {
    let sb = on_tasks(&SwitchboardModel::new("Switchboard"));
    assert_eq!(sb.tasks.selected, None);
    assert!(
        sb.tasks.rail.is_empty(),
        "with no subject the rail offers no command"
    );
}

#[test]
fn choosing_a_row_gives_the_rail_its_whole_command_set() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    let b = bounds();
    select_task_row(&mut sb, b, Scale::ONE, &theme, 1);

    assert_eq!(sb.tasks.selected, Some(task_id(1)));
    assert_eq!(sb.tasks.rail.len(), 7, "every command keeps its slot");
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
        (6, TaskControl::ForceQuit),
    ];
    for (slot, control) in wanted {
        let mut sb = on_tasks(&one_task(all_ready()));
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
    let mut sb = on_tasks(&one_task(TaskAuthority {
        switch: ActionVerdict::Ready,
        ..TaskAuthority::default()
    }));
    let actions = invoke_rail(&mut sb, b, &theme, 0, 6);
    assert!(
        actions.is_empty(),
        "a command the caller may not use must not activate"
    );
    assert_eq!(sb.tasks.rail.len(), 7, "it keeps its slot regardless");
    assert_eq!(
        sb.tasks.rail.items()[6].state().disposition(),
        ControlDisposition::DeniedByAuthority,
        "and wears the Authority Mark"
    );
}

#[test]
fn a_command_the_state_rules_out_is_plainly_disabled() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = on_tasks(&one_task(TaskAuthority {
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
    let mut sb = on_tasks(&one_task(all_ready()));
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
    let mut sb = on_tasks(&model());
    select_task_row(&mut sb, bounds(), Scale::ONE, &theme, 0);
    assert_eq!(sb.tasks.rail.items()[6].role(), ControlRole::Destructive);
    for slot in 0..6 {
        assert_eq!(sb.tasks.rail.items()[slot].role(), ControlRole::Neutral);
    }
}

#[test]
fn the_selection_follows_the_task_when_a_re_sort_moves_it() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    let b = bounds();
    select_task_row(&mut sb, b, Scale::ONE, &theme, 2);
    let chosen = sb.tasks.selected.expect("a selected task");

    // Reverse the name order; the chosen task is now somewhere else.
    sb.tasks
        .header
        .adopt_sort(Some((0, super::SortOrder::Ascending)));
    sb.tasks.arrange(&mut Sweep::adopting(&mut damage::sink()));
    sb.tasks
        .header
        .adopt_sort(Some((0, super::SortOrder::Descending)));
    sb.tasks.arrange(&mut Sweep::adopting(&mut damage::sink()));

    assert_eq!(
        sb.tasks.selected,
        Some(chosen),
        "the selection names the task, never the row it sat in"
    );
    assert_eq!(sb.tasks.rail.len(), 7, "so its commands are still offered");
}

#[test]
fn a_sample_that_drops_the_selected_task_drops_its_commands_too() {
    let theme = Theme::dark();
    let mut sb = on_tasks(&model());
    let b = bounds();
    select_task_row(&mut sb, b, Scale::ONE, &theme, 0);
    assert!(sb.tasks.selected.is_some());

    // The task has gone, so nothing is left for the commands to act on.
    sb.tasks
        .adopt(&table_model(&[]), &mut Sweep::adopting(&mut damage::sink()));

    assert_eq!(sb.tasks.selected, None);
    assert!(
        sb.tasks.rail.is_empty(),
        "commands with no visible subject are withdrawn, not left dangling"
    );
}

/// Walk the content cursor down onto rail slot `slot` from wherever it is.
///
/// The rail's stops follow the rows, so a reader reaches a command by
/// carrying on down past the last row exactly as the cursor does.
fn walk_to_rail_slot(sb: &mut Switchboard, slot: usize) {
    let target = sb.tasks.rail_focus_index(slot);
    while sb.active().content_focus() < target {
        assert_eq!(key(sb, Key::Named(NamedKey::Down)), None);
    }
    assert_eq!(sb.active().content_focus(), target);
}

#[test]
fn the_keyboard_selects_a_row_then_reaches_its_commands() {
    let mut sb = on_tasks(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(
        key(&mut sb, Key::Named(NamedKey::Enter)),
        None,
        "choosing a row reports nothing of its own"
    );
    assert_eq!(sb.tasks.selected, Some(task_id(0)));

    walk_to_rail_slot(&mut sb, 0);
    assert_eq!(
        key(&mut sb, Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Task {
            index: 0,
            control: TaskControl::Switch
        })
    );
}

#[test]
fn a_rail_command_takes_the_focus_ring_from_the_rows() {
    let mut sb = on_tasks(&model());
    focus_task_row(&mut sb, 0);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
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
/// shows it: what it is called, which principal owns it, the condition it
/// is in, and the two figures the census and sort tests read.
struct RowSpec {
    name: &'static str,
    uid: u32,
    recovery: RecoveryState,
    cpu: Option<u16>,
    memory: Option<u64>,
}

/// One [`RowSpec`], so a fixture reads as a table of rows rather than as a
/// column of struct literals.
const fn row(
    name: &'static str,
    uid: u32,
    recovery: RecoveryState,
    cpu: Option<u16>,
    memory: Option<u64>,
) -> RowSpec {
    RowSpec {
        name,
        uid,
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
            owner: TaskOwner::new(spec.uid),
            core: Some(0),
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
        row("alpha", 1000, RecoveryState::None, Some(300), Some(2048)),
        row("Beta", 1000, RecoveryState::None, Some(100), Some(1024)),
        row("gamma", 1000, RecoveryState::Hung, Some(200), None),
        row("delta", 0, RecoveryState::None, None, Some(4096)),
        row("epsilon", 0, RecoveryState::None, Some(50), Some(512)),
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
        assert_eq!(key(sb, Key::Named(NamedKey::Down)), None);
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
        assert_eq!(key(sb, Key::Named(NamedKey::Down)), None);
    }
    assert_eq!(sb.active().content_focus(), target);
}

/// Walk the action cursor to `index` within the focused stop.
fn walk_action_to(sb: &mut Switchboard, index: usize) {
    for _ in 0..index {
        assert_eq!(key(sb, Key::Named(NamedKey::Right)), None);
    }
    assert_eq!(sb.active().row_action(), index);
}

/// Sort by the column at `column`, returning the shown names.
fn sorted_by(model: &SwitchboardModel, column: usize) -> alloc::vec::Vec<alloc::string::String> {
    let mut sb = on_tasks(model);
    focus_header_stop(&mut sb, 0);
    walk_action_to(&mut sb, column);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
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
        alloc::vec!["delta", "epsilon", "alpha", "Beta", "gamma"],
        "Owner sorts by uid, stably within an owner"
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
    let mut sb = on_tasks(&mixed_model());
    focus_header_stop(&mut sb, 0);
    walk_action_to(&mut sb, 4);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    let ascending = shown(&sb);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
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
        row("d", 1000, RecoveryState::None, None, None),
        row("c", 1000, RecoveryState::None, None, None),
        row("b", 1000, RecoveryState::None, None, None),
        row("a", 1000, RecoveryState::None, None, None),
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
    let sb = on_tasks(&m);
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
    let mut sb = on_tasks(&m);
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);

    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
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
    let sb = on_tasks(&m);
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
        let mut sb = on_tasks(&m);
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
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
fn the_network_column_renders_an_explicit_unmeasured_mark() {
    let sb = on_tasks(&mixed_model());
    let cells = sb.tasks.entries[0].row.cells();
    // Per-task network has no interface at all, so the column says so —
    // disabled, never as a small figure a reader would take for a rate.
    assert_eq!(
        cells[COL_NETWORK],
        TableCell::new(UNMEASURED_READING)
            .with_align(CellAlign::Trailing)
            .with_state(ControlState::disabled())
    );
}

#[test]
fn the_core_column_reads_the_cpu_the_scheduler_placed_the_task_on() {
    let sb = on_tasks(&mixed_model());
    // Core is a real reading off the process record, so it is a figure
    // rather than the mark the Network column carries.
    assert_eq!(
        sb.tasks.entries[0].row.cells()[COL_CORE],
        TableCell::new("0").with_align(CellAlign::Trailing)
    );
}

#[test]
fn an_unmeasured_figure_never_renders_as_a_zero() {
    let sb = on_tasks(&mixed_model());
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
    let mut sb = on_tasks(&mixed_model());
    assert_eq!(sb.tasks.count, StatusPill::new("5 of 5 shown"));
    // The readout is re-derived with the rows, so a smaller sample restates
    // both figures rather than quoting a count the table is not showing.
    sb.tasks.adopt(
        &table_model(&[row("solo", 1000, RecoveryState::None, None, None)]),
        &mut Sweep::adopting(&mut damage::sink()),
    );
    assert_eq!(sb.tasks.count, StatusPill::new("1 of 1 shown"));
}

#[test]
fn the_grouping_control_arranges_the_same_rows() {
    let mut sb = on_tasks(&mixed_model());
    focus_footer_stop(&mut sb, 0);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    assert!(sb.tasks.grouping.is_expanded(), "the choices open");
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    assert_eq!(sb.tasks.grouping.selected_text(), Some("By owner"));
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
    let mut sb = on_tasks(&m);
    sb.tasks.grouping.set_selected(2);
    sb.tasks
        .adopt(&m, &mut Sweep::adopting(&mut damage::sink()));
    assert_eq!(
        shown(&sb).first().map(alloc::string::String::as_str),
        Some("epsilon"),
        "the working row leads its group"
    );
}

#[test]
fn auto_refresh_off_holds_the_rows_the_reader_was_reading() {
    let mut sb = on_tasks(&mixed_model());
    focus_footer_stop(&mut sb, 1);
    assert!(sb.tasks.auto_refresh.is_on(), "refreshing by default");
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    assert!(!sb.tasks.auto_refresh.is_on(), "the toggle turns it off");

    let mut later = mixed_model();
    later.tasks.truncate(1);
    sb.tasks
        .adopt(&later, &mut Sweep::adopting(&mut damage::sink()));
    assert_eq!(sb.tasks.entries.len(), 5, "a paused table holds its rows");

    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    assert!(sb.tasks.auto_refresh.is_on());
    sb.tasks
        .adopt(&later, &mut Sweep::adopting(&mut damage::sink()));
    assert_eq!(sb.tasks.entries.len(), 1, "resuming takes the new sample");
}

#[test]
fn the_cursor_reaches_every_header_rail_and_footer_control() {
    let mut sb = on_tasks(&mixed_model());
    let span = sb.active().focus_span();
    assert_eq!(
        span,
        1 + 5 + 7 + 2,
        "one header stop, five rows, seven commands, two footer"
    );

    let mut rows = alloc::vec::Vec::new();
    for stop in 0..span {
        if stop > 0 {
            assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
        }
        assert_eq!(sb.active().content_focus(), stop);
        rows.push(sb.active().focus_row(stop));
    }
    // Only the row band names a row to scroll to: the header's stops, the
    // rail's anchored commands and the footer's controls all sit outside the
    // scrolling list.
    let mut expected = alloc::vec![None];
    expected.extend((0..5).map(Some));
    expected.extend(core::iter::repeat_n(None, 7 + 2));
    assert_eq!(rows, expected);
}

#[test]
fn each_header_and_footer_control_takes_the_focus_ring_in_turn() {
    let mut sb = on_tasks(&mixed_model());
    let resting = on_tasks(&mixed_model());
    // Step onto a row, then back: the ring lands where the cursor is and
    // leaves what it left.
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    let on_row = sb.tasks.header.clone();
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Up)), None);
    assert_ne!(
        sb.tasks.header, on_row,
        "the column headings take the ring back from the row"
    );

    let mut sb = on_tasks(&mixed_model());
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
        pointer(sb, b, Scale::ONE, theme, &moved(x, y)),
        None,
        "moving onto a row asks for nothing"
    );
}

#[test]
fn a_refresh_keeps_the_hover_on_the_row_under_the_pointer() {
    let m = model();
    let mut sb = on_tasks(&m);
    let (b, theme) = (bounds(), Theme::dark());
    hover_row(&mut sb, b, &theme, 2);
    assert_eq!(row_pointer(&sb, 2), PointerState::Hover);

    let _ = refresh(&mut sb, &m);

    assert_eq!(
        row_pointer(&sb, 2),
        PointerState::Hover,
        "a refresh moves neither the pointer nor the slot it is over"
    );
}

#[test]
fn a_refresh_drops_a_press_begun_on_a_row() {
    let m = model();
    let mut sb = on_tasks(&m);
    let (b, theme) = (bounds(), Theme::dark());
    hover_row(&mut sb, b, &theme, 2);
    assert_eq!(pointer(&mut sb, b, Scale::ONE, &theme, &PRESS), None);
    assert_eq!(row_pointer(&sb, 2), PointerState::Pressed);

    let _ = refresh(&mut sb, &m);

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
    let mut sb = on_tasks(&m);
    let (b, theme) = (bounds(), Theme::dark());
    hover_row(&mut sb, b, &theme, 2);

    let mut shorter = m.clone();
    shorter.tasks.truncate(1);
    let _ = refresh(&mut sb, &shorter);

    assert_eq!(sb.tasks.entries.len(), 1);
    assert_eq!(
        row_pointer(&sb, 0),
        PointerState::None,
        "the pointer is over no row once the slot it was over has gone"
    );
}

/// Press rail command `index`, publishing `published` before the release when
/// one is given, and report what the release produced.
fn press_rail_command(
    sb: &mut Switchboard,
    b: Rect,
    theme: &Theme,
    index: usize,
    published: Option<&SwitchboardModel>,
) -> Option<SwitchboardAction> {
    let (x, y) = centre(task_rail_rects(sb, b, Scale::ONE, theme)[index]);
    assert_eq!(pointer(sb, b, Scale::ONE, theme, &moved(x, y)), None);
    assert_eq!(pointer(sb, b, Scale::ONE, theme, &PRESS), None);
    if let Some(model) = published {
        let _ = refresh(sb, model);
    }
    pointer(sb, b, Scale::ONE, theme, &RELEASE)
}

#[test]
fn a_refresh_does_not_swallow_a_press_begun_on_a_rail_command() {
    let m = model();
    let (b, theme) = (bounds(), Theme::dark());

    let mut undisturbed = on_tasks(&m);
    select_task_row(&mut undisturbed, b, Scale::ONE, &theme, 0);
    let expected = press_rail_command(&mut undisturbed, b, &theme, 0, None);
    assert!(
        expected.is_some(),
        "a press and release on a rail command commands the selected task"
    );

    let mut refreshed = on_tasks(&m);
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
        let mut sb = on_tasks(&m);
        let b = bounds();
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);
        let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
        let frame = resolve_section_frame(layout.content, sb.tasks.anatomy(), Scale::ONE, &theme);
        assert!(has_ink(&surface, layout.content), "the table draws");
        // The section claims no header band of its own — the column headings
        // are pinned inside the table — so the band has no height to draw in.
        assert_eq!(frame.header.height, 0, "the header band claims no room");
        assert!(has_ink(&surface, frame.footer), "and its footer band draws");
    }
}

// --- The row's identity icon -----------------------------------------------

/// A one-task model whose sole task was launched from `bundle`.
fn one_task_from(bundle: Option<&str>) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    m.tasks.push(TaskSummary {
        proc_id: task_id(0),
        name: alloc::string::String::from("terminal"),
        bundle: bundle.map(alloc::string::String::from),
        ..TaskSummary::default()
    });
    m
}

#[test]
fn a_row_launched_from_a_bundle_names_the_application_icon() {
    // Every row used to name the executable class, so a reader could not tell
    // one process from another at a glance.
    let launched = on_tasks(&one_task_from(Some("/System/Applications/terminal.app")));
    let unattested = on_tasks(&one_task_from(None));

    let leading = |sb: &Switchboard| {
        sb.tasks.entries[0]
            .row
            .cells()
            .iter()
            .find_map(TableCell::icon)
    };
    assert_eq!(leading(&launched), Some(IconKind::AppBundle));
    assert_eq!(
        leading(&unattested),
        Some(IconKind::Executable),
        "a process nothing attests keeps the executable class"
    );
}

#[test]
fn a_rows_bundle_is_carried_to_the_paint_that_resolves_its_picture() {
    // The row's own picture is resolved from the bundle at draw time, so the
    // entry has to keep it: without this the render could only ask for a kind.
    let sb = on_tasks(&one_task_from(Some("/Apps/Terminal.app")));
    assert_eq!(
        sb.tasks.entries[0].bundle.as_deref(),
        Some("/Apps/Terminal.app")
    );
}

#[test]
fn a_row_draws_its_icon_whether_or_not_a_cache_answers() {
    // `NoArtwork` holds no cache, so the row falls back to the inline glyph
    // arithmetic. Either way the leading gutter must carry ink: a row with no
    // picture at all would be the blank slot the glyph tier exists to prevent.
    let theme = Theme::dark();
    let sb = on_tasks(&one_task_from(Some("/Apps/Terminal.app")));
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    let mut sb = sb;
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut NoArtwork);

    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let item = info.item_rect(0);
    let side = tairix_controls::TableRow::icon_side(item, Scale::ONE, &theme);
    assert!(side > 0, "the row reserves a slot for its icon");
    let gutter = Rect::new(item.left(), item.top(), side, item.height);
    assert!(has_ink(&surface, gutter), "the icon slot draws something");
}

/// An artwork lookup that records every request it is asked for and answers
/// none, so a test can see what the render *asked* for rather than only what
/// it drew.
#[derive(Default)]
struct RecordingArtwork {
    asked: alloc::vec::Vec<(IconKind, u32)>,
}

impl tairix_icon::IconArtwork for RecordingArtwork {
    fn artwork(
        &mut self,
        request: tairix_icon::IconRequest<'_>,
        side: u32,
    ) -> Option<tairix_icon::IconPicture<'_>> {
        self.asked.push((request.icon_kind(), side));
        None
    }
}

#[test]
fn every_drawn_row_asks_the_cache_for_its_own_picture() {
    // The lookup is threaded to the render so no draw site rasterises a glyph
    // itself. A render that never asked would still *look* right — it would
    // fall back to the inline path — and would keep the defect the cache was
    // added to close, so what matters is that it asks.
    let theme = Theme::dark();
    let mut sb = on_tasks(&one_task_from(Some("/Apps/Terminal.app")));
    let b = bounds();
    let mut surface = Surface::new(b.width, b.height).expect("surface");
    let mut artwork = RecordingArtwork::default();
    sb.render(&mut surface, b, Scale::ONE, &theme, font(), &mut artwork);

    let layout = Switchboard::compute_layout(b, Scale::ONE, &theme);
    let info = sb.list_info(&layout, Scale::ONE, &theme);
    let side = tairix_controls::TableRow::icon_side(info.item_rect(0), Scale::ONE, &theme);
    assert!(
        artwork.asked.contains(&(IconKind::AppBundle, side)),
        "the row must ask for its own picture at the side it draws at: {:?}",
        artwork.asked
    );
}
