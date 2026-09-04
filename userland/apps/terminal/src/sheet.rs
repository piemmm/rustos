//! The settings sheet's picture, retained between frames, and the rectangle
//! it owes its next paint.
//!
//! [`crate::render::Screen`] is this for the terminal's grid; this is it for
//! the sheet, and for the same reason. The sheet's controls already report the
//! rectangles they change — that is what the shared damage sink is for — but
//! the popup was re-derived from nothing on every pointer sample: a
//! whole-sheet surface allocated afresh, every tab, row, label and swatch
//! re-rendered, and the whole window presented. A slider drag delivers samples
//! as fast as a hand moves, so the cost of one edit was the cost of the whole
//! sheet, several dozen times a second, and the knob lagged the pointer.
//!
//! Retaining the picture is what makes a reported rectangle worth anything:
//! the pixels outside it are already right, so the paint clips to what moved
//! and the present carries only that.

use tairix_controls::damage;
use tairix_geometry::{Rect, Region, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::settings::Settings;

/// The sheet's painted picture and what it owes.
#[derive(Debug)]
pub struct SheetScreen {
    /// The picture as last painted, whole.
    surface: Surface,
    /// The rectangles the sheet's controls have reported since that paint.
    /// The shared sink, so a batch of pointer samples degrades to the box it
    /// may as well have been rather than growing a rectangle per sample.
    damage: Region,
    /// Whether the surface holds pixels no report describes, which is what
    /// makes the next paint cover the sheet: before the first paint, and after
    /// anything a control could not have reported — a re-theme, a scale
    /// change, a profile adopted from the store, a frame region the session
    /// took back.
    stale: bool,
}

impl SheetScreen {
    /// A `width_px` × `height_px` picture with nothing painted yet, so the
    /// first [`paint`](Self::paint) draws the whole sheet.
    ///
    /// Returns `None` only when those dimensions cannot be allocated, so the
    /// caller fails closed rather than panicking.
    #[must_use]
    pub fn new(width_px: u32, height_px: u32) -> Option<Self> {
        Some(Self {
            surface: Surface::new(width_px, height_px)?,
            damage: damage::sink(),
            stale: true,
        })
    }

    /// The painted picture.
    #[must_use]
    pub const fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Where the sheet's controls report what they change.
    ///
    /// Handed to the routing for one event or one drained batch; the paint
    /// then covers everything reported since it last ran.
    pub const fn sink(&mut self) -> &mut Region {
        &mut self.damage
    }

    /// Mark every pixel stale, so the next paint covers the sheet.
    ///
    /// For a change no control could have reported: a re-theme, a new scale, a
    /// profile adopted from somewhere other than these widgets, or a frame
    /// region that was released and re-attached and so holds none of the
    /// pixels a partial present would leave standing.
    pub const fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Paint whatever is owed and answer the rectangle it covers, which is
    /// [`Rect::EMPTY`] when nothing was owed.
    ///
    /// `viewport` is the sheet's whole extent throughout: the sheet lays
    /// itself out against it and draws in its own coordinates, and the clip is
    /// what confines the result to the rectangle that moved — so a scoped
    /// paint is pixel-identical to the same rectangle of a whole one.
    pub fn paint(&mut self, sheet: &Settings, viewport: Rect, scale: Scale, theme: &Theme) -> Rect {
        let bounds = Rect::new(
            0,
            0,
            self.surface.width().min(viewport.width),
            self.surface.height().min(viewport.height),
        );
        let rect = if self.stale {
            bounds
        } else {
            self.damage.bounds().intersection(&bounds)
        };
        self.damage.clear();
        self.stale = false;
        if rect.is_empty() {
            return Rect::EMPTY;
        }
        let (Ok(x), Ok(y)) = (u32::try_from(rect.left()), u32::try_from(rect.top())) else {
            return Rect::EMPTY;
        };
        self.surface.with_clip(x, y, rect.width, rect.height, |s| {
            // The sheet's panel is centred and does not fill the popup, and
            // what shows through around it is the terminal behind. Clearing
            // first is what keeps a repaint from compositing over the pixels
            // it is replacing.
            s.fill(Color::TRANSPARENT);
            sheet.render(s, viewport, scale, theme);
        });
        rect
    }
}

#[cfg(test)]
#[path = "sheet_tests.rs"]
mod tests;
