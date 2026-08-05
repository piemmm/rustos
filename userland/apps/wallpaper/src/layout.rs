//! The chooser's window geometry: the one function every painter and every
//! hit-test agrees on.
//!
//! The window is three bands stacked between equal margins:
//!
//! ```text
//! +--------------------------------------------------------------+
//! |  +---------------------------+  Fit      [ Fill screen   v ] |
//! |  |                           |  Backdrop [ Theme         v ] |
//! |  |       live preview        |  Icons    [ Top left      v ] |
//! |  |                           |  Sort     [ Name          v ] |
//! |  +---------------------------+  tairix-dark.jpg              |
//! |  Wallpapers                                                  |
//! |  +---------------------------------------------------+ +--+  |
//! |  |  [tile]  [tile]  [tile]  [tile]  [tile]           | |##|  |
//! |  |  [tile]  [tile]                                   | |  |  |
//! |  +---------------------------------------------------+ +--+  |
//! |  Applied.                                  [Close] [Apply]   |
//! +--------------------------------------------------------------+
//! ```
//!
//! Every extent is derived from the active theme's metrics at the desktop UI
//! scale and from the text face's own line height — never from a pixel
//! constant that a denser theme or a larger scale would leave wrong. The
//! footer is claimed first, from the bottom edge upward, so however small the
//! window becomes the Apply and Close buttons stay reachable and only the
//! preview band and the gallery give up room. Every region is total: a window
//! too small for a region yields an empty rectangle there, which every
//! painter and hit-test treats as absent rather than as an error.
//!
//! The preview band itself is shaped to the real screen's own aspect ratio
//! ([`Layout::compute`] is handed the desktop's screen extent for exactly
//! this), and [`Layout::preview_model`] is the largest rectangle of that
//! same aspect ratio, centred, that fits inside the band — the shared
//! placement geometry's own [`WallpaperFit::Fit`] contains the screen's shape
//! inside the band precisely as it would contain any other source inside any
//! other destination, so the model is never a second, private fit
//! computation. Whatever the band's own proportions leave over is left as
//! plain window background, so the model reads as a screen sitting inside
//! the panel rather than filling it edge to edge.

use tairix_browse::layout::{GridFill, GridFlow, GridMetrics, GridView};
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_theme::Theme;
use tairix_wallpaper::WallpaperFit;

use crate::{to_i32, OptionGroup, OPTION_GROUP_COUNT};

/// The option column's field width in logical pixels: room for the longest
/// choice any of the four groups offers, so the four fields line up as one
/// column instead of measuring themselves individually.
const OPTION_FIELD_WIDTH: u32 = 148;

/// The share of the content width the option column may take, in
/// twenty-fourths, so the preview keeps the larger part of a narrow window.
const OPTION_COLUMN_SHARE: u32 = 10;

/// The share of the body height the preview band may take, in
/// twenty-fourths.
///
/// A preview that simply grew with the window would push the gallery down to
/// a single row on a tall screen, which is the opposite of what a taller
/// window is for: the extra height belongs to the wallpapers being chosen
/// between.
const PREVIEW_BAND_SHARE: u32 = 10;

/// One gallery tile's width in logical pixels.
const TILE_WIDTH: u32 = 132;

/// One gallery tile's height in logical pixels: the square picture the tile
/// derives from its own bounds, plus the label line beneath it.
const TILE_HEIGHT: u32 = 116;

/// The narrowest a footer button is drawn, in logical pixels.
const BUTTON_WIDTH: u32 = 92;

/// The chooser's resolved window geometry.
///
/// Built by [`Layout::compute`] and consumed unchanged by both the painter
/// and the pointer hit-test, so what the user sees and what a click lands on
/// can never disagree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    preview: Rect,
    preview_model: Rect,
    option_labels: [Rect; OPTION_GROUP_COUNT],
    option_fields: [Rect; OPTION_GROUP_COUNT],
    caption: Rect,
    heading: Rect,
    tiles: Rect,
    scrollbar: Rect,
    status: Rect,
    apply: Rect,
    close: Rect,
    tile_metrics: GridMetrics,
}

impl Layout {
    /// Resolve the geometry of a `width` x `height` client area for the
    /// active theme, UI scale, and text face, with the preview shaped to
    /// `screen` — the desktop's own screen extent, in physical pixels.
    #[must_use]
    pub fn compute(
        width: u32,
        height: u32,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        screen: (u32, u32),
    ) -> Self {
        let metrics = theme.metrics();
        let gap = scale.scale_length(metrics.control_gap).max(1);
        let margin = gap.saturating_mul(2);
        let line = font.line_height().max(1);
        let row = scale.scale_length(metrics.control_height).max(line);
        let gutter = scale.scale_length(metrics.scrollbar_breadth).max(1);

        let left = margin;
        let content_w = width.saturating_sub(margin.saturating_mul(2));

        // The footer is claimed from the bottom edge upward so the buttons
        // survive any window size; everything above it shares what is left.
        let footer_bottom = height.saturating_sub(margin);
        let footer_h = row.min(footer_bottom);
        let footer_y = footer_bottom.saturating_sub(footer_h);
        let body_bottom = footer_y.saturating_sub(gap).min(footer_y);
        let body_top = margin.min(body_bottom);
        let body_h = body_bottom.saturating_sub(body_top);

        // The option column claims the least it can read in; the preview
        // takes the screen's own shape, scaled to fit what is left, and
        // whatever the preview's proportions do not use widens the column
        // rather than sitting between them as a hole.
        let (screen_w, screen_h) = (screen.0.max(1), screen.1.max(1));
        let least_column_w = Self::option_column_width(content_w, scale, font);
        let free_w = content_w
            .saturating_sub(least_column_w)
            .saturating_sub(if least_column_w == 0 { 0 } else { gap });
        let band_h = Self::band_height(body_h, free_w, row, line, gap, scale, screen_w, screen_h);
        let preview_w = free_w.min(
            band_h
                .saturating_mul(screen_w)
                .checked_div(screen_h)
                .unwrap_or(0),
        );
        let column_w = content_w
            .saturating_sub(preview_w)
            .saturating_sub(if preview_w == 0 { 0 } else { gap });

        let column_x = left.saturating_add(content_w).saturating_sub(column_w);
        let (option_labels, option_fields, caption) =
            Self::option_column(column_x, body_top, column_w, band_h, row, line, gap, font);

        let heading_y = body_top.saturating_add(band_h).saturating_add(gap);
        let heading_h = line.min(body_bottom.saturating_sub(heading_y.min(body_bottom)));
        let tiles_y = heading_y.saturating_add(heading_h).saturating_add(gap);
        let tiles_h = body_bottom.saturating_sub(tiles_y.min(body_bottom));
        let tiles_w = content_w
            .saturating_sub(gutter)
            .saturating_sub(if gutter == 0 { 0 } else { gap });

        let (apply, close, status) =
            Self::footer(left, footer_y, content_w, footer_h, gap, scale, font);

        let preview = Rect::new(to_i32(left), to_i32(body_top), preview_w, band_h);
        Self {
            preview,
            preview_model: Self::screen_model_box(preview, screen_w, screen_h),
            option_labels,
            option_fields,
            caption,
            heading: Rect::new(to_i32(left), to_i32(heading_y), content_w, heading_h),
            tiles: Rect::new(to_i32(left), to_i32(tiles_y), tiles_w, tiles_h),
            scrollbar: Rect::new(
                to_i32(left.saturating_add(content_w).saturating_sub(gutter)),
                to_i32(tiles_y),
                gutter,
                tiles_h,
            ),
            status,
            apply,
            close,
            tile_metrics: GridMetrics {
                cell_width: scale.scale_length(TILE_WIDTH).max(1),
                cell_height: scale.scale_length(TILE_HEIGHT).max(1),
                gap,
            },
        }
    }

    /// The width the four option rows need: their widest label, the shared
    /// field width, and the gap between them — capped so the preview keeps
    /// the larger share of a narrow window, and dropped entirely when the
    /// window cannot afford a legible column.
    fn option_column_width(content_w: u32, scale: Scale, font: BitmapFont) -> u32 {
        let label_w = OptionGroup::ALL
            .iter()
            .map(|group| font.text_width(group.label()))
            .max()
            .unwrap_or(0);
        let ideal = label_w
            .saturating_add(font.text_width(" "))
            .saturating_add(scale.scale_length(OPTION_FIELD_WIDTH));
        let ceiling = content_w
            .saturating_mul(OPTION_COLUMN_SHARE)
            .checked_div(24)
            .unwrap_or(0);
        if ceiling < label_w {
            return 0;
        }
        ideal.min(ceiling)
    }

    /// The preview band's height: the screen's own shape scaled to the
    /// width available, capped at its share of the window so the gallery
    /// keeps the rest, never less than the option column needs, and never so
    /// tall that the gallery loses its first row of tiles.
    #[expect(
        clippy::too_many_arguments,
        reason = "one private claim over the band's own resolved metrics plus the \
                  screen aspect it is shaped to; grouping them would only move the \
                  same values behind a type no other caller has a use for"
    )]
    fn band_height(
        body_h: u32,
        free_w: u32,
        row: u32,
        line: u32,
        gap: u32,
        scale: Scale,
        screen_w: u32,
        screen_h: u32,
    ) -> u32 {
        let wanted = free_w
            .saturating_mul(screen_h)
            .checked_div(screen_w)
            .unwrap_or(0);
        let share = body_h
            .saturating_mul(PREVIEW_BAND_SHARE)
            .checked_div(24)
            .unwrap_or(0);
        let column = row
            .saturating_mul(u32::try_from(OPTION_GROUP_COUNT).unwrap_or(4))
            .saturating_add(gap.saturating_mul(4))
            .saturating_add(line);
        let gallery_floor = scale
            .scale_length(TILE_HEIGHT)
            .saturating_add(line)
            .saturating_add(gap.saturating_mul(2));
        let ceiling = body_h.saturating_sub(gallery_floor);
        wanted.min(share).max(column).min(ceiling)
    }

    /// Claim the option column top-down: one label-and-field row per group,
    /// then the caption naming what the preview shows. A row the band cannot
    /// afford is empty, so a squeezed window drops rows from the bottom
    /// rather than drawing them over the gallery.
    #[expect(
        clippy::too_many_arguments,
        reason = "one private claim over the column's own resolved metrics; \
                  grouping them would only move the same values behind a type \
                  no other caller has a use for"
    )]
    fn option_column(
        x: u32,
        top: u32,
        width: u32,
        band_h: u32,
        row: u32,
        line: u32,
        gap: u32,
        font: BitmapFont,
    ) -> ([Rect; OPTION_GROUP_COUNT], [Rect; OPTION_GROUP_COUNT], Rect) {
        let label_w = OptionGroup::ALL
            .iter()
            .map(|group| font.text_width(group.label()))
            .max()
            .unwrap_or(0)
            .min(width);
        let field_x = x
            .saturating_add(label_w)
            .saturating_add(font.text_width(" "));
        let field_w = x
            .saturating_add(width)
            .saturating_sub(field_x.min(x.saturating_add(width)));

        let bottom = top.saturating_add(band_h);
        let mut y = top;
        let mut labels = [Rect::new(0, 0, 0, 0); OPTION_GROUP_COUNT];
        let mut fields = [Rect::new(0, 0, 0, 0); OPTION_GROUP_COUNT];
        for slot in 0..OPTION_GROUP_COUNT {
            if y.saturating_add(row) > bottom {
                break;
            }
            labels[slot] = Rect::new(to_i32(x), to_i32(y), label_w, row);
            fields[slot] = Rect::new(to_i32(field_x), to_i32(y), field_w, row);
            y = y.saturating_add(row).saturating_add(gap);
        }
        let caption = if y.saturating_add(line) > bottom {
            Rect::new(0, 0, 0, 0)
        } else {
            Rect::new(to_i32(x), to_i32(y), width, line)
        };
        (labels, fields, caption)
    }

    /// Claim the footer: Apply at the trailing edge, Close beside it, and the
    /// status line filling whatever is left at the leading edge.
    fn footer(
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        gap: u32,
        scale: Scale,
        font: BitmapFont,
    ) -> (Rect, Rect, Rect) {
        let wanted = scale
            .scale_length(BUTTON_WIDTH)
            .max(font.text_width(crate::APPLY_LABEL).saturating_mul(2));
        let pair_ceiling = width.saturating_sub(gap.min(width));
        let button_w = wanted.min(pair_ceiling.checked_div(2).unwrap_or(0));
        let right = x.saturating_add(width);
        let apply_x = right.saturating_sub(button_w);
        let close_x = apply_x.saturating_sub(button_w).saturating_sub(gap);
        let status_w = close_x.saturating_sub(x.min(close_x)).saturating_sub(gap);
        (
            Rect::new(to_i32(apply_x), to_i32(y), button_w, height),
            Rect::new(to_i32(close_x), to_i32(y), button_w, height),
            Rect::new(to_i32(x), to_i32(y), status_w, height),
        )
    }

    /// The live preview of the selected wallpaper.
    #[must_use]
    pub fn preview(&self) -> Rect {
        self.preview
    }

    /// The true-scale model of the screen inside [`Self::preview`]: the
    /// largest rectangle sharing the desktop's own screen aspect ratio,
    /// centred within the band. This is where the preview's pixels are
    /// drawn; the rest of the band is left as plain window background so it
    /// reads as a screen sitting inside the panel.
    #[must_use]
    pub fn preview_model(&self) -> Rect {
        self.preview_model
    }

    /// [`Self::preview_model`]'s own computation: the shared placement
    /// geometry's [`WallpaperFit::Fit`] contains a `screen_w`x`screen_h`
    /// "source" inside the `panel`-sized "screen", centred — exactly the
    /// model box this wants, so it is never a second, private fit
    /// computation.
    fn screen_model_box(panel: Rect, screen_w: u32, screen_h: u32) -> Rect {
        if panel.is_empty() {
            return Rect::new(panel.left(), panel.top(), 0, 0);
        }
        let Some(placement) = tairix_wallpaper::place(
            (screen_w, screen_h),
            (panel.width, panel.height),
            WallpaperFit::Fit,
        ) else {
            return Rect::new(panel.left(), panel.top(), 0, 0);
        };
        let model = placement.destination();
        Rect::new(
            panel.left().saturating_add(model.left()),
            panel.top().saturating_add(model.top()),
            model.width,
            model.height,
        )
    }

    /// The label of one option group.
    #[must_use]
    pub fn option_label(&self, group: OptionGroup) -> Rect {
        self.option_labels[group.index()]
    }

    /// The collapsed drop-down field of one option group.
    #[must_use]
    pub fn option_field(&self, group: OptionGroup) -> Rect {
        self.option_fields[group.index()]
    }

    /// The caption naming the wallpaper the preview is showing.
    #[must_use]
    pub fn caption(&self) -> Rect {
        self.caption
    }

    /// The gallery's section heading.
    #[must_use]
    pub fn heading(&self) -> Rect {
        self.heading
    }

    /// The gallery's tile area, excluding the scrollbar's reserved gutter.
    #[must_use]
    pub fn tiles(&self) -> Rect {
        self.tiles
    }

    /// The gallery scrollbar's gutter, reserved whether or not the gallery
    /// currently overflows so the tiles never re-flow as the bar appears.
    #[must_use]
    pub fn scrollbar(&self) -> Rect {
        self.scrollbar
    }

    /// The apply-outcome status line.
    #[must_use]
    pub fn status(&self) -> Rect {
        self.status
    }

    /// The Apply button.
    #[must_use]
    pub fn apply(&self) -> Rect {
        self.apply
    }

    /// The Close button.
    #[must_use]
    pub fn close(&self) -> Rect {
        self.close
    }

    /// One gallery tile's `(width, height)` in pixels.
    ///
    /// Exposed so an owner can ask a tile what square side it will draw its
    /// picture at, and rasterise artwork at exactly that side.
    #[must_use]
    pub fn tile_size(&self) -> (u32, u32) {
        (self.tile_metrics.cell_width, self.tile_metrics.cell_height)
    }

    /// The gallery grid over `entries` tiles.
    ///
    /// The shared icon-grid engine every icon view in the desktop uses, so
    /// the gallery's wrapping, its spread of a line's leftover space, its
    /// hit-test, and its scroll range are the one definition the file
    /// manager and the desktop's own icon field already share.
    #[must_use]
    pub fn grid(&self, entries: usize) -> GridView {
        GridView::new(
            self.tiles,
            self.tile_metrics,
            0,
            entries,
            GridFlow::RowsFromLeading,
            GridFill::Spread,
        )
    }
}
