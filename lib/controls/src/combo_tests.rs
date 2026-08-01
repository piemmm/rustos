//! Unit tests for the combo box (spec §11.9, §20 checklist).
//!
//! These cover the collapsed field (plate, selection text, disclosure), the
//! open/close lifecycle by pointer and keyboard, choosing a row from the popup
//! menu it composes, the outside-press and Escape dismissals, the fail-closed
//! denied field, popup sizing, theme switching, and scale.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::combo::{ComboAction, ComboBox};
use crate::state::{AuthorityState, ControlState};

const W: u32 = 160;
const H: u32 = 28;
const ROW_H: u32 = 28;
const BORDER: u32 = 1;

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn choices() -> Vec<alloc::string::String> {
    vec!["Low".to_string(), "Med".to_string(), "High".to_string()]
}

fn combo() -> ComboBox {
    ComboBox::new(choices())
}

fn field_bounds() -> Rect {
    Rect::new(0, 0, W, H)
}

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

/// Open the combo (by a field click) and return its popup bounds directly
/// below the field.
fn open_and_popup(combo: &mut ComboBox, theme: &Theme) -> Rect {
    let field = field_bounds();
    combo.set_focused(true);
    // A field click opens the list.
    combo.on_pointer(
        &moved(10, 14),
        field,
        Rect::new(0, 0, 0, 0),
        Scale::ONE,
        theme,
    );
    combo.on_pointer(&PRESS, field, Rect::new(0, 0, 0, 0), Scale::ONE, theme);
    combo.on_pointer(&RELEASE, field, Rect::new(0, 0, 0, 0), Scale::ONE, theme);
    let (pw, ph) = combo.popup_size(W, Scale::ONE, theme, font());
    Rect::new(0, xi(H), pw, ph)
}

// --- Collapsed field ----------------------------------------------------

#[test]
fn new_combo_has_no_selection_and_is_collapsed() {
    let combo = combo();
    assert_eq!(combo.selected(), None);
    assert!(!combo.is_expanded());
    assert_eq!(combo.selected_text(), None);
}

#[test]
fn with_selected_shows_the_choice_text() {
    let combo = combo().with_selected(2);
    assert_eq!(combo.selected(), Some(2));
    assert_eq!(combo.selected_text(), Some("High"));
}

#[test]
fn collapsed_field_paints_a_plate_and_rim() {
    let theme = Theme::dark();
    let mut surface = Surface::new(W, H).expect("surface");
    combo()
        .with_selected(0)
        .render(&mut surface, field_bounds(), Scale::ONE, &theme, font());
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
    assert_eq!(surface.get(0, H / 2), Some(premul(theme.palette().rim)));
}

// --- Open / close lifecycle --------------------------------------------

#[test]
fn clicking_the_field_opens_the_list() {
    let theme = Theme::dark();
    let mut combo = combo();
    combo.set_focused(true);
    let field = field_bounds();
    let none = Rect::new(0, 0, 0, 0);
    combo.on_pointer(&moved(10, 14), field, none, Scale::ONE, &theme);
    combo.on_pointer(&PRESS, field, none, Scale::ONE, &theme);
    assert_eq!(
        combo.on_pointer(&RELEASE, field, none, Scale::ONE, &theme),
        Some(ComboAction::Opened)
    );
    assert!(combo.is_expanded());
}

#[test]
fn clicking_a_popup_row_selects_and_closes() {
    let theme = Theme::dark();
    let mut combo = combo();
    let popup = open_and_popup(&mut combo, &theme);
    let field = field_bounds();
    // Row 1 centre in popup coordinates.
    let y = xi(H) + xi(BORDER + ROW_H + ROW_H / 2);
    combo.on_pointer(&moved(40, y), field, popup, Scale::ONE, &theme);
    combo.on_pointer(&PRESS, field, popup, Scale::ONE, &theme);
    assert_eq!(
        combo.on_pointer(&RELEASE, field, popup, Scale::ONE, &theme),
        Some(ComboAction::Selected { index: 1 })
    );
    assert!(!combo.is_expanded());
    assert_eq!(combo.selected(), Some(1));
}

#[test]
fn pressing_outside_closes_the_list() {
    let theme = Theme::dark();
    let mut combo = combo();
    let popup = open_and_popup(&mut combo, &theme);
    let field = field_bounds();
    combo.on_pointer(&moved(500, 500), field, popup, Scale::ONE, &theme);
    assert_eq!(
        combo.on_pointer(&PRESS, field, popup, Scale::ONE, &theme),
        Some(ComboAction::Closed)
    );
    assert!(!combo.is_expanded());
}

// --- Keyboard -----------------------------------------------------------

#[test]
fn focused_field_opens_on_down_and_navigates_then_selects() {
    let mut combo = combo();
    combo.set_focused(true);
    assert_eq!(
        combo.on_key(Key::Named(NamedKey::Down)),
        Some(ComboAction::Opened)
    );
    assert!(combo.is_expanded());
    // Now expanded: Down moves the menu current (starts at 0), Enter chooses.
    assert_eq!(combo.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        combo.on_key(Key::Named(NamedKey::Enter)),
        Some(ComboAction::Selected { index: 1 })
    );
    assert!(!combo.is_expanded());
}

#[test]
fn escape_closes_an_open_list() {
    let mut combo = combo();
    combo.set_focused(true);
    combo.on_key(Key::Named(NamedKey::Down));
    assert_eq!(
        combo.on_key(Key::Named(NamedKey::Escape)),
        Some(ComboAction::Closed)
    );
    assert!(!combo.is_expanded());
}

#[test]
fn unfocused_field_ignores_keys() {
    let mut combo = combo();
    assert_eq!(combo.on_key(Key::Named(NamedKey::Down)), None);
    assert!(!combo.is_expanded());
}

// --- §13 authority ------------------------------------------------------

#[test]
fn denied_field_never_opens() {
    let theme = Theme::dark();
    let mut combo = combo();
    combo.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    combo.set_focused(true);
    let field = field_bounds();
    let none = Rect::new(0, 0, 0, 0);
    combo.on_pointer(&moved(10, 14), field, none, Scale::ONE, &theme);
    combo.on_pointer(&PRESS, field, none, Scale::ONE, &theme);
    assert_eq!(
        combo.on_pointer(&RELEASE, field, none, Scale::ONE, &theme),
        None
    );
    assert!(!combo.is_expanded());
    // A denied field also shows the lock bead.
    let mut surface = Surface::new(W, H).expect("surface");
    combo.render(&mut surface, field, Scale::ONE, &theme, font());
    assert!(has_pixel(&surface, premul(theme.palette().denied)));
}

// --- Popup sizing, theme, scale ----------------------------------------

#[test]
fn popup_is_never_narrower_than_the_field() {
    let theme = Theme::dark();
    let (w, h) = combo().popup_size(W, Scale::ONE, &theme, font());
    assert!(w >= W);
    assert_eq!(h, BORDER * 2 + 3 * ROW_H);
}

#[test]
fn theme_switch_repaints_the_field_rim() {
    let combo = combo().with_selected(0);
    let mut dark = Surface::new(W, H).expect("surface");
    let mut light = Surface::new(W, H).expect("surface");
    combo.render(
        &mut dark,
        field_bounds(),
        Scale::ONE,
        &Theme::dark(),
        font(),
    );
    combo.render(
        &mut light,
        field_bounds(),
        Scale::ONE,
        &Theme::light(),
        font(),
    );
    assert_ne!(dark.get(0, H / 2), light.get(0, H / 2));
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let mut surface = Surface::new(W * 2, H * 2).expect("surface");
    combo().with_selected(0).render(
        &mut surface,
        Rect::new(0, 0, W * 2, H * 2),
        scale,
        &theme,
        font(),
    );
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_combo_box() {
    let theme = Theme::dark();
    let render = |combo: &ComboBox| {
        let mut surface = Surface::new(W, H).expect("surface");
        combo.render(&mut surface, field_bounds(), Scale::ONE, &theme, font());
        surface
    };

    // Two samples clear of the field, so only the recorded coordinate differs.
    let none = Rect::new(0, 0, 0, 0);
    let mut a = combo();
    let mut b = a.clone();
    a.on_pointer(&moved(400, 60), field_bounds(), none, Scale::ONE, &theme);
    b.on_pointer(&moved(460, 70), field_bounds(), none, Scale::ONE, &theme);
    assert_eq!(
        a, b,
        "a coordinate clear of the field is not a drawn property"
    );
    assert_eq!(
        render(&a).pixels(),
        render(&b).pixels(),
        "…and the two must therefore paint identically"
    );

    // A press on the field only latches; the disposition a press *shows* is
    // the owner-set `ControlState`, which is still compared.
    let mut latched = combo();
    latched.on_pointer(&moved(8, 14), field_bounds(), none, Scale::ONE, &theme);
    latched.on_pointer(&PRESS, field_bounds(), none, Scale::ONE, &theme);
    let resting = combo();
    assert_eq!(latched, resting, "the press latch is not a drawn property");
    assert_eq!(
        render(&latched).pixels(),
        render(&resting).pixels(),
        "…and the two must therefore paint identically"
    );

    let mut pressed = combo();
    let mut state = ControlState::idle();
    state.pointer = crate::state::PointerState::Pressed;
    pressed.set_state(state);
    assert_ne!(resting, pressed, "the shown pressed disposition is visible");
}
