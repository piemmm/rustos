//! Unit tests for the overview panel's window lifecycle and effect
//! application, driven entirely through the recording host.

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport, SwitchboardRequest};
use tairix_abi::sysinfo::ProcessState;
use tairix_abi::{Errno, SchedPriority, Signal};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::InputEvent;
use tairix_theme::Theme;

use super::{refusal_notice, Panel, PANEL_TITLE};
use crate::activities::{Activities, Member};
use crate::derive::{derive_summary, Hysteresis, CPU_PRESSURE_ENTER_PERMILLE};
use crate::model::{build_model, PanelModel, RollingMeters, SessionReport};
use crate::sample::Sample;
use crate::test_host::{
    process_summary, process_summary_with, sample_with, RecordingHost, DEFAULT_UID, NO_AUTHORITY,
    PROC_CONTROL_AUTHORITY,
};
use crate::view::{
    ActivityControl, PressureControl, RecoveryControl, Section, SwitchboardAction, TaskControl,
};
use crate::wait::required_members;

/// This service's own scheduler task id in these tests.
const OWN_PID: u64 = 4242;

/// A recovery-bearing model: one stopped task the panel can act on.
fn stopped_model(pid: u64, can_force: bool) -> PanelModel {
    let sample = sample_with(alloc::vec![process_summary(
        pid,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let authority = if can_force {
        &PROC_CONTROL_AUTHORITY
    } else {
        &NO_AUTHORITY
    };
    build_model(
        PANEL_TITLE,
        &sample,
        &SessionReport::HEALTHY,
        &RollingMeters::new(),
        authority,
        &Activities::new(),
        None,
    )
}

/// A model with one live task the panel can switch to.
fn task_model(pid: u64) -> PanelModel {
    let sample = sample_with(alloc::vec![process_summary(
        pid,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    build_model(
        PANEL_TITLE,
        &sample,
        &SessionReport::HEALTHY,
        &RollingMeters::new(),
        &NO_AUTHORITY,
        &Activities::new(),
        None,
    )
}

/// Enough sampled processes to overflow the task list at [`WINDOW`], so a
/// test can scroll it. The first row names `first_pid`, so which reading a
/// row came from is observable through the request activating it produces.
fn busy_model(first_pid: u64) -> PanelModel {
    let processes = (0..40)
        .map(|i| process_summary(first_pid + i, ProcessState::Running, b"task", None))
        .collect();
    build_model(
        PANEL_TITLE,
        &sample_with(processes),
        &SessionReport::HEALTHY,
        &RollingMeters::new(),
        &NO_AUTHORITY,
        &Activities::new(),
        None,
    )
}

/// A model with nothing in it.
fn empty_model() -> PanelModel {
    build_model(
        PANEL_TITLE,
        &Sample::default(),
        &SessionReport::HEALTHY,
        &RollingMeters::new(),
        &NO_AUTHORITY,
        &Activities::new(),
        None,
    )
}

/// A model with a single CPU pressure card, culprit `pid`, controllable as
/// the fixture's own uid with no capability needed.
fn pressured_model(pid: u64) -> PanelModel {
    let sample = Sample {
        cpu_busy_permille: Some(CPU_PRESSURE_ENTER_PERMILLE),
        processes: alloc::vec![process_summary_with(
            pid,
            ProcessState::Running,
            b"hog",
            Some(900),
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        )],
        ..Sample::default()
    };
    let mut hysteresis = Hysteresis::new();
    let _ = derive_summary(&sample, &mut hysteresis);
    let mut meters = RollingMeters::new();
    meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
    build_model(
        PANEL_TITLE,
        &sample,
        &SessionReport::HEALTHY,
        &meters,
        &NO_AUTHORITY,
        &Activities::new(),
        Some(DEFAULT_UID),
    )
}

/// A model with one activity grouping two live, controllable members.
fn activity_model(first_pid: u64, second_pid: u64) -> PanelModel {
    let sample = sample_with(alloc::vec![
        process_summary_with(
            first_pid,
            ProcessState::Running,
            b"a",
            None,
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        ),
        process_summary_with(
            second_pid,
            ProcessState::Running,
            b"b",
            None,
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        ),
    ]);
    let mut activities = Activities::new();
    activities
        .create(Member {
            proc_id: sample.processes[0].proc_id,
            pid: first_pid,
            name: alloc::string::String::from("a"),
        })
        .expect("room");
    activities
        .assign(
            0,
            Member {
                proc_id: sample.processes[1].proc_id,
                pid: second_pid,
                name: alloc::string::String::from("b"),
            },
        )
        .expect("room");
    build_model(
        PANEL_TITLE,
        &sample,
        &SessionReport::HEALTHY,
        &RollingMeters::new(),
        &NO_AUTHORITY,
        &activities,
        Some(DEFAULT_UID),
    )
}

fn open(panel: &mut Panel, host: &mut RecordingHost, section: CommandSection) {
    panel.open_section(host, section);
    panel.flush(host);
}

/// The window rectangle the scrolling test lays the composition out in.
const WINDOW: Rect = Rect::new(0, 0, 600, 400);

/// Scroll the open panel's active section down by `lines`, the way the
/// compositor delivers a wheel event: resolved against the real window
/// geometry, theme metrics, and font metrics, so the offset a test reads
/// back is the one the user would have.
fn wheel(panel: &mut Panel, lines: i32) -> u64 {
    let view = panel.view_mut().expect("the panel is open");
    view.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: lines },
        WINDOW,
        Scale::ONE,
        &Theme::dark(),
        BitmapFont::console(),
    );
    view.scroll_offset()
}

/// Feed a bare pointer move to the open panel, the way the run loop
/// delivers a `WindowEvent::Pointer` `Moved` action.
fn pointer_move(panel: &mut Panel, to: Point) {
    let view = panel.view_mut().expect("the panel is open");
    view.on_pointer(
        &InputEvent::PointerMoved { to },
        WINDOW,
        Scale::ONE,
        &Theme::dark(),
        BitmapFont::console(),
    );
}

#[test]
fn a_fresh_panel_is_closed_and_shows_nothing() {
    let panel = Panel::new(OWN_PID, empty_model());
    assert!(!panel.is_open());
    assert_eq!(panel.section(), None);
    assert_eq!(panel.session_report(), &SessionReport::HEALTHY);
}

#[test]
fn opening_shows_the_requested_section_and_arms_the_window_source() {
    for (command, expected) in [
        (CommandSection::Tasks, Section::Tasks),
        (CommandSection::Jobs, Section::Jobs),
        (CommandSection::Pressure, Section::Pressure),
        (CommandSection::Activities, Section::Activities),
        (CommandSection::Recovery, Section::Recovery),
        (CommandSection::System, Section::System),
    ] {
        let mut host = RecordingHost::new();
        let mut panel = Panel::new(OWN_PID, empty_model());
        open(&mut panel, &mut host, command);
        assert!(panel.is_open());
        assert_eq!(panel.section(), Some(expected));
        assert_eq!(host.opened, 1);
        assert_eq!(host.presents, 1);
        assert_eq!(host.armed(), required_members(true));
        assert!(host.refusals.is_empty());
    }
}

#[test]
fn a_second_open_raises_the_one_window_rather_than_stacking_another() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());

    open(&mut panel, &mut host, CommandSection::Tasks);
    open(&mut panel, &mut host, CommandSection::Recovery);

    assert_eq!(host.opened, 1);
    assert_eq!(
        host.requests,
        alloc::vec![SwitchboardRequest::ActivateOwner { owner: OWN_PID }]
    );
    assert_eq!(panel.section(), Some(Section::Recovery));
    assert_eq!(host.armed(), required_members(true));
}

#[test]
fn a_refused_raise_is_stated_and_the_section_still_changes() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    host.request_refusal = Some(Errno::NotFound);

    open(&mut panel, &mut host, CommandSection::Jobs);

    assert_eq!(
        host.refused_actions(),
        alloc::vec!["raise the overview window"]
    );
    assert_eq!(panel.section(), Some(Section::Jobs));
    assert!(panel.is_open());
}

#[test]
fn a_refused_window_create_leaves_the_panel_closed_and_states_why() {
    let mut host = RecordingHost::new();
    host.open_refusal = Some(Errno::PermissionDenied);
    let mut panel = Panel::new(OWN_PID, empty_model());

    open(&mut panel, &mut host, CommandSection::Tasks);

    assert!(!panel.is_open());
    assert_eq!(host.opened, 0);
    assert_eq!(host.presents, 0);
    assert_eq!(host.armed(), required_members(false));
    assert_eq!(
        host.refusals,
        alloc::vec![(
            alloc::string::String::from("open the overview window"),
            Errno::PermissionDenied
        )]
    );
}

#[test]
fn closing_returns_to_headless_sampling() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);

    // The window manager's close request drives the panel's own close.
    panel.close(&mut host);

    assert!(!panel.is_open());
    assert_eq!(panel.section(), None);
    assert_eq!(host.closed, 1);
    assert_eq!(host.armed(), required_members(false));

    // A later model change draws nothing: there is no window to draw into,
    // and the panel never re-opens one on its own.
    let presents = host.presents;
    panel.refresh(task_model(10));
    assert_eq!(host.presents, presents);
    assert!(!panel.is_open());
}

#[test]
fn closing_an_already_closed_panel_does_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    panel.close(&mut host);
    assert_eq!(host.closed, 0);
}

#[test]
fn a_seat_report_is_stored_for_the_callers_next_rebuild() {
    let host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    let report = SeatReport::new(3, &[11, 12]).expect("valid report");

    panel.set_seat_report(report);

    assert_eq!(panel.session_report().seat.owners(), &[11, 12]);
    assert_eq!(panel.session_report().seat.total(), 3);
    assert!(!panel.is_open());
    assert_eq!(host.opened, 0, "a seat report opens no window");
}

#[test]
fn refreshing_with_an_unchanged_model_draws_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    panel.refresh(empty_model());

    assert_eq!(host.presents, presents);
}

#[test]
fn refreshing_with_a_changed_model_redraws_and_keeps_the_section() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Recovery);
    let presents = host.presents;

    panel.refresh(task_model(10));
    panel.flush(&mut host);

    assert_eq!(host.presents, presents + 1);
    assert_eq!(panel.section(), Some(Section::Recovery));
}

#[test]
fn a_refresh_keeps_the_users_place_and_shows_the_new_reading() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    assert_eq!(wheel(&mut panel, 4), 4);

    panel.refresh(busy_model(200));
    panel.flush(&mut host);

    assert_eq!(panel.section(), Some(Section::Tasks));
    assert_eq!(
        panel.view_mut().expect("still open").scroll_offset(),
        4,
        "a live refresh must not snap the list back to the top"
    );

    // The rows really are the new reading's: activating the first one names
    // the process the refreshed sample put there, not the one it replaced.
    panel.act(
        &mut host,
        SwitchboardAction::Task {
            index: 0,
            control: TaskControl::Switch,
        },
        &NO_AUTHORITY,
    );
    assert_eq!(
        host.requests,
        alloc::vec![SwitchboardRequest::ActivateOwner { owner: 200 }]
    );
}

#[test]
fn a_task_action_asks_the_session_to_activate_that_owner() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, task_model(10));
    open(&mut panel, &mut host, CommandSection::Tasks);

    panel.act(
        &mut host,
        SwitchboardAction::Task {
            index: 0,
            control: TaskControl::Switch,
        },
        &NO_AUTHORITY,
    );

    assert_eq!(
        host.requests,
        alloc::vec![SwitchboardRequest::ActivateOwner { owner: 10 }]
    );
    assert!(host.signals.is_empty());
}

#[test]
fn a_restart_action_asks_the_session_to_relaunch_that_owner() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, stopped_model(7, false));

    panel.act(
        &mut host,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Restart,
        },
        &NO_AUTHORITY,
    );

    assert_eq!(
        host.requests,
        alloc::vec![SwitchboardRequest::RestartOwner { owner: 7 }]
    );
}

#[test]
fn a_force_action_signals_the_owner_when_authorised() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, stopped_model(7, true));

    panel.act(
        &mut host,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force,
        },
        &PROC_CONTROL_AUTHORITY,
    );

    assert_eq!(host.signals, alloc::vec![(7, Signal::Kill)]);
    assert!(host.refusals.is_empty());
}

#[test]
fn a_force_action_is_never_attempted_without_the_capability() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, stopped_model(7, false));

    panel.act(
        &mut host,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force,
        },
        &NO_AUTHORITY,
    );

    assert!(host.signals.is_empty());
    assert!(host.requests.is_empty());
}

#[test]
fn a_force_action_on_an_id_beyond_the_syscall_width_is_refused_not_truncated() {
    let beyond = u64::try_from(i32::MAX).expect("i32::MAX fits a u64") + 1;
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, stopped_model(beyond, true));

    panel.act(
        &mut host,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force,
        },
        &PROC_CONTROL_AUTHORITY,
    );

    assert!(host.signals.is_empty());
    assert_eq!(
        host.refusals,
        alloc::vec![(
            alloc::string::String::from("force that task to quit"),
            Errno::OutOfRange
        )]
    );
}

#[test]
fn a_refused_action_is_stated_and_the_panel_stays_open() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, task_model(10));
    open(&mut panel, &mut host, CommandSection::Tasks);
    host.request_refusal = Some(Errno::NotFound);

    panel.act(
        &mut host,
        SwitchboardAction::Task {
            index: 0,
            control: TaskControl::Switch,
        },
        &NO_AUTHORITY,
    );

    assert_eq!(
        host.refused_actions(),
        alloc::vec!["switch to that task's window"]
    );
    assert!(panel.is_open());
}

#[test]
fn a_refused_present_is_stated_and_the_panel_stays_open() {
    let mut host = RecordingHost::new();
    host.present_refusal = Some(Errno::NoSpace);
    let mut panel = Panel::new(OWN_PID, empty_model());

    open(&mut panel, &mut host, CommandSection::Tasks);

    assert!(panel.is_open());
    assert_eq!(host.presents, 0);
    assert_eq!(
        host.refused_actions(),
        alloc::vec!["redraw the overview window"]
    );
}

#[test]
fn a_scroll_action_changes_nothing_outside_the_panel() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, task_model(10));
    open(&mut panel, &mut host, CommandSection::Tasks);

    panel.act(
        &mut host,
        SwitchboardAction::Scrolled { offset: 2 },
        &NO_AUTHORITY,
    );

    assert!(host.requests.is_empty());
    assert!(host.signals.is_empty());
    assert!(panel.is_open());
}

#[test]
fn a_lower_priority_refusal_is_stated_and_the_panel_stays_open() {
    let mut host = RecordingHost::new();
    host.lower_refusal = Some(Errno::PermissionDenied);
    let mut panel = Panel::new(OWN_PID, pressured_model(50));
    open(&mut panel, &mut host, CommandSection::Pressure);

    panel.act(
        &mut host,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::LowerPriority,
        },
        &NO_AUTHORITY,
    );

    assert_eq!(host.lowered, alloc::vec![50]);
    assert_eq!(host.refused_actions(), alloc::vec!["lower priority"]);
    assert!(panel.is_open());
}

#[test]
fn a_lower_priority_action_lowers_the_culprit_when_ready() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, pressured_model(50));
    open(&mut panel, &mut host, CommandSection::Pressure);

    panel.act(
        &mut host,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::LowerPriority,
        },
        &NO_AUTHORITY,
    );

    assert_eq!(host.lowered, alloc::vec![50]);
    assert!(host.refusals.is_empty());
}

#[test]
fn a_signal_many_sweep_continues_past_an_individual_refusal() {
    let mut host = RecordingHost::new();
    host.signal_refusal = Some(Errno::PermissionDenied);
    let mut panel = Panel::new(OWN_PID, activity_model(10, 20));
    open(&mut panel, &mut host, CommandSection::Activities);

    panel.act(
        &mut host,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Pause,
        },
        &NO_AUTHORITY,
    );

    // Both members were still signalled even though each one refused: one
    // member's refusal never aborts the sweep.
    assert_eq!(
        host.signals,
        alloc::vec![(10, Signal::Stop), (20, Signal::Stop)]
    );
    assert_eq!(
        host.refused_actions(),
        alloc::vec!["pause that activity", "pause that activity"]
    );
}

#[test]
fn an_activate_owners_sweep_raises_windows_back_to_front() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, activity_model(10, 20));
    open(&mut panel, &mut host, CommandSection::Activities);

    panel.act(
        &mut host,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Switch,
        },
        &NO_AUTHORITY,
    );

    // Raising the last member first, and the first member last, leaves the
    // group's first member frontmost.
    assert_eq!(
        host.requests,
        alloc::vec![
            SwitchboardRequest::ActivateOwner { owner: 20 },
            SwitchboardRequest::ActivateOwner { owner: 10 },
        ]
    );
}

#[test]
fn a_refusal_notice_names_the_action_and_the_refusal() {
    assert_eq!(
        refusal_notice("restart that task", Errno::PermissionDenied),
        "switchboard: could not restart that task (permission denied)\n"
    );
}

#[test]
fn the_first_flush_after_opening_always_presents() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());

    panel.open_section(&mut host, CommandSection::Tasks);
    panel.flush(&mut host);

    assert_eq!(host.presents, 1);
}

#[test]
fn flushing_a_closed_panel_presents_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());

    panel.flush(&mut host);

    assert_eq!(host.presents, 0);
}

#[test]
fn flushing_an_unchanged_panel_presents_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    panel.flush(&mut host);

    assert_eq!(host.presents, presents);
}

#[test]
fn repeated_unchanged_flushes_present_only_once_in_total() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    panel.open_section(&mut host, CommandSection::Tasks);

    // The first flush presents the initial paint; every later one, with
    // nothing having changed in between, must not.
    for _ in 0..5 {
        panel.flush(&mut host);
    }

    assert_eq!(host.presents, 1);
}

#[test]
fn a_pointer_move_that_reaches_the_composition_unchanged_presents_nothing() {
    // Regression test for the reported defect: a pointer that reports the
    // same position again crosses no control and leaves the composition
    // byte-for-byte what it was, so the panel that used to redraw on every
    // delivered event no longer may.
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    pointer_move(&mut panel, Point::new(5, 5));
    panel.flush(&mut host);
    let presents = host.presents;

    pointer_move(&mut panel, Point::new(5, 5));
    panel.flush(&mut host);

    assert_eq!(host.presents, presents);
}

#[test]
fn a_scroll_that_changes_the_composition_presents_exactly_once() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    assert_eq!(wheel(&mut panel, 4), 4);
    panel.flush(&mut host);

    assert_eq!(host.presents, presents + 1);
}

#[test]
fn a_window_resize_alone_presents_again() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    host.bounds.2 += 100;
    panel.flush(&mut host);

    assert_eq!(host.presents, presents + 1);
}

#[test]
fn a_theme_change_alone_presents_again() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    host.theme_id = 2;
    panel.flush(&mut host);

    assert_eq!(host.presents, presents + 1);
}

#[test]
fn a_scale_change_alone_presents_again() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    host.scale_percent = 150;
    panel.flush(&mut host);

    assert_eq!(host.presents, presents + 1);
}

#[test]
fn a_refused_present_is_reported_once_and_not_retried_by_an_unchanged_flush() {
    let mut host = RecordingHost::new();
    host.present_refusal = Some(Errno::PermissionDenied);
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    // `open` helper already flushed, but it failed and reported.
    assert_eq!(
        host.refused_actions(),
        alloc::vec!["redraw the overview window"]
    );

    // A second flush with nothing changed does not retry: the record was
    // updated even though the present was refused.
    panel.flush(&mut host);
    assert_eq!(
        host.refused_actions(),
        alloc::vec!["redraw the overview window"]
    );

    // An actual change compares unequal again and retries.
    host.theme_id = 2;
    panel.flush(&mut host);
    assert_eq!(
        host.refused_actions(),
        alloc::vec!["redraw the overview window", "redraw the overview window"]
    );
}

#[test]
fn refreshing_with_an_unchanged_model_then_flushing_presents_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    panel.refresh(empty_model());
    panel.flush(&mut host);

    assert_eq!(host.presents, presents);
}
