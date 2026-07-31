//! Unit tests for [`derive_summary`].

use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{TrayPressureKind, TRAY_TASK_NAME_MAX};

use crate::sample::{MemoryPressureSample, Sample, TopTask};

use super::{derive_summary, Hysteresis, CPU_PRESSURE_ENTER_PERMILLE, CPU_PRESSURE_EXIT_PERMILLE};

fn sample() -> Sample {
    Sample::default()
}

#[test]
fn jobs_is_always_the_honest_zero() {
    let summary = derive_summary(&sample(), &mut Hysteresis::new());
    assert_eq!(summary.jobs, 0);
}

#[test]
fn recovery_is_the_stopped_count() {
    let mut s = sample();
    s.stopped_count = 4;
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert_eq!(summary.recovery, 4);
}

#[test]
fn cpu_busy_permille_falls_back_to_zero_when_unmeasured() {
    let summary = derive_summary(&sample(), &mut Hysteresis::new());
    assert_eq!(summary.cpu_busy_permille.as_u16(), 0);
}

#[test]
fn cpu_busy_permille_reflects_the_measured_value() {
    let mut s = sample();
    s.cpu_busy_permille = Some(420);
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert_eq!(summary.cpu_busy_permille.as_u16(), 420);
}

#[test]
fn cpu_pressure_enters_at_the_threshold_and_not_below_it() {
    let mut hysteresis = Hysteresis::new();
    let mut s = sample();
    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE - 1);
    let summary = derive_summary(&s, &mut hysteresis);
    assert!(summary.pressure.is_none());

    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
    let summary = derive_summary(&s, &mut hysteresis);
    let pressure = summary.pressure.expect("CPU pressure entered");
    assert_eq!(pressure.kind, TrayPressureKind::Cpu);
    assert_eq!(pressure.level.as_u16(), CPU_PRESSURE_ENTER_PERMILLE);
    assert_eq!(pressure.count.as_u8(), 1);
}

#[test]
fn cpu_pressure_has_hysteresis_between_enter_and_exit() {
    let mut hysteresis = Hysteresis::new();
    let mut s = sample();
    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
    let summary = derive_summary(&s, &mut hysteresis);
    assert!(summary.pressure.is_some());

    // Dropping into the hysteresis gap (below enter, at or above exit)
    // must not clear the pressure yet.
    s.cpu_busy_permille = Some(CPU_PRESSURE_EXIT_PERMILLE);
    let summary = derive_summary(&s, &mut hysteresis);
    assert!(summary.pressure.is_some(), "still latched inside the gap");

    // Dropping below the exit threshold clears it.
    s.cpu_busy_permille = Some(CPU_PRESSURE_EXIT_PERMILLE - 1);
    let summary = derive_summary(&s, &mut hysteresis);
    assert!(
        summary.pressure.is_none(),
        "cleared below the exit threshold"
    );
}

#[test]
fn cpu_pressure_does_not_flap_at_a_hovering_load() {
    let mut hysteresis = Hysteresis::new();
    let mut s = sample();
    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
    assert!(derive_summary(&s, &mut hysteresis).pressure.is_some());

    // A load that oscillates between the enter and exit thresholds stays
    // latched active throughout (that is the point of the hysteresis gap).
    for _ in 0..5 {
        s.cpu_busy_permille = Some(CPU_PRESSURE_EXIT_PERMILLE);
        assert!(derive_summary(&s, &mut hysteresis).pressure.is_some());
        s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
        assert!(derive_summary(&s, &mut hysteresis).pressure.is_some());
    }
}

#[test]
fn memory_pressure_is_reported_at_and_above_the_band_threshold() {
    let mut s = sample();
    s.memory_pressure = Some(MemoryPressureSample {
        band: 0,
        used_permille: 999,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert!(
        summary.pressure.is_none(),
        "band 0 (normal) is not pressure"
    );

    s.memory_pressure = Some(MemoryPressureSample {
        band: 1,
        used_permille: 850,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    let pressure = summary.pressure.expect("mild band is pressure");
    assert_eq!(pressure.kind, TrayPressureKind::Memory);
    assert_eq!(pressure.level.as_u16(), 850);
    assert_eq!(pressure.count.as_u8(), 1);
}

#[test]
fn both_pressures_report_the_higher_level_dominant_with_count_two() {
    let mut hysteresis = Hysteresis::new();
    let mut s = sample();
    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
    s.memory_pressure = Some(MemoryPressureSample {
        band: 2,
        used_permille: 999,
    });
    let summary = derive_summary(&s, &mut hysteresis);
    let pressure = summary.pressure.expect("both pressured");
    assert_eq!(pressure.kind, TrayPressureKind::Memory);
    assert_eq!(pressure.count.as_u8(), 2);
}

#[test]
fn a_tie_between_both_pressures_favours_cpu() {
    let mut hysteresis = Hysteresis::new();
    let mut s = sample();
    s.cpu_busy_permille = Some(CPU_PRESSURE_ENTER_PERMILLE);
    s.memory_pressure = Some(MemoryPressureSample {
        band: 1,
        used_permille: CPU_PRESSURE_ENTER_PERMILLE,
    });
    let summary = derive_summary(&s, &mut hysteresis);
    let pressure = summary.pressure.expect("both pressured");
    assert_eq!(pressure.kind, TrayPressureKind::Cpu);
    assert_eq!(pressure.count.as_u8(), 2);
}

#[test]
fn a_valid_top_task_name_survives_derivation() {
    let mut s = sample();
    s.top_task = Some(TopTask {
        name: Vec::from(*b"compositor"),
        cpu_permille: 310,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    let top = summary.top_task.expect("a valid name derives a top task");
    assert_eq!(top.name.as_str(), "compositor");
    assert_eq!(top.cpu_permille.as_u16(), 310);
}

#[test]
fn an_empty_top_task_name_yields_no_top_task() {
    let mut s = sample();
    s.top_task = Some(TopTask {
        name: Vec::new(),
        cpu_permille: 100,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert!(summary.top_task.is_none());
}

#[test]
fn an_invalid_utf8_top_task_name_yields_no_top_task() {
    let mut s = sample();
    s.top_task = Some(TopTask {
        name: alloc::vec![0xFFu8, 0xFE],
        cpu_permille: 100,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert!(summary.top_task.is_none());
}

#[test]
fn an_over_long_top_task_name_yields_no_top_task() {
    let mut s = sample();
    s.top_task = Some(TopTask {
        name: alloc::vec![b'n'; TRAY_TASK_NAME_MAX + 1],
        cpu_permille: 100,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert!(summary.top_task.is_none());
}

#[test]
fn a_control_character_in_the_top_task_name_yields_no_top_task() {
    let mut s = sample();
    s.top_task = Some(TopTask {
        name: alloc::vec![b'a', b'\n', b'b'],
        cpu_permille: 100,
    });
    let summary = derive_summary(&s, &mut Hysteresis::new());
    assert!(summary.top_task.is_none());
}

#[test]
fn no_sample_top_task_yields_no_summary_top_task() {
    let summary = derive_summary(&sample(), &mut Hysteresis::new());
    assert!(summary.top_task.is_none());
}
