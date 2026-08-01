//! Unit tests for the run loop's body: the service samples, refreshes the
//! panel, and publishes every cycle, whether or not a window is open.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport, SwitchboardCommand};
use tairix_abi::sysinfo::{ProcessRecord, ProcessState};
use tairix_abi::{Errno, ProcId};
use tairix_controls::Section;

use super::{CycleOutcome, Service, MAX_CONSECUTIVE_PUBLISH_FAILURES};
use crate::model::GroupingEdit;
use crate::panel::PanelOutcome;
use crate::publish::KEEPALIVE_NS;
use crate::sample::{DegradedField, ScopeVerdicts};
use crate::test_host::{
    process_record, DeadTransport, ProcessListTransport, RecordingHost, DEFAULT_UID, NO_AUTHORITY,
    PROC_CONTROL_AUTHORITY,
};
use crate::wait::required_members;

/// This service's own scheduler task id in these tests.
const OWN_PID: u64 = 4242;

/// Neither optional scope granted — the ordinary unprivileged ceiling.
const NO_SCOPES: ScopeVerdicts = ScopeVerdicts {
    global_process_scope: false,
    memory_pressure: false,
};

/// The global process scope granted, so [`ProcessListTransport`]'s records
/// are read exactly like the real gate would serve them.
const GRANTED_SCOPES: ScopeVerdicts = ScopeVerdicts {
    global_process_scope: true,
    memory_pressure: false,
};

fn service() -> Service {
    Service::new(OWN_PID, NO_SCOPES, &NO_AUTHORITY)
}

fn cycle(service: &mut Service, host: &mut RecordingHost, now_ns: u64) -> CycleOutcome {
    service.cycle(host, &DeadTransport, now_ns, &NO_AUTHORITY)
}

#[test]
fn the_first_cycle_publishes_and_notes_each_degraded_measurement_once() {
    let mut host = RecordingHost::new();
    let mut service = service();

    assert_eq!(cycle(&mut service, &mut host, 0), CycleOutcome::Continue);

    assert_eq!(host.published.len(), 1);
    assert_eq!(
        host.degradations,
        alloc::vec![DegradedField::ProcessList, DegradedField::CpuTime]
    );

    // The same failures on the next cycle are not re-announced.
    cycle(&mut service, &mut host, KEEPALIVE_NS);
    assert_eq!(host.degradations.len(), 2);
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

#[test]
fn a_session_that_refuses_this_instance_stops_the_service_cleanly() {
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
            cycle(&mut service, &mut host, u64::from(attempt)),
            CycleOutcome::Continue
        );
    }
    assert_eq!(
        cycle(
            &mut service,
            &mut host,
            u64::from(MAX_CONSECUTIVE_PUBLISH_FAILURES)
        ),
        CycleOutcome::PublishFailed
    );
    assert_eq!(
        host.published.len(),
        MAX_CONSECUTIVE_PUBLISH_FAILURES as usize
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

    assert_eq!(service.panel().seat_report().owners(), &[11]);
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

    // The capability alone still grants it, even with no self row.
    service.command(
        &mut host,
        SwitchboardCommand::SeatReport {
            report: SeatReport::HEALTHY,
        },
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
