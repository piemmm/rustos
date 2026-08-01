//! Unit tests for the menu command surface (spec §11.10, §20 checklist).
//!
//! These cover measurement (preferred sizing, the elevated plate and rim), the
//! current-row highlight, the spec §13 denied-vs-disabled distinction and the
//! Authority Mark, the destructive danger rail, the full keyboard model
//! (Up/Down wrap, Home/End, Enter/Space, Escape, submenu open), the pointer
//! hover/click model with fail-closed non-actionable rows, hit-testing, theme
//! switching, and scale.

use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::menu::{Menu, MenuAction, MenuItem};
use crate::state::{AuthorityState, ControlRole, ControlState};

const W: u32 = 200;
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

fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

/// The surface-y centre of row `i`.
fn row_centre_y(i: u32) -> i32 {
    xi(BORDER + i * ROW_H + ROW_H / 2)
}

fn render(menu: &Menu, theme: &Theme, height: u32) -> Surface {
    let mut surface = Surface::new(W, height).expect("surface");
    menu.render(
        &mut surface,
        Rect::new(0, 0, W, height),
        Scale::ONE,
        theme,
        font(),
    );
    surface
}

fn three_item_menu() -> Menu {
    Menu::new(vec![
        MenuItem::new("Open"),
        MenuItem::new("Save").with_shortcut("Ctrl+S"),
        MenuItem::new("Close"),
    ])
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

// --- Measurement --------------------------------------------------------

#[test]
fn preferred_height_covers_the_rims_and_every_row() {
    let menu = three_item_menu();
    assert_eq!(
        menu.preferred_height(Scale::ONE, &Theme::dark()),
        BORDER * 2 + 3 * ROW_H
    );
}

#[test]
fn preferred_width_is_positive_and_grows_with_a_shortcut() {
    let plain = Menu::new(vec![MenuItem::new("A")]);
    let with_shortcut = Menu::new(vec![MenuItem::new("A").with_shortcut("Ctrl+Shift+A")]);
    let theme = Theme::dark();
    let w0 = plain.preferred_width(Scale::ONE, &theme, font());
    let w1 = with_shortcut.preferred_width(Scale::ONE, &theme, font());
    assert!(w0 > 0);
    assert!(w1 > w0);
}

#[test]
fn menu_paints_the_elevated_plate_and_rim() {
    let theme = Theme::dark();
    let h = three_item_menu().preferred_height(Scale::ONE, &theme);
    let surface = render(&three_item_menu(), &theme, h);
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
    assert_eq!(surface.get(0, h / 2), Some(premul(theme.palette().rim)));
    // The extreme corner is rounded away.
    assert_eq!(surface.get(0, 0), Some(Color::TRANSPARENT.premultiply()));
}

// --- Current-row highlight ---------------------------------------------

#[test]
fn current_actionable_row_fills_with_the_accent() {
    let theme = Theme::dark();
    let menu = three_item_menu().with_current(1);
    let h = menu.preferred_height(Scale::ONE, &theme);
    let surface = render(&menu, &theme, h);
    let top = BORDER + ROW_H;
    assert!(region_has(
        &surface,
        (BORDER + 2, W - BORDER - 2),
        (top + 2, top + ROW_H - 2),
        premul(theme.palette().accent),
    ));
}

#[test]
fn non_current_rows_stay_on_the_plate() {
    let theme = Theme::dark();
    let menu = three_item_menu().with_current(0);
    let h = menu.preferred_height(Scale::ONE, &theme);
    let surface = render(&menu, &theme, h);
    // Row 2 (never current) keeps the raised surface in its empty trailing
    // area (away from the leading label glyphs).
    let top = BORDER + 2 * ROW_H;
    assert_eq!(
        surface.get(W - 15, top + ROW_H / 2),
        Some(premul(theme.palette().surface_raised))
    );
}

// --- §13 authority rendering -------------------------------------------

#[test]
fn denied_row_shows_the_lock_bead_and_disabled_does_not() {
    let theme = Theme::dark();
    let denied = Menu::new(vec![
        MenuItem::new("Ok"),
        MenuItem::new("Delete")
            .with_state(ControlState::idle().with_authority(AuthorityState::Denied)),
    ]);
    let disabled = Menu::new(vec![
        MenuItem::new("Ok"),
        MenuItem::new("Delete").with_state(ControlState::disabled()),
    ]);
    let h = denied.preferred_height(Scale::ONE, &theme);
    assert!(has_pixel(
        &render(&denied, &theme, h),
        premul(theme.palette().denied)
    ));
    assert!(!has_pixel(
        &render(&disabled, &theme, h),
        premul(theme.palette().denied)
    ));
}

#[test]
fn destructive_row_paints_a_leading_danger_rail() {
    let theme = Theme::dark();
    let menu = Menu::new(vec![
        MenuItem::new("Keep"),
        MenuItem::new("Erase disk").with_role(ControlRole::Destructive),
    ]);
    let h = menu.preferred_height(Scale::ONE, &theme);
    let surface = render(&menu, &theme, h);
    let top = BORDER + ROW_H;
    assert!(region_has(
        &surface,
        (BORDER, BORDER + 3),
        (top, top + ROW_H),
        premul(theme.palette().danger),
    ));
}

// --- Keyboard -----------------------------------------------------------

#[test]
fn down_and_up_move_the_current_row_and_wrap() {
    let mut menu = three_item_menu();
    assert_eq!(menu.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(menu.current(), Some(0));
    menu.on_key(Key::Named(NamedKey::Down));
    menu.on_key(Key::Named(NamedKey::Down));
    assert_eq!(menu.current(), Some(2));
    menu.on_key(Key::Named(NamedKey::Down));
    assert_eq!(menu.current(), Some(0), "down wraps to the top");
    menu.on_key(Key::Named(NamedKey::Up));
    assert_eq!(menu.current(), Some(2), "up wraps to the bottom");
}

#[test]
fn home_and_end_jump_to_the_ends() {
    let mut menu = three_item_menu().with_current(1);
    menu.on_key(Key::Named(NamedKey::Home));
    assert_eq!(menu.current(), Some(0));
    menu.on_key(Key::Named(NamedKey::End));
    assert_eq!(menu.current(), Some(2));
}

#[test]
fn enter_and_space_activate_the_current_row() {
    let mut menu = three_item_menu().with_current(1);
    assert_eq!(
        menu.on_key(Key::Named(NamedKey::Enter)),
        Some(MenuAction::Activated { index: 1 })
    );
    assert_eq!(
        menu.on_key(Key::Char(' ')),
        Some(MenuAction::Activated { index: 1 })
    );
}

#[test]
fn escape_dismisses() {
    let mut menu = three_item_menu().with_current(0);
    assert_eq!(
        menu.on_key(Key::Named(NamedKey::Escape)),
        Some(MenuAction::Dismissed)
    );
}

#[test]
fn submenu_parent_opens_on_right_and_enter() {
    let mut menu = Menu::new(vec![MenuItem::new("More").with_submenu(true)]).with_current(0);
    assert_eq!(
        menu.on_key(Key::Named(NamedKey::Right)),
        Some(MenuAction::OpenSubmenu { index: 0 })
    );
    assert_eq!(
        menu.on_key(Key::Named(NamedKey::Enter)),
        Some(MenuAction::OpenSubmenu { index: 0 })
    );
}

#[test]
fn disabled_current_row_never_activates() {
    let mut menu = Menu::new(vec![
        MenuItem::new("Ok"),
        MenuItem::new("Nope").with_state(ControlState::disabled()),
    ]);
    menu.on_key(Key::Named(NamedKey::End));
    assert_eq!(menu.current(), Some(1));
    assert_eq!(menu.on_key(Key::Named(NamedKey::Enter)), None);
}

// --- Pointer ------------------------------------------------------------

#[test]
fn hover_sets_the_current_row_and_click_activates_it() {
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    let y = row_centre_y(1);
    assert_eq!(
        menu.on_pointer(&moved(xi(W) / 2, y), bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(menu.current(), Some(1));
    assert_eq!(menu.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        menu.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(MenuAction::Activated { index: 1 })
    );
}

#[test]
fn release_outside_the_pressed_row_does_not_activate() {
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    menu.on_pointer(
        &moved(xi(W) / 2, row_centre_y(0)),
        bounds,
        Scale::ONE,
        &theme,
    );
    menu.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    menu.on_pointer(
        &moved(xi(W) / 2, row_centre_y(2)),
        bounds,
        Scale::ONE,
        &theme,
    );
    assert_eq!(menu.on_pointer(&RELEASE, bounds, Scale::ONE, &theme), None);
}

#[test]
fn row_at_maps_points_to_rows() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    assert_eq!(
        menu.row_at(
            bounds,
            Scale::ONE,
            &theme,
            Point::new(xi(W) / 2, row_centre_y(0))
        ),
        Some(0)
    );
    assert_eq!(
        menu.row_at(
            bounds,
            Scale::ONE,
            &theme,
            Point::new(xi(W) / 2, row_centre_y(2))
        ),
        Some(2)
    );
    assert_eq!(
        menu.row_at(bounds, Scale::ONE, &theme, Point::new(-5, 5)),
        None
    );
}

#[test]
fn row_rect_is_the_forward_mirror_of_row_at() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    // Every row's rect centre hit-tests back to that row, and an out-of-range
    // index yields no rect (fail closed).
    for index in 0..3 {
        let rect = menu
            .row_rect(index, bounds, Scale::ONE, &theme)
            .expect("row rect");
        let centre = Point::new(
            rect.left() + i32::try_from(rect.width / 2).unwrap(),
            rect.top() + i32::try_from(rect.height / 2).unwrap(),
        );
        assert_eq!(
            menu.row_at(bounds, Scale::ONE, &theme, centre),
            Some(index),
            "row {index} rect round-trips"
        );
    }
    assert_eq!(menu.row_rect(3, bounds, Scale::ONE, &theme), None);
}

// --- Theme switching and scale -----------------------------------------

#[test]
fn theme_switch_repaints_the_rim() {
    let menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &Theme::dark());
    let dark = render(&menu, &Theme::dark(), h);
    let light = render(&menu, &Theme::light(), h);
    assert_ne!(dark.get(0, h / 2), light.get(0, h / 2));
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let menu = three_item_menu();
    let h = menu.preferred_height(scale, &theme);
    let mut surface = Surface::new(W * 2, h).expect("surface");
    menu.render(
        &mut surface,
        Rect::new(0, 0, W * 2, h),
        scale,
        &theme,
        font(),
    );
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_menu() {
    let theme = Theme::dark();
    let h = ROW_H * 3 + BORDER * 2;
    let bounds = Rect::new(0, 0, W, h);
    let away = moved(xi(W) + 40, xi(h) + 40);

    // Two samples clear of the menu, so only the recorded coordinate differs.
    let mut a = three_item_menu();
    let mut b = a.clone();
    a.on_pointer(&away, bounds, Scale::ONE, &theme);
    b.on_pointer(&moved(xi(W) + 90, xi(h) + 12), bounds, Scale::ONE, &theme);
    assert_eq!(
        a, b,
        "a coordinate clear of the menu is not a drawn property"
    );
    assert_eq!(
        render(&a, &theme, h).pixels(),
        render(&b, &theme, h).pixels(),
        "…and the two must therefore paint identically"
    );

    // Both hover row 1, so the highlight matches; only one of them holds the
    // press, and which row a release would activate is not drawn.
    let over_row = moved(xi(W) / 2, row_centre_y(1));
    let mut latched = three_item_menu();
    latched.on_pointer(&over_row, bounds, Scale::ONE, &theme);
    latched.on_pointer(&PRESS, bounds, Scale::ONE, &theme);
    let mut hovered = three_item_menu();
    hovered.on_pointer(&over_row, bounds, Scale::ONE, &theme);
    assert_eq!(latched, hovered, "the armed row is not a drawn property");
    assert_eq!(
        render(&latched, &theme, h).pixels(),
        render(&hovered, &theme, h).pixels(),
        "…and the two must therefore paint identically"
    );
}
