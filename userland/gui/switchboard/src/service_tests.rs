//! Unit tests for the run loop's body: the service samples, refreshes the
//! panel, and publishes every cycle, whether or not a window is open.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, FrameReport, SeatReport, SwitchboardCommand};
use tairix_abi::sysinfo::{ProcessRecord, ProcessState};
use tairix_abi::{Errno, PowerAction, ProcId};

use super::{CycleOutcome, Service, MAX_CONSECUTIVE_PUBLISH_FAILURES};
use crate::model::{GroupingEdit, TASK_HISTORY_LEN};
use crate::panel::PanelOutcome;
use crate::publish::KEEPALIVE_NS;
use crate::sample::{DegradedField, ScopeVerdicts};
use crate::test_host::{
    process_record, DeadTransport, ProcessListTransport, RecordingHost, DEFAULT_UID, NO_AUTHORITY,
    PROC_CONTROL_AUTHORITY, SYSTEM_POWER_AUTHORITY,
};
use crate::view::{Reading, Section};
use crate::wait::required_members;

/// This service's own scheduler task id in these tests.
const OWN_PID: u64 = 4242;

/// No optional scope granted — the ordinary unprivileged ceiling.
const NO_SCOPES: ScopeVerdicts = ScopeVerdicts {
    global_process_scope: false,
    memory_pressure: false,
    hardware_scope: false,
};

/// The global process scope granted, so [`ProcessListTransport`]'s records
/// are read exactly like the real gate would serve them.
const GRANTED_SCOPES: ScopeVerdicts = ScopeVerdicts {
    global_process_scope: true,
    memory_pressure: false,
    hardware_scope: false,
};

fn service() -> Service {
    Service::new(OWN_PID, NO_SCOPES, &NO_AUTHORITY)
}

fn test_proc_id(pid: u64) -> ProcId {
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&pid.to_le_bytes());
    ProcId::from_raw(raw)
}

fn cycle(service: &mut Service, host: &mut RecordingHost, now_ns: u64) -> CycleOutcome {
    let outcome = service.cycle(host, &DeadTransport, now_ns, &NO_AUTHORITY);
    service.panel_mut().flush(host);
    outcome
}

#[test]
fn the_first_cycle_publishes_and_notes_each_degraded_measurement_once() {
    let mut host = RecordingHost::new();
    let mut service = service();

    assert_eq!(cycle(&mut service, &mut host, 0), CycleOutcome::Continue);

    assert_eq!(host.published.len(), 1);
    // Every reading this unprivileged ceiling permits was attempted against
    // a dead transport, so each states its own degradation once, in the
    // order the sampler reads them. The capability-gated readings are
    // absent rather than degraded: they were never issued.
    assert_eq!(
        host.degradations,
        alloc::vec![
            DegradedField::ProcessList,
            DegradedField::CpuTime,
            DegradedField::Uptime,
            DegradedField::LoadAverage,
            DegradedField::CpuInfo,
            DegradedField::Identity,
            DegradedField::MemoryTotal,
            DegradedField::ResourceLimits,
            DegradedField::Mounts,
        ]
    );
    let announced = host.degradations.len();

    // The same failures on the next cycle are not re-announced.
    cycle(&mut service, &mut host, KEEPALIVE_NS);
    assert_eq!(host.degradations.len(), announced);
}

#[test]
fn an_unchanged_summary_is_only_re_published_on_the_keepalive() {
    let mut host = RecordingHost::new();
    let mut service = service();

    cycle(&mut service, &mut host, 0);
    cycle(&mut service, &mut host, KEEPALIVE_NS / 2);
    assert_eq!(host.published.len(), 1);

    cycle(&mut service, &mut host, KEEPALIVE_NS);
    assert_eq!(host.published.len(), 2);
}

#[test]
fn publishing_continues_while_a_window_is_open_and_after_it_closes() {
    let mut host = RecordingHost::new();
    let mut service = service();

    cycle(&mut service, &mut host, 0);
    assert_eq!(host.published.len(), 1);

    service.command(
        &mut host,
        SwitchboardCommand::OpenPanel {
            section: CommandSection::Tasks,
        },
        &NO_AUTHORITY,
    );
    assert!(service.panel().is_open());
    assert_eq!(host.armed(), required_members(true));

    cycle(&mut service, &mut host, KEEPALIVE_NS);
    assert_eq!(host.published.len(), 2);

    service.panel_mut().close(&mut host);
    assert!(!service.panel().is_open());
    assert_eq!(host.armed(), required_members(false));

    cycle(&mut service, &mut host, KEEPALIVE_NS * 2);
    assert_eq!(host.published.len(), 3);
}

#[test]
fn an_unbound_endpoint_stops_the_service_cleanly() {
    let mut host = RecordingHost::new();
    host.publish_refusal = Some(Errno::NotFound);
    let mut service = service();

    assert_eq!(
        cycle(&mut service, &mut host, 0),
        CycleOutcome::SessionUnbound
    );
}

/// A refusal is told apart from a session that simply is not there, because
/// the run loop ends quietly on the second and fails loudly on the first.
#[test]
fn a_session_that_refuses_this_instance_stops_the_service_abnormally() {
    let mut host = RecordingHost::new();
    host.publish_refusal = Some(Errno::PermissionDenied);
    let mut service = service();

    assert_eq!(
        cycle(&mut service, &mut host, 0),
        CycleOutcome::SessionRefused
    );
}

#[test]
fn repeated_publish_failures_eventually_stop_the_service() {
    let mut host = RecordingHost::new();
    host.publish_refusal = Some(Errno::DeviceFault);
    let mut service = service();

    for attempt in 1..MAX_CONSECUTIVE_PUBLISH_FAILURES {
        assert_eq!(
            cycle(
                &mut service,
                &mut host,
                u64::from(attempt).saturating_mul(crate::SAMPLE_PERIOD_NS)
            ),
            CycleOutcome::Continue
        );
    }
    assert_eq!(
        cycle(
            &mut service,
            &mut host,
            u64::from(MAX_CONSECUTIVE_PUBLISH_FAILURES).saturating_mul(crate::SAMPLE_PERIOD_NS)
        ),
        CycleOutcome::PublishFailed
    );
    assert_eq!(
        host.published.len(),
        MAX_CONSECUTIVE_PUBLISH_FAILURES as usize
    );
}

/// A desktop that has not drained its queue must never cost this service
/// its life.
///
/// The regression, and the whole reason the tray capsule went permanently
/// dead: a call endpoint at capacity refuses the post outright rather than
/// blocking, so a session busy enough to leave its queue full for five
/// sample periods used to exhaust the give-up budget and the monitor exited
/// — and nothing restarts one. It is back-pressure, not a fault: the
/// summary is still unacknowledged, so the next sample simply offers it
/// again, and one attempt per period is not a retry loop.
#[test]
fn a_session_that_has_not_drained_its_queue_never_stops_the_service() {
    let mut host = RecordingHost::new();
    host.publish_refusal = Some(Errno::WouldBlock);
    let mut service = service();

    let periods = MAX_CONSECUTIVE_PUBLISH_FAILURES * 4;
    for period in 1..=periods {
        assert_eq!(
            cycle(
                &mut service,
                &mut host,
                u64::from(period).saturating_mul(crate::SAMPLE_PERIOD_NS)
            ),
            CycleOutcome::Continue
        );
    }

    // Every period tried, so the summary is offered the moment the session
    // drains rather than waiting on a keepalive.
    assert_eq!(host.published.len(), periods as usize);
    host.publish_refusal = None;
    assert_eq!(
        cycle(
            &mut service,
            &mut host,
            u64::from(periods + 1).saturating_mul(crate::SAMPLE_PERIOD_NS)
        ),
        CycleOutcome::Continue
    );
    assert_eq!(host.published.len(), periods as usize + 1);
}

/// Back-pressure does not launder a genuine fault: a real failure that
/// follows one still counts.
#[test]
fn back_pressure_does_not_clear_the_give_up_budget() {
    let mut host = RecordingHost::new();
    let mut service = service();
    let mut period = 0;
    let mut next = |host: &mut RecordingHost, service: &mut Service, refusal| {
        host.publish_refusal = refusal;
        period += 1;
        cycle(service, host, period * crate::SAMPLE_PERIOD_NS)
    };

    for _ in 1..MAX_CONSECUTIVE_PUBLISH_FAILURES {
        assert_eq!(
            next(&mut host, &mut service, Some(Errno::DeviceFault)),
            CycleOutcome::Continue
        );
    }
    assert_eq!(
        next(&mut host, &mut service, Some(Errno::WouldBlock)),
        CycleOutcome::Continue
    );
    assert_eq!(
        next(&mut host, &mut service, Some(Errno::DeviceFault)),
        CycleOutcome::PublishFailed
    );
}

#[test]
fn an_open_command_shows_the_panel_without_waiting_for_a_cycle() {
    let mut host = RecordingHost::new();
    let mut service = service();

    service.command(
        &mut host,
        SwitchboardCommand::OpenPanel {
            section: CommandSection::Recovery,
        },
        &NO_AUTHORITY,
    );

    assert_eq!(service.panel().section(), Some(Section::Recovery));
    assert_eq!(host.opened, 1);
    assert!(host.published.is_empty());
}

#[test]
fn a_seat_report_is_folded_into_the_panel_at_once() {
    let mut host = RecordingHost::new();
    let mut service = service();
    let report = SeatReport::new(2, &[11]).expect("valid report");

    service.command(
        &mut host,
        SwitchboardCommand::SeatReport { report },
        &NO_AUTHORITY,
    );

    assert_eq!(service.panel().session_report().seat.owners(), &[11]);
}

/// A frame report the tests below feed the service.
fn frame_report() -> FrameReport {
    FrameReport {
        screen_px: 1920 * 1080,
        damaged_px: 3_200,
        blended_px: 42_000,
        opaque_px: 1_100,
        dirty_rects: 3,
        present_calls: 1,
        chrome_hits: 12,
        chrome_misses: 1,
    }
}

/// Open the panel on a named section, as the session's own `OpenPanel`
/// command does.
fn open(service: &mut Service, host: &mut RecordingHost, section: CommandSection) {
    service.command(
        host,
        SwitchboardCommand::OpenPanel { section },
        &NO_AUTHORITY,
    );
}

#[test]
fn a_frame_report_reaches_an_open_resources_page_at_once() {
    let mut host = RecordingHost::new();
    let mut service = service();
    let report = frame_report();

    open(&mut service, &mut host, CommandSection::System);
    service.command(
        &mut host,
        SwitchboardCommand::FrameReport { report },
        &NO_AUTHORITY,
    );

    assert_eq!(service.panel().session_report().frame, Some(report));
    let facts = &service.panel().model().model.system.compositor;
    assert_eq!(facts[0].label, "Last frame");
    assert_eq!(
        facts[0].value,
        Reading::measured("3.2k px of 2.0M px recomposed"),
        "the report must be rebuilt into the page, not held until the next sample"
    );
}

/// A frame report is adopted while the panel is closed, but nothing is
/// rebuilt for it.
///
/// This is the regression for the pointer-over-wallpaper storm's second
/// half. The session's frame path can produce a report several times a
/// second, and rebuilding walks every sampled process to allocate a row, a
/// name, and a CPU history for each — work that with no window open reaches
/// no screen at all, since `Panel::refresh` renders nothing without a view
/// and the next `cycle` rebuilds from a fresh sample regardless. Together
/// with the session's own rate limit this is what takes the monitor from
/// half a core down to nothing while a user simply moves the pointer.
#[test]
fn a_frame_report_does_not_rebuild_the_model_while_the_panel_is_closed() {
    let mut host = RecordingHost::new();
    let mut service = service();
    let before = service.panel().model().clone();

    service.command(
        &mut host,
        SwitchboardCommand::FrameReport {
            report: frame_report(),
        },
        &NO_AUTHORITY,
    );

    assert_eq!(
        service.panel().session_report().frame,
        Some(frame_report()),
        "the report itself is still adopted: that is a field write, not a rebuild"
    );
    assert_eq!(
        *service.panel().model(),
        before,
        "with no window open, nothing may be rebuilt for a report nothing can show"
    );
    assert_eq!(host.presents, 0, "and nothing may be presented");
}

/// A seat report is on the same terms: adopted always, rebuilt only for a
/// panel that can show it.
///
/// Sampled against a real process list and naming one of its rows, so the
/// report is one that genuinely *would* change the model — a closed panel
/// whose model happens to be empty either way would prove nothing.
#[test]
fn a_seat_report_does_not_rebuild_the_model_while_the_panel_is_closed() {
    let target_pid = 50;
    let transport = ProcessListTransport::new(two_row_records(OWN_PID, target_pid));
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    let report = SeatReport::new(1, &[target_pid]).expect("valid report");
    let before = service.panel().model().clone();

    service.command(
        &mut host,
        SwitchboardCommand::SeatReport { report },
        &NO_AUTHORITY,
    );

    assert_eq!(
        service.panel().session_report().seat.owners(),
        &[target_pid]
    );
    assert_eq!(
        *service.panel().model(),
        before,
        "with no window open, nothing may be rebuilt for a report nothing can show"
    );

    // The premise, and the freshness guarantee: opening the panel is what
    // folds it in, and it really does change what the page shows.
    open(&mut service, &mut host, CommandSection::Recovery);
    assert_ne!(
        *service.panel().model(),
        before,
        "the report this test withholds must be one that changes the page"
    );
}

/// Opening the panel is what folds in every report that arrived while it
/// was closed, so a user never sees a page built from a report the service
/// had already been told about.
///
/// This is the whole reason the rebuild can be skipped above: the window is
/// created from the model, so the model is rebuilt first.
#[test]
fn opening_the_panel_shows_the_reports_that_arrived_while_it_was_closed() {
    let mut host = RecordingHost::new();
    let mut service = service();

    service.command(
        &mut host,
        SwitchboardCommand::FrameReport {
            report: frame_report(),
        },
        &NO_AUTHORITY,
    );
    open(&mut service, &mut host, CommandSection::System);

    let facts = &service.panel().model().model.system.compositor;
    assert_eq!(facts[0].label, "Last frame");
    assert_eq!(
        facts[0].value,
        Reading::measured("3.2k px of 2.0M px recomposed"),
        "a page opening now must carry the report the service already holds"
    );
}

/// Two sampled rows: the service's own, and a target task grouped into an
/// activity by the tests below.
fn two_row_records(self_pid: u64, target_pid: u64) -> Vec<ProcessRecord> {
    alloc::vec![
        process_record(
            self_pid,
            ProcId::from_raw([1; 16]),
            DEFAULT_UID,
            ProcessState::Running,
            b"switchboard"
        ),
        process_record(
            target_pid,
            ProcId::from_raw([2; 16]),
            DEFAULT_UID,
            ProcessState::Running,
            b"task"
        ),
    ]
}

#[test]
fn self_uid_derived_from_the_services_own_row_grants_same_uid_control() {
    let target_pid = 50;
    let transport = ProcessListTransport::new(two_row_records(OWN_PID, target_pid));
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);

    service.apply_grouping(
        &mut host,
        PanelOutcome::Edit(GroupingEdit::Assign {
            task: 1,
            activity: None,
        }),
        &NO_AUTHORITY,
    );

    assert_eq!(service.panel().model().model.activities.len(), 1);
    assert!(
        service.panel().model().model.activities[0].can_control,
        "the target shares the service's own derived uid"
    );
}

#[test]
fn a_missing_self_row_denies_control_without_the_capability() {
    let target_pid = 50;
    let records = alloc::vec![process_record(
        target_pid,
        ProcId::from_raw([2; 16]),
        DEFAULT_UID,
        ProcessState::Running,
        b"task"
    )];
    let transport = ProcessListTransport::new(records);
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);

    service.apply_grouping(
        &mut host,
        PanelOutcome::Edit(GroupingEdit::Assign {
            task: 0,
            activity: None,
        }),
        &NO_AUTHORITY,
    );
    assert!(
        !service.panel().model().model.activities[0].can_control,
        "an unknown self uid must never grant the same-uid rule"
    );

    // The capability alone still grants it, even with no self row. Re-derived
    // through a cycle, which is what rebuilds the model under an authority in
    // production; the assigned activity survives it because its member is
    // still in the sampled process list.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS,
        &PROC_CONTROL_AUTHORITY,
    );
    assert!(service.panel().model().model.activities[0].can_control);
}

#[test]
fn a_grouping_edit_re_presents_the_panel_immediately() {
    let target_pid = 50;
    let transport = ProcessListTransport::new(two_row_records(OWN_PID, target_pid));
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    service.command(
        &mut host,
        SwitchboardCommand::OpenPanel {
            section: CommandSection::Activities,
        },
        &NO_AUTHORITY,
    );
    let presents = host.presents;

    service.apply_grouping(
        &mut host,
        PanelOutcome::Edit(GroupingEdit::Assign {
            task: 1,
            activity: None,
        }),
        &NO_AUTHORITY,
    );
    service.panel_mut().flush(&mut host);

    assert_eq!(service.panel().model().model.activities.len(), 1);
    assert!(
        host.presents > presents,
        "the new activity is shown without waiting for the next sample"
    );
}

#[test]
fn activities_survive_a_degraded_process_list_sample() {
    let target_pid = 50;
    let transport = ProcessListTransport::new(two_row_records(OWN_PID, target_pid));
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    service.apply_grouping(
        &mut host,
        PanelOutcome::Edit(GroupingEdit::Assign {
            task: 1,
            activity: None,
        }),
        &NO_AUTHORITY,
    );
    assert_eq!(service.panel().model().model.activities.len(), 1);

    // A later cycle whose process-list query fails must not wipe the
    // activity: an honestly empty list from a query failure is not "every
    // process exited".
    service.cycle(&mut host, &DeadTransport, KEEPALIVE_NS, &NO_AUTHORITY);
    assert_eq!(
        service.panel().model().model.activities.len(),
        1,
        "a degraded sample must never prune live activities"
    );
}

#[test]
fn a_rename_refusal_is_reported_and_leaves_the_name_unchanged() {
    let target_pid = 50;
    let transport = ProcessListTransport::new(two_row_records(OWN_PID, target_pid));
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    service.apply_grouping(
        &mut host,
        PanelOutcome::Edit(GroupingEdit::Assign {
            task: 1,
            activity: None,
        }),
        &NO_AUTHORITY,
    );

    service.apply_grouping(
        &mut host,
        PanelOutcome::Renamed {
            activity: 0,
            name: String::from("   "),
        },
        &NO_AUTHORITY,
    );

    assert_eq!(
        service.panel().model().model.activities[0].name,
        "Activity 1"
    );
    assert_eq!(host.refused_actions(), alloc::vec!["rename that activity"]);
}

#[test]
fn a_cycle_before_the_deadline_is_a_no_op() {
    let transport = ProcessListTransport::new(alloc::vec![process_record(
        10,
        test_proc_id(10),
        DEFAULT_UID,
        ProcessState::Running,
        b"alpha"
    )]);
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);

    // First cycle samples immediately (deadline is 0).
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    assert_eq!(host.published.len(), 1);
    let requests = transport.request_count();

    // A second cycle before the deadline does nothing.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS / 2,
        &NO_AUTHORITY,
    );
    assert_eq!(host.published.len(), 1);
    assert_eq!(transport.request_count(), requests);
}

#[test]
fn a_cycle_at_the_deadline_samples_exactly_once_and_advances_the_deadline() {
    let transport = ProcessListTransport::new(alloc::vec![process_record(
        10,
        test_proc_id(10),
        DEFAULT_UID,
        ProcessState::Running,
        b"alpha"
    )]);
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);

    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    let requests_after_one = transport.request_count();
    assert!(requests_after_one > 0);

    // At the deadline it samples again. A later sample costs fewer queries
    // than the first, because the static and slow-moving readings are not
    // due, so the evidence that a sample happened is that *some* queries
    // were issued.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS,
        &NO_AUTHORITY,
    );
    let requests_after_two = transport.request_count();
    assert!(requests_after_two > requests_after_one);

    // And the deadline has moved: another cycle immediately is a no-op.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS,
        &NO_AUTHORITY,
    );
    assert_eq!(transport.request_count(), requests_after_two);
}

#[test]
fn many_sub_deadline_cycles_produce_exactly_one_sample_once_the_deadline_passes() {
    let transport = ProcessListTransport::new(alloc::vec![process_record(
        10,
        test_proc_id(10),
        DEFAULT_UID,
        ProcessState::Running,
        b"alpha"
    )]);
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);

    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    let requests_after_one = transport.request_count();

    // Many cycles before the deadline.
    for i in 1..100 {
        service.cycle(
            &mut host,
            &transport,
            (crate::SAMPLE_PERIOD_NS / 100) * i,
            &NO_AUTHORITY,
        );
    }
    assert_eq!(transport.request_count(), requests_after_one);

    // Once it passes, one sample happens: further queries are issued, and
    // the cycle straight after it issues none.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS + 1,
        &NO_AUTHORITY,
    );
    let requests_after_two = transport.request_count();
    assert!(requests_after_two > requests_after_one);
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS + 1,
        &NO_AUTHORITY,
    );
    assert_eq!(transport.request_count(), requests_after_two);
}

#[test]
fn an_unchanged_sample_one_period_later_presents_nothing_new() {
    let transport = ProcessListTransport::new(alloc::vec![process_record(
        10,
        test_proc_id(10),
        DEFAULT_UID,
        ProcessState::Running,
        b"alpha"
    )]);
    let mut host = RecordingHost::new();
    let mut service = Service::new(OWN_PID, GRANTED_SCOPES, &NO_AUTHORITY);
    service.command(
        &mut host,
        SwitchboardCommand::OpenPanel {
            section: CommandSection::Tasks,
        },
        &NO_AUTHORITY,
    );
    // The first sample has no prior reading to diff a per-process CPU share
    // against, so it measures as unmeasured; the second sample is the first
    // one that can measure a share at all (0%, the fixture's rows never
    // advance their recorded CPU time) and so still differs from the first.
    // From the third sample on the reading itself is steady, but the row's
    // plotted CPU history is still one reading longer each time, so the
    // sparkline genuinely differs until that ring is full: settle it first,
    // then measure, so "unchanged" is asked of a composition that has
    // actually stopped changing.
    let settle = TASK_HISTORY_LEN + 2;
    for step in 0..settle {
        let now = crate::SAMPLE_PERIOD_NS.saturating_mul(step as u64);
        service.cycle(&mut host, &transport, now, &NO_AUTHORITY);
        service.panel_mut().flush(&mut host);
    }
    let presents = host.presents;

    // One full sample period later still, the transport reports the exact
    // same process list and the measured share is the same steady 0%.
    // Nothing the composition draws differs, so this must not present
    // again.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS.saturating_mul(settle as u64),
        &NO_AUTHORITY,
    );
    service.panel_mut().flush(&mut host);

    assert_eq!(host.presents, presents);
}

// ---- power transitions -------------------------------------------------

#[test]
fn a_power_command_acts_only_under_the_power_capability() {
    for action in [PowerAction::PowerOff, PowerAction::Restart] {
        let mut host = RecordingHost::new();
        let mut service = service();

        service.command(
            &mut host,
            SwitchboardCommand::Power { action },
            &SYSTEM_POWER_AUTHORITY,
        );

        assert_eq!(host.powered, alloc::vec![action]);
        assert!(
            host.refusals.is_empty(),
            "a granted transition states no refusal"
        );
    }
}

#[test]
fn a_power_command_without_the_capability_is_refused_and_never_attempted() {
    let mut host = RecordingHost::new();
    let mut service = service();

    service.command(
        &mut host,
        SwitchboardCommand::Power {
            action: PowerAction::PowerOff,
        },
        &NO_AUTHORITY,
    );

    assert!(
        host.powered.is_empty(),
        "the machine is never asked to stop without the authority to stop it"
    );
    assert_eq!(
        host.refusals,
        alloc::vec![(
            String::from("power the machine off"),
            Errno::PermissionDenied
        )]
    );
}

#[test]
fn a_capability_that_is_not_the_power_one_still_refuses() {
    // Holding some other authority is not holding this one: the check names
    // the capability it needs rather than settling for "privileged enough".
    let mut host = RecordingHost::new();
    let mut service = service();

    service.command(
        &mut host,
        SwitchboardCommand::Power {
            action: PowerAction::Restart,
        },
        &PROC_CONTROL_AUTHORITY,
    );

    assert!(host.powered.is_empty());
    assert_eq!(
        host.refused_actions(),
        alloc::vec!["restart the machine"],
        "the refusal names the transition that did not happen"
    );
}

#[test]
fn a_kernel_refusal_of_a_permitted_transition_is_stated_and_the_service_lives_on() {
    // The capability is held, so the call is made — and comes back, which
    // only happens when the kernel refused (a platform with no reset
    // primitive, say). The user is told and the service keeps monitoring.
    let mut host = RecordingHost::new();
    host.power_refusal = Some(Errno::NotSupported);
    let mut service = service();

    service.command(
        &mut host,
        SwitchboardCommand::Power {
            action: PowerAction::Restart,
        },
        &SYSTEM_POWER_AUTHORITY,
    );

    assert_eq!(host.powered, alloc::vec![PowerAction::Restart]);
    assert_eq!(
        host.refusals,
        alloc::vec![(String::from("restart the machine"), Errno::NotSupported)]
    );

    // Still a live monitor: the next cycle still publishes.
    assert_eq!(cycle(&mut service, &mut host, 0), CycleOutcome::Continue);
    assert_eq!(host.published.len(), 1);
}

#[test]
fn the_published_power_flag_tracks_the_live_capability() {
    // Unheld: the summary says so, so the desktop's Restart and Shut Down
    // rows render refused rather than offering an action nothing can carry
    // out.
    let mut host = RecordingHost::new();
    let mut service = service();
    service.cycle(&mut host, &DeadTransport, 0, &NO_AUTHORITY);
    assert_eq!(host.published.len(), 1);
    assert!(!host.published[0].power_capable);

    // Held: the very next publish attests it, without waiting for a
    // restart — the flag is re-read every cycle rather than cached.
    service.cycle(
        &mut host,
        &DeadTransport,
        KEEPALIVE_NS,
        &SYSTEM_POWER_AUTHORITY,
    );
    assert_eq!(host.published.len(), 2);
    assert!(host.published[1].power_capable);

    // Dropped again: the attestation is withdrawn just as promptly.
    service.cycle(&mut host, &DeadTransport, KEEPALIVE_NS * 2, &NO_AUTHORITY);
    assert_eq!(host.published.len(), 3);
    assert!(!host.published[2].power_capable);
}

#[test]
fn a_derived_summary_never_claims_power_authority_on_its_own() {
    // The derivation reads measurements, which carry no authority, so its
    // own answer is always the denied one; only the service's live check
    // can raise it.
    let summary = crate::derive::derive_summary(
        &crate::sample::Sample::default(),
        &mut crate::derive::Hysteresis::new(),
    );
    assert!(!summary.power_capable);
}

#[test]
fn wait_timeout_ns_shrinks_as_the_deadline_approaches() {
    let mut service = Service::new(OWN_PID, NO_SCOPES, &NO_AUTHORITY);
    let mut host = RecordingHost::new();

    // Deadline is 0, so it's already overdue.
    service.cycle(&mut host, &DeadTransport, 0, &NO_AUTHORITY);
    // Next deadline is SAMPLE_PERIOD_NS.

    let t1 = service.wait_timeout_ns(0);
    let t2 = service.wait_timeout_ns(crate::SAMPLE_PERIOD_NS / 2);

    assert_eq!(t1, crate::SAMPLE_PERIOD_NS);
    assert_eq!(t2, crate::SAMPLE_PERIOD_NS / 2);
}

/// A cycle whose own work costs a whole sample period must still park for a
/// period afterwards. The deadline is anchored to the clock as it stood
/// before the work, so without re-anchoring the wait the loop would find
/// nothing left to wait for, re-enter the full cycle at once, and keep doing
/// so — the runaway a busy monitor was observed to fall into.
#[test]
fn a_cycle_that_costs_a_whole_period_still_parks_for_one() {
    let mut service = Service::new(OWN_PID, NO_SCOPES, &NO_AUTHORITY);
    let mut host = RecordingHost::new();

    let entered = 0;
    service.cycle(&mut host, &DeadTransport, entered, &NO_AUTHORITY);
    let finished = entered + 3 * crate::SAMPLE_PERIOD_NS;

    let timeout = service.wait_timeout_ns(finished);

    assert_eq!(timeout, crate::SAMPLE_PERIOD_NS);
    // And the adopted deadline is the one the next cycle checks, so the
    // sample after this park is due exactly when the park ends.
    assert_eq!(
        service.cycle(
            &mut host,
            &DeadTransport,
            finished + crate::SAMPLE_PERIOD_NS - 1,
            &NO_AUTHORITY
        ),
        CycleOutcome::Continue
    );
    assert_eq!(
        host.published.len(),
        1,
        "a cycle before the adopted deadline samples nothing"
    );
}
