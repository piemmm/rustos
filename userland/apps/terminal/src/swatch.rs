//! The colour-well grid: an ordered set of named colour swatches with one
//! selected index, used by the settings sheet's custom-scheme editor.
//!
//! `plans/GUI-CONTROLS-DESIGN.md` §4 keeps a control in its owning
//! application until a second independent consumer needs it, so this stays
//! app-local rather than moving to `lib/controls`. It still follows the
//! Reactive Alloy rules the shared controls do: every length is authored
//! logically and converted through [`Scale`], every colour and radius comes
//! from the active [`Theme`], and selection reads by shape as well as by
//! colour, since a well's own colour cannot be relied on for contrast.
//!
//! [`SwatchGrid::from_scheme`] lays a [`ColorScheme`]'s twenty colours —
//! background, foreground, cursor, cursor text, then the sixteen ANSI
//! colours — into a fixed five-column, four-row grid, and
//! [`SwatchGrid::apply_to`] writes them back; the two are exact inverses of
//! each other.

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{Contrast, Theme};

use crate::scheme::{ColorScheme, Rgb, ANSI_COLORS};

/// The number of colour wells a full scheme lays out: the four screen roles
/// (background, foreground, cursor, cursor text) plus the sixteen ANSI
/// colours.
pub const WELL_COUNT: usize = ANSI_COLORS + 4;

/// Wells per row of the grid.
const COLUMNS: usize = 5;

/// Rows the grid lays its wells into.
const ROWS: usize = WELL_COUNT / COLUMNS;

// `cell_rect` divides the grid into exactly `COLUMNS * ROWS` wells; a
// remainder would silently drop or duplicate a well.
const _: () = assert!(WELL_COUNT.is_multiple_of(COLUMNS));

/// The accessible/tooltip name of each ANSI colour slot, normal then bright,
/// in [`ColorScheme::ansi`] order.
const ANSI_LABELS: [&str; ANSI_COLORS] = [
    "Black",
    "Red",
    "Green",
    "Yellow",
    "Blue",
    "Magenta",
    "Cyan",
    "White",
    "Bright black",
    "Bright red",
    "Bright green",
    "Bright yellow",
    "Bright blue",
    "Bright magenta",
    "Bright cyan",
    "Bright white",
];

/// One named colour well of a [`SwatchGrid`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Swatch {
    label: &'static str,
    color: Rgb,
}

impl Swatch {
    /// A well named `label` holding `color`.
    #[must_use]
    const fn new(label: &'static str, color: Rgb) -> Self {
        Self { label, color }
    }

    /// The well's accessible/tooltip name (e.g. "Background", "Bright red").
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// The well's colour.
    #[must_use]
    pub const fn color(&self) -> Rgb {
        self.color
    }
}

/// An ordered grid of named colour wells with one selected index.
///
/// [`from_scheme`](Self::from_scheme) and [`apply_to`](Self::apply_to) are
/// exact inverses: applying a grid built from a scheme back onto a fresh copy
/// of that scheme reproduces it exactly.
#[derive(Copy, Clone, Debug)]
pub struct SwatchGrid {
    wells: [Swatch; WELL_COUNT],
    selected: usize,
    /// The last pointer position — hit-testing input, never a drawn
    /// property.
    pointer: Point,
    /// The well armed by a primary press, committed only if the matching
    /// release lands over the same well.
    armed: Option<usize>,
}

impl SwatchGrid {
    /// The grid over `scheme`'s twenty colours, in the fixed order
    /// Background, Foreground, Cursor, Cursor text, then the sixteen ANSI
    /// colours — with well `0` (Background) selected.
    #[must_use]
    pub fn from_scheme(scheme: &ColorScheme) -> Self {
        Self {
            wells: wells_from_scheme(scheme),
            selected: 0,
            pointer: Point::ORIGIN,
            armed: None,
        }
    }

    /// Write this grid's twenty colours back onto `scheme`, in the same
    /// fixed order [`from_scheme`](Self::from_scheme) reads them in.
    pub fn apply_to(&self, scheme: &mut ColorScheme) {
        let [background, foreground, cursor, cursor_text, ansi @ ..] = self.wells;
        scheme.background = background.color;
        scheme.foreground = foreground.color;
        scheme.cursor = cursor.color;
        scheme.cursor_text = cursor_text.color;
        for (slot, well) in scheme.ansi.iter_mut().zip(ansi) {
            *slot = well.color;
        }
    }

    /// The index of the currently selected well.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// Select well `index`; an out-of-range index is ignored (fail closed).
    pub fn set_selected(&mut self, index: usize) {
        if index < WELL_COUNT {
            self.selected = index;
        }
    }

    /// The colour of well `index`, or `None` if it is out of range.
    #[must_use]
    pub fn color(&self, index: usize) -> Option<Rgb> {
        self.wells.get(index).map(Swatch::color)
    }

    /// Set the colour of well `index`; an out-of-range index is ignored
    /// (fail closed).
    pub fn set_color(&mut self, index: usize, color: Rgb) {
        if let Some(well) = self.wells.get_mut(index) {
            well.color = color;
        }
    }

    /// The accessible/tooltip name of well `index`, or `None` if it is out
    /// of range.
    #[must_use]
    pub fn label(&self, index: usize) -> Option<&'static str> {
        self.wells.get(index).map(Swatch::label)
    }

    /// The height the grid needs at `scale` under `theme`: every row of
    /// wells at the theme's control height, separated by its control gap.
    #[must_use]
    pub fn preferred_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let side = well_side(scale, theme);
        let gap = well_gap(scale, theme);
        let rows = u32::try_from(ROWS).unwrap_or(u32::MAX);
        side.saturating_mul(rows)
            .saturating_add(gap.saturating_mul(rows.saturating_sub(1)))
    }

    /// Paint the grid into `surface` at `bounds` for the active theme.
    ///
    /// Each well is a rounded plate filled with its own colour, on a
    /// hairline rim so a well the same colour as the panel stays visible.
    /// The selected well additionally carries an accent ring and a drawn
    /// mark, so selection reads by shape as well as by colour — the wells
    /// hold arbitrary colours, so hue alone cannot carry the state.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        // Wells are pure colour plates; a well's name is accessible/tooltip
        // metadata the caller draws where it needs to, never text on the
        // swatch face, so the font is accepted only to keep the shape every
        // control's `render` shares.
        let _ = font;
        for index in 0..WELL_COUNT {
            if let Some(cell) = cell_rect(bounds, index) {
                self.paint_well(surface, index, cell, scale, theme);
            }
        }
    }

    /// Paint one well's plate, rim, and (if selected) accent ring and mark.
    fn paint_well(
        &self,
        surface: &mut Surface,
        index: usize,
        cell: (u32, u32, u32, u32),
        scale: Scale,
        theme: &Theme,
    ) {
        let Some(well) = self.wells.get(index).copied() else {
            return;
        };
        let (cx, cy, cw, ch) = cell;
        let margin = well_gap(scale, theme) / 2;
        let Some((x, y, w, h)) = inset(cx, cy, cw, ch, margin) else {
            return;
        };
        let radius = scale
            .scale_length(theme.metrics().control_corner_radius)
            .min(w / 2)
            .min(h / 2);
        surface.fill_round_rect(x, y, w, h, radius, well.color.opaque());

        let border = plate_border(theme, scale);
        draw_outline(
            surface,
            x,
            y,
            w,
            h,
            border,
            Color::from(theme.palette().rim),
        );

        if index == self.selected {
            let ring = border.saturating_mul(2).max(1).min(w / 2).min(h / 2);
            draw_outline(
                surface,
                x,
                y,
                w,
                h,
                ring,
                Color::from(theme.palette().rim_active),
            );
            paint_selection_mark(surface, (x, y, w, h), well.color);
        }
    }

    /// Feed a pointer event; a primary press-and-release completed over the
    /// same well selects it.
    pub fn on_pointer(&mut self, event: &InputEvent, bounds: Rect) -> Option<SwatchAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        let over = well_at(bounds, self.pointer);
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                self.armed = over;
                None
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                match (armed, over) {
                    (Some(a), Some(o)) if a == o => {
                        self.selected = o;
                        Some(SwatchAction::Selected { index: o })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key event; Left/Right move the selection by one well, wrapping
    /// from the last well to the first (and back); Up/Down move by one row,
    /// wrapping within the same column.
    pub fn on_key(&mut self, key: Key) -> Option<SwatchAction> {
        let columns = to_i32(u32::try_from(COLUMNS).unwrap_or(1));
        let delta = match key {
            Key::Named(NamedKey::Right) => 1,
            Key::Named(NamedKey::Left) => -1,
            Key::Named(NamedKey::Down) => columns,
            Key::Named(NamedKey::Up) => -columns,
            _ => return None,
        };
        let len = to_i32(u32::try_from(WELL_COUNT).unwrap_or(1));
        let current = to_i32(u32::try_from(self.selected).unwrap_or(0));
        let next = usize::try_from(current.wrapping_add(delta).rem_euclid(len.max(1))).unwrap_or(0);
        self.selected = next;
        Some(SwatchAction::Selected { index: next })
    }
}

/// What routing an input event into a [`SwatchGrid`] concluded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwatchAction {
    /// The well at `index` became the selected one.
    Selected {
        /// The zero-based index of the newly selected well.
        index: usize,
    },
}

/// The twenty wells of `scheme`, in the fixed order [`SwatchGrid::from_scheme`]
/// documents.
fn wells_from_scheme(scheme: &ColorScheme) -> [Swatch; WELL_COUNT] {
    let ansi = scheme.ansi;
    [
        Swatch::new("Background", scheme.background),
        Swatch::new("Foreground", scheme.foreground),
        Swatch::new("Cursor", scheme.cursor),
        Swatch::new("Cursor text", scheme.cursor_text),
        Swatch::new(ANSI_LABELS[0], ansi[0]),
        Swatch::new(ANSI_LABELS[1], ansi[1]),
        Swatch::new(ANSI_LABELS[2], ansi[2]),
        Swatch::new(ANSI_LABELS[3], ansi[3]),
        Swatch::new(ANSI_LABELS[4], ansi[4]),
        Swatch::new(ANSI_LABELS[5], ansi[5]),
        Swatch::new(ANSI_LABELS[6], ansi[6]),
        Swatch::new(ANSI_LABELS[7], ansi[7]),
        Swatch::new(ANSI_LABELS[8], ansi[8]),
        Swatch::new(ANSI_LABELS[9], ansi[9]),
        Swatch::new(ANSI_LABELS[10], ansi[10]),
        Swatch::new(ANSI_LABELS[11], ansi[11]),
        Swatch::new(ANSI_LABELS[12], ansi[12]),
        Swatch::new(ANSI_LABELS[13], ansi[13]),
        Swatch::new(ANSI_LABELS[14], ansi[14]),
        Swatch::new(ANSI_LABELS[15], ansi[15]),
    ]
}

/// The scaled square side of one well, from the theme's control height.
fn well_side(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().control_height).max(1)
}

/// The scaled gap between wells, from the theme's control gap.
fn well_gap(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().control_gap).max(1)
}

/// Whether the theme asks for the heavier-contrast treatment.
fn heavy_contrast(theme: &Theme) -> bool {
    !matches!(theme.contrast(), Contrast::Normal)
}

/// The scaled plate rim thickness, doubled under heavy contrast — the same
/// recipe the shared controls use, reimplemented here since it is private to
/// `lib/controls`.
fn plate_border(theme: &Theme, scale: Scale) -> u32 {
    scale
        .scale_length(theme.metrics().border_thickness)
        .max(1)
        .saturating_mul(if heavy_contrast(theme) { 2 } else { 1 })
}

/// Clamp a logical rectangle's origin into non-negative surface coordinates,
/// or `None` if it lies off the top-left.
fn surface_rect(bounds: Rect) -> Option<(u32, u32, u32, u32)> {
    let x = u32::try_from(bounds.left()).ok()?;
    let y = u32::try_from(bounds.top()).ok()?;
    Some((x, y, bounds.width, bounds.height))
}

/// Inset a surface rectangle by `by` on every side, or `None` if it collapses.
fn inset(x: u32, y: u32, w: u32, h: u32, by: u32) -> Option<(u32, u32, u32, u32)> {
    let iw = w.checked_sub(by.saturating_mul(2))?;
    let ih = h.checked_sub(by.saturating_mul(2))?;
    if iw == 0 || ih == 0 {
        return None;
    }
    Some((x + by, y + by, iw, ih))
}

/// Draw a hollow rectangular outline of `thickness` inside `(x, y, w, h)`.
fn draw_outline(
    surface: &mut Surface,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    thickness: u32,
    color: Color,
) {
    if w == 0 || h == 0 || thickness == 0 {
        return;
    }
    let edge = thickness.min(w).min(h);
    surface.fill_rect(x, y, w, edge, color);
    surface.fill_rect(x, y + h - edge, w, edge, color);
    surface.fill_rect(x, y, edge, h, color);
    surface.fill_rect(x + w - edge, y, edge, h, color);
}

/// Paint the selection mark: a small diamond in whichever of black or white
/// contrasts with `well_color`, so the mark reads over any hue the well
/// happens to hold.
fn paint_selection_mark(surface: &mut Surface, cell: (u32, u32, u32, u32), well_color: Rgb) {
    let (x, y, w, h) = cell;
    let side = (w.min(h) / 2).max(4).min(w).min(h);
    if side == 0 {
        return;
    }
    let Some(mut mark) = Surface::new(side, side) else {
        return;
    };
    let span = to_i32(side);
    let color = if well_color.luminance() > 128 {
        Color::rgb(0, 0, 0)
    } else {
        Color::rgb(255, 255, 255)
    };
    let points: [(i32, i32); 4] = [
        (span / 2, 0),
        (span, span / 2),
        (span / 2, span),
        (0, span / 2),
    ];
    mark.fill_polygon(&points, side, color);
    let mx = to_i32(x) + (to_i32(w) - span) / 2;
    let my = to_i32(y) + (to_i32(h) - span) / 2;
    surface.blit(mx, my, &mark);
}

/// The pixel span `(offset, length)` of share `index` of `count` equal shares
/// splitting `total` pixels, with the rounding remainder distributed one
/// pixel at a time to the leading shares so every pixel of `total` is
/// covered exactly once. `None` for an out-of-range `index`.
fn axis_span(index: usize, count: usize, total: u32) -> Option<(u32, u32)> {
    if count == 0 || index >= count {
        return None;
    }
    let count_u32 = u32::try_from(count).ok()?;
    let index_u32 = u32::try_from(index).ok()?;
    let base = total / count_u32;
    let remainder = total % count_u32;
    let extra_before = remainder.min(index_u32);
    let this_extra = u32::from(index_u32 < remainder);
    let offset = base.saturating_mul(index_u32).saturating_add(extra_before);
    let length = base.saturating_add(this_extra);
    Some((offset, length))
}

/// The surface rectangle `(x, y, w, h)` of well `index` within `bounds`, or
/// `None` if `index` is out of range or `bounds` collapses.
///
/// Wells share `bounds` evenly across [`COLUMNS`] columns and [`ROWS`] rows;
/// this is the one layout both [`SwatchGrid::render`] and
/// [`SwatchGrid::on_pointer`] read, so drawing and hit-testing can never
/// disagree.
fn cell_rect(bounds: Rect, index: usize) -> Option<(u32, u32, u32, u32)> {
    if index >= WELL_COUNT {
        return None;
    }
    let (x, y, w, h) = surface_rect(bounds)?;
    if w == 0 || h == 0 {
        return None;
    }
    let row = index / COLUMNS;
    let col = index % COLUMNS;
    let (cx, cw) = axis_span(col, COLUMNS, w)?;
    let (cy, ch) = axis_span(row, ROWS, h)?;
    Some((x + cx, y + cy, cw, ch))
}

/// The well `point` falls over within `bounds`, or `None` if it is over none
/// of them.
fn well_at(bounds: Rect, point: Point) -> Option<usize> {
    (0..WELL_COUNT).find(|&index| {
        cell_rect(bounds, index)
            .is_some_and(|(x, y, w, h)| Rect::new(to_i32(x), to_i32(y), w, h).contains(point))
    })
}

#[cfg(test)]
#[path = "swatch_tests.rs"]
mod tests;
