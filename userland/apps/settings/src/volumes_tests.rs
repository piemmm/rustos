//! Unit tests for the Storage pane's cards.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::filesystem::{MountFlags, VolumeStats};
use tairix_abi::sysinfo::{MountAvailability, MountRecord, MountVolumeState};
use tairix_controls::{
    FieldControl, MeterValue, MetricInstrument, MetricTile, PressureKind, ProgressValue, StatusPill,
};
use tairix_geometry::{Rect, Scale};
use tairix_icon::IconKind;
use tairix_theme::SignalRole;

use crate::volumes::{Readings, VolumeReading, VOLUME_FACTS};

/// A volume of `total` blocks with `free` unallocated, of which `avail` may
/// still be handed out. A 4 KiB block, as every real format here uses.
fn usage(total: u64, free: u64, avail: u64) -> VolumeStats {
    VolumeStats {
        block_size: 4096,
        total_blocks: total,
        free_blocks: free,
        avail_blocks: avail,
        ..VolumeStats::default()
    }
}

/// One mount table entry.
fn record(
    source: &[u8],
    target: &[u8],
    fstype: &[u8],
    usage: VolumeStats,
    availability: MountAvailability,
    medium: Option<BlkDeviceClass>,
) -> MountRecord {
    MountRecord::new(
        source,
        target,
        fstype,
        MountFlags::READ_ONLY,
        MountVolumeState {
            usage,
            availability,
            medium,
        },
        [0; 16],
    )
    .expect("a well-formed record")
}

/// The system volume: a quarter full solid-state disk, healthy.
fn system() -> MountRecord {
    record(
        b"arx0p2",
        b"/System",
        b"arxfs",
        usage(1024, 768, 768),
        MountAvailability::Available,
        Some(BlkDeviceClass::SolidState),
    )
}

use crate::test_support::theme;

fn readings(records: &[MountRecord]) -> Readings {
    let volumes: Vec<VolumeReading> = records.iter().map(VolumeReading::of).collect();
    Readings::new(&volumes)
}

/// What one row of a card says, as `(label, reading)` — with the reading
/// tagged so a stated absence can never be mistaken for a measurement.
fn rows(readings: &Readings, index: usize) -> Vec<(String, String, bool)> {
    readings
        .rows(index)
        .expect("a card")
        .iter()
        .map(|row| match row.control() {
            FieldControl::Reading(text) => (String::from(row.label()), text.clone(), true),
            FieldControl::Unmeasured(text) => (String::from(row.label()), text.clone(), false),
            _ => panic!("{} is not a read-only row", row.label()),
        })
        .collect()
}

#[test]
fn a_card_is_captioned_by_its_volume_and_states_the_mount_tables_facts() {
    let readings = readings(&[system()]);
    assert_eq!(readings.len(), 1);
    assert_eq!(readings.caption(0), Some("arx0p2"));
    assert_eq!(
        rows(&readings, 0),
        [
            (String::from("Mounted at"), String::from("/System"), true),
            (String::from("Filesystem"), String::from("arxfs"), true),
            (String::from("Device"), String::from("arx0p2"), true),
            (String::from("Medium"), String::from("solid state"), true),
            (
                String::from("Availability"),
                String::from("available"),
                true
            ),
        ]
    );
}

#[test]
fn a_mount_with_no_source_is_captioned_by_where_it_is_mounted() {
    // The shared naming rule reaches the card, so a volume named one way in
    // the Switchboard's rail is named the same way here.
    let readings = readings(&[record(
        b"",
        b"/Storage/scratch",
        b"arxfs",
        usage(8, 8, 8),
        MountAvailability::Available,
        None,
    )]);
    assert_eq!(readings.caption(0), Some("/Storage/scratch"));
    // And the device row states the absence rather than drawing a blank a
    // reader would take for a reading.
    let device = rows(&readings, 0);
    assert_eq!(
        device.get(2),
        Some(&(String::from("Device"), String::from("not reported"), false))
    );
    assert_eq!(
        device.get(3),
        Some(&(String::from("Medium"), String::from("unclassified"), true))
    );
}

#[test]
fn the_capacity_card_reads_the_share_of_the_whole_medium() {
    let readings = readings(&[system()]);
    // 1024 blocks of 4 KiB, 768 unallocated: a quarter of the medium is
    // gone, the byte pair is the medium's rather than the allocatable
    // part's, and the medium the table reported picks the glyph, so a
    // spinning disk and a stick are told apart at a glance.
    let expected = MetricTile::new("Capacity", "1.0 MiB of 4.0 MiB", PressureKind::Disk)
        .with_icon(IconKind::DiskSolidState)
        .with_detail("3.0 MiB available")
        .with_instrument(MetricInstrument::Track(MeterValue::Measured(
            ProgressValue::new(250),
        )));
    assert_eq!(readings.capacity(0), Some(&expected));
}

#[test]
fn a_withheld_reserve_is_used_up_by_neither_figure() {
    // 1000 blocks, 200 unallocated, of which only 100 may be handed out.
    let readings = readings(&[record(
        b"arx0p3",
        b"/Users",
        b"arxfs",
        usage(1000, 200, 100),
        MountAvailability::Available,
        Some(BlkDeviceClass::Rotational),
    )]);
    // The reserve is unallocated, so it is not *used*; it is not offered
    // either, so it is not *available*. The bar is of the whole medium.
    let expected = MetricTile::new("Capacity", "3.1 MiB of 3.9 MiB", PressureKind::Disk)
        .with_icon(IconKind::DiskHard)
        .with_detail("400.0 KiB available")
        .with_instrument(MetricInstrument::Track(MeterValue::Measured(
            ProgressValue::new(800),
        )));
    assert_eq!(readings.capacity(0), Some(&expected));
}

#[test]
fn a_volume_that_tracks_no_capacity_gets_no_bar_and_says_so() {
    // The in-RAM layout mounts report an all-zero accounting. A full bar or
    // an invented percentage would be a reading the machine never took.
    let readings = readings(&[record(
        b"",
        b"/",
        b"tairixfs",
        VolumeStats::default(),
        MountAvailability::Available,
        None,
    )]);
    assert!(readings.capacity(0).is_none());
    let stated = rows(&readings, 0);
    assert_eq!(
        stated.last(),
        Some(&(
            String::from("Capacity"),
            String::from("this format tracks no fixed capacity"),
            false
        ))
    );
}

#[test]
fn a_card_wears_the_band_its_availability_falls_in() {
    for (availability, word, tone) in [
        (MountAvailability::Available, "Healthy", SignalRole::Success),
        (
            MountAvailability::Recovering,
            "Degraded",
            SignalRole::Warning,
        ),
        (MountAvailability::Degraded, "Degraded", SignalRole::Warning),
        (
            MountAvailability::UnavailableLost,
            "Failing",
            SignalRole::Recovery,
        ),
        (
            MountAvailability::UnavailableDirty,
            "Failing",
            SignalRole::Recovery,
        ),
        (
            MountAvailability::RecoveryConflict,
            "Failing",
            SignalRole::Recovery,
        ),
    ] {
        let readings = readings(&[record(
            b"disk",
            b"/Storage/disk",
            b"arxfs",
            usage(4, 2, 2),
            availability,
            None,
        )]);
        assert_eq!(
            readings.pill(0),
            Some(&StatusPill::new(word).with_tone(tone)),
            "{availability:?}"
        );
        // The band is a summary, so the exact state stays on its own row:
        // "recovering" and "degraded" must not collapse into one word a
        // reader cannot tell apart.
        let stated = rows(&readings, 0);
        assert_eq!(
            stated.get(4).map(|(_, text, _)| text.as_str()),
            Some(tairix_procinfo::availability_name(availability)),
            "{availability:?}"
        );
    }
}

#[test]
fn every_label_a_card_draws_is_one_a_search_can_reach() {
    // The registry's search index is this list, so a term that reaches the
    // pane must reach a row it actually shows — in both card shapes.
    let measured = readings(&[system()]);
    let unmeasured = readings(&[record(
        b"",
        b"/",
        b"tairixfs",
        VolumeStats::default(),
        MountAvailability::Available,
        None,
    )]);
    let mut drawn: Vec<String> = Vec::new();
    for shape in [&measured, &unmeasured] {
        for (label, _, _) in rows(shape, 0) {
            if !drawn.contains(&label) {
                drawn.push(label);
            }
        }
    }
    // The tile's own label is the sixth: it is a reading like any other and
    // a reader looking for "capacity" must land on the pane whichever shape
    // their volumes take.
    drawn.push(String::from("Capacity"));
    for label in &drawn {
        assert!(
            VOLUME_FACTS.contains(&label.as_str()),
            "`{label}` is drawn but no search reaches it"
        );
    }
    for label in VOLUME_FACTS {
        assert!(
            drawn.iter().any(|shown| shown == label),
            "`{label}` is searchable but no card draws it"
        );
    }
}

#[test]
fn a_column_too_short_for_every_card_still_seats_one_and_scrolls_by_whole_cards() {
    let theme = theme();
    let readings = readings(&[system(), system(), system()]);
    let tall = Rect::new(0, 0, 600, 4000);
    let short = Rect::new(0, 0, 600, 120);
    assert_eq!(readings.seated(tall, Scale::ONE, &theme), 3);
    // A card half off the bottom would draw over the window's edge, so a
    // column that seats none whole still draws the first: a blank pane
    // would be worse than a clipped card.
    assert_eq!(readings.seated(short, Scale::ONE, &theme), 1);
    // And the height every card needs together is more than a short column,
    // which is what raises the scrollbar beside it.
    assert!(readings.measured_height(Scale::ONE, &theme) > short.height);
}

#[test]
fn the_column_draws_from_the_card_it_is_scrolled_to_and_clamps_at_the_last() {
    let mut readings = readings(&[system(), system()]);
    assert_eq!(readings.first(), 0);
    readings.set_first(1);
    assert_eq!(readings.first(), 1);
    // Past the end clamps rather than drawing nothing at all.
    readings.set_first(9);
    assert_eq!(readings.first(), 1);
}

#[test]
fn a_machine_with_no_mounted_volume_draws_no_card() {
    let readings = Readings::new(&[]);
    assert!(readings.is_empty());
    assert_eq!(readings.caption(0), None);
    assert_eq!(
        readings.seated(Rect::new(0, 0, 600, 800), Scale::ONE, &theme()),
        0
    );
}
