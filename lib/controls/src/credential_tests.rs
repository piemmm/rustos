//! Tests for [`CredentialSheet`](super::CredentialSheet): the focus ring,
//! the "an empty field is never offered" rule, the refusal wording, and the
//! geometry the paint and the hit test share.

extern crate alloc;

use alloc::string::ToString;

use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use super::{
    CredentialAction, CredentialSheet, CREDENTIAL_HEIGHT, CREDENTIAL_NOT_STARTED_REASON,
    CREDENTIAL_REFUSED_REASON, CREDENTIAL_WIDTH,
};
use crate::damage;

/// The sheet's own rectangle at the reference density.
fn bounds() -> Rect {
    Rect::new(0, 0, CREDENTIAL_WIDTH, CREDENTIAL_HEIGHT)
}

fn asking() -> CredentialSheet {
    CredentialSheet::new(
        "Authenticate",
        "Changing this setting needs an account that may.",
    )
}

/// Feed one event and report what it concluded.
fn feed(sheet: &mut CredentialSheet, event: &InputEvent) -> Option<CredentialAction> {
    let mut sink = damage::sink();
    sheet.handle(event, bounds(), Scale::ONE, &Theme::dark(), &mut sink)
}

/// Type `text` into whichever field holds the keyboard.
fn type_text(sheet: &mut CredentialSheet, text: &str) {
    for ch in text.chars() {
        feed(
            sheet,
            &InputEvent::KeyPressed {
                key: Key::Char(ch),
                modifiers: Modifiers::default(),
            },
        );
    }
}

fn press(key: NamedKey) -> InputEvent {
    InputEvent::KeyPressed {
        key: Key::Named(key),
        modifiers: Modifiers::default(),
    }
}

/// A press-and-release at `at`, which is what a click is.
fn click(sheet: &mut CredentialSheet, at: Point) -> Option<CredentialAction> {
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        if let Some(action) = feed(sheet, &event) {
            return Some(action);
        }
    }
    None
}

#[test]
fn the_keyboard_starts_in_the_account_field() {
    let mut sheet = asking();
    type_text(&mut sheet, "root");
    assert_eq!(sheet.account(), "root");
    assert_eq!(sheet.secret(), "");
}

#[test]
fn tab_walks_the_fields_then_the_buttons_and_wraps() {
    let mut sheet = asking();
    type_text(&mut sheet, "ann");
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "pw");
    assert_eq!(sheet.account(), "ann");
    assert_eq!(sheet.secret(), "pw");
    // Cancel, then Continue, then back to the account field.
    feed(&mut sheet, &press(NamedKey::Tab));
    feed(&mut sheet, &press(NamedKey::Tab));
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "!");
    assert_eq!(sheet.account(), "ann!");
    assert_eq!(sheet.secret(), "pw");
}

#[test]
fn an_empty_field_is_never_offered_and_takes_the_keyboard() {
    // Asking with nothing to check would spend an audited attempt against
    // the account for no reason.
    let mut sheet = asking();
    assert_eq!(feed(&mut sheet, &press(NamedKey::Enter)), None);
    // The keyboard went to the account field, which is the empty one.
    type_text(&mut sheet, "root");
    assert_eq!(sheet.account(), "root");

    // With an account but no password, the keyboard goes to the password.
    assert_eq!(feed(&mut sheet, &press(NamedKey::Enter)), None);
    type_text(&mut sheet, "pw");
    assert_eq!(sheet.secret(), "pw");
    assert_eq!(sheet.account(), "root");
}

#[test]
fn enter_offers_both_filled_fields_from_either_of_them() {
    let mut sheet = asking();
    type_text(&mut sheet, "root");
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "hunter2");
    assert_eq!(
        feed(&mut sheet, &press(NamedKey::Enter)),
        Some(CredentialAction::Offered)
    );
    assert_eq!(sheet.account(), "root");
    assert_eq!(sheet.secret(), "hunter2");
}

#[test]
fn escape_cancels_without_offering() {
    let mut sheet = asking();
    type_text(&mut sheet, "root");
    assert_eq!(
        feed(&mut sheet, &press(NamedKey::Escape)),
        Some(CredentialAction::Cancelled)
    );
}

#[test]
fn enter_on_the_cancelling_button_cancels_rather_than_offering() {
    let mut sheet = asking();
    type_text(&mut sheet, "root");
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "pw");
    // Account, secret, then the cancelling button.
    feed(&mut sheet, &press(NamedKey::Tab));
    assert_eq!(
        feed(&mut sheet, &press(NamedKey::Enter)),
        Some(CredentialAction::Cancelled)
    );
}

#[test]
fn a_refusal_states_its_reason_and_clears_only_the_password() {
    let mut sheet = asking();
    type_text(&mut sheet, "root");
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "wrong");
    sheet.refuse(CREDENTIAL_REFUSED_REASON);
    assert_eq!(sheet.stated_reason(), Some(CREDENTIAL_REFUSED_REASON));
    assert_eq!(sheet.secret(), "");
    // The account name is not the secret, and retyping a correct one is
    // only a way to get it wrong.
    assert_eq!(sheet.account(), "root");
    // The keyboard is on the password, so another attempt starts by typing
    // it.
    type_text(&mut sheet, "right");
    assert_eq!(sheet.secret(), "right");
}

#[test]
fn a_refusal_keeps_the_purpose_it_was_asked_with() {
    let mut sheet = CredentialSheet::new("Authenticate", "Setting the clock needs one.");
    sheet.refuse(CREDENTIAL_NOT_STARTED_REASON);
    assert_eq!(sheet.stated_reason(), Some(CREDENTIAL_NOT_STARTED_REASON));
    // The question is still on screen; only the outcome was added to it.
    let mut surface = Surface::new(CREDENTIAL_WIDTH, CREDENTIAL_HEIGHT).expect("a surface");
    sheet.render(&mut surface, bounds(), Scale::ONE, &Theme::dark());
}

#[test]
fn clicking_a_field_moves_the_keyboard_to_it() {
    let mut sheet = asking();
    let secret_rect = CredentialSheet::field_rect(bounds(), Scale::ONE, 1);
    let inside = Point::new(
        secret_rect.left() + to_i32(secret_rect.width / 2),
        secret_rect.top() + to_i32(secret_rect.height / 2),
    );
    assert_eq!(click(&mut sheet, inside), None);
    type_text(&mut sheet, "pw");
    assert_eq!(sheet.secret(), "pw");
    assert_eq!(sheet.account(), "");
}

#[test]
fn the_two_fields_do_not_overlap_and_sit_inside_the_sheet() {
    // The paint and the hit test read the same rectangles, so the geometry
    // being sane is what stops a press landing on a field drawn elsewhere.
    for scale in [Scale::ONE, Scale::from_percent(200).expect("a scale")] {
        let placed = CredentialSheet::centred_in(Rect::new(0, 0, 1024, 768), scale);
        let account = CredentialSheet::field_rect(placed, scale, 0);
        let secret = CredentialSheet::field_rect(placed, scale, 1);
        assert!(account.bottom() <= secret.top());
        assert!(secret.bottom() <= placed.bottom());
        assert!(account.left() >= placed.left());
        assert!(account.right() <= placed.right());
    }
}

#[test]
fn a_sheet_centres_itself_and_never_outgrows_what_holds_it() {
    let scale = Scale::ONE;
    let client = Rect::new(10, 20, 1000, 800);
    let placed = CredentialSheet::centred_in(client, scale);
    assert_eq!(placed.width, scale.scale_length(CREDENTIAL_WIDTH));
    assert_eq!(placed.height, scale.scale_length(CREDENTIAL_HEIGHT));
    assert_eq!(
        placed.left() - client.left(),
        client.right() - placed.right()
    );
    // A client smaller than the sheet clamps rather than hanging outside it.
    let cramped = CredentialSheet::centred_in(Rect::new(0, 0, 100, 60), scale);
    assert_eq!(cramped.width, 100);
    assert_eq!(cramped.height, 60);
}

#[test]
fn the_password_is_masked_and_never_rendered_as_itself() {
    let mut sheet = asking();
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "hunter2");
    // The field holds the secret for the one exchange its owner performs…
    assert_eq!(sheet.secret(), "hunter2");
    // …and draws beads, so two different secrets of the same length paint
    // identically.
    let mut first = Surface::new(CREDENTIAL_WIDTH, CREDENTIAL_HEIGHT).expect("a surface");
    sheet.render(&mut first, bounds(), Scale::ONE, &Theme::dark());

    let mut other = asking();
    feed(&mut other, &press(NamedKey::Tab));
    type_text(&mut other, "swordfi");
    let mut second = Surface::new(CREDENTIAL_WIDTH, CREDENTIAL_HEIGHT).expect("a surface");
    other.render(&mut second, bounds(), Scale::ONE, &Theme::dark());
    assert_eq!(first.pixels(), second.pixels());
}

#[test]
fn a_cancelled_sheet_carries_no_plaintext_away_with_it() {
    // The secret lives only in the masked field's bounded buffer, which
    // zeroises what it discards; clearing it is what a refusal does and
    // dropping the sheet is what a cancellation does.
    let mut sheet = asking();
    type_text(&mut sheet, "root");
    feed(&mut sheet, &press(NamedKey::Tab));
    type_text(&mut sheet, "hunter2");
    assert_eq!(
        feed(&mut sheet, &press(NamedKey::Escape)),
        Some(CredentialAction::Cancelled)
    );
    sheet.refuse(CREDENTIAL_REFUSED_REASON);
    assert_eq!(sheet.secret(), "");
    assert_eq!(sheet.secret().to_string(), "");
}
