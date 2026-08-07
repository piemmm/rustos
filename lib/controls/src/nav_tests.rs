//! Unit tests for the breadcrumb trail (spec §11.12 sibling family, §20
//! checklist).
//!
//! These cover construction and measurement, the non-activatable current
//! (trailing) crumb via both pointer and keyboard, hover/focus emphasis,
//! disabled and denied ancestors (and the denied Authority Mark), the
//! keyboard model (Left/Right wrap, Home/End, Enter/Space), the pointer
//! press/release model, deterministic elision (the current crumb always
//! survives, the ellipsis activates the newest hidden ancestor, and
//! hit-testing agrees with the elided render), degenerate bounds, the empty
//! trail, and both built-in themes plus the heavier-contrast fixture.

use alloc::vec;
use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::nav::{Breadcrumb, BreadcrumbAction, Crumb};
use crate::state::{AuthorityState, ControlState};
use crate::testkit::high_contrast;

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn trail(labels: &[&str]) -> Breadcrumb {
    Breadcrumb::new(labels.iter().map(|l| Crumb::new(*l)).collect())
}

fn bounds(w: u32, h: u32) -> Rect {
    Rect::new(0, 0, w, h)
}

fn render(bc: &Breadcrumb, theme: &Theme, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    bc.render(&mut surface, bounds(w, h), Scale::ONE, theme);
    surface
}

/// A surface exactly `bc.measured_width()` wide, the tight-fit width at
/// which [`Breadcrumb::layout`] admits every crumb unelided (the elision
/// check is `<=`), so a test using this can reason about pointer positions
/// without needing elision itself.
fn render_full(bc: &Breadcrumb, theme: &Theme) -> Surface {
    let w = bc.measured_width(Scale::ONE, theme);
    let h = Breadcrumb::measured_height(Scale::ONE, theme);
    render(bc, theme, w, h)
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

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

// --- Construction and measurement ---------------------------------------

#[test]
fn construction_reports_len_current_and_crumbs() {
    let bc = trail(&["Root", "Docs", "file.txt"]);
    assert_eq!(bc.len(), 3);
    assert!(!bc.is_empty());
    assert_eq!(bc.current(), Some(2));
    assert_eq!(bc.crumbs()[0].label(), "Root");
    assert_eq!(bc.crumbs()[0].state(), ControlState::idle());
}

#[test]
fn crumb_builder_carries_label_and_state() {
    let crumb = Crumb::new("Root").with_state(ControlState::disabled());
    assert_eq!(crumb.label(), "Root");
    assert_eq!(crumb.state(), ControlState::disabled());
    let mut crumb = crumb;
    crumb.set_state(ControlState::idle());
    assert_eq!(crumb.state(), ControlState::idle());
}

#[test]
fn empty_trail_reports_empty_and_no_current() {
    let bc = Breadcrumb::new(Vec::new());
    assert!(bc.is_empty());
    assert_eq!(bc.len(), 0);
    assert_eq!(bc.current(), None);
    assert_eq!(bc.focus(), None);
}

#[test]
fn measured_height_grows_with_scale() {
    let theme = Theme::dark();
    let unit = Breadcrumb::measured_height(Scale::ONE, &theme);
    let doubled =
        Breadcrumb::measured_height(Scale::from_percent(200).expect("valid scale"), &theme);
    assert!(doubled > unit);
}

#[test]
fn measured_width_grows_with_more_or_longer_crumbs() {
    let theme = Theme::dark();
    let short = trail(&["Root", "file.txt"]);
    let long = trail(&["Root", "Docs", "Projects", "file.txt"]);
    assert!(long.measured_width(Scale::ONE, &theme) > short.measured_width(Scale::ONE, &theme));
}

#[test]
fn measured_width_of_empty_trail_is_zero() {
    let theme = Theme::dark();
    let bc = Breadcrumb::new(Vec::new());
    assert_eq!(bc.measured_width(Scale::ONE, &theme), 0);
}

// --- The current (trailing) crumb never activates -----------------------

#[test]
fn current_crumb_never_activates_by_pointer() {
    let theme = Theme::dark();
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);
    let point = Point::new(xi(w) - 1, xi(h) / 2);
    assert_eq!(
        bc.crumb_at(b, Scale::ONE, &theme, point),
        Some(2),
        "the point must land on the current crumb's own cell"
    );
    bc.on_pointer(&moved(point.x, point.y), b, Scale::ONE, &theme);
    assert_eq!(bc.on_pointer(&PRESS, b, Scale::ONE, &theme), None);
    assert_eq!(
        bc.on_pointer(&RELEASE, b, Scale::ONE, &theme),
        None,
        "pressing the current location is a no-op"
    );
}

#[test]
fn current_crumb_never_receives_keyboard_focus() {
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    bc.set_focus(Some(2));
    assert_eq!(bc.focus(), None, "focus on the current crumb is refused");
    bc.on_key(Key::Named(NamedKey::End));
    assert_eq!(
        bc.focus(),
        Some(1),
        "End reaches only the last activatable ancestor"
    );
}

#[test]
fn single_crumb_trail_has_no_activatable_ancestor() {
    let mut bc = trail(&["file.txt"]);
    assert_eq!(bc.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(bc.focus(), None);
    let theme = Theme::dark();
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);
    let point = Point::new(1, xi(h) / 2);
    bc.on_pointer(&moved(point.x, point.y), b, Scale::ONE, &theme);
    bc.on_pointer(&PRESS, b, Scale::ONE, &theme);
    assert_eq!(bc.on_pointer(&RELEASE, b, Scale::ONE, &theme), None);
}

// --- Hover and focus rendering differ from rest --------------------------

#[test]
fn hover_changes_the_rendered_ancestor() {
    let theme = Theme::dark();
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);
    let rest = render(&bc, &theme, w, h);

    assert_eq!(
        bc.crumb_at(b, Scale::ONE, &theme, Point::new(1, xi(h) / 2)),
        Some(0)
    );
    bc.on_pointer(&moved(1, xi(h) / 2), b, Scale::ONE, &theme);
    let hovered = render(&bc, &theme, w, h);
    assert_ne!(
        rest.pixels(),
        hovered.pixels(),
        "hover must change the rendered ancestor"
    );
}

#[test]
fn hovering_the_current_crumb_changes_nothing() {
    let theme = Theme::dark();
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);
    let rest = render(&bc, &theme, w, h);
    bc.on_pointer(&moved(xi(w) - 1, xi(h) / 2), b, Scale::ONE, &theme);
    let after = render(&bc, &theme, w, h);
    assert_eq!(
        rest.pixels(),
        after.pixels(),
        "the current location never takes hover emphasis"
    );
}

#[test]
fn keyboard_focus_draws_a_ring_distinct_from_rest_and_from_hover() {
    let theme = Theme::dark();
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let rest = render(&bc, &theme, w, h);

    let mut hovered_only = bc.clone();
    hovered_only.on_pointer(&moved(1, xi(h) / 2), bounds(w, h), Scale::ONE, &theme);
    let hovered = render(&hovered_only, &theme, w, h);

    bc.set_focus(Some(0));
    let focused = render(&bc, &theme, w, h);

    assert_ne!(rest.pixels(), focused.pixels());
    assert_ne!(
        hovered.pixels(),
        focused.pixels(),
        "keyboard focus must draw its own ring, distinct from hover"
    );
}

// --- Disabled and denied ancestors ---------------------------------------

#[test]
fn disabled_ancestor_refuses_activation() {
    let mut bc = Breadcrumb::new(vec![
        Crumb::new("Root").with_state(ControlState::disabled()),
        Crumb::new("file.txt"),
    ]);
    bc.set_focus(Some(0));
    assert_eq!(bc.on_key(Key::Named(NamedKey::Enter)), None);
    assert_eq!(bc.on_key(Key::Char(' ')), None);
}

#[test]
fn disabled_ancestor_refuses_activation_by_pointer() {
    let theme = Theme::dark();
    let mut bc = Breadcrumb::new(vec![
        Crumb::new("Root").with_state(ControlState::disabled()),
        Crumb::new("file.txt"),
    ]);
    let surface = render_full(&bc, &theme);
    let (w, h) = (surface.width(), surface.height());
    let b = bounds(w, h);
    bc.on_pointer(&moved(1, xi(h) / 2), b, Scale::ONE, &theme);
    bc.on_pointer(&PRESS, b, Scale::ONE, &theme);
    assert_eq!(bc.on_pointer(&RELEASE, b, Scale::ONE, &theme), None);
}

#[test]
fn denied_ancestor_refuses_activation_and_shows_authority_mark() {
    let theme = Theme::dark();
    let mut bc = Breadcrumb::new(vec![
        Crumb::new("Root").with_state(ControlState::idle().with_authority(AuthorityState::Denied)),
        Crumb::new("file.txt"),
    ]);
    bc.set_focus(Some(0));
    assert_eq!(bc.on_key(Key::Named(NamedKey::Enter)), None);
    let surface = render_full(&bc, &theme);
    assert!(
        has_pixel(&surface, premul(theme.palette().denied)),
        "a denied ancestor must paint the Authority Mark"
    );
}

// --- Keyboard focus movement ---------------------------------------------

#[test]
fn left_and_right_move_focus_among_ancestors_and_wrap() {
    let mut bc = trail(&["Root", "Docs", "Projects", "file.txt"]);
    bc.on_key(Key::Named(NamedKey::Right));
    assert_eq!(bc.focus(), Some(0));
    bc.on_key(Key::Named(NamedKey::Right));
    bc.on_key(Key::Named(NamedKey::Right));
    assert_eq!(bc.focus(), Some(2));
    bc.on_key(Key::Named(NamedKey::Right));
    assert_eq!(bc.focus(), Some(0), "wraps past the last ancestor");
    bc.on_key(Key::Named(NamedKey::Left));
    assert_eq!(bc.focus(), Some(2), "wraps back past the first ancestor");
}

#[test]
fn home_and_end_jump_to_first_and_last_ancestor() {
    let mut bc = trail(&["Root", "Docs", "Projects", "file.txt"]);
    bc.set_focus(Some(1));
    bc.on_key(Key::Named(NamedKey::Home));
    assert_eq!(bc.focus(), Some(0));
    bc.on_key(Key::Named(NamedKey::End));
    assert_eq!(bc.focus(), Some(2));
}

#[test]
fn set_focus_clamps_the_current_crumb_and_out_of_range_indices() {
    let mut bc = trail(&["Root", "file.txt"]);
    bc.set_focus(Some(1));
    assert_eq!(bc.focus(), None, "the current crumb never holds focus");
    bc.set_focus(Some(99));
    assert_eq!(bc.focus(), None, "an out-of-range index clears focus");
    bc.set_focus(Some(0));
    assert_eq!(bc.focus(), Some(0));
    bc.set_focus(None);
    assert_eq!(bc.focus(), None);
}

#[test]
fn a_disabled_or_denied_ancestor_keeps_its_slot_in_focus_order() {
    // Left/Right must not skip an ancestor whose state refuses activation —
    // it still holds its place in the trail, exactly as a Menu row does.
    let mut bc = Breadcrumb::new(vec![
        Crumb::new("Root").with_state(ControlState::disabled()),
        Crumb::new("Docs").with_state(ControlState::idle().with_authority(AuthorityState::Denied)),
        Crumb::new("file.txt"),
    ]);
    bc.on_key(Key::Named(NamedKey::Right));
    assert_eq!(bc.focus(), Some(0));
    bc.on_key(Key::Named(NamedKey::Right));
    assert_eq!(bc.focus(), Some(1));
}

#[test]
fn enter_and_space_activate_the_focused_ancestor() {
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    bc.set_focus(Some(1));
    assert_eq!(
        bc.on_key(Key::Named(NamedKey::Enter)),
        Some(BreadcrumbAction::Activate { index: 1 })
    );
    assert_eq!(
        bc.on_key(Key::Char(' ')),
        Some(BreadcrumbAction::Activate { index: 1 })
    );
}

// --- Pointer press/release -------------------------------------------------

#[test]
fn hover_then_click_activates_an_ancestor() {
    let theme = Theme::dark();
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);
    let point = Point::new(1, xi(h) / 2);
    assert_eq!(
        bc.on_pointer(&moved(point.x, point.y), b, Scale::ONE, &theme),
        None
    );
    assert_eq!(bc.on_pointer(&PRESS, b, Scale::ONE, &theme), None);
    assert_eq!(
        bc.on_pointer(&RELEASE, b, Scale::ONE, &theme),
        Some(BreadcrumbAction::Activate { index: 0 })
    );
}

#[test]
fn release_outside_the_pressed_crumb_does_not_activate() {
    let theme = Theme::dark();
    let mut bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);
    bc.on_pointer(&moved(1, xi(h) / 2), b, Scale::ONE, &theme);
    bc.on_pointer(&PRESS, b, Scale::ONE, &theme);
    // Slide onto the current (trailing) crumb before releasing.
    bc.on_pointer(&moved(xi(w) - 1, xi(h) / 2), b, Scale::ONE, &theme);
    assert_eq!(bc.on_pointer(&RELEASE, b, Scale::ONE, &theme), None);
}

// --- Elision --------------------------------------------------------------

/// Sweep every hit-testable x at `y` and collect the distinct crumb indices
/// [`Breadcrumb::crumb_at`] answers, left to right. Because the elided
/// layout is a single shared function every render and hit test consults,
/// this sequence is exactly the order the trail painted its cells in.
fn sweep(bc: &Breadcrumb, b: Rect, theme: &Theme, h: u32) -> Vec<usize> {
    let mut seen = Vec::new();
    for x in 0..b.width {
        if let Some(idx) = bc.crumb_at(b, Scale::ONE, theme, Point::new(xi(x), xi(h) / 2)) {
            if seen.last() != Some(&idx) {
                seen.push(idx);
            }
        }
    }
    seen
}

#[test]
fn elision_keeps_the_current_crumb_and_activates_the_newest_hidden_ancestor() {
    let theme = Theme::dark();
    let labels = [
        "Alpha",
        "Bravo",
        "Charlie",
        "Delta",
        "Echo",
        "Foxtrot",
        "Golf",
        "Hotel",
        "current.txt",
    ];
    let mut bc = trail(&labels);
    let last = bc.len() - 1;
    let full_w = bc.measured_width(Scale::ONE, &theme);
    // A quarter of the unelided width comfortably forces elision while
    // still fitting the ellipsis and a couple of trailing crumbs.
    let w = full_w / 4;
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);

    let seen = sweep(&bc, b, &theme, h);
    assert_eq!(
        seen.last(),
        Some(&last),
        "the current crumb must remain visible"
    );
    assert!(
        seen.len() < labels.len(),
        "a narrow trail must elide some ancestors"
    );
    assert!(
        seen.windows(2).all(|pair| pair[0] < pair[1]),
        "hit-testing must agree with a render that lays crumbs out in order"
    );

    let newest_hidden = seen[0];
    assert!(
        newest_hidden > 0,
        "at least the root ancestor must be hidden"
    );

    // Rendering paints both the muted ellipsis and the emphasised current
    // crumb, confirming the sweep's structural read is backed by real pixels.
    let surface = render(&bc, &theme, w, h);
    assert_eq!(surface.pixels().len(), (w * h) as usize);
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));

    // Activating the leading (ellipsis) cell reaches the newest ancestor it
    // hides.
    bc.on_pointer(&moved(1, xi(h) / 2), b, Scale::ONE, &theme);
    bc.on_pointer(&PRESS, b, Scale::ONE, &theme);
    assert_eq!(
        bc.on_pointer(&RELEASE, b, Scale::ONE, &theme),
        Some(BreadcrumbAction::Activate {
            index: newest_hidden
        })
    );
}

#[test]
fn a_wide_bound_elides_nothing() {
    let theme = Theme::dark();
    let bc = trail(&["Root", "Docs", "Projects", "file.txt"]);
    let full_w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let seen = sweep(&bc, bounds(full_w, h), &theme, h);
    // Every crumb shows individually: no ellipsis stands in for any of them.
    assert_eq!(seen, vec![0, 1, 2, 3]);
}

#[test]
fn extremely_narrow_bounds_show_only_the_truncated_current_crumb() {
    let theme = Theme::dark();
    let bc = trail(&["Alpha", "Bravo", "Charlie", "much-longer-current-name.txt"]);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    // Too small even for the leading ellipsis plus the current crumb.
    let w = 40;
    let b = bounds(w, h);
    let seen = sweep(&bc, b, &theme, h);
    assert_eq!(
        seen,
        vec![3],
        "only the current crumb is shown, with no ellipsis at all"
    );
    let surface = render(&bc, &theme, w, h);
    assert_eq!(surface.pixels().len(), (w * h) as usize);
}

// --- Degenerate bounds ------------------------------------------------------

#[test]
fn zero_width_bounds_paint_nothing_and_hit_test_nothing() {
    let theme = Theme::dark();
    let bc = trail(&["Root", "file.txt"]);
    let mut surface = Surface::new(20, 40).expect("surface");
    let b = Rect::new(0, 0, 0, 40);
    bc.render(&mut surface, b, Scale::ONE, &theme);
    assert!(surface.pixels().iter().all(|&p| p == Pixel::TRANSPARENT));
    assert_eq!(bc.crumb_at(b, Scale::ONE, &theme, Point::new(0, 0)), None);
}

#[test]
fn zero_height_bounds_paint_nothing_and_hit_test_nothing() {
    let theme = Theme::dark();
    let bc = trail(&["Root", "file.txt"]);
    let mut surface = Surface::new(40, 20).expect("surface");
    let b = Rect::new(0, 0, 40, 0);
    bc.render(&mut surface, b, Scale::ONE, &theme);
    assert!(surface.pixels().iter().all(|&p| p == Pixel::TRANSPARENT));
    assert_eq!(bc.crumb_at(b, Scale::ONE, &theme, Point::new(0, 0)), None);
}

#[test]
fn bounds_narrower_than_one_crumb_render_without_panicking() {
    let theme = Theme::dark();
    let bc = trail(&["Root", "Docs", "file.txt"]);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let w = 2;
    let mut surface = Surface::new(w, h).expect("surface");
    bc.render(&mut surface, bounds(w, h), Scale::ONE, &theme);
    // No panic, and the crumb_at contract still holds: any hit must resolve
    // to the trailing (current) crumb, since only it can fit at all.
    let hit = bc.crumb_at(bounds(w, h), Scale::ONE, &theme, Point::new(0, xi(h) / 2));
    assert!(hit.is_none() || hit == Some(2));
}

// --- Empty trail ------------------------------------------------------------

#[test]
fn empty_trail_renders_nothing_and_answers_none_everywhere() {
    let theme = Theme::dark();
    let bc = Breadcrumb::new(Vec::new());
    let mut surface = Surface::new(80, 40).expect("surface");
    bc.render(&mut surface, bounds(80, 40), Scale::ONE, &theme);
    assert!(surface.pixels().iter().all(|&p| p == Pixel::TRANSPARENT));
    assert_eq!(
        bc.crumb_at(bounds(80, 40), Scale::ONE, &theme, Point::new(5, 5)),
        None
    );
    assert_eq!(bc.current(), None);
    assert_eq!(bc.focus(), None);
    let mut bc = bc;
    assert_eq!(bc.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(
        bc.on_pointer(&moved(5, 5), bounds(80, 40), Scale::ONE, &theme),
        None
    );
}

// --- Themes and contrast -----------------------------------------------

#[test]
fn theme_switch_changes_the_plate() {
    let bc = trail(&["Root", "Docs", "file.txt"]);
    let dark = render_full(&bc, &Theme::dark());
    let light = render_full(&bc, &Theme::light());
    assert_ne!(dark.pixels(), light.pixels());
}

#[test]
fn high_contrast_strengthens_the_separator_and_the_current_crumb() {
    let bc = trail(&["Root", "Docs", "file.txt"]);
    let normal = render_full(&bc, &Theme::dark());
    let heavy = render_full(&bc, &high_contrast());
    assert_ne!(
        normal.pixels(),
        heavy.pixels(),
        "the heavier-contrast theme must render differently"
    );
}

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(scale, &theme);
    let h = Breadcrumb::measured_height(scale, &theme);
    let mut surface = Surface::new(w, h).expect("surface");
    bc.render(&mut surface, bounds(w, h), scale, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

// --- Render-equivalence equality (the host's repaint gate) ---------------

#[test]
fn hit_test_bookkeeping_is_invisible_to_the_trail() {
    let theme = Theme::dark();
    let bc = trail(&["Root", "Docs", "file.txt"]);
    let w = bc.measured_width(Scale::ONE, &theme);
    let h = Breadcrumb::measured_height(Scale::ONE, &theme);
    let b = bounds(w, h);

    // Two samples clear of the trail, so only the recorded coordinate
    // differs — that is not a drawn property.
    let mut a = bc.clone();
    let mut b_copy = bc.clone();
    a.on_pointer(&moved(xi(w) + 40, xi(h) + 40), b, Scale::ONE, &theme);
    b_copy.on_pointer(&moved(xi(w) + 90, xi(h) + 12), b, Scale::ONE, &theme);
    assert_eq!(a, b_copy);
    assert_eq!(
        render(&a, &theme, w, h).pixels(),
        render(&b_copy, &theme, w, h).pixels()
    );
}
