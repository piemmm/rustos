//! Unit tests for the overview panel's window lifecycle and effect
//! application, driven entirely through the recording host.

use tairix_abi::driver::display::DamageRect;
use tairix_abi::switchboard_ipc::{CommandSection, FrameReport, SeatReport, SwitchboardRequest};
use tairix_abi::sysinfo::ProcessState;
use tairix_abi::{Errno, Signal};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::InputEvent;
use tairix_theme::Theme;

use super::{refusal_notice, Panel, PANEL_TITLE};
use crate::model::{build_model, PanelModel, RollingMeters, SessionReport};
use crate::sample::Sample;
use crate::test_host::{
    process_summary, sample_with, RecordingHost, NO_AUTHORITY, PROC_CONTROL_AUTHORITY,
};
use crate::view::{RecoveryControl, Section, SwitchboardAction, TaskControl};
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
        &mut RollingMeters::new(),
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
        &SessionReport::HEALTHY,
        &mut RollingMeters::new(),
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
        &SessionReport::HEALTHY,
        &mut RollingMeters::new(),
        &NO_AUTHORITY,
    )
}

/// A model whose only reading is what the session's last frame cost, so a
/// refresh from one to another changes the System section's compositor
/// figures and nothing else any section draws.
fn frame_model(damaged_px: u64) -> PanelModel {
    let session = SessionReport {
        seat: SeatReport::HEALTHY,
        frame: Some(FrameReport {
            screen_px: 1920 * 1080,
            damaged_px,
            blended_px: 42_000,
            opaque_px: 1_100,
            dirty_rects: 3,
            present_calls: 1,
            chrome_hits: 12,
            chrome_misses: 1,
        }),
    };
    build_model(
        PANEL_TITLE,
        &Sample::default(),
        &session,
        &mut RollingMeters::new(),
        &NO_AUTHORITY,
    )
}

/// A model with nothing in it.
fn empty_model() -> PanelModel {
    build_model(
        PANEL_TITLE,
        &Sample::default(),
        &SessionReport::HEALTHY,
        &mut RollingMeters::new(),
        &NO_AUTHORITY,
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
    panel.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: lines },
        WINDOW,
        Scale::ONE,
        &Theme::dark(),
        BitmapFont::console(),
    );
    scroll_offset(panel)
}

/// The open composition's scroll offset, read straight off the panel's own
/// field: these tests are the panel's own module, so no accessor exists for
/// their sake alone.
fn scroll_offset(panel: &Panel) -> u64 {
    panel
        .view
        .as_ref()
        .expect("the panel is open")
        .scroll_offset()
}

/// Feed a bare pointer move to the open panel, the way the run loop
/// delivers a `WindowEvent::Pointer` `Moved` action.
fn pointer_move(panel: &mut Panel, to: Point) {
    panel.on_pointer(
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

    panel.refresh(stopped_model(7, false));
    panel.flush(&mut host);

    assert_eq!(host.presents, presents + 1);
    assert_eq!(panel.section(), Some(Section::Recovery));
}

#[test]
fn refreshing_a_section_that_is_not_on_show_draws_nothing() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, empty_model());
    open(&mut panel, &mut host, CommandSection::Recovery);
    let presents = host.presents;

    // A task appears, which only the Tasks section draws.
    panel.refresh(task_model(10));
    panel.flush(&mut host);

    assert_eq!(
        host.presents, presents,
        "a reading no shown section draws must not repaint the window"
    );
    assert_eq!(panel.section(), Some(Section::Recovery));
}

#[test]
fn a_fresh_frame_reading_draws_nothing_while_tasks_is_on_show() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, frame_model(3_200));
    open(&mut panel, &mut host, CommandSection::Tasks);
    let presents = host.presents;

    panel.refresh(frame_model(6_400));
    panel.flush(&mut host);

    assert_eq!(
        host.presents, presents,
        "the session reports a frame per compositor frame, and only the \
         System section draws it"
    );
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
        scroll_offset(&panel),
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
    // Past the signed range no pid exists, so a sample claiming one is
    // refused rather than folded onto a different, arbitrary process.
    let beyond = i64::MAX.cast_unsigned() + 1;
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

// --- What a present covers ------------------------------------------------

/// The client rectangle a present would cover in full, as the recording
/// host's own bounds describe it.
fn whole_client(host: &RecordingHost) -> DamageRect {
    DamageRect {
        x: 0,
        y: 0,
        width_px: host.bounds.2,
        height_px: host.bounds.3,
    }
}

#[test]
fn a_hover_presents_the_control_it_crossed_rather_than_the_window() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    assert_eq!(host.last_presented_rect(), Some(whole_client(&host)));

    pointer_move(&mut panel, Point::new(40, 200));
    panel.flush(&mut host);

    let rect = host.last_presented_rect().expect("the hover presented");
    assert!(
        rect.height_px < host.bounds.3,
        "a hover repaints the row it entered, not every row: {rect:?}"
    );
}

#[test]
fn a_fresh_reading_presents_the_whole_window() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    pointer_move(&mut panel, Point::new(40, 200));
    panel.flush(&mut host);

    panel.refresh(busy_model(200));
    panel.flush(&mut host);

    assert_eq!(
        host.last_presented_rect(),
        Some(whole_client(&host)),
        "every row is re-derived at once, which no control round described"
    );
}

#[test]
fn a_hover_report_does_not_survive_into_the_next_present() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    pointer_move(&mut panel, Point::new(40, 200));
    panel.flush(&mut host);

    // Nothing reported this time, so the fail-safe covers the window rather
    // than re-presenting the last round's rectangle.
    host.theme_id += 1;
    panel.flush(&mut host);

    assert_eq!(host.last_presented_rect(), Some(whole_client(&host)));
}

#[test]
fn discarded_pixels_are_redrawn_whole() {
    let mut host = RecordingHost::new();
    let mut panel = Panel::new(OWN_PID, busy_model(100));
    open(&mut panel, &mut host, CommandSection::Tasks);
    pointer_move(&mut panel, Point::new(40, 200));

    panel.invalidate_presented();
    panel.flush(&mut host);

    assert_eq!(
        host.last_presented_rect(),
        Some(whole_client(&host)),
        "the session gave back the whole window's pixels, so the whole window is drawn"
    );
}
