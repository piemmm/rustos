//! Where everything is drawn, in physical pixels.
//!
//! Every length is authored in *logical* pixels at the desktop's reference
//! density and converted through the one shared scale, so a dense display gets
//! a bigger board rather than a smaller one, and nothing here does the
//! conversion twice.
//!
//! The window is one header band — mine counter, new-game button, clock — over
//! a grid of square cells, centred in whatever is left. The cell side is
//! derived from the space available rather than fixed, so a resized window
//! shows the same board larger; the application declares a resize floor from
//! [`Layout::minimum`] so the grid can never be squeezed smaller than its
//! smallest legible cell.
//!
//! Pure integer arithmetic with no division by a value that can be zero: a
//! board always has at least one column and one row, and a cell side is clamped
//! to at least one pixel.

use tairix_geometry::{Point, Rect, Scale};

use crate::board::{Coord, Dimensions};

/// The smallest cell a board is drawn with, in logical pixels. The application's
/// declared resize floor keeps a window from ever being narrower than this.
const CELL_MIN: u32 = 18;
/// The cell size a window opens at, in logical pixels.
const CELL_IDEAL: u32 = 26;
/// The largest a cell grows to in an over-sized window, in logical pixels.
/// Past this the board stops growing and centres in the space instead.
const CELL_MAX: u32 = 46;
/// The space between two cells, in logical pixels.
const GAP: u32 = 2;
/// The space around the whole grid, in logical pixels.
const MARGIN: u32 = 14;
/// The header band's height, in logical pixels.
const HEADER_HEIGHT: u32 = 56;
/// The header's inner padding, in logical pixels.
const HEADER_PAD: u32 = 12;
/// A readout plate's width, in logical pixels: wide enough for a sign and three
/// digits, which is every value either readout can show.
const READOUT_WIDTH: u32 = 82;
/// A readout plate's height, in logical pixels.
const READOUT_HEIGHT: u32 = 34;
/// The new-game button's side, in logical pixels.
const FACE_SIDE: u32 = 40;

/// Where each part of the window is drawn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    /// The whole header band.
    pub header: Rect,
    /// The remaining-mine readout.
    pub counter: Rect,
    /// The new-game button.
    pub face: Rect,
    /// The elapsed-time readout.
    pub clock: Rect,
    /// The grid's bounding box, gaps included.
    pub grid: Rect,
    /// One cell's side, in physical pixels.
    pub cell: u32,
    /// The space between two cells, in physical pixels.
    pub gap: u32,
    /// Columns, so a hit test needs nothing else.
    cols: u16,
    /// Rows.
    rows: u16,
}

impl Layout {
    /// Lay `dims` out inside `client` at `scale`.
    #[must_use]
    pub fn resolve(client: Rect, dims: Dimensions, scale: Scale) -> Self {
        let pad = scale.scale_length(HEADER_PAD);
        let header_height = scale.scale_length(HEADER_HEIGHT).min(client.height);
        let header = Rect::new(client.left(), client.top(), client.width, header_height);

        let readout = Rect::new(
            0,
            0,
            scale.scale_length(READOUT_WIDTH),
            scale.scale_length(READOUT_HEIGHT),
        );
        // Both readouts sit on the header's vertical centre line; only their
        // horizontal edges differ.
        let row = centred_in(header, readout);
        let counter = Rect::new(
            header.left() + to_i32(pad),
            row.top(),
            row.width,
            row.height,
        );
        let clock = Rect::new(
            header.right() - to_i32(pad.saturating_add(row.width)),
            row.top(),
            row.width,
            row.height,
        );
        let face_side = scale.scale_length(FACE_SIDE);
        let face = centred_in(header, Rect::new(0, 0, face_side, face_side));

        let margin = scale.scale_length(MARGIN);
        let field = inset_below(client, header_height, margin);
        let gap = scale.scale_length(GAP);
        let cell = cell_side(field, dims, gap, scale);
        let grid_width = span(dims.cols(), cell, gap);
        let grid_height = span(dims.rows(), cell, gap);
        let grid = centred_in(field, Rect::new(0, 0, grid_width, grid_height));

        Self {
            header,
            counter,
            face,
            clock,
            grid,
            cell,
            gap,
            cols: dims.cols(),
            rows: dims.rows(),
        }
    }

    /// The client size a window of `dims` opens at, in physical pixels.
    #[must_use]
    pub fn preferred(dims: Dimensions, scale: Scale) -> (u32, u32) {
        window_for(dims, scale.scale_length(CELL_IDEAL).max(1), scale)
    }

    /// The smallest client size the grid stays legible in, in physical pixels —
    /// the resize floor the application declares to the window manager.
    #[must_use]
    pub fn minimum(dims: Dimensions, scale: Scale) -> (u32, u32) {
        window_for(dims, scale.scale_length(CELL_MIN).max(1), scale)
    }

    /// Where `at` is drawn. Off-board coordinates give an empty rectangle, so a
    /// stale coordinate paints nothing rather than somewhere wrong.
    #[must_use]
    pub fn cell_rect(&self, at: Coord) -> Rect {
        if at.col >= self.cols || at.row >= self.rows {
            return Rect::EMPTY;
        }
        let step = self.cell.saturating_add(self.gap);
        Rect::new(
            self.grid.left() + offset(at.col, step),
            self.grid.top() + offset(at.row, step),
            self.cell,
            self.cell,
        )
    }

    /// Where `at` is drawn, grown by one gap on every side: what a repaint of
    /// that cell must cover, because a cell's motion spills into the gutter
    /// around it (a shadow, a mark landing over its edge).
    #[must_use]
    pub fn cell_damage(&self, at: Coord) -> Rect {
        self.cell_damage_reaching(at, 0)
    }

    /// [`cell_damage`](Self::cell_damage) grown by a further `reach`, for a
    /// wave that draws past the tile it belongs to.
    ///
    /// A repaint scoped to the tile alone would clip such a wave and draw a
    /// square edge across it.
    #[must_use]
    pub fn cell_damage_reaching(&self, at: Coord, reach: u32) -> Rect {
        let rect = self.cell_rect(at);
        if rect.is_empty() {
            return rect;
        }
        let bleed = self.gap.max(1).saturating_add(reach);
        Rect::new(
            rect.left() - to_i32(bleed),
            rect.top() - to_i32(bleed),
            rect.width.saturating_add(bleed.saturating_mul(2)),
            rect.height.saturating_add(bleed.saturating_mul(2)),
        )
    }

    /// The cell under `point`, or `None` when the point is outside the grid.
    ///
    /// A point in the gutter belongs to the cell it is nearest, because a
    /// two-pixel gap is not something a player aims around and a click that
    /// lands in one should not be silently discarded.
    #[must_use]
    pub fn cell_at(&self, point: Point) -> Option<Coord> {
        if !self.grid.contains(point) {
            return None;
        }
        let step = self.cell.saturating_add(self.gap).max(1);
        let col = index(point.x - self.grid.left(), step, self.cols)?;
        let row = index(point.y - self.grid.top(), step, self.rows)?;
        Some(Coord::new(col, row))
    }
}

/// The pixels `count` cells and the gaps between them occupy.
fn span(count: u16, cell: u32, gap: u32) -> u32 {
    let count = u32::from(count);
    cell.saturating_mul(count)
        .saturating_add(gap.saturating_mul(count.saturating_sub(1)))
}

/// The client size a board of `dims` needs when each cell is `cell` pixels.
fn window_for(dims: Dimensions, cell: u32, scale: Scale) -> (u32, u32) {
    let gap = scale.scale_length(GAP);
    let margin = scale.scale_length(MARGIN);
    let width = span(dims.cols(), cell, gap).saturating_add(margin.saturating_mul(2));
    let height = span(dims.rows(), cell, gap)
        .saturating_add(margin.saturating_mul(2))
        .saturating_add(scale.scale_length(HEADER_HEIGHT));
    (width.max(1), height.max(1))
}

/// The largest cell side that fits `dims` inside `field`, within the legibility
/// bounds.
fn cell_side(field: Rect, dims: Dimensions, gap: u32, scale: Scale) -> u32 {
    let fit = |extent: u32, count: u16| {
        let count = u32::from(count).max(1);
        extent.saturating_sub(gap.saturating_mul(count.saturating_sub(1))) / count
    };
    let smallest = scale.scale_length(CELL_MIN).max(1);
    let largest = scale.scale_length(CELL_MAX).max(smallest);
    fit(field.width, dims.cols())
        .min(fit(field.height, dims.rows()))
        .clamp(smallest, largest)
}

/// `client` with `header` taken off the top and `margin` off every side, never
/// inverted.
fn inset_below(client: Rect, header: u32, margin: u32) -> Rect {
    let top = client.top() + to_i32(header.saturating_add(margin));
    Rect::new(
        client.left() + to_i32(margin),
        top,
        client.width.saturating_sub(margin.saturating_mul(2)),
        client
            .height
            .saturating_sub(header)
            .saturating_sub(margin.saturating_mul(2)),
    )
}

/// `inner`'s size, centred on `outer`.
fn centred_in(outer: Rect, inner: Rect) -> Rect {
    let slack = |outer: u32, inner: u32| to_i32(outer.saturating_sub(inner) / 2);
    Rect::new(
        outer.left() + slack(outer.width, inner.width),
        outer.top() + slack(outer.height, inner.height),
        inner.width,
        inner.height,
    )
}

/// The pixel offset of the `n`th cell along an axis.
fn offset(n: u16, step: u32) -> i32 {
    to_i32(u32::from(n).saturating_mul(step))
}

/// A pixel length as a signed coordinate, saturating rather than wrapping.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Which cell a distance along an axis falls in, clamped to the last one so a
/// point in the trailing gutter belongs to the cell beside it.
fn index(distance: i32, step: u32, count: u16) -> Option<u16> {
    let distance = u32::try_from(distance).ok()?;
    let last = count.checked_sub(1)?;
    Some(u16::try_from(distance / step).unwrap_or(last).min(last))
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
