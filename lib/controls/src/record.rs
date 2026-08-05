//! The record-list family: [`FactList`] and [`Timeline`], the two read-only
//! instruments that state what a thing *is* and what has *happened* to it
//! (`plans/GUI-CONTROLS-DESIGN.md`).
//!
//! Both controls report a record — a set of facts, or a sequence of dated
//! events — rather than something the reader can drive. Neither carries
//! pointer or keyboard handling, an action type, or interior mutability: the
//! owner supplies every visible fact and re-renders when it changes, exactly
//! as [`Meter`](crate::Meter) and [`Progress`](crate::Progress) do for a
//! single reading.
//!
//! [`FactList`] is the key-and-value readout a detail surface uses to state
//! what a thing *is*: a column of label/value pairs, the label quiet on the
//! left and the value emphasised on the right, optionally toned by a
//! [`SignalRole`] and separated by hairline rules. [`Timeline`] is the
//! ordered record of what happened and when: a connector spine down the
//! left, a shape-coded mark per event, a stamp column, and the event's text.
//!
//! Every colour, radius, thickness, gap, and font metric resolves from the
//! active [`Theme`] and [`Scale`] through the shared accessors the rest of
//! the crate already uses (`crate::paint`); circles are drawn through the
//! one shared circle-fill helper rather than a second circle recipe. An
//! empty collection renders nothing at all for either control — no plate, no
//! spine, no rule — because an empty frame implies a record that exists and
//! is known to be empty, a fact neither control can know on its own.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::{SignalRole, Theme};

use crate::paint::{paint_filled_circle, plate_border, surface_rect, to_i32};

/// One label/value pair of a [`FactList`].
///
/// A fact is a plain reading unless the owner marks it with a [`SignalRole`]
/// tone, so a healthy or alarming value reads as such without the reader
/// parsing the words.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fact {
    label: String,
    value: String,
    tone: Option<SignalRole>,
}

impl Fact {
    /// A fact with the given `label` and `value`, untoned.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            tone: None,
        }
    }

    /// This fact with its value toned by a semantic signal role, so a
    /// healthy reading reads as such without the reader parsing the words.
    #[must_use]
    pub fn with_tone(mut self, tone: SignalRole) -> Self {
        self.tone = Some(tone);
        self
    }

    /// The fact's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The fact's value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The fact's tone, if any.
    #[must_use]
    pub fn tone(&self) -> Option<SignalRole> {
        self.tone
    }
}

/// The key-and-value readout a detail surface uses to state what a thing
/// *is*: one row per [`Fact`] at a shared row height, the label quiet on the
/// left and the value emphasised on the right.
///
/// A fact list is an instrument, not an action: it has no pointer or
/// keyboard handling and reports nothing back to its owner. The owner
/// supplies the facts and re-renders when any of them changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactList {
    facts: Vec<Fact>,
    separated: bool,
}

impl FactList {
    /// A fact list over `facts`, drawn without separators.
    #[must_use]
    pub fn new(facts: Vec<Fact>) -> Self {
        Self {
            facts,
            separated: false,
        }
    }

    /// This list with a hairline rule between rows.
    #[must_use]
    pub fn with_separators(mut self, separated: bool) -> Self {
        self.separated = separated;
        self
    }

    /// The list's facts.
    #[must_use]
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }

    /// The number of facts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Whether the list carries no facts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// The height, in physical pixels, one row of the list needs at `scale`.
    ///
    /// One definition shared by [`measured_height`](Self::measured_height)
    /// and [`render`](Self::render), so a host laying several rows out
    /// cannot disagree with what the list actually draws.
    #[must_use]
    pub fn row_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        text_row_height(scale, theme, font)
    }

    /// The height, in physical pixels, this list needs at `scale` to draw
    /// every fact without clipping.
    #[must_use]
    pub fn measured_height(&self, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let count = u32::try_from(self.facts.len()).unwrap_or(u32::MAX);
        Self::row_height(scale, theme, font).saturating_mul(count)
    }

    /// Paint the list into `surface` at `bounds` for the active theme.
    ///
    /// A row draws only while a whole row still fits in `bounds`; the
    /// remainder is omitted rather than clipped mid-row (fail closed). Within
    /// a drawn row the value keeps its measured width and the label
    /// truncates into whatever remains, because the reading is what the
    /// reader came for. An empty list draws nothing at all.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        if self.facts.is_empty() {
            return;
        }
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let line_h = font.line_height();
        let row_h = Self::row_height(scale, theme, font);
        let bottom = y.saturating_add(h);
        let right = x.saturating_add(w);
        let sep_thickness = plate_border(theme, scale).min(gap);
        let last = self.facts.len().saturating_sub(1);

        let mut cursor_y = y;
        for (index, fact) in self.facts.iter().enumerate() {
            if cursor_y.saturating_add(line_h) > bottom {
                break;
            }

            let value_w = font.text_width(fact.value()).min(w);
            let fitted_value = font.truncate_to_width(fact.value(), value_w);
            let label_avail = w.saturating_sub(value_w).saturating_sub(gap);
            let fitted_label = font.truncate_to_width(fact.label(), label_avail);

            font.draw_text(
                surface,
                to_i32(x),
                to_i32(cursor_y),
                fitted_label,
                Color::from(palette.on_surface_muted),
            );
            let value_color = match fact.tone() {
                Some(role) => Color::from(palette.signal(role)),
                None => Color::from(palette.on_surface),
            };
            font.draw_text(
                surface,
                to_i32(right.saturating_sub(value_w)),
                to_i32(cursor_y),
                fitted_value,
                value_color,
            );

            let next_y = cursor_y.saturating_add(row_h);
            let next_row_fits = next_y.saturating_add(line_h) <= bottom;
            if self.separated && index != last && next_row_fits {
                let offset = gap.saturating_sub(sep_thickness) / 2;
                let sep_y = cursor_y
                    .saturating_add(line_h)
                    .saturating_add(offset)
                    .min(bottom.saturating_sub(sep_thickness));
                surface.fill_rect(x, sep_y, w, sep_thickness, Color::from(palette.rim));
            }
            cursor_y = next_y;
        }
    }
}

/// How an [`EventMark`] draws: the *shape* difference that keeps the two
/// [`TimelineEvent`] kinds legible without relying on hue.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EventMark {
    /// A routine step: a quiet hollow mark.
    Routine,
    /// A notable step the reader's eye should catch: a filled mark, toned.
    Notable,
}

/// One dated event of a [`Timeline`].
///
/// An event carries a display stamp, its text, a shape-coded mark, and an
/// optional [`SignalRole`] tone applied to a [`EventMark::Notable`] mark
/// (an untoned notable mark takes the theme accent).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineEvent {
    stamp: String,
    text: String,
    mark: EventMark,
    tone: Option<SignalRole>,
}

impl TimelineEvent {
    /// An event at `stamp` reading `text`, drawn as a routine mark.
    #[must_use]
    pub fn new(stamp: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            stamp: stamp.into(),
            text: text.into(),
            mark: EventMark::Routine,
            tone: None,
        }
    }

    /// This event with the given mark.
    #[must_use]
    pub fn with_mark(mut self, mark: EventMark) -> Self {
        self.mark = mark;
        self
    }

    /// This event with its mark toned by a semantic signal role.
    #[must_use]
    pub fn with_tone(mut self, tone: SignalRole) -> Self {
        self.tone = Some(tone);
        self
    }

    /// The event's display stamp.
    #[must_use]
    pub fn stamp(&self) -> &str {
        &self.stamp
    }

    /// The event's text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The event's mark.
    #[must_use]
    pub fn mark(&self) -> EventMark {
        self.mark
    }
}

/// The ordered record of what happened and when: a connector spine down the
/// left, a mark per event, a stamp column, and the event's text.
///
/// A timeline is an instrument, not an action: it has no pointer or keyboard
/// handling and reports nothing back to its owner. The owner supplies the
/// events, oldest first or newest first as it prefers, and re-renders when
/// they change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Timeline {
    events: Vec<TimelineEvent>,
}

impl Timeline {
    /// A timeline over `events`.
    #[must_use]
    pub fn new(events: Vec<TimelineEvent>) -> Self {
        Self { events }
    }

    /// The timeline's events.
    #[must_use]
    pub fn events(&self) -> &[TimelineEvent] {
        &self.events
    }

    /// The number of events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the timeline carries no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The height, in physical pixels, one row of the timeline needs at
    /// `scale`.
    ///
    /// One definition shared by [`measured_height`](Self::measured_height)
    /// and [`render`](Self::render), so a host laying several rows out
    /// cannot disagree with what the timeline actually draws.
    #[must_use]
    pub fn row_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        text_row_height(scale, theme, font)
    }

    /// The height, in physical pixels, this timeline needs at `scale` to
    /// draw every event without clipping.
    #[must_use]
    pub fn measured_height(&self, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        let count = u32::try_from(self.events.len()).unwrap_or(u32::MAX);
        Self::row_height(scale, theme, font).saturating_mul(count)
    }

    /// Width of the gutter (spine plus marks) before the stamp column.
    ///
    /// The gutter is exactly the mark's own diameter: the spine runs down
    /// its centre and every mark is centred within it, so nothing else needs
    /// to know the mark's geometry to lay out the column that follows.
    #[must_use]
    pub fn gutter_width(scale: Scale, theme: &Theme) -> u32 {
        mark_diameter(scale, theme)
    }

    /// Paint the timeline into `surface` at `bounds` for the active theme.
    ///
    /// A row draws only while a whole row still fits in `bounds`; the
    /// remainder is omitted rather than clipped mid-row (fail closed). The
    /// spine spans only from the first drawn mark's centre to the last
    /// drawn mark's centre, and is omitted entirely for a single event, so
    /// it never implies an event that was not recorded. The stamp column's
    /// width is the widest stamp measured through `font`, so every stamp
    /// aligns on one shared measurement. An empty timeline draws nothing at
    /// all.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        if self.events.is_empty() {
            return;
        }
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let line_h = font.line_height();
        let row_h = Self::row_height(scale, theme, font);
        let bottom = y.saturating_add(h);
        let right = x.saturating_add(w);

        let diameter = mark_diameter(scale, theme);
        let radius = diameter / 2;
        let gutter_w = Self::gutter_width(scale, theme);
        let spine_x = x.saturating_add(radius);
        let mark_fits = spine_x.saturating_add(radius) <= right;

        let stamp_w = self
            .events
            .iter()
            .map(|event| font.text_width(event.stamp()))
            .max()
            .unwrap_or(0);
        let stamp_x = x.saturating_add(gutter_w).saturating_add(gap);
        let text_x = stamp_x.saturating_add(stamp_w).saturating_add(gap);

        // Which rows fit, and each one's mark centre and top, computed once
        // so the spine (drawn first, beneath the marks) and the per-row
        // content agree on exactly the same rows without a second pass over
        // `self.events` re-deciding what fits.
        let mut rows: Vec<(&TimelineEvent, u32, u32)> = Vec::new();
        let mut cursor_y = y;
        for event in &self.events {
            if cursor_y.saturating_add(line_h) > bottom {
                break;
            }
            let center_y = cursor_y.saturating_add(line_h / 2);
            rows.push((event, cursor_y, center_y));
            cursor_y = cursor_y.saturating_add(row_h);
        }
        if rows.is_empty() {
            return;
        }

        if mark_fits {
            if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
                let (_, _, first_center) = *first;
                let (_, _, last_center) = *last;
                if last_center > first_center {
                    let thickness = plate_border(theme, scale);
                    let half = thickness / 2;
                    surface.fill_rect(
                        spine_x.saturating_sub(half),
                        first_center,
                        thickness,
                        last_center.saturating_sub(first_center),
                        Color::from(palette.rim),
                    );
                }
            }
        }

        for (event, row_top, center_y) in rows {
            if mark_fits {
                paint_mark(surface, theme, scale, spine_x, center_y, diameter, event);
            }
            if stamp_x < right {
                let avail = right.saturating_sub(stamp_x).min(stamp_w);
                let fitted = font.truncate_to_width(event.stamp(), avail);
                font.draw_text(
                    surface,
                    to_i32(stamp_x),
                    to_i32(row_top),
                    fitted,
                    Color::from(palette.on_surface_muted),
                );
            }
            if text_x < right {
                let avail = right.saturating_sub(text_x);
                let fitted = font.truncate_to_width(event.text(), avail);
                font.draw_text(
                    surface,
                    to_i32(text_x),
                    to_i32(row_top),
                    fitted,
                    Color::from(palette.on_surface),
                );
            }
        }
    }
}

/// One text-line row height shared by [`FactList`] and [`Timeline`]: the
/// font's line height plus the theme's control gap, so a caller stacking
/// several rows of either control uses one row pitch.
#[must_use]
fn text_row_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    font.line_height().saturating_add(gap)
}

/// The physical diameter of a [`TimelineEvent`] mark: the theme's Signal
/// Bead size, the same compact mark extent the shell surfaces already draw
/// their beads at.
#[must_use]
fn mark_diameter(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().bead_size).max(1)
}

/// Paint one event's mark centred at `(cx, cy)` with the given `diameter`: a
/// filled circle for [`EventMark::Notable`] (toned, or the accent when
/// untoned), a hollow ring for [`EventMark::Routine`] — drawn as two filled
/// circles, an outer rim-coloured disc and a smaller surface-coloured one,
/// so the shared circle helper is used both times and no new circle geometry
/// is written for the ring.
fn paint_mark(
    surface: &mut Surface,
    theme: &Theme,
    scale: Scale,
    cx: u32,
    cy: u32,
    diameter: u32,
    event: &TimelineEvent,
) {
    let radius = diameter / 2;
    let x = cx.saturating_sub(radius);
    let y = cy.saturating_sub(radius);
    let palette = theme.palette();
    match event.mark() {
        EventMark::Notable => {
            let color = match event.tone {
                Some(role) => Color::from(palette.signal(role)),
                None => Color::from(palette.accent),
            };
            paint_filled_circle(surface, x, y, diameter, color);
        }
        EventMark::Routine => {
            paint_filled_circle(surface, x, y, diameter, Color::from(palette.rim));
            let thickness = plate_border(theme, scale);
            let inner = diameter.saturating_sub(thickness.saturating_mul(2));
            if inner > 0 {
                let inset = diameter.saturating_sub(inner) / 2;
                paint_filled_circle(
                    surface,
                    x.saturating_add(inset),
                    y.saturating_add(inset),
                    inner,
                    Color::from(palette.surface),
                );
            }
        }
    }
}
