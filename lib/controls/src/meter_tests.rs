//! Unit tests for the resource-instrument family (spec §20 checklist).
//!
//! These cover the measure/layout entry point at unit and non-unit scale,
//! the honest unmeasured track (never a fabricated fill), each
//! [`PressureKind`]'s own rail colour, the [`PressureState`] emphasis
//! outline, dark/light/high-contrast coverage, reduced motion, the bounded
//! sparkline history (zero, one, many, saturated, and flat-zero series), and
//! fail-closed behaviour when a meter is drawn smaller than its content.

use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::Theme;

use crate::meter::{Meter, MeterValue, MAX_HISTORY_SAMPLES};
use crate::state::{PressureKind, PressureState, ProgressValue};
use crate::testkit::high_contrast;

const W: u32 = 96;

fn font() -> BitmapFont {
    BitmapFont::console()
}

fn premul(rgba: tairix_theme::Rgba) -> Pixel {
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

/// A theme identical to [`Theme::dark`] but with reduced motion.
fn reduced_motion() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test Reduced Motion",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        base.contrast(),
    )
}

fn meter_surface_at(meter: &Meter, theme: &Theme, scale: Scale) -> Surface {
    let h = Meter::measured_height(scale, theme, font());
    let w = scale.scale_length(W).max(1);
    let mut surface = Surface::new(w, h).expect("surface");
    meter.render(&mut surface, Rect::new(0, 0, w, h), scale, theme, font());
    surface
}

fn meter_surface(meter: &Meter, theme: &Theme) -> Surface {
    meter_surface_at(meter, theme, Scale::ONE)
}

fn measured(permille: u16) -> MeterValue {
    MeterValue::Measured(ProgressValue::new(permille))
}

// --- Layout / measure (§11 value/measured family) -----------------------

#[test]
fn measured_height_grows_with_scale() {
    let theme = Theme::dark();
    let unit = Meter::measured_height(Scale::ONE, &theme, font());
    let doubled = Meter::measured_height(
        Scale::from_percent(200).expect("valid scale"),
        &theme,
        font(),
    );
    assert!(doubled > unit, "a larger scale must need more height");
}

#[test]
fn render_fits_entirely_within_its_measured_bounds() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "62%", PressureKind::Cpu, measured(620));
    let surface = meter_surface(&meter, &theme);
    // Every element the meter draws (label, reading, track) fits within the
    // surface sized to exactly `measured_height`.
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(has_pixel(&surface, premul(theme.palette().cpu_pressure)));
}

#[test]
fn render_fits_at_a_non_unit_scale() {
    let theme = Theme::dark();
    let scale = Scale::from_percent(150).expect("valid scale");
    let meter = Meter::new("CPU", "62%", PressureKind::Cpu, measured(620));
    let surface = meter_surface_at(&meter, &theme, scale);
    assert!(has_pixel(&surface, premul(theme.palette().cpu_pressure)));
}

// --- Honest unmeasured state ---------------------------------------------

#[test]
fn unmeasured_meter_shows_only_the_quiet_track() {
    let theme = Theme::dark();
    let meter = Meter::new("NET", "—", PressureKind::Network, MeterValue::Unmeasured);
    let surface = meter_surface(&meter, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(!has_pixel(
        &surface,
        premul(theme.palette().network_activity)
    ));
}

#[test]
fn unmeasured_meter_ignores_any_supplied_samples() {
    let theme = Theme::dark();
    let meter = Meter::new("NET", "—", PressureKind::Network, MeterValue::Unmeasured)
        .with_samples([1000, 1000, 1000]);
    let surface = meter_surface(&meter, &theme);
    assert!(!has_pixel(
        &surface,
        premul(theme.palette().network_activity)
    ));
}

// --- Each PressureKind resolves its own rail colour ----------------------

#[test]
fn each_pressure_kind_resolves_its_own_rail_colour() {
    let theme = Theme::dark();
    let p = theme.palette();
    let cases = [
        (PressureKind::Cpu, p.cpu_pressure),
        (PressureKind::Memory, p.memory_pressure),
        (PressureKind::Disk, p.disk_pressure),
        (PressureKind::Network, p.network_activity),
        (PressureKind::Power, p.power_pressure),
        (PressureKind::Thermal, p.thermal_pressure),
    ];
    for (kind, expected) in cases {
        let meter = Meter::new("R", "50%", kind, measured(500));
        let surface = meter_surface(&meter, &theme);
        assert!(
            has_pixel(&surface, premul(expected)),
            "{kind:?} must fill with its own rail colour"
        );
    }
}

// --- PressureState drives the rendered emphasis --------------------------

#[test]
fn pressure_state_changes_the_rendered_emphasis() {
    let theme = Theme::dark();
    let quiet = Meter::new("CPU", "80%", PressureKind::Cpu, measured(800));
    let under = quiet
        .clone()
        .with_pressure(PressureState::Under(PressureKind::Cpu));
    assert_ne!(
        meter_surface(&quiet, &theme).pixels(),
        meter_surface(&under, &theme).pixels(),
        "pressure emphasis must change the rendered pixels"
    );
}

#[test]
fn pressure_none_is_the_default() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "80%", PressureKind::Cpu, measured(800));
    let explicit = meter.clone().with_pressure(PressureState::None);
    assert_eq!(
        meter_surface(&meter, &theme).pixels(),
        meter_surface(&explicit, &theme).pixels()
    );
}

// --- Theme coverage -------------------------------------------------------

#[test]
fn meter_renders_in_both_themes() {
    let meter = Meter::new("MEM", "8.6 GB / 16 GB", PressureKind::Memory, measured(538));
    assert_ne!(
        meter_surface(&meter, &Theme::dark()).pixels(),
        meter_surface(&meter, &Theme::light()).pixels()
    );
}

#[test]
fn high_contrast_changes_the_meter_rendering() {
    let meter = Meter::new("DISK", "72%", PressureKind::Disk, measured(720));
    let normal = meter_surface(&meter, &Theme::dark());
    let heavy = meter_surface(&meter, &high_contrast());
    assert_ne!(normal.pixels(), heavy.pixels());
}

#[test]
fn reduced_motion_does_not_change_a_static_meter() {
    let meter = Meter::new("NET", "1.3 MB/s", PressureKind::Network, measured(400))
        .with_samples([100, 300, 200, 900, 400]);
    let normal = meter_surface(&meter, &Theme::dark());
    let reduced = meter_surface(&meter, &reduced_motion());
    // Reduced motion changes only the theme's motion policy; a meter has no
    // animation of its own, so nothing else about the theme differs here
    // and the two renders match exactly.
    assert_eq!(normal.pixels(), reduced.pixels());
}

// --- Sparkline history -----------------------------------------------------

#[test]
fn no_samples_falls_back_to_the_plain_proportional_fill() {
    let theme = Theme::dark();
    let low = Meter::new("CPU", "20%", PressureKind::Cpu, measured(200));
    let high = Meter::new("CPU", "90%", PressureKind::Cpu, measured(900));
    let cpu = premul(theme.palette().cpu_pressure);
    let low_extent = rightmost(&meter_surface(&low, &theme), cpu);
    let high_extent = rightmost(&meter_surface(&high, &theme), cpu);
    assert!(high_extent > low_extent, "a higher value must fill further");
}

#[test]
fn single_sample_draws_a_visible_point() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "0%", PressureKind::Cpu, measured(0)).with_samples([0]);
    let surface = meter_surface(&meter, &theme);
    assert!(
        has_pixel(&surface, premul(theme.palette().cpu_pressure)),
        "a lone sample must still draw a point, even at value zero"
    );
}

#[test]
fn zero_samples_draws_no_sparkline() {
    let theme = Theme::dark();
    let with_empty_history = Meter::new("CPU", "50%", PressureKind::Cpu, measured(500))
        .with_samples(core::iter::empty());
    let plain = Meter::new("CPU", "50%", PressureKind::Cpu, measured(500));
    assert_eq!(
        meter_surface(&with_empty_history, &theme).pixels(),
        meter_surface(&plain, &theme).pixels(),
        "an empty history must render exactly like no history at all"
    );
}

#[test]
fn saturated_series_fills_the_full_track_height() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "100%", PressureKind::Cpu, measured(1000))
        .with_samples([1000, 1000, 1000, 1000]);
    let surface = meter_surface(&meter, &theme);
    let cpu = premul(theme.palette().cpu_pressure);
    // A fully saturated bar reaches the very top row of the track band.
    let top = surface.height() - crate::paint::progress_thickness(&theme, Scale::ONE);
    assert!(region_has(
        &surface,
        (0, surface.width()),
        (top, top + 1),
        cpu
    ));
}

#[test]
fn flat_zero_series_still_draws_minimal_points() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "0%", PressureKind::Cpu, measured(0)).with_samples([0, 0, 0, 0]);
    let surface = meter_surface(&meter, &theme);
    assert!(
        has_pixel(&surface, premul(theme.palette().cpu_pressure)),
        "a flat zero-range series must still draw its minimal bars"
    );
}

#[test]
fn samples_render_oldest_to_newest_left_to_right() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "—", PressureKind::Cpu, measured(1000)).with_samples([0, 1000]);
    let surface = meter_surface(&meter, &theme);
    let cpu = premul(theme.palette().cpu_pressure);
    let band = crate::paint::progress_thickness(&theme, Scale::ONE);
    let top = surface.height() - band;
    let mid = surface.width() / 2;
    // Only the newest (rightmost) bar reaches the very top of the band.
    assert!(!region_has(&surface, (0, mid), (top, top + 1), cpu));
    assert!(region_has(
        &surface,
        (mid, surface.width()),
        (top, top + 1),
        cpu
    ));
}

#[test]
fn sample_history_is_capped_and_keeps_the_most_recent() {
    let theme = Theme::dark();
    let long: Vec<u16> = (0..MAX_HISTORY_SAMPLES + 10)
        .map(|i| u16::try_from(i).expect("test index fits in u16"))
        .collect();
    let capped = Meter::new("CPU", "—", PressureKind::Cpu, measured(1000)).with_samples(long);
    // Rendering the over-long history must not panic and must render
    // identically to supplying only its most recent, already-bounded tail.
    let tail: Vec<u16> = (10..MAX_HISTORY_SAMPLES + 10)
        .map(|i| u16::try_from(i).expect("test index fits in u16"))
        .collect();
    let bounded = Meter::new("CPU", "—", PressureKind::Cpu, measured(1000)).with_samples(tail);
    assert_eq!(
        meter_surface(&capped, &theme).pixels(),
        meter_surface(&bounded, &theme).pixels()
    );
}

#[test]
fn sample_values_are_clamped_fail_closed() {
    let theme = Theme::dark();
    let over = Meter::new("CPU", "—", PressureKind::Cpu, measured(1000)).with_samples([5000]);
    let clamped = Meter::new("CPU", "—", PressureKind::Cpu, measured(1000)).with_samples([1000]);
    assert_eq!(
        meter_surface(&over, &theme).pixels(),
        meter_surface(&clamped, &theme).pixels()
    );
}

// --- Fail-closed clipping --------------------------------------------------

#[test]
fn a_meter_narrower_and_shorter_than_its_content_still_renders() {
    let theme = Theme::dark();
    let meter =
        Meter::new("CPU", "62%", PressureKind::Cpu, measured(620)).with_samples([100, 900, 500]);
    let mut surface = Surface::new(1, 1).expect("surface");
    // Must not panic, and must not report any pixel outside the 1x1 bound —
    // there is nowhere else for it to have drawn.
    meter.render(
        &mut surface,
        Rect::new(0, 0, 1, 1),
        Scale::ONE,
        &theme,
        font(),
    );
    assert_eq!(surface.pixels().len(), 1);
}

#[test]
fn a_meter_shorter_than_two_lines_still_renders_its_track() {
    let theme = Theme::dark();
    let meter = Meter::new("CPU", "62%", PressureKind::Cpu, measured(620));
    let band = crate::paint::progress_thickness(&theme, Scale::ONE);
    let mut surface = Surface::new(W, band).expect("surface");
    meter.render(
        &mut surface,
        Rect::new(0, 0, W, band),
        Scale::ONE,
        &theme,
        font(),
    );
    // Too short for either text line, but the track band itself still fits
    // and paints its groove.
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
}

/// The rightmost x on the track's bottom row painted `want`, if any. The
/// bottom row always lies inside the track band, since the band is the last
/// element the meter paints.
fn rightmost(surface: &Surface, want: Pixel) -> u32 {
    let y = surface.height().saturating_sub(1);
    (0..surface.width())
        .rev()
        .find(|&x| surface.get(x, y) == Some(want))
        .unwrap_or(0)
}
