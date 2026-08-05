//! Unit tests for the run loop's body: the service samples, refreshes the
//! panel, and publishes every cycle, whether or not a window is open.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport, SwitchboardCommand};
use tairix_abi::sysinfo::{ProcessRecord, ProcessState};
use tairix_abi::{Errno, PowerAction, ProcId};

use super::{CycleOutcome, Service, MAX_CONSECUTIVE_PUBLISH_FAILURES};
use crate::model::GroupingEdit;
use crate::panel::PanelOutcome;
use crate::publish::KEEPALIVE_NS;
use crate::sample::{DegradedField, ScopeVerdicts};
use crate::test_host::{
    process_record, DeadTransport, ProcessListTransport, RecordingHost, DEFAULT_UID, NO_AUTHORITY,
    PROC_CONTROL_AUTHORITY, SYSTEM_POWER_AUTHORITY,
};
use crate::view::Section;
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

    // At the deadline it samples again.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS,
        &NO_AUTHORITY,
    );
    assert_eq!(transport.request_count(), requests_after_one * 2);

    // And the deadline has moved: another cycle immediately is a no-op.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS,
        &NO_AUTHORITY,
    );
    assert_eq!(transport.request_count(), requests_after_one * 2);
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

    // Once it passes, one sample happens.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS + 1,
        &NO_AUTHORITY,
    );
    assert_eq!(transport.request_count(), requests_after_one * 2);
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
    // Only from the third sample on is the reading itself genuinely steady.
    service.cycle(&mut host, &transport, 0, &NO_AUTHORITY);
    service.panel_mut().flush(&mut host);
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS,
        &NO_AUTHORITY,
    );
    service.panel_mut().flush(&mut host);
    let presents = host.presents;

    // One full sample period later still, the transport reports the exact
    // same process list and the measured share is the same steady 0%.
    // Nothing the composition draws differs, so this must not present
    // again.
    service.cycle(
        &mut host,
        &transport,
        crate::SAMPLE_PERIOD_NS * 2,
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

    assert!(t2 < t1);
    assert!(t2 >= crate::schedule::MIN_WAIT_NS);
}
