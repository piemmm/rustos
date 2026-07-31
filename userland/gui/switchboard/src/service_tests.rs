//! Unit tests for the run loop's body: the service samples, refreshes the
//! panel, and publishes every cycle, whether or not a window is open.

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport, SwitchboardCommand};
use tairix_abi::Errno;
use tairix_controls::Section;

use super::{CycleOutcome, Service, MAX_CONSECUTIVE_PUBLISH_FAILURES};
use crate::publish::KEEPALIVE_NS;
use crate::sample::{DegradedField, ScopeVerdicts};
use crate::test_host::{DeadTransport, RecordingHost, NO_AUTHORITY};
use crate::wait::required_members;

/// This service's own scheduler task id in these tests.
const OWN_PID: u64 = 4242;

/// Neither optional scope granted — the ordinary unprivileged ceiling.
const NO_SCOPES: ScopeVerdicts = ScopeVerdicts {
    global_process_scope: false,
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
