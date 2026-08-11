//! Unit tests for the Recovery section: its fault cards, the selected
//! fault's detail pages, its impact readings, and its action rail.

use alloc::vec::Vec;

use tairix_geometry::Scale;
use tairix_input::{Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::testkit::high_contrast;
use tairix_controls::{
    damage, AuthorityState, ControlDisposition, ControlRole, ControlState, EventMark, Fact,
    MetricLayout, RecoveryState, Tab,
};

use super::{FaultImpact, FaultPage, RecoveryControl};
use crate::view::test_support::{
    activate, bounds, card_body_centre, card_slot, centre, click, fault_crash, fault_id, font,
    has_ink, key, model, recovery_item,
};
use crate::view::{
    resolve_section_frame, FocusSweep, Reading, Section, SectionFrame, SectionView, Switchboard,
    SwitchboardAction, SwitchboardModel, Unmeasured, UNMEASURED_READING,
};

/// The Recovery section's resolved regions for a default-sized window.
fn frame(sb: &Switchboard, theme: &Theme) -> SectionFrame {
    let layout = sb.compute_layout(bounds(), Scale::ONE, theme);
    resolve_section_frame(layout.content, sb.recovery.anatomy(), Scale::ONE, theme)
}

/// A model carrying exactly the fixture faults named by `indices`, each with
/// its own stable identity so a test can move one and watch the selection
/// follow it.
fn faults(indices: &[usize]) -> SwitchboardModel {
    let mut m = SwitchboardModel::new("Switchboard");
    for index in indices {
        m.recovery.push(recovery_item(*index, RecoveryState::Hung));
    }
    m
}

/// A Switchboard built on one fault, so a command always names index 0.
fn one_fault() -> Switchboard {
    Switchboard::new(&faults(&[0]))
}

#[test]
fn force_action_carries_confirmation_posture() {
    let mut m = SwitchboardModel::new("Switchboard");
    m.recovery.push(recovery_item(0, RecoveryState::Hung));
    let sb = Switchboard::new(&m);
    // The commands moved out of the row and into the anchored rail, so the
    // posture is asserted on the rail's Force slot rather than the row's.
    assert_eq!(
        sb.recovery.rail.items()[1].state().disposition(),
        ControlDisposition::NeedsConfirmation
    );
    assert_eq!(sb.recovery.rail.items()[1].role(), ControlRole::Destructive);
}

#[test]
fn recovery_row_force_activates_by_pointer() {
    let theme = Theme::dark();
    let mut sb = Switchboard::new(&model());
    sb.select_section(Section::Recovery);
    let b = bounds();
    // Force is aimed at where the rail paints it, not at a rectangle split
    // out of the row: the row no longer carries the commands.
    let content = sb
        .recovery
        .rail_content(&frame(&sb, &theme), Scale::ONE, &theme)
        .expect("the default window seats the recovery rail");
    let rect = sb
        .recovery
        .rail
        .item_rect(content, 1, Scale::ONE, &theme)
        .expect("the rail seats both of its commands");
    let (x, y) = centre(rect);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
    assert!(actions.contains(&SwitchboardAction::Recovery {
        index: 0,
        control: RecoveryControl::Force
    }));
}

/// A point in the body of the fault card at `index`.
///
/// A fault card carries no footer buttons of its own — its commands live in
/// the anchored rail — so every point on it is body; the shared helper still
/// checks that against the card's own (empty) footer layout rather than
/// assuming it.
fn recovery_body_centre(sb: &Switchboard, theme: &Theme, index: usize) -> (i32, i32) {
    let item = card_slot(sb, bounds(), theme, index);
    let footer = sb.recovery.cards[index].footer_rects(item, Scale::ONE, theme);
    card_body_centre(item, &footer)
}

#[test]
fn a_press_on_a_fault_card_body_selects_that_fault() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&faults(&[0, 1, 2]));
    sb.select_section(Section::Recovery);
    assert_eq!(
        sb.recovery.selected,
        Some(fault_id(0)),
        "the first fault is the one open to begin with"
    );

    let (x, y) = recovery_body_centre(&sb, &theme, 1);
    let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);

    // Pressing a card opens its detail: the fault it is about becomes the
    // selected one, and the pane describes that fault.
    assert_eq!(sb.recovery.selected, Some(fault_id(1)));
    let fault = sb.recovery.selected_item().expect("a fault is selected");
    assert_eq!(fault.proc_id, fault_id(1));
    assert_eq!(fault.name, recovery_item(1, RecoveryState::Hung).name);
    assert!(
        actions.is_empty(),
        "a body press opens the detail; it is not a command: {actions:?}"
    );
}

#[test]
fn a_press_on_the_rail_resolves_the_command_for_the_pressed_fault() {
    let theme = Theme::dark();
    let b = bounds();
    let mut sb = Switchboard::new(&faults(&[0, 1, 2]));
    sb.select_section(Section::Recovery);
    let (x, y) = recovery_body_centre(&sb, &theme, 1);
    assert!(click(&mut sb, b, Scale::ONE, &theme, x, y).is_empty());

    // The commands live in the rail rather than on the card, so the press
    // that opened the fault and the command that acts on it must agree about
    // which fault is meant.
    let content = sb
        .recovery
        .rail_content(&frame(&sb, &theme), Scale::ONE, &theme)
        .expect("the default window seats the recovery rail");
    let rect = sb
        .recovery
        .rail
        .item_rect(content, 1, Scale::ONE, &theme)
        .expect("the rail seats both of its commands");
    let (rx, ry) = centre(rect);
    assert!(
        click(&mut sb, b, Scale::ONE, &theme, rx, ry).contains(&SwitchboardAction::Recovery {
            index: 1,
            control: RecoveryControl::Force
        })
    );
}

#[test]
fn a_press_on_a_disabled_or_denied_fault_card_selects_nothing() {
    let theme = Theme::dark();
    let b = bounds();
    for state in [
        ControlState::disabled(),
        ControlState::idle().with_authority(AuthorityState::Denied),
    ] {
        let mut sb = Switchboard::new(&faults(&[0, 1, 2]));
        sb.select_section(Section::Recovery);
        sb.recovery.cards[1].set_state(state);
        let (x, y) = recovery_body_centre(&sb, &theme, 1);
        let actions = click(&mut sb, b, Scale::ONE, &theme, x, y);
        assert_eq!(
            sb.recovery.selected,
            Some(fault_id(0)),
            "a card that is not actionable must not become the open fault"
        );
        assert!(actions.is_empty(), "{actions:?}");
    }
}

#[test]
fn keyboard_reaches_the_recovery_force_action() {
    // One fault, because walking the cursor down a longer list would select
    // the fault it lands on: the commands always name the selected fault,
    // and this test is about reaching them, not about which one they act on.
    let mut m = SwitchboardModel::new("Switchboard");
    m.recovery.push(recovery_item(0, RecoveryState::Hung));
    let mut sb = Switchboard::new(&m);
    sb.select_section(Section::Recovery);
    // The cursor now walks every fault card, then the detail pane's page
    // strip, then the rail, so Enter on a card only selects that fault.
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Enter)), None);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(
        key(&mut sb, Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Restart
        })
    );
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(
        key(&mut sb, Key::Named(NamedKey::Enter)),
        Some(SwitchboardAction::Recovery {
            index: 0,
            control: RecoveryControl::Force
        }),
        "Force must be keyboard-reachable"
    );
}

// --- The section frame -------------------------------------------------

#[test]
fn the_section_asks_for_a_detail_pane_an_impact_column_a_rail_and_a_footer() {
    let sb = one_fault();
    let anatomy = sb.recovery.anatomy();
    assert!(anatomy.detail_width > 0, "a fault's detail needs a pane");
    assert!(
        anatomy.impact_width > 0,
        "the faulting task's cost needs its own column"
    );
    assert!(anatomy.rail_width > 0, "a fault's commands need a rail");
    assert!(
        anatomy.footer_height > 0,
        "the resolved count needs a footer band"
    );
}

// --- The master list ---------------------------------------------------

#[test]
fn each_fault_card_says_what_happened_and_how_long_ago() {
    let item = recovery_item(0, RecoveryState::Hung);
    let body = super::card_body(&item);
    assert!(body.contains("not responding"), "{body}");
    assert!(body.contains("4m"), "{body}");
}

#[test]
fn a_card_whose_age_is_unmeasured_says_why_rather_than_guessing() {
    let mut item = recovery_item(0, RecoveryState::Hung);
    item.since = Reading::Absent(Unmeasured::NotPermitted);
    let body = super::card_body(&item);
    assert!(body.contains(UNMEASURED_READING), "{body}");
    assert!(body.contains("not permitted"), "{body}");
    assert!(
        !body.contains("4m"),
        "an absent age must never read as a measurement"
    );
}

#[test]
fn one_card_per_fault_in_model_order() {
    let sb = Switchboard::new(&model());
    assert_eq!(sb.recovery.cards.len(), sb.recovery.items.len());
    assert!(!sb.recovery.items.is_empty());
}

// --- Selection ---------------------------------------------------------

#[test]
fn selection_follows_the_fault_when_the_list_reorders() {
    let mut sb = Switchboard::new(&faults(&[0, 1]));
    assert_eq!(sb.recovery.selected, Some(fault_id(0)));
    sb.set_model(&faults(&[1, 0]));
    assert_eq!(
        sb.recovery.selected,
        Some(fault_id(0)),
        "a refresh that reorders the list must not re-point the selection"
    );
    assert_eq!(sb.recovery.selected_index(), Some(1));
}

#[test]
fn selection_drops_only_when_the_fault_clears() {
    let mut sb = Switchboard::new(&faults(&[0, 1]));
    sb.set_model(&faults(&[1]));
    assert_eq!(
        sb.recovery.selected,
        Some(fault_id(1)),
        "a cleared fault hands the selection to what is left"
    );
    sb.set_model(&faults(&[]));
    assert_eq!(
        sb.recovery.selected, None,
        "with nothing faulted there is nothing to select"
    );
}

// --- The detail pane ---------------------------------------------------

#[test]
fn the_detail_pane_names_the_fault_and_its_impact() {
    let item = recovery_item(0, RecoveryState::Hung);
    let identity = super::identity_text(&item);
    assert!(identity.contains(&item.name), "{identity}");
    assert!(identity.contains("400"), "{identity}");
    assert_eq!(item.impact, FaultImpact::of(RecoveryState::Hung));
}

#[test]
fn the_detail_facts_state_status_age_and_recommendation() {
    let item = recovery_item(0, RecoveryState::Hung);
    let facts = super::detail_facts(&item);
    let labels: Vec<&str> = facts.facts().iter().map(Fact::label).collect();
    assert_eq!(labels, alloc::vec!["Status", "Faulted", "Recommendation"]);
    assert_eq!(facts.facts()[1].value(), "4m");
}

#[test]
fn the_page_strip_offers_the_three_pages_in_order() {
    let sb = one_fault();
    let titles: Vec<&str> = sb.recovery.pages.tabs().iter().map(Tab::label).collect();
    assert_eq!(titles, alloc::vec!["Timeline", "Crash Snapshot", "Logs"]);
    assert_eq!(sb.recovery.page, FaultPage::Timeline);
}

#[test]
fn the_timeline_page_marks_the_fault_itself() {
    let item = recovery_item(0, RecoveryState::Hung);
    let timeline = super::fault_timeline(&item);
    assert_eq!(timeline.events().len(), 1);
    assert_eq!(timeline.events()[0].mark(), EventMark::Notable);
    assert!(timeline.events()[0].text().contains("Stopped answering"));
}

#[test]
fn the_crash_snapshot_page_shows_the_recorded_registers_and_frames() {
    let mut item = recovery_item(0, RecoveryState::Hung);
    item.crash = Some(fault_crash());
    let facts = super::crash_facts(&item).expect("a fault with a crash record has a snapshot");
    let labels: Vec<&str> = facts.facts().iter().map(Fact::label).collect();
    for expected in ["Cause", "Address", "Access", "Owner", "pc", "sp", "fp"] {
        assert!(
            labels.contains(&expected),
            "{expected} missing from {labels:?}"
        );
    }
    assert!(labels.contains(&"x0"), "the named registers must be shown");
    assert!(
        labels.iter().any(|label| label.starts_with("frame ")),
        "the backtrace frames must be shown"
    );
}

#[test]
fn a_fault_with_no_crash_record_says_so_plainly() {
    let item = recovery_item(0, RecoveryState::Hung);
    assert!(item.crash.is_none());
    assert!(super::crash_facts(&item).is_none());
    assert!(
        !super::NO_CRASH_RECORD.contains(UNMEASURED_READING),
        "a fault that never raised a user fault is a fact, not a missing reading"
    );
    assert!(super::NO_CRASH_RECORD.contains("No crash record"));
}

#[test]
fn the_logs_page_states_there_is_no_interface() {
    let line = super::logs_absence();
    assert!(line.contains(UNMEASURED_READING), "{line}");
    assert!(line.contains("no interface"), "{line}");
}

// --- The impact column -------------------------------------------------

#[test]
fn the_impact_stack_carries_four_unplated_readings() {
    let item = recovery_item(0, RecoveryState::Hung);
    let tiles = super::impact_tiles(&item);
    assert_eq!(tiles.len(), 4);
    for tile in &tiles {
        assert_eq!(tile.layout(), MetricLayout::Stacked);
    }
}

#[test]
fn the_impact_stacks_network_reading_is_unmeasured() {
    let item = recovery_item(0, RecoveryState::Hung);
    assert_eq!(item.network, Reading::Absent(Unmeasured::NoInterface));
    let text = super::reading_text(&item.network);
    assert!(text.contains(UNMEASURED_READING), "{text}");
    assert!(text.contains("no interface"), "{text}");
}

// --- The rail ----------------------------------------------------------

#[test]
fn the_rail_carries_restart_and_force_with_their_verdicts() {
    let sb = one_fault();
    assert_eq!(sb.recovery.rail.len(), 2);
    assert_eq!(
        sb.recovery.rail.items()[0].state().disposition(),
        ControlDisposition::Interactive
    );
    assert_eq!(
        sb.recovery.rail.items()[1].state().disposition(),
        ControlDisposition::NeedsConfirmation
    );
}

#[test]
fn a_force_this_caller_may_not_take_wears_the_authority_mark() {
    let mut m = SwitchboardModel::new("Switchboard");
    let mut item = recovery_item(0, RecoveryState::Hung);
    item.can_force = false;
    m.recovery.push(item);
    let sb = Switchboard::new(&m);
    assert_eq!(
        sb.recovery.rail.items()[1].state().disposition(),
        ControlDisposition::DeniedByAuthority
    );
}

#[test]
fn enter_on_a_refused_rail_stop_dispatches_nothing() {
    // Both commit keys are checked, and both refused commands: the keyboard
    // must reach the same verdict the pointer does, and a route that
    // consulted only one of them would still be open on the other.
    for commit in [Key::Named(NamedKey::Enter), Key::Char(' ')] {
        for slot in 0..2 {
            let mut m = SwitchboardModel::new("Switchboard");
            let mut item = recovery_item(0, RecoveryState::Hung);
            item.can_restart = false;
            item.can_force = false;
            m.recovery.push(item);
            let mut sb = Switchboard::new(&m);
            sb.select_section(Section::Recovery);
            sb.recovery.set_content_focus(2 + slot);
            assert!(
                activate(&mut sb, commit).is_none(),
                "a refused command must refuse the keyboard exactly as it refuses \
                 the pointer (slot {slot})"
            );
        }
    }
}

// --- The footer --------------------------------------------------------

#[test]
fn the_footer_counts_the_resolved_faults() {
    assert_eq!(super::resolved_text(0), "0 faults resolved");
    assert_eq!(super::resolved_text(1), "1 fault resolved");
    assert_eq!(super::resolved_text(4), "4 faults resolved");
}

#[test]
fn the_footer_count_comes_from_the_model() {
    let mut m = faults(&[0]);
    m.recovery_resolved = 3;
    let sb = Switchboard::new(&m);
    assert_eq!(sb.recovery.resolved, 3);
}

// --- The keyboard ------------------------------------------------------

#[test]
fn the_cursor_reaches_every_fault_then_the_pages_then_every_command() {
    let sb = one_fault();
    assert_eq!(
        sb.recovery.focus_span(),
        sb.recovery.items.len() + 1 + sb.recovery.rail.len()
    );
}

#[test]
fn the_cursor_walks_the_page_strip_sideways() {
    let mut sb = one_fault();
    sb.select_section(Section::Recovery);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Down)), None);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Right)), None);
    assert_eq!(sb.recovery.page, FaultPage::CrashSnapshot);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Right)), None);
    assert_eq!(sb.recovery.page, FaultPage::Logs);
    assert_eq!(key(&mut sb, Key::Named(NamedKey::Left)), None);
    assert_eq!(sb.recovery.page, FaultPage::CrashSnapshot);
}

#[test]
fn nothing_faulted_leaves_no_cursor_stops() {
    let sb = Switchboard::new(&SwitchboardModel::new("Switchboard"));
    assert_eq!(sb.recovery.focus_span(), 0);
    assert!(sb.recovery.rail.is_empty());
    assert_eq!(sb.recovery.selected, None);
}

// --- Painting ----------------------------------------------------------

#[test]
fn both_themes_and_the_heavier_contrast_path_render() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut sb = Switchboard::new(&model());
        sb.select_section(Section::Recovery);
        let b = bounds();
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        assert!(
            has_ink(&surface, b),
            "the Recovery screen must paint under every theme"
        );
    }
}

#[test]
fn every_detail_page_paints() {
    let theme = Theme::dark();
    for page in FaultPage::ALL {
        let mut m = faults(&[0]);
        if page == FaultPage::CrashSnapshot {
            m.recovery[0].crash = Some(fault_crash());
        }
        let mut sb = Switchboard::new(&m);
        sb.select_section(Section::Recovery);
        sb.recovery
            .select_page(page, &mut FocusSweep::adopting(&mut damage::sink()));
        let b = bounds();
        let mut surface = Surface::new(b.width, b.height).expect("surface");
        sb.render(&mut surface, b, Scale::ONE, &theme, font());
        assert!(has_ink(&surface, b), "the {} page must paint", page.title());
    }
}
