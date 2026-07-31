//! Unit tests for the overview panel's window lifecycle and effect
//! application, driven entirely through the recording host.

use tairix_abi::switchboard_ipc::{
    CommandSection, SeatReport, SwitchboardCommand, SwitchboardRequest,
};
use tairix_abi::sysinfo::ProcessState;
use tairix_abi::{Errno, Signal};
use tairix_controls::{RecoveryControl, Section, SwitchboardAction, WindowControlKind};
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_input::InputEvent;
use tairix_theme::Theme;

use super::{refusal_notice, CommandOutcome, Panel, PANEL_TITLE};
use crate::model::{build_model, LiveMeters, PanelModel};
use crate::sample::Sample;
use crate::test_host::{
    process_summary, sample_with, RecordingHost, NO_AUTHORITY, PROC_CONTROL_AUTHORITY,
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
        &SeatReport::HEALTHY,
        &LiveMeters::new(),
        authority,
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
        &SeatReport::HEALTHY,
        &LiveMeters::new(),
        &NO_AUTHORITY,
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
        &SeatReport::HEALTHY,
        &LiveMeters::new(),
        &NO_AUTHORITY,
    )
}

/// A model with nothing in it.
fn empty_model() -> PanelModel {
    build_model(
        PANEL_TITLE,
        &Sample::default(),
        &SeatReport::HEALTHY,
        &LiveMeters::new(),
        &NO_AUTHORITY,
    )
}

fn open(panel: &mut Panel, host: &mut RecordingHost, section: CommandSection) -> CommandOutcome {
    panel.command(host, SwitchboardCommand::OpenPanel { section })
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
        BitmapFont::inconsolata(),
    );
    view.scroll_offset()
}

#[test]
fn a_fresh_panel_is_closed_and_shows_nothing() {
    let panel = Panel::new(OWN_PID, empty_model());
    assert!(!panel.is_open());
    assert_eq!(panel.section(), None);
    assert_eq!(panel.seat_report(), &SeatReport::HEALTHY);
}

#[test]
fn opening_shows_the_requested_section_and_arms_the_window_source() {
    for (command, expected) in [
        (CommandSection::Tasks, Section::Tasks),
        (CommandSection::Jobs, Section::Jobs),
        (CommandSection::Recovery, Section::Recovery),
        (CommandSection::Overview, Section::Overview),
    ] {
        let mut host = RecordingHost::new();
        let mut panel = Panel::new(OWN_PID, empty_model());
        assert_eq!(
            open(&mut panel, &mut host, command),
            CommandOutcome::Unchanged
        );
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

    panel.act(
        &mut host,
        SwitchboardAction::Window(WindowControlKind::Close),
        &NO_AUTHORITY,
    );

    assert!(!panel.is_open());
    assert_eq!(panel.section(), None);
    assert_eq!(host.closed, 1);
    assert_eq!(host.armed(), required_members(false));

    // A later model change draws nothing: there is no window to draw into,
    // and the panel never re-opens one on its own.
    let presents = host.presents;
    panel.refresh(&mut host, task_model(10));
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
fn a_seat_report_is_stored_and_asks_the_caller_to_rebuild() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    let report = SeatReport::new(3, &[11, 12]).expect("valid report");

    let outcome = panel.command(&mut host, SwitchboardCommand::SeatReport { report });

    assert_eq!(outcome, CommandOutcome::Rebuild);
    assert_eq!(panel.seat_report().owners(), &[11, 12]);
    assert_eq!(panel.seat_report().total(), 3);
    assert!(!panel.is_open());
}

#[test]
fn refreshing_with_an_unchanged_model_draws_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    panel.refresh(&mut host, empty_model());

    assert_eq!(host.presents, presents);
}

#[test]
fn refreshing_with_a_changed_model_redraws_and_keeps_the_section() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Recovery);
    let presents = host.presents;

    panel.refresh(&mut host, task_model(10));

    assert_eq!(host.presents, presents + 1);
    assert_eq!(panel.section(), Some(Section::Recovery));
}

#[test]
fn a_refresh_keeps_the_users_place_and_shows_the_new_reading() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    assert_eq!(wheel(&mut panel, 4), 4);

    panel.refresh(&mut host, busy_model(200));

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
        SwitchboardAction::Task { index: 0 },
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
        SwitchboardAction::Task { index: 0 },
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
        SwitchboardAction::Task { index: 0 },
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
fn a_refusal_notice_names_the_action_and_the_refusal() {
    assert_eq!(
        refusal_notice("restart that task", Errno::PermissionDenied),
        "switchboard: could not restart that task (permission denied)\n"
    );
}
