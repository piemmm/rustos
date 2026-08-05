//! Unit tests for the action rail (`crate::rail`).
//!
//! These cover construction and an empty rail, agreement between
//! measurement and the layout [`ActionRail::render`] actually draws, pointer
//! press/release activation (same item and a different item), hover
//! changing the render, the spec §13 denied-vs-disabled distinction, the
//! full keyboard model (Up/Down clamping, Home/End, activation, a
//! non-actionable focused item consuming the key), role variants, overflow
//! omitting whole items, degenerate bounds, theme switching, the
//! heavier-contrast path, and the Edge Wake (lit versus unlit, and its
//! heavy-contrast strengthening).

use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::rail::{ActionRail, RailAction};
use crate::state::{AuthorityState, ControlRole, ControlState};
use crate::testkit::high_contrast;

const W: u32 = 160;
const CH: u32 = 28;
const GAP: u32 = 8;
const ITEM_SLOT: u32 = CH + GAP;

fn font() -> BitmapFont {
    BitmapFont::console()
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

/// The surface-y centre of item `i`, laid out flush from the top.
fn item_centre_y(i: u32) -> i32 {
    xi(i * ITEM_SLOT + CH / 2)
}

fn three_item_rail() -> ActionRail {
    ActionRail::new(vec![
        Button::labelled("One"),
        Button::labelled("Two"),
        Button::labelled("Three"),
    ])
}

/// A rail with an initial keyboard focus, mirroring `Menu::with_current`.
fn with_focus(mut rail: ActionRail, index: usize) -> ActionRail {
    rail.set_focus(Some(index));
    rail
}

fn render(rail: &ActionRail, theme: &Theme, height: u32) -> Surface {
    let mut surface = Surface::new(W, height).expect("surface");
    rail.render(
        &mut surface,
        Rect::new(0, 0, W, height),
        Scale::ONE,
        theme,
        font(),
    );
    surface
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

// --- Construction and empty rail ----------------------------------------

#[test]
fn an_empty_rail_reports_empty_and_answers_none_everywhere() {
    let theme = Theme::dark();
    let mut rail = ActionRail::new(vec![]);
    assert!(rail.is_empty());
    assert_eq!(rail.len(), 0);
    assert_eq!(rail.focus(), None);
    assert_eq!(
        rail.item_at(
            Rect::new(0, 0, W, 100),
            Scale::ONE,
            &theme,
            font(),
            Point::ORIGIN
        ),
        None
    );
    assert_eq!(
        rail.item_rect(Rect::new(0, 0, W, 100), 0, Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(rail.measured_height(Scale::ONE, &theme, font()), 0);
    assert_eq!(rail.measured_width(Scale::ONE, &theme, font()), 0);
    assert_eq!(
        rail.on_pointer(&PRESS, Rect::new(0, 0, W, 100), Scale::ONE, &theme, font()),
        None
    );
    assert_eq!(rail.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(rail.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn an_empty_rail_renders_nothing() {
    let theme = Theme::dark();
    let rail = ActionRail::new(vec![]);
    let surface = render(&rail, &theme, 100);
    let empty = Surface::new(W, 100).expect("surface");
    assert_eq!(surface.pixels(), empty.pixels());
}

#[test]
fn new_rail_holds_no_focus() {
    let rail = three_item_rail();
    assert_eq!(rail.len(), 3);
    assert!(!rail.is_empty());
    assert_eq!(rail.focus(), None);
}

// --- Measurement agrees with layout ---------------------------------------

#[test]
fn item_height_is_the_control_height_plus_the_gap() {
    let theme = Theme::dark();
    assert_eq!(
        ActionRail::item_height(Scale::ONE, &theme, font()),
        ITEM_SLOT
    );
}

#[test]
fn measured_height_is_every_items_slot_stacked() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    assert_eq!(
        rail.measured_height(Scale::ONE, &theme, font()),
        ITEM_SLOT * 3
    );
}

#[test]
fn measured_width_is_positive_and_grows_with_a_longer_label() {
    let theme = Theme::dark();
    let short = ActionRail::new(vec![Button::labelled("A")]);
    let long = ActionRail::new(vec![Button::labelled("A Much Longer Command")]);
    let w0 = short.measured_width(Scale::ONE, &theme, font());
    let w1 = long.measured_width(Scale::ONE, &theme, font());
    assert!(w0 > 0);
    assert!(w1 > w0);
}

#[test]
fn item_rect_matches_the_geometry_render_draws_at() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);
    for index in 0..3u32 {
        let rect = rail
            .item_rect(bounds, index as usize, Scale::ONE, &theme, font())
            .expect("in-range item has a rect");
        assert_eq!(rect.left(), 0);
        assert_eq!(rect.top(), xi(index * ITEM_SLOT));
        assert_eq!(rect.width, W);
        assert_eq!(rect.height, CH);
    }
    assert_eq!(
        rail.item_rect(bounds, 3, Scale::ONE, &theme, font()),
        None,
        "an out-of-range index fails closed"
    );
}

#[test]
fn item_at_is_the_reverse_mirror_of_item_rect() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);
    for index in 0..3usize {
        let rect = rail
            .item_rect(bounds, index, Scale::ONE, &theme, font())
            .expect("item rect");
        let centre = Point::new(
            rect.left() + xi(rect.width / 2),
            rect.top() + xi(rect.height / 2),
        );
        assert_eq!(
            rail.item_at(bounds, Scale::ONE, &theme, font(), centre),
            Some(index)
        );
    }
    // A point in the trailing gap belongs to no item.
    let gap_y = xi(CH + GAP / 2);
    assert_eq!(
        rail.item_at(bounds, Scale::ONE, &theme, font(), Point::new(4, gap_y)),
        None
    );
}

// --- Pointer --------------------------------------------------------------

#[test]
fn pressing_and_releasing_over_the_same_item_activates_it() {
    let theme = Theme::dark();
    let mut rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);
    let y = item_centre_y(1);
    rail.on_pointer(&moved(10, y), bounds, Scale::ONE, &theme, font());
    rail.on_pointer(&PRESS, bounds, Scale::ONE, &theme, font());
    assert_eq!(
        rail.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, font()),
        Some(RailAction::Activate { index: 1 })
    );
}

#[test]
fn releasing_over_a_different_item_activates_nothing() {
    let theme = Theme::dark();
    let mut rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);
    rail.on_pointer(
        &moved(10, item_centre_y(0)),
        bounds,
        Scale::ONE,
        &theme,
        font(),
    );
    rail.on_pointer(&PRESS, bounds, Scale::ONE, &theme, font());
    rail.on_pointer(
        &moved(10, item_centre_y(2)),
        bounds,
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(
        rail.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, font()),
        None
    );
}

#[test]
fn hovering_an_item_changes_the_render() {
    let theme = Theme::dark();
    let mut rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);
    let resting = render(&rail, &theme, h);
    rail.on_pointer(
        &moved(10, item_centre_y(0)),
        bounds,
        Scale::ONE,
        &theme,
        font(),
    );
    let hovered = render(&rail, &theme, h);
    assert_ne!(
        resting.pixels(),
        hovered.pixels(),
        "a hovered item must repaint differently from the resting rail"
    );
}

// --- §13 authority rendering ------------------------------------------

#[test]
fn a_disabled_item_never_activates() {
    let theme = Theme::dark();
    let mut rail = ActionRail::new(vec![Button::labelled("Ok"), {
        let mut b = Button::labelled("Nope");
        b.set_state(ControlState::disabled());
        b
    }]);
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);
    rail.on_pointer(
        &moved(10, item_centre_y(1)),
        bounds,
        Scale::ONE,
        &theme,
        font(),
    );
    rail.on_pointer(&PRESS, bounds, Scale::ONE, &theme, font());
    assert_eq!(
        rail.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, font()),
        None
    );
}

#[test]
fn a_denied_item_never_activates_and_shows_the_authority_mark() {
    let theme = Theme::dark();
    let mut rail = ActionRail::new(vec![Button::labelled("Ok"), {
        let mut b = Button::labelled("Delete");
        b.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
        b
    }]);
    let disabled = ActionRail::new(vec![Button::labelled("Ok"), {
        let mut b = Button::labelled("Delete");
        b.set_state(ControlState::disabled());
        b
    }]);
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let bounds = Rect::new(0, 0, W, h);

    rail.on_pointer(
        &moved(10, item_centre_y(1)),
        bounds,
        Scale::ONE,
        &theme,
        font(),
    );
    rail.on_pointer(&PRESS, bounds, Scale::ONE, &theme, font());
    assert_eq!(
        rail.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, font()),
        None
    );

    assert!(has_pixel(
        &render(&rail, &theme, h),
        premul(theme.palette().denied)
    ));
    assert!(!has_pixel(
        &render(&disabled, &theme, h),
        premul(theme.palette().denied)
    ));
}

// --- Keyboard ---------------------------------------------------------

#[test]
fn down_moves_focus_and_clamps_at_the_last_item() {
    let mut rail = three_item_rail();
    assert_eq!(rail.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(rail.focus(), Some(0));
    rail.on_key(Key::Named(NamedKey::Down));
    assert_eq!(rail.focus(), Some(1));
    rail.on_key(Key::Named(NamedKey::Down));
    assert_eq!(rail.focus(), Some(2));
    rail.on_key(Key::Named(NamedKey::Down));
    assert_eq!(rail.focus(), Some(2), "down clamps at the last item");
}

#[test]
fn up_moves_focus_and_clamps_at_the_first_item() {
    let mut rail = with_focus(three_item_rail(), 2);
    rail.on_key(Key::Named(NamedKey::Up));
    assert_eq!(rail.focus(), Some(1));
    rail.on_key(Key::Named(NamedKey::Up));
    assert_eq!(rail.focus(), Some(0));
    rail.on_key(Key::Named(NamedKey::Up));
    assert_eq!(rail.focus(), Some(0), "up clamps at the first item");
}

#[test]
fn home_and_end_jump_to_the_ends() {
    let mut rail = with_focus(three_item_rail(), 1);
    rail.on_key(Key::Named(NamedKey::Home));
    assert_eq!(rail.focus(), Some(0));
    rail.on_key(Key::Named(NamedKey::End));
    assert_eq!(rail.focus(), Some(2));
}

#[test]
fn enter_and_space_activate_the_focused_item() {
    let mut rail = with_focus(three_item_rail(), 1);
    assert_eq!(
        rail.on_key(Key::Named(NamedKey::Enter)),
        Some(RailAction::Activate { index: 1 })
    );
    assert_eq!(
        rail.on_key(Key::Char(' ')),
        Some(RailAction::Activate { index: 1 })
    );
}

#[test]
fn a_focused_disabled_item_consumes_the_key_without_activating() {
    let mut rail = ActionRail::new(vec![Button::labelled("Ok"), {
        let mut b = Button::labelled("Nope");
        b.set_state(ControlState::disabled());
        b
    }]);
    rail.on_key(Key::Named(NamedKey::End));
    assert_eq!(
        rail.focus(),
        Some(1),
        "focus still lands on the disabled item"
    );
    assert_eq!(rail.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn a_focused_denied_item_consumes_the_key_without_activating() {
    let mut rail = ActionRail::new(vec![Button::labelled("Ok"), {
        let mut b = Button::labelled("Delete");
        b.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
        b
    }]);
    rail.on_key(Key::Named(NamedKey::End));
    assert_eq!(rail.focus(), Some(1));
    assert_eq!(rail.on_key(Key::Named(NamedKey::Enter)), None);
}

// --- Roles ---------------------------------------------------------------

#[test]
fn role_variants_render_differently() {
    let theme = Theme::dark();
    let neutral = ActionRail::new(vec![Button::new(
        ButtonContent::Label("Go".into()),
        ControlRole::Neutral,
    )]);
    let primary = ActionRail::new(vec![Button::new(
        ButtonContent::Label("Go".into()),
        ControlRole::Primary,
    )]);
    let destructive = ActionRail::new(vec![Button::new(
        ButtonContent::Label("Go".into()),
        ControlRole::Destructive,
    )]);
    let h = neutral.measured_height(Scale::ONE, &theme, font());
    let a = render(&neutral, &theme, h);
    let b = render(&primary, &theme, h);
    let c = render(&destructive, &theme, h);
    assert_ne!(
        a.pixels(),
        b.pixels(),
        "primary must read differently from neutral"
    );
    assert_ne!(
        a.pixels(),
        c.pixels(),
        "destructive must read differently from neutral"
    );
    assert_ne!(
        b.pixels(),
        c.pixels(),
        "primary and destructive must read differently from each other"
    );
}

// --- Overflow --------------------------------------------------------------

#[test]
fn overflow_omits_whole_items_and_hit_testing_agrees() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    // Room for exactly one whole item plus a sliver of the next's gap.
    let h = CH + 2;
    let bounds = Rect::new(0, 0, W, h);
    let surface = render(&rail, &theme, h);

    assert_eq!(
        rail.item_rect(bounds, 0, Scale::ONE, &theme, font()),
        Some(Rect::new(0, 0, W, CH))
    );
    assert_eq!(
        rail.item_rect(bounds, 1, Scale::ONE, &theme, font()),
        None,
        "the second item has no room and is omitted"
    );
    assert_eq!(
        rail.item_at(
            bounds,
            Scale::ONE,
            &theme,
            font(),
            Point::new(10, xi(h - 1))
        ),
        None,
        "the sliver of unfinished space hit-tests to nothing"
    );
    // Nothing is drawn below the one whole item that fit.
    for y in CH..h {
        for x in 0..W {
            assert_eq!(
                surface.get(x, y),
                Some(Color::TRANSPARENT.premultiply()),
                "no partial item is ever painted"
            );
        }
    }
}

// --- Degenerate bounds -----------------------------------------------------

#[test]
fn zero_width_bounds_render_and_hit_test_to_nothing() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let bounds = Rect::new(0, 0, 0, 200);
    assert_eq!(
        rail.item_at(bounds, Scale::ONE, &theme, font(), Point::ORIGIN),
        None
    );
    assert_eq!(rail.item_rect(bounds, 0, Scale::ONE, &theme, font()), None);
    let mut surface = Surface::new(1, 200).expect("surface");
    rail.render(&mut surface, bounds, Scale::ONE, &theme, font());
    let empty = Surface::new(1, 200).expect("surface");
    assert_eq!(surface.pixels(), empty.pixels());
}

#[test]
fn zero_height_bounds_render_and_hit_test_to_nothing() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let bounds = Rect::new(0, 0, W, 0);
    assert_eq!(
        rail.item_at(bounds, Scale::ONE, &theme, font(), Point::ORIGIN),
        None
    );
    let mut surface = Surface::new(W, 1).expect("surface");
    rail.render(&mut surface, bounds, Scale::ONE, &theme, font());
    let empty = Surface::new(W, 1).expect("surface");
    assert_eq!(surface.pixels(), empty.pixels());
}

#[test]
fn bounds_shorter_than_one_item_draws_nothing() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let bounds = Rect::new(0, 0, W, CH - 1);
    assert_eq!(
        rail.item_rect(bounds, 0, Scale::ONE, &theme, font()),
        None,
        "not even the first item fits"
    );
    let mut surface = Surface::new(W, CH - 1).expect("surface");
    rail.render(&mut surface, bounds, Scale::ONE, &theme, font());
    let empty = Surface::new(W, CH - 1).expect("surface");
    assert_eq!(surface.pixels(), empty.pixels());
}

// --- Theme switching, scale, and contrast --------------------------------

#[test]
fn theme_switch_changes_the_render() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let dark = render(&rail, &Theme::dark(), h);
    let light = render(&rail, &Theme::light(), h);
    assert_ne!(dark.pixels(), light.pixels());
}

#[test]
fn a_heavier_contrast_theme_still_renders_the_rail() {
    let theme = high_contrast();
    let rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let surface = render(&rail, &theme, h);
    assert!(!surface.pixels().is_empty());
    assert_ne!(
        surface.pixels(),
        Surface::new(W, h).expect("surface").pixels(),
        "the rail must actually paint its items"
    );
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let rail = three_item_rail();
    let h = rail.measured_height(scale, &theme, font());
    let mut surface = Surface::new(W * 2, h).expect("surface");
    rail.render(
        &mut surface,
        Rect::new(0, 0, W * 2, h),
        scale,
        &theme,
        font(),
    );
    assert!(
        surface
            .pixels()
            .iter()
            .any(|&p| p != Color::TRANSPARENT.premultiply()),
        "a doubled scale must still paint every item"
    );
}

// --- Edge Wake -------------------------------------------------------------

/// How many consecutive pixels from the left of `surface`'s top row equal
/// `wake`, i.e. the Edge Wake's painted thickness.
fn wake_thickness(surface: &Surface, wake: Pixel) -> u32 {
    let mut w = 0;
    while surface.get(w, 0) == Some(wake) {
        w += 1;
    }
    w
}

#[test]
fn edge_wake_lights_the_leading_edge_only_when_lit() {
    let theme = Theme::dark();
    let rail = three_item_rail();
    let h = rail.measured_height(Scale::ONE, &theme, font());
    let lit = rail.clone().with_edge_wake(true);
    let unlit = rail.with_edge_wake(false);
    assert!(lit.edge_wake());
    assert!(!unlit.edge_wake());
    let rim_active = premul(theme.palette().rim_active);
    assert_eq!(
        render(&lit, &theme, h).get(0, 0),
        Some(rim_active),
        "a lit rail must show the Edge Wake on its own leading edge"
    );
    assert_ne!(
        render(&unlit, &theme, h).get(0, 0),
        Some(rim_active),
        "an unlit rail must not carry the Edge Wake"
    );
}

#[test]
fn heavy_contrast_strengthens_the_edge_wake() {
    let theme = Theme::dark();
    let heavy = high_contrast();
    let rail = three_item_rail().with_edge_wake(true);
    let dark_surface = render(
        &rail,
        &theme,
        rail.measured_height(Scale::ONE, &theme, font()),
    );
    let heavy_surface = render(
        &rail,
        &heavy,
        rail.measured_height(Scale::ONE, &heavy, font()),
    );
    let dark_wake = wake_thickness(&dark_surface, premul(theme.palette().rim_active));
    let heavy_wake = wake_thickness(&heavy_surface, premul(heavy.palette().rim_active));
    assert!(
        heavy_wake > dark_wake,
        "heavy contrast must widen the Edge Wake before any tint"
    );
}
