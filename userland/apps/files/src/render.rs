//! Painting the browser's current directory into a pixel [`Surface`].
//!
//! [`render`] turns a [`Browser`]'s path and entries into a premultiplied-alpha
//! [`Surface`] sized to the app's content viewport, using the active theme's
//! [`Palette`] for every colour and the shared [`BitmapFont`] for every label.
//! The surface is the window manager's to place and round: the browser paints
//! a *rectangular* buffer and the compositor applies any corner radius through
//! its single anti-aliased rounded-corner path (`AGENTS.md` §2.2). There is no
//! rounding — and no colour algebra — here.
//!
//! The top row is a path bar showing the current directory; the rows below it
//! list the entries, one per line, the selected entry highlighted with the
//! accent role. When there are more entries than fit, the list scrolls so the
//! selected entry stays visible. Every length saturates and every blit clips,
//! so a degenerate viewport paints nothing rather than panicking (`AGENTS.md`
//! §2.9).

use alloc::string::String;

use rustos_font::BitmapFont;
use rustos_geometry::Rect;
use rustos_raster::{Color, Surface};
use rustos_theme::{Palette, Theme};

use crate::browser::Browser;
use crate::entry::Entry;
use crate::source::DirectorySource;

/// Padding in pixels between a row's edge and its label text.
const LABEL_PADDING: u32 = 4;

/// Vertical padding above and below a row's glyphs.
const ROW_PADDING: u32 = 2;

/// Paint `browser`'s current directory into a [`Surface`] the size of
/// `viewport`, using `theme`'s palette.
///
/// Only `viewport`'s dimensions are used; the window manager places the
/// returned surface at `viewport`'s origin. Returns `None` only when those
/// dimensions cannot be allocated (a surface that could never exist), so the
/// caller fails closed rather than panicking (`AGENTS.md` §2.9).
#[must_use]
pub fn render<S: DirectorySource>(
    browser: &Browser<S>,
    theme: &Theme,
    viewport: Rect,
) -> Option<Surface> {
    let font = BitmapFont::mono5x7();
    let row_height = font
        .glyph_height()
        .saturating_add(ROW_PADDING.saturating_mul(2));
    let mut surface = Surface::new(viewport.width, viewport.height)?;
    let palette = theme.palette();

    surface.fill(palette.surface.into());
    draw_path_bar(&mut surface, &font, palette, &browser.path(), row_height);
    draw_entries(&mut surface, &font, palette, browser, row_height);
    Some(surface)
}

/// Fill the top path bar and draw the current directory's path into it.
fn draw_path_bar(
    surface: &mut Surface,
    font: &BitmapFont,
    palette: &Palette,
    path: &str,
    row_height: u32,
) {
    surface.fill_rect(
        0,
        0,
        surface.width(),
        row_height,
        palette.surface_raised.into(),
    );
    draw_label(
        surface,
        font,
        0,
        row_height,
        path,
        palette.on_surface.into(),
    );
}

/// Draw the visible entry rows below the path bar, highlighting the selection
/// and scrolling so the selected entry stays on screen.
fn draw_entries<S: DirectorySource>(
    surface: &mut Surface,
    font: &BitmapFont,
    palette: &Palette,
    browser: &Browser<S>,
    row_height: u32,
) {
    let list_top = row_height;
    let list_height = surface.height().saturating_sub(list_top);
    let visible_rows = usize::try_from(list_height / row_height).unwrap_or(usize::MAX);
    if visible_rows == 0 {
        return;
    }

    let entries = browser.entries();
    let selected = browser.selected_index();
    let first = first_visible(selected, visible_rows);

    for (offset, (index, entry)) in entries
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_rows)
        .enumerate()
    {
        let step = u32::try_from(offset).unwrap_or(u32::MAX);
        let y = list_top.saturating_add(row_height.saturating_mul(step));
        let is_selected = selected == Some(index);
        if is_selected {
            surface.fill_rect(0, y, surface.width(), row_height, palette.accent.into());
        }
        let color = if is_selected {
            palette.on_accent
        } else {
            palette.on_surface
        };
        draw_label(
            surface,
            font,
            y,
            row_height,
            &entry_label(entry),
            color.into(),
        );
    }
}

/// The label shown for an entry: a directory is suffixed with `/` so its kind
/// reads at a glance without a separate icon column.
fn entry_label(entry: &Entry) -> String {
    let mut label = String::from(entry.name());
    if entry.is_directory() {
        label.push('/');
    }
    label
}

/// The index of the first entry to draw so that the `selected` entry is within
/// the `visible_rows`-row window. Anchors to the top when nothing is selected.
fn first_visible(selected: Option<usize>, visible_rows: usize) -> usize {
    match selected {
        Some(sel) if sel >= visible_rows => sel + 1 - visible_rows,
        _ => 0,
    }
}

/// Draw `text` leading-aligned and vertically centred within the row spanning
/// the full surface width at top `y` with height `row_height`. Text wider than
/// the row is truncated to what fits (`AGENTS.md` §2.9).
fn draw_label(
    surface: &mut Surface,
    font: &BitmapFont,
    y: u32,
    row_height: u32,
    text: &str,
    color: Color,
) {
    if text.is_empty() {
        return;
    }
    let usable = surface
        .width()
        .saturating_sub(LABEL_PADDING.saturating_mul(2));
    let fitted = font.truncate_to_width(text, usable);
    if fitted.is_empty() {
        return;
    }
    let y_offset = row_height.saturating_sub(font.glyph_height()) / 2;
    font.draw_text(
        surface,
        to_i32(LABEL_PADDING),
        to_i32(y.saturating_add(y_offset)),
        fitted,
        color,
    );
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
