//! Unit tests for the device rail: that a group with no entries states why
//! it is empty, in its own rail position, and that stating it shifts no
//! entry's index.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::PressureKind;

use super::{build_rail, rail_absences};
use crate::view::reading::{Reading, Unmeasured};
use crate::view::resources::{
    DeviceGroup, DeviceId, PaneHero, ResourceDevice, ResourceReport, StorageId,
};

/// A bare rail entry in `group`, with no instrument and no pane detail: this
/// suite is about which groups the rail states, not what a pane draws.
fn entry(id: DeviceId, group: DeviceGroup, name: &str) -> ResourceDevice {
    ResourceDevice {
        id,
        group,
        name: String::from(name),
        kind: PressureKind::Disk,
        reading: Reading::measured("41%"),
        trend: Vec::new(),
        hero: PaneHero::facts(Reading::measured("41%"), "%"),
        blocks: Vec::new(),
        banner: None,
        actions: Vec::new(),
    }
}

/// A report over `devices`, with the two absence verdicts as given.
fn report(
    devices: Vec<ResourceDevice>,
    storage_absent: Option<Unmeasured>,
    interfaces_absent: Option<Unmeasured>,
) -> ResourceReport {
    ResourceReport {
        devices,
        storage_absent,
        interfaces_absent,
    }
}

/// The processor and the machine's memory, which always answer.
fn resources() -> Vec<ResourceDevice> {
    alloc::vec![
        entry(DeviceId::Cpu, DeviceGroup::Resources, "CPU"),
        entry(DeviceId::Memory, DeviceGroup::Resources, "Memory"),
    ]
}

/// One storage device on the rail.
fn disk() -> ResourceDevice {
    entry(
        DeviceId::Storage(StorageId::Device(7)),
        DeviceGroup::Storage,
        "virtio-blk · ARXFSRoot",
    )
}

/// The graphics entry, which always has a pane.
fn graphics() -> ResourceDevice {
    entry(DeviceId::Graphics, DeviceGroup::Graphics, "Compositor")
}

/// Each stated absence as `(heading, statement)`, in rail order.
fn stated(report: &ResourceReport) -> Vec<(String, String)> {
    rail_absences(report, 0)
        .iter()
        .map(|absence| {
            (
                String::from(absence.heading()),
                String::from(absence.statement()),
            )
        })
        .collect()
}

#[test]
fn a_group_with_entries_states_no_absence() {
    let report = report(
        alloc::vec![
            resources().remove(0),
            disk(),
            entry(DeviceId::Interface([0; 16]), DeviceGroup::Network, "eth0"),
            graphics(),
        ],
        None,
        None,
    );
    assert!(stated(&report).is_empty());
}

#[test]
fn an_empty_group_the_query_answered_says_there_is_none() {
    // The mount table answered and named no storage device: the machine has
    // none, which is a fact about the machine rather than about this
    // session's authority.
    let report = report(alloc::vec![resources().remove(0), graphics()], None, None);
    assert_eq!(
        stated(&report),
        alloc::vec![
            (
                String::from("STORAGE"),
                String::from("No storage device is present.")
            ),
            (
                String::from("NETWORK"),
                String::from("No managed interface is present.")
            ),
        ]
    );
}

#[test]
fn an_empty_group_the_query_was_refused_says_so_instead() {
    // A refusal and an absence are different answers, and the whole point of
    // the report carrying the verdict is that a reader can tell them apart.
    let report = report(
        alloc::vec![resources().remove(0), graphics()],
        Some(Unmeasured::NotPermitted),
        Some(Unmeasured::Unavailable),
    );
    let stated = stated(&report);
    assert_eq!(stated[0].0, "STORAGE");
    assert!(
        stated[0].1.contains("not permitted"),
        "a refused inventory must not read as a machine with no disk: {:?}",
        stated[0].1
    );
    assert_eq!(stated[1].0, "NETWORK");
    assert!(stated[1].1.contains("unavailable"), "{:?}", stated[1].1);
}

#[test]
fn an_empty_group_is_stated_in_its_own_rail_position() {
    // STORAGE sits between RESOURCES and GRAPHICS, so its absence belongs
    // there — not after everything, where it would read as a footnote.
    let mut devices = resources();
    devices.push(graphics());
    let report = report(devices, None, None);
    let absences = rail_absences(&report, 0);
    let storage = absences
        .iter()
        .find(|absence| absence.heading() == "STORAGE")
        .expect("the empty storage group is stated");
    assert_eq!(
        storage.before(),
        2,
        "before the graphics entry, after the two resources entries"
    );
}

#[test]
fn a_trailing_empty_group_is_stated_last() {
    // Nothing follows NETWORK on this rail, so its absence sits at the end.
    let mut devices = resources();
    devices.push(disk());
    let report = report(devices, None, None);
    let absences = rail_absences(&report, 0);
    let network = absences
        .iter()
        .find(|absence| absence.heading() == "NETWORK")
        .expect("the empty network group is stated");
    assert_eq!(network.before(), 3);
}

#[test]
fn stating_an_absence_shifts_no_entry_index() {
    // The selection is resolved by counting *items*, so an absence drawn
    // among them must not move one: a rail that renumbered its entries would
    // select the device below the one a reader pressed.
    let mut devices = resources();
    devices.push(graphics());
    let report = report(devices, Some(Unmeasured::NotPermitted), None);
    let rail = build_rail(&report, 0, Some(DeviceId::Graphics));
    assert_eq!(rail.len(), 3, "three entries, two stated absences");
    assert_eq!(rail.absences().len(), 2);
    assert_eq!(
        rail.selected(),
        Some(2),
        "the graphics entry is the third *item*, whatever is drawn between them"
    );
}
