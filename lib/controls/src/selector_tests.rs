//! Unit tests for the boolean-selector family (spec §20 checklist).
//!
//! These cover measurement (each control paints its plate within its bounds
//! and rounds its corners), the shape-coded value marks (toggle contact/thumb,
//! checkbox filled-square vs mixed-bar, radio bead) so state reads without
//! colour, theme switching, the spec §13 denied-vs-disabled distinction, the
//! Pressure Rail, the pending Heat Seam, focus, high contrast, and the
//! pointer/keyboard activation and next-value semantics of each control.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::selector::{
    selector_mark_rect, toggle_track_rect, Checkbox, Radio, SelectorAction, Toggle,
};
use crate::state::{
    AuthorityState, ControlState, PressureKind, PressureState, SelectionState, ValidationState,
};
use crate::testkit::high_contrast;

const W: u32 = 160;
const H: u32 = 28;

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
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

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// Whether `want` appears anywhere in the given rectangular region.
fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

fn toggle_surface(toggle: &Toggle, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    toggle.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        font(),
    );
    surface
}

fn checkbox_surface(checkbox: &Checkbox, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    checkbox.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        font(),
    );
    surface
}

fn radio_surface(radio: &Radio, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    radio.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        font(),
    );
    surface
}

/// The value-mark rectangle of a square selector (checkbox box, radio circle)
/// for the test row, measured through the same helper `render` paints with, so
/// a probe point can never drift from the drawn mark.
fn mark(theme: &Theme) -> (u32, u32, u32, u32) {
    selector_mark_rect(Rect::new(0, 0, W, H), Scale::ONE, theme).expect("mark rect")
}

/// The toggle track rectangle for the test row, measured the same way.
fn track(theme: &Theme) -> (u32, u32, u32, u32) {
    toggle_track_rect(Rect::new(0, 0, W, H), Scale::ONE, theme).expect("track rect")
}

// --- Measurement --------------------------------------------------------

#[test]
fn checkbox_paints_plate_and_rounds_its_corners() {
    let theme = Theme::dark();
    let surface = checkbox_surface(&Checkbox::new("Sync", SelectionState::Unselected), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
    // A mid-edge pixel is the Signal Rim; the extreme corner is rounded away.
    assert_eq!(surface.get(0, H / 2), Some(premul(theme.palette().rim)));
    assert_eq!(surface.get(0, 0), Some(Color::TRANSPARENT.premultiply()));
}

#[test]
fn toggle_track_is_a_rounded_pill() {
    let theme = Theme::dark();
    let surface = toggle_surface(&Toggle::new("Wi-Fi", false), &theme);
    assert_eq!(surface.get(0, H / 2), Some(premul(theme.palette().rim)));
    assert_eq!(surface.get(0, 0), Some(Color::TRANSPARENT.premultiply()));
}

// --- Toggle value: contact and thumb (§11.4) ---------------------------

#[test]
fn toggle_on_shows_an_accent_contact_and_off_does_not() {
    let theme = Theme::dark();
    let on = toggle_surface(&Toggle::new("On", true), &theme);
    let off = toggle_surface(&Toggle::new("Off", false), &theme);
    let accent = premul(theme.palette().accent);
    assert!(has_pixel(&on, accent));
    assert!(!has_pixel(&off, accent));
}

#[test]
fn toggle_thumb_reads_on_the_accent_only_when_on() {
    let theme = Theme::dark();
    let on = toggle_surface(&Toggle::new("On", true), &theme);
    let off = toggle_surface(&Toggle::new("Off", false), &theme);
    // The on thumb sits on the accent contact and uses the on-accent colour;
    // the off thumb uses the muted foreground instead.
    assert!(has_pixel(&on, premul(theme.palette().on_accent)));
    assert!(!has_pixel(&off, premul(theme.palette().on_accent)));
    assert!(has_pixel(&off, premul(theme.palette().on_surface_muted)));
}

// --- Checkbox value: filled square vs mixed bar (§11.5) ----------------

#[test]
fn checked_checkbox_fills_a_square_mark() {
    let theme = Theme::dark();
    let checked = checkbox_surface(&Checkbox::new("A", SelectionState::Selected), &theme);
    let empty = checkbox_surface(&Checkbox::new("A", SelectionState::Unselected), &theme);
    let accent = premul(theme.palette().accent);
    let (mx, my, mw, _) = mark(&theme);
    // The square fills the top of the mark region; an empty box has no mark.
    assert_eq!(checked.get(mx + mw / 2, my), Some(accent));
    assert!(!has_pixel(&empty, accent));
}

#[test]
fn mixed_checkbox_draws_a_bar_not_a_full_square() {
    let theme = Theme::dark();
    let mixed = checkbox_surface(&Checkbox::new("A", SelectionState::Mixed), &theme);
    let accent = premul(theme.palette().accent);
    let (mx, my, mw, mh) = mark(&theme);
    // The horizontal bar sits at the mark's centre, leaving the top clear —
    // so mixed is distinguishable from checked by shape, not just colour.
    assert_eq!(mixed.get(mx + mw / 2, my + mh / 2), Some(accent));
    assert_ne!(mixed.get(mx + mw / 2, my), Some(accent));
}

// --- Radio value: centre bead (§11.5) ----------------------------------

#[test]
fn selected_radio_draws_a_centre_bead() {
    let theme = Theme::dark();
    let selected = radio_surface(&Radio::new("One", true), &theme);
    let empty = radio_surface(&Radio::new("Two", false), &theme);
    let accent = premul(theme.palette().accent);
    let (mx, my, mw, mh) = mark(&theme);
    assert_eq!(selected.get(mx + mw / 2, my + mh / 2), Some(accent));
    assert!(!has_pixel(&empty, accent));
}

// --- Theme switching ----------------------------------------------------

#[test]
fn theme_switch_repaints_the_rim() {
    let checkbox = Checkbox::new("X", SelectionState::Unselected);
    let dark = checkbox_surface(&checkbox, &Theme::dark());
    let light = checkbox_surface(&checkbox, &Theme::light());
    assert_ne!(dark.get(0, H / 2), light.get(0, H / 2));
}

// --- §13 authority rendering -------------------------------------------

#[test]
fn denied_and_disabled_render_distinctly() {
    let theme = Theme::dark();
    let mut disabled = Checkbox::new("X", SelectionState::Unselected);
    disabled.set_state(ControlState::disabled());
    let mut denied = Checkbox::new("X", SelectionState::Unselected);
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));

    let d = checkbox_surface(&disabled, &theme);
    let n = checkbox_surface(&denied, &theme);
    assert_eq!(d.get(0, H / 2), Some(premul(theme.palette().border)));
    assert_eq!(n.get(0, H / 2), Some(premul(theme.palette().denied)));
    assert_ne!(d.get(0, H / 2), n.get(0, H / 2));
}

#[test]
fn denied_paints_a_lock_bead_and_disabled_does_not() {
    let theme = Theme::dark();
    let mut denied = Checkbox::new("Save", SelectionState::Unselected);
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let mut disabled = Checkbox::new("Save", SelectionState::Unselected);
    disabled.set_state(ControlState::disabled());
    let denied_s = checkbox_surface(&denied, &theme);
    let disabled_s = checkbox_surface(&disabled, &theme);
    let lock = premul(theme.palette().denied);
    // The Authority Mark bead sits at the trailing-top corner of the control.
    assert!(region_has(&denied_s, (W - 8, W), (0, 8), lock));
    assert!(!region_has(&disabled_s, (W - 8, W), (0, 8), lock));
}

// --- Pressure rail ------------------------------------------------------

#[test]
fn pressure_paints_a_leading_rail() {
    let theme = Theme::dark();
    let mut checkbox = Checkbox::new("Cache", SelectionState::Selected);
    checkbox
        .set_state(ControlState::idle().with_pressure(PressureState::Under(PressureKind::Memory)));
    let surface = checkbox_surface(&checkbox, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().memory_pressure)));
}

// --- Pending heat seam (§11.4) -----------------------------------------

#[test]
fn pending_toggle_shows_a_heat_seam() {
    let theme = Theme::dark();
    // Off toggles, so the accent contact/thumb never covers the sampled track
    // point: it sits at the trailing end of the track, past the off thumb.
    let mut pending = Toggle::new("Wi-Fi", false);
    pending.set_state(ControlState::idle().with_validation(ValidationState::Pending));
    let idle = Toggle::new("Wi-Fi", false);
    let pending_s = toggle_surface(&pending, &theme);
    let idle_s = toggle_surface(&idle, &theme);
    // The seam paints the active rim along the lower edge of the track while a
    // check is pending; an unchanged toggle has no seam there, and its plate
    // centre stays the resting surface.
    let (tx, ty, tw, th) = track(&theme);
    let (sx, sy) = (tx + tw - 2, ty + th - 2);
    assert_eq!(
        pending_s.get(sx, sy),
        Some(premul(theme.palette().rim_active))
    );
    assert_ne!(idle_s.get(sx, sy), Some(premul(theme.palette().rim_active)));
    assert_eq!(
        idle_s.get(sx, ty + th / 2),
        Some(premul(theme.palette().surface_raised))
    );
}

// --- Focus and high contrast -------------------------------------------

#[test]
fn focus_lifts_the_rim_to_the_active_colour() {
    let theme = Theme::dark();
    let mut checkbox = Checkbox::new("X", SelectionState::Unselected);
    let resting = checkbox_surface(&checkbox, &theme);
    checkbox.set_focused(true);
    let focused = checkbox_surface(&checkbox, &theme);
    assert_eq!(resting.get(0, H / 2), Some(premul(theme.palette().rim)));
    assert_eq!(
        focused.get(0, H / 2),
        Some(premul(theme.palette().rim_active))
    );
}

#[test]
fn high_contrast_thickens_the_signal_rim() {
    let rim = premul(Theme::dark().palette().rim);
    let normal = checkbox_surface(
        &Checkbox::new("X", SelectionState::Unselected),
        &Theme::dark(),
    );
    let heavy = checkbox_surface(
        &Checkbox::new("X", SelectionState::Unselected),
        &high_contrast(),
    );
    assert_ne!(normal.get(1, H / 2), Some(rim));
    assert_eq!(heavy.get(1, H / 2), Some(rim));
}

// --- Pointer interaction ------------------------------------------------

#[test]
fn toggle_press_then_release_requests_the_flipped_value() {
    let mut toggle = Toggle::new("X", false);
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(toggle.on_pointer(&moved(5, 14), bounds), None);
    assert_eq!(toggle.on_pointer(&PRESS, bounds), None);
    assert_eq!(
        toggle.on_pointer(&RELEASE, bounds),
        Some(SelectorAction::Set { on: true })
    );
}

#[test]
fn release_outside_cancels() {
    let mut toggle = Toggle::new("X", false);
    let bounds = Rect::new(0, 0, W, H);
    toggle.on_pointer(&moved(5, 14), bounds);
    toggle.on_pointer(&PRESS, bounds);
    toggle.on_pointer(&moved(1000, 1000), bounds);
    assert_eq!(toggle.on_pointer(&RELEASE, bounds), None);
}

#[test]
fn disabled_or_denied_selector_never_activates() {
    let bounds = Rect::new(0, 0, W, H);
    let mut disabled = Toggle::new("X", false);
    disabled.set_state(ControlState::disabled());
    disabled.on_pointer(&moved(5, 14), bounds);
    disabled.on_pointer(&PRESS, bounds);
    assert_eq!(disabled.on_pointer(&RELEASE, bounds), None);

    let mut denied = Toggle::new("X", false);
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    denied.on_pointer(&moved(5, 14), bounds);
    denied.on_pointer(&PRESS, bounds);
    assert_eq!(denied.on_pointer(&RELEASE, bounds), None);
    // A denied toggle keeps its value: it never emits an action.
    assert!(!denied.is_on());
}

// --- Keyboard interaction ----------------------------------------------

#[test]
fn focused_selector_activates_on_space_and_enter() {
    let mut checkbox = Checkbox::new("X", SelectionState::Unselected);
    checkbox.set_focused(true);
    assert_eq!(
        checkbox.on_key(Key::Char(' ')),
        Some(SelectorAction::Set { on: true })
    );
    assert_eq!(
        checkbox.on_key(Key::Named(NamedKey::Enter)),
        Some(SelectorAction::Set { on: true })
    );
    assert_eq!(checkbox.on_key(Key::Char('x')), None);
}

#[test]
fn unfocused_selector_ignores_keys() {
    let mut checkbox = Checkbox::new("X", SelectionState::Unselected);
    assert_eq!(checkbox.on_key(Key::Char(' ')), None);
}

// --- Next-value semantics ----------------------------------------------

#[test]
fn checkbox_activation_computes_the_next_value() {
    let bounds = Rect::new(0, 0, W, H);
    let click = |mut c: Checkbox| -> Option<SelectorAction> {
        c.on_pointer(&moved(5, 14), bounds);
        c.on_pointer(&PRESS, bounds);
        c.on_pointer(&RELEASE, bounds)
    };
    assert_eq!(
        click(Checkbox::new("A", SelectionState::Unselected)),
        Some(SelectorAction::Set { on: true })
    );
    assert_eq!(
        click(Checkbox::new("A", SelectionState::Selected)),
        Some(SelectorAction::Set { on: false })
    );
    // A mixed checkbox resolves to checked.
    assert_eq!(
        click(Checkbox::new("A", SelectionState::Mixed)),
        Some(SelectorAction::Set { on: true })
    );
}

#[test]
fn radio_activation_always_requests_selection() {
    let bounds = Rect::new(0, 0, W, H);
    let click = |mut r: Radio| -> Option<SelectorAction> {
        r.on_pointer(&moved(5, 14), bounds);
        r.on_pointer(&PRESS, bounds);
        r.on_pointer(&RELEASE, bounds)
    };
    assert_eq!(
        click(Radio::new("One", false)),
        Some(SelectorAction::Set { on: true })
    );
    // Clicking an already-selected radio still requests selection (idempotent);
    // a radio is only cleared by selecting a sibling.
    assert_eq!(
        click(Radio::new("One", true)),
        Some(SelectorAction::Set { on: true })
    );
}

// --- Scale --------------------------------------------------------------

#[test]
fn renders_at_a_larger_scale_without_panicking() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(200).expect("valid scale");
    let mut surface = Surface::new(W * 2, H * 2).expect("surface");
    Toggle::new("Big", true).render(
        &mut surface,
        Rect::new(0, 0, W * 2, H * 2),
        scale,
        &theme,
        font(),
    );
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// The bounds every render-equivalence case draws into.
fn plate() -> Rect {
    Rect::new(0, 0, W, H)
}

/// Paint `draw` into a fresh surface and hand back its pixels.
fn pixels_of(draw: impl FnOnce(&mut Surface)) -> alloc::vec::Vec<Pixel> {
    let mut surface = Surface::new(W, H).expect("surface");
    draw(&mut surface);
    surface.pixels().to_vec()
}

#[test]
fn pointer_position_alone_never_changes_a_selector_render() {
    let theme = Theme::dark();
    // Both samples stay off the plate, so only the recorded coordinate — the
    // one thing the shared selector core keeps for hit-testing — differs.
    let outside = moved(i32::try_from(W).expect("width") + 30, 4);
    let farther = moved(i32::try_from(W).expect("width") + 80, 9);

    let mut a = Toggle::new("Wi-Fi", false);
    let mut b = a.clone();
    a.on_pointer(&outside, plate());
    b.on_pointer(&farther, plate());
    assert_eq!(a, b, "a coordinate off the plate is not a drawn property");
    assert_eq!(
        pixels_of(|s| a.render(s, plate(), Scale::ONE, &theme, font())),
        pixels_of(|s| b.render(s, plate(), Scale::ONE, &theme, font())),
        "…and the two must therefore paint identically"
    );

    let mut a = Checkbox::new("Include hidden", SelectionState::Unselected);
    let mut b = a.clone();
    a.on_pointer(&outside, plate());
    b.on_pointer(&farther, plate());
    assert_eq!(a, b);
    assert_eq!(
        pixels_of(|s| a.render(s, plate(), Scale::ONE, &theme, font())),
        pixels_of(|s| b.render(s, plate(), Scale::ONE, &theme, font())),
    );

    let mut a = Radio::new("Daily", false);
    let mut b = a.clone();
    a.on_pointer(&outside, plate());
    b.on_pointer(&farther, plate());
    assert_eq!(a, b);
    assert_eq!(
        pixels_of(|s| a.render(s, plate(), Scale::ONE, &theme, font())),
        pixels_of(|s| b.render(s, plate(), Scale::ONE, &theme, font())),
    );
}

#[test]
fn press_latch_alone_never_changes_a_selector_render() {
    let theme = Theme::dark();
    // One toggle holds a real press latch; the other is merely *shown*
    // pressed. Only the latch differs, and a latch is not drawn.
    let mut latched = Toggle::new("Wi-Fi", false);
    latched.on_pointer(&PRESS, plate());

    let mut shown = Toggle::new("Wi-Fi", false);
    let mut pressed = ControlState::idle();
    pressed.pointer = crate::state::PointerState::Pressed;
    shown.set_state(pressed);

    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        pixels_of(|s| latched.render(s, plate(), Scale::ONE, &theme, font())),
        pixels_of(|s| shown.render(s, plate(), Scale::ONE, &theme, font())),
        "…and the two must therefore paint identically"
    );
    assert_eq!(
        latched.on_pointer(&RELEASE, plate()),
        Some(SelectorAction::Set { on: true }),
        "the latch still governs activation, it is only invisible"
    );
}

#[test]
fn hover_and_value_each_change_a_selector_render() {
    let resting = Toggle::new("Wi-Fi", false);

    let mut hovered = resting.clone();
    hovered.on_pointer(&moved(4, 4), plate());
    assert_ne!(resting, hovered, "a hover highlight is visible");

    let switched = Toggle::new("Wi-Fi", true);
    assert_ne!(resting, switched, "the value's thumb position is visible");
}
