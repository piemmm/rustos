//! Unit tests for the tab strip (spec §11.12, §20 checklist).
//!
//! These cover measurement and equal-width layout, the selected lower seam,
//! the loading Heat Seam, the modified and error Signal Beads (so state reads
//! without colour), the keyboard model (Left/Right wrap, Home/End, Enter/Space
//! select), the pointer hover/click model, the fail-closed disabled tab, theme
//! switching, and scale.
//!
//! The orientation cases cover the vertical (sidebar) strip over the same one
//! definition: its column layout and full-width tabs, the leading selection
//! and loading seams, its beads, the per-axis arrow keys (a column must not
//! answer to a row's arrows), Home/End, pointer selection agreeing with what
//! was drawn, the extent each orientation measures, overflow omitting whole
//! tabs, degenerate bounds, and the heavier-contrast path.

use alloc::string::String;
use alloc::vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::chart::Chart;
use crate::damage::sink;
use crate::state::{ActivityState, ControlState, PressureKind, SelectionState, ValidationState};
use crate::tabs::{Tab, Tabs, TabsAction, TabsOrientation};
use crate::testkit::{control_font, high_contrast};

const W: u32 = 240;
const H: u32 = 28;
const EACH: u32 = W / 3;

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
    tabs.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, theme);
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
    tabs.adopt_selected(1);
    assert_eq!(tabs.selected(), Some(1));
    assert!(tabs.tabs()[1].is_selected());
    assert!(!tabs.tabs()[0].is_selected());
}

#[test]
fn selected_tab_draws_a_strong_lower_accent_seam() {
    let theme = Theme::dark();
    let mut tabs = three_tabs();
    tabs.adopt_selected(1);
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
    tabs.adopt_selected(1);
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
    tabs.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, &theme);
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
    tabs.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, &theme);
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
    let bounds = Rect::new(0, 0, W, H);
    tabs.on_key(
        Key::Named(NamedKey::Right),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(0));
    tabs.on_key(
        Key::Named(NamedKey::Right),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    tabs.on_key(
        Key::Named(NamedKey::Right),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(2));
    tabs.on_key(
        Key::Named(NamedKey::Right),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(0));
    tabs.on_key(
        Key::Named(NamedKey::Left),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(2));
}

#[test]
fn enter_selects_the_current_tab() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.adopt_current(Some(2));
    assert_eq!(
        tabs.on_key(
            Key::Named(NamedKey::Enter),
            bounds,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        Some(TabsAction::Selected { index: 2 })
    );
    assert_eq!(
        tabs.on_key(
            Key::Char(' '),
            bounds,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        Some(TabsAction::Selected { index: 2 })
    );
}

#[test]
fn home_and_end_jump_to_the_ends() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.adopt_current(Some(1));
    tabs.on_key(
        Key::Named(NamedKey::Home),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(0));
    tabs.on_key(
        Key::Named(NamedKey::End),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(2));
}

// --- Pointer ------------------------------------------------------------

#[test]
fn hover_lifts_a_tab_and_click_selects_it() {
    let theme = Theme::dark();
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    let x = xi(EACH + EACH / 2);
    assert_eq!(
        tabs.on_pointer(
            &moved(x, 14),
            bounds,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
    assert_eq!(
        render(&tabs, &theme).get(EACH + 2, 2),
        Some(premul(theme.palette().surface_raised)),
        "the tab under the pointer lifts"
    );
    assert_eq!(
        tabs.current(),
        None,
        "and the keyboard cursor stays where the keyboard left it"
    );
    assert_eq!(
        tabs.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        None
    );
    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        Some(TabsAction::Selected { index: 1 })
    );
}

#[test]
fn release_outside_the_pressed_tab_does_not_select() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.on_pointer(
        &moved(xi(EACH / 2), 14),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    tabs.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink());
    tabs.on_pointer(
        &moved(xi(2 * EACH + EACH / 2), 14),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        None
    );
}

#[test]
fn re_stating_the_keyboard_cursor_leaves_the_hover_where_it_is() {
    let theme = Theme::dark();
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.on_pointer(
        &moved(xi(EACH + EACH / 2), 14),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    let hovered = render(&tabs, &theme);
    assert_eq!(
        hovered.get(EACH + 2, 2),
        Some(premul(theme.palette().surface_raised))
    );

    // What a host does on every model refresh, however often that is.
    tabs.adopt_current(None);

    assert_eq!(
        render(&tabs, &theme).pixels(),
        hovered.pixels(),
        "stating where the keyboard is says nothing about where the pointer is"
    );
}

#[test]
fn only_the_keyboard_cursor_wears_the_focus_ring() {
    let theme = Theme::dark();
    let ring = premul(theme.palette().rim_active);
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.on_pointer(
        &moved(xi(EACH + EACH / 2), 14),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert!(
        !has_pixel(&render(&tabs, &theme), ring),
        "a hover lifts a tab without claiming the keyboard"
    );

    tabs.adopt_current(Some(1));

    assert!(has_pixel(&render(&tabs, &theme), ring));
}

#[test]
fn the_pointer_and_the_keyboard_cursor_light_their_own_tabs() {
    let theme = Theme::dark();
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.adopt_current(Some(0));
    tabs.on_pointer(
        &moved(xi(2 * EACH + EACH / 2), 14),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );

    let surface = render(&tabs, &theme);
    let raised = premul(theme.palette().surface_raised);
    assert_eq!(surface.get(2, 2), Some(raised), "the keyboard's tab lifts");
    assert_eq!(
        surface.get(2 * EACH + 2, 2),
        Some(raised),
        "so does the pointer's"
    );
    assert_eq!(
        surface.get(EACH + 2, 2),
        Some(premul(theme.palette().surface_pressed)),
        "and the tab neither is on stays quiet"
    );
    assert_eq!(tabs.current(), Some(0));
}

#[test]
fn re_labelling_keeps_the_selection_and_a_click_in_flight() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.adopt_selected(2);
    tabs.on_pointer(
        &moved(xi(EACH + EACH / 2), 14),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(
        tabs.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        None
    );

    for (tab, label) in tabs.tabs_mut().iter_mut().zip(["One 4", "Two 9", "Tri 0"]) {
        tab.set_label(label);
    }

    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        Some(TabsAction::Selected { index: 1 }),
        "a live reading changing mid-click cannot swallow it"
    );
    assert_eq!(tabs.selected(), Some(2), "nor commit the selection early");
    assert_eq!(tabs.tabs()[1].label(), "Two 9");
}

#[test]
fn disabled_tab_never_selects() {
    let mut tabs = Tabs::new(vec![
        Tab::new("Ok"),
        Tab::new("No").with_state(ControlState::disabled()),
    ]);
    tabs.adopt_current(Some(1));
    assert_eq!(
        tabs.on_key(
            Key::Named(NamedKey::Enter),
            Rect::new(0, 0, W, H),
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
}

#[test]
fn tab_at_maps_points_to_tabs() {
    let tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(
        tabs.tab_at(bounds, Scale::ONE, &Theme::dark(), Point::new(5, 14)),
        Some(0)
    );
    assert_eq!(
        tabs.tab_at(
            bounds,
            Scale::ONE,
            &Theme::dark(),
            Point::new(xi(2 * EACH + 5), 14)
        ),
        Some(2)
    );
    assert_eq!(
        tabs.tab_at(bounds, Scale::ONE, &Theme::dark(), Point::new(5, 999)),
        None
    );
}

// --- Theme switching and scale -----------------------------------------

#[test]
fn theme_switch_changes_the_plate() {
    let mut tabs = three_tabs();
    tabs.adopt_selected(0);
    let dark = render(&tabs, &Theme::dark());
    let light = render(&tabs, &Theme::light());
    assert_ne!(dark.get(5, H / 2), light.get(5, H / 2));
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let mut tabs = three_tabs();
    tabs.adopt_selected(0);
    let mut surface = Surface::new(W, H * 2).expect("surface");
    tabs.render(&mut surface, Rect::new(0, 0, W, H * 2), scale, &theme);
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
    a.on_pointer(
        &moved(xi(W) + 40, xi(H) + 40),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    b.on_pointer(
        &moved(xi(W) + 90, xi(H) + 12),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(
        a, b,
        "a coordinate clear of the strip is not a drawn property"
    );
    assert_eq!(
        render(&a, &theme).pixels(),
        render(&b, &theme).pixels(),
        "…and the two must therefore paint identically"
    );

    // Both rest the pointer on the same tab, so only the press latch differs,
    // and which tab a release would choose is not drawn.
    let over_selected = moved(xi(EACH) / 2, xi(H) / 2);
    let mut latched = three_tabs();
    latched.on_pointer(
        &over_selected,
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    latched.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink());
    let mut hovered = three_tabs();
    hovered.on_pointer(
        &over_selected,
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(latched, hovered, "the armed tab is not a drawn property");
    assert_eq!(
        render(&latched, &theme).pixels(),
        render(&hovered, &theme).pixels(),
        "…and the two must therefore paint identically"
    );
}

// --- Orientation: the vertical (sidebar) strip (spec §11.12) --------------

/// A tall, narrow strip: three entries stacked down `VW` pixels of width, so
/// every entry spans the full width and its labels align. `VH` is generous —
/// a vertical strip stacks rather than splits, so the entries claim only
/// [`veach`] each and the rest of the column stays the owner's.
const VW: u32 = 96;
const VH: u32 = 240;

/// The height one plain vertical entry claims: the shared one-line plate
/// height a strip stacks its entries at.
fn veach() -> u32 {
    crate::paint::text_plate_height(&Theme::dark(), Scale::ONE, tairix_theme::TextRole::Body)
}

fn vertical_three() -> Tabs {
    three_tabs().with_orientation(TabsOrientation::Vertical)
}

/// Render `tabs` into its own `w`×`h` surface at `scale`.
fn render_in(tabs: &Tabs, theme: &Theme, scale: Scale, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    tabs.render(&mut surface, Rect::new(0, 0, w, h), scale, theme);
    surface
}

/// Whether nothing at all was painted — every pixel is still an untouched
/// surface's.
fn is_blank(surface: &Surface) -> bool {
    let blank = Surface::new(surface.width(), surface.height()).expect("blank surface");
    surface.pixels() == blank.pixels()
}

/// Whether nothing was painted outside the `w`×`h` rectangle the strip was
/// given, so a control can never paint past its own bounds.
fn untouched_outside(surface: &Surface, w: u32, h: u32) -> bool {
    let blank = Surface::new(surface.width(), surface.height()).expect("blank surface");
    (0..surface.height()).all(|y| {
        (0..surface.width()).all(|x| (x < w && y < h) || surface.get(x, y) == blank.get(x, y))
    })
}

#[test]
fn a_new_strip_is_horizontal() {
    let tabs = three_tabs();
    assert_eq!(tabs.orientation(), TabsOrientation::Horizontal);
    assert_eq!(
        tabs.with_orientation(TabsOrientation::Vertical)
            .orientation(),
        TabsOrientation::Vertical
    );
}

#[test]
fn orientation_is_a_drawn_property() {
    let theme = Theme::dark();
    let horizontal = three_tabs();
    let vertical = vertical_three();
    assert_ne!(
        horizontal, vertical,
        "the two lay their tabs out differently, so a repaint gate must tell them apart"
    );
    assert_ne!(
        render_in(&horizontal, &theme, Scale::ONE, VW, VH).pixels(),
        render_in(&vertical, &theme, Scale::ONE, VW, VH).pixels(),
    );
}

#[test]
fn vertical_tabs_stack_down_the_side_and_span_the_full_width() {
    let tabs = vertical_three();
    let bounds = Rect::new(0, 0, VW, VH);
    for (index, y) in [
        (0_usize, veach() / 2),
        (1, veach() + veach() / 2),
        (2, 2 * veach() + veach() / 2),
    ] {
        assert_eq!(
            tabs.tab_at(bounds, Scale::ONE, &Theme::dark(), Point::new(0, xi(y))),
            Some(index)
        );
        assert_eq!(
            tabs.tab_at(
                bounds,
                Scale::ONE,
                &Theme::dark(),
                Point::new(xi(VW - 1), xi(y))
            ),
            Some(index),
            "a vertical tab spans the whole width so its label aligns with the others"
        );
    }
    assert_eq!(
        tabs.tab_at(
            bounds,
            Scale::ONE,
            &Theme::dark(),
            Point::new(xi(VW), xi(veach() / 2))
        ),
        None
    );
    assert_eq!(
        tabs.tab_at(bounds, Scale::ONE, &Theme::dark(), Point::new(0, xi(VH))),
        None
    );
}

#[test]
fn vertical_selection_seam_runs_down_the_leading_edge() {
    let theme = Theme::dark();
    let accent = premul(theme.palette().accent);
    let mut tabs = vertical_three();
    tabs.adopt_selected(1);
    let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
    // The seam hugs the leading edge of tab 1, running along its own height.
    assert!(region_has(
        &surface,
        (0, 1),
        (veach() + 4, 2 * veach() - 4),
        accent
    ));
    // A column carries no lower seam: that is the horizontal strip's edge.
    assert!(!region_has(
        &surface,
        (4, VW),
        (2 * veach() - 2, 2 * veach()),
        accent
    ));
    // Nor does it carry a trailing one.
    assert!(!region_has(
        &surface,
        (VW - 1, VW),
        (veach() + 4, 2 * veach() - 4),
        accent
    ));
    // Tab 0 is neither selected nor loading, so it carries no seam at all.
    assert!(!region_has(&surface, (0, VW), (4, veach() - 4), accent));
}

#[test]
fn vertical_loading_tab_shows_a_leading_heat_seam() {
    let theme = Theme::dark();
    let accent = premul(theme.palette().accent);
    let tabs = Tabs::new(vec![
        Tab::new("Idle"),
        Tab::new("Busy").with_state(ControlState::idle().with_activity(ActivityState::Working)),
    ])
    .with_orientation(TabsOrientation::Vertical);
    let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
    let each = veach();
    assert!(region_has(
        &surface,
        (0, 1),
        (each + 2, 2 * each - 2),
        accent
    ));
    assert!(!region_has(&surface, (0, 1), (2, each - 2), accent));
}

#[test]
fn a_vertical_tab_still_shows_its_modified_and_error_beads() {
    let theme = Theme::dark();
    let tabs = Tabs::new(vec![
        Tab::new("Doc").with_modified(true),
        Tab::new("Bad").with_state(ControlState::idle().with_validation(ValidationState::Invalid)),
    ])
    .with_orientation(TabsOrientation::Vertical);
    let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
    let each = veach();
    // Each bead sits at the top-trailing corner of its own full-width band.
    assert!(region_has(
        &surface,
        (VW - 12, VW),
        (0, 12),
        premul(theme.palette().accent)
    ));
    assert!(region_has(
        &surface,
        (VW - 12, VW),
        (each, each + 12),
        premul(theme.palette().recovery)
    ));
}

#[test]
fn a_tab_too_narrow_for_its_bead_omits_it_rather_than_reaching_past_its_edge() {
    let theme = Theme::dark();
    // Two pixels per tab: the bead cannot sit inside its own tab past the
    // plate border, so it is dropped rather than stamped over the neighbour —
    // and its placement must not run backwards off the leading edge.
    let crowded = 120_usize;
    let tabs = Tabs::new(vec![Tab::new("x").with_modified(true); crowded]);
    let surface = render_in(&tabs, &theme, Scale::ONE, W, H);
    assert_eq!(
        W / u32::try_from(crowded).expect("count fits"),
        2,
        "the fixture must leave each tab two pixels of width"
    );
    assert!(
        !has_pixel(&surface, premul(theme.palette().accent)),
        "a bead that cannot fit is omitted, never drawn over the next tab"
    );
    assert!(!is_blank(&surface), "the tabs' own plates still draw");
}

#[test]
fn only_the_strip_own_axis_arrows_move_the_current_tab() {
    let across = Rect::new(0, 0, W, H);
    let down = Rect::new(0, 0, VW, VH);
    let mut horizontal = three_tabs();
    horizontal.adopt_current(Some(1));
    assert_eq!(
        horizontal.on_key(
            Key::Named(NamedKey::Down),
            across,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
    assert_eq!(
        horizontal.current(),
        Some(1),
        "a row must not answer to a column's arrows"
    );
    assert_eq!(
        horizontal.on_key(
            Key::Named(NamedKey::Up),
            across,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
    assert_eq!(horizontal.current(), Some(1));
    horizontal.on_key(
        Key::Named(NamedKey::Right),
        across,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(horizontal.current(), Some(2));

    let mut vertical = vertical_three();
    vertical.adopt_current(Some(1));
    assert_eq!(
        vertical.on_key(
            Key::Named(NamedKey::Left),
            down,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
    assert_eq!(
        vertical.current(),
        Some(1),
        "a column must not answer to a row's arrows"
    );
    assert_eq!(
        vertical.on_key(
            Key::Named(NamedKey::Right),
            down,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
    assert_eq!(vertical.current(), Some(1));
    vertical.on_key(
        Key::Named(NamedKey::Down),
        down,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(vertical.current(), Some(2));
    vertical.on_key(
        Key::Named(NamedKey::Down),
        down,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(vertical.current(), Some(0), "…and wraps along its own axis");
    vertical.on_key(
        Key::Named(NamedKey::Up),
        down,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(vertical.current(), Some(2));
}

#[test]
fn home_and_end_jump_to_the_ends_of_a_vertical_strip_too() {
    let mut tabs = vertical_three();
    let bounds = Rect::new(0, 0, VW, VH);
    tabs.adopt_current(Some(1));
    tabs.on_key(
        Key::Named(NamedKey::End),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(2));
    tabs.on_key(
        Key::Named(NamedKey::Home),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(tabs.current(), Some(0));
    assert_eq!(
        tabs.on_key(
            Key::Named(NamedKey::Enter),
            bounds,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        Some(TabsAction::Selected { index: 0 })
    );
}

#[test]
fn a_vertical_press_selects_the_tab_that_was_drawn() {
    let theme = Theme::dark();
    let mut tabs = vertical_three();
    let bounds = Rect::new(0, 0, VW, VH);
    let over_band_one = moved(xi(VW / 2), xi(veach() + veach() / 2));
    assert_eq!(
        tabs.on_pointer(
            &over_band_one,
            bounds,
            Scale::ONE,
            &Theme::dark(),
            &mut sink()
        ),
        None
    );
    assert_eq!(
        render_in(&tabs, &theme, Scale::ONE, VW, VH).get(VW - 2, veach() + 4),
        Some(premul(theme.palette().surface_raised)),
        "the band under the pointer lifts"
    );
    assert_eq!(
        tabs.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        None
    );
    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        Some(TabsAction::Selected { index: 1 })
    );
    // The owner commits the selection, and the band the press chose is the one
    // that draws as selected.
    tabs.adopt_selected(1);
    let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
    assert_eq!(
        surface.get(VW - 2, veach() + 4),
        Some(premul(theme.palette().surface))
    );
    assert_eq!(
        surface.get(VW - 2, 4),
        Some(premul(theme.palette().surface_pressed))
    );
}

#[test]
fn a_release_off_the_pressed_band_does_not_select_it() {
    let mut tabs = vertical_three();
    let bounds = Rect::new(0, 0, VW, VH);
    tabs.on_pointer(
        &moved(xi(VW / 2), xi(veach() / 2)),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    tabs.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink());
    tabs.on_pointer(
        &moved(xi(VW / 2), xi(2 * veach() + veach() / 2)),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        None
    );
}

#[test]
fn a_horizontal_axis_too_short_for_every_tab_omits_them_all() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let crowded = Tabs::new(vec![Tab::new("x"); 300]);
    let surface = render_in(&crowded, &theme, Scale::ONE, W, H);
    assert!(
        is_blank(&surface),
        "a tab that cannot have a whole band of its own is omitted, never drawn partially"
    );
    // Hit-testing agrees: nothing was drawn, so nothing can be pressed.
    assert!((0..W).all(|x| crowded
        .tab_at(
            bounds,
            Scale::ONE,
            &Theme::dark(),
            Point::new(xi(x), xi(H / 2))
        )
        .is_none()));
}

#[test]
fn a_vertical_strip_seats_the_entries_it_can_and_leaves_the_rest_to_its_owner() {
    // A discovered list — a hundred cores, a dozen volumes — must scroll, not
    // truncate and not squeeze: the strip stacks whole entries at their own
    // height and states the height it wants, which is what an owner scrolls.
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, VW, VH);
    let long = Tabs::new(vec![Tab::new("x"); 300]).with_orientation(TabsOrientation::Vertical);
    let each = veach();
    let seated = usize::try_from(VH / each).expect("count fits");

    assert!(
        long.measured_height(Scale::ONE, &theme) > VH,
        "the strip must state the height its whole list wants"
    );
    for index in 0..seated {
        let step = u32::try_from(index).expect("index fits");
        let y = step * each + each / 2;
        assert_eq!(
            long.tab_at(
                bounds,
                Scale::ONE,
                &Theme::dark(),
                Point::new(xi(VW / 2), xi(y))
            ),
            Some(index),
            "entry {index} must be seated at its own stacked height"
        );
    }
    // An entry past the column is not drawn and cannot be pressed.
    assert_eq!(long.tab_area(seated + 1, bounds, Scale::ONE, &theme), None);

    // A list the column can hold seats every entry and nothing beyond it.
    let short = Tabs::new(vec![Tab::new("x"); 3]).with_orientation(TabsOrientation::Vertical);
    assert_eq!(short.measured_height(Scale::ONE, &theme), each * 3);
    let surface = render_in(&short, &theme, Scale::ONE, VW, VH);
    assert!(
        (each * 3..VH).all(|y| (0..VW).all(|x| surface.get(x, y) == Some(Pixel::TRANSPARENT))),
        "a vertical strip stacks its entries; it does not stretch them to fill the column"
    );
}

#[test]
fn measured_extent_covers_both_orientations() {
    let theme = Theme::dark();
    let horizontal = three_tabs();
    let vertical = vertical_three();
    let across = horizontal.measured_extent(Scale::ONE, &theme);
    let down = vertical.measured_extent(Scale::ONE, &theme);
    assert!(across >= Scale::ONE.scale_length(theme.metrics().control_height));
    assert!(
        down > across,
        "a column's fixed width also reserves the bead's footprint beside its labels"
    );
    let dense = Scale::from_percent(200).expect("valid scale");
    assert!(horizontal.measured_extent(dense, &theme) > across);
    assert!(vertical.measured_extent(dense, &theme) > down);
}

#[test]
fn degenerate_bounds_paint_nothing_in_either_orientation() {
    let theme = Theme::dark();
    for tabs in [three_tabs(), vertical_three()] {
        for (w, h) in [(0, VH), (VW, 0), (0, 0)] {
            let bounds = Rect::new(0, 0, w, h);
            let mut surface = Surface::new(VW, VH).expect("surface");
            tabs.render(&mut surface, bounds, Scale::ONE, &theme);
            assert!(is_blank(&surface));
            assert_eq!(
                tabs.tab_at(bounds, Scale::ONE, &Theme::dark(), Point::new(0, 0)),
                None
            );
        }
        // An off-surface origin is refused rather than wrapped into the surface.
        let off = Rect::new(-4, -4, VW, VH);
        let mut surface = Surface::new(VW, VH).expect("surface");
        tabs.render(&mut surface, off, Scale::ONE, &theme);
        assert!(is_blank(&surface));
        assert_eq!(
            tabs.tab_at(off, Scale::ONE, &Theme::dark(), Point::new(0, 0)),
            None
        );
        // A bounds smaller than the surface leaves the rest of it untouched.
        let mut surface = Surface::new(VW, VH).expect("surface");
        tabs.render(
            &mut surface,
            Rect::new(0, 0, VW / 2, VH / 2),
            Scale::ONE,
            &theme,
        );
        assert!(untouched_outside(&surface, VW / 2, VH / 2));
    }
}

#[test]
fn a_vertical_strip_reads_in_both_themes_and_under_heavy_contrast() {
    let accent = premul(Theme::dark().palette().accent);
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut tabs = vertical_three();
        tabs.adopt_selected(1);
        let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
        assert!(
            region_has(
                &surface,
                (0, 1),
                (veach() + 4, 2 * veach() - 4),
                premul(theme.palette().accent)
            ),
            "the leading seam must read in every theme"
        );
        assert!(
            has_pixel(&surface, premul(theme.palette().on_surface)),
            "an unselected label must read in every theme"
        );
    }

    // Heavier contrast strengthens the seam itself rather than leaning on hue:
    // the high-contrast fixture shares the dark palette, so only the treatment
    // can differ.
    let seam_run = |theme: &Theme| {
        let mut tabs = vertical_three();
        tabs.adopt_selected(1);
        let surface = render_in(&tabs, theme, Scale::ONE, VW, VH);
        (0..VW)
            .take_while(|&x| surface.get(x, veach() + 4) == Some(accent))
            .count()
    };
    assert!(seam_run(&high_contrast()) > seam_run(&Theme::dark()));
}

/// A hover crossing from one tab to the next repaints those two tabs, not the
/// strip they sit in.
#[test]
fn a_hover_crossing_tabs_reports_the_two_tabs() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    let centre = |i: u32| moved(i32::try_from(i * EACH + EACH / 2).expect("fits"), 4);
    tabs.on_pointer(&centre(0), bounds, Scale::ONE, &Theme::dark(), &mut sink());

    let mut damage = sink();
    tabs.on_pointer(&centre(1), bounds, Scale::ONE, &Theme::dark(), &mut damage);
    assert!(
        damage.bounds().width < W,
        "two tabs, not the whole strip: {:?}",
        damage.bounds()
    );
    assert!(
        damage.contains(Point::new(i32::try_from(EACH / 2).expect("fits"), 4))
            && damage.contains(Point::new(i32::try_from(EACH + EACH / 2).expect("fits"), 4)),
        "both the tab left and the tab entered"
    );
    assert!(
        !damage.contains(Point::new(
            i32::try_from(2 * EACH + EACH / 2).expect("fits"),
            4
        )),
        "and never a tab the pointer did not visit"
    );
}

/// Motion inside one tab reports nothing: the lift it draws has not moved.
#[test]
fn motion_within_one_tab_reports_nothing() {
    let mut tabs = three_tabs();
    let bounds = Rect::new(0, 0, W, H);
    tabs.on_pointer(
        &moved(4, 4),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );

    let mut damage = sink();
    tabs.on_pointer(
        &moved(9, 6),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut damage,
    );
    assert!(damage.is_empty(), "the same tab stays lifted");
}

// --- What a host setter reports -----------------------------------------

#[test]
fn moving_the_selection_reports_the_two_plates_that_change() {
    let bounds = Rect::new(0, 0, W, H);
    let mut tabs = three_tabs();
    tabs.adopt_selected(0);

    let mut damage = sink();
    tabs.set_selected(1, bounds, Scale::ONE, &Theme::dark(), &mut damage);

    assert_eq!(tabs.selected(), Some(1));
    // The two plates are adjacent, so the canonical region holds them as one
    // rectangle covering exactly those two cells — never the whole strip.
    assert_eq!(damage.rects(), &[Rect::new(0, 0, EACH * 2, H)]);
    assert!(
        !damage.contains(Point::new(xi(EACH * 2 + EACH / 2), xi(H / 2))),
        "the untouched third tab is not reported"
    );
}

#[test]
fn re_stating_the_selection_reports_nothing() {
    let bounds = Rect::new(0, 0, W, H);
    let mut tabs = three_tabs();
    tabs.adopt_selected(1);

    let mut damage = sink();
    tabs.set_selected(1, bounds, Scale::ONE, &Theme::dark(), &mut damage);

    assert!(
        damage.is_empty(),
        "a host may re-state its selection as often as its model refreshes"
    );
}

#[test]
fn moving_the_keyboard_cursor_reports_the_two_tabs_it_moves_between() {
    let bounds = Rect::new(0, 0, W, H);
    let mut tabs = three_tabs();
    tabs.adopt_current(Some(0));

    let mut damage = sink();
    tabs.set_current(Some(2), bounds, Scale::ONE, &Theme::dark(), &mut damage);

    assert_eq!(tabs.current(), Some(2));
    assert_eq!(
        damage.rects(),
        &[
            Rect::new(0, 0, EACH, H),
            Rect::new(xi(EACH * 2), 0, EACH, H)
        ]
    );

    // Taking the cursor off the strip costs only the tab it was on.
    let mut off = sink();
    tabs.set_current(None, bounds, Scale::ONE, &Theme::dark(), &mut off);
    assert_eq!(tabs.current(), None);
    assert_eq!(off.rects(), &[Rect::new(xi(EACH * 2), 0, EACH, H)]);
}

#[test]
fn an_out_of_range_cursor_reports_only_the_tab_it_clears() {
    let bounds = Rect::new(0, 0, W, H);
    let mut tabs = three_tabs();
    tabs.adopt_current(Some(1));

    let mut damage = sink();
    tabs.set_current(Some(9), bounds, Scale::ONE, &Theme::dark(), &mut damage);

    assert_eq!(tabs.current(), None, "an out-of-range index takes it off");
    assert_eq!(damage.rects(), &[Rect::new(xi(EACH), 0, EACH, H)]);
}

#[test]
fn adopting_reports_nothing_and_admits_what_setting_admits() {
    let bounds = Rect::new(0, 0, W, H);
    let mut adopted = three_tabs();
    let mut reported = three_tabs();
    let mut damage = sink();

    for index in [Some(1), Some(9), None] {
        adopted.adopt_current(index);
        reported.set_current(index, bounds, Scale::ONE, &Theme::dark(), &mut damage);
        assert_eq!(
            adopted.current(),
            reported.current(),
            "a rebuild must not admit a cursor the interactive path refuses"
        );
    }

    let mut selection = sink();
    adopted.adopt_selected(2);
    assert!(selection.is_empty());
    assert_eq!(adopted.selected(), Some(2));
    // The reporting sibling agrees on the state, differing only in the report.
    reported.set_selected(2, bounds, Scale::ONE, &Theme::dark(), &mut selection);
    assert_eq!(reported.selected(), Some(2));
    assert!(!selection.is_empty());
}

// --- The sidebar list's own anatomy: groups, readings, trends -------------

/// A device-rail-shaped strip: two groups, each entry carrying a reading, and
/// only the first group's entries carrying a trend.
fn device_rail() -> Tabs {
    Tabs::new(vec![
        Tab::new("CPU")
            .with_group("Resources")
            .with_reading("18%")
            .with_trend(Chart::new(PressureKind::Cpu).with_samples([200, 600, 400])),
        Tab::new("Memory")
            .with_reading("53%")
            .with_trend(Chart::new(PressureKind::Memory).with_samples([500, 520, 530])),
        Tab::new("Identity & uptime")
            .with_group("Machine")
            .with_reading("2h 12m"),
    ])
    .with_orientation(TabsOrientation::Vertical)
}

fn heading_band(theme: &Theme) -> u32 {
    let font = control_font(theme, Scale::ONE);
    let gap = Scale::ONE.scale_length(theme.metrics().control_gap).max(1);
    font.line_height() + gap * 2
}

fn trend_band(theme: &Theme) -> u32 {
    Scale::ONE.scale_length(theme.metrics().chart_height).max(1)
}

#[test]
fn builders_and_readers_agree_on_the_sidebar_anatomy() {
    let rail = device_rail();
    let cpu = &rail.tabs()[0];
    assert_eq!(cpu.group(), Some("Resources"));
    assert_eq!(cpu.reading(), Some("18%"));
    assert!(cpu.trend().is_some());
    assert_eq!(rail.tabs()[1].group(), None);
    assert_eq!(rail.tabs()[2].trend(), None);
}

#[test]
fn a_group_heading_claims_its_own_band_above_the_entry_that_starts_the_group() {
    // A heading is not an entry: it selects nothing, and the entries below it
    // shift down by exactly the band it claims.
    let theme = Theme::dark();
    let rail = device_rail();
    let bounds = Rect::new(0, 0, VW, VH);
    let heading = heading_band(&theme);
    let entry = veach();
    let with_trend = entry + trend_band(&theme);

    let cpu = rail
        .tab_area(0, bounds, Scale::ONE, &theme)
        .expect("the first entry is seated");
    assert_eq!(cpu.top(), xi(heading), "the heading precedes its own group");
    assert_eq!(cpu.height, with_trend);

    let memory = rail
        .tab_area(1, bounds, Scale::ONE, &theme)
        .expect("the second entry is seated");
    assert_eq!(memory.top(), cpu.top() + xi(with_trend));

    // The Machine group's heading pushes its entry down again, and that entry
    // carries no trend, so it is shorter.
    let machine = rail
        .tab_area(2, bounds, Scale::ONE, &theme)
        .expect("the third entry is seated");
    assert_eq!(machine.top(), memory.top() + xi(with_trend) + xi(heading));
    assert_eq!(
        machine.height, entry,
        "an entry with no rate behind it claims no room for a trace"
    );

    // A press on either heading selects nothing.
    for y in [
        heading / 2,
        u32::try_from(memory.bottom()).expect("fits") + 1,
    ] {
        assert_eq!(
            rail.tab_at(bounds, Scale::ONE, &theme, Point::new(xi(VW / 2), xi(y))),
            None,
            "a heading is not selectable"
        );
    }
}

#[test]
fn measured_height_counts_every_heading_and_every_entry() {
    let theme = Theme::dark();
    let rail = device_rail();
    let wanted = heading_band(&theme) * 2 + (veach() + trend_band(&theme)) * 2 + veach();
    assert_eq!(rail.measured_height(Scale::ONE, &theme), wanted);
}

#[test]
fn a_heading_never_draws_without_at_least_its_own_first_entry() {
    // Room for the heading but not the entry beneath it would leave a group
    // label introducing nothing.
    let theme = Theme::dark();
    let heading = heading_band(&theme);
    let short = heading + veach() / 2;
    let rail = device_rail();
    let surface = render_in(&rail, &theme, Scale::ONE, VW, short);
    assert!(
        is_blank(&surface),
        "a group whose first entry cannot be seated is not drawn at all"
    );
}

#[test]
fn a_reading_draws_trailing_and_the_label_leading() {
    let theme = Theme::dark();
    let plain = Tabs::new(vec![Tab::new("CPU")]).with_orientation(TabsOrientation::Vertical);
    let read = Tabs::new(vec![Tab::new("CPU").with_reading("18%")])
        .with_orientation(TabsOrientation::Vertical);
    let height = veach();
    let with_reading = render_in(&read, &theme, Scale::ONE, VW, height);
    let without = render_in(&plain, &theme, Scale::ONE, VW, height);
    assert_ne!(with_reading.pixels(), without.pixels());

    // The reading is quiet and trails; the label is the plain foreground and
    // leads.
    let muted = premul(theme.palette().on_surface_muted);
    let reading_x = (0..VW)
        .rev()
        .find(|&x| (0..height).any(|y| with_reading.get(x, y) == Some(muted)))
        .expect("the reading draws");
    let label_x = (0..VW)
        .find(|&x| {
            (0..height).any(|y| with_reading.get(x, y) == Some(premul(theme.palette().on_surface)))
        })
        .expect("the label draws");
    assert!(
        label_x < reading_x,
        "the label leads and the reading trails on the same line"
    );
}

#[test]
fn a_narrow_entry_truncates_its_label_before_its_reading() {
    // The reading is what the reader came for, so the name is what gives way.
    let theme = Theme::dark();
    let entry = Tabs::new(vec![
        Tab::new("A very long device name indeed").with_reading("18%")
    ])
    .with_orientation(TabsOrientation::Vertical);
    let height = veach();
    let narrow = 64;
    let surface = render_in(&entry, &theme, Scale::ONE, narrow, height);
    assert!(
        has_pixel(&surface, premul(theme.palette().on_surface_muted)),
        "the reading keeps its room"
    );
    assert!(
        untouched_outside(&surface, narrow, height),
        "neither the label nor the reading may run past the entry"
    );
}

#[test]
fn an_entrys_trend_draws_beneath_its_label_in_the_resources_own_colour() {
    let theme = Theme::dark();
    let rail = device_rail();
    let height = rail.measured_height(Scale::ONE, &theme);
    let surface = render_in(&rail, &theme, Scale::ONE, VW, height);
    assert!(has_pixel(&surface, premul(theme.palette().cpu_pressure)));
    assert!(has_pixel(&surface, premul(theme.palette().memory_pressure)));

    // The CPU trace sits below that entry's own label row, inside its band.
    let heading = heading_band(&theme);
    let label_row = veach();
    let cpu = premul(theme.palette().cpu_pressure);
    assert!(
        !region_has(&surface, (0, VW), (heading, heading + label_row), cpu),
        "a trace never draws over its own label row"
    );
    assert!(region_has(
        &surface,
        (0, VW),
        (
            heading + label_row,
            heading + label_row + trend_band(&theme)
        ),
        cpu
    ));
}

#[test]
fn a_live_reading_and_trend_are_re_stated_in_place() {
    // A fresh sample re-states the entries' readings without rebuilding the
    // strip, so the pointer's and the keyboard's places survive it.
    let mut rail = device_rail();
    let bounds = Rect::new(0, 0, VW, VH);
    rail.on_pointer(
        &moved(xi(VW / 2), xi(heading_band(&Theme::dark()) + 2)),
        bounds,
        Scale::ONE,
        &Theme::dark(),
        &mut sink(),
    );
    rail.on_pointer(&PRESS, bounds, Scale::ONE, &Theme::dark(), &mut sink());

    rail.tabs_mut()[0].set_reading(Some(String::from("94%")));
    rail.tabs_mut()[0].set_trend(Some(
        Chart::new(PressureKind::Cpu).with_samples([900, 940, 960]),
    ));
    assert_eq!(rail.tabs()[0].reading(), Some("94%"));

    assert_eq!(
        rail.on_pointer(&RELEASE, bounds, Scale::ONE, &Theme::dark(), &mut sink()),
        Some(TabsAction::Selected { index: 0 }),
        "a live reading changing mid-click cannot swallow it"
    );

    rail.tabs_mut()[0].set_reading(None);
    rail.tabs_mut()[0].set_trend(None);
    assert_eq!(rail.tabs()[0].reading(), None);
    assert_eq!(rail.tabs()[0].trend(), None);
}

#[test]
fn a_horizontal_strip_draws_neither_a_reading_a_trend_nor_a_heading() {
    // One row has no line for either, and a reading belongs in a horizontal
    // tab's label — so the strip draws the same pixels with or without them
    // rather than crowding its own label.
    let theme = Theme::dark();
    let plain = Tabs::new(vec![Tab::new("All"), Tab::new("Mine")]);
    let dressed = Tabs::new(vec![
        Tab::new("All")
            .with_group("Filters")
            .with_reading("12")
            .with_trend(Chart::new(PressureKind::Cpu).with_samples([500])),
        Tab::new("Mine").with_reading("3"),
    ]);
    assert_eq!(
        render(&plain, &theme).pixels(),
        render(&dressed, &theme).pixels()
    );
}

#[test]
fn the_sidebar_anatomy_reads_in_both_themes_and_under_heavy_contrast() {
    let rail = device_rail();
    let dark = Theme::dark();
    let height = rail.measured_height(Scale::ONE, &dark);
    let in_dark = render_in(&rail, &dark, Scale::ONE, VW, height);
    let in_light = render_in(&rail, &Theme::light(), Scale::ONE, VW, height);
    assert_ne!(in_dark.pixels(), in_light.pixels());

    for theme in [Theme::light(), high_contrast()] {
        let height = rail.measured_height(Scale::ONE, &theme);
        let surface = render_in(&rail, &theme, Scale::ONE, VW, height);
        assert!(
            has_pixel(&surface, premul(theme.palette().on_surface_muted)),
            "the headings and readings must read in every theme"
        );
        assert!(
            has_pixel(&surface, premul(theme.palette().cpu_pressure)),
            "an entry's trace must read in every theme"
        );
        assert!(untouched_outside(&surface, VW, height));
    }
}
