//! Unit tests for the Switchboard tray-summary IPC protocol.

use super::{
    SwitchboardRequest, TrayPermille, TrayPressure, TrayPressureCount, TrayPressureKind,
    TraySummary, TrayTask, TrayTaskName, TRAY_PRESSURE_KIND_COUNT, TRAY_TASK_NAME_MAX,
};
use crate::Errno;

fn bare_summary() -> TraySummary {
    TraySummary {
        jobs: 3,
        recovery: 0,
        cpu_busy_permille: TrayPermille::new(420).expect("within bounds"),
        pressure: None,
        top_task: None,
    }
}

fn full_summary() -> TraySummary {
    TraySummary {
        jobs: 2,
        recovery: 1,
        cpu_busy_permille: TrayPermille::new(875).expect("within bounds"),
        pressure: Some(TrayPressure {
            kind: TrayPressureKind::Memory,
            level: TrayPermille::new(910).expect("within bounds"),
            count: TrayPressureCount::new(2).expect("within bounds"),
        }),
        top_task: Some(TrayTask {
            name: TrayTaskName::new("compositor").expect("a valid name"),
            cpu_permille: TrayPermille::new(310).expect("within bounds"),
        }),
    }
}

fn round_trip(summary: TraySummary) -> TraySummary {
    let request = SwitchboardRequest::PublishSummary { summary };
    match SwitchboardRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame") {
        SwitchboardRequest::PublishSummary { summary } => summary,
    }
}

#[test]
fn round_trips_no_pressure_and_no_top_task() {
    assert_eq!(round_trip(bare_summary()), bare_summary());
}

#[test]
fn round_trips_pressure_and_top_task_together() {
    assert_eq!(round_trip(full_summary()), full_summary());
}

#[test]
fn round_trips_pressure_alone() {
    let mut summary = bare_summary();
    summary.pressure = Some(TrayPressure {
        kind: TrayPressureKind::Thermal,
        level: TrayPermille::FULL,
        count: TrayPressureCount::ONE,
    });
    assert_eq!(round_trip(summary), summary);
}

#[test]
fn round_trips_top_task_alone() {
    let mut summary = bare_summary();
    summary.top_task = Some(TrayTask {
        name: TrayTaskName::new("x").expect("a valid one-byte name"),
        cpu_permille: TrayPermille::ZERO,
    });
    assert_eq!(round_trip(summary), summary);
}

#[test]
fn round_trips_boundary_permilles() {
    for value in [0u16, 1000] {
        let mut summary = bare_summary();
        summary.cpu_busy_permille = TrayPermille::new(value).expect("boundary is valid");
        assert_eq!(round_trip(summary), summary);
    }
}

#[test]
fn round_trips_full_width_top_task_name() {
    let mut summary = bare_summary();
    summary.top_task = Some(TrayTask {
        name: TrayTaskName::new(&"n".repeat(TRAY_TASK_NAME_MAX)).expect("max-length name"),
        cpu_permille: TrayPermille::new(1000).expect("within bounds"),
    });
    assert_eq!(round_trip(summary), summary);
}

#[test]
fn every_pressure_kind_round_trips() {
    for kind in [
        TrayPressureKind::Cpu,
        TrayPressureKind::Memory,
        TrayPressureKind::Disk,
        TrayPressureKind::Network,
        TrayPressureKind::Power,
        TrayPressureKind::Thermal,
    ] {
        let mut summary = bare_summary();
        summary.pressure = Some(TrayPressure {
            kind,
            level: TrayPermille::new(500).expect("within bounds"),
            count: TrayPressureCount::ONE,
        });
        assert_eq!(round_trip(summary), summary);
    }
}

#[test]
fn round_trips_boundary_pressure_counts() {
    for count in [1u8, TRAY_PRESSURE_KIND_COUNT] {
        let mut summary = bare_summary();
        summary.pressure = Some(TrayPressure {
            kind: TrayPressureKind::Cpu,
            level: TrayPermille::new(700).expect("within bounds"),
            count: TrayPressureCount::new(count).expect("boundary is valid"),
        });
        assert_eq!(round_trip(summary), summary);
    }
}

#[test]
fn pressure_count_constructor_rejects_zero_and_above_the_kind_count() {
    assert_eq!(TrayPressureCount::new(0), Err(Errno::OutOfRange));
    assert_eq!(
        TrayPressureCount::new(TRAY_PRESSURE_KIND_COUNT + 1),
        Err(Errno::OutOfRange)
    );
    assert!(TrayPressureCount::new(TRAY_PRESSURE_KIND_COUNT).is_ok());
}

#[test]
fn permille_constructor_fails_closed_above_full() {
    assert_eq!(TrayPermille::new(1001), Err(Errno::OutOfRange));
    assert!(TrayPermille::new(1000).is_ok());
}

#[test]
fn pressure_kind_rejects_the_reserved_zero_and_unknown_bytes() {
    assert_eq!(TrayPressureKind::from_u8(0), Err(Errno::OutOfRange));
    assert_eq!(TrayPressureKind::from_u8(7), Err(Errno::OutOfRange));
    assert_eq!(
        TrayPressureKind::from_u8(TrayPressureKind::Cpu.as_u8()),
        Ok(TrayPressureKind::Cpu)
    );
}

#[test]
fn rejects_short_buffer() {
    let frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame[..frame.len() - 1]),
        Err(Errno::BufferTooSmall)
    );
}

#[test]
fn rejects_bad_magic_version_and_op() {
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[0] ^= 0xFF;
    assert_eq!(SwitchboardRequest::from_bytes(&frame), Err(Errno::BadMagic));

    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[4] = 0xFF;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::AbiVersionUnsupported)
    );

    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    // Operation 9 is outside the closed set.
    frame[6] = 9;
    frame[7] = 0;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn rejects_out_of_range_pressure_kind() {
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::PRESSURE_KIND_OFFSET] = 7;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn rejects_out_of_range_pressure_count_beside_a_named_pressure() {
    // A named pressure counts at least itself; zero is out of range.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::PRESSURE_COUNT_OFFSET] = 0;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );

    // More pressured resources than the closed kind set holds.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::PRESSURE_COUNT_OFFSET] = TRAY_PRESSURE_KIND_COUNT + 1;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn rejects_out_of_range_permilles() {
    // The three independent permille fields: overall CPU, pressure
    // level, and top-task CPU each fail closed above 1000.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::CPU_BUSY_OFFSET] = 0xE9;
    frame[super::CPU_BUSY_OFFSET + 1] = 0x03; // 1001
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );

    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::PRESSURE_LEVEL_OFFSET] = 0xE9;
    frame[super::PRESSURE_LEVEL_OFFSET + 1] = 0x03;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );

    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::TOP_TASK_CPU_OFFSET] = 0xE9;
    frame[super::TOP_TASK_CPU_OFFSET + 1] = 0x03;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn rejects_over_long_and_malformed_top_task_name() {
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::TOP_TASK_NAME_LEN_OFFSET] =
        u8::try_from(TRAY_TASK_NAME_MAX + 1).expect("32 + 1 fits a u8");
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::LengthOutOfRange)
    );

    // A control character in the declared name bytes is refused, never
    // sanitised.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: full_summary(),
    }
    .to_le_bytes();
    frame[super::TOP_TASK_NAME_OFFSET] = b'\n';
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn rejects_dirty_reserved_pressure_and_top_task_fields_both_ways() {
    // "No pressure" (kind byte 0) with a non-zero level is dirty.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[super::PRESSURE_LEVEL_OFFSET] = 5;
    assert_eq!(SwitchboardRequest::from_bytes(&frame), Err(Errno::BadMagic));

    // "No pressure" (kind byte 0) with a non-zero count is dirty: the
    // badge may never claim pressured resources the rail does not show.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[super::PRESSURE_COUNT_OFFSET] = 2;
    assert_eq!(SwitchboardRequest::from_bytes(&frame), Err(Errno::BadMagic));

    // "No top task" (name length 0) with non-zero name bytes is dirty.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[super::TOP_TASK_NAME_OFFSET] = b'x';
    assert_eq!(SwitchboardRequest::from_bytes(&frame), Err(Errno::BadMagic));

    // "No top task" (name length 0) with a non-zero CPU fraction is
    // dirty.
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[super::TOP_TASK_CPU_OFFSET] = 1;
    assert_eq!(SwitchboardRequest::from_bytes(&frame), Err(Errno::BadMagic));

    // The clean encoding of `None` is confirmed zero in both directions:
    // a fresh encode of an absent pressure/top-task leaves every one of
    // these bytes zero, and that zeroed frame decodes back to `None`.
    let clean = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    assert_eq!(clean[super::PRESSURE_KIND_OFFSET], 0);
    assert_eq!(clean[super::PRESSURE_COUNT_OFFSET], 0);
    assert_eq!(clean[super::PRESSURE_LEVEL_OFFSET], 0);
    assert_eq!(clean[super::PRESSURE_LEVEL_OFFSET + 1], 0);
    assert_eq!(clean[super::TOP_TASK_NAME_LEN_OFFSET], 0);
    assert_eq!(clean[super::TOP_TASK_CPU_OFFSET], 0);
    assert_eq!(clean[super::TOP_TASK_CPU_OFFSET + 1], 0);
    match SwitchboardRequest::from_bytes(&clean).expect("a well-formed frame") {
        SwitchboardRequest::PublishSummary { summary } => {
            assert_eq!(summary.pressure, None);
            assert_eq!(summary.top_task, None);
        }
    }
}
