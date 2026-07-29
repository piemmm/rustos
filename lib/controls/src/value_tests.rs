//! Unit tests for the value-control family (spec §20 checklist).
//!
//! These cover slider measurement and value-to-position mapping, drag and
//! keyboard stepping (with fail-closed bounds and zero-step), the bounded-cap
//! marker, the §13 denied-vs-disabled distinction, the resource (pressure)
//! rail colour, dark/light and high-contrast coverage, scale, and the progress
//! trace's known/working/indeterminate/complete/failed rendering including the
//! reduced-motion static trace.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Contrast, Rgba, Theme};

use crate::paint::progress_thickness;
use crate::state::{
    ActivityState, AuthorityState, ControlState, PressureKind, PressureState, ProgressValue,
    RecoveryState,
};
use crate::value::{Progress, Slider, SliderAction};

const W: u32 = 200;
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

fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

fn slider_surface(slider: &Slider, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    slider.render(&mut surface, Rect::new(0, 0, W, H), Scale::ONE, theme);
    surface
}

fn progress_surface(progress: &Progress, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    progress.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        font(),
    );
    surface
}

/// A theme identical to [`Theme::dark`] but with [`Contrast::High`].
fn high_contrast() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test High Contrast",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        Contrast::High,
    )
}

/// A theme identical to [`Theme::dark`] but with reduced motion.
fn reduced_motion() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test Reduced Motion",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        base.contrast(),
    )
}

/// The rightmost x at row `y` painted the value-track accent, if any.
fn active_extent(surface: &Surface, accent: Pixel, y: u32) -> Option<u32> {
    (0..W).rev().find(|&x| surface.get(x, y) == Some(accent))
}

fn bounds() -> Rect {
    Rect::new(0, 0, W, H)
}

/// The trace's thin band within the test row as `(top, bottom)`: the theme's
/// progress thickness, top-aligned when a caption fits beneath it and centred
/// otherwise, mirroring the layout the trace itself resolves.
fn band_rows(theme: &Theme) -> (u32, u32) {
    let band = progress_thickness(theme, Scale::ONE);
    let spare = H - band;
    let top = if spare >= font().glyph_height() {
        0
    } else {
        spare / 2
    };
    (top, top + band)
}

// --- Slider measurement and value mapping (§11.6) ----------------------

#[test]
fn slider_paints_groove_track_and_thumb() {
    let theme = Theme::dark();
    let surface = slider_surface(&Slider::new(500), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(has_pixel(&surface, premul(theme.palette().accent)));
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
    // The control's extreme corner lies outside the groove and the thumb.
    assert_eq!(surface.get(0, 0), Some(Color::TRANSPARENT.premultiply()));
}

#[test]
fn slider_value_maps_to_thumb_position() {
    let theme = Theme::dark();
    let accent = premul(theme.palette().accent);
    let low = slider_surface(&Slider::new(200), &theme);
    let high = slider_surface(&Slider::new(800), &theme);
    let lo = active_extent(&low, accent, H / 2).expect("low accent");
    let hi = active_extent(&high, accent, H / 2).expect("high accent");
    assert!(
        hi > lo,
        "higher value must fill further right ({hi} > {lo})"
    );
}

#[test]
fn slider_renders_in_both_themes() {
    assert!(has_pixel(
        &slider_surface(&Slider::new(500), &Theme::dark()),
        premul(Theme::dark().palette().accent)
    ));
    assert!(has_pixel(
        &slider_surface(&Slider::new(500), &Theme::light()),
        premul(Theme::light().palette().accent)
    ));
}

// --- Slider keyboard stepping (§11.6) ----------------------------------

#[test]
fn slider_arrows_step_by_the_line_step() {
    let mut slider = Slider::new(500);
    slider.set_focused(true);
    assert_eq!(
        slider.on_key(Key::Named(NamedKey::Right)),
        Some(SliderAction::SetValue { permille: 510 })
    );
    assert_eq!(slider.value(), 510);
    assert_eq!(
        slider.on_key(Key::Named(NamedKey::Left)),
        Some(SliderAction::SetValue { permille: 500 })
    );
}

#[test]
fn slider_page_and_home_end_keys() {
    let mut slider = Slider::new(500);
    slider.set_focused(true);
    assert_eq!(
        slider.on_key(Key::Named(NamedKey::PageUp)),
        Some(SliderAction::SetValue { permille: 600 })
    );
    assert_eq!(
        slider.on_key(Key::Named(NamedKey::Home)),
        Some(SliderAction::SetValue { permille: 0 })
    );
    assert_eq!(
        slider.on_key(Key::Named(NamedKey::End)),
        Some(SliderAction::SetValue { permille: 1000 })
    );
}

#[test]
fn slider_bounds_are_fail_closed() {
    let mut top = Slider::new(1000);
    top.set_focused(true);
    assert_eq!(top.on_key(Key::Named(NamedKey::Right)), None);
    let mut bottom = Slider::new(0);
    bottom.set_focused(true);
    assert_eq!(bottom.on_key(Key::Named(NamedKey::Left)), None);
}

#[test]
fn slider_zero_step_moves_nothing() {
    let mut slider = Slider::new(500).with_steps(0, 0);
    slider.set_focused(true);
    assert_eq!(slider.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(slider.on_key(Key::Named(NamedKey::PageUp)), None);
    assert_eq!(slider.value(), 500);
}

#[test]
fn slider_unfocused_ignores_keys() {
    let mut slider = Slider::new(500);
    assert_eq!(slider.on_key(Key::Named(NamedKey::Right)), None);
}

#[test]
fn slider_clamps_construction_and_set_value() {
    assert_eq!(Slider::new(5000).value(), 1000);
    let mut slider = Slider::new(300);
    slider.set_value(9000);
    assert_eq!(slider.value(), 1000);
}

// --- Slider pointer drag (§11.6) ---------------------------------------

#[test]
fn slider_drag_updates_and_commits() {
    let mut slider = Slider::new(0);
    let b = bounds();
    assert_eq!(slider.on_pointer(&moved(100, 14), b), None);
    assert_eq!(
        slider.on_pointer(&PRESS, b),
        Some(SliderAction::SetValue { permille: 500 })
    );
    let dragged = slider.on_pointer(&moved(151, 14), b);
    assert!(matches!(
        dragged,
        Some(SliderAction::SetValue { permille }) if permille > 700
    ));
    assert_eq!(slider.on_pointer(&RELEASE, b), None);
}

#[test]
fn slider_move_without_press_does_not_commit() {
    let mut slider = Slider::new(500);
    assert_eq!(slider.on_pointer(&moved(150, 14), bounds()), None);
    assert_eq!(slider.value(), 500);
}

// --- Slider bounded cap (§11.6) ----------------------------------------

#[test]
fn slider_cap_constrains_value_and_shows_a_marker() {
    let theme = Theme::dark();
    let slider = Slider::new(500).with_cap(600);
    assert!(has_pixel(
        &slider_surface(&slider, &theme),
        premul(theme.palette().warning)
    ));
}

#[test]
fn slider_cannot_step_past_its_cap() {
    let mut slider = Slider::new(500).with_cap(600);
    slider.set_focused(true);
    // End requests the maximum, which the cap holds at 600 rather than full.
    assert_eq!(
        slider.on_key(Key::Named(NamedKey::End)),
        Some(SliderAction::SetValue { permille: 600 })
    );
    assert_eq!(slider.value(), 600);
    // At the cap, a further step reports no change (fail closed).
    assert_eq!(slider.on_key(Key::Named(NamedKey::Right)), None);
}

#[test]
fn slider_drag_clamps_to_its_cap() {
    let mut slider = Slider::new(300).with_cap(600);
    let b = bounds();
    // A drag to the far right resolves past the cap but commits only the cap.
    let _ = slider.on_pointer(&moved(195, 14), b);
    assert_eq!(
        slider.on_pointer(&PRESS, b),
        Some(SliderAction::SetValue { permille: 600 })
    );
    assert_eq!(slider.value(), 600);
}

// --- Slider §13 authority rendering ------------------------------------

#[test]
fn denied_slider_keeps_value_and_shows_a_lock_bead() {
    let theme = Theme::dark();
    let mut slider = Slider::new(400);
    slider.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let surface = slider_surface(&slider, &theme);
    assert!(region_has(
        &surface,
        (W - 10, W),
        (0, 10),
        premul(theme.palette().denied)
    ));
    // A denied slider ignores input and keeps its value (fail closed).
    slider.set_focused(true);
    assert_eq!(slider.on_key(Key::Named(NamedKey::Right)), None);
    assert_eq!(slider.on_pointer(&PRESS, bounds()), None);
    assert_eq!(slider.value(), 400);
}

#[test]
fn disabled_slider_is_not_actionable_and_differs_from_denied() {
    let theme = Theme::dark();
    let mut disabled = Slider::new(400);
    disabled.set_state(ControlState::disabled());
    let mut denied = Slider::new(400);
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let d = slider_surface(&disabled, &theme);
    let n = slider_surface(&denied, &theme);
    let denied_px = premul(theme.palette().denied);
    assert!(has_pixel(&n, denied_px));
    assert!(!has_pixel(&d, denied_px));
    disabled.set_focused(true);
    assert_eq!(disabled.on_key(Key::Named(NamedKey::Right)), None);
}

// --- Slider resource rail, contrast, and scale -------------------------

#[test]
fn resource_slider_uses_the_semantic_rail_colour() {
    let theme = Theme::dark();
    let mut slider = Slider::new(600);
    slider.set_state(ControlState::idle().with_pressure(PressureState::Under(PressureKind::Disk)));
    assert!(has_pixel(
        &slider_surface(&slider, &theme),
        premul(theme.palette().disk_pressure)
    ));
}

#[test]
fn high_contrast_changes_the_slider_rendering() {
    let slider = Slider::new(500);
    let normal = slider_surface(&slider, &Theme::dark());
    let heavy = slider_surface(&slider, &high_contrast());
    assert_ne!(normal.pixels(), heavy.pixels());
}

#[test]
fn slider_renders_at_a_larger_scale() {
    let theme = Theme::dark();
    let mut surface = Surface::new(W, H).expect("surface");
    let scale = Scale::from_percent(200).expect("valid scale");
    Slider::new(500).render(&mut surface, bounds(), scale, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().accent)));
}

// --- Progress trace (§11.7) --------------------------------------------

fn progress_with(activity: ActivityState) -> Progress {
    let mut progress = Progress::new();
    progress.set_state(ControlState::idle().with_activity(activity));
    progress
}

#[test]
fn known_progress_fills_proportionally_and_labels_percent() {
    let theme = Theme::dark();
    let accent = premul(theme.palette().accent);
    let quarter = progress_surface(
        &progress_with(ActivityState::Progress(ProgressValue::new(250))),
        &theme,
    );
    let most = progress_surface(
        &progress_with(ActivityState::Progress(ProgressValue::new(750))),
        &theme,
    );
    let lo = active_extent(&quarter, accent, H / 2).expect("quarter fill");
    let hi = active_extent(&most, accent, H / 2).expect("most fill");
    assert!(hi > lo);
    // The percentage caption paints foreground text on the plate.
    assert!(has_pixel(&most, premul(theme.palette().on_surface)));
}

#[test]
fn complete_progress_fills_success_and_shows_a_check_bead() {
    let theme = Theme::dark();
    let surface = progress_surface(&progress_with(ActivityState::Complete), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().success)));
}

#[test]
fn failed_progress_shows_a_recovery_rim_and_reason() {
    let theme = Theme::dark();
    let mut progress = Progress::new().with_label("Stalled");
    progress.set_state(ControlState::idle().with_recovery(RecoveryState::Hung));
    let surface = progress_surface(&progress, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().recovery)));
    // A failed trace paints no accent value fill.
    assert!(!has_pixel(&surface, premul(theme.palette().accent)));
}

#[test]
fn idle_progress_shows_only_the_groove() {
    let theme = Theme::dark();
    let surface = progress_surface(&Progress::new(), &theme);
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(!has_pixel(&surface, premul(theme.palette().accent)));
}

#[test]
fn indeterminate_trace_moves_with_phase() {
    let theme = Theme::dark();
    let mut early = progress_with(ActivityState::Indeterminate);
    early.set_phase(150);
    let mut late = progress_with(ActivityState::Indeterminate);
    late.set_phase(850);
    assert_ne!(
        progress_surface(&early, &theme).pixels(),
        progress_surface(&late, &theme).pixels()
    );
}

#[test]
fn reduced_motion_freezes_the_indeterminate_trace() {
    let theme = reduced_motion();
    let mut early = progress_with(ActivityState::Indeterminate);
    early.set_phase(150);
    let mut late = progress_with(ActivityState::Indeterminate);
    late.set_phase(850);
    assert_eq!(
        progress_surface(&early, &theme).pixels(),
        progress_surface(&late, &theme).pixels()
    );
}

#[test]
fn denied_progress_shows_a_lock_bead() {
    let theme = Theme::dark();
    let mut progress = Progress::new();
    progress.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let surface = progress_surface(&progress, &theme);
    let band = progress_thickness(&theme, Scale::ONE);
    assert!(region_has(
        &surface,
        (W - band, W),
        band_rows(&theme),
        premul(theme.palette().denied)
    ));
}

#[test]
fn progress_trace_is_a_thin_band_not_the_whole_row() {
    let theme = Theme::dark();
    let surface = progress_surface(
        &progress_with(ActivityState::Progress(ProgressValue::new(1000))),
        &theme,
    );
    let (top, bottom) = band_rows(&theme);
    assert!(bottom - top < H, "the band must be thinner than its row");
    let accent = premul(theme.palette().accent);
    assert!(region_has(&surface, (0, W), (top, bottom), accent));
    assert!(!region_has(&surface, (0, W), (0, top), accent));
    assert!(!region_has(&surface, (0, W), (bottom, H), accent));
}

#[test]
fn progress_renders_in_both_themes() {
    let p = progress_with(ActivityState::Progress(ProgressValue::new(500)));
    assert_ne!(
        progress_surface(&p, &Theme::dark()).pixels(),
        progress_surface(&p, &Theme::light()).pixels()
    );
}

#[test]
fn progress_phase_is_clamped() {
    let theme = Theme::dark();
    let mut full = progress_with(ActivityState::Indeterminate);
    full.set_phase(1000);
    let mut over = progress_with(ActivityState::Indeterminate);
    over.set_phase(60000);
    assert_eq!(
        progress_surface(&full, &theme).pixels(),
        progress_surface(&over, &theme).pixels()
    );
}

#[test]
fn progress_default_matches_new() {
    let theme = Theme::dark();
    assert_eq!(
        progress_surface(&Progress::default(), &theme).pixels(),
        progress_surface(&Progress::new(), &theme).pixels()
    );
}
