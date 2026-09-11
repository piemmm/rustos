//! The viewer's window geometry: the one function every painter, every
//! hit-test, and every test agrees on.
//!
//! ```text
//! +--------------------------------------------------------------+
//! | [-][+][fit][1:1] [<][>] [rot][rot][mir] [play] [i]  --o----- |
//! +--------------------------------------------------------------+
//! |                                                      |     ||
//! |                                                      |     ||
//! |                       canvas                         |info ||
//! |                                                      |panel||
//! |                                                      |     ||
//! +------------------------------------------------------+     ||
//! |======================================================|     ||
//! +--------------------------------------------------------------+
//! | photo.jpg — JPEG 4032x3024 — page 1 of 1 — 2.4 MB      100%  |
//! +--------------------------------------------------------------+
//! ```
//!
//! Every extent is derived from the active theme's metrics at the desktop UI
//! scale and from the text face's own line height — never a pixel constant a
//! denser theme or a larger scale would leave wrong. The toolbar and the
//! status line are claimed first, from their own edges inward, so however
//! small the window becomes the tools stay reachable and only the canvas
//! gives up room. Every region is total: a window with no room for one
//! yields an empty rectangle there, which every painter and hit-test treats
//! as absent rather than as an error.

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_theme::Theme;

/// The share of the content width the info panel may take, in twenty-fourths.
const INFO_SHARE: u32 = 7;

/// The narrowest info panel worth drawing, in logical pixels. Below this a
/// fact cannot be read, so no panel is drawn at all.
const MIN_INFO_WIDTH: u32 = 132;

/// The zoom slider's width in logical pixels: long enough to reach any rung
/// of the ladder without the thumb covering the whole track.
const ZOOM_SLIDER_WIDTH: u32 = 132;

/// How many tools the toolbar carries, so the layout reserves the strip they
/// need rather than measuring a toolbar it does not own.
pub const TOOL_COUNT: usize = 11;

/// The viewer's resolved window geometry.
///
/// Built by [`Layout::for_window`] and consumed unchanged by both the painter
/// and every hit-test, so what the user sees and what a click lands on can
/// never disagree.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    window: Rect,
    toolbar: Rect,
    zoom_slider: Rect,
    canvas: Rect,
    vertical_bar: Rect,
    horizontal_bar: Rect,
    info: Rect,
    status: Rect,
}

impl Layout {
    /// Resolve the geometry of a `width`×`height` client area for the active
    /// theme and UI scale, with `font` the face the status line and the info
    /// panel are set in.
    ///
    /// `info` says whether the user has the information panel open; closed,
    /// its rectangle is empty and the canvas takes the room.
    #[must_use]
    pub fn for_window(
        width: u32,
        height: u32,
        theme: &Theme,
        scale: Scale,
        font: BitmapFont,
        info: bool,
    ) -> Self {
        let metrics = theme.metrics();
        let gap = scale.scale_length(metrics.control_gap).max(1);
        let line = font.line_height().max(1);
        let row = scale.scale_length(metrics.control_height).max(line);
        let bar = scale.scale_length(metrics.scrollbar_breadth).max(1);
        let inset = gap.saturating_mul(2);

        // The toolbar takes the top edge and the status line the bottom, each
        // claimed before the body so neither can be squeezed out.
        let toolbar_h = row.saturating_add(inset).min(height);
        let toolbar = Rect::new(0, 0, width, toolbar_h);
        let status_h = line
            .saturating_add(gap)
            .min(height.saturating_sub(toolbar_h));
        let status = Rect::new(
            0,
            tairix_geometry::to_i32(height.saturating_sub(status_h)),
            width,
            status_h,
        );

        let body_top = toolbar_h;
        let body_h = height.saturating_sub(toolbar_h).saturating_sub(status_h);

        // The zoom slider sits at the toolbar's trailing edge, so the tools
        // grow from the leading edge and the slider never moves under them.
        let slider_w = scale.scale_length(ZOOM_SLIDER_WIDTH).min(width);
        let zoom_slider = if toolbar_h == 0 || slider_w == 0 {
            Rect::EMPTY
        } else {
            Rect::new(
                tairix_geometry::to_i32(width.saturating_sub(slider_w.saturating_add(gap))),
                tairix_geometry::to_i32(gap),
                slider_w,
                toolbar_h.saturating_sub(inset).max(1),
            )
        };

        let info_w = panel_width(info, width, INFO_SHARE, scale.scale_length(MIN_INFO_WIDTH));
        let info = band(width.saturating_sub(info_w), body_top, info_w, body_h);

        // What is left beside the panel is the canvas and its two bars. The
        // bars are claimed from the canvas's own trailing edges, so a picture
        // that overflows loses a strip rather than the bars overlapping it.
        let between_x = 0;
        let between_w = width.saturating_sub(info_w);
        let canvas_w = between_w.saturating_sub(bar);
        let canvas_h = body_h.saturating_sub(bar);
        let canvas = band(between_x, body_top, canvas_w, canvas_h);
        let vertical_bar = band(
            between_x.saturating_add(canvas_w),
            body_top,
            between_w.saturating_sub(canvas_w),
            canvas_h,
        );
        let horizontal_bar = band(
            between_x,
            body_top.saturating_add(canvas_h),
            canvas_w,
            body_h.saturating_sub(canvas_h),
        );

        Self {
            window: Rect::new(0, 0, width, height),
            toolbar,
            zoom_slider,
            canvas,
            vertical_bar,
            horizontal_bar,
            info,
            status,
        }
    }

    /// The whole client area.
    ///
    /// What a change that reshapes the bands reports, so "everything" is the
    /// window's own rectangle rather than a sentinel a painter would have to
    /// recognise.
    #[must_use]
    pub const fn window(&self) -> Rect {
        self.window
    }

    /// The toolbar strip across the top.
    #[must_use]
    pub const fn toolbar(&self) -> Rect {
        self.toolbar
    }

    /// The strip of the toolbar the tools themselves occupy: everything
    /// before the zoom slider.
    #[must_use]
    pub fn tools(&self) -> Rect {
        let claimed = u32::try_from(self.zoom_slider.left().max(0)).unwrap_or(0);
        Rect::new(
            self.toolbar.left(),
            self.toolbar.top(),
            claimed.min(self.toolbar.width),
            self.toolbar.height,
        )
    }

    /// The zoom slider at the toolbar's trailing edge.
    #[must_use]
    pub const fn zoom_slider(&self) -> Rect {
        self.zoom_slider
    }

    /// The canvas the picture is drawn in.
    #[must_use]
    pub const fn canvas(&self) -> Rect {
        self.canvas
    }

    /// The vertical scrollbar's strip, down the canvas's trailing edge.
    #[must_use]
    pub const fn vertical_bar(&self) -> Rect {
        self.vertical_bar
    }

    /// The horizontal scrollbar's strip, along the canvas's bottom edge.
    #[must_use]
    pub const fn horizontal_bar(&self) -> Rect {
        self.horizontal_bar
    }

    /// The info panel down the trailing edge, empty when closed.
    #[must_use]
    pub const fn info(&self) -> Rect {
        self.info
    }

    /// The status line across the bottom.
    #[must_use]
    pub const fn status(&self) -> Rect {
        self.status
    }
}

/// The width a side panel takes: its share of the window, never below what
/// it needs to say anything, and nothing at all when it is closed or the
/// window is too narrow.
fn panel_width(open: bool, width: u32, share: u32, least: u32) -> u32 {
    if !open {
        return 0;
    }
    // The window extent comes from the desktop session, so the share is taken
    // wide and narrowed back rather than multiplied in place.
    let allowed = u32::try_from(u64::from(width) * u64::from(share) / 24).unwrap_or(width);
    if allowed < least {
        return 0;
    }
    allowed
}

/// A band of the body, empty when it has no room.
fn band(x: u32, y: u32, width: u32, height: u32) -> Rect {
    if width == 0 || height == 0 {
        return Rect::EMPTY;
    }
    Rect::new(
        tairix_geometry::to_i32(x),
        tairix_geometry::to_i32(y),
        width,
        height,
    )
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
