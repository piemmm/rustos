//! The [`Meter`] control: one resource reading, shown as a rounded track
//! tinted by its resource's semantic rail colour, with an optional history
//! sparkline (`plans/NEW-TASKBAR.md` T11, `plans/GUI-CONTROLS-DESIGN.md`
//! value/measured family).
//!
//! A meter is a read-only instrument, like [`Progress`](crate::Progress): it
//! carries a label, a display reading, and a validated fraction (or an
//! honest "cannot be measured" state) and never accepts pointer or keyboard
//! input. Unlike [`Slider`](crate::Slider) or [`Progress`](crate::Progress),
//! whose fill colour is the plain accent unless the owner marks the object as
//! genuinely under pressure, a meter's fill is *always* tinted by the
//! resource it represents — a CPU meter reads as the compute colour whether
//! it shows 5% or 95% — because that tint is the meter's own identity, not a
//! transient severity. The [`PressureState`] the owner supplies still drives
//! emphasis exactly as the shared Pressure Rail does elsewhere: at rest the
//! meter shows the plain tinted track, and under genuine pressure it gains
//! the same rail-thickness emphasis outline a card or row would show, so
//! nothing here invents a second severity vocabulary.
//!
//! Every colour, radius, thickness, gap, and font metric resolves from the
//! active [`Theme`] and [`Scale`] through the shared accessors the value
//! family already uses (`crate::paint`), and the rounded track itself is the
//! same groove/band recipe [`Slider`](crate::Slider) and
//! [`Progress`](crate::Progress) draw from, so a meter never carries its own
//! copy of that geometry.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    clamp_permille, draw_outline, progress_thickness, rail_thickness, signal_color, surface_rect,
    to_i32, FULL,
};
use crate::state::{PressureKind, PressureState, ProgressValue};

/// The most samples a [`Meter`]'s sparkline history may hold.
///
/// The sparkline draws inside one meter's own track band, so a series far
/// beyond a small window could never resolve to individually visible bars;
/// bounding it here keeps the history a small, owner-controlled window
/// rather than an unbounded log the render path would have to skip past.
pub const MAX_HISTORY_SAMPLES: usize = 64;

/// A meter's reading: a validated fraction, or an honest "cannot currently be
/// measured" state.
///
/// A resource with no wired query or a denied capability must never render as
/// a fabricated `0%` — that tells the reader "idle" when the truth is
/// "unknown". Modelling the two as separate variants, rather than reusing `0`
/// for both, makes that misrepresentation unrepresentable: a caller can never
/// accidentally construct a [`Meter`] that looks like a real empty reading
/// when it has none.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MeterValue {
    /// A validated fraction of the resource's capacity, reused from the
    /// [`Progress`](crate::Progress) family's own known-work value so the
    /// permille validation is never restated.
    Measured(ProgressValue),
    /// The resource cannot currently be measured. The meter renders only the
    /// quiet unmeasured track, never a filled one.
    Unmeasured,
}

/// One resource reading: a label, a display reading, and a measured rounded
/// track tinted by the resource's semantic rail colour, with an optional
/// bounded history sparkline (spec §11 value/measured family).
///
/// A meter is an instrument, not an action: it has no pointer or keyboard
/// handling and reports nothing back to its owner. The owner supplies every
/// visible fact (label, reading text, resource kind, pressure emphasis,
/// value, and history) and re-renders when any of them changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Meter {
    label: String,
    reading: String,
    kind: PressureKind,
    pressure: PressureState,
    value: MeterValue,
    samples: Vec<u16>,
}

impl Meter {
    /// A meter labelled `label`, showing `reading` as its display text, for
    /// the resource `kind`, at `value` (or [`MeterValue::Unmeasured`] when
    /// the resource cannot currently be read). The meter starts with no
    /// pressure emphasis and no history; add either with
    /// [`with_pressure`](Self::with_pressure) or
    /// [`with_samples`](Self::with_samples).
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        reading: impl Into<String>,
        kind: PressureKind,
        value: MeterValue,
    ) -> Self {
        Self {
            label: label.into(),
            reading: reading.into(),
            kind,
            pressure: PressureState::None,
            value,
            samples: Vec::new(),
        }
    }

    /// This meter with the given pressure emphasis, as used by
    /// [`Card`](crate::Card)'s Pressure Rail: [`PressureState::Under`] draws
    /// the emphasis outline around the track; [`PressureState::None`] leaves
    /// the plain tinted instrument.
    #[must_use]
    pub fn with_pressure(mut self, pressure: PressureState) -> Self {
        self.pressure = pressure;
        self
    }

    /// This meter with an oldest-to-newest sparkline history. Each sample is
    /// a permille fraction, clamped fail closed; the series is capped to the
    /// most recent [`MAX_HISTORY_SAMPLES`], dropping the oldest first.
    #[must_use]
    pub fn with_samples(mut self, samples: impl IntoIterator<Item = u16>) -> Self {
        let mut buf: Vec<u16> = samples.into_iter().map(clamp_permille).collect();
        if buf.len() > MAX_HISTORY_SAMPLES {
            let drop = buf.len() - MAX_HISTORY_SAMPLES;
            buf.drain(..drop);
        }
        self.samples = buf;
        self
    }

    /// The minimum height, in physical pixels, one meter needs at `scale` to
    /// draw its label, reading, and track without clipping.
    ///
    /// A header band lays several meters out in one row sharing a single row
    /// height; this is the one place that height is computed, so the row and
    /// each meter's own [`render`](Self::render) agree on it rather than each
    /// guessing independently.
    #[must_use]
    pub fn measured_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let band = progress_thickness(theme, scale);
        let line_h = font.line_height();
        line_h
            .saturating_add(gap)
            .saturating_add(line_h)
            .saturating_add(gap)
            .saturating_add(band)
    }

    /// Paint the meter into `surface` at `bounds` for the active theme.
    ///
    /// The label and reading each draw only when the remaining height still
    /// fits a full line, and the track claims whatever height is left after
    /// them, capped by the theme's track thickness; a `bounds` too small for
    /// the full anatomy degrades by omitting a line rather than overlapping
    /// or drawing past its own edge (fail closed).
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let bottom = y.saturating_add(h);

        let label_color = Color::from(palette.on_surface_muted);
        let limits = (bottom, w, gap);
        let cursor_y = paint_line(surface, &self.label, (x, y), limits, font, label_color);
        let reading_color = Color::from(palette.on_surface);
        let cursor_y = paint_line(
            surface,
            &self.reading,
            (x, cursor_y),
            limits,
            font,
            reading_color,
        );

        let track_h = bottom.saturating_sub(cursor_y);
        self.paint_track(surface, (x, cursor_y, w, track_h), scale, theme);
    }

    /// Paint the rounded track band: the quiet groove, then — for a measured
    /// value — the tinted fill or sparkline, then the pressure-emphasis
    /// outline when the owner marked this resource as genuinely under load.
    fn paint_track(
        &self,
        surface: &mut Surface,
        band: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) {
        let (x, y, w, avail_h) = band;
        let band_h = progress_thickness(theme, scale).min(avail_h);
        if band_h == 0 || w == 0 {
            return;
        }
        let palette = theme.palette();
        let radius = band_h / 2;
        surface.fill_round_rect(x, y, w, band_h, radius, Color::from(palette.scroll_track));

        let MeterValue::Measured(value) = self.value else {
            // Honest unmeasured state: the quiet groove alone, never a
            // fabricated fill.
            return;
        };
        let fill = signal_color(theme, self.kind);
        if self.samples.is_empty() {
            let fill_w = proportional(w, value.permille()).max(band_h.min(w));
            surface.fill_round_rect(x, y, fill_w, band_h, radius, fill);
        } else {
            paint_sparkline(surface, (x, y, w, band_h), &self.samples, fill);
        }

        if matches!(self.pressure, PressureState::Under(_)) {
            let thickness = rail_thickness(theme, scale).min(band_h / 2).max(1);
            draw_outline(surface, x, y, w, band_h, thickness, fill);
        }
    }
}

/// Draw one text line at `pos` (`(x, y)`) if a full line still fits before
/// `limits`' `bottom` within its `w`, returning the y the next line starts
/// at (advanced by the line height and `gap`, the third element of
/// `limits`). A bound too short to hold the line is left untouched — the
/// line is simply omitted rather than overlapping whatever follows it.
fn paint_line(
    surface: &mut Surface,
    text: &str,
    pos: (u32, u32),
    limits: (u32, u32, u32),
    font: BitmapFont,
    color: Color,
) -> u32 {
    let (x, y) = pos;
    let (bottom, w, gap) = limits;
    let line_h = font.line_height();
    if w == 0 || y.saturating_add(line_h) > bottom {
        return y;
    }
    let fitted = font.truncate_to_width(text, w);
    font.draw_text(surface, to_i32(x), to_i32(y), fitted, color);
    y.saturating_add(line_h).saturating_add(gap)
}

/// `extent` scaled by `permille / 1000`, rounded down and never exceeding
/// `extent` (arithmetic saturates rather than overflowing).
fn proportional(extent: u32, permille: u16) -> u32 {
    u32::try_from(u64::from(extent) * u64::from(permille) / u64::from(FULL)).unwrap_or(extent)
}

/// Paint a bounded, oldest-to-newest sparkline of `samples` (permille) inside
/// `band`, one bottom-aligned bar per sample sized to its value. A sample of
/// `0` still draws a minimal one-pixel-tall bar so a lone reading — or a
/// flat, zero-range series — is never invisible.
fn paint_sparkline(
    surface: &mut Surface,
    band: (u32, u32, u32, u32),
    samples: &[u16],
    color: Color,
) {
    let (x, y, w, h) = band;
    let count = samples.len();
    if count == 0 || w == 0 || h == 0 {
        return;
    }
    let seg_w = (w / u32::try_from(count).unwrap_or(u32::MAX).max(1)).max(1);
    let right = x.saturating_add(w);
    for (i, &permille) in samples.iter().enumerate() {
        let idx = u32::try_from(i).unwrap_or(u32::MAX);
        let bar_x = x.saturating_add(idx.saturating_mul(seg_w));
        if bar_x >= right {
            break;
        }
        let bar_w = seg_w.min(right.saturating_sub(bar_x));
        let bar_h = proportional(h, permille).max(1).min(h);
        let bar_y = y.saturating_add(h).saturating_sub(bar_h);
        surface.fill_rect(bar_x, bar_y, bar_w, bar_h, color);
    }
}
