//! Unit tests for the menu command surface (spec §11.10, §20 checklist).
//!
//! These cover measurement (preferred sizing, the elevated plate and rim), the
//! current-row highlight, the spec §13 denied-vs-disabled distinction and the
//! Authority Mark, the destructive danger rail, the full keyboard model
//! (Up/Down wrap, Home/End, Enter/Space, Escape, submenu open), the pointer
//! hover/click model with fail-closed non-actionable rows, hit-testing, theme
//! switching, and scale.

use alloc::vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::damage::sink;
use crate::menu::{Menu, MenuAction, MenuItem};
use crate::state::{AuthorityState, ControlRole, ControlState};
use crate::testkit::{control_font, high_contrast, text_ladder};

const W: u32 = 200;
const ROW_H: u32 = 28;
const BORDER: u32 = 1;

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
    menu.render(&mut surface, Rect::new(0, 0, W, height), Scale::ONE, theme);
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
    let w0 = plain.preferred_width(Scale::ONE, &theme);
    let w1 = with_shortcut.preferred_width(Scale::ONE, &theme);
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
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Down),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
    assert_eq!(menu.current(), Some(0));
    menu.on_key(
        Key::Named(NamedKey::Down),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    menu.on_key(
        Key::Named(NamedKey::Down),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(2));
    menu.on_key(
        Key::Named(NamedKey::Down),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(0), "down wraps to the top");
    menu.on_key(
        Key::Named(NamedKey::Up),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(2), "up wraps to the bottom");
}

#[test]
fn home_and_end_jump_to_the_ends() {
    let theme = Theme::dark();
    let mut menu = three_item_menu().with_current(1);
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    menu.on_key(
        Key::Named(NamedKey::Home),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(0));
    menu.on_key(
        Key::Named(NamedKey::End),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(2));
}

#[test]
fn enter_and_space_activate_the_current_row() {
    let theme = Theme::dark();
    let mut menu = three_item_menu().with_current(1);
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Enter),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(MenuAction::Activated { index: 1 })
    );
    assert_eq!(
        menu.on_key(Key::Char(' '), bounds, Scale::ONE, &theme, &mut sink()),
        Some(MenuAction::Activated { index: 1 })
    );
}

#[test]
fn escape_dismisses() {
    let theme = Theme::dark();
    let mut menu = three_item_menu().with_current(0);
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Escape),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(MenuAction::Dismissed)
    );
}

#[test]
fn submenu_parent_opens_on_right_and_enter() {
    let theme = Theme::dark();
    let mut menu = Menu::new(vec![MenuItem::new("More").with_submenu(true)]).with_current(0);
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Right),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(MenuAction::OpenSubmenu { index: 0 })
    );
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Enter),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(MenuAction::OpenSubmenu { index: 0 })
    );
}

#[test]
fn disabled_current_row_never_activates() {
    let theme = Theme::dark();
    let mut menu = Menu::new(vec![
        MenuItem::new("Ok"),
        MenuItem::new("Nope").with_state(ControlState::disabled()),
    ]);
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    menu.on_key(
        Key::Named(NamedKey::End),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(1));
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Enter),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
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
        menu.on_pointer(
            &moved(xi(W) / 2, y),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        None
    );
    assert_eq!(menu.current(), Some(1));
    assert_eq!(
        menu.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        menu.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
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
        &mut sink(),
    );
    menu.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    menu.on_pointer(
        &moved(xi(W) / 2, row_centre_y(2)),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(
        menu.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
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
    menu.render(&mut surface, Rect::new(0, 0, W * 2, h), scale, &theme);
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
    a.on_pointer(&away, bounds, Scale::ONE, &theme, &mut sink());
    b.on_pointer(
        &moved(xi(W) + 90, xi(h) + 12),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
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
    latched.on_pointer(&over_row, bounds, Scale::ONE, &theme, &mut sink());
    latched.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink());
    let mut hovered = three_item_menu();
    hovered.on_pointer(&over_row, bounds, Scale::ONE, &theme, &mut sink());
    assert_eq!(latched, hovered, "the armed row is not a drawn property");
    assert_eq!(
        render(&latched, &theme, h).pixels(),
        render(&hovered, &theme, h).pixels(),
        "…and the two must therefore paint identically"
    );
}

/// A menu whose middle row opens a second group.
fn grouped_menu() -> Menu {
    Menu::new(vec![
        MenuItem::new("Open"),
        MenuItem::new("Configure").with_group_break(true),
        MenuItem::new("Shut Down").with_role(ControlRole::Destructive),
    ])
}

#[test]
fn a_group_break_adds_its_band_to_the_preferred_height() {
    let theme = Theme::dark();
    let plain = three_item_menu().preferred_height(Scale::ONE, &theme);
    let grouped = grouped_menu().preferred_height(Scale::ONE, &theme);
    assert!(
        grouped > plain,
        "the divider band must claim height of its own: {grouped} vs {plain}"
    );
    // One break only, so exactly one band is added.
    let band = grouped - plain;
    let two_breaks = Menu::new(vec![
        MenuItem::new("Open"),
        MenuItem::new("Configure").with_group_break(true),
        MenuItem::new("Shut Down").with_group_break(true),
    ])
    .preferred_height(Scale::ONE, &theme);
    assert_eq!(two_breaks, plain + band * 2, "each break adds one band");
}

#[test]
fn a_group_break_on_the_first_row_draws_nothing() {
    let theme = Theme::dark();
    let broken = Menu::new(vec![
        MenuItem::new("Open").with_group_break(true),
        MenuItem::new("Save").with_shortcut("Ctrl+S"),
        MenuItem::new("Close"),
    ]);
    let h = three_item_menu().preferred_height(Scale::ONE, &theme);
    assert_eq!(
        broken.preferred_height(Scale::ONE, &theme),
        h,
        "there is no preceding group to divide from"
    );
    assert_eq!(
        render(&broken, &theme, h).pixels(),
        render(&three_item_menu(), &theme, h).pixels(),
        "…and it must therefore paint as an unbroken menu"
    );
}

#[test]
fn the_divider_paints_a_rule_between_the_two_groups() {
    let theme = Theme::dark();
    let menu = grouped_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    let surface = render(&menu, &theme, h);

    // The rule sits in the gap between the row that closes the first group
    // and the row that opens the second.
    let above = menu.row_rect(0, bounds, Scale::ONE, &theme).expect("row 0");
    let below = menu.row_rect(1, bounds, Scale::ONE, &theme).expect("row 1");
    let gap_top = u32::try_from(above.top()).expect("in range") + above.height;
    let gap_bottom = u32::try_from(below.top()).expect("in range");
    assert!(gap_bottom > gap_top, "the divider band separates the rows");
    assert!(
        region_has(
            &surface,
            (BORDER, W - BORDER),
            (gap_top, gap_bottom),
            premul(theme.palette().border),
        ),
        "the divider rule is drawn in the band between the groups"
    );
}

#[test]
fn a_point_in_the_divider_band_belongs_to_no_row() {
    let theme = Theme::dark();
    let menu = grouped_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    let above = menu.row_rect(0, bounds, Scale::ONE, &theme).expect("row 0");
    let below = menu.row_rect(1, bounds, Scale::ONE, &theme).expect("row 1");
    let gap_top = above.top() + xi(above.height);
    let gap_bottom = below.top();
    for y in gap_top..gap_bottom {
        assert_eq!(
            menu.row_at(bounds, Scale::ONE, &theme, Point::new(xi(W) / 2, y)),
            None,
            "the divider band at y={y} is inert"
        );
    }
}

#[test]
fn a_click_landing_in_the_divider_band_activates_nothing() {
    let theme = Theme::dark();
    let mut menu = grouped_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    let above = menu.row_rect(0, bounds, Scale::ONE, &theme).expect("row 0");
    let y = above.top() + xi(above.height);
    let at = moved(xi(W) / 2, y);
    assert_eq!(
        menu.on_pointer(&at, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(menu.current(), None, "no row highlights from the gap");
    assert_eq!(
        menu.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
    assert_eq!(
        menu.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &mut sink()),
        None
    );
}

#[test]
fn keyboard_navigation_is_unaffected_by_a_group_break() {
    // The divider is a property of the row that opens the group, not a row of
    // its own, so there is no separator slot for Down/Up to skip over.
    let theme = Theme::dark();
    let mut menu = grouped_menu();
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    for expected in [0, 1, 2, 0] {
        menu.on_key(
            Key::Named(NamedKey::Down),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink(),
        );
        assert_eq!(menu.current(), Some(expected));
    }
    menu.on_key(
        Key::Named(NamedKey::End),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(2), "End reaches the last command");
    menu.on_key(
        Key::Named(NamedKey::Home),
        bounds,
        Scale::ONE,
        &theme,
        &mut sink(),
    );
    assert_eq!(menu.current(), Some(0), "Home reaches the first command");
    assert_eq!(
        menu.on_key(
            Key::Named(NamedKey::Enter),
            bounds,
            Scale::ONE,
            &theme,
            &mut sink()
        ),
        Some(MenuAction::Activated { index: 0 }),
        "reported indices stay indices into the caller's command list"
    );
}

#[test]
fn grouped_row_rects_still_mirror_hit_testing() {
    let theme = Theme::dark();
    let menu = grouped_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    for index in 0..3 {
        let rect = menu
            .row_rect(index, bounds, Scale::ONE, &theme)
            .expect("row rect");
        let centre = Point::new(
            rect.left() + xi(rect.width / 2),
            rect.top() + xi(rect.height / 2),
        );
        assert_eq!(
            menu.row_at(bounds, Scale::ONE, &theme, centre),
            Some(index),
            "grouped row {index} rect round-trips"
        );
    }
    assert_eq!(menu.row_rect(3, bounds, Scale::ONE, &theme), None);
}

#[test]
fn a_heavier_contrast_theme_thickens_the_divider() {
    let theme = Theme::dark();
    let heavy = high_contrast();
    let plain_rows = three_item_menu();
    let grouped = grouped_menu();
    let band = grouped.preferred_height(Scale::ONE, &theme)
        - plain_rows.preferred_height(Scale::ONE, &theme);
    let heavy_band = grouped.preferred_height(Scale::ONE, &heavy)
        - plain_rows.preferred_height(Scale::ONE, &heavy);
    assert!(
        heavy_band > band,
        "high contrast must widen the rule: {heavy_band} vs {band}"
    );
}

// --- Anchored popup placement -------------------------------------------

#[test]
fn anchored_rect_places_the_top_left_at_the_anchor_when_it_fits() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let viewport = Rect::new(0, 0, 800, 600);
    let rect = menu.anchored_rect(Point::new(40, 30), viewport, Scale::ONE, &theme);
    assert_eq!((rect.origin.x, rect.origin.y), (40, 30));
    assert!(rect.width > 0 && rect.height > 0);
}

#[test]
fn anchored_rect_shifts_left_near_the_right_edge() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let viewport = Rect::new(0, 0, 800, 600);
    let anchor = Point::new(798, 30);
    let rect = menu.anchored_rect(anchor, viewport, Scale::ONE, &theme);
    assert!(
        rect.origin.x < anchor.x,
        "an anchor near the right edge shifts the menu left off it"
    );
    assert!(rect.origin.x + xi(rect.width) <= xi(viewport.width));
}

#[test]
fn anchored_rect_shifts_up_near_the_bottom_edge() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let viewport = Rect::new(0, 0, 800, 600);
    let anchor = Point::new(30, 598);
    let rect = menu.anchored_rect(anchor, viewport, Scale::ONE, &theme);
    assert!(
        rect.origin.y < anchor.y,
        "an anchor near the bottom edge shifts the menu up off it"
    );
    assert!(rect.origin.y + xi(rect.height) <= xi(viewport.height));
}

#[test]
fn anchored_rect_shifts_both_ways_at_a_corner() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let viewport = Rect::new(0, 0, 800, 600);
    let corner = menu.anchored_rect(Point::new(798, 598), viewport, Scale::ONE, &theme);
    assert!(
        corner.origin.x >= 0 && corner.origin.y >= 0,
        "the menu never leaves the viewport origin"
    );
    assert!(corner.origin.x + xi(corner.width) <= xi(viewport.width));
    assert!(corner.origin.y + xi(corner.height) <= xi(viewport.height));
}

#[test]
fn anchored_rect_clamps_to_a_viewport_smaller_than_the_menu() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let tiny = Rect::new(0, 0, 10, 8);
    let rect = menu.anchored_rect(Point::new(3, 3), tiny, Scale::ONE, &theme);
    assert!(rect.width >= 1 && rect.width <= tiny.width);
    assert!(rect.height >= 1 && rect.height <= tiny.height);
    assert!(rect.origin.x >= 0 && rect.origin.y >= 0);
}

#[test]
fn anchored_rect_grows_with_a_larger_scale() {
    let theme = Theme::dark();
    let menu = three_item_menu();
    let viewport = Rect::new(0, 0, 1600, 1200);
    let anchor = Point::new(10, 10);
    let unscaled = menu.anchored_rect(anchor, viewport, Scale::ONE, &theme);
    let scale = Scale::from_percent(200).expect("valid scale");
    let doubled = menu.anchored_rect(anchor, viewport, scale, &theme);
    assert!(doubled.width > unscaled.width);
    assert!(doubled.height > unscaled.height);
}

// --- The theme owns the face -------------------------------------------

/// A menu measures and paints with the *theme's* interface face, so a theme
/// that authors larger interface text yields a wider plate and different
/// pixels for the same rows.
///
/// The regression: the menu used to measure with a face handed in by whoever
/// drew it and never consulted `theme.fonts()` at all, so both assertions
/// below held equal — its typography followed the drawing application rather
/// than the desktop. The graphical terminal handed it the monospace grid face
/// at the user's terminal text size, and the menu duly rendered in it.
#[test]
fn the_plate_measures_and_paints_with_the_themes_own_interface_face() {
    let small = text_ladder(tairix_theme::Fonts::MIN_BASE_SIZE_PX);
    let large = text_ladder(tairix_theme::Fonts::MAX_BASE_SIZE_PX);
    let menu = three_item_menu();

    let narrow = menu.preferred_width(Scale::ONE, &small);
    let wide = menu.preferred_width(Scale::ONE, &large);
    assert!(
        wide > narrow,
        "a larger interface face must widen the plate"
    );

    let viewport = Rect::new(0, 0, 1600, 1200);
    let anchor = Point::new(10, 10);
    assert!(
        menu.anchored_rect(anchor, viewport, Scale::ONE, &large)
            .width
            > menu
                .anchored_rect(anchor, viewport, Scale::ONE, &small)
                .width,
        "the placement rule must size from the same face"
    );

    // Measuring right while painting with something else would still be the
    // defect, so the drawn rows must differ too.
    let h = menu.preferred_height(Scale::ONE, &small);
    assert_ne!(
        render(&menu, &small, h).pixels(),
        render(&menu, &large, h).pixels(),
        "the rows must be drawn in the face the theme names"
    );
}

/// A row is never shorter than the line of text it draws, so a theme whose
/// type ladder rises above the standard control height gets taller rows rather
/// than labels cut off inside them.
///
/// The standard control height is a floor, not a fit: the shipped themes sit
/// under it and are unchanged, but a theme may author text up to
/// `Fonts::MAX_BASE_SIZE_PX` and once did so into a row pinned at 28 physical
/// pixels.
#[test]
fn a_row_grows_to_hold_text_taller_than_the_standard_control_height() {
    let menu = three_item_menu();
    let shipped = Theme::dark();
    let standard = Scale::ONE.scale_length(shipped.metrics().control_height);
    assert!(control_font(&shipped, Scale::ONE).line_height() < standard);
    assert_eq!(
        menu.preferred_height(Scale::ONE, &shipped),
        BORDER * 2 + 3 * standard,
        "a theme under the floor keeps the standard row"
    );

    let tall = text_ladder(tairix_theme::Fonts::MAX_BASE_SIZE_PX);
    let line = control_font(&tall, Scale::ONE).line_height();
    assert!(
        line > standard,
        "the fixture must clear the floor to test it"
    );
    assert_eq!(
        menu.preferred_height(Scale::ONE, &tall),
        BORDER * 2 + 3 * line,
        "a row must hold the line it draws"
    );
}

/// A keyboard move repaints the row the highlight leaves and the row it
/// arrives on, and nothing else in the popup.
#[test]
fn a_keyboard_move_reports_the_two_rows_it_moves_between() {
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    let down = Key::Named(NamedKey::Down);
    // The first Down lands the highlight on row 0, the second moves it.
    menu.on_key(down, bounds, Scale::ONE, &theme, &mut sink());
    let mut damage = sink();
    menu.on_key(down, bounds, Scale::ONE, &theme, &mut damage);

    let row = |i| menu.row_rect(i, bounds, Scale::ONE, &theme).expect("row");
    assert_eq!(
        damage.bounds(),
        row(0).union(&row(1)),
        "the report covers the row left and the row entered"
    );
    assert!(
        !damage.intersects(row(2)),
        "and no row the highlight never touched"
    );
}

/// Pointer motion inside one row changes no pixel, so it reports nothing —
/// this is the sample rate a host would otherwise repaint the popup at.
#[test]
fn motion_within_one_row_reports_nothing() {
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let h = menu.preferred_height(Scale::ONE, &theme);
    let bounds = Rect::new(0, 0, W, h);
    let first = menu.row_rect(0, bounds, Scale::ONE, &theme).expect("row 0");
    let inside = |dx| moved(first.left() + dx, first.top() + 1);
    menu.on_pointer(&inside(4), bounds, Scale::ONE, &theme, &mut sink());

    let mut damage = sink();
    menu.on_pointer(&inside(9), bounds, Scale::ONE, &theme, &mut damage);
    assert!(damage.is_empty(), "the same row stays the same row");
}

// --- What a host setter reports -----------------------------------------

#[test]
fn moving_the_highlight_reports_the_two_rows_it_moves_between() {
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    menu.adopt_current(Some(0));

    let mut damage = sink();
    menu.set_current(Some(2), bounds, Scale::ONE, &theme, &mut damage);

    assert_eq!(menu.current(), Some(2));
    let first = menu
        .row_rect(0, bounds, Scale::ONE, &theme)
        .expect("row 0 laid out");
    let last = menu
        .row_rect(2, bounds, Scale::ONE, &theme)
        .expect("row 2 laid out");
    assert!(damage.contains(Point::new(first.left(), first.top())));
    assert!(damage.contains(Point::new(last.left(), last.top())));

    let middle = menu
        .row_rect(1, bounds, Scale::ONE, &theme)
        .expect("row 1 laid out");
    assert!(
        !damage.contains(Point::new(middle.left(), middle.top())),
        "the row the highlight passed over is not redrawn: {:?}",
        damage.rects()
    );
}

#[test]
fn re_stating_the_highlight_reports_nothing() {
    let theme = Theme::dark();
    let mut menu = three_item_menu();
    let bounds = Rect::new(0, 0, W, menu.preferred_height(Scale::ONE, &theme));
    menu.adopt_current(Some(1));

    let mut damage = sink();
    menu.set_current(Some(1), bounds, Scale::ONE, &theme, &mut damage);
    assert!(damage.is_empty());
}

#[test]
fn adopting_a_highlight_reports_nothing_and_admits_what_setting_admits() {
    let theme = Theme::dark();
    let mut adopted = three_item_menu();
    let mut reported = three_item_menu();
    let bounds = Rect::new(0, 0, W, adopted.preferred_height(Scale::ONE, &theme));
    let mut damage = sink();

    for index in [Some(1), Some(9), None] {
        adopted.adopt_current(index);
        reported.set_current(index, bounds, Scale::ONE, &theme, &mut damage);
        assert_eq!(
            adopted.current(),
            reported.current(),
            "a rebuild must not admit a highlight the interactive path refuses"
        );
    }
}
