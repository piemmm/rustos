//! Unit tests for the tab strip (spec §11.12, §20 checklist).
//!
//! These cover measurement and equal-width layout, the selected lower seam,
//! the loading Heat Seam, the modified and error Signal Beads (so state reads
//! without colour), the keyboard model (Left/Right wrap, Home/End, Enter/Space
//! select), the pointer hover/click model, the fail-closed disabled tab, theme
//! switching, and scale.

use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::state::{ActivityState, ControlState, SelectionState, ValidationState};
use crate::tabs::{Tab, Tabs, TabsAction};

const W: u32 = 240;
const H: u32 = 28;
const EACH: u32 = W / 3;

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

fn render(tabs: &Tabs, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    tabs.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        font(),
    );
    surface
}

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

fn selected_state() -> ControlState {
    let mut s = ControlState::idle();
    s.selection = SelectionState::Selected;
    s
}

fn three_tabs() -> Tabs {
    Tabs::new(vec![Tab::new("One"), Tab::new("Two"), Tab::new("Tri")])
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

// --- Selection ----------------------------------------------------------

#[test]
fn set_selected_marks_one_tab_and_clears_the_rest() {
    let mut tabs = three_tabs();
    tabs.set_selected(1);
    assert_eq!(tabs.selected(), Some(1));
    assert!(tabs.tabs()[1].is_selected());
    assert!(!tabs.tabs()[0].is_selected());
}

#[test]
fn selected_tab_draws_a_strong_lower_accent_seam() {
    let theme = Theme::dark();
    let mut tabs = three_tabs();
    tabs.set_selected(1);
    let surface = render(&tabs, &theme);
    // The seam runs along the bottom edge of tab 1 only.
    assert!(region_has(
        &surface,
        (EACH + 4, 2 * EACH - 4),
        (H - 2, H),
        premul(theme.palette().accent),
    ));
    // Tab 0 (unselected, not loading) has no accent seam.
    assert!(!region_has(
        &surface,
        (4, EACH - 4),
        (H - 2, H),
        premul(theme.palette().accent),
    ));
}

#[test]
fn selected_and_unselected_plates_differ() {
    let theme = Theme::dark();
    let mut tabs = three_tabs();
    tabs.set_selected(1);
    let surface = render(&tabs, &theme);
    // Empty (label-free) area of each tab, mid-height.
    assert_eq!(
        surface.get(EACH + 5, H / 2),
        Some(premul(theme.palette().surface))
    );
    assert_eq!(
        surface.get(5, H / 2),
        Some(premul(theme.palette().surface_pressed))
    );
}

// --- Loading, modified, error signals ----------------------------------

#[test]
fn loading_tab_shows_a_lower_heat_seam() {
    let theme = Theme::dark();
    let tabs = Tabs::new(vec![
        Tab::new("Idle"),
        Tab::new("Busy").with_state(ControlState::idle().with_activity(ActivityState::Working)),
    ]);
    let mut surface = Surface::new(W, H).expect("surface");
    tabs.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        &theme,
        font(),
    );
    let each = W / 2;
    assert!(region_has(
        &surface,
        (each + 4, 2 * each - 4),
        (H - 2, H),
        premul(theme.palette().accent),
    ));
}

#[test]
fn modified_tab_shows_a_bead() {
    let theme = Theme::dark();
    let tabs = Tabs::new(vec![Tab::new("Doc").with_modified(true), Tab::new("Other")]);
    let mut surface = Surface::new(W, H).expect("surface");
    tabs.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        &theme,
        font(),
    );
    let each = W / 2;
    // The modified bead sits at the top-trailing corner of tab 0.
    assert!(region_has(
        &surface,
        (each - 12, each - 1),
        (1, 10),
        premul(theme.palette().accent),
    ));
}

#[test]
fn error_tab_shows_a_recovery_or_warning_bead() {
    let theme = Theme::dark();
    let invalid =
        Tabs::new(vec![Tab::new("Bad").with_state(
            ControlState::idle().with_validation(ValidationState::Invalid),
        )]);
    let warning =
        Tabs::new(vec![Tab::new("Warn").with_state(
            ControlState::idle().with_validation(ValidationState::Warning),
        )]);
    assert!(has_pixel(
        &render(&invalid, &theme),
        premul(theme.palette().recovery)
    ));
    assert!(has_pixel(
        &render(&warning, &theme),
        premul(theme.palette().warning)
    ));
}

// --- Keyboard -----------------------------------------------------------

#[test]
fn left_and_right_move_the_current_tab_and_wrap() {
    let mut tabs = three_tabs();
    tabs.on_key(Key::Named(NamedKey::Right));
    assert_eq!(tabs.current(), Some(0));
    tabs.on_key(Key::Named(NamedKey::Right));
    tabs.on_key(Key::Named(NamedKey::Right));
    assert_eq!(tabs.current(), Some(2));
    tabs.on_key(Key::Named(NamedKey::Right));
    assert_eq!(tabs.current(), Some(0));
    tabs.on_key(Key::Named(NamedKey::Left));
    assert_eq!(tabs.current(), Some(2));
}

#[test]
fn enter_selects_the_current_tab() {
    let mut tabs = three_tabs();
    tabs.set_current(Some(2));
    assert_eq!(
        tabs.on_key(Key::Named(NamedKey::Enter)),
        Some(TabsAction::Selected { index: 2 })
    );
    assert_eq!(
        tabs.on_key(Key::Char(' ')),
        Some(TabsAction::Selected { index: 2 })
    );
}

#[test]
fn home_and_end_jump_to_the_ends() {
    let mut tabs = three_tabs();
    tabs.set_current(Some(1));
    tabs.on_key(Key::Named(NamedKey::Home));
    assert_eq!(tabs.current(), Some(0));
    tabs.on_key(Key::Named(NamedKey::End));
    assert_eq!(tabs.current(), Some(2));
}

// --- Pointer ------------------------------------------------------------

#[test]
fn hover_focuses_a_tab_and_click_selects_it() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    let x = xi(EACH + EACH / 2);
    assert_eq!(tabs.on_pointer(&moved(x, 14), bounds), None);
    assert_eq!(tabs.current(), Some(1));
    assert_eq!(tabs.on_pointer(&PRESS, bounds), None);
    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds),
        Some(TabsAction::Selected { index: 1 })
    );
}

#[test]
fn release_outside_the_pressed_tab_does_not_select() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.on_pointer(&moved(xi(EACH / 2), 14), bounds);
    tabs.on_pointer(&PRESS, bounds);
    tabs.on_pointer(&moved(xi(2 * EACH + EACH / 2), 14), bounds);
    assert_eq!(tabs.on_pointer(&RELEASE, bounds), None);
}

#[test]
fn disabled_tab_never_selects() {
    let mut tabs = Tabs::new(vec![
        Tab::new("Ok"),
        Tab::new("No").with_state(ControlState::disabled()),
    ]);
    tabs.set_current(Some(1));
    assert_eq!(tabs.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn tab_at_maps_points_to_tabs() {
    let tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(tabs.tab_at(bounds, Point::new(5, 14)), Some(0));
    assert_eq!(
        tabs.tab_at(bounds, Point::new(xi(2 * EACH + 5), 14)),
        Some(2)
    );
    assert_eq!(tabs.tab_at(bounds, Point::new(5, 999)), None);
}

// --- Theme switching and scale -----------------------------------------

#[test]
fn theme_switch_changes_the_plate() {
    let mut tabs = three_tabs();
    tabs.set_selected(0);
    let dark = render(&tabs, &Theme::dark());
    let light = render(&tabs, &Theme::light());
    assert_ne!(dark.get(5, H / 2), light.get(5, H / 2));
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let mut tabs = three_tabs();
    tabs.set_selected(0);
    let mut surface = Surface::new(W, H * 2).expect("surface");
    tabs.render(
        &mut surface,
        Rect::new(0, 0, W, H * 2),
        scale,
        &theme,
        font(),
    );
    assert!(has_pixel(&surface, premul(theme.palette().accent)));
}

#[test]
fn selected_helper_builds_a_selected_state() {
    // A tab built with a selected state reports selected without set_selected.
    let tabs = Tabs::new(vec![Tab::new("A").with_state(selected_state())]);
    assert_eq!(tabs.selected(), Some(0));
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_tab_strip() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);

    // Two samples clear of the strip, so only the recorded coordinate differs.
    let mut a = three_tabs();
    let mut b = a.clone();
    a.on_pointer(&moved(xi(W) + 40, xi(H) + 40), bounds);
    b.on_pointer(&moved(xi(W) + 90, xi(H) + 12), bounds);
    assert_eq!(
        a, b,
        "a coordinate clear of the strip is not a drawn property"
    );
    assert_eq!(
        render(&a, &theme).pixels(),
        render(&b, &theme).pixels(),
        "…and the two must therefore paint identically"
    );

    // Both hover the already-selected tab, so the selection and the hover
    // match; only one holds the press, and which tab a release would choose
    // is not drawn.
    let over_selected = moved(xi(EACH) / 2, xi(H) / 2);
    let mut latched = three_tabs();
    latched.on_pointer(&over_selected, bounds);
    latched.on_pointer(&PRESS, bounds);
    let mut hovered = three_tabs();
    hovered.on_pointer(&over_selected, bounds);
    assert_eq!(latched, hovered, "the armed tab is not a drawn property");
    assert_eq!(
        render(&latched, &theme).pixels(),
        render(&hovered, &theme).pixels(),
        "…and the two must therefore paint identically"
    );
}
