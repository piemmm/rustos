//! The chooser's painter.
//!
//! Everything interactive is drawn by the shared control that owns it, in the
//! state that control is already in, so this module never decides what a
//! hover or a press looks like — it decides only *where* each thing goes,
//! from the same [`Layout`] the hit-test uses.
//!
//! Painting runs from the back of the window forward, and the expanded
//! drop-down's list is painted last of all: it is the only surface allowed to
//! cover another, and it is also the only surface the pointer is routed to
//! while it is open, so what covers what and what receives a click are the
//! same order.

use tairix_controls::collection::IconTile;
use tairix_controls::state::{ControlState, FocusState, PointerState, SelectionState};
use tairix_font::BitmapFont;
use tairix_geometry::Rect;
use tairix_icon::IconKind;
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;
use tairix_wallpaper::{Backdrop, WallpaperChoice};

use crate::{
    to_i32, to_u32, ApplyOutcome, Chooser, Focus, Layout, OptionGroup, Style, Thumbnail,
    GALLERY_HEADING, REFUSED_MARKER,
};

/// The text the preview shows while its wallpaper is still being rendered.
const PREVIEW_PENDING: &str = "Rendering…";

/// Paint the whole window, or `None` when the surface cannot be allocated.
pub(crate) fn render(chooser: &Chooser, layout: &Layout, style: Style<'_>) -> Option<Surface> {
    let (width, height) = chooser.size();
    let mut surface = Surface::new(width, height)?;
    surface.fill(style.theme().palette().surface.into());

    paint_preview(&mut surface, chooser, layout, style);
    paint_options(&mut surface, chooser, layout, style);
    paint_gallery(&mut surface, chooser, layout, style);
    paint_footer(&mut surface, chooser, layout, style);
    paint_expanded_list(&mut surface, chooser, layout, style);

    Some(surface)
}

/// Paint the preview panel: the chosen backdrop, the chosen wallpaper placed
/// over it exactly as the desktop would place it, and a rim so the panel
/// reads as a viewport rather than as a hole in the window.
///
/// The wallpaper's own render leaves every pixel its placement does not cover
/// transparent, so painting the backdrop first is what makes a contained fit
/// show its letterbox bars and a centred one its margins — the same
/// composition the desktop performs.
fn paint_preview(surface: &mut Surface, chooser: &Chooser, layout: &Layout, style: Style<'_>) {
    let panel = layout.preview();
    if panel.is_empty() {
        return;
    }
    let theme = style.theme();
    let (x, y) = (to_u32(panel.left()), to_u32(panel.top()));
    surface.fill_rect(
        x,
        y,
        panel.width,
        panel.height,
        backdrop_color(theme, chooser.backdrop()),
    );

    // No wanted preview means the selection *is* the backdrop already
    // painted above, which is the whole picture.
    if let Some(want) = chooser.wanted_preview(style) {
        if let Some(pixels) = chooser.preview_surface(&want) {
            surface.blit(panel.left(), panel.top(), pixels);
        } else {
            // Pixels that do not exist yet, or a wallpaper the sandbox
            // refused: say which, rather than showing an empty frame.
            let (text, ink) = if chooser.preview_refused(&want) {
                (REFUSED_MARKER, theme.palette().danger)
            } else {
                (PREVIEW_PENDING, theme.palette().on_surface_muted)
            };
            centre_text(surface, panel, text, style.font(), ink.into());
        }
    }
    paint_rim(surface, panel, theme);
}

/// Paint the four settings drop-downs and the caption naming what the
/// preview is showing.
fn paint_options(surface: &mut Surface, chooser: &Chooser, layout: &Layout, style: Style<'_>) {
    let palette = style.theme().palette();
    for group in OptionGroup::ALL {
        let label = layout.option_label(group);
        if !label.is_empty() {
            let font = style.font();
            let baseline = centred_baseline(label, font);
            font.draw_text(
                surface,
                label.left(),
                baseline,
                font.truncate_to_width(group.label(), label.width),
                palette.on_surface_muted.into(),
            );
        }
        let field = layout.option_field(group);
        if !field.is_empty() {
            chooser
                .field(group)
                .render(surface, field, style.scale(), style.theme(), style.font());
        }
    }

    let caption = layout.caption();
    if caption.is_empty() {
        return;
    }
    let Some(selected) = chooser.candidates().get(chooser.selected()) else {
        return;
    };
    let font = style.caption_font();
    font.draw_text(
        surface,
        caption.left(),
        centred_baseline(caption, font),
        font.truncate_to_width(&selected.label, caption.width),
        palette.on_surface_muted.into(),
    );
}

/// Paint the gallery: its heading, the tiles the viewport holds, and the
/// scrollbar in its reserved gutter.
fn paint_gallery(surface: &mut Surface, chooser: &Chooser, layout: &Layout, style: Style<'_>) {
    let heading = layout.heading();
    if !heading.is_empty() {
        let font = style.heading_font();
        font.draw_text(
            surface,
            heading.left(),
            centred_baseline(heading, font),
            font.truncate_to_width(GALLERY_HEADING, heading.width),
            style.theme().palette().on_surface.into(),
        );
    }

    let tiles = layout.tiles();
    let grid = layout.grid(chooser.candidates().len());
    let offset = chooser.scroll_offset();
    if !tiles.is_empty() {
        let swatch = backdrop_swatch(chooser, layout, style);
        surface.with_clip(
            to_u32(tiles.left()),
            to_u32(tiles.top()),
            tiles.width,
            tiles.height,
            |clipped| {
                for index in grid.visible_range(offset) {
                    let (Some(bounds), Some(candidate)) = (
                        grid.cell_rect(offset, index),
                        chooser.candidates().get(index),
                    ) else {
                        continue;
                    };
                    paint_tile(
                        clipped,
                        chooser,
                        index,
                        candidate,
                        bounds,
                        swatch.as_ref(),
                        style,
                    );
                }
            },
        );
    }

    let gutter = layout.scrollbar();
    if !gutter.is_empty() {
        chooser
            .scrollbar()
            .render(surface, gutter, style.scale(), style.theme());
    }
}

/// Paint one gallery tile in the state the pointer, the selection, and the
/// keyboard focus put it in.
fn paint_tile(
    surface: &mut Surface,
    chooser: &Chooser,
    index: usize,
    candidate: &crate::Candidate,
    bounds: Rect,
    swatch: Option<&Surface>,
    style: Style<'_>,
) {
    let selected = index == chooser.selected();
    let pointer = if chooser.armed() == Some(index) {
        PointerState::Pressed
    } else if chooser.hovered() == Some(index) {
        PointerState::Hover
    } else {
        PointerState::None
    };
    let state = ControlState::idle()
        .with_pointer(pointer)
        .with_selection(if selected {
            SelectionState::Selected
        } else {
            SelectionState::Unselected
        })
        .with_focus(FocusState {
            focused: selected && chooser.focus() == Focus::Gallery,
            in_focus_field: chooser.focus() == Focus::Gallery,
        });

    let artwork = match (&candidate.thumbnail, &candidate.choice) {
        (Thumbnail::Ready(pixels), _) => Some(pixels),
        (Thumbnail::Backdrop, _) | (_, WallpaperChoice::None) => swatch,
        _ => None,
    };
    IconTile::new(candidate.label.clone(), IconKind::Image)
        .with_state(state)
        .render(
            surface,
            bounds,
            style.scale(),
            style.theme(),
            style.font(),
            artwork,
        );

    if candidate.thumbnail == Thumbnail::Refused {
        let side = IconTile::icon_side(bounds, style.scale(), style.theme());
        let slot = Rect::new(bounds.left(), bounds.top(), bounds.width, side);
        centre_text(
            surface,
            slot,
            REFUSED_MARKER,
            style.font(),
            style.theme().palette().danger.into(),
        );
    }
}

/// Paint the footer: the last apply's outcome, and the two actions.
fn paint_footer(surface: &mut Surface, chooser: &Chooser, layout: &Layout, style: Style<'_>) {
    let status = layout.status();
    if let (false, Some(outcome)) = (status.is_empty(), chooser.apply_outcome()) {
        let palette = style.theme().palette();
        let (text, ink): (&str, Color) = match outcome {
            ApplyOutcome::Applied => ("Applied.", palette.success.into()),
            ApplyOutcome::Refused(reason) => (reason.as_str(), palette.danger.into()),
            ApplyOutcome::NoDesktop => ("No desktop session is listening.", palette.warning.into()),
        };
        let font = style.font();
        font.draw_text(
            surface,
            status.left(),
            centred_baseline(status, font),
            font.truncate_to_width(text, status.width),
            ink,
        );
    }

    chooser.close_button().render(
        surface,
        layout.close(),
        style.scale(),
        style.theme(),
        style.font(),
    );
    chooser.apply_button().render(
        surface,
        layout.apply(),
        style.scale(),
        style.theme(),
        style.font(),
    );
}

/// Paint the expanded drop-down's list, over everything else.
fn paint_expanded_list(
    surface: &mut Surface,
    chooser: &Chooser,
    layout: &Layout,
    style: Style<'_>,
) {
    let Some(group) = chooser.expanded() else {
        return;
    };
    let popup = chooser.popup_rect(group, layout, style);
    chooser
        .field(group)
        .render_popup(surface, popup, style.scale(), style.theme(), style.font());
}

/// The square swatch of the chosen backdrop a tile draws as its picture when
/// the candidate *is* the backdrop, or `None` when no tile needs one.
///
/// Built once per paint rather than per tile: the "no wallpaper" entry is the
/// only candidate that shows it, and its colour is the same wherever it is
/// drawn.
fn backdrop_swatch(chooser: &Chooser, layout: &Layout, style: Style<'_>) -> Option<Surface> {
    let (width, height) = layout.tile_size();
    let side = IconTile::icon_side(Rect::new(0, 0, width, height), style.scale(), style.theme());
    if side == 0 {
        return None;
    }
    Surface::filled(
        side,
        side,
        backdrop_color(style.theme(), chooser.backdrop()).premultiply(),
    )
}

/// The flat colour a backdrop names, resolving [`Backdrop::Theme`] against
/// the active theme's own desktop colour.
fn backdrop_color(theme: &Theme, backdrop: Backdrop) -> Color {
    match backdrop {
        Backdrop::Theme => theme.palette().desktop.into(),
        Backdrop::Colour(rgb) => Color::rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// Outline `rect` in the theme's own rim colour, one border thickness wide.
fn paint_rim(surface: &mut Surface, rect: Rect, theme: &Theme) {
    let thickness = theme.metrics().border_thickness.max(1);
    let rim = Color::from(theme.palette().rim);
    let (x, y) = (to_u32(rect.left()), to_u32(rect.top()));
    let (w, h) = (rect.width, rect.height);
    surface.fill_rect(x, y, w, thickness.min(h), rim);
    surface.fill_rect(
        x,
        y.saturating_add(h.saturating_sub(thickness)),
        w,
        thickness.min(h),
        rim,
    );
    surface.fill_rect(x, y, thickness.min(w), h, rim);
    surface.fill_rect(
        x.saturating_add(w.saturating_sub(thickness)),
        y,
        thickness.min(w),
        h,
        rim,
    );
}

/// Draw `text` centred in `rect`, truncated to fit.
fn centre_text(surface: &mut Surface, rect: Rect, text: &str, font: BitmapFont, ink: Color) {
    let fitted = font.truncate_to_width(text, rect.width);
    let width = font.text_width(fitted);
    let x = rect
        .left()
        .saturating_add(to_i32(rect.width.saturating_sub(width) / 2));
    font.draw_text(surface, x, centred_baseline(rect, font), fitted, ink);
}

/// The `y` a line of `font` is drawn at to sit centred in `rect`.
fn centred_baseline(rect: Rect, font: BitmapFont) -> i32 {
    let glyph = font.glyph_height();
    rect.top()
        .saturating_add(to_i32(rect.height.saturating_sub(glyph) / 2))
}
