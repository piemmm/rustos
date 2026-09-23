//! [`resolve_frame`]: the one place the window's client is divided into the
//! regions the shell draws into and hit-tests against.
//!
//! Every surface reads the same resolution, so a press can never land on a
//! control drawn somewhere else. A client too narrow to seat the sidebar
//! sheds it — and the search field with it, there being no strip left to
//! filter — and the content column always survives, because the pane is what
//! the reader came for.

use tairix_abi::window_ipc::WindowSizing;
use tairix_controls::{Breadcrumb, TextField};
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_theme::Theme;

/// The window's logical width at the reference density: the strip plus a
/// content column wide enough for a pane's widest row.
pub const WIN_WIDTH: u32 = 780;

/// The window's logical height at the reference density.
pub const WIN_HEIGHT: u32 = 600;

/// The narrowest logical client the window may be resized to: enough for the
/// content column alone, the strip having been shed.
const MIN_WIDTH: u32 = 320;

/// The shortest logical client the window may be resized to.
const MIN_HEIGHT: u32 = 240;

/// The sizing the window asks the window manager for at `scale`.
///
/// No ceiling: a wider window seats more of a pane's rows and a taller one
/// scrolls less, at every size it is given.
#[must_use]
pub fn win_sizing(scale: Scale) -> WindowSizing {
    sizing_of(
        scale.scale_length(MIN_WIDTH),
        scale.scale_length(MIN_HEIGHT),
    )
}

/// One spelling of the sizing variant, so [`win_sizing`] and
/// [`WIN_RESIZABLE`] cannot state different things about the decoration.
const fn sizing_of(min_width_px: u32, min_height_px: u32) -> WindowSizing {
    WindowSizing::Resizable {
        min_width_px,
        min_height_px,
        max_width_px: 0,
        max_height_px: 0,
    }
}

/// Whether the window is decorated resizable, which is what decides the
/// furniture band the window manager reserves around the client.
pub const WIN_RESIZABLE: bool = sizing_of(0, 0).resizable();

/// The logical width of the category sidebar, at the reference density.
///
/// Wide enough at that density, in the shipped face, for the longest category
/// label the registry holds beside its leading glyph — with the strip's own
/// scrollbar taken out of it, because a window short enough to scroll the strip
/// carves the bar from this column. A label that still does not fit (another
/// locale's, a larger face) is elided with the shared mark rather than
/// widening the column.
pub const SIDEBAR_WIDTH: u32 = 208;

/// The narrowest logical width the content column is given before the sidebar
/// is shed to widen it.
///
/// Below this a pane's rows would be narrower than their own labels, so the
/// window keeps the pane and drops the navigation, which the location trail
/// then carries on its own.
pub const CONTENT_FLOOR: u32 = 280;

/// Which of the shell's two columns hold more than they can show.
///
/// The frame cannot work this out for itself — one answer needs the strip's
/// own measured extent and the other a wrapped statement's height — so the
/// shell, which holds both scroll ranges, states them.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Overflow {
    /// The category strip is longer than the column it is drawn in.
    pub strip: bool,
    /// The pane is taller than the column it is drawn in.
    pub pane: bool,
}

/// Whether the pane on show offers an action band beneath its column.
///
/// The frame cannot work this out either: it is the registry's answer for
/// the pane the shell has open.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Actions {
    /// The pane's effect is its own feedback; there is nothing to batch.
    #[default]
    None,
    /// The pane stages its change, or offers the command that changes what
    /// it reports, so it has a band beneath its column.
    Band,
}

/// The regions of the settings window, resolved once per layout.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ShellFrame {
    /// The search field above the sidebar, or `None` when the sidebar is shed.
    pub search: Option<Rect>,
    /// The location trail, which spans the band's whole width once the
    /// sidebar is shed.
    pub breadcrumb: Rect,
    /// The category and pane strip, or `None` when the client is too narrow
    /// to seat it.
    pub sidebar: Option<Rect>,
    /// The strip's scrollbar gutter, or `None` when the whole strip fits (or
    /// there is no strip at all).
    pub strip_scrollbar: Option<Rect>,
    /// The pane's own column, which every client wide enough to draw anything
    /// at all has.
    pub content: Rect,
    /// The content column's scrollbar gutter, or `None` when the pane fits.
    pub scrollbar: Option<Rect>,
    /// The pane's action band, or `None` for a pane that offers none.
    pub footer: Option<Rect>,
}

/// Divide `viewport` into the shell's regions, `overflow` saying which
/// columns need their scrollbar.
///
/// What fits is the caller's to know — it owns both scroll ranges — because
/// measuring a wrapped statement is far too much work to do on the input
/// path, where a frame is resolved per event.
///
/// A viewport with no room for the band at all yields an empty frame rather
/// than a sliver: a band drawn over the pane would be worse than no band.
#[must_use]
pub fn resolve_frame(
    viewport: Rect,
    scale: Scale,
    theme: &Theme,
    overflow: Overflow,
    actions: Actions,
) -> ShellFrame {
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    // A search field is a text field plus its magnifier and clear mark, so
    // the field's own height is the band's; there is no second definition.
    let band_h = TextField::height(scale, theme).max(Breadcrumb::measured_height(scale, theme));
    let body_top = viewport.top().saturating_add(to_i32(band_h));
    let body_h = viewport.height.saturating_sub(band_h);
    if viewport.width == 0 || body_h == 0 {
        return ShellFrame {
            search: None,
            breadcrumb: Rect::EMPTY,
            sidebar: None,
            strip_scrollbar: None,
            content: Rect::EMPTY,
            scrollbar: None,
            footer: None,
        };
    }
    let bar_w = scale.scale_length(theme.metrics().scrollbar_breadth).max(1);

    let sidebar_w = scale.scale_length(SIDEBAR_WIDTH).max(1);
    let seats_sidebar = viewport.width
        >= sidebar_w
            .saturating_add(gap)
            .saturating_add(scale.scale_length(CONTENT_FLOOR).max(1));
    let (search, sidebar, content_x, content_w) = if seats_sidebar {
        let content_x = viewport.left().saturating_add(to_i32(sidebar_w + gap));
        (
            Some(Rect::new(
                viewport.left(),
                viewport.top(),
                sidebar_w,
                band_h,
            )),
            Some(Rect::new(viewport.left(), body_top, sidebar_w, body_h)),
            content_x,
            viewport.width.saturating_sub(sidebar_w + gap),
        )
    } else {
        (None, None, viewport.left(), viewport.width)
    };

    // The strip's own gutter is carved out of the strip's column, not out of
    // the pane's: a long list must not narrow the pane beside it.
    let (sidebar, strip_scrollbar) = match sidebar {
        Some(rect) if overflow.strip && rect.width > bar_w.saturating_add(1) => (
            Some(Rect::new(
                rect.left(),
                rect.top(),
                rect.width.saturating_sub(bar_w),
                rect.height,
            )),
            Some(Rect::new(
                rect.left()
                    .saturating_add(to_i32(rect.width.saturating_sub(bar_w))),
                rect.top(),
                bar_w,
                rect.height,
            )),
        ),
        seated => (seated, None),
    };

    let breadcrumb = Rect::new(content_x, viewport.top(), content_w, band_h);
    // The band is carved out of the pane's own column before the gutter is,
    // so the scrollbar runs beside the column the pane actually gets rather
    // than past the commands beneath it.
    let action_h = match actions {
        Actions::Band => crate::footer::Footer::measured_height(scale, theme),
        Actions::None => 0,
    };
    let (body_h, footer) = if action_h > 0 && body_h > action_h {
        (
            body_h.saturating_sub(action_h),
            Some(Rect::new(
                content_x,
                body_top.saturating_add(to_i32(body_h.saturating_sub(action_h))),
                content_w,
                action_h,
            )),
        )
    } else {
        (body_h, None)
    };
    let (content, scrollbar) = if overflow.pane && content_w > bar_w.saturating_add(1) {
        (
            Rect::new(content_x, body_top, content_w.saturating_sub(bar_w), body_h),
            Some(Rect::new(
                content_x.saturating_add(to_i32(content_w.saturating_sub(bar_w))),
                body_top,
                bar_w,
                body_h,
            )),
        )
    } else {
        (Rect::new(content_x, body_top, content_w, body_h), None)
    };
    ShellFrame {
        search,
        breadcrumb,
        sidebar,
        strip_scrollbar,
        content,
        scrollbar,
        footer,
    }
}
