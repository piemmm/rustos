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

use alloc::vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::state::{ActivityState, ControlState, SelectionState, ValidationState};
use crate::tabs::{Tab, Tabs, TabsAction, TabsOrientation};
use crate::testkit::high_contrast;

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

// --- Orientation: the vertical (sidebar) strip (spec §11.12) --------------

/// A tall, narrow strip: three tabs of `VEACH` stacked down `VW` pixels of
/// width, so every tab spans the full width and its labels align.
const VW: u32 = 96;
const VH: u32 = 240;
const VEACH: u32 = VH / 3;

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
        (0_usize, VEACH / 2),
        (1, VEACH + VEACH / 2),
        (2, 2 * VEACH + VEACH / 2),
    ] {
        assert_eq!(tabs.tab_at(bounds, Point::new(0, xi(y))), Some(index));
        assert_eq!(
            tabs.tab_at(bounds, Point::new(xi(VW - 1), xi(y))),
            Some(index),
            "a vertical tab spans the whole width so its label aligns with the others"
        );
    }
    assert_eq!(tabs.tab_at(bounds, Point::new(xi(VW), xi(VEACH / 2))), None);
    assert_eq!(tabs.tab_at(bounds, Point::new(0, xi(VH))), None);
}

#[test]
fn vertical_selection_seam_runs_down_the_leading_edge() {
    let theme = Theme::dark();
    let accent = premul(theme.palette().accent);
    let mut tabs = vertical_three();
    tabs.set_selected(1);
    let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
    // The seam hugs the leading edge of tab 1, running along its own height.
    assert!(region_has(
        &surface,
        (0, 1),
        (VEACH + 4, 2 * VEACH - 4),
        accent
    ));
    // A column carries no lower seam: that is the horizontal strip's edge.
    assert!(!region_has(
        &surface,
        (4, VW),
        (2 * VEACH - 2, 2 * VEACH),
        accent
    ));
    // Nor does it carry a trailing one.
    assert!(!region_has(
        &surface,
        (VW - 1, VW),
        (VEACH + 4, 2 * VEACH - 4),
        accent
    ));
    // Tab 0 is neither selected nor loading, so it carries no seam at all.
    assert!(!region_has(&surface, (0, VW), (4, VEACH - 4), accent));
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
    let each = VH / 2;
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
    let each = VH / 2;
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
    let mut horizontal = three_tabs();
    horizontal.set_current(Some(1));
    assert_eq!(horizontal.on_key(Key::Named(NamedKey::Down)), None);
    assert_eq!(
        horizontal.current(),
        Some(1),
        "a row must not answer to a column's arrows"
    );
    assert_eq!(horizontal.on_key(Key::Named(NamedKey::Up)), None);
    assert_eq!(horizontal.current(), Some(1));
    horizontal.on_key(Key::Named(NamedKey::Right));
    assert_eq!(horizontal.current(), Some(2));

    let mut vertical = vertical_three();
    vertical.set_current(Some(1));
    assert_eq!(vertical.on_key(Key::Named(NamedKey::Left)), None);
    assert_eq!(
        vertical.current(),
        Some(1),
        "a column must not answer to a row's arrows"
    );
    assert_eq!(vertical.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(vertical.current(), Some(1));
    vertical.on_key(Key::Named(NamedKey::Down));
    assert_eq!(vertical.current(), Some(2));
    vertical.on_key(Key::Named(NamedKey::Down));
    assert_eq!(vertical.current(), Some(0), "…and wraps along its own axis");
    vertical.on_key(Key::Named(NamedKey::Up));
    assert_eq!(vertical.current(), Some(2));
}

#[test]
fn home_and_end_jump_to_the_ends_of_a_vertical_strip_too() {
    let mut tabs = vertical_three();
    tabs.set_current(Some(1));
    tabs.on_key(Key::Named(NamedKey::End));
    assert_eq!(tabs.current(), Some(2));
    tabs.on_key(Key::Named(NamedKey::Home));
    assert_eq!(tabs.current(), Some(0));
    assert_eq!(
        tabs.on_key(Key::Named(NamedKey::Enter)),
        Some(TabsAction::Selected { index: 0 })
    );
}

#[test]
fn a_vertical_press_selects_the_tab_that_was_drawn() {
    let theme = Theme::dark();
    let mut tabs = vertical_three();
    let bounds = Rect::new(0, 0, VW, VH);
    let over_band_one = moved(xi(VW / 2), xi(VEACH + VEACH / 2));
    assert_eq!(tabs.on_pointer(&over_band_one, bounds), None);
    assert_eq!(tabs.current(), Some(1));
    assert_eq!(tabs.on_pointer(&PRESS, bounds), None);
    assert_eq!(
        tabs.on_pointer(&RELEASE, bounds),
        Some(TabsAction::Selected { index: 1 })
    );
    // The owner commits the selection, and the band the press chose is the one
    // that draws as selected.
    tabs.set_selected(1);
    let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
    assert_eq!(
        surface.get(VW - 2, VEACH + 4),
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
    tabs.on_pointer(&moved(xi(VW / 2), xi(VEACH / 2)), bounds);
    tabs.on_pointer(&PRESS, bounds);
    tabs.on_pointer(&moved(xi(VW / 2), xi(2 * VEACH + VEACH / 2)), bounds);
    assert_eq!(tabs.on_pointer(&RELEASE, bounds), None);
}

#[test]
fn an_axis_too_short_for_every_tab_omits_them_all() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, VW, VH);
    let crowded = Tabs::new(vec![Tab::new("x"); 300]).with_orientation(TabsOrientation::Vertical);
    let surface = render_in(&crowded, &theme, Scale::ONE, VW, VH);
    assert!(
        is_blank(&surface),
        "a tab that cannot have a whole band of its own is omitted, never drawn partially"
    );
    // Hit-testing agrees: nothing was drawn, so nothing can be pressed.
    assert!((0..VH).all(|y| crowded
        .tab_at(bounds, Point::new(xi(VW / 2), xi(y)))
        .is_none()));

    // The same strip with bands it can afford draws them all, and every band
    // hit-tests to the tab that was drawn there.
    let roomy = Tabs::new(vec![Tab::new("x"); 12]).with_orientation(TabsOrientation::Vertical);
    assert!(!is_blank(&render_in(&roomy, &theme, Scale::ONE, VW, VH)));
    let band = VH / 12;
    for index in 0..12_usize {
        let step = u32::try_from(index).expect("index fits");
        let y = step * band + band / 2;
        assert_eq!(
            roomy.tab_at(bounds, Point::new(xi(VW / 2), xi(y))),
            Some(index)
        );
    }
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
            assert_eq!(tabs.tab_at(bounds, Point::new(0, 0)), None);
        }
        // An off-surface origin is refused rather than wrapped into the surface.
        let off = Rect::new(-4, -4, VW, VH);
        let mut surface = Surface::new(VW, VH).expect("surface");
        tabs.render(&mut surface, off, Scale::ONE, &theme);
        assert!(is_blank(&surface));
        assert_eq!(tabs.tab_at(off, Point::new(0, 0)), None);
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
        tabs.set_selected(1);
        let surface = render_in(&tabs, &theme, Scale::ONE, VW, VH);
        assert!(
            region_has(
                &surface,
                (0, 1),
                (VEACH + 4, 2 * VEACH - 4),
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
        tabs.set_selected(1);
        let surface = render_in(&tabs, theme, Scale::ONE, VW, VH);
        (0..VW)
            .take_while(|&x| surface.get(x, VEACH + 4) == Some(accent))
            .count()
    };
    assert!(seam_run(&high_contrast()) > seam_run(&Theme::dark()));
}
