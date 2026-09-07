//! Unit tests for the device rail: that a group with no entries states why
//! it is empty, in its own rail position, that stating it shifts no entry's
//! index, and that pressing an entry selects it and repaints the pane it
//! now draws.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_theme::Theme;

use tairix_controls::{damage, PressureKind};

use super::{build_rail, rail_absences};
use crate::view::reading::{Reading, Unmeasured};
use crate::view::resources::{
    DeviceGroup, DeviceId, PaneHero, ResourceDevice, ResourceReport, StorageId,
};
use crate::view::test_support::{
    bounds, centre, click, font, model, moved, refresh, shot, unreported_change, PRESS, RELEASE,
};
use crate::view::{Section, Switchboard};

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

/// The screen on the Resources section, showing the shared fixture report.
fn resources_screen() -> Switchboard {
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Resources);
    let _ = shot(&mut sb);
    sb
}

/// The window point that hits rail entry `index`, read from the strip's own
/// layout so a test aims where the screen really seats the entry.
fn rail_point(sb: &Switchboard, index: usize) -> (i32, i32) {
    let theme = Theme::dark();
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    let sidebar = ctx
        .frame
        .sidebar
        .expect("the fixture window seats a sidebar");
    let area = sb
        .resources
        .rail
        .tab_area(index, sidebar, Scale::ONE, &theme)
        .expect("the entry is seated");
    centre(area)
}

/// The sidebar's own rectangle in the fixture window.
fn sidebar_rect(sb: &Switchboard) -> Rect {
    let theme = Theme::dark();
    let b = bounds();
    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    ctx.frame
        .sidebar
        .expect("the fixture window seats a sidebar")
}

/// Feed one event to the screen, accumulating what it reports into `reported`.
fn feed(sb: &mut Switchboard, event: &InputEvent, reported: &mut Region) {
    sb.on_pointer(
        event,
        bounds(),
        Scale::ONE,
        &Theme::dark(),
        font(),
        reported,
    );
}

/// The fixture's storage device, the entry these tests select onto.
const STORAGE: DeviceId = DeviceId::Storage(StorageId::Device(0x5953_2001));

#[test]
fn selecting_a_device_reports_the_pane_it_now_draws() {
    // The pane is the whole point of pressing a rail entry, so a press that
    // switches device owes every pixel the new pane draws. Reporting only
    // the strip's own lift leaves the reader looking at the previous
    // device's readings until something else repaints the window whole.
    let mut sb = resources_screen();
    let before = shot(&mut sb);
    let (x, y) = rail_point(&sb, 2);
    let mut reported = damage::sink();
    for event in [moved(x, y), PRESS, RELEASE] {
        feed(&mut sb, &event, &mut reported);
    }
    assert_eq!(sb.resources.selected, Some(STORAGE), "the press selected");
    let after = shot(&mut sb);
    assert_eq!(
        unreported_change(&before, &after, bounds(), &reported),
        None,
        "a press that switches pane must report every pixel it moved"
    );
}

#[test]
fn a_sample_landing_under_a_resting_pointer_does_not_swallow_the_click() {
    // A reader moves onto an entry, rests, and clicks. A sample lands in
    // between — they arrive about once a second — and the press must still
    // select what the pointer is over.
    let mut sb = resources_screen();
    let (x, y) = rail_point(&sb, 2);
    let mut reported = damage::sink();
    feed(&mut sb, &moved(x, y), &mut reported);
    let _ = refresh(&mut sb, &model());
    feed(&mut sb, &PRESS, &mut reported);
    feed(&mut sb, &RELEASE, &mut reported);
    assert_eq!(
        sb.resources.selected,
        Some(STORAGE),
        "a sample between the pointer's motion and its press swallowed the click"
    );
}

#[test]
fn a_sample_keeps_the_lift_under_a_resting_pointer() {
    // The lift states where the pointer is, and the pointer has not moved,
    // so re-deriving the strip from an identical sample must draw the same
    // sidebar rather than blinking the highlight off.
    let mut sb = resources_screen();
    let (x, y) = rail_point(&sb, 2);
    feed(&mut sb, &moved(x, y), &mut damage::sink());
    let before = shot(&mut sb);
    let _ = refresh(&mut sb, &model());
    let after = shot(&mut sb);
    assert_eq!(
        unreported_change(&before, &after, sidebar_rect(&sb), &damage::sink()),
        None,
        "an identical sample must leave the rail's own pixels alone"
    );
}

#[test]
fn the_keyboard_reports_the_pane_it_selects_onto() {
    // The cursor on a rail entry *is* the selection, so Down owes the new
    // pane exactly as a press does.
    let mut sb = resources_screen();
    let before = shot(&mut sb);
    let mut reported = damage::sink();
    sb.on_key(
        Key::Named(NamedKey::Down),
        bounds(),
        Scale::ONE,
        &Theme::dark(),
        font(),
        &mut reported,
    );
    assert_ne!(
        sb.resources.selected,
        Some(DeviceId::Cpu),
        "Down moved off the first entry"
    );
    let after = shot(&mut sb);
    assert_eq!(
        unreported_change(&before, &after, bounds(), &reported),
        None,
        "a cursor move that switches pane must report every pixel it moved"
    );
}

#[test]
fn the_pressure_banner_draws_nothing_outside_the_pane() {
    // The banner's summary and detail are model text of any length. Drawn
    // past the pane they land in the gap beside it and over the action
    // column, where no repaint of the pane can clean them up. The entry with
    // no banner is the control: nothing draws in the gap for either, so the
    // two must leave it identical.
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = resources_screen();
    let bare = shot(&mut sb);

    let (x, y) = rail_point(&sb, 1);
    let _ = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(
        sb.resources
            .device()
            .and_then(|device| device.banner.as_ref())
            .is_some(),
        "the memory entry wears a pressure banner"
    );
    let bannered = shot(&mut sb);

    let layout = sb.compute_layout(b, Scale::ONE, &theme);
    let ctx = sb.section_ctx(&layout, b, Scale::ONE, &theme, font());
    let pane = ctx.frame.primary;
    let rail = ctx.frame.rail.expect("the fixture window seats a rail");
    let gap = Rect::new(
        pane.right(),
        pane.top(),
        u32::try_from(rail.left().saturating_sub(pane.right())).unwrap_or(0),
        pane.height,
    );
    assert!(gap.width > 0, "the fixture seats a gap to overrun into");
    assert_eq!(
        unreported_change(&bare, &bannered, gap, &damage::sink()),
        None,
        "the banner drew outside its own pane"
    );
}
