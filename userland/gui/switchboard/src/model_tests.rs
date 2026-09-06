//! Unit tests for the live model builder and action-to-effect mapping.

use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::SeatReport;
use tairix_abi::sysinfo::{
    CrashFaultBucket, CrashFaultClass, CrashNamedReg, CrashRecord, ProcessState, Uptime,
};
use tairix_abi::{Duration64, ProcId, Signal, Time64};

use super::{
    apply_action, build_model, signal_pid, Effect, RollingMeters, SessionReport, TaskMeters,
    TASK_HISTORY_LEN,
};
use crate::derive::{derive_summary, Hysteresis};
use crate::sample::{ProcessSummary, Sample};
use crate::test_host::{
    process_summary as process, sample_with, DEFAULT_UID, NO_AUTHORITY as NONE,
    PROC_CONTROL_AUTHORITY as PROC_CONTROL,
};
use crate::view::{Reading, RecoveryControl, SwitchboardAction, TaskControl, Unmeasured};

/// A binary-unit byte count with one decimal digit; kept alongside the test
/// data so the expected pressure-card text is computed from the same
/// The meter state the run loop would hold after deriving and recording
/// exactly `samples`, in order — the same sequence the service performs.
fn meters_over(samples: &[Sample]) -> RollingMeters {
    let mut hysteresis = Hysteresis::new();
    let mut meters = RollingMeters::new();
    for sample in samples {
        let _ = derive_summary(sample, &mut hysteresis);
        meters.record(sample, hysteresis, &SessionReport::HEALTHY);
    }
    meters
}

fn meters_for(sample: &Sample) -> RollingMeters {
    meters_over(core::slice::from_ref(sample))
}

/// [`build_model`] with no activities and an unknown self-uid — the shape
/// most tests that do not touch pressure or activities need.
fn model(
    sample: &Sample,
    session: &SessionReport,
    meters: &mut RollingMeters,
    authority: &dyn tairix_abi::CapabilityQuery,
) -> super::PanelModel {
    build_model("Switchboard", sample, session, meters, authority)
}

#[test]
fn stopped_processes_become_recovery_rows() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    assert_eq!(panel.model.recovery.len(), 1);
    assert_eq!(panel.model.recovery[0].name, "stuck");
    assert!(panel.model.recovery[0].can_restart);
    assert!(!panel.model.recovery[0].can_force);
    assert_eq!(panel.recovery_owner(0), Some(7));
}

#[test]
fn recovery_force_is_allowed_only_with_the_capability() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &PROC_CONTROL,
    );
    assert!(panel.model.recovery[0].can_force);
}

#[test]
fn seat_report_owners_are_joined_against_sampled_names() {
    let sample = sample_with(alloc::vec![process(
        42,
        ProcessState::Running,
        b"hungapp",
        None
    )]);
    let report = SessionReport {
        seat: SeatReport::new(1, &[42]).expect("valid report"),
        frame: None,
    };
    let panel = model(&sample, &report, &mut meters_for(&sample), &NONE);
    assert_eq!(panel.model.recovery.len(), 1);
    assert_eq!(panel.model.recovery[0].name, "hungapp");
    assert_eq!(panel.recovery_owner(0), Some(42));
}

#[test]
fn an_unknown_reported_owner_does_not_fabricate_a_row() {
    let sample = sample_with(alloc::vec![process(
        1,
        ProcessState::Running,
        b"known",
        None
    )]);
    // Owner 99 was never sampled, so it cannot be named honestly.
    let report = SessionReport {
        seat: SeatReport::new(1, &[99]).expect("valid report"),
        frame: None,
    };
    let panel = model(&sample, &report, &mut meters_for(&sample), &NONE);
    assert!(panel.model.recovery.is_empty());
}

#[test]
fn a_stopped_process_also_named_by_the_seat_report_is_not_duplicated() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let report = SessionReport {
        seat: SeatReport::new(1, &[7]).expect("valid report"),
        frame: None,
    };
    let panel = model(&sample, &report, &mut meters_for(&sample), &NONE);
    assert_eq!(panel.model.recovery.len(), 1);
}

// --- Pressure section --------------------------------------------------

// --- Activities section --------------------------------------------------

// --- apply_action: existing actions -------------------------------------

#[test]
fn switch_and_reveal_both_ask_the_session_for_that_owner() {
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    for control in [TaskControl::Switch, TaskControl::Reveal] {
        let effect = apply_action(&panel, SwitchboardAction::Task { index: 0, control }, &NONE);
        assert_eq!(
            effect,
            alloc::vec![Effect::ActivateOwner { owner: 10 }],
            "{control:?} raises the task's own window"
        );
    }
}

#[test]
fn each_signalling_command_maps_to_its_own_signal() {
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &PROC_CONTROL,
    );
    for (control, expected) in [
        (
            TaskControl::Pause,
            Effect::Signal {
                pid: 10,
                signal: Signal::Stop,
            },
        ),
        (
            TaskControl::LowerPriority,
            Effect::LowerPriority { pid: 10 },
        ),
        (
            TaskControl::ForceQuit,
            Effect::Signal {
                pid: 10,
                signal: Signal::Kill,
            },
        ),
    ] {
        let effect = apply_action(
            &panel,
            SwitchboardAction::Task { index: 0, control },
            &PROC_CONTROL,
        );
        assert_eq!(effect, alloc::vec![expected], "{control:?}");
    }
}

#[test]
fn resume_only_reaches_a_stopped_task() {
    let running = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    let panel = model(
        &running,
        &SessionReport::HEALTHY,
        &mut meters_for(&running),
        &PROC_CONTROL,
    );
    assert!(
        apply_action(
            &panel,
            SwitchboardAction::Task {
                index: 0,
                control: TaskControl::Resume,
            },
            &PROC_CONTROL,
        )
        .is_empty(),
        "a running task has nothing to continue"
    );

    let stopped = sample_with(alloc::vec![process(
        10,
        ProcessState::Stopped,
        b"alpha",
        None
    )]);
    let panel = model(
        &stopped,
        &SessionReport::HEALTHY,
        &mut meters_for(&stopped),
        &PROC_CONTROL,
    );
    assert_eq!(
        apply_action(
            &panel,
            SwitchboardAction::Task {
                index: 0,
                control: TaskControl::Resume,
            },
            &PROC_CONTROL,
        ),
        alloc::vec![Effect::Signal {
            pid: 10,
            signal: Signal::Continue,
        }]
    );
}

#[test]
fn a_task_command_the_caller_may_not_use_produces_no_effect() {
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    // Built *and* dispatched without process control: the verdict the model
    // reached is the server-side check, so the effect never happens.
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    for control in [TaskControl::Pause, TaskControl::ForceQuit] {
        assert!(
            apply_action(&panel, SwitchboardAction::Task { index: 0, control }, &NONE,).is_empty(),
            "{control:?} must fail closed"
        );
    }
}

#[test]
fn open_logs_produces_no_effect_because_no_interface_exists() {
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &PROC_CONTROL,
    );
    assert!(apply_action(
        &panel,
        SwitchboardAction::Task {
            index: 0,
            control: TaskControl::OpenLogs,
        },
        &PROC_CONTROL,
    )
    .is_empty());
}

#[test]
fn an_out_of_range_task_index_produces_no_effect() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Task {
            index: 0,
            control: TaskControl::Switch,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn a_recovery_restart_action_maps_to_restart_owner() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Restart,
        },
        &NONE,
    );
    assert_eq!(effect, alloc::vec![Effect::RestartOwner { owner: 7 }]);
}

#[test]
fn a_recovery_force_action_signals_kill_when_authorised() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &PROC_CONTROL,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force,
        },
        &PROC_CONTROL,
    );
    assert_eq!(
        effect,
        alloc::vec![Effect::Signal {
            pid: 7,
            signal: Signal::Kill
        }]
    );
}

#[test]
fn a_recovery_force_action_is_never_attempted_without_the_capability() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn a_scroll_action_has_no_effect() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &mut meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::Scrolled { offset: 3 }, &NONE);
    assert!(effect.is_empty());
}

#[test]
fn a_task_id_within_the_syscall_width_narrows_unchanged() {
    assert_eq!(signal_pid(0), Some(0));
    assert_eq!(signal_pid(4321), Some(4321));
    // The widest id the kernel can draw still round-trips: pids span the
    // whole non-negative signed range, not a 32-bit window.
    let widest = i64::MAX.cast_unsigned();
    assert_eq!(signal_pid(widest), Some(i64::MAX));
}

#[test]
fn a_task_id_beyond_the_syscall_width_is_refused_never_truncated() {
    let beyond = i64::MAX.cast_unsigned() + 1;
    assert_eq!(signal_pid(beyond), None);
    assert_eq!(signal_pid(u64::MAX), None);
}

/// One second in nanoseconds — the interval the disk-rate fixtures sample
/// over, so a byte count and the per-second rate it produces read as the
/// same figure and the arithmetic is checkable by eye.
const ONE_SECOND_NS: u64 = 1_000_000_000;

/// One running process carrying storage counters, so a test can move a
/// task's I/O between samples and read the rate that produces.
fn process_with_io(pid: u64, cpu_permille: Option<u16>, read: u64, written: u64) -> ProcessSummary {
    ProcessSummary {
        io_bytes_read: read,
        io_bytes_written: written,
        ..process(pid, ProcessState::Running, b"task", cpu_permille)
    }
}

/// A sample of exactly `processes` taken `elapsed_ns` after the last one.
fn sample_over(elapsed_ns: Option<u64>, processes: Vec<ProcessSummary>) -> Sample {
    Sample {
        elapsed_ns,
        ..sample_with(processes)
    }
}

/// The per-task meter state after recording exactly `samples`, in order —
/// the same sequence the run loop performs each cycle.
fn task_meters_over(samples: &[Sample]) -> TaskMeters {
    let mut meters = TaskMeters::new();
    for sample in samples {
        meters.record(sample);
    }
    meters
}

/// The never-reused identity the process fixtures derive for task `pid`,
/// read back from a fixture rather than re-derived, so the tests key on
/// exactly what the sampler would produce.
fn ident(pid: u64) -> ProcId {
    process(pid, ProcessState::Running, b"task", None).proc_id
}

#[test]
fn the_first_sample_has_no_interval_to_measure_a_disk_rate_over() {
    let meters = task_meters_over(&[sample_over(
        Some(ONE_SECOND_NS),
        alloc::vec![process_with_io(7, Some(100), 4096, 2048)],
    )]);
    assert_eq!(
        meters.disk_rate(ident(7)),
        None,
        "a cumulative counter's first reading is a total, not a rate"
    );
}

#[test]
fn a_moved_counter_measures_its_bytes_over_the_sampled_interval() {
    let meters = task_meters_over(&[
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(100), 1_000, 500)],
        ),
        sample_over(
            Some(ONE_SECOND_NS / 2),
            alloc::vec![process_with_io(7, Some(100), 2_000, 1_000)],
        ),
    ]);
    assert_eq!(
        meters.disk_rate(ident(7)),
        Some(3_000),
        "1500 bytes read and written over half a second is 3000 a second"
    );
}

#[test]
fn a_counter_that_did_not_move_measures_zero_rather_than_nothing() {
    let idle = || {
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(100), 4096, 2048)],
        )
    };
    let meters = task_meters_over(&[idle(), idle()]);
    assert_eq!(
        meters.disk_rate(ident(7)),
        Some(0),
        "a task that did no I/O over a real interval genuinely did none, \
         which is a measurement and not an absence"
    );
}

#[test]
fn a_task_first_seen_this_sample_has_no_previous_reading_to_delta() {
    let meters = task_meters_over(&[
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(100), 1_000, 0)],
        ),
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![
                process_with_io(7, Some(100), 3_000, 0),
                process_with_io(9, Some(100), 8_000, 0),
            ],
        ),
    ]);
    assert_eq!(
        meters.disk_rate(ident(7)),
        Some(2_000),
        "the task that was already here is measured against its own last reading"
    );
    assert_eq!(
        meters.disk_rate(ident(9)),
        None,
        "a task seen for the first time has no earlier reading of its own, so \
         its lifetime total is never mistaken for a rate"
    );
}

#[test]
fn an_unmeasured_interval_reports_no_rate_rather_than_dividing_by_nothing() {
    for elapsed_ns in [None, Some(0)] {
        let meters = task_meters_over(&[
            sample_over(
                Some(ONE_SECOND_NS),
                alloc::vec![process_with_io(7, Some(100), 1_000, 0)],
            ),
            sample_over(
                elapsed_ns,
                alloc::vec![process_with_io(7, Some(100), 5_000, 0)],
            ),
        ]);
        assert_eq!(
            meters.disk_rate(ident(7)),
            None,
            "bytes over an interval nobody measured is not a rate"
        );
    }
}

#[test]
fn a_task_keeps_its_own_cpu_readings_in_the_order_they_were_measured() {
    let samples: Vec<Sample> = [100u16, 250, 400]
        .iter()
        .map(|permille| {
            sample_over(
                Some(ONE_SECOND_NS),
                alloc::vec![
                    process_with_io(7, Some(*permille), 0, 0),
                    process_with_io(9, Some(permille / 2), 0, 0),
                ],
            )
        })
        .collect();
    let meters = task_meters_over(&samples);
    assert_eq!(meters.cpu_history(ident(7)), &[100, 250, 400]);
    assert_eq!(
        meters.cpu_history(ident(9)),
        &[50, 125, 200],
        "each task's history is its own, keyed by its own identity"
    );
}

#[test]
fn a_sample_that_measured_no_share_adds_no_point_to_the_history() {
    let meters = task_meters_over(&[
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(100), 0, 0)],
        ),
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, None, 0, 0)],
        ),
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(300), 0, 0)],
        ),
    ]);
    assert_eq!(
        meters.cpu_history(ident(7)),
        &[100, 300],
        "an unmeasured share plots nothing, never a zero that reads as idle"
    );
}

#[test]
fn a_tasks_cpu_history_is_bounded_and_drops_its_oldest_reading() {
    let samples: Vec<Sample> = (0..TASK_HISTORY_LEN + 3)
        .map(|index| {
            let permille = u16::try_from(index).expect("small index");
            sample_over(
                Some(ONE_SECOND_NS),
                alloc::vec![process_with_io(7, Some(permille), 0, 0)],
            )
        })
        .collect();
    let meters = task_meters_over(&samples);
    let history = meters.cpu_history(ident(7));
    assert_eq!(
        history.len(),
        TASK_HISTORY_LEN,
        "a long-lived task's ring stays at its bound however long it runs"
    );
    assert_eq!(history.first(), Some(&3), "the oldest three fell out");
    let newest = u16::try_from(TASK_HISTORY_LEN + 2).expect("small index");
    assert_eq!(history.last(), Some(&newest));
}

#[test]
fn a_task_the_sample_no_longer_names_takes_its_history_and_counters_with_it() {
    let meters = task_meters_over(&[
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![
                process_with_io(7, Some(100), 1_000, 0),
                process_with_io(9, Some(200), 1_000, 0),
            ],
        ),
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![
                process_with_io(7, Some(150), 3_000, 0),
                process_with_io(9, Some(250), 3_000, 0),
            ],
        ),
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(175), 4_000, 0)],
        ),
    ]);
    assert_eq!(meters.cpu_history(ident(7)), &[100, 150, 175]);
    assert_eq!(
        meters.cpu_history(ident(9)),
        &[] as &[u16],
        "the exited task's history is gone the first sample it is absent from"
    );
    assert_eq!(
        meters.disk_rate(ident(9)),
        None,
        "and so are its counters, so a churn of short-lived tasks accumulates nothing"
    );
}

#[test]
fn a_returning_identity_starts_its_history_afresh() {
    let seen = |cpu| {
        sample_over(
            Some(ONE_SECOND_NS),
            alloc::vec![process_with_io(7, Some(cpu), 1_000, 0)],
        )
    };
    let meters = task_meters_over(&[
        seen(100),
        seen(200),
        sample_over(Some(ONE_SECOND_NS), Vec::new()),
        seen(900),
    ]);
    assert_eq!(
        meters.cpu_history(ident(7)),
        &[900],
        "nothing is carried across an absence, so a row never plots a stale span"
    );
    assert_eq!(
        meters.disk_rate(ident(7)),
        None,
        "and the counter it comes back with is a total again, not a delta"
    );
}

// --- The fault clock ---------------------------------------------------

/// A sample carrying `processes` and an uptime reading of `secs`, so the
/// fault clock has something to measure a fault's age against.
fn sample_at(secs: i64, processes: Vec<ProcessSummary>) -> Sample {
    Sample {
        processes,
        uptime: Some(Uptime {
            since_boot: Duration64::from_secs(secs),
            boot_time: Time64::from_secs(0),
        }),
        ..Sample::default()
    }
}

/// One stopped process, which the shared classifier resolves to a fault.
fn stopped(pid: u64) -> ProcessSummary {
    process(pid, ProcessState::Stopped, b"stuck", Some(10))
}

#[test]
fn a_faults_age_is_measured_from_when_it_was_first_seen() {
    let mut meters = RollingMeters::new();
    let hysteresis = Hysteresis::new();
    let first = sample_at(100, alloc::vec![stopped(7)]);
    meters.record(&first, hysteresis, &SessionReport::HEALTHY);
    let later = sample_at(160, alloc::vec![stopped(7)]);
    meters.record(&later, hysteresis, &SessionReport::HEALTHY);

    let proc_id = later.processes[0].proc_id;
    let elapsed = meters
        .faults
        .elapsed(proc_id, later.uptime.map(|up| up.since_boot))
        .expect("a fault seen twice has an age");
    assert_eq!(elapsed.secs(), 60);
}

#[test]
fn a_fault_with_no_uptime_reading_has_no_age_rather_than_a_zero() {
    let mut meters = RollingMeters::new();
    let sample = sample_with(alloc::vec![stopped(7)]);
    meters.record(&sample, Hysteresis::new(), &SessionReport::HEALTHY);
    assert_eq!(
        meters.faults.elapsed(sample.processes[0].proc_id, None),
        None
    );
}

#[test]
fn a_fault_that_clears_is_counted_and_forgotten() {
    let mut meters = RollingMeters::new();
    let hysteresis = Hysteresis::new();
    let faulted = sample_at(10, alloc::vec![stopped(7)]);
    meters.record(&faulted, hysteresis, &SessionReport::HEALTHY);
    assert_eq!(meters.faults.resolved(), 0);

    let healthy = sample_at(
        20,
        alloc::vec![process(7, ProcessState::Running, b"stuck", Some(10))],
    );
    meters.record(&healthy, hysteresis, &SessionReport::HEALTHY);
    assert_eq!(meters.faults.resolved(), 1);
    assert_eq!(
        meters.faults.elapsed(
            healthy.processes[0].proc_id,
            healthy.uptime.map(|up| up.since_boot)
        ),
        None,
        "a recovered task must not keep the age of the fault it left"
    );
}

#[test]
fn a_fault_that_recovers_and_faults_again_is_timed_from_the_new_fault() {
    let mut meters = RollingMeters::new();
    let hysteresis = Hysteresis::new();
    for (secs, state) in [
        (10, ProcessState::Stopped),
        (20, ProcessState::Running),
        (30, ProcessState::Stopped),
        (50, ProcessState::Stopped),
    ] {
        let sample = sample_at(secs, alloc::vec![process(7, state, b"stuck", Some(10))]);
        meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
    }
    let last = sample_at(50, alloc::vec![stopped(7)]);
    let elapsed = meters
        .faults
        .elapsed(
            last.processes[0].proc_id,
            last.uptime.map(|up| up.since_boot),
        )
        .expect("the second fault has its own age");
    assert_eq!(elapsed.secs(), 20);
}

// --- The pressure clock ------------------------------------------------

#[test]
fn the_models_resolved_count_is_the_clocks_count() {
    let mut meters = RollingMeters::new();
    let hysteresis = Hysteresis::new();
    meters.record(
        &sample_at(10, alloc::vec![stopped(7)]),
        hysteresis,
        &SessionReport::HEALTHY,
    );
    let healthy = sample_at(20, Vec::new());
    meters.record(&healthy, hysteresis, &SessionReport::HEALTHY);
    let built = model(&healthy, &SessionReport::HEALTHY, &mut meters, &NONE);
    assert_eq!(built.model.recovery_resolved, 1);
}

// --- The crash snapshot ------------------------------------------------

/// A crash record for `proc_id` with one named register and two frames.
fn crash_for(proc_id: ProcId, pid: u64) -> CrashRecord {
    let mut record = CrashRecord::new(
        proc_id,
        pid,
        DEFAULT_UID,
        0,
        true,
        CrashFaultClass::Wild,
        CrashFaultBucket::NullPage,
        8,
        b"stuck",
    )
    .expect("a short name fits");
    record.pc = 0x0040_1234;
    record.sp = 0x7ffe_0000;
    assert!(record.push_frame(0x0040_1234));
    assert!(record.push_frame(0x0040_5678));
    assert!(record
        .push_reg(CrashNamedReg::new(b"x0", 0xdead_beef).expect("a short register name fits")));
    record
}

#[test]
fn a_faults_crash_record_is_matched_by_process_identity() {
    let process = stopped(7);
    let proc_id = process.proc_id;
    let sample = Sample {
        processes: alloc::vec![process],
        crashes: Some(alloc::vec![crash_for(proc_id, 7)]),
        ..Sample::default()
    };
    let mut meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &mut meters, &NONE);
    let crash = built.model.recovery[0]
        .crash
        .as_ref()
        .expect("the sampled crash record belongs to this fault");
    assert_eq!(crash.frames, alloc::vec![0x0040_1234, 0x0040_5678]);
    assert_eq!(crash.registers.len(), 1);
    assert_eq!(crash.registers[0].0, "x0");
    assert_eq!(crash.registers[0].1, 0xdead_beef);
    assert_eq!(
        crash.access, "write",
        "the record's access direction must survive the join"
    );
    assert!(crash.location.contains("null page"), "{}", crash.location);
}

#[test]
fn an_instruction_side_crash_names_no_data_location() {
    let process = stopped(7);
    let proc_id = process.proc_id;
    let mut record = crash_for(proc_id, 7);
    record.fault_class = CrashFaultClass::Instruction;
    record.fault_bucket = CrashFaultBucket::NoDataAddress;
    record.fault_offset = 0;
    record.flags &= !tairix_abi::sysinfo::CRASH_FLAG_WRITE;
    let sample = Sample {
        processes: alloc::vec![process],
        crashes: Some(alloc::vec![record]),
        ..Sample::default()
    };
    let mut meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &mut meters, &NONE);
    let crash = built.model.recovery[0]
        .crash
        .as_ref()
        .expect("the sampled crash record belongs to this fault");
    assert_eq!(crash.access, "instruction (no data access)");
    assert!(crash.cause.contains("instruction"), "{}", crash.cause);
    assert!(
        crash.location.contains("no data address"),
        "{}",
        crash.location
    );
}

#[test]
fn a_crash_record_for_another_task_is_never_attributed_to_this_fault() {
    let process = stopped(7);
    let other = ProcId::from_raw([0xab; 16]);
    let sample = Sample {
        processes: alloc::vec![process],
        crashes: Some(alloc::vec![crash_for(other, 7)]),
        ..Sample::default()
    };
    let mut meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &mut meters, &NONE);
    assert!(
        built.model.recovery[0].crash.is_none(),
        "matching on the reused pid would attribute a dead task's crash to a live one"
    );
}

#[test]
fn a_fault_carries_its_own_resource_cost_with_network_unmeasured() {
    let sample = sample_with(alloc::vec![stopped(7)]);
    let mut meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &mut meters, &NONE);
    let item = &built.model.recovery[0];
    assert_eq!(item.cpu, Reading::measured("1%"));
    assert_eq!(item.memory, Reading::measured("0 B"));
    assert_eq!(
        item.network,
        Reading::Absent(Unmeasured::NoInterface),
        "no query reports a process's network use, so the tile must say so"
    );
}
