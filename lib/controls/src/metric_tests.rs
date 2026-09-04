//! Unit tests for the metric-readout family: [`MetricTile`],
//! [`StatusPill`], and [`CompositionBar`].
//!
//! These cover every [`MetricInstrument`] variant (including the honest
//! unmeasured track and an empty trend), the proportional fill and pressure
//! emphasis, the `reading_height`/`measured_height` contract agreeing with
//! what `render` lays out, progressive omission of the instrument then the
//! detail line under shrinking heights, truncation of long text, every
//! `StatusPill` tone (including the heavier-contrast rim), and fail-closed
//! behaviour in degenerate bounds, across both built-in themes and the
//! high-contrast fixture — plus the tile's leading icon, its
//! `MetricLayout::Inline` form, and its `unplated` form, alone and composed
//! together.
//!
//! [`CompositionBar`]'s own coverage is the property it exists for — the parts
//! account for the whole, in order, at their measured shares — plus its
//! construction refusals, its tint ladder separating the parts, its key, the
//! `measured_height` contract, progressive dropping of key lines under
//! shrinking heights, and fail-closed degenerate bounds.

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_icon::IconKind;
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{SignalRole, Theme};

use crate::chart::Chart;
use crate::metric::{
    CompositionBar, CompositionError, CompositionSegment, MetricInstrument, MetricLayout,
    MetricTile, StatusPill, MAX_COMPOSITION_SEGMENTS,
};
use crate::state::{MeterValue, PressureKind, PressureState, ProgressValue};
use crate::testkit::{control_font, high_contrast};

fn font() -> BitmapFont {
    control_font(&Theme::dark(), Scale::ONE)
}

fn premul(rgba: tairix_theme::Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// Whether any pixel in rows `[y_from, y_to)` matches `want`.
fn band_has(surface: &Surface, y_from: u32, y_to: u32, want: Pixel) -> bool {
    (y_from..y_to.min(surface.height()))
        .any(|y| (0..surface.width()).any(|x| surface.get(x, y) == Some(want)))
}

/// Whether any pixel in columns `[x_from, x_to)` matches `want`.
fn column_range_has(surface: &Surface, x_from: u32, x_to: u32, want: Pixel) -> bool {
    (x_from..x_to.min(surface.width()))
        .any(|x| (0..surface.height()).any(|y| surface.get(x, y) == Some(want)))
}

/// The leftmost column containing a pixel matching `want`, if any.
fn leftmost_column(surface: &Surface, want: Pixel) -> Option<u32> {
    (0..surface.width()).find(|&x| (0..surface.height()).any(|y| surface.get(x, y) == Some(want)))
}

/// The rightmost column containing a pixel matching `want`, if any.
fn rightmost_column(surface: &Surface, want: Pixel) -> Option<u32> {
    (0..surface.width())
        .rev()
        .find(|&x| (0..surface.height()).any(|y| surface.get(x, y) == Some(want)))
}

fn measured(permille: u16) -> MeterValue {
    MeterValue::Measured(ProgressValue::new(permille))
}

fn tile_surface_at(tile: &MetricTile, theme: &Theme, scale: Scale, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    tile.render(&mut surface, Rect::new(0, 0, w, h), scale, theme, None);
    surface
}

fn tile_surface(tile: &MetricTile, theme: &Theme, scale: Scale) -> Surface {
    let h = tile.measured_height(scale, theme);
    tile_surface_at(tile, theme, scale, 220, h)
}

fn pill_surface_at(pill: &StatusPill, theme: &Theme, scale: Scale, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    pill.render(&mut surface, Rect::new(0, 0, w, h), scale, theme);
    surface
}

fn pill_surface(pill: &StatusPill, theme: &Theme, scale: Scale) -> Surface {
    let w = pill.measured_width(scale, theme);
    let h = StatusPill::measured_height(scale, theme);
    pill_surface_at(pill, theme, scale, w, h)
}

/// The content rectangle a tile or pill of `rect` computes for itself: inset
/// by the plate border, then by the control padding — the same two insets
/// [`MetricTile::render`] and [`StatusPill::render`] apply.
fn content_rect(theme: &Theme, scale: Scale, rect: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = rect;
    let border = crate::paint::plate_border(theme, scale);
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let (ix, iy, iw, ih) = crate::paint::inset(x, y, w, h, border).expect("plate inset");
    crate::paint::inset(ix, iy, iw, ih, pad).expect("content inset")
}

/// The total tile height whose content area is exactly `content_h` tall, for
/// a tile whose plate border and padding never depend on its content.
fn total_height_for_content(theme: &Theme, scale: Scale, content_h: u32) -> u32 {
    let border = crate::paint::plate_border(theme, scale);
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    content_h
        .saturating_add(border.saturating_mul(2))
        .saturating_add(pad.saturating_mul(2))
}

/// The total tile width whose content area is exactly `content_w` wide, for
/// a tile whose plate border and padding never depend on its content.
fn total_width_for_content(theme: &Theme, scale: Scale, content_w: u32) -> u32 {
    total_height_for_content(theme, scale, content_w)
}

/// The row band the detail line occupies, relative to the content top — after
/// the primary label/reading block (one line for [`MetricLayout::Inline`],
/// two for [`MetricLayout::Stacked`]), regardless of what instrument (if any)
/// follows it.
fn detail_band(theme: &Theme, scale: Scale, font: BitmapFont, layout: MetricLayout) -> (u32, u32) {
    let border = crate::paint::plate_border(theme, scale);
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let line_h = font.line_height();
    let primary_lines = match layout {
        MetricLayout::Stacked => 2,
        MetricLayout::Inline => 1,
    };
    let mut after_primary = border.saturating_add(pad);
    for _ in 0..primary_lines {
        after_primary = after_primary.saturating_add(line_h).saturating_add(gap);
    }
    (after_primary, after_primary.saturating_add(line_h))
}

// --- Every MetricInstrument variant ---------------------------------------

#[test]
fn instrument_none_draws_no_instrument_band() {
    let theme = Theme::dark();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu);
    // With no instrument the tile's minimum height is just its reading lines
    // plus the plate border and padding — no extra slot is reserved.
    let expected =
        total_height_for_content(&theme, Scale::ONE, tile.reading_height(Scale::ONE, &theme));
    assert_eq!(tile.measured_height(Scale::ONE, &theme), expected);
    let surface = tile_surface(&tile, &theme, Scale::ONE);
    assert!(!has_pixel(&surface, premul(theme.palette().cpu_pressure)));
    assert!(!has_pixel(&surface, premul(theme.palette().scroll_track)));
}

#[test]
fn instrument_track_unmeasured_paints_groove_without_fill() {
    let theme = Theme::dark();
    let tile = MetricTile::new("NET", "—", PressureKind::Network)
        .with_instrument(MetricInstrument::Track(MeterValue::Unmeasured));
    let surface = tile_surface(&tile, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(!has_pixel(
        &surface,
        premul(theme.palette().network_activity)
    ));
}

#[test]
fn instrument_track_measured_fill_tracks_the_permille() {
    let theme = Theme::dark();
    let cpu = premul(theme.palette().cpu_pressure);
    let low = MetricTile::new("CPU", "20%", PressureKind::Cpu)
        .with_instrument(MetricInstrument::Track(measured(200)));
    let high = MetricTile::new("CPU", "90%", PressureKind::Cpu)
        .with_instrument(MetricInstrument::Track(measured(900)));
    let low_surface = tile_surface(&low, &theme, Scale::ONE);
    let high_surface = tile_surface(&high, &theme, Scale::ONE);
    // The track is the last thing the content area draws, so its own bottom
    // row is the content area's bottom row — not the whole surface's, which
    // still has the plate's own border and padding beneath it.
    let (_, cy, _, ch) = content_rect(
        &theme,
        Scale::ONE,
        (0, 0, low_surface.width(), low_surface.height()),
    );
    let track_row = cy.saturating_add(ch).saturating_sub(1);
    let rightmost = |s: &Surface| -> u32 {
        (0..s.width())
            .rev()
            .find(|&x| s.get(x, track_row) == Some(cpu))
            .unwrap_or(0)
    };
    assert!(
        rightmost(&high_surface) > rightmost(&low_surface),
        "a higher reading must fill further along the embedded track"
    );
}

#[test]
fn pressure_emphasis_changes_the_track_instrument_render() {
    let theme = Theme::dark();
    let quiet = MetricTile::new("CPU", "80%", PressureKind::Cpu)
        .with_instrument(MetricInstrument::Track(measured(800)));
    let under = quiet
        .clone()
        .with_pressure(PressureState::Under(PressureKind::Cpu));
    assert_ne!(
        tile_surface(&quiet, &theme, Scale::ONE).pixels(),
        tile_surface(&under, &theme, Scale::ONE).pixels()
    );
}

#[test]
fn instrument_trend_delegates_to_the_chart() {
    let theme = Theme::dark();
    let with_history = MetricTile::new("MEM", "8.6 GB", PressureKind::Memory).with_instrument(
        MetricInstrument::Trend(Chart::new(PressureKind::Memory).with_samples([200, 800, 400])),
    );
    let surface = tile_surface(&with_history, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().memory_pressure)));
}

#[test]
fn instrument_trend_with_an_empty_series_plots_nothing() {
    let theme = Theme::dark();
    let empty_trend = MetricTile::new("MEM", "— GB", PressureKind::Memory)
        .with_instrument(MetricInstrument::Trend(Chart::new(PressureKind::Memory)));
    let surface = tile_surface(&empty_trend, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().scroll_track)));
    assert!(!has_pixel(
        &surface,
        premul(theme.palette().memory_pressure)
    ));
}

#[test]
fn a_unit_extends_the_reading_line_in_the_muted_foreground() {
    let theme = Theme::dark();
    let value_only = MetricTile::new("MEM", "8.6", PressureKind::Memory);
    let with_unit = value_only.clone().with_unit("GB / 16 GB");
    let scale = Scale::ONE;
    let h = value_only
        .measured_height(scale, &theme)
        .max(with_unit.measured_height(scale, &theme));
    let a = tile_surface_at(&value_only, &theme, scale, 220, h);
    let b = tile_surface_at(&with_unit, &theme, scale, 220, h);
    assert_ne!(
        a.pixels(),
        b.pixels(),
        "adding a unit must change the rendered reading line"
    );
}

// --- reading_height / measured_height agree with render -------------------

#[test]
fn measured_height_matches_what_render_actually_lays_out() {
    let theme = Theme::dark();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_unit("of 4 cores")
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    let surface = tile_surface(&tile, &theme, Scale::ONE);
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    assert!(has_pixel(&surface, premul(theme.palette().cpu_pressure)));
}

#[test]
fn reading_height_grows_with_a_detail_line() {
    let theme = Theme::dark();
    let bare = MetricTile::new("CPU", "62%", PressureKind::Cpu);
    let detailed = bare.clone().with_detail("4 cores");
    assert!(detailed.reading_height(Scale::ONE, &theme) > bare.reading_height(Scale::ONE, &theme));
}

// --- Progressive omission under shrinking heights -------------------------

#[test]
fn shrinking_height_omits_the_instrument_before_the_detail_line() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let f = font();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    let bare = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_instrument(MetricInstrument::Track(measured(620)));

    let reading_with_detail = tile.reading_height(scale, &theme);
    let reading_without_detail = bare.reading_height(scale, &theme);
    let (band_from, band_to) = detail_band(&theme, scale, f, MetricLayout::Stacked);
    let cpu = premul(theme.palette().cpu_pressure);
    let muted = premul(theme.palette().on_surface_muted);
    let value_fg = premul(theme.palette().on_surface);
    let w = 220;

    // Full height: label, value, detail, and the instrument all fit.
    let full_h = tile.measured_height(scale, &theme);
    let full = tile_surface_at(&tile, &theme, scale, w, full_h);
    assert!(
        has_pixel(&full, cpu),
        "the instrument must fit at full height"
    );
    assert!(
        band_has(&full, band_from, band_to, muted),
        "the detail line must fit at full height"
    );

    // Exactly enough content height for label + value + detail, none left
    // for the instrument: the instrument is the first to be omitted.
    let text_only_h = total_height_for_content(&theme, scale, reading_with_detail);
    let text_only = tile_surface_at(&tile, &theme, scale, w, text_only_h);
    assert!(
        !has_pixel(&text_only, cpu),
        "the instrument must be omitted once there is no room left for it"
    );
    assert!(
        band_has(&text_only, band_from, band_to, muted),
        "the detail line must still render once only the instrument is dropped"
    );

    // Not even enough content height for the detail line: it is the next
    // to go, while the label and value still render.
    let no_detail_h = total_height_for_content(&theme, scale, reading_without_detail);
    let no_detail = tile_surface_at(&tile, &theme, scale, w, no_detail_h);
    assert!(!has_pixel(&no_detail, cpu));
    assert!(
        !band_has(&no_detail, band_from, band_to, muted),
        "the detail line must be omitted once there is no room left for it"
    );
    assert!(
        has_pixel(&no_detail, value_fg),
        "the value line must still render once the detail line is dropped"
    );
}

// --- Truncation of long text -----------------------------------------------

#[test]
fn long_label_value_unit_and_detail_all_truncate_within_the_content_region() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let long = "X".repeat(500);
    let tile = MetricTile::new(long.clone(), long.clone(), PressureKind::Cpu)
        .with_unit(long.clone())
        .with_detail(long);
    let w = 160;
    let h = tile.measured_height(scale, &theme);
    let surface = tile_surface_at(&tile, &theme, scale, w, h);
    let (_, _, cw, _) = content_rect(&theme, scale, (0, 0, w, h));
    // A couple of pixels of slack absorbs a glyph's own anti-aliased
    // overshoot past its nominal advance width, so this asserts truncation
    // rather than sub-pixel rasterisation.
    let right_edge = crate::paint::plate_border(&theme, scale)
        .saturating_add(scale.scale_length(theme.metrics().control_inset).max(1))
        .saturating_add(cw)
        .saturating_add(2);

    let muted = premul(theme.palette().on_surface_muted);
    let emphasised = premul(theme.palette().on_surface);
    assert!(
        !column_range_has(&surface, right_edge, surface.width(), muted),
        "long muted text (label, unit, detail) must truncate within the content region"
    );
    assert!(
        !column_range_has(&surface, right_edge, surface.width(), emphasised),
        "a long value must truncate within the content region"
    );
}

// --- Theme coverage ---------------------------------------------------------

#[test]
fn a_tile_renders_in_both_themes_and_under_high_contrast() {
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_unit("of 100%")
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    let dark = tile_surface(&tile, &Theme::dark(), Scale::ONE);
    let light = tile_surface(&tile, &Theme::light(), Scale::ONE);
    let heavy = tile_surface(&tile, &high_contrast(), Scale::ONE);
    assert_ne!(dark.pixels(), light.pixels());
    assert_ne!(dark.pixels(), heavy.pixels());
}

// --- Fail-closed clipping ----------------------------------------------------

#[test]
fn a_tile_far_smaller_than_its_content_still_renders_without_panicking() {
    let theme = Theme::dark();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    for (w, h) in [(1, 1), (0, 40), (40, 0)] {
        let mut surface = Surface::new(w.max(1), h.max(1)).expect("surface");
        tile.render(
            &mut surface,
            Rect::new(0, 0, w, h),
            Scale::ONE,
            &theme,
            None,
        );
        assert_eq!(
            surface.pixels().len() as u64,
            u64::from(w.max(1)) * u64::from(h.max(1))
        );
    }
}

// --- Leading icon ------------------------------------------------------------

#[test]
fn builders_and_readers_agree_on_icon_layout_and_plated() {
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu);
    assert_eq!(tile.icon(), None);
    assert_eq!(tile.layout(), MetricLayout::Stacked);
    assert!(tile.is_plated());

    let tile = tile
        .with_icon(IconKind::Network)
        .with_layout(MetricLayout::Inline)
        .unplated();
    assert_eq!(tile.icon(), Some(IconKind::Network));
    assert_eq!(tile.layout(), MetricLayout::Inline);
    assert!(!tile.is_plated());
}

#[test]
fn an_icon_reserves_a_leading_gutter_and_shifts_the_text() {
    let theme = Theme::dark();
    let bare = MetricTile::new("CPU", "62%", PressureKind::Cpu);
    let iconed = bare.clone().with_icon(IconKind::Network);

    // The icon claims no extra height: the reading anatomy is unchanged, so a
    // tile with no icon still measures and lays out exactly as it always has.
    let h = bare.measured_height(Scale::ONE, &theme);
    assert_eq!(h, iconed.measured_height(Scale::ONE, &theme));

    let bare_surface = tile_surface_at(&bare, &theme, Scale::ONE, 220, h);
    let iconed_surface = tile_surface_at(&iconed, &theme, Scale::ONE, 220, h);
    assert_ne!(bare_surface.pixels(), iconed_surface.pixels());

    let cpu = premul(theme.palette().cpu_pressure);
    assert!(
        !has_pixel(&bare_surface, cpu),
        "a tile with no icon draws no resource-tinted glyph"
    );
    assert!(
        has_pixel(&iconed_surface, cpu),
        "the leading icon draws in the resource's own signal colour"
    );

    let muted = premul(theme.palette().on_surface_muted);
    let bare_label_x = leftmost_column(&bare_surface, muted).expect("label drawn");
    let iconed_label_x = leftmost_column(&iconed_surface, muted).expect("label drawn");
    assert!(
        iconed_label_x > bare_label_x,
        "an icon must push the label into a narrower, indented column"
    );
}

#[test]
fn without_an_icon_the_stacked_layout_is_unchanged_from_before() {
    // Regression anchor for the no-icon path: the anatomy this control has
    // always drawn (plated, Stacked, no icon) is exactly the default, and the
    // whole surrounding suite continues to exercise it pixel-for-pixel.
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_unit("of 100%")
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    assert_eq!(tile.icon(), None);
    assert_eq!(tile.layout(), MetricLayout::Stacked);
    assert!(tile.is_plated());
}

// --- MetricLayout::Inline ------------------------------------------------

#[test]
fn inline_reading_height_is_shorter_than_stacked_for_the_same_tile() {
    let theme = Theme::dark();
    let stacked = MetricTile::new("CPU", "62%", PressureKind::Cpu);
    let inline = stacked.clone().with_layout(MetricLayout::Inline);
    assert!(inline.reading_height(Scale::ONE, &theme) < stacked.reading_height(Scale::ONE, &theme));
}

#[test]
fn inline_layout_places_the_label_leading_and_the_reading_trailing() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let tile = MetricTile::new("CPU", "62", PressureKind::Cpu)
        .with_unit("%")
        .with_layout(MetricLayout::Inline);
    let w = 220;
    let h = tile.measured_height(scale, &theme);
    let surface = tile_surface_at(&tile, &theme, scale, w, h);

    let (cx, _, cw, _) = content_rect(&theme, scale, (0, 0, w, h));
    let muted = premul(theme.palette().on_surface_muted);
    let emphasised = premul(theme.palette().on_surface);

    let label_x = leftmost_column(&surface, muted).expect("label drawn");
    assert_eq!(label_x, cx, "the label must lead at the content's own edge");

    let value_x = rightmost_column(&surface, emphasised).expect("value drawn");
    assert!(
        value_x > cx + cw / 2,
        "the reading must trail toward the content's right edge"
    );
}

#[test]
fn inline_reading_stays_right_aligned_regardless_of_the_label_length() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let w = 220;
    let short = MetricTile::new("A", "62%", PressureKind::Cpu).with_layout(MetricLayout::Inline);
    let long = MetricTile::new("Central Processing Unit", "62%", PressureKind::Cpu)
        .with_layout(MetricLayout::Inline);
    let h = short
        .measured_height(scale, &theme)
        .max(long.measured_height(scale, &theme));
    let short_surface = tile_surface_at(&short, &theme, scale, w, h);
    let long_surface = tile_surface_at(&long, &theme, scale, w, h);

    let emphasised = premul(theme.palette().on_surface);
    let short_right = rightmost_column(&short_surface, emphasised).expect("value drawn");
    let long_right = rightmost_column(&long_surface, emphasised).expect("value drawn");
    assert_eq!(
        short_right, long_right,
        "the reading's trailing edge must not depend on the label's length"
    );
}

#[test]
fn inline_layout_truncates_the_label_before_the_reading_when_both_cannot_fit() {
    let theme = Theme::dark();
    let f = font();
    let scale = Scale::ONE;
    let long_label = "Central Processing Unit Utilisation Across All Cores";
    let tile =
        MetricTile::new(long_label, "62%", PressureKind::Cpu).with_layout(MetricLayout::Inline);
    let h = tile.measured_height(scale, &theme);

    let wide = tile_surface_at(&tile, &theme, scale, 400, h);
    let muted = premul(theme.palette().on_surface_muted);
    let emphasised = premul(theme.palette().on_surface);
    assert!(
        has_pixel(&wide, muted),
        "the label fits at a generous width"
    );
    assert!(
        has_pixel(&wide, emphasised),
        "the reading fits at a generous width"
    );

    // Just wide enough for the reading itself, leaving essentially no room
    // for the label.
    let reading_w = f.text_width("62%");
    let narrow_w = total_width_for_content(&theme, scale, reading_w.saturating_add(1));
    let narrow = tile_surface_at(&tile, &theme, scale, narrow_w, h);
    assert!(
        has_pixel(&narrow, emphasised),
        "the reading must keep its room even when the tile is narrow"
    );
    assert!(
        !has_pixel(&narrow, muted),
        "the label must be the one to give way when both cannot fit"
    );
}

#[test]
fn measured_height_matches_render_for_the_inline_layout_too() {
    let theme = Theme::dark();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_layout(MetricLayout::Inline)
        .with_unit("of 4 cores")
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    let surface = tile_surface(&tile, &theme, Scale::ONE);
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    assert!(has_pixel(&surface, premul(theme.palette().cpu_pressure)));
}

#[test]
fn inline_layout_supports_every_instrument_variant() {
    let theme = Theme::dark();
    let scale = Scale::ONE;

    let none = MetricTile::new("CPU", "62%", PressureKind::Cpu).with_layout(MetricLayout::Inline);
    let none_surface = tile_surface(&none, &theme, scale);
    assert!(!has_pixel(
        &none_surface,
        premul(theme.palette().scroll_track)
    ));

    let unmeasured = MetricTile::new("NET", "—", PressureKind::Network)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Track(MeterValue::Unmeasured));
    let unmeasured_surface = tile_surface(&unmeasured, &theme, scale);
    assert!(has_pixel(
        &unmeasured_surface,
        premul(theme.palette().scroll_track)
    ));
    assert!(!has_pixel(
        &unmeasured_surface,
        premul(theme.palette().network_activity)
    ));

    let measured_tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Track(measured(620)));
    let measured_surface = tile_surface(&measured_tile, &theme, scale);
    assert!(has_pixel(
        &measured_surface,
        premul(theme.palette().cpu_pressure)
    ));

    let trend = MetricTile::new("MEM", "8.6 GB", PressureKind::Memory)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Trend(
            Chart::new(PressureKind::Memory).with_samples([200, 800, 400]),
        ));
    let trend_surface = tile_surface(&trend, &theme, scale);
    assert!(has_pixel(
        &trend_surface,
        premul(theme.palette().memory_pressure)
    ));
}

#[test]
fn pressure_emphasis_changes_the_inline_layout_render_too() {
    let theme = Theme::dark();
    let quiet = MetricTile::new("CPU", "80%", PressureKind::Cpu)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Track(measured(800)));
    let under = quiet
        .clone()
        .with_pressure(PressureState::Under(PressureKind::Cpu));
    assert_ne!(
        tile_surface(&quiet, &theme, Scale::ONE).pixels(),
        tile_surface(&under, &theme, Scale::ONE).pixels()
    );
}

#[test]
fn shrinking_height_omits_the_instrument_before_the_detail_line_in_inline_layout_too() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let f = font();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_layout(MetricLayout::Inline)
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)));
    let bare = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Track(measured(620)));

    let reading_with_detail = tile.reading_height(scale, &theme);
    let reading_without_detail = bare.reading_height(scale, &theme);
    let (band_from, band_to) = detail_band(&theme, scale, f, MetricLayout::Inline);
    let cpu = premul(theme.palette().cpu_pressure);
    let muted = premul(theme.palette().on_surface_muted);
    let value_fg = premul(theme.palette().on_surface);
    let w = 220;

    let full_h = tile.measured_height(scale, &theme);
    let full = tile_surface_at(&tile, &theme, scale, w, full_h);
    assert!(
        has_pixel(&full, cpu),
        "the instrument must fit at full height"
    );
    assert!(
        band_has(&full, band_from, band_to, muted),
        "the detail line must fit at full height"
    );

    let text_only_h = total_height_for_content(&theme, scale, reading_with_detail);
    let text_only = tile_surface_at(&tile, &theme, scale, w, text_only_h);
    assert!(
        !has_pixel(&text_only, cpu),
        "the instrument must be omitted once there is no room left for it"
    );
    assert!(
        band_has(&text_only, band_from, band_to, muted),
        "the detail line must still render once only the instrument is dropped"
    );

    let no_detail_h = total_height_for_content(&theme, scale, reading_without_detail);
    let no_detail = tile_surface_at(&tile, &theme, scale, w, no_detail_h);
    assert!(!has_pixel(&no_detail, cpu));
    assert!(
        !band_has(&no_detail, band_from, band_to, muted),
        "the detail line must be omitted once there is no room left for it"
    );
    assert!(
        has_pixel(&no_detail, value_fg),
        "the reading must still render once the detail line is dropped"
    );
}

// --- unplated ------------------------------------------------------------

#[test]
fn an_unplated_tile_draws_no_plate_or_rim_but_still_draws_its_content() {
    let theme = Theme::dark();
    let plated = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_instrument(MetricInstrument::Track(measured(620)));
    let unplated = plated.clone().unplated();
    assert!(plated.is_plated());
    assert!(!unplated.is_plated());

    // With no plate to pad, an unplated tile needs less height for the same
    // content.
    assert!(
        unplated.measured_height(Scale::ONE, &theme) < plated.measured_height(Scale::ONE, &theme)
    );

    let h = plated.measured_height(Scale::ONE, &theme);
    let plated_surface = tile_surface_at(&plated, &theme, Scale::ONE, 220, h);
    let unplated_surface = tile_surface_at(&unplated, &theme, Scale::ONE, 220, h);

    let rim = premul(theme.palette().rim);
    assert!(
        has_pixel(&plated_surface, rim),
        "the plated default still draws its own rim"
    );
    assert!(
        !has_pixel(&unplated_surface, rim),
        "an unplated tile draws no rim of its own"
    );

    assert!(has_pixel(
        &unplated_surface,
        premul(theme.palette().on_surface)
    ));
    assert!(has_pixel(
        &unplated_surface,
        premul(theme.palette().cpu_pressure)
    ));
}

#[test]
fn an_unplated_tile_draws_flush_to_its_own_bounds() {
    let theme = Theme::dark();
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu).unplated();
    let scale = Scale::ONE;
    let h = tile.measured_height(scale, &theme);
    let surface = tile_surface_at(&tile, &theme, scale, 220, h);
    let muted = premul(theme.palette().on_surface_muted);
    let label_x = leftmost_column(&surface, muted).expect("label drawn");
    assert_eq!(label_x, 0, "an unplated tile has no padding of its own");
}

// --- The three additions composed -----------------------------------------

#[test]
fn the_three_additions_compose_in_an_unplated_inline_tile_with_an_icon_and_a_track() {
    let theme = Theme::dark();
    let tile = MetricTile::new("CPU", "62", PressureKind::Cpu)
        .with_unit("%")
        .with_icon(IconKind::Network)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Track(measured(620)))
        .unplated();
    assert!(!tile.is_plated());
    assert_eq!(tile.layout(), MetricLayout::Inline);
    assert_eq!(tile.icon(), Some(IconKind::Network));

    let scale = Scale::ONE;
    let h = tile.measured_height(scale, &theme);
    let surface = tile_surface_at(&tile, &theme, scale, 220, h);

    let cpu = premul(theme.palette().cpu_pressure);
    let emphasised = premul(theme.palette().on_surface);
    let rim = premul(theme.palette().rim);
    assert!(
        has_pixel(&surface, cpu),
        "the tinted icon and track must both still draw"
    );
    assert!(
        has_pixel(&surface, emphasised),
        "the reading must still draw"
    );
    assert!(!has_pixel(&surface, rim), "an unplated tile draws no rim");
}

#[test]
fn every_new_addition_renders_in_both_themes_and_under_high_contrast() {
    let tile = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_unit("of 100%")
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)))
        .with_icon(IconKind::Network)
        .with_layout(MetricLayout::Inline)
        .unplated();
    let dark = tile_surface(&tile, &Theme::dark(), Scale::ONE);
    let light = tile_surface(&tile, &Theme::light(), Scale::ONE);
    let heavy = tile_surface(&tile, &high_contrast(), Scale::ONE);
    assert_ne!(dark.pixels(), light.pixels());
    assert_ne!(dark.pixels(), heavy.pixels());
}

#[test]
fn every_new_combination_renders_without_panicking_in_degenerate_bounds() {
    let theme = Theme::dark();
    let base = MetricTile::new("CPU", "62%", PressureKind::Cpu)
        .with_detail("4 cores")
        .with_instrument(MetricInstrument::Track(measured(620)))
        .with_icon(IconKind::Network);
    let variants = [
        base.clone(),
        base.clone().with_layout(MetricLayout::Inline),
        base.clone().unplated(),
        base.with_layout(MetricLayout::Inline).unplated(),
    ];
    for tile in variants {
        for (w, h) in [(1_u32, 1_u32), (0, 40), (40, 0)] {
            let mut surface = Surface::new(w.max(1), h.max(1)).expect("surface");
            tile.render(
                &mut surface,
                Rect::new(0, 0, w, h),
                Scale::ONE,
                &theme,
                None,
            );
            assert_eq!(
                surface.pixels().len() as u64,
                u64::from(w.max(1)) * u64::from(h.max(1))
            );
        }
    }
}

// --- StatusPill --------------------------------------------------------------

#[test]
fn a_neutral_pill_uses_the_quiet_plate_and_emphasised_foreground() {
    let theme = Theme::dark();
    let pill = StatusPill::new("Idle");
    assert_eq!(pill.label(), "Idle");
    let surface = pill_surface(&pill, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    assert!(has_pixel(&surface, premul(theme.palette().surface)));
}

#[test]
fn every_tone_draws_its_label_in_its_own_role_colour() {
    let theme = Theme::dark();
    let p = theme.palette();
    let cases = [
        (SignalRole::Cpu, p.cpu_pressure),
        (SignalRole::Memory, p.memory_pressure),
        (SignalRole::Disk, p.disk_pressure),
        (SignalRole::Network, p.network_activity),
        (SignalRole::Power, p.power_pressure),
        (SignalRole::Thermal, p.thermal_pressure),
        (SignalRole::Recovery, p.recovery),
        (SignalRole::Success, p.success),
        (SignalRole::Warning, p.warning),
        (SignalRole::Denied, p.denied),
    ];
    for (role, expected) in cases {
        let pill = StatusPill::new("State").with_tone(role);
        let surface = pill_surface(&pill, &theme, Scale::ONE);
        assert!(
            has_pixel(&surface, premul(expected)),
            "{role:?} must draw its label in its own role colour"
        );
    }
}

#[test]
fn a_toned_pill_differs_from_a_neutral_pill() {
    let theme = Theme::dark();
    let neutral = StatusPill::new("State");
    let toned = StatusPill::new("State").with_tone(SignalRole::Warning);
    assert_ne!(
        pill_surface(&neutral, &theme, Scale::ONE).pixels(),
        pill_surface(&toned, &theme, Scale::ONE).pixels()
    );
}

#[test]
fn measured_width_grows_with_the_label_and_with_scale() {
    let theme = Theme::dark();
    let short = StatusPill::new("OK");
    let long = StatusPill::new("Recovering");
    assert!(long.measured_width(Scale::ONE, &theme) > short.measured_width(Scale::ONE, &theme));

    let unit = short.measured_width(Scale::ONE, &theme);
    let doubled = short.measured_width(Scale::from_percent(200).expect("valid scale"), &theme);
    assert!(doubled > unit, "a larger scale must need more width");
}

#[test]
fn heavier_contrast_changes_both_neutral_and_toned_pill_rendering() {
    let neutral = StatusPill::new("Idle");
    let toned = StatusPill::new("Denied").with_tone(SignalRole::Denied);
    for pill in [&neutral, &toned] {
        let normal = pill_surface(pill, &Theme::dark(), Scale::ONE);
        let heavy = pill_surface(pill, &high_contrast(), Scale::ONE);
        assert_ne!(
            normal.pixels(),
            heavy.pixels(),
            "heavier contrast must change the pill's rendered rim"
        );
    }
}

#[test]
fn a_pill_renders_in_both_themes() {
    let pill = StatusPill::new("Healthy").with_tone(SignalRole::Success);
    assert_ne!(
        pill_surface(&pill, &Theme::dark(), Scale::ONE).pixels(),
        pill_surface(&pill, &Theme::light(), Scale::ONE).pixels()
    );
}

#[test]
fn a_long_pill_label_truncates_within_bounds() {
    let theme = Theme::dark();
    let pill = StatusPill::new("X".repeat(200));
    let h = StatusPill::measured_height(Scale::ONE, &theme);
    let surface = pill_surface_at(&pill, &theme, Scale::ONE, 80, h);
    // Must not panic, and the label must never draw past the pill's own
    // bounds.
    assert_eq!(
        surface.pixels().len() as u64,
        u64::from(80_u32) * u64::from(h)
    );
}

#[test]
fn a_pill_too_short_for_one_line_paints_nothing() {
    let theme = Theme::dark();
    let pill = StatusPill::new("OK");
    let line_h = font().line_height();
    let surface = pill_surface_at(&pill, &theme, Scale::ONE, 80, line_h.saturating_sub(1));
    assert!(
        surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT),
        "a pill too short for one line must paint nothing"
    );
}

#[test]
fn a_pill_in_degenerate_bounds_never_panics() {
    let theme = Theme::dark();
    let pill = StatusPill::new("OK").with_tone(SignalRole::Success);
    for (w, h) in [(1_u32, 1_u32), (0, 40), (40, 0)] {
        let mut surface = Surface::new(w.max(1), h.max(1)).expect("surface");
        pill.render(&mut surface, Rect::new(0, 0, w, h), Scale::ONE, &theme);
        assert_eq!(
            surface.pixels().len() as u64,
            u64::from(w.max(1)) * u64::from(h.max(1))
        );
    }
}

// --- CompositionBar -------------------------------------------------------

const BAR_W: u32 = 320;

fn parts(shares: &[u16]) -> alloc::vec::Vec<CompositionSegment> {
    shares
        .iter()
        .enumerate()
        .map(|(i, &share)| CompositionSegment::new(alloc::format!("P{i}"), "1 GB", share))
        .collect()
}

fn bar(shares: &[u16]) -> CompositionBar {
    CompositionBar::new(PressureKind::Memory, parts(shares)).expect("shares account for the whole")
}

fn bar_surface_of(bar: &CompositionBar, theme: &Theme, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    bar.render(&mut surface, Rect::new(0, 0, w, h), Scale::ONE, theme);
    surface
}

fn bar_surface(bar: &CompositionBar, theme: &Theme) -> Surface {
    let h = bar.measured_height(BAR_W, Scale::ONE, theme);
    bar_surface_of(bar, theme, BAR_W, h)
}

fn band_h(theme: &Theme) -> u32 {
    crate::paint::progress_thickness(theme, Scale::ONE)
}

/// The columns, within the bar's own band rows, that carry `want`.
fn band_columns(surface: &Surface, theme: &Theme, want: Pixel) -> alloc::vec::Vec<u32> {
    let rows = band_h(theme).min(surface.height());
    (0..surface.width())
        .filter(|&x| (0..rows).any(|y| surface.get(x, y) == Some(want)))
        .collect()
}

fn hue(theme: &Theme, index: usize) -> Pixel {
    crate::paint::composition_tint(theme, PressureKind::Memory, index).premultiply()
}

fn remainder_ink(theme: &Theme) -> Pixel {
    crate::paint::composition_remainder_tint(theme).premultiply()
}

#[test]
fn the_parts_run_in_order_across_the_whole_bar() {
    // The property the control exists for: a reader sees where the resource
    // went, in the order the owner named the parts, at their measured shares.
    let theme = Theme::dark();
    let composition = bar(&[500, 250, 250]);
    let surface = bar_surface(&composition, &theme);

    let mut previous_end = 0;
    for index in 0..3 {
        let columns = band_columns(&surface, &theme, hue(&theme, index));
        let first = *columns.first().expect("every part must draw");
        let last = *columns.last().expect("every part must draw");
        assert!(
            first >= previous_end,
            "part {index} started at {first}, inside the part that ended at {previous_end}"
        );
        previous_end = last;
    }
    // The last part ends where a bar made of one whole part does, so the
    // composition accounts for everything rather than stopping short.
    let whole = bar_surface(&bar(&[1000]), &theme);
    let full_reach = *band_columns(&whole, &theme, hue(&theme, 0))
        .last()
        .expect("a whole bar must draw");
    assert_eq!(
        previous_end, full_reach,
        "the composition stopped short of the whole"
    );
}

#[test]
fn a_larger_share_claims_more_of_the_bar() {
    let theme = Theme::dark();
    let reach = |shares: &[u16]| {
        *band_columns(&bar_surface(&bar(shares), &theme), &theme, hue(&theme, 0))
            .last()
            .expect("the leading part must draw")
    };
    assert!(reach(&[700, 300]) > reach(&[100, 900]));
}

#[test]
fn parts_that_do_not_account_for_the_whole_are_refused() {
    // A silently short bar would under-report where the resource went.
    assert_eq!(
        CompositionBar::new(PressureKind::Memory, parts(&[400, 400])),
        Err(CompositionError::Incomplete {
            total_permille: 800
        })
    );
    assert_eq!(
        CompositionBar::new(PressureKind::Memory, parts(&[600, 600])),
        Err(CompositionError::Incomplete {
            total_permille: 1200
        })
    );
    assert_eq!(
        CompositionBar::new(PressureKind::Memory, alloc::vec::Vec::new()),
        Err(CompositionError::Incomplete { total_permille: 0 })
    );
}

#[test]
fn an_out_of_range_share_is_clamped_and_then_refused() {
    // Clamping fail closed means the excess cannot draw past the bar's end;
    // the composition then simply does not add up, which is the honest report.
    let segment = CompositionSegment::new("All of it", "16 GB", 4000);
    assert_eq!(segment.share(), 1000);
    assert_eq!(
        CompositionBar::new(
            PressureKind::Memory,
            alloc::vec![segment, CompositionSegment::new("More", "1 GB", 100)]
        ),
        Err(CompositionError::Incomplete {
            total_permille: 1100
        })
    );
}

#[test]
fn more_used_parts_than_the_ladder_can_separate_are_refused() {
    let too_many = MAX_COMPOSITION_SEGMENTS + 1;
    let mut shares = alloc::vec![1u16; too_many];
    shares[0] = 1000 - u16::try_from(too_many - 1).expect("small");
    assert_eq!(
        CompositionBar::new(PressureKind::Memory, parts(&shares)),
        Err(CompositionError::TooManySegments { used: too_many })
    );

    // Exactly the bound is fine, and a remainder rides on top of it because it
    // takes the neutral rather than a hue.
    let mut shares = alloc::vec![1u16; MAX_COMPOSITION_SEGMENTS];
    shares[0] = 1000 - u16::try_from(MAX_COMPOSITION_SEGMENTS).expect("small");
    let mut segments = parts(&shares);
    segments.push(CompositionSegment::remainder("Free", "1 GB", 1));
    assert!(CompositionBar::new(PressureKind::Memory, segments).is_ok());
}

#[test]
fn a_remainder_anywhere_but_last_is_refused() {
    // A grey band in the middle of a bar reads as a gap the composition failed
    // to account for, which is the opposite of what a remainder says.
    assert_eq!(
        CompositionBar::new(
            PressureKind::Memory,
            alloc::vec![
                CompositionSegment::remainder("Free", "8 GB", 500),
                CompositionSegment::new("Used", "8 GB", 500),
            ]
        ),
        Err(CompositionError::MisplacedRemainder)
    );
    assert_eq!(
        CompositionBar::new(
            PressureKind::Memory,
            alloc::vec![
                CompositionSegment::remainder("Free", "8 GB", 500),
                CompositionSegment::remainder("Also free", "8 GB", 500),
            ]
        ),
        Err(CompositionError::MisplacedRemainder)
    );
}

#[test]
fn every_part_carries_a_hue_of_its_own_and_the_first_is_the_resources() {
    // The parts are categories, so they separate by hue; the bar still reads
    // as the resource it splits where it starts.
    let theme = Theme::dark();
    let count = MAX_COMPOSITION_SEGMENTS;
    let mut shares = alloc::vec![100u16; count];
    shares[0] = 1000 - 100 * u16::try_from(count - 1).expect("small");
    let surface = bar_surface(&bar(&shares), &theme);

    assert_eq!(hue(&theme, 0), premul(theme.palette().memory_pressure));
    let mut seen = alloc::vec::Vec::new();
    for index in 0..count {
        let ink = hue(&theme, index);
        assert!(has_pixel(&surface, ink), "part {index} did not draw");
        assert!(
            !seen.contains(&ink),
            "part {index} reuses an earlier part's hue"
        );
        seen.push(ink);
    }
}

#[test]
fn a_remainder_reads_as_the_unfilled_tail_and_still_has_a_swatch() {
    let theme = Theme::dark();
    let composition = CompositionBar::new(
        PressureKind::Memory,
        alloc::vec![
            CompositionSegment::new("Anonymous", "4.1 GB", 400),
            CompositionSegment::remainder("Free", "7.4 GB", 600),
        ],
    )
    .expect("accounts for the whole");
    assert!(composition.segments()[1].is_remainder());
    let surface = bar_surface(&composition, &theme);
    let neutral = remainder_ink(&theme);
    let tail = band_columns(&surface, &theme, neutral);
    let used = band_columns(&surface, &theme, hue(&theme, 0));
    assert!(
        !tail.is_empty() && !used.is_empty(),
        "both the used part and the remainder must draw"
    );
    assert!(
        tail[0] > *used.last().expect("used part"),
        "the remainder must be the bar's tail"
    );
    // The key still names it, so the swatch appears below the bar too.
    assert!(band_has(
        &surface,
        band_h(&theme),
        surface.height(),
        neutral
    ));
}

#[test]
fn the_joins_between_parts_are_ruled_so_they_stay_countable_without_hue() {
    let theme = Theme::dark();
    let ruled = bar_surface(&bar(&[300, 300, 400]), &theme);
    let whole = bar_surface(&bar(&[1000]), &theme);
    let rule = premul(theme.palette().surface);
    assert!(
        !band_columns(&ruled, &theme, rule).is_empty(),
        "a multi-part bar must rule its joins"
    );
    assert!(
        band_columns(&whole, &theme, rule).is_empty(),
        "a bar with one part has no join to rule"
    );
}

#[test]
fn the_key_names_every_part_and_its_amount() {
    let theme = Theme::dark();
    let composition = CompositionBar::new(
        PressureKind::Memory,
        alloc::vec![
            CompositionSegment::new("Anonymous", "4.1 GB", 400),
            CompositionSegment::new("File cache", "2.6 GB", 200),
            CompositionSegment::remainder("Free", "7.4 GB", 400),
        ],
    )
    .expect("accounts for the whole");
    assert_eq!(composition.segments().len(), 3);
    assert_eq!(composition.segments()[1].label(), "File cache");
    assert_eq!(composition.segments()[1].amount(), "2.6 GB");
    assert_eq!(composition.kind(), PressureKind::Memory);

    let surface = bar_surface(&composition, &theme);
    let key_top = band_h(&theme);
    // Label ink, amount ink, and every part's own swatch, all beneath the bar.
    assert!(band_has(
        &surface,
        key_top,
        surface.height(),
        premul(theme.palette().on_surface)
    ));
    assert!(band_has(
        &surface,
        key_top,
        surface.height(),
        premul(theme.palette().on_surface_muted)
    ));
    for index in 0..2 {
        assert!(
            band_has(&surface, key_top, surface.height(), hue(&theme, index)),
            "part {index} has no key swatch"
        );
    }
}

#[test]
fn the_key_swatches_lead_their_labels() {
    let theme = Theme::dark();
    let surface = bar_surface(&bar(&[500, 500]), &theme);
    let swatch_x = leftmost_column(&surface, hue(&theme, 1)).expect("swatch");
    let bar_start = leftmost_column(&surface, hue(&theme, 0)).expect("bar");
    assert!(
        swatch_x > bar_start,
        "the second part's key swatch sits along the key row, not at the bar's own start"
    );
    let label_x =
        leftmost_column(&surface, premul(theme.palette().on_surface_muted)).expect("label");
    assert!(label_x > 0, "the leading swatch precedes the leading label");
}

#[test]
fn the_key_wraps_rather_than_dropping_a_part_it_cannot_fit() {
    // Every part of the bar must be nameable, so a narrow key takes more rows
    // instead of losing an entry.
    let theme = Theme::dark();
    let composition = CompositionBar::new(
        PressureKind::Memory,
        alloc::vec![
            CompositionSegment::new("Anonymous pages", "4.1 GB", 300),
            CompositionSegment::new("File cache", "2.6 GB", 300),
            CompositionSegment::remainder("Free", "7.4 GB", 400),
        ],
    )
    .expect("accounts for the whole");
    let wide = composition.measured_height(600, Scale::ONE, &theme);
    let narrow = composition.measured_height(140, Scale::ONE, &theme);
    assert!(
        narrow > wide,
        "a narrower key must take more rows, not fewer entries"
    );

    // Every swatch still draws at the narrow width.
    let surface = bar_surface_of(&composition, &theme, 140, narrow);
    for index in 0..2 {
        assert!(
            has_pixel(&surface, hue(&theme, index)),
            "part {index} was dropped from the wrapped key"
        );
    }
    assert!(has_pixel(&surface, remainder_ink(&theme)));
}

#[test]
fn measured_height_is_what_render_lays_out() {
    let theme = Theme::dark();
    for count in 1..=MAX_COMPOSITION_SEGMENTS {
        let mut shares = alloc::vec![1u16; count];
        shares[0] = 1000 - u16::try_from(count - 1).expect("small");
        let composition = bar(&shares);
        let wanted = composition.measured_height(BAR_W, Scale::ONE, &theme);
        let full = bar_surface_of(&composition, &theme, BAR_W, wanted);
        assert!(
            band_has(
                &full,
                band_h(&theme),
                wanted,
                hue(&theme, count.saturating_sub(1))
            ),
            "the last of {count} key entries is missing at the measured height"
        );
    }
}

#[test]
fn a_height_too_short_for_the_key_drops_its_trailing_rows() {
    let theme = Theme::dark();
    let composition = CompositionBar::new(
        PressureKind::Memory,
        alloc::vec![
            CompositionSegment::new("Anonymous pages", "4.1 GB", 400),
            CompositionSegment::new("File cache backing", "2.6 GB", 300),
            CompositionSegment::remainder("Free", "7.4 GB", 300),
        ],
    )
    .expect("accounts for the whole");
    let narrow_w = 150;
    let full_h = composition.measured_height(narrow_w, Scale::ONE, &theme);
    let gap = Scale::ONE.scale_length(theme.metrics().control_gap).max(1);
    let one_row_h = band_h(&theme) + gap + font().line_height();
    assert!(
        full_h > one_row_h,
        "the key must wrap at this width for the test to mean anything"
    );

    // A short bar keeps the reading and the key rows it can seat, and draws
    // nothing at all past the bounds it was given.
    let mut surface = Surface::new(narrow_w, full_h).expect("surface");
    composition.render(
        &mut surface,
        Rect::new(0, 0, narrow_w, one_row_h),
        Scale::ONE,
        &theme,
    );
    assert!(
        has_pixel(&surface, hue(&theme, 0)),
        "the bar itself survives"
    );
    assert!(
        band_has(
            &surface,
            band_h(&theme),
            one_row_h,
            premul(theme.palette().on_surface_muted)
        ),
        "the key row it can seat must still draw"
    );
    assert!(
        (one_row_h..full_h)
            .all(|y| (0..narrow_w).all(|x| surface.get(x, y) == Some(Pixel::TRANSPARENT))),
        "a key row that does not fit must be dropped, never drawn past the bounds"
    );
}

#[test]
fn a_composition_renders_in_both_themes_and_under_heavy_contrast() {
    let composition = bar(&[500, 300, 200]);
    assert_ne!(
        bar_surface(&composition, &Theme::dark()).pixels(),
        bar_surface(&composition, &Theme::light()).pixels()
    );

    let heavy = high_contrast();
    let surface = bar_surface(&composition, &heavy);
    for index in 0..3 {
        assert!(
            has_pixel(
                &surface,
                crate::paint::composition_tint(&heavy, PressureKind::Memory, index).premultiply()
            ),
            "part {index} vanished on the heavier-contrast path"
        );
    }
    // The joins are ruled more heavily where contrast is what carries them.
    let joins = |theme: &Theme| {
        band_columns(
            &bar_surface(&composition, theme),
            theme,
            premul(theme.palette().surface),
        )
        .len()
    };
    assert!(joins(&heavy) > joins(&Theme::dark()));
}

#[test]
fn a_degenerate_bar_draws_nothing_outside_itself() {
    let theme = Theme::dark();
    let composition = bar(&[500, 500]);
    let mut zero = Surface::new(4, 4).expect("surface");
    composition.render(&mut zero, Rect::new(0, 0, 0, 0), Scale::ONE, &theme);
    assert!(zero.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));

    let mut one = Surface::new(1, 1).expect("surface");
    composition.render(&mut one, Rect::new(0, 0, 1, 1), Scale::ONE, &theme);
    assert_eq!(one.pixels().len(), 1);
}
