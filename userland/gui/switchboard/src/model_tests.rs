//! Unit tests for the live model builder and action-to-effect mapping.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport};
use tairix_abi::sysinfo::{
    CrashFaultBucket, CrashFaultClass, CrashNamedReg, CrashRecord, ProcessState, Uptime,
};
use tairix_abi::{Duration64, ProcId, SchedPriority, Signal, Time64, PROC_ID_LEN};
use tairix_controls::{ActivityState, PressureKind, MAX_CHART_SAMPLES};

use super::{
    apply_action, build_model, map_section, signal_pid, Effect, GroupingEdit, RollingMeters,
    SessionReport, TaskMeters, TASK_HISTORY_LEN,
};
use crate::activities::{Activities, Member};
use crate::derive::{derive_summary, Hysteresis, CPU_PRESSURE_ENTER_PERMILLE};
use crate::sample::{MemoryPressureSample, ProcessSummary, Sample};
use crate::test_host::{
    process_summary as process, process_summary_with, sample_with, DEFAULT_UID,
    NO_AUTHORITY as NONE, PROC_CONTROL_AUTHORITY as PROC_CONTROL,
};
use crate::view::{
    ActionVerdict, ActivityControl, PressureControl, Reading, RecoveryControl, Section,
    SwitchboardAction, TaskControl, TileInstrument, Unmeasured,
};

/// A binary-unit byte count with one decimal digit; kept alongside the test
/// data so the expected pressure-card text is computed from the same
/// literal constants a reader can check by eye.
const GIB: u64 = 1024 * 1024 * 1024;

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
    meters: &RollingMeters,
    authority: &dyn tairix_abi::CapabilityQuery,
) -> super::PanelModel {
    build_model(
        "Switchboard",
        sample,
        session,
        meters,
        authority,
        &Activities::new(),
        None,
    )
}

fn member(proc_id: ProcId, pid: u64, name: &str) -> Member {
    Member {
        proc_id,
        pid,
        name: String::from(name),
    }
}

#[test]
fn map_section_covers_every_wire_section() {
    assert_eq!(map_section(CommandSection::Tasks), Section::Tasks);
    assert_eq!(map_section(CommandSection::Jobs), Section::Jobs);
    assert_eq!(map_section(CommandSection::Pressure), Section::Pressure);
    assert_eq!(map_section(CommandSection::Activities), Section::Activities);
    assert_eq!(map_section(CommandSection::Recovery), Section::Recovery);
    assert_eq!(map_section(CommandSection::System), Section::System);
}

#[test]
fn tasks_are_built_in_sampled_order_with_a_switch_action() {
    let sample = sample_with(alloc::vec![
        process(10, ProcessState::Running, b"alpha", Some(500)),
        process(20, ProcessState::Running, b"beta", None),
    ]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    assert_eq!(panel.model.tasks.len(), 2);
    assert_eq!(panel.model.tasks[0].name, "alpha");
    assert_eq!(panel.model.tasks[0].cpu_permille, Some(500));
    // A live task's window may always be asked for; nothing else is, without
    // the process-control capability this caller does not hold.
    assert_eq!(
        panel.model.tasks[0].authority.verdict(TaskControl::Switch),
        ActionVerdict::Ready
    );
    for control in [
        TaskControl::Pause,
        TaskControl::Resume,
        TaskControl::LowerPriority,
        TaskControl::ForceQuit,
    ] {
        assert_eq!(
            panel.model.tasks[0].authority.verdict(control),
            ActionVerdict::DeniedByAuthority,
            "{control:?} needs process control"
        );
    }
    assert_eq!(panel.model.tasks[0].group, None);
    assert_eq!(panel.task_owner(0), Some(10));
    assert_eq!(panel.task_owner(1), Some(20));
    assert_eq!(panel.task_owner(2), None);
}

#[test]
fn a_task_grouped_into_an_activity_carries_its_index() {
    let sample = sample_with(alloc::vec![
        process(10, ProcessState::Running, b"alpha", None),
        process(20, ProcessState::Running, b"beta", None),
    ]);
    let real = sample.processes[0].proc_id;
    let mut activities = Activities::new();
    activities.create(member(real, 10, "alpha")).expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert_eq!(panel.model.tasks[0].group, Some(0));
    assert_eq!(panel.model.tasks[1].group, None);
}

#[test]
fn a_live_process_is_working_and_a_finished_or_stopped_one_is_idle() {
    let sample = sample_with(alloc::vec![
        process(1, ProcessState::Runnable, b"runnable", None),
        process(2, ProcessState::Running, b"running", None),
        process(3, ProcessState::Blocked, b"blocked", None),
        process(4, ProcessState::Zombie, b"zombie", None),
        process(5, ProcessState::Stopped, b"stopped", None),
    ]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let activities: Vec<ActivityState> =
        panel.model.tasks.iter().map(|task| task.activity).collect();
    assert_eq!(
        activities,
        alloc::vec![
            ActivityState::Working,
            ActivityState::Working,
            ActivityState::Working,
            ActivityState::Idle,
            ActivityState::Idle,
        ]
    );
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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
    let panel = model(&sample, &report, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &report, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &report, &meters_for(&sample), &NONE);
    assert_eq!(panel.model.recovery.len(), 1);
}

#[test]
fn an_unsampled_resource_reads_unknown_and_stays_unmeasured() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let cpu = &panel.model.system.headline[0];
    let memory = &panel.model.system.headline[1];
    assert_eq!(cpu.name, "CPU");
    assert_eq!(memory.name, "Memory");
    // An unsampled reading names why it is missing rather than showing a
    // zero, which would read as "idle" when the truth is "unknown".
    assert_eq!(cpu.value, Reading::Absent(Unmeasured::Unavailable));
    assert_eq!(memory.value, Reading::Absent(Unmeasured::NotPermitted));
    assert_eq!(memory.instrument, TileInstrument::Track(None));
    assert_eq!(cpu.instrument, TileInstrument::Trend(alloc::vec![]));
}

#[test]
fn a_sampled_resource_carries_its_measured_reading() {
    let sample = Sample {
        cpu_busy_permille: Some(624),
        memory_pressure: Some(MemoryPressureSample {
            band: 0,
            used_permille: 310,
            total_bytes: 0,
        }),
        ..Sample::default()
    };
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let cpu = &panel.model.system.headline[0];
    let memory = &panel.model.system.headline[1];
    assert_eq!(cpu.value, Reading::measured("62%"));
    assert_eq!(cpu.instrument, TileInstrument::Trend(alloc::vec![624]));
    assert_eq!(memory.value, Reading::measured("31%"));
    assert_eq!(memory.instrument, TileInstrument::Track(Some(310)));
}

#[test]
fn the_cpu_meter_carries_the_pressure_the_derivation_latched() {
    let sample = Sample {
        cpu_busy_permille: Some(CPU_PRESSURE_ENTER_PERMILLE),
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    assert!(meters.system.cpu_pressured());
    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let cpu = &panel.model.system.headline[0];
    assert_eq!(cpu.value, Reading::measured("90%"));
    assert!(
        cpu.pressured,
        "the header must carry the same latch the tray icon reads"
    );
    assert_eq!(
        cpu.instrument,
        TileInstrument::Trend(alloc::vec![CPU_PRESSURE_ENTER_PERMILLE])
    );
}

#[test]
fn the_memory_meter_carries_the_band_the_sampler_read() {
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 950,
            total_bytes: 0,
        }),
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    assert!(meters.system.memory_pressured());
    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let memory = &panel.model.system.headline[1];
    assert_eq!(memory.value, Reading::measured("95%"));
    assert!(
        memory.pressured,
        "the header must carry the same latch the tray icon reads"
    );
    assert_eq!(memory.instrument, TileInstrument::Track(Some(950)));
}

#[test]
fn the_cpu_history_records_every_measured_sample_in_order() {
    let samples: Vec<Sample> = [100u16, 200, 300]
        .iter()
        .map(|busy| Sample {
            cpu_busy_permille: Some(*busy),
            ..Sample::default()
        })
        .collect();
    let meters = meters_over(&samples);
    assert_eq!(meters.system.cpu_history(), &[100, 200, 300]);
}

#[test]
fn an_unmeasurable_interval_contributes_no_history_point() {
    let samples = alloc::vec![
        Sample {
            cpu_busy_permille: Some(120),
            ..Sample::default()
        },
        Sample::default(),
        Sample {
            cpu_busy_permille: Some(340),
            ..Sample::default()
        },
    ];
    let meters = meters_over(&samples);
    assert_eq!(meters.system.cpu_history(), &[120, 340]);
}

#[test]
fn the_cpu_history_is_bounded_and_drops_the_oldest_reading() {
    let samples: Vec<Sample> = (0..MAX_CHART_SAMPLES + 3)
        .map(|index| Sample {
            cpu_busy_permille: Some(u16::try_from(index).expect("small index")),
            ..Sample::default()
        })
        .collect();
    let meters = meters_over(&samples);
    assert_eq!(meters.system.cpu_history().len(), MAX_CHART_SAMPLES);
    assert_eq!(meters.system.cpu_history().first(), Some(&3));
    let last = u16::try_from(MAX_CHART_SAMPLES + 2).expect("small index");
    assert_eq!(meters.system.cpu_history().last(), Some(&last));
}

#[test]
fn jobs_services_and_system_actions_stay_honestly_empty() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    assert!(panel.model.jobs.is_empty());
    // There is no service registry to enumerate, so the screen states the
    // absence rather than showing an empty list that reads as "none".
    assert!(panel
        .model
        .system
        .actions
        .iter()
        .all(|action| { !action.allowed && action.refusal == Some(Unmeasured::NoInterface) }));
}

// --- Pressure section --------------------------------------------------

#[test]
fn no_pressure_cards_when_neither_latch_is_active() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    assert!(panel.model.pressure.is_empty());
}

#[test]
fn a_cpu_culprit_card_recommends_lowering_priority_for_its_own_uid() {
    let sample = sample_with(alloc::vec![process_summary_with(
        10,
        ProcessState::Running,
        b"hog",
        Some(900),
        DEFAULT_UID,
        0,
        SchedPriority::Normal,
    )]);
    let mut s = sample;
    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
    let meters = meters_for(&s);
    let panel = build_model(
        "Switchboard",
        &s,
        &SessionReport::HEALTHY,
        &meters,
        &NONE,
        &Activities::new(),
        Some(DEFAULT_UID),
    );
    assert_eq!(panel.model.pressure.len(), 1);
    let cause = &panel.model.pressure[0];
    assert_eq!(cause.resource, "CPU");
    assert_eq!(cause.kind, PressureKind::Cpu);
    assert_eq!(cause.culprit, "hog");
    assert_eq!(cause.cause, "Using 90% of the CPU over the last sample.");
    assert_eq!(cause.task_index, Some(0));
    let lower = cause
        .actions
        .iter()
        .find(|action| action.control == PressureControl::LowerPriority)
        .expect("a lower-priority action");
    assert_eq!(lower.verdict, ActionVerdict::Ready);
    assert!(lower.recommended);
    let pause = cause
        .actions
        .iter()
        .find(|action| action.control == PressureControl::Pause)
        .expect("a pause action");
    assert_eq!(pause.verdict, ActionVerdict::Ready);
    assert!(panel.model.pressure[0]
        .actions
        .iter()
        .any(|action| action.control == PressureControl::ShowTasks));
    assert_eq!(panel.pressure_target(0), Some(10));
}

#[test]
fn a_cpu_culprit_is_denied_without_authority_over_another_uid() {
    let sample = {
        let mut s = sample_with(alloc::vec![process_summary_with(
            10,
            ProcessState::Running,
            b"hog",
            Some(900),
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        )]);
        s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
        s
    };
    let meters = meters_for(&sample);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters,
        &NONE,
        &Activities::new(),
        Some(DEFAULT_UID + 1),
    );
    let cause = &panel.model.pressure[0];
    let lower = cause
        .actions
        .iter()
        .find(|action| action.control == PressureControl::LowerPriority)
        .expect("a lower-priority action");
    assert_eq!(lower.verdict, ActionVerdict::DeniedByAuthority);
}

#[test]
fn a_cpu_culprit_is_ready_across_uids_with_the_capability() {
    let sample = {
        let mut s = sample_with(alloc::vec![process_summary_with(
            10,
            ProcessState::Running,
            b"hog",
            Some(900),
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        )]);
        s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
        s
    };
    let meters = meters_for(&sample);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters,
        &PROC_CONTROL,
        &Activities::new(),
        Some(DEFAULT_UID + 1),
    );
    let cause = &panel.model.pressure[0];
    let lower = cause
        .actions
        .iter()
        .find(|action| action.control == PressureControl::LowerPriority)
        .expect("a lower-priority action");
    assert_eq!(lower.verdict, ActionVerdict::Ready);
}

#[test]
fn a_cpu_culprit_already_low_disables_lower_priority() {
    let sample = {
        let mut s = sample_with(alloc::vec![process_summary_with(
            10,
            ProcessState::Running,
            b"hog",
            Some(900),
            DEFAULT_UID,
            0,
            SchedPriority::Low,
        )]);
        s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
        s
    };
    let meters = meters_for(&sample);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters,
        &NONE,
        &Activities::new(),
        Some(DEFAULT_UID),
    );
    let cause = &panel.model.pressure[0];
    let lower = cause
        .actions
        .iter()
        .find(|action| action.control == PressureControl::LowerPriority)
        .expect("a lower-priority action");
    assert_eq!(lower.verdict, ActionVerdict::DisabledByState);
}

#[test]
fn a_stopped_cpu_culprit_disables_pause() {
    let sample = {
        let mut s = sample_with(alloc::vec![process_summary_with(
            10,
            ProcessState::Stopped,
            b"hog",
            Some(900),
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        )]);
        s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
        s
    };
    let meters = meters_for(&sample);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters,
        &NONE,
        &Activities::new(),
        Some(DEFAULT_UID),
    );
    let cause = &panel.model.pressure[0];
    let pause = cause
        .actions
        .iter()
        .find(|action| action.control == PressureControl::Pause)
        .expect("a pause action");
    assert_eq!(pause.verdict, ActionVerdict::DisabledByState);
}

#[test]
fn a_culprit_less_cpu_card_names_the_resource_when_no_rate_is_measured() {
    let sample = Sample {
        cpu_busy_permille: Some(CPU_PRESSURE_ENTER_PERMILLE),
        processes: alloc::vec![process(10, ProcessState::Running, b"unmeasured", None)],
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let cause = &panel.model.pressure[0];
    assert_eq!(cause.culprit, "CPU");
    assert_eq!(
        cause.cause,
        "The processor is saturated; per-task rates are not measured yet."
    );
    assert_eq!(cause.task_index, None);
    assert_eq!(cause.actions.len(), 1);
    assert_eq!(cause.actions[0].control, PressureControl::ShowTasks);
    assert_eq!(panel.pressure_target(0), None);
}

#[test]
fn a_memory_culprit_card_names_bytes_and_the_share_of_memory() {
    let mem_bytes = 2 * GIB;
    let total_bytes = 4 * GIB;
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 500,
            total_bytes,
        }),
        processes: alloc::vec![process_summary_with(
            10,
            ProcessState::Running,
            b"leaky",
            None,
            DEFAULT_UID,
            mem_bytes,
            SchedPriority::Normal,
        )],
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    assert_eq!(panel.model.pressure.len(), 1);
    let cause = &panel.model.pressure[0];
    assert_eq!(cause.resource, "Memory");
    assert_eq!(cause.culprit, "leaky");
    assert_eq!(cause.cause, "Using 2.0 GiB of RAM (50% of memory).");
    assert_eq!(cause.task_index, Some(0));
    assert_eq!(cause.actions.len(), 1);
    assert_eq!(cause.actions[0].control, PressureControl::ShowTasks);
    assert_eq!(cause.actions[0].verdict, ActionVerdict::Ready);
    assert!(cause.actions[0].recommended);
    assert_eq!(panel.pressure_target(0), Some(10));
}

#[test]
fn a_memory_culprit_card_omits_the_percent_clause_without_a_total() {
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 900,
            total_bytes: 0,
        }),
        processes: alloc::vec![process_summary_with(
            10,
            ProcessState::Running,
            b"leaky",
            None,
            DEFAULT_UID,
            640 * 1024,
            SchedPriority::Normal,
        )],
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let cause = &panel.model.pressure[0];
    assert_eq!(cause.cause, "Using 640.0 KiB of RAM.");
}

#[test]
fn a_culprit_less_memory_card_names_the_resource_when_nothing_measures_it() {
    let sample = Sample {
        memory_pressure: Some(MemoryPressureSample {
            band: 2,
            used_permille: 900,
            total_bytes: 0,
        }),
        processes: alloc::vec![process(10, ProcessState::Running, b"clean", None)],
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let cause = &panel.model.pressure[0];
    assert_eq!(cause.culprit, "Memory");
    assert_eq!(cause.cause, "Memory pressure is high.");
    assert_eq!(cause.task_index, None);
    assert_eq!(panel.pressure_target(0), None);
}

// --- Activities section --------------------------------------------------

#[test]
fn an_activity_summary_reports_its_id_name_and_member_count() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let real = sample.processes[0].proc_id;
    let mut activities = Activities::new();
    let id = activities.create(member(real, 10, "a")).expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert_eq!(panel.model.activities.len(), 1);
    let summary = &panel.model.activities[0];
    assert_eq!(summary.id, id);
    assert_eq!(summary.detail, "1 task");
    assert!(!summary.paused);
    assert_eq!(panel.activity_id(0), Some(id));
    assert_eq!(panel.activity_members(0), &[10]);
}

#[test]
fn an_activity_detail_is_plural_for_multiple_members() {
    let sample = sample_with(alloc::vec![
        process(10, ProcessState::Running, b"a", None),
        process(20, ProcessState::Running, b"b", None),
    ]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");
    activities
        .assign(0, member(sample.processes[1].proc_id, 20, "b"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert_eq!(panel.model.activities[0].detail, "2 tasks");
    assert_eq!(panel.activity_members(0), &[10, 20]);
}

#[test]
fn an_activitys_combined_reading_totals_its_joined_members() {
    let sample = sample_with(alloc::vec![
        process_summary_with(
            10,
            ProcessState::Running,
            b"a",
            Some(120),
            DEFAULT_UID,
            2 * GIB,
            SchedPriority::Normal,
        ),
        process_summary_with(
            20,
            ProcessState::Running,
            b"b",
            Some(80),
            DEFAULT_UID,
            GIB,
            SchedPriority::Normal,
        ),
    ]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");
    activities
        .assign(0, member(sample.processes[1].proc_id, 20, "b"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    let summary = &panel.model.activities[0];

    // The group's cost is the sum of what its own members were measured at —
    // there is no per-group accounting to read instead.
    assert_eq!(summary.cpu, Reading::measured("20%"), "120‰ + 80‰");
    assert_eq!(
        summary.memory,
        Reading::measured("3.0 GiB"),
        "2 GiB + 1 GiB"
    );
    assert_eq!(
        summary.network,
        Reading::Absent(Unmeasured::NoInterface),
        "no per-task network accounting exists to total"
    );
}

#[test]
fn an_activity_total_is_absent_when_a_member_reading_is() {
    // The second member's CPU share was never read, so the group's CPU total
    // is absent rather than understating the group by skipping it.
    let sample = sample_with(alloc::vec![
        process(10, ProcessState::Running, b"a", Some(120)),
        process(20, ProcessState::Running, b"b", None),
    ]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");
    activities
        .assign(0, member(sample.processes[1].proc_id, 20, "b"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert!(
        matches!(panel.model.activities[0].cpu, Reading::Absent(_)),
        "one unread part makes the whole total absent, not a smaller measurement"
    );
    assert!(
        matches!(panel.model.activities[0].memory, Reading::Measured(_)),
        "memory is read for every process, so its total still stands"
    );
}

#[test]
fn an_activity_with_no_running_member_has_no_total_at_all() {
    // The group's member has exited, so nothing in the sample supports a
    // total: reporting nought would claim the group costs nothing.
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"other",
        Some(500)
    )]);
    let mut activities = Activities::new();
    activities
        .create(member(ProcId::from_raw([7u8; PROC_ID_LEN]), 99, "gone"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    let summary = &panel.model.activities[0];
    assert!(matches!(summary.cpu, Reading::Absent(_)));
    assert!(matches!(summary.memory, Reading::Absent(_)));
    assert!(matches!(summary.disk, Reading::Absent(_)));
    assert!(!summary.members[0].joined, "its member is not running");
}

#[test]
fn an_activity_is_working_when_a_member_is_working_and_not_paused() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert_eq!(panel.model.activities[0].activity, ActivityState::Working);
}

#[test]
fn a_paused_activity_is_idle_even_with_a_working_member() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");
    activities.set_paused(0, true);

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert_eq!(panel.model.activities[0].activity, ActivityState::Idle);
    assert!(panel.model.activities[0].paused);
}

#[test]
fn can_control_is_true_for_a_same_uid_member_without_the_capability() {
    let sample = sample_with(alloc::vec![process_summary_with(
        10,
        ProcessState::Running,
        b"a",
        None,
        DEFAULT_UID,
        0,
        SchedPriority::Normal,
    )]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        Some(DEFAULT_UID),
    );
    assert!(panel.model.activities[0].can_control);
}

#[test]
fn can_control_is_false_for_a_foreign_uid_member_without_the_capability() {
    let sample = sample_with(alloc::vec![process_summary_with(
        10,
        ProcessState::Running,
        b"a",
        None,
        DEFAULT_UID,
        0,
        SchedPriority::Normal,
    )]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        Some(DEFAULT_UID + 1),
    );
    assert!(!panel.model.activities[0].can_control);

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &PROC_CONTROL,
        &activities,
        Some(DEFAULT_UID + 1),
    );
    assert!(panel.model.activities[0].can_control);
}

#[test]
fn an_unjoined_member_falls_back_to_its_stored_name_and_is_idle() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let vanished = ProcId::from_raw([9; 16]);
    let mut activities = Activities::new();
    activities
        .create(member(vanished, 99, "gone"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    let summary = &panel.model.activities[0];
    assert_eq!(summary.members.len(), 1);
    assert_eq!(summary.members[0].name, "gone");
    assert_eq!(summary.members[0].detail, "");
    assert_eq!(summary.members[0].activity, ActivityState::Idle);
    // An unjoined member cannot be signalled; it never appears in the
    // targets an activity action would act on.
    assert!(panel.activity_members(0).is_empty());
}

#[test]
fn can_accept_member_reflects_the_member_bound() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");

    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert!(panel.model.activities[0].can_accept_member);
}

#[test]
fn can_create_activity_reflects_activities_can_create() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    assert!(panel.model.can_create_activity);
}

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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
        &meters_for(&running),
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
        &meters_for(&stopped),
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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
        &meters_for(&sample),
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
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::Scrolled { offset: 3 }, &NONE);
    assert!(effect.is_empty());
}

#[test]
fn a_task_id_within_the_syscall_width_narrows_unchanged() {
    assert_eq!(signal_pid(0), Some(0));
    assert_eq!(signal_pid(4321), Some(4321));
    let widest = u64::try_from(i32::MAX).expect("i32::MAX fits a u64");
    assert_eq!(signal_pid(widest), Some(i32::MAX));
}

#[test]
fn a_task_id_beyond_the_syscall_width_is_refused_never_truncated() {
    let beyond = u64::try_from(i32::MAX).expect("i32::MAX fits a u64") + 1;
    assert_eq!(signal_pid(beyond), None);
    assert_eq!(signal_pid(u64::MAX), None);
}

// --- apply_action: pressure ----------------------------------------------

fn cpu_pressure_panel(
    priority: SchedPriority,
    state: ProcessState,
    self_uid: Option<u32>,
    authority: &dyn tairix_abi::CapabilityQuery,
) -> super::PanelModel {
    let sample = Sample {
        cpu_busy_permille: Some(CPU_PRESSURE_ENTER_PERMILLE),
        processes: alloc::vec![process_summary_with(
            10,
            state,
            b"hog",
            Some(900),
            DEFAULT_UID,
            0,
            priority,
        )],
        ..Sample::default()
    };
    build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        authority,
        &Activities::new(),
        self_uid,
    )
}

#[test]
fn apply_action_lowers_priority_when_ready() {
    let panel = cpu_pressure_panel(
        SchedPriority::Normal,
        ProcessState::Running,
        Some(DEFAULT_UID),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::LowerPriority,
        },
        &NONE,
    );
    assert_eq!(effect, alloc::vec![Effect::LowerPriority { pid: 10 }]);
}

#[test]
fn apply_action_refuses_to_lower_an_already_low_priority() {
    let panel = cpu_pressure_panel(
        SchedPriority::Low,
        ProcessState::Running,
        Some(DEFAULT_UID),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::LowerPriority,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn apply_action_refuses_pause_without_authority() {
    let panel = cpu_pressure_panel(
        SchedPriority::Normal,
        ProcessState::Running,
        Some(DEFAULT_UID + 1),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::Pause,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn apply_action_pauses_the_culprit_when_ready() {
    let panel = cpu_pressure_panel(
        SchedPriority::Normal,
        ProcessState::Running,
        Some(DEFAULT_UID),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::Pause,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![Effect::Signal {
            pid: 10,
            signal: Signal::Stop
        }]
    );
}

#[test]
fn apply_action_show_tasks_never_produces_an_effect() {
    let panel = cpu_pressure_panel(
        SchedPriority::Normal,
        ProcessState::Running,
        Some(DEFAULT_UID),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::ShowTasks,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn apply_action_pressure_index_out_of_range_is_empty() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Pressure {
            index: 0,
            control: PressureControl::Pause,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

// --- apply_action: grouping ------------------------------------------------

#[test]
fn task_grouped_into_a_new_activity_yields_an_assign_edit() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::TaskGrouped {
            task: 0,
            activity: None,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![Effect::Grouping(GroupingEdit::Assign {
            task: 0,
            activity: None
        })]
    );
}

#[test]
fn task_grouped_with_an_out_of_range_task_is_empty() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::TaskGrouped {
            task: 0,
            activity: None,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn task_grouped_with_an_out_of_range_activity_is_empty() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::TaskGrouped {
            task: 0,
            activity: Some(0),
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn task_ungrouped_yields_an_unassign_edit() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::TaskUngrouped { task: 0 }, &NONE);
    assert_eq!(
        effect,
        alloc::vec![Effect::Grouping(GroupingEdit::Unassign { task: 0 })]
    );
}

#[test]
fn task_ungrouped_out_of_range_is_empty() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::TaskUngrouped { task: 0 }, &NONE);
    assert!(effect.is_empty());
}

#[test]
fn activity_renamed_yields_a_rename_edit() {
    let sample = sample_with(alloc::vec![process(10, ProcessState::Running, b"a", None)]);
    let mut activities = Activities::new();
    activities
        .create(member(sample.processes[0].proc_id, 10, "a"))
        .expect("room");
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::ActivityRenamed { index: 0 },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![Effect::Grouping(GroupingEdit::Rename { activity: 0 })]
    );
}

#[test]
fn activity_renamed_out_of_range_is_empty() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::ActivityRenamed { index: 0 },
        &NONE,
    );
    assert!(effect.is_empty());
}

fn activity_panel(
    paused: bool,
    self_uid: Option<u32>,
    authority: &dyn tairix_abi::CapabilityQuery,
) -> (super::PanelModel, ProcId) {
    let sample = sample_with(alloc::vec![
        process_summary_with(
            10,
            ProcessState::Running,
            b"a",
            None,
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        ),
        process_summary_with(
            20,
            ProcessState::Running,
            b"b",
            None,
            DEFAULT_UID,
            0,
            SchedPriority::Normal,
        ),
    ]);
    let real_a = sample.processes[0].proc_id;
    let real_b = sample.processes[1].proc_id;
    let mut activities = Activities::new();
    activities.create(member(real_a, 10, "a")).expect("room");
    activities.assign(0, member(real_b, 20, "b")).expect("room");
    activities.set_paused(0, paused);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        authority,
        &activities,
        self_uid,
    );
    (panel, real_a)
}

#[test]
fn activity_switch_activates_every_joined_member_in_group_order() {
    let (panel, _) = activity_panel(false, Some(DEFAULT_UID), &NONE);
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Switch,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![Effect::ActivateOwners {
            owners: alloc::vec![10, 20]
        }]
    );
}

#[test]
fn activity_pause_signals_and_edits_when_controllable() {
    let (panel, _) = activity_panel(false, Some(DEFAULT_UID), &NONE);
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Pause,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![
            Effect::SignalMany {
                pids: alloc::vec![10, 20],
                signal: Signal::Stop
            },
            Effect::Grouping(GroupingEdit::SetPaused {
                activity: 0,
                paused: true
            }),
        ]
    );
}

#[test]
fn activity_pause_is_refused_without_control() {
    let (panel, _) = activity_panel(false, Some(DEFAULT_UID + 1), &NONE);
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Pause,
        },
        &NONE,
    );
    assert!(effect.is_empty());
}

#[test]
fn activity_resume_signals_continue_and_clears_paused() {
    let (panel, _) = activity_panel(true, Some(DEFAULT_UID), &NONE);
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Resume,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![
            Effect::SignalMany {
                pids: alloc::vec![10, 20],
                signal: Signal::Continue
            },
            Effect::Grouping(GroupingEdit::SetPaused {
                activity: 0,
                paused: false
            }),
        ]
    );
}

#[test]
fn activity_close_terminates_joined_members_and_closes() {
    let (panel, _) = activity_panel(false, Some(DEFAULT_UID), &NONE);
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Close,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![
            Effect::SignalMany {
                pids: alloc::vec![10, 20],
                signal: Signal::Terminate
            },
            Effect::Grouping(GroupingEdit::Close { activity: 0 }),
        ]
    );
}

#[test]
fn activity_close_skips_a_member_not_joined_to_the_current_sample() {
    let sample = sample_with(alloc::vec![process_summary_with(
        10,
        ProcessState::Running,
        b"a",
        None,
        DEFAULT_UID,
        0,
        SchedPriority::Normal,
    )]);
    let real_a = sample.processes[0].proc_id;
    let vanished = ProcId::from_raw([9; 16]);
    let mut activities = Activities::new();
    activities.create(member(real_a, 10, "a")).expect("room");
    activities
        .assign(0, member(vanished, 99, "gone"))
        .expect("room");
    let panel = build_model(
        "Switchboard",
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        Some(DEFAULT_UID),
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Close,
        },
        &NONE,
    );
    assert_eq!(
        effect,
        alloc::vec![
            Effect::SignalMany {
                pids: alloc::vec![10],
                signal: Signal::Terminate
            },
            Effect::Grouping(GroupingEdit::Close { activity: 0 }),
        ]
    );
}

#[test]
fn activity_index_out_of_range_is_empty() {
    let sample = Sample::default();
    let panel = model(
        &sample,
        &SessionReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Activity {
            index: 0,
            control: ActivityControl::Switch,
        },
        &NONE,
    );
    assert!(effect.is_empty());
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

/// One busy CPU hog, so a band has a culprit to blame.
fn cpu_hog() -> ProcessSummary {
    process_summary_with(
        10,
        ProcessState::Running,
        b"hog",
        Some(900),
        DEFAULT_UID,
        0,
        SchedPriority::Normal,
    )
}

/// A sample taken `secs` after boot whose machine-wide CPU busy share is
/// `busy` — the reading the pressure band is actually latched from.
fn banded_sample_at(secs: i64, busy: u16) -> Sample {
    Sample {
        cpu_busy_permille: Some(busy),
        ..sample_at(secs, alloc::vec![cpu_hog()])
    }
}

#[test]
fn a_pressure_bands_age_is_measured_from_when_the_band_was_entered() {
    let mut meters = RollingMeters::new();
    let mut hysteresis = Hysteresis::new();
    let mut record = |secs: i64, meters: &mut RollingMeters| {
        let sample = banded_sample_at(secs, CPU_PRESSURE_ENTER_PERMILLE);
        let _ = derive_summary(&sample, &mut hysteresis);
        meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
        sample
    };
    record(100, &mut meters);
    let later = record(160, &mut meters);

    let elapsed = meters
        .pressure
        .cpu_elapsed(later.uptime.map(|up| up.since_boot))
        .expect("a band held across two samples has an age");
    assert_eq!(
        elapsed.secs(),
        60,
        "the band is aged from the sample that first saw it, not from this one"
    );

    // The card carries that same measurement rather than deriving a second
    // opinion of its own.
    let panel = model(&later, &SessionReport::HEALTHY, &meters, &NONE);
    let cause = panel
        .model
        .pressure
        .iter()
        .find(|cause| cause.kind == PressureKind::Cpu)
        .expect("the CPU band is flagged");
    assert_eq!(cause.since, Reading::measured("1m"));
}

#[test]
fn a_pressure_band_with_no_uptime_reading_has_no_age_rather_than_a_zero() {
    let mut meters = RollingMeters::new();
    let mut hysteresis = Hysteresis::new();
    // No uptime reading at all, so there is no clock to measure the band
    // against — which the card must state, not paper over with a nought.
    let sample = Sample {
        cpu_busy_permille: Some(CPU_PRESSURE_ENTER_PERMILLE),
        ..sample_with(alloc::vec![cpu_hog()])
    };
    let _ = derive_summary(&sample, &mut hysteresis);
    meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
    assert_eq!(meters.pressure.cpu_elapsed(None), None);

    let panel = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let cause = panel
        .model
        .pressure
        .iter()
        .find(|cause| cause.kind == PressureKind::Cpu)
        .expect("the CPU band is flagged");
    assert!(
        matches!(cause.since, Reading::Absent(_)),
        "an age nobody could measure is stated as absent, never as 0s"
    );
}

#[test]
fn a_band_that_eases_and_returns_is_timed_from_the_new_band() {
    let mut meters = RollingMeters::new();
    let mut hysteresis = Hysteresis::new();
    for (secs, busy) in [
        (10, CPU_PRESSURE_ENTER_PERMILLE),
        (20, 0),
        (30, CPU_PRESSURE_ENTER_PERMILLE),
        (50, CPU_PRESSURE_ENTER_PERMILLE),
    ] {
        let sample = banded_sample_at(secs, busy);
        let _ = derive_summary(&sample, &mut hysteresis);
        meters.record(&sample, hysteresis, &SessionReport::HEALTHY);
    }
    let elapsed = meters
        .pressure
        .cpu_elapsed(Some(Duration64::from_secs(50)))
        .expect("the second band has its own age");
    assert_eq!(elapsed.secs(), 20, "the eased band's start is forgotten");
}

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
    let built = model(&healthy, &SessionReport::HEALTHY, &meters, &NONE);
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
    let meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
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

/// An instruction-side kill reads as one: it names no data location and is
/// never rendered as a read, which is what a bare write bit would have
/// made of it.
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
    let meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
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
    let meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    assert!(
        built.model.recovery[0].crash.is_none(),
        "matching on the reused pid would attribute a dead task's crash to a live one"
    );
}

#[test]
fn a_fault_carries_its_own_resource_cost_with_network_unmeasured() {
    let sample = sample_with(alloc::vec![stopped(7)]);
    let meters = meters_for(&sample);
    let built = model(&sample, &SessionReport::HEALTHY, &meters, &NONE);
    let item = &built.model.recovery[0];
    assert_eq!(item.cpu, Reading::measured("1%"));
    assert_eq!(item.memory, Reading::measured("0 B"));
    assert_eq!(
        item.network,
        Reading::Absent(Unmeasured::NoInterface),
        "no query reports a process's network use, so the tile must say so"
    );
}
