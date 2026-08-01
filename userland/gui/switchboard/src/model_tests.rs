//! Unit tests for the live model builder and action-to-effect mapping.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport};
use tairix_abi::sysinfo::ProcessState;
use tairix_abi::{ProcId, SchedPriority, Signal};
use tairix_controls::{
    ActionVerdict, ActivityControl, ActivityState, MeterValue, PressureControl, PressureKind,
    PressureState, ProgressValue, RecoveryControl, ResourceSummary, Section, SwitchboardAction,
    MAX_HISTORY_SAMPLES,
};

use super::{apply_action, build_model, map_section, signal_pid, Effect, GroupingEdit, LiveMeters};
use crate::activities::{Activities, Member};
use crate::derive::{derive_summary, Hysteresis, CPU_PRESSURE_ENTER_PERMILLE};
use crate::sample::{MemoryPressureSample, Sample};
use crate::test_host::{
    process_summary as process, process_summary_with, sample_with, DEFAULT_UID,
    NO_AUTHORITY as NONE, PROC_CONTROL_AUTHORITY as PROC_CONTROL,
};

/// A binary-unit byte count with one decimal digit; kept alongside the test
/// data so the expected pressure-card text is computed from the same
/// literal constants a reader can check by eye.
const GIB: u64 = 1024 * 1024 * 1024;

/// The meter state the run loop would hold after deriving and recording
/// exactly `samples`, in order — the same sequence the service performs.
fn meters_over(samples: &[Sample]) -> LiveMeters {
    let mut hysteresis = Hysteresis::new();
    let mut meters = LiveMeters::new();
    for sample in samples {
        let _ = derive_summary(sample, &mut hysteresis);
        meters.record(sample, hysteresis);
    }
    meters
}

fn meters_for(sample: &Sample) -> LiveMeters {
    meters_over(core::slice::from_ref(sample))
}

/// [`build_model`] with no activities and an unknown self-uid — the shape
/// most tests that do not touch pressure or activities need.
fn model(
    sample: &Sample,
    seat_report: &SeatReport,
    meters: &LiveMeters,
    authority: &dyn tairix_abi::CapabilityQuery,
) -> super::PanelModel {
    build_model(
        "Switchboard",
        sample,
        seat_report,
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
    assert_eq!(map_section(CommandSection::Overview), Section::Overview);
}

#[test]
fn tasks_are_built_in_sampled_order_with_a_switch_action() {
    let sample = sample_with(alloc::vec![
        process(10, ProcessState::Running, b"alpha", Some(500)),
        process(20, ProcessState::Running, b"beta", None),
    ]);
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    assert_eq!(panel.model.tasks.len(), 2);
    assert_eq!(panel.model.tasks[0].name, "alpha");
    assert_eq!(panel.model.tasks[0].detail, "50%");
    assert_eq!(panel.model.tasks[0].action, "Switch");
    assert!(panel.model.tasks[0].action_allowed);
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
        &SeatReport::HEALTHY,
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
    let report = SeatReport::new(1, &[42]).expect("valid report");
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
    let report = SeatReport::new(1, &[99]).expect("valid report");
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
    let report = SeatReport::new(1, &[7]).expect("valid report");
    let panel = model(&sample, &report, &meters_for(&sample), &NONE);
    assert_eq!(panel.model.recovery.len(), 1);
}

#[test]
fn an_unsampled_resource_reads_unknown_and_stays_unmeasured() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    assert_eq!(
        panel.model.resources,
        alloc::vec![
            ResourceSummary::new("CPU", "unknown", PressureKind::Cpu, ActivityState::Idle),
            ResourceSummary::new(
                "Memory",
                "unknown",
                PressureKind::Memory,
                ActivityState::Idle
            ),
        ]
    );
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    assert_eq!(
        panel.model.resources,
        alloc::vec![
            ResourceSummary::new("CPU", "62%", PressureKind::Cpu, ActivityState::Working)
                .with_meter(
                    MeterValue::Measured(ProgressValue::new(624)),
                    PressureState::None,
                    [624u16],
                ),
            ResourceSummary::new(
                "Memory",
                "31%",
                PressureKind::Memory,
                ActivityState::Working
            )
            .with_meter(
                MeterValue::Measured(ProgressValue::new(310)),
                PressureState::None,
                core::iter::empty(),
            ),
        ]
    );
}

#[test]
fn the_cpu_meter_carries_the_pressure_the_derivation_latched() {
    let sample = Sample {
        cpu_busy_permille: Some(CPU_PRESSURE_ENTER_PERMILLE),
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    assert!(meters.cpu_pressured());
    let panel = model(&sample, &SeatReport::HEALTHY, &meters, &NONE);
    assert_eq!(
        panel.model.resources[0],
        ResourceSummary::new("CPU", "90%", PressureKind::Cpu, ActivityState::Working).with_meter(
            MeterValue::Measured(ProgressValue::new(CPU_PRESSURE_ENTER_PERMILLE)),
            PressureState::Under(PressureKind::Cpu),
            [CPU_PRESSURE_ENTER_PERMILLE],
        )
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
    assert!(meters.memory_pressured());
    let panel = model(&sample, &SeatReport::HEALTHY, &meters, &NONE);
    assert_eq!(
        panel.model.resources[1],
        ResourceSummary::new(
            "Memory",
            "95%",
            PressureKind::Memory,
            ActivityState::Working
        )
        .with_meter(
            MeterValue::Measured(ProgressValue::new(950)),
            PressureState::Under(PressureKind::Memory),
            core::iter::empty(),
        )
    );
}

#[test]
fn the_cpu_sparkline_records_every_measured_sample_in_order() {
    let samples: Vec<Sample> = [100u16, 200, 300]
        .iter()
        .map(|busy| Sample {
            cpu_busy_permille: Some(*busy),
            ..Sample::default()
        })
        .collect();
    let meters = meters_over(&samples);
    assert_eq!(meters.cpu_history(), &[100, 200, 300]);
}

#[test]
fn an_unmeasurable_interval_contributes_no_sparkline_bar() {
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
    assert_eq!(meters.cpu_history(), &[120, 340]);
}

#[test]
fn the_cpu_sparkline_is_bounded_and_drops_the_oldest_bar() {
    let samples: Vec<Sample> = (0..MAX_HISTORY_SAMPLES + 3)
        .map(|index| Sample {
            cpu_busy_permille: Some(u16::try_from(index).expect("small index")),
            ..Sample::default()
        })
        .collect();
    let meters = meters_over(&samples);
    assert_eq!(meters.cpu_history().len(), MAX_HISTORY_SAMPLES);
    assert_eq!(meters.cpu_history().first(), Some(&3));
    let last = u16::try_from(MAX_HISTORY_SAMPLES + 2).expect("small index");
    assert_eq!(meters.cpu_history().last(), Some(&last));
}

#[test]
fn jobs_services_and_system_actions_stay_honestly_empty() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    assert!(panel.model.jobs.is_empty());
    assert!(panel.model.services.is_empty());
    assert!(panel.model.system_actions.is_empty());
}

// --- Pressure section --------------------------------------------------

#[test]
fn no_pressure_cards_when_neither_latch_is_active() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters, &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters, &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters, &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters, &NONE);
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        None,
    );
    assert_eq!(panel.model.activities[0].detail, "2 tasks");
    assert_eq!(panel.activity_members(0), &[10, 20]);
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
        &activities,
        Some(DEFAULT_UID + 1),
    );
    assert!(!panel.model.activities[0].can_control);

    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    assert!(panel.model.can_create_activity);
}

// --- apply_action: existing actions -------------------------------------

#[test]
fn a_task_action_maps_to_activate_owner() {
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    let effect = apply_action(&panel, SwitchboardAction::Task { index: 0 }, &NONE);
    assert_eq!(effect, alloc::vec![Effect::ActivateOwner { owner: 10 }]);
}

#[test]
fn an_out_of_range_task_index_produces_no_effect() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    let effect = apply_action(&panel, SwitchboardAction::Task { index: 0 }, &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
fn a_close_window_action_maps_to_close_window() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    let effect = apply_action(
        &panel,
        SwitchboardAction::Window(tairix_controls::WindowControlKind::Close),
        &NONE,
    );
    assert_eq!(effect, alloc::vec![Effect::CloseWindow]);
}

#[test]
fn a_scroll_action_has_no_effect() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
    let effect = apply_action(&panel, SwitchboardAction::TaskUngrouped { task: 0 }, &NONE);
    assert_eq!(
        effect,
        alloc::vec![Effect::Grouping(GroupingEdit::Unassign { task: 0 })]
    );
}

#[test]
fn task_ungrouped_out_of_range_is_empty() {
    let sample = Sample::default();
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
        &SeatReport::HEALTHY,
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
        &SeatReport::HEALTHY,
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
    let panel = model(&sample, &SeatReport::HEALTHY, &meters_for(&sample), &NONE);
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
