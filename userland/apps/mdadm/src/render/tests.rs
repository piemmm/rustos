//! Renderer tests: an optimal array, a degraded array with an absent slot, a
//! rebuild in progress, an empty machine, and a blank-device listing.

extern crate std;

use alloc::vec::Vec;

use tairix_abi::raid::{ArrayHealth, RaidLevel};
use tairix_abi::raid_admin::{
    RaidArrayRecord, RaidMemberDisposition, RaidMemberRecord, RAID_ARRAY_FLAG_RESYNCING,
    RAID_ARRAY_FLAG_SCRUBBING, RAID_SLOT_NONE,
};

use super::{format_identity, render_array_detail, render_detail, render_examine, render_version};

/// An identity whose bytes render to a recognisable hex string.
const ID_A: [u8; 16] = [
    0x3f, 0x2a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const ID_B: [u8; 16] = [
    0xb1, 0x77, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// The builder takes one argument per rendered array field, which is what
// makes each case readable at its call site.
#[allow(clippy::too_many_arguments)]
fn array(
    id: [u8; 16],
    level: RaidLevel,
    health: ArrayHealth,
    flags: u8,
    member_count: u16,
    active_members: u16,
    chunk_blocks: u32,
    resync_cursor: u64,
    scrub_cursor: u64,
) -> RaidArrayRecord {
    RaidArrayRecord::new(
        id,
        level,
        health,
        flags,
        member_count,
        active_members,
        512,
        chunk_blocks,
        1_048_576,
        7,
        12,
        scrub_cursor,
        resync_cursor,
        5,
    )
}

#[test]
fn identity_is_thirty_two_lowercase_hex_digits() {
    let text = format_identity(&ID_A);
    assert_eq!(text.len(), 32);
    assert_eq!(&text, "3f2a0000000000000000000000000000");
}

#[test]
fn version_names_the_tool() {
    assert!(render_version().starts_with("mdadm"));
    assert!(render_version().contains("0.1.0"));
}

#[test]
fn optimal_array_detail_lists_the_shape() {
    let record = array(
        ID_A,
        RaidLevel::Parity,
        ArrayHealth::Optimal,
        0,
        3,
        3,
        128,
        1_048_576,
        1_048_576,
    );
    let lines = render_array_detail(&record);
    assert_eq!(lines[0], "3f2a0000000000000000000000000000:");
    let body: Vec<&str> = lines.iter().map(|l| l.trim_start()).collect();
    assert!(body.contains(&"Raid Level : raid5"), "{lines:?}");
    assert!(body.contains(&"State : optimal"), "{lines:?}");
    assert!(body.contains(&"Raid Devices : 3"), "{lines:?}");
    assert!(body.contains(&"Active Devices : 3"), "{lines:?}");
    assert!(body.contains(&"Chunk Size : 128 blocks"), "{lines:?}");
    assert!(
        body.contains(&"Array Size : 1048576 blocks x 512 B"),
        "{lines:?}"
    );
    assert!(body.contains(&"Published As : node:12"), "{lines:?}");
    // An optimal array shows no rebuild or scrub status line.
    assert!(
        !lines.iter().any(|l| l.contains("Rebuild Status")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("Scrub Status")),
        "{lines:?}"
    );
}

#[test]
fn a_mirror_omits_the_chunk_line() {
    let record = array(
        ID_A,
        RaidLevel::Mirror,
        ArrayHealth::Optimal,
        0,
        2,
        2,
        0,
        1_048_576,
        1_048_576,
    );
    let lines = render_array_detail(&record);
    assert!(!lines.iter().any(|l| l.contains("Chunk Size")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.trim_start() == "Raid Level : raid1"),
        "{lines:?}"
    );
}

#[test]
fn degraded_array_with_an_absent_slot_is_reported() {
    // Three defined slots, only two active: one slot is absent.
    let record = array(
        ID_A,
        RaidLevel::Parity,
        ArrayHealth::Degraded,
        0,
        3,
        2,
        128,
        1_048_576,
        1_048_576,
    );
    let lines = render_array_detail(&record);
    let body: Vec<&str> = lines.iter().map(|l| l.trim_start()).collect();
    assert!(body.contains(&"State : degraded"), "{lines:?}");
    assert!(body.contains(&"Raid Devices : 3"), "{lines:?}");
    assert!(body.contains(&"Active Devices : 2"), "{lines:?}");
}

#[test]
fn a_rebuild_in_progress_shows_its_position() {
    let record = array(
        ID_A,
        RaidLevel::Parity,
        ArrayHealth::Recovering,
        RAID_ARRAY_FLAG_RESYNCING,
        3,
        2,
        128,
        262_144,
        1_048_576,
    );
    let lines = render_array_detail(&record);
    let body: Vec<&str> = lines.iter().map(|l| l.trim_start()).collect();
    assert!(body.contains(&"State : recovering"), "{lines:?}");
    assert!(
        body.contains(&"Rebuild Status : 262144 / 1048576 blocks"),
        "{lines:?}"
    );
}

#[test]
fn a_scrub_in_progress_shows_its_position() {
    let record = array(
        ID_A,
        RaidLevel::Parity,
        ArrayHealth::Optimal,
        RAID_ARRAY_FLAG_SCRUBBING,
        3,
        3,
        128,
        1_048_576,
        524_288,
    );
    let lines = render_array_detail(&record);
    assert!(
        lines
            .iter()
            .any(|l| l.trim_start() == "Scrub Status : 524288 / 1048576 blocks"),
        "{lines:?}"
    );
}

#[test]
fn detail_of_several_arrays_separates_them_with_a_blank_line() {
    let a = array(
        ID_A,
        RaidLevel::Parity,
        ArrayHealth::Optimal,
        0,
        3,
        3,
        128,
        1_048_576,
        1_048_576,
    );
    let b = array(
        ID_B,
        RaidLevel::Mirror,
        ArrayHealth::Optimal,
        0,
        2,
        2,
        0,
        1_048_576,
        1_048_576,
    );
    let lines = render_detail(&[a, b]);
    assert_eq!(lines[0], "3f2a0000000000000000000000000000:");
    // Exactly one blank line separates the two blocks.
    let blanks = lines.iter().filter(|l| l.is_empty()).count();
    assert_eq!(blanks, 1, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("b177") && l.ends_with(':')),
        "{lines:?}"
    );
}

#[test]
fn an_empty_machine_renders_nothing() {
    assert!(render_detail(&[]).is_empty());
    assert!(render_examine(&[]).is_empty());
}

#[test]
fn examine_lists_members_and_blank_devices() {
    let member = RaidMemberRecord::new(
        ID_A,
        RaidMemberDisposition::InSync,
        0,
        20,
        100,
        1_048_576,
        512,
        5,
    );
    let blank = RaidMemberRecord::new(
        [0u8; 16],
        RaidMemberDisposition::Candidate,
        RAID_SLOT_NONE,
        21,
        101,
        2_097_152,
        512,
        0,
    );
    let lines = render_examine(&[member, blank]);
    // A header row, then one row per device.
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].contains("Device"), "{lines:?}");
    assert!(lines[0].contains("Array"), "{lines:?}");
    assert!(lines[0].contains("State"), "{lines:?}");
    // The member: its node, array identity, slot, and state.
    assert!(lines[1].contains("node:20"), "{lines:?}");
    assert!(
        lines[1].contains("3f2a0000000000000000000000000000"),
        "{lines:?}"
    );
    assert!(lines[1].contains("in-sync"), "{lines:?}");
    // The blank candidate: node, a `-` array and `-` slot, and `candidate`.
    assert!(lines[2].contains("node:21"), "{lines:?}");
    assert!(lines[2].contains("candidate"), "{lines:?}");
    assert!(lines[2].contains('-'), "{lines:?}");
}

#[test]
fn a_faulted_member_reports_its_state() {
    let faulted = RaidMemberRecord::new(
        ID_A,
        RaidMemberDisposition::Faulted,
        1,
        22,
        102,
        1_048_576,
        512,
        5,
    );
    let lines = render_examine(&[faulted]);
    assert!(lines[1].contains("faulted"), "{lines:?}");
    assert!(lines[1].contains("node:22"), "{lines:?}");
}
