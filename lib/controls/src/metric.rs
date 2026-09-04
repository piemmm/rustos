//! The metric-readout family: [`MetricTile`], the at-a-glance card that
//! reports one resource; [`StatusPill`], the compact capsule that names a
//! state; and [`CompositionBar`], the split of a measured whole into its
//! named parts (`plans/GUI-CONTROLS-DESIGN.md`'s System overview resource
//! cards).
//!
//! A metric tile is what a resource becomes when it needs its own bounded
//! card rather than a row in a shared header band: an optional leading icon
//! naming the resource, a quiet label, a large reading with a quieter unit,
//! an optional line of supporting detail, and an optional instrument beneath
//! — either a proportional track, or a [`Chart`] of the resource's recent
//! history. Like those two instruments, a tile is read-only: it carries the
//! facts its owner supplies and never accepts pointer or keyboard input.
//!
//! A tile draws in one of two [`MetricLayout`]s. [`MetricLayout::Stacked`],
//! the default, is the form a tile uses when it owns a column of its own:
//! the label sits above the large reading, exactly as the control has always
//! drawn. [`MetricLayout::Inline`] is the compact single-line form a stack of
//! readings uses when several must fit a narrow column and be scanned down:
//! the label leads, the reading (with its unit) trails, right-aligned on the
//! same line, and the reading keeps the room it needs when the two cannot
//! both fit — the label truncates first, never the reading, because the
//! reading is what the reader came for. Both layouts still draw the optional
//! detail line and instrument beneath, spanning the tile's full width.
//!
//! A tile is plated by default: an Alloy Plate of its own, exactly as before.
//! [`unplated`](MetricTile::unplated) drops that plate, its rim, and its own
//! padding, for a tile that sits inside a container — a [`Panel`
//! ](crate::collection::Panel) — that already provides the surface, so
//! several readings can share one plate without nesting a second one inside
//! it.
//!
//! A composition bar answers a question neither of the other two can: *where
//! did it go*. A track says how much of a resource is in use and a chart says
//! what it has been doing, but neither splits that use into the parts it is
//! made of. The bar draws those parts as one proportional row through the same
//! measured-track geometry a track uses, tinted down one ladder of the
//! resource's own hue so they separate by weight rather than by a second
//! colour vocabulary, and names each one with its amount in a key beneath.
//! Parts that do not sum to the whole are a construction error: a composition
//! that does not account for everything is not one.
//!
//! A status pill fills a gap neither instrument covers: naming a condition —
//! "Healthy", "Denied", "Recovering" — in a compact capsule with no action of
//! its own, for a surface that needs to badge a state without offering a
//! button. It reuses the theme's own [`SignalRole`] vocabulary rather than
//! inventing a second one, so a pill's tone always means the same thing a
//! Pressure Rail or Signal Bead of that role means elsewhere.
//!
//! Every colour, radius, thickness, gap, and font metric resolves from the
//! active [`Theme`] and [`Scale`] through the shared accessors
//! (`crate::paint`), and a tile's embedded track is the one shared
//! groove/fill/outline recipe every measured track in the crate draws from,
//! and its leading icon is the same slot [`Button`](crate::Button)'s
//! [`ButtonContent::Icon`](crate::button::ButtonContent::Icon) draws through,
//! so neither control here carries its own copy of that geometry.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_icon::{IconKind, IconPicture};
use tairix_raster::{Color, Surface};
use tairix_theme::{SignalRole, TextRole, Theme};

use crate::chart::Chart;
use crate::paint::{
    clamp_permille, composition_remainder_tint, composition_tint, heavy_contrast, inset,
    paint_icon_slot, paint_measured_track, paint_plate, paint_text_line, plate_border,
    progress_thickness, role_font, signal_color, surface_rect, to_i32, PlateStyle, TrackBand,
    COMPOSITION_HUE_COUNT, FULL, FULL_COLOUR,
};
use crate::state::{MeterValue, PressureKind, PressureState};

/// How much of a toned [`StatusPill`]'s own role colour its wash carries,
/// against the quiet surface: low enough that the capsule still reads as a
/// quiet background with a hint of the role's hue, never as a solid block of
/// it (the label itself, drawn at the full role colour, carries the emphasis).
const TONE_WASH_PERMILLE: u16 = 800;

/// The instrument an at-a-glance [`MetricTile`] shows beneath its reading, if
/// any.
///
/// Each variant reuses the value type of the instrument family it draws from
/// — [`MeterValue`] for a proportional track, [`Chart`] for a trend — so a
/// tile's honestly-unmeasured state is the same type every other resource
/// instrument already validates, rather than a second copy of that
/// vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetricInstrument {
    /// No instrument: the reading alone, with the tile ending after its text.
    None,
    /// A proportional track of the resource's current level, tinted by the
    /// tile's own resource kind and emphasised under genuine pressure.
    Track(MeterValue),
    /// A bounded history trace of the resource's recent readings.
    Trend(Chart),
}

/// How a [`MetricTile`] arranges its label and reading.
///
/// The anatomy beneath them — the optional detail line and instrument — is
/// identical either way; only the label/reading arrangement, and so the
/// height it claims, changes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MetricLayout {
    /// The label above a large reading: the form a tile uses when it has a
    /// column of its own to fill.
    Stacked,
    /// The label and the reading on one line, label leading and reading
    /// trailing: the form a stack of readings uses when several must fit a
    /// narrow column and be scanned down.
    Inline,
}

/// One at-a-glance report of a resource: an optional leading icon, a quiet
/// label, a large reading with a quieter unit, an optional line of detail,
/// and an optional instrument beneath.
///
/// A metric tile is an instrument, not an action: it has no pointer or
/// keyboard handling and reports nothing back to its owner. The owner
/// supplies every visible fact and re-renders when any of them changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetricTile {
    label: String,
    value: String,
    unit: Option<String>,
    detail: Option<String>,
    kind: PressureKind,
    pressure: PressureState,
    instrument: MetricInstrument,
    icon: Option<IconKind>,
    layout: MetricLayout,
    plated: bool,
}

impl MetricTile {
    /// A tile labelled `label`, reporting `value` for the resource `kind`.
    /// The tile starts with no unit, no detail, no pressure emphasis, no
    /// instrument, and no icon; is [`MetricLayout::Stacked`]; and is plated.
    /// Add the rest with the `with_*` methods.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>, kind: PressureKind) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            unit: None,
            detail: None,
            kind,
            pressure: PressureState::None,
            instrument: MetricInstrument::None,
            icon: None,
            layout: MetricLayout::Stacked,
            plated: true,
        }
    }

    /// This tile with a unit drawn immediately after its value, in the muted
    /// foreground, on the same baseline.
    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// This tile with a line of supporting detail beneath its reading.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// This tile with the given pressure emphasis, as used by
    /// [`Card`](crate::Card)'s Pressure Rail: [`PressureState::Under`] draws
    /// the emphasis outline around an embedded [`MetricInstrument::Track`];
    /// [`PressureState::None`] leaves the plain tinted instrument.
    #[must_use]
    pub fn with_pressure(mut self, pressure: PressureState) -> Self {
        self.pressure = pressure;
        self
    }

    /// This tile with the given instrument claiming the height beneath its
    /// reading, replacing [`MetricInstrument::None`].
    #[must_use]
    pub fn with_instrument(mut self, instrument: MetricInstrument) -> Self {
        self.instrument = instrument;
        self
    }

    /// This tile with a leading icon identifying what the reading is about,
    /// drawn in the tile's leading gutter and tinted by the resource's own
    /// signal colour — the same identity tint its embedded track wears.
    #[must_use]
    pub fn with_icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    /// This tile's leading icon, if any.
    #[must_use]
    pub fn icon(&self) -> Option<IconKind> {
        self.icon
    }

    /// This tile drawn with the given [`MetricLayout`], replacing the default
    /// [`MetricLayout::Stacked`].
    #[must_use]
    pub fn with_layout(mut self, layout: MetricLayout) -> Self {
        self.layout = layout;
        self
    }

    /// This tile's layout.
    #[must_use]
    pub fn layout(&self) -> MetricLayout {
        self.layout
    }

    /// This tile with no plate, rim, or padding of its own — for a tile
    /// seated inside a container, such as a [`Panel`](crate::collection::Panel),
    /// that already provides the surface, so several readings can share one
    /// plate without nesting a second one inside it. The default is plated.
    #[must_use]
    pub fn unplated(mut self) -> Self {
        self.plated = false;
        self
    }

    /// Whether this tile draws its own plate.
    #[must_use]
    pub fn is_plated(&self) -> bool {
        self.plated
    }

    /// The height, in physical pixels, the icon (if any), label, value, and
    /// detail lines occupy at `scale` before the instrument beneath them.
    ///
    /// This depends on the tile's layout and whether it carries a detail
    /// line, so it is computed from `self` rather than statically.
    /// [`measured_height`](Self::measured_height) and
    /// [`render`](Self::render) both build on this one definition, so the two
    /// can never disagree about where the instrument slot begins.
    #[must_use]
    pub fn reading_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let line_h = font.line_height();
        let mut height = match self.layout {
            MetricLayout::Stacked => line_h.saturating_add(gap).saturating_add(line_h),
            MetricLayout::Inline => line_h,
        };
        if self.detail.is_some() {
            height = height.saturating_add(gap).saturating_add(line_h);
        }
        height.saturating_add(gap)
    }

    /// The minimum height, in physical pixels, the whole tile needs at
    /// `scale` — its plate border and padding (when plated), its reading
    /// lines, and its instrument slot — to draw without clipping.
    #[must_use]
    pub fn measured_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let content = self
            .reading_height(scale, theme)
            .saturating_add(self.instrument_extent(scale, theme));
        if !self.plated {
            return content;
        }
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        content
            .saturating_add(pad.saturating_mul(2))
            .saturating_add(border.saturating_mul(2))
    }

    /// The instrument slot's own natural height at `scale`: the shared
    /// measured-track thickness for [`MetricInstrument::Track`], the theme's
    /// chart height for [`MetricInstrument::Trend`], and zero for
    /// [`MetricInstrument::None`].
    #[must_use]
    fn instrument_extent(&self, scale: Scale, theme: &Theme) -> u32 {
        match &self.instrument {
            MetricInstrument::None => 0,
            MetricInstrument::Track(_) => progress_thickness(theme, scale),
            MetricInstrument::Trend(_) => scale.scale_length(theme.metrics().chart_height).max(1),
        }
    }

    /// The side, in physical pixels, of the square leading-icon slot within a
    /// primary label/reading block of height `primary_h`: the whole block for
    /// [`MetricLayout::Stacked`] (a two-line reading gets an icon its own
    /// size), capped at `avail_h` so a tile too short for its full anatomy
    /// never grows the icon past what actually fits.
    #[must_use]
    fn icon_side(primary_h: u32, avail_h: u32) -> u32 {
        primary_h.min(avail_h)
    }

    /// Paint the tile into `surface` at `bounds` for the active theme.
    ///
    /// A plated tile (the default) fills `bounds` with a quiet Alloy Plate
    /// first; an [`unplated`](Self::unplated) one skips the plate, rim, and
    /// padding entirely and draws its content straight into `bounds`, for a
    /// container that already provides the surface. Inside the content area,
    /// a leading icon (if any) claims a square gutter beside the label and
    /// reading, sized to the layout's own primary block so it never shifts
    /// the detail line or instrument, which always span the tile's full
    /// width. The label, value (with its unit), and optional detail each draw
    /// only while a whole line still fits, and the instrument claims whatever
    /// height remains; a `bounds` too small for the full anatomy degrades by
    /// omitting the instrument, then the detail line, rather than overlapping
    /// or drawing past its own edge (fail closed).
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<IconPicture<'_>>,
    ) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();

        let Some((cx, cy, cw, ch)) = self.content_rect(surface, (x, y, w, h), scale, theme) else {
            return;
        };

        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let bottom = cy.saturating_add(ch);
        let full_limits = (bottom, cw, gap);

        let line_h = font.line_height();
        let primary_h = match self.layout {
            MetricLayout::Stacked => line_h.saturating_add(gap).saturating_add(line_h),
            MetricLayout::Inline => line_h,
        };
        let mut primary_x = cx;
        let mut primary_w = cw;
        if let Some(kind) = self.icon {
            let side = Self::icon_side(primary_h, ch);
            if side > 0 {
                let tint = signal_color(theme, self.kind);
                paint_icon_slot(surface, (cx, cy, side), kind, tint, artwork, FULL_COLOUR);
                let gutter = side.saturating_add(gap);
                primary_x = cx.saturating_add(gutter);
                primary_w = cw.saturating_sub(gutter);
            }
        }
        let primary_limits = (bottom, primary_w, gap);

        let label_color = Color::from(palette.on_surface_muted);
        let colors = (
            Color::from(palette.on_surface),
            Color::from(palette.on_surface_muted),
        );
        let cursor_y = match self.layout {
            MetricLayout::Stacked => {
                let cursor_y = paint_text_line(
                    surface,
                    &self.label,
                    (primary_x, cy),
                    primary_limits,
                    font,
                    label_color,
                );
                paint_reading_line(
                    surface,
                    &self.value,
                    self.unit.as_deref(),
                    (primary_x, cursor_y),
                    primary_limits,
                    font,
                    colors,
                )
            }
            MetricLayout::Inline => paint_inline_reading_line(
                surface,
                &InlineReading {
                    label: &self.label,
                    value: &self.value,
                    unit: self.unit.as_deref(),
                },
                (primary_x, cy),
                primary_limits,
                font,
                label_color,
                colors,
            ),
        };

        let cursor_y = match &self.detail {
            Some(detail) => paint_text_line(
                surface,
                detail,
                (cx, cursor_y),
                full_limits,
                font,
                Color::from(palette.on_surface_muted),
            ),
            None => cursor_y,
        };

        let instrument_h = bottom.saturating_sub(cursor_y);
        self.paint_instrument(surface, (cx, cursor_y, cw, instrument_h), scale, theme);
    }

    /// The content rectangle `(x, y, w, h)` this tile draws its icon, text,
    /// and instrument into: `bounds` inset by the plate border and control
    /// padding when [`plated`](Self::is_plated), painting that plate first;
    /// `bounds` unchanged, with nothing painted, when
    /// [`unplated`](Self::unplated) — the one place the two forms diverge, so
    /// [`render`](Self::render) itself never has to branch on it again.
    fn content_rect(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = rect;
        if !self.plated {
            return Some((x, y, w, h));
        }
        let palette = theme.palette();
        let radius = scale
            .scale_length(theme.metrics().control_corner_radius)
            .min(w / 2)
            .min(h / 2);
        let border = plate_border(theme, scale);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );

        let (ix, iy, iw, ih) = inset(x, y, w, h, border)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        inset(ix, iy, iw, ih, pad)
    }

    /// Paint the instrument slot: a proportional track through the shared
    /// measured-track recipe, a chart delegated to
    /// [`Chart::render`](Chart::render), or nothing.
    fn paint_instrument(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) {
        let (x, y, w, h) = rect;
        if w == 0 || h == 0 {
            return;
        }
        match &self.instrument {
            MetricInstrument::None => {}
            MetricInstrument::Track(value) => {
                let fill = match value {
                    MeterValue::Measured(progress) => Some(progress.permille()),
                    // Honest unmeasured state: the quiet groove alone, never
                    // a fabricated fill.
                    MeterValue::Unmeasured => None,
                };
                let tint = signal_color(theme, self.kind);
                let emphasised = matches!(self.pressure, PressureState::Under(_));
                paint_measured_track(surface, (x, y, w, h), fill, tint, emphasised, scale, theme);
            }
            MetricInstrument::Trend(chart) => {
                chart.render(surface, Rect::new(to_i32(x), to_i32(y), w, h), scale, theme);
            }
        }
    }
}

/// The truncated value text, and the truncated unit text (if room remains
/// after the value), for a reading fitted into at most `max_w` physical
/// pixels.
///
/// This is the one "value claims its width first, the unit takes whatever is
/// left" fit every reading anatomy shares — [`paint_reading_line`]'s
/// left-aligned stacked reading and [`paint_inline_reading_line`]'s
/// right-aligned one both compute their fit from this one definition, so the
/// two can never disagree about how a reading degrades under width pressure.
fn fit_reading<'a>(
    font: BitmapFont,
    value: &'a str,
    unit: Option<&'a str>,
    max_w: u32,
) -> (&'a str, Option<&'a str>) {
    let value_fitted = font.truncate_to_width(value, max_w);
    let Some(unit) = unit else {
        return (value_fitted, None);
    };
    let value_w = font.text_width(value_fitted);
    let space = font.advance(' ');
    let used = value_w.saturating_add(space).min(max_w);
    let remaining = max_w.saturating_sub(used);
    if remaining == 0 {
        return (value_fitted, None);
    }
    (value_fitted, Some(font.truncate_to_width(unit, remaining)))
}

/// The physical width `value_fitted` plus, if present, a space and
/// `unit_fitted` occupy — the one width computation both
/// [`paint_reading_line`] and [`paint_inline_reading_line`] use to place the
/// unit after the value and, for the inline form, to right-align the whole
/// reading.
fn reading_width(font: BitmapFont, value_fitted: &str, unit_fitted: Option<&str>) -> u32 {
    let value_w = font.text_width(value_fitted);
    match unit_fitted {
        Some(unit_fitted) => value_w
            .saturating_add(font.advance(' '))
            .saturating_add(font.text_width(unit_fitted)),
        None => value_w,
    }
}

/// Draw the value/unit reading line at `pos` (`(x, y)`) if a full line still
/// fits before `limits`' `bottom` within its `w`, returning the y the next
/// line starts at. The value is truncated to the full width and drawn in
/// `colors.0`; a present `unit` is then drawn, separated by one space
/// advance, in `colors.1`, truncated to whatever width remains — so "8.6"
/// reads loud and a trailing "GB / 16 GB" reads quiet without either
/// overrunning the line.
fn paint_reading_line(
    surface: &mut Surface,
    value: &str,
    unit: Option<&str>,
    pos: (u32, u32),
    limits: (u32, u32, u32),
    font: BitmapFont,
    colors: (Color, Color),
) -> u32 {
    let (x, y) = pos;
    let (bottom, w, gap) = limits;
    let (value_color, unit_color) = colors;
    let line_h = font.line_height();
    if w == 0 || y.saturating_add(line_h) > bottom {
        return y;
    }
    let (value_fitted, unit_fitted) = fit_reading(font, value, unit, w);
    font.draw_text(surface, to_i32(x), to_i32(y), value_fitted, value_color);
    if let Some(unit_fitted) = unit_fitted {
        let value_w = font.text_width(value_fitted);
        let unit_x = x.saturating_add(value_w).saturating_add(font.advance(' '));
        font.draw_text(surface, to_i32(unit_x), to_i32(y), unit_fitted, unit_color);
    }
    y.saturating_add(line_h).saturating_add(gap)
}

/// The label, value, and unit text one [`MetricLayout::Inline`] reading line
/// draws, grouped so [`paint_inline_reading_line`] takes one field rather
/// than three positional string parameters (its argument count is already
/// tight without them).
struct InlineReading<'a> {
    /// The leading label, which truncates first when the line is too
    /// narrow for both it and the reading.
    label: &'a str,
    /// The reading's value text.
    value: &'a str,
    /// The reading's unit text, if any.
    unit: Option<&'a str>,
}

/// Draw one [`MetricLayout::Inline`] label/reading line at `pos` (`(x, y)`)
/// if a full line still fits before `limits`' `bottom` within its `w`,
/// returning the y the next line starts at.
///
/// The reading (value plus its unit) is fitted first, through the same
/// [`fit_reading`] recipe [`paint_reading_line`] uses, and right-aligned
/// within `w`; the label then draws leading, truncated to whatever width
/// remains before the reading's own leading edge. The reading therefore
/// always keeps the room it needs and the label is what gives way — the
/// reading is what the reader came for.
fn paint_inline_reading_line(
    surface: &mut Surface,
    reading: &InlineReading<'_>,
    pos: (u32, u32),
    limits: (u32, u32, u32),
    font: BitmapFont,
    label_color: Color,
    colors: (Color, Color),
) -> u32 {
    let label = reading.label;
    let value = reading.value;
    let unit = reading.unit;
    let (x, y) = pos;
    let (bottom, w, gap) = limits;
    let (value_color, unit_color) = colors;
    let line_h = font.line_height();
    if w == 0 || y.saturating_add(line_h) > bottom {
        return y;
    }
    let (value_fitted, unit_fitted) = fit_reading(font, value, unit, w);
    let reading_w = reading_width(font, value_fitted, unit_fitted).min(w);
    let reading_x = x.saturating_add(w).saturating_sub(reading_w);

    let label_avail = reading_x.saturating_sub(gap).saturating_sub(x);
    if label_avail > 0 {
        let label_fitted = font.truncate_to_width(label, label_avail);
        font.draw_text(surface, to_i32(x), to_i32(y), label_fitted, label_color);
    }

    font.draw_text(
        surface,
        to_i32(reading_x),
        to_i32(y),
        value_fitted,
        value_color,
    );
    if let Some(unit_fitted) = unit_fitted {
        let value_w = font.text_width(value_fitted);
        let unit_x = reading_x
            .saturating_add(value_w)
            .saturating_add(font.advance(' '));
        font.draw_text(surface, to_i32(unit_x), to_i32(y), unit_fitted, unit_color);
    }
    y.saturating_add(line_h).saturating_add(gap)
}

/// A compact capsule that names a state, with no action of its own.
///
/// A status pill is read-only, like [`MetricTile`]: it has no pointer or
/// keyboard handling. The design language has no standalone pill anywhere
/// else, so a surface that needs to badge a condition — a service's health, a
/// denied capability, a recovering device — without offering a button reaches
/// for this one shared shape rather than inventing its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusPill {
    label: String,
    tone: Option<SignalRole>,
}

impl StatusPill {
    /// A neutral pill: the emphasised foreground on a quiet capsule, with no
    /// semantic tone. Add one with [`with_tone`](Self::with_tone).
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            tone: None,
        }
    }

    /// This pill toned by a semantic signal role: a low-emphasis wash of the
    /// role's colour, with the label in that same colour.
    #[must_use]
    pub fn with_tone(mut self, tone: SignalRole) -> Self {
        self.tone = Some(tone);
        self
    }

    /// The pill's label text.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The width, in physical pixels, this pill's label needs at `scale`,
    /// including the control padding either side.
    #[must_use]
    pub fn measured_width(&self, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        font.text_width(&self.label)
            .saturating_add(pad.saturating_mul(2))
    }

    /// The height, in physical pixels, one pill needs at `scale`: its
    /// label's line height plus the control padding above and below.
    #[must_use]
    pub fn measured_height(scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        font.line_height().saturating_add(pad.saturating_mul(2))
    }

    /// Paint the pill into `surface` at `bounds` for the active theme.
    ///
    /// The capsule is fully rounded (radius = half its height) through the
    /// shared plate fill, with the label centred inside it. A `bounds` too
    /// small for one full line paints nothing; a label wider than `bounds`
    /// truncates.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        let line_h = font.line_height();
        if w == 0 || h < line_h {
            return;
        }
        let palette = theme.palette();
        let (fill, label_color) = match self.tone {
            Some(role) => {
                let role_color = palette.signal(role);
                (
                    Color::from(role_color.mix(palette.surface, TONE_WASH_PERMILLE)),
                    Color::from(role_color),
                )
            }
            None => (
                Color::from(palette.surface),
                Color::from(palette.on_surface),
            ),
        };

        let radius = h / 2;
        let border = plate_border(theme, scale);
        let rim = if heavy_contrast(theme) {
            label_color
        } else {
            fill
        };
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: fill,
                rim,
                focused: false,
                ring: label_color,
            },
        );

        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let Some(avail) = w.checked_sub(pad.saturating_mul(2)) else {
            return;
        };
        if avail == 0 {
            return;
        }
        let fitted = font.truncate_to_width(&self.label, avail);
        let text_w = font.text_width(fitted).min(avail);
        let text_x = x.saturating_add(pad).saturating_add((avail - text_w) / 2);
        let text_y = y.saturating_add(h.saturating_sub(line_h) / 2);
        font.draw_text(surface, to_i32(text_x), to_i32(text_y), fitted, label_color);
    }
}

/// The most *used* parts one [`CompositionBar`] may separate.
///
/// One per hue the composition ladder offers: a part wearing another part's
/// hue is not a part a reader can find in the bar. A composition's own
/// remainder does not count against this, because it takes the neutral rather
/// than a hue. This is a validation bound on the composition, not a capacity
/// that should grow with the machine.
pub const MAX_COMPOSITION_SEGMENTS: usize = COMPOSITION_HUE_COUNT;

/// Why a [`CompositionBar`] could not be built.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CompositionError {
    /// The segments' shares do not account for the whole: they sum to
    /// `total_permille` rather than to all of it. A silently short bar would
    /// under-report where the resource went, so this fails construction.
    Incomplete {
        /// What the shares actually sum to, in permille.
        total_permille: u32,
    },
    /// More used parts than the ladder can tell apart — see
    /// [`MAX_COMPOSITION_SEGMENTS`].
    TooManySegments {
        /// How many used parts were offered.
        used: usize,
    },
    /// A remainder was not the last part, or there was more than one.
    ///
    /// The remainder is the share of the whole that is *not* in use, so it
    /// reads as the unfilled tail of the bar. Anywhere else it would read as a
    /// gap the composition failed to account for, which is the opposite of
    /// what it says.
    MisplacedRemainder,
}

/// One named part of a [`CompositionBar`]'s measured whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionSegment {
    label: String,
    amount: String,
    share: u16,
    remainder: bool,
}

impl CompositionSegment {
    /// A part in use, labelled `label`, holding `share` permille of the whole,
    /// whose own measured quantity reads `amount`.
    ///
    /// `share` is clamped fail closed, so an out-of-range share surfaces as
    /// [`CompositionError::Incomplete`] from [`CompositionBar::new`] rather
    /// than as a bar drawn past its own end.
    #[must_use]
    pub fn new(label: impl Into<String>, amount: impl Into<String>, share: u16) -> Self {
        Self {
            label: label.into(),
            amount: amount.into(),
            share: clamp_permille(share),
            remainder: false,
        }
    }

    /// The composition's remainder: the share of the whole that is *not* in
    /// use, drawn in the track's own quiet neutral so it reads as the unfilled
    /// tail while the key still names it.
    ///
    /// A composition has at most one, and it is the last part.
    #[must_use]
    pub fn remainder(label: impl Into<String>, amount: impl Into<String>, share: u16) -> Self {
        Self {
            remainder: true,
            ..Self::new(label, amount, share)
        }
    }

    /// The part's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The part's own measured quantity, as its owner formatted it.
    #[must_use]
    pub fn amount(&self) -> &str {
        &self.amount
    }

    /// The part's share of the whole, in permille.
    #[must_use]
    pub fn share(&self) -> u16 {
        self.share
    }

    /// Whether this part is the composition's remainder.
    #[must_use]
    pub fn is_remainder(&self) -> bool {
        self.remainder
    }
}

/// A measured whole split into its named parts: one proportional row, then a
/// key naming each part and its amount.
///
/// Like [`MetricTile`] this is an instrument, not an action: it has no pointer
/// or keyboard handling and reports nothing back to its owner. It draws no
/// plate of its own, so several readings seated in one
/// [`Panel`](crate::collection::Panel) share that container's surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionBar {
    kind: PressureKind,
    segments: Vec<CompositionSegment>,
}

impl CompositionBar {
    /// A bar splitting resource `kind` into `segments`, in reading order.
    ///
    /// # Errors
    ///
    /// [`CompositionError::Incomplete`] unless the shares sum to exactly the
    /// whole; [`CompositionError::TooManySegments`] past
    /// [`MAX_COMPOSITION_SEGMENTS`] used parts; and
    /// [`CompositionError::MisplacedRemainder`] for a remainder that is not
    /// the one last part.
    pub fn new(
        kind: PressureKind,
        segments: Vec<CompositionSegment>,
    ) -> Result<Self, CompositionError> {
        let used = segments.iter().filter(|s| !s.remainder).count();
        if used > MAX_COMPOSITION_SEGMENTS {
            return Err(CompositionError::TooManySegments { used });
        }
        if segments
            .iter()
            .enumerate()
            .any(|(i, s)| s.remainder && i + 1 != segments.len())
        {
            return Err(CompositionError::MisplacedRemainder);
        }
        let total: u32 = segments.iter().map(|s| u32::from(s.share)).sum();
        if total != u32::from(FULL) {
            return Err(CompositionError::Incomplete {
                total_permille: total,
            });
        }
        Ok(Self { kind, segments })
    }

    /// The resource this bar splits.
    #[must_use]
    pub fn kind(&self) -> PressureKind {
        self.kind
    }

    /// The bar's parts, in reading order.
    #[must_use]
    pub fn segments(&self) -> &[CompositionSegment] {
        &self.segments
    }

    /// The height, in physical pixels, the bar and its whole key need at
    /// `scale` within `width`.
    ///
    /// The key names *every* part — dropping one would leave a segment of the
    /// bar nameless — so it wraps onto further rows rather than truncating its
    /// list, and how many rows that is depends on the width it is given.
    #[must_use]
    pub fn measured_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let rows = self.key_rows(width, scale, theme);
        let key = rows
            .saturating_mul(font.line_height())
            .saturating_add(rows.saturating_sub(1).saturating_mul(gap));
        progress_thickness(theme, scale)
            .saturating_add(gap)
            .saturating_add(key)
    }

    /// How many rows the key packs into `width`.
    #[must_use]
    fn key_rows(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        self.key_layout(width, scale, theme)
            .last()
            .map_or(0, |entry| entry.row.saturating_add(1))
    }

    /// Paint the bar and its key into `surface` at `bounds` for the active
    /// theme.
    ///
    /// A `bounds` too short for the whole key drops the key rows that do not
    /// fit rather than overlapping them or drawing past its own edge, and one
    /// too short to seat the bar itself draws nothing at all (fail closed).
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        let Some(band) = TrackBand::groove(surface, (x, y, w, h), scale, theme) else {
            return;
        };
        self.paint_segments(surface, band, scale, theme);
        self.paint_key(surface, (x, y, w, h), band.height(), scale, theme);
    }

    /// The tint part `index` wears: its own step of the hue rotation, or the
    /// track's quiet neutral for the composition's remainder.
    fn tint(&self, index: usize, theme: &Theme) -> Color {
        match self.segments.get(index) {
            Some(segment) if segment.remainder => composition_remainder_tint(theme),
            _ => composition_tint(theme, self.kind, index),
        }
    }

    /// Fill the bar's parts into `band`, last part first, then rule the joins
    /// between them.
    ///
    /// Each part is filled to its *cumulative* share, so the part drawn over it
    /// covers the rounded end of the one before: the bar's own two ends round
    /// and the parts butt cleanly, all from the one shared band fill rather
    /// than a second segment geometry. The joins are then ruled in the surface
    /// colour, so the parts stay countable where hue carries nothing — on the
    /// monochrome-safe path, and for a reader who cannot separate two of them.
    fn paint_segments(&self, surface: &mut Surface, band: TrackBand, scale: Scale, theme: &Theme) {
        let mut ends = Vec::with_capacity(self.segments.len());
        let mut running = 0u16;
        for segment in &self.segments {
            running = running.saturating_add(segment.share);
            ends.push(clamp_permille(running));
        }
        for (index, end) in ends.iter().enumerate().rev() {
            band.fill(surface, *end, self.tint(index, theme));
        }
        let rule = plate_border(theme, scale);
        for end in ends.iter().take(ends.len().saturating_sub(1)) {
            band.rule(surface, *end, rule, Color::from(theme.palette().surface));
        }
    }

    /// Paint the key beneath the bar: one bead per part in its own tint, then
    /// the part's label and its amount, packed across the width and wrapped.
    fn paint_key(
        &self,
        surface: &mut Surface,
        rect: (u32, u32, u32, u32),
        band_h: u32,
        scale: Scale,
        theme: &Theme,
    ) {
        let (x, y, w, h) = rect;
        let font = role_font(theme, scale, TextRole::Body);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let bead = scale.scale_length(theme.metrics().bead_size).max(3);
        let line_h = font.line_height();
        let bottom = y.saturating_add(h);
        let top = y.saturating_add(band_h).saturating_add(gap);
        let palette = theme.palette();
        for entry in self.key_layout(w, scale, theme) {
            let row_y = top.saturating_add(entry.row.saturating_mul(line_h.saturating_add(gap)));
            if row_y.saturating_add(line_h) > bottom {
                return;
            }
            let Some(segment) = self.segments.get(entry.index) else {
                continue;
            };
            let entry_x = x.saturating_add(entry.offset);
            // A neutral swatch, not a Signal Bead: a key entry names a part of
            // a reading, it does not raise an alert.
            if let Some(swatch) = TrackBand::chip((
                entry_x,
                row_y.saturating_add(line_h.saturating_sub(bead) / 2),
                bead,
                bead,
            )) {
                swatch.fill(surface, FULL, self.tint(entry.index, theme));
            }
            let text_x = entry_x.saturating_add(bead).saturating_add(gap);
            let avail = x
                .saturating_add(w)
                .saturating_sub(text_x)
                .min(entry.width.saturating_sub(bead).saturating_sub(gap));
            let label = font.truncate_to_width(&segment.label, avail);
            font.draw_text(
                surface,
                to_i32(text_x),
                to_i32(row_y),
                label,
                Color::from(palette.on_surface_muted),
            );
            let after = text_x
                .saturating_add(font.text_width(label))
                .saturating_add(font.advance(' '));
            let remaining = x
                .saturating_add(w)
                .saturating_sub(after.min(x.saturating_add(w)));
            if remaining == 0 {
                continue;
            }
            let amount = font.truncate_to_width(&segment.amount, remaining);
            font.draw_text(
                surface,
                to_i32(after),
                to_i32(row_y),
                amount,
                Color::from(palette.on_surface),
            );
        }
    }

    /// Where every key entry sits: packed leading to trailing across `width`,
    /// wrapping to the next row when the next entry would not fit whole.
    ///
    /// The one layout both [`measured_height`](Self::measured_height) and
    /// [`paint_key`](Self::paint_key) read, so the height an owner reserves is
    /// the height the key draws in.
    fn key_layout(&self, width: u32, scale: Scale, theme: &Theme) -> Vec<KeyEntry> {
        let font = role_font(theme, scale, TextRole::Body);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let bead = scale.scale_length(theme.metrics().bead_size).max(3);
        let mut entries = Vec::with_capacity(self.segments.len());
        let mut offset = 0u32;
        let mut row = 0u32;
        for (index, segment) in self.segments.iter().enumerate() {
            let entry_w = bead
                .saturating_add(gap)
                .saturating_add(font.text_width(&segment.label))
                .saturating_add(font.advance(' '))
                .saturating_add(font.text_width(&segment.amount));
            if offset > 0 && offset.saturating_add(entry_w) > width {
                row = row.saturating_add(1);
                offset = 0;
            }
            entries.push(KeyEntry {
                index,
                offset,
                row,
                width: entry_w,
            });
            offset = offset
                .saturating_add(entry_w)
                .saturating_add(gap.saturating_mul(2));
        }
        entries
    }
}

/// Where one key entry sits within the key's own packed rows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct KeyEntry {
    /// The part this entry names.
    index: usize,
    /// The entry's leading edge, from the key's own left.
    offset: u32,
    /// Which packed row it landed on.
    row: u32,
    /// The width its bead, label and amount want.
    width: u32,
}
