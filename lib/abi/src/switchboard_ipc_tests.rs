//! Unit tests for the Switchboard tray-summary IPC protocol.

use super::{
    command_endpoint_for, decode_publish_reply, encode_publish_reply, CommandSection, FrameReport,
    SeatReport, SwitchboardCommand, SwitchboardRequest, TrayPermille, TrayPressure,
    TrayPressureCount, TrayPressureKind, TraySummary, TrayTask, TrayTaskName,
    SEAT_REPORT_OWNERS_MAX, SWITCHBOARD_PUBLISH_REPLY_LEN, TRAY_PRESSURE_KIND_COUNT,
    TRAY_TASK_NAME_MAX,
};
use crate::power::PowerAction;
use crate::{Errno, ProcId};

fn bare_summary() -> TraySummary {
    TraySummary {
        jobs: 3,
        recovery: 0,
        cpu_busy_permille: TrayPermille::new(420).expect("within bounds"),
        pressure: None,
        top_task: None,
        power_capable: false,
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
        power_capable: true,
    }
}

fn round_trip(summary: TraySummary) -> TraySummary {
    let request = SwitchboardRequest::PublishSummary { summary };
    match SwitchboardRequest::from_bytes(&request.to_le_bytes()).expect("a well-formed frame") {
        SwitchboardRequest::PublishSummary { summary } => summary,
        other => panic!("a publish frame must decode as a publish request, got {other:?}"),
    }
}

#[test]
fn round_trips_no_pressure_and_no_top_task() {
    assert_eq!(round_trip(bare_summary()), bare_summary());
}

#[test]
fn round_trips_the_power_capable_flag_both_ways() {
    let mut capable = bare_summary();
    capable.power_capable = true;
    assert_eq!(round_trip(capable), capable);

    let mut denied = bare_summary();
    denied.power_capable = false;
    assert_eq!(round_trip(denied), denied);
}

#[test]
fn rejects_an_out_of_range_power_capable_byte() {
    let mut frame = SwitchboardRequest::PublishSummary {
        summary: bare_summary(),
    }
    .to_le_bytes();
    frame[super::POWER_CAPABLE_OFFSET] = 2;
    assert_eq!(
        SwitchboardRequest::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
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
        other => panic!("a publish frame must decode as a publish request, got {other:?}"),
    }
}

#[test]
fn round_trips_the_owner_directed_operations() {
    for request in [
        SwitchboardRequest::ActivateOwner { owner: 1 },
        SwitchboardRequest::ActivateOwner { owner: u64::MAX },
        SwitchboardRequest::RestartOwner { owner: 42 },
    ] {
        assert_eq!(
            SwitchboardRequest::from_bytes(&request.to_le_bytes()),
            Ok(request)
        );
    }
}

#[test]
fn owner_directed_operations_reject_the_reserved_zero_owner() {
    // Task id zero names no process, so a frame carrying it is refused
    // rather than resolved against whatever the session happens to hold.
    for request in [
        SwitchboardRequest::ActivateOwner { owner: 0 },
        SwitchboardRequest::RestartOwner { owner: 0 },
    ] {
        assert_eq!(
            SwitchboardRequest::from_bytes(&request.to_le_bytes()),
            Err(Errno::OutOfRange)
        );
    }
}

#[test]
fn owner_directed_operations_reject_a_dirty_summary_payload() {
    // The summary block is reserved on an owner-directed operation: a
    // sender that smuggles bytes there is malformed, not merely verbose.
    for tail in [super::OWNER_TAIL_OFFSET, SwitchboardRequest::WIRE_LEN - 1] {
        let mut frame = SwitchboardRequest::ActivateOwner { owner: 7 }.to_le_bytes();
        frame[tail] = 1;
        assert_eq!(SwitchboardRequest::from_bytes(&frame), Err(Errno::BadMagic));
    }

    // A clean owner-directed encoding leaves that whole span zero.
    let clean = SwitchboardRequest::RestartOwner { owner: 7 }.to_le_bytes();
    assert!(clean[super::OWNER_TAIL_OFFSET..]
        .iter()
        .all(|&byte| byte == 0));
}

#[test]
fn publish_reply_round_trips_the_serving_session_identity() {
    let session = ProcId::from_raw([9u8; crate::PROC_ID_LEN]);
    let reply = encode_publish_reply(session);
    assert_eq!(reply.len(), SWITCHBOARD_PUBLISH_REPLY_LEN);
    assert_eq!(decode_publish_reply(&reply), Ok(session));
}

#[test]
fn publish_reply_fails_closed_on_a_refusal_short_buffer_and_the_kernel_sentinel() {
    let session = ProcId::from_raw([1u8; crate::PROC_ID_LEN]);
    let reply = encode_publish_reply(session);
    assert_eq!(
        decode_publish_reply(&reply[..SWITCHBOARD_PUBLISH_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    // A refusal is the plain status frame; the identity is never invented
    // from the zeroed tail of a short reply.
    let mut refusal = [0u8; SWITCHBOARD_PUBLISH_REPLY_LEN];
    refusal[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(
        Errno::PermissionDenied,
    )));
    assert_eq!(decode_publish_reply(&refusal), Err(Errno::PermissionDenied));

    // A success naming the kernel sentinel could never authenticate a
    // command, so it is refused where it is decoded rather than stored.
    assert_eq!(
        decode_publish_reply(&encode_publish_reply(ProcId::KERNEL)),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn command_endpoint_is_derived_from_the_task_id_and_is_never_reserved() {
    assert_ne!(command_endpoint_for(1), command_endpoint_for(2));
    for pid in [1u64, 2, 97, u64::from(u32::MAX), crate::PID_MAX] {
        let endpoint = command_endpoint_for(pid);
        assert!(!crate::ipc::is_reserved_endpoint(endpoint));
        assert!(!crate::ipc::is_seat_scoped_endpoint(endpoint));
        // The tag is a pure prefix, right up to the widest pid the kernel can
        // draw: the task id is recoverable, so two instances can never
        // collide on one mailbox.
        assert_eq!(endpoint & crate::PID_MAX, pid);
    }
}

#[test]
fn command_section_rejects_the_reserved_zero_and_unknown_bytes() {
    for section in [
        CommandSection::Tasks,
        CommandSection::Resources,
        CommandSection::Recovery,
    ] {
        assert_eq!(CommandSection::from_u8(section.as_u8()), Ok(section));
    }
    for byte in [0u8, 7, 0xFF] {
        assert_eq!(CommandSection::from_u8(byte), Err(Errno::OutOfRange));
    }
}

#[test]
fn seat_report_constructor_refuses_a_contradictory_report() {
    assert_eq!(SeatReport::HEALTHY.total(), 0);
    assert!(SeatReport::HEALTHY.owners().is_empty());

    // More names than the frame can carry.
    let too_many: [u64; SEAT_REPORT_OWNERS_MAX + 1] =
        core::array::from_fn(|index| index as u64 + 1);
    assert_eq!(
        SeatReport::new(u16::MAX, &too_many),
        Err(Errno::LengthOutOfRange)
    );

    // A total below the number of names cannot be true of any seat.
    assert_eq!(SeatReport::new(1, &[7, 8]), Err(Errno::OutOfRange));

    // Task id zero names no owner, and a repeated owner is not a set.
    assert_eq!(SeatReport::new(2, &[7, 0]), Err(Errno::OutOfRange));
    assert_eq!(SeatReport::new(2, &[7, 7]), Err(Errno::OutOfRange));

    // A truthful report may name fewer owners than it counts.
    let partial = SeatReport::new(9, &[3, 4]).expect("a truthful report");
    assert_eq!(partial.total(), 9);
    assert_eq!(partial.owners(), &[3, 4]);
}

#[test]
fn round_trips_open_panel_for_every_section() {
    for section in [
        CommandSection::Tasks,
        CommandSection::Resources,
        CommandSection::Recovery,
    ] {
        let command = SwitchboardCommand::OpenPanel { section };
        assert_eq!(
            SwitchboardCommand::from_bytes(&command.to_le_bytes()),
            Ok(command)
        );
    }
}

#[test]
fn round_trips_every_power_action() {
    for action in [PowerAction::PowerOff, PowerAction::Restart] {
        let command = SwitchboardCommand::Power { action };
        assert_eq!(
            SwitchboardCommand::from_bytes(&command.to_le_bytes()),
            Ok(command)
        );
    }
}

#[test]
fn power_command_rejects_a_dirty_tail_and_an_unknown_action() {
    let mut frame = SwitchboardCommand::Power {
        action: PowerAction::PowerOff,
    }
    .to_le_bytes();
    frame[super::POWER_ACTION_OFFSET + 4] = 1;
    assert_eq!(SwitchboardCommand::from_bytes(&frame), Err(Errno::BadMagic));

    let mut frame = SwitchboardCommand::Power {
        action: PowerAction::Restart,
    }
    .to_le_bytes();
    frame[super::POWER_ACTION_OFFSET] = 9;
    frame[super::POWER_ACTION_OFFSET + 1] = 0;
    frame[super::POWER_ACTION_OFFSET + 2] = 0;
    frame[super::POWER_ACTION_OFFSET + 3] = 0;
    assert_eq!(
        SwitchboardCommand::from_bytes(&frame),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn round_trips_seat_reports_at_the_bounds() {
    let full: [u64; SEAT_REPORT_OWNERS_MAX] = core::array::from_fn(|index| index as u64 + 1);
    for report in [
        SeatReport::HEALTHY,
        SeatReport::new(1, &[u64::MAX]).expect("a truthful report"),
        SeatReport::new(u16::MAX, &full).expect("a truthful report"),
    ] {
        let command = SwitchboardCommand::SeatReport { report };
        assert_eq!(
            SwitchboardCommand::from_bytes(&command.to_le_bytes()),
            Ok(command)
        );
    }
}

#[test]
fn command_rejects_short_buffer_bad_magic_version_and_op() {
    let frame = SwitchboardCommand::OpenPanel {
        section: CommandSection::Tasks,
    }
    .to_le_bytes();
    assert_eq!(
        SwitchboardCommand::from_bytes(&frame[..frame.len() - 1]),
        Err(Errno::BufferTooSmall)
    );

    // A request frame is not a command frame: the two directions carry
    // distinct magics so a misdirected send can never be misread.
    let mut wrong_magic = frame;
    wrong_magic[0] ^= 0xFF;
    assert_eq!(
        SwitchboardCommand::from_bytes(&wrong_magic),
        Err(Errno::BadMagic)
    );
    let mut misdirected = [0u8; SwitchboardCommand::WIRE_LEN];
    let request = SwitchboardRequest::ActivateOwner { owner: 1 }.to_le_bytes();
    misdirected[..request.len()].copy_from_slice(&request);
    assert_eq!(
        SwitchboardCommand::from_bytes(&misdirected),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = frame;
    wrong_version[4] = 0xFF;
    assert_eq!(
        SwitchboardCommand::from_bytes(&wrong_version),
        Err(Errno::AbiVersionUnsupported)
    );

    let mut unknown_op = frame;
    unknown_op[6] = 9;
    unknown_op[7] = 0;
    assert_eq!(
        SwitchboardCommand::from_bytes(&unknown_op),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn command_rejects_dirty_reserved_fields_and_an_over_long_owner_count() {
    // An open-panel frame carries nothing but its section byte.
    let mut frame = SwitchboardCommand::OpenPanel {
        section: CommandSection::Recovery,
    }
    .to_le_bytes();
    frame[SwitchboardCommand::WIRE_LEN - 1] = 1;
    assert_eq!(SwitchboardCommand::from_bytes(&frame), Err(Errno::BadMagic));

    // A seat report carries no section byte.
    let mut frame = SwitchboardCommand::SeatReport {
        report: SeatReport::new(1, &[5]).expect("a truthful report"),
    }
    .to_le_bytes();
    frame[super::SECTION_OFFSET] = 1;
    assert_eq!(SwitchboardCommand::from_bytes(&frame), Err(Errno::BadMagic));

    // An owner slot beyond the named count is dirty: a decoder that
    // ignored it would silently drop an owner the sender believed it sent.
    let mut frame = SwitchboardCommand::SeatReport {
        report: SeatReport::new(1, &[5]).expect("a truthful report"),
    }
    .to_le_bytes();
    frame[super::REPORT_OWNERS_OFFSET + 8] = 1;
    assert_eq!(SwitchboardCommand::from_bytes(&frame), Err(Errno::BadMagic));

    // A count above what the frame can hold is refused before any slot is
    // read, so an over-long count can never walk off the payload.
    let mut frame = SwitchboardCommand::SeatReport {
        report: SeatReport::HEALTHY,
    }
    .to_le_bytes();
    frame[super::REPORT_COUNT_OFFSET] = u8::MAX;
    assert_eq!(
        SwitchboardCommand::from_bytes(&frame),
        Err(Errno::LengthOutOfRange)
    );

    // The wire admits no report the constructor would refuse: a total
    // below the named count, a zero owner, and a repeated owner all fail
    // closed on decode exactly as they do on construction.
    let sound = SwitchboardCommand::SeatReport {
        report: SeatReport::new(2, &[5, 6]).expect("a truthful report"),
    }
    .to_le_bytes();

    let mut short_total = sound;
    short_total[super::REPORT_TOTAL_OFFSET] = 1;
    assert_eq!(
        SwitchboardCommand::from_bytes(&short_total),
        Err(Errno::OutOfRange)
    );

    let mut zero_owner = sound;
    zero_owner[super::REPORT_OWNERS_OFFSET] = 0;
    assert_eq!(
        SwitchboardCommand::from_bytes(&zero_owner),
        Err(Errno::OutOfRange)
    );

    let mut repeated = sound;
    repeated[super::REPORT_OWNERS_OFFSET + 8] = 5;
    assert_eq!(
        SwitchboardCommand::from_bytes(&repeated),
        Err(Errno::OutOfRange)
    );
}

/// A frame the compositor could plausibly have produced: a cursor-sized
/// patch of a 1080p screen, blended twice over where the pointer crosses a
/// window, with one furniture strip served from the cache.
fn sound_frame() -> FrameReport {
    FrameReport {
        screen_px: 1920 * 1080,
        damaged_px: 3_200,
        blended_px: 6_400,
        opaque_px: 1_100,
        dirty_rects: 2,
        present_calls: 2,
        chrome_hits: 1,
        chrome_misses: 0,
    }
}

#[test]
fn round_trips_frame_reports_at_the_bounds() {
    let idle = FrameReport {
        screen_px: 1920 * 1080,
        damaged_px: 0,
        blended_px: 0,
        opaque_px: 0,
        dirty_rects: 0,
        present_calls: 0,
        chrome_hits: 0,
        chrome_misses: 0,
    };
    assert!(idle.is_idle());
    assert!(!sound_frame().is_idle());

    // The whole screen recomposed in one rectangle, published once: the
    // widest frame there is.
    let whole_screen = FrameReport {
        screen_px: u64::MAX,
        damaged_px: u64::MAX,
        blended_px: u64::MAX,
        opaque_px: u64::MAX,
        dirty_rects: 1,
        present_calls: 1,
        chrome_hits: u32::MAX,
        chrome_misses: u32::MAX,
    };
    for report in [idle, sound_frame(), whole_screen] {
        let command = SwitchboardCommand::FrameReport { report };
        assert_eq!(
            SwitchboardCommand::from_bytes(&command.to_le_bytes()),
            Ok(command)
        );
    }
}

#[test]
fn frame_report_admits_every_frame_the_compositor_can_compose() {
    // Damage over bare desktop resolves to the root fill: nothing is
    // blended and nothing is copied, and that is not a contradiction.
    let bare = FrameReport {
        blended_px: 0,
        opaque_px: 0,
        ..sound_frame()
    };
    // A stack of windows blends one damaged pixel many times over.
    let deep = FrameReport {
        blended_px: 4_200_000,
        ..sound_frame()
    };
    // Every damaged pixel resolved by an opaque copy, blending nothing.
    let flat = FrameReport {
        blended_px: 0,
        opaque_px: 3_200,
        ..sound_frame()
    };
    // Many rectangles published as one bounding-box present.
    let boxed = FrameReport {
        dirty_rects: 9,
        present_calls: 1,
        ..sound_frame()
    };
    // A hardware-layer present publishes one call having composed nothing.
    let accelerated = FrameReport {
        damaged_px: 0,
        blended_px: 0,
        opaque_px: 0,
        dirty_rects: 0,
        present_calls: 1,
        ..sound_frame()
    };
    for report in [bare, deep, flat, boxed, accelerated] {
        let command = SwitchboardCommand::FrameReport { report };
        assert_eq!(
            SwitchboardCommand::from_bytes(&command.to_le_bytes()),
            Ok(command)
        );
    }
}

#[test]
fn frame_command_rejects_a_dirty_tail_and_contradictory_counts() {
    let mut frame = SwitchboardCommand::FrameReport {
        report: sound_frame(),
    }
    .to_le_bytes();
    frame[super::FRAME_END_OFFSET] = 1;
    assert_eq!(SwitchboardCommand::from_bytes(&frame), Err(Errno::BadMagic));

    // Each contradiction is refused on decode, so the panel renders no
    // sender's arithmetic.
    for report in [
        // More damage than the screen holds.
        FrameReport {
            damaged_px: 1920 * 1080 + 1,
            ..sound_frame()
        },
        // Rectangles that changed no pixel.
        FrameReport {
            damaged_px: 0,
            opaque_px: 0,
            ..sound_frame()
        },
        // Damage recomposed by no rectangle.
        FrameReport {
            dirty_rects: 0,
            present_calls: 1,
            ..sound_frame()
        },
        // More pixels copied than were damaged.
        FrameReport {
            opaque_px: 3_201,
            ..sound_frame()
        },
        // More driver calls than rectangles plus the whole-screen case.
        FrameReport {
            present_calls: 4,
            ..sound_frame()
        },
    ] {
        assert_eq!(
            SwitchboardCommand::from_bytes(
                &SwitchboardCommand::FrameReport { report }.to_le_bytes()
            ),
            Err(Errno::OutOfRange)
        );
    }
}
