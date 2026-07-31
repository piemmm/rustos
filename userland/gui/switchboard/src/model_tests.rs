//! Unit tests for the live model builder and action-to-effect mapping.

use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{CommandSection, SeatReport};
use tairix_abi::sysinfo::ProcessState;
use tairix_abi::Signal;
use tairix_controls::{
    ActivityState, MeterValue, PressureKind, PressureState, ProgressValue, RecoveryControl,
    ResourceSummary, Section, SwitchboardAction, MAX_HISTORY_SAMPLES,
};

use super::{apply_action, build_model, map_section, signal_pid, Effect, LiveMeters};
use crate::derive::{derive_summary, Hysteresis, CPU_PRESSURE_ENTER_PERMILLE};
use crate::sample::{MemoryPressureSample, Sample};
use crate::test_host::{
    process_summary as process, sample_with, NO_AUTHORITY as NONE,
    PROC_CONTROL_AUTHORITY as PROC_CONTROL,
};

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

#[test]
fn map_section_covers_all_four_sections() {
    assert_eq!(map_section(CommandSection::Tasks), Section::Tasks);
    assert_eq!(map_section(CommandSection::Jobs), Section::Jobs);
    assert_eq!(map_section(CommandSection::Recovery), Section::Recovery);
    assert_eq!(map_section(CommandSection::Overview), Section::Overview);
}

#[test]
fn tasks_are_built_in_sampled_order_with_a_switch_action() {
    let sample = sample_with(alloc::vec![
        process(10, ProcessState::Running, b"alpha", Some(500)),
        process(20, ProcessState::Running, b"beta", None),
    ]);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    assert_eq!(panel.model.tasks.len(), 2);
    assert_eq!(panel.model.tasks[0].name, "alpha");
    assert_eq!(panel.model.tasks[0].detail, "50%");
    assert_eq!(panel.model.tasks[0].action, "Switch");
    assert!(panel.model.tasks[0].action_allowed);
    assert_eq!(panel.task_owner(0), Some(10));
    assert_eq!(panel.task_owner(1), Some(20));
    assert_eq!(panel.task_owner(2), None);
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
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
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
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
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
    let panel = build_model(
        "Switchboard",
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
    let panel = build_model("Switchboard", &sample, &report, &meters_for(&sample), &NONE);
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
    let panel = build_model("Switchboard", &sample, &report, &meters_for(&sample), &NONE);
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
    let panel = build_model("Switchboard", &sample, &report, &meters_for(&sample), &NONE);
    assert_eq!(panel.model.recovery.len(), 1);
}

#[test]
fn an_unsampled_resource_reads_unknown_and_stays_unmeasured() {
    let sample = Sample::default();
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
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
        }),
        ..Sample::default()
    };
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
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
    let panel = build_model("Switchboard", &sample, &SeatReport::HEALTHY, &meters, &NONE);
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
        }),
        ..Sample::default()
    };
    let meters = meters_for(&sample);
    assert!(meters.memory_pressured());
    let panel = build_model("Switchboard", &sample, &SeatReport::HEALTHY, &meters, &NONE);
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
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    assert!(panel.model.jobs.is_empty());
    assert!(panel.model.services.is_empty());
    assert!(panel.model.system_actions.is_empty());
}

#[test]
fn a_task_action_maps_to_activate_owner() {
    let sample = sample_with(alloc::vec![process(
        10,
        ProcessState::Running,
        b"alpha",
        None
    )]);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::Task { index: 0 }, &NONE);
    assert_eq!(effect, Effect::ActivateOwner { owner: 10 });
}

#[test]
fn an_out_of_range_task_index_produces_no_effect() {
    let sample = Sample::default();
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::Task { index: 0 }, &NONE);
    assert_eq!(effect, Effect::None);
}

#[test]
fn a_recovery_restart_action_maps_to_restart_owner() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
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
    assert_eq!(effect, Effect::RestartOwner { owner: 7 });
}

#[test]
fn a_recovery_force_action_signals_kill_when_authorised() {
    let sample = sample_with(alloc::vec![process(
        7,
        ProcessState::Stopped,
        b"stuck",
        None
    )]);
    let panel = build_model(
        "Switchboard",
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
        Effect::Signal {
            pid: 7,
            signal: Signal::Kill
        }
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
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
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
    assert_eq!(effect, Effect::None);
}

#[test]
fn a_close_window_action_maps_to_close_window() {
    let sample = Sample::default();
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(
        &panel,
        SwitchboardAction::Window(tairix_controls::WindowControlKind::Close),
        &NONE,
    );
    assert_eq!(effect, Effect::CloseWindow);
}

#[test]
fn a_scroll_action_has_no_effect() {
    let sample = Sample::default();
    let panel = build_model(
        "Switchboard",
        &sample,
        &SeatReport::HEALTHY,
        &meters_for(&sample),
        &NONE,
    );
    let effect = apply_action(&panel, SwitchboardAction::Scrolled { offset: 3 }, &NONE);
    assert_eq!(effect, Effect::None);
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
