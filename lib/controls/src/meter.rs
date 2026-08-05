//! The [`Meter`] control: one resource reading, shown as a rounded track
//! tinted by its resource's semantic rail colour (`plans/NEW-TASKBAR.md` T11,
//! `plans/GUI-CONTROLS-DESIGN.md` value/measured family).
//!
//! A meter says how much of a resource is in use *now*. What it has been doing
//! over time is a different instrument with a different shape — a
//! [`Chart`](crate::Chart), which owns a whole box rather than an instrument
//! groove, because a trend crammed into a track's thickness cannot rise more
//! than a pixel or two whatever its values are.
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

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::paint::{
    paint_measured_track, paint_text_line, progress_thickness, signal_color, surface_rect,
};
use crate::state::{PressureKind, PressureState, ProgressValue};

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
/// track tinted by the resource's semantic rail colour (spec §11.33).
///
/// A meter is an instrument, not an action: it has no pointer or keyboard
/// handling and reports nothing back to its owner. The owner supplies every
/// visible fact (label, reading text, resource kind, pressure emphasis, and
/// value) and re-renders when any of them changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Meter {
    label: String,
    reading: String,
    kind: PressureKind,
    pressure: PressureState,
    value: MeterValue,
}

impl Meter {
    /// A meter labelled `label`, showing `reading` as its display text, for
    /// the resource `kind`, at `value` (or [`MeterValue::Unmeasured`] when
    /// the resource cannot currently be read). The meter starts with no
    /// pressure emphasis; add it with [`with_pressure`](Self::with_pressure).
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

    /// The height, in physical pixels, the label and reading lines occupy at
    /// `scale` before the track beneath them.
    ///
    /// A band that shows a resource's *history* rather than its current value
    /// puts a [`Chart`](crate::Chart) in the slot the track would have taken,
    /// and needs to know where that slot starts. Exposing the text height —
    /// rather than having the band subtract a track thickness from
    /// [`measured_height`](Self::measured_height) — keeps the anatomy's one
    /// definition here, so the two can never disagree.
    #[must_use]
    pub fn reading_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let line_h = font.line_height();
        line_h
            .saturating_add(gap)
            .saturating_add(line_h)
            .saturating_add(gap)
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
        Self::reading_height(scale, theme, font).saturating_add(progress_thickness(theme, scale))
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
        let cursor_y = paint_text_line(surface, &self.label, (x, y), limits, font, label_color);
        let reading_color = Color::from(palette.on_surface);
        let cursor_y = paint_text_line(
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
    /// value — the tinted proportional fill, then the pressure-emphasis
    /// outline when the owner marked this resource as genuinely under load.
    ///
    /// The groove/fill/outline geometry itself is the one every measured
    /// track shares (`crate::paint::paint_measured_track`); a meter's own
    /// part is only picking its fill and tint from its typed value and
    /// resource kind.
    fn paint_track(
        &self,
        surface: &mut Surface,
        band: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) {
        let fill = match self.value {
            MeterValue::Measured(value) => Some(value.permille()),
            // Honest unmeasured state: the quiet groove alone, never a
            // fabricated fill.
            MeterValue::Unmeasured => None,
        };
        let tint = signal_color(theme, self.kind);
        let emphasised = matches!(self.pressure, PressureState::Under(_));
        paint_measured_track(surface, band, fill, tint, emphasised, scale, theme);
    }
}
