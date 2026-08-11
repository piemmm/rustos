//! Unit tests for the refresh rules.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{
    ActionRail, Button, ButtonContent, Card, ControlRole, ControlState, ListRow, PointerState,
};

use super::{carry_hover, carry_hover_one, resettle_card, resettle_cards, restate_rail};

/// A row wearing `pointer`, the shape a refresh finds a slot in.
fn row(label: &str, pointer: PointerState) -> ListRow {
    let mut row = ListRow::new(String::from(label));
    row.set_state(ControlState::idle().with_pointer(pointer));
    row
}

fn command(label: &str) -> Button {
    Button::new(
        ButtonContent::Label(String::from(label)),
        ControlRole::Neutral,
    )
}

#[test]
fn hover_lands_on_the_row_that_took_the_slot() {
    let was = row("was", PointerState::Hover);
    let mut now = row("now", PointerState::None);
    carry_hover_one(&was, &mut now);
    assert_eq!(now.state().pointer, PointerState::Hover);
}

#[test]
fn a_press_is_never_carried() {
    for latch in [
        PointerState::Pressed,
        PointerState::DragSource,
        PointerState::DragTarget,
    ] {
        let was = row("was", latch);
        let mut now = row("now", PointerState::None);
        carry_hover_one(&was, &mut now);
        assert_eq!(now.state().pointer, PointerState::None, "{latch:?}");
    }
}

#[test]
fn carrying_leaves_the_rest_of_the_state_alone() {
    let was = row("was", PointerState::Hover);
    let mut now = row("now", PointerState::None);
    now.set_state(ControlState::disabled());
    carry_hover_one(&was, &mut now);
    assert_eq!(
        now.state(),
        ControlState::disabled().with_pointer(PointerState::Hover)
    );
}

#[test]
fn a_slot_the_refresh_dropped_carries_nothing() {
    let was = alloc::vec![
        row("first", PointerState::None),
        row("second", PointerState::Hover),
    ];
    let mut now = alloc::vec![row("first", PointerState::None)];
    carry_hover(&was, &mut now);
    assert!(now
        .iter()
        .all(|row| row.state().pointer == PointerState::None));
}

#[test]
fn each_slot_carries_its_own_hover() {
    let was = alloc::vec![
        row("first", PointerState::None),
        row("second", PointerState::Hover),
    ];
    let mut now = alloc::vec![
        row("first", PointerState::None),
        row("second", PointerState::None),
    ];
    carry_hover(&was, &mut now);
    assert_eq!(
        now.iter()
            .map(|row| row.state().pointer)
            .collect::<Vec<_>>(),
        alloc::vec![PointerState::None, PointerState::Hover]
    );
}

/// A card with one footer command, the shape the card sections derive.
fn card(title: &str) -> Card {
    Card::new(String::from(title)).with_footer(alloc::vec![command("Pause")])
}

/// The card `title` with its footer command wearing `pointer` — a card draws
/// no highlight of its own, so its footer is where the pointer shows.
fn pointed_card(title: &str, pointer: PointerState) -> Card {
    let mut card = card(title);
    if let Some(item) = card.footer_mut().first_mut() {
        item.set_state(ControlState::idle().with_pointer(pointer));
    }
    card
}

/// The pointer state a card's footer command is wearing.
fn footer_pointer(card: &Card) -> Option<PointerState> {
    card.footer().first().map(|item| item.state().pointer)
}

#[test]
fn an_unchanged_card_keeps_the_hover_on_its_footer_command() {
    let mut fresh = card("same");
    resettle_card(pointed_card("same", PointerState::Hover), &mut fresh);
    assert_eq!(footer_pointer(&fresh), Some(PointerState::Hover));
}

#[test]
fn a_changed_card_rests_its_footer() {
    let mut fresh = card("now");
    resettle_card(pointed_card("was", PointerState::Hover), &mut fresh);
    assert_eq!(
        footer_pointer(&fresh),
        Some(PointerState::None),
        "a fresh card's own hover record cannot be restated, so its footer rests"
    );
    assert_eq!(fresh, card("now"));
}

#[test]
fn a_card_holding_a_press_is_never_kept() {
    let mut fresh = card("same");
    resettle_card(pointed_card("same", PointerState::Pressed), &mut fresh);
    assert_eq!(
        footer_pointer(&fresh),
        Some(PointerState::None),
        "a footer press names the slot it began on, so it must not complete after a refresh"
    );
}

#[test]
fn cards_are_resettled_slot_for_slot() {
    let live = alloc::vec![
        pointed_card("first", PointerState::Hover),
        pointed_card("second", PointerState::Hover),
    ];
    let mut fresh = alloc::vec![card("first"), card("changed")];

    resettle_cards(live, &mut fresh);

    assert_eq!(
        fresh.iter().map(footer_pointer).collect::<Vec<_>>(),
        alloc::vec![Some(PointerState::Hover), Some(PointerState::None)]
    );
}

#[test]
fn the_same_commands_are_restated_in_place() {
    let mut rail = ActionRail::new(alloc::vec![command("Pause"), command("Cancel")]);
    if let Some(item) = rail.items_mut().first_mut() {
        item.set_state(ControlState::idle().with_pointer(PointerState::Pressed));
    }
    restate_rail(&mut rail, alloc::vec![command("Pause"), command("Cancel")]);
    assert_eq!(
        rail.items().first().map(|item| item.state().pointer),
        Some(PointerState::Pressed)
    );
}

#[test]
fn restating_takes_the_refreshed_states() {
    let mut rail = ActionRail::new(alloc::vec![command("Pause")]);
    let mut fresh = command("Pause");
    fresh.set_state(ControlState::disabled());
    restate_rail(&mut rail, alloc::vec![fresh]);
    assert_eq!(
        rail.items().first().map(|item| item.state().enabled),
        Some(ControlState::disabled().enabled)
    );
}

#[test]
fn different_commands_replace_the_rail() {
    let mut rail = ActionRail::new(alloc::vec![command("Pause"), command("Cancel")]);
    if let Some(item) = rail.items_mut().first_mut() {
        item.set_state(ControlState::idle().with_pointer(PointerState::Pressed));
    }
    restate_rail(&mut rail, alloc::vec![command("Restart")]);
    assert_eq!(rail.len(), 1);
    assert_eq!(
        rail.items().first().map(|item| item.state().pointer),
        Some(PointerState::None)
    );
}

#[test]
fn restating_keeps_the_rails_focus() {
    let mut rail = ActionRail::new(alloc::vec![command("Pause"), command("Cancel")]);
    rail.adopt_focus(Some(1));
    restate_rail(&mut rail, alloc::vec![command("Pause"), command("Cancel")]);
    assert_eq!(rail.focus(), Some(1));
}
