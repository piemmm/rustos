//! Painting the viewer, from state alone.
//!
//! Nothing here reads a file, asks a service, or decodes anything: every pixel
//! comes from what the engine already holds. A picture that has not arrived
//! yet is drawn as the reason it has not — never as a blank canvas, and never
//! as a fabricated image.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;

use tairix_controls::{Fact, FactList};
use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_icon::IconArtwork;
use tairix_raster::{Color, Surface};
use tairix_sandbox::imagerender::ViewFormat;
use tairix_theme::Theme;

use crate::view::View;
use crate::{Document, Layout};

/// One square of the transparency checkerboard, in logical pixels.
const CHECKER_SIDE: u32 = 8;

/// How far the lighter checkerboard square is lifted from the darker one, in
/// eighths of the way to white — the same visible step in a dark theme and a
/// light one.
const CHECKER_LIFT: u32 = 1;

/// Paint the whole viewer into the caller's retained window `surface`.
///
/// `artwork` is the icon cache the toolbar resolves its glyphs through. A
/// viewer holds no filesystem capability, so it can read no shipped asset and
/// resolves through the refusing seam — which falls to the mandatory built-in
/// glyph tier, so every tool still draws.
///
/// The surface is the host's, held for the life of the window, so a caller
/// that has narrowed its clip to the rectangle a round reported redraws only
/// that band: every pixel outside it is the one already on screen.
pub fn render_into(
    surface: &mut Surface,
    view: &View,
    layout: &Layout,
    theme: &Theme,
    scale: Scale,
    font: BitmapFont,
    artwork: &mut dyn IconArtwork,
) {
    surface.fill(Color::from(theme.palette().surface));
    // The two chrome strips stand off the body, so the canvas reads as the
    // content it is rather than as more window.
    let chrome = Color::from(theme.palette().surface_raised);
    plate(surface, layout.toolbar(), chrome);
    plate(surface, layout.status(), chrome);
    canvas(surface, view, layout, theme, scale, font);
    view.toolbar_control()
        .render(surface, layout.tools(), scale, theme, artwork);
    view.zoom_control()
        .render(surface, layout.zoom_slider(), scale, theme);
    let (vertical, horizontal) = view.bars();
    vertical.render(surface, layout.vertical_bar(), scale, theme);
    horizontal.render(surface, layout.horizontal_bar(), scale, theme);
    if !layout.info().is_empty() {
        FactList::new(facts(view)).with_separators(true).render(
            surface,
            layout.info(),
            scale,
            theme,
        );
    }
    status(surface, view, layout, theme, scale, font);
}

/// Lay `color` down over `bounds`, which the active clip narrows to whatever
/// of it this round is repainting.
fn plate(surface: &mut Surface, bounds: Rect, color: Color) {
    let (Ok(x), Ok(y)) = (u32::try_from(bounds.left()), u32::try_from(bounds.top())) else {
        return;
    };
    surface.fill_rect(x, y, bounds.width, bounds.height, color);
}

/// Paint the canvas: the checkerboard, then the picture over it, or the reason
/// there is no picture.
fn canvas(
    surface: &mut Surface,
    view: &View,
    layout: &Layout,
    theme: &Theme,
    scale: Scale,
    font: BitmapFont,
) {
    let bounds = layout.canvas();
    if bounds.is_empty() {
        return;
    }
    let (Some(picture), Some(natural)) = (view.picture(), view.document().map(Document::natural))
    else {
        reason(surface, view, bounds, theme, font);
        return;
    };
    let placement = view.viewport().placement(natural, bounds);
    checkerboard(surface, placement, theme, scale);
    // The picture goes over the checkerboard so its own transparency shows
    // what is behind it, which is the whole point of drawing one.
    surface.blit(placement.left(), placement.top(), picture);
}

/// Paint the transparency checkerboard under `bounds`.
fn checkerboard(surface: &mut Surface, bounds: Rect, theme: &Theme, scale: Scale) {
    let (Ok(x0), Ok(y0)) = (u32::try_from(bounds.left()), u32::try_from(bounds.top())) else {
        return;
    };
    if bounds.is_empty() {
        return;
    }
    let side = scale.scale_length(CHECKER_SIDE).max(1);
    let dark = Color::from(theme.palette().surface);
    let light = Color::rgba(lift(dark.r), lift(dark.g), lift(dark.b), dark.a);
    surface.fill_rect(x0, y0, bounds.width, bounds.height, dark);
    let mut row = 0;
    while row * side < bounds.height {
        let mut column = u32::from(row % 2 == 0);
        while column * side < bounds.width {
            surface.fill_rect(
                x0 + column * side,
                y0 + row * side,
                side.min(bounds.width - column * side),
                side.min(bounds.height - row * side),
                light,
            );
            column += 2;
        }
        row += 1;
    }
}

/// One channel lifted toward white.
fn lift(channel: u8) -> u8 {
    let lifted = u32::from(channel) + (255 - u32::from(channel)) * CHECKER_LIFT / 8;
    u8::try_from(lifted).unwrap_or(u8::MAX)
}

/// Draw the reason the canvas is showing no picture, centred.
fn reason(surface: &mut Surface, view: &View, bounds: Rect, theme: &Theme, font: BitmapFont) {
    let text = view
        .refusal()
        .map_or_else(|| OPENING.to_string(), ToString::to_string);
    centre_text(
        surface,
        bounds,
        &text,
        font,
        Color::from(theme.palette().on_surface_muted),
    );
}

/// What the canvas and the status line say before anything has arrived.
const OPENING: &str = "Opening…";

/// What the information panel states.
///
/// Drawn from the container's own declaration, and from the *selected*
/// entry's geometry where a page container's pages differ in size.
pub(crate) fn facts(view: &View) -> Vec<Fact> {
    let Some(document) = view.document() else {
        return alloc::vec![Fact::new("Document", "none open")];
    };
    let natural = document.natural();
    let mut facts = alloc::vec![
        Fact::new("Name", document.name.clone()),
        Fact::new("Format", format_name(document.format())),
        Fact::new("Pixels", format!("{} x {}", natural.0, natural.1)),
        Fact::new("Size", bytes(document.bytes)),
    ];
    if document.info.count > 1 {
        facts.push(Fact::new(
            entry_word(document.info.animated),
            format!("{} of {}", document.index() + 1, document.info.count),
        ));
    }
    if document.info.animated {
        facts.push(Fact::new(
            "Repeats",
            document
                .info
                .loop_count
                .map_or_else(|| "for ever".to_string(), |count| format!("{count}")),
        ));
    }
    if let Some(page) = document.page.filter(|page| page.delay_ns > 0) {
        facts.push(Fact::new(
            "Delay",
            format!("{} ms", page.delay_ns / 1_000_000),
        ));
    }
    facts.push(Fact::new("Zoom", percent(view.viewport().zoom())));
    facts
}

/// Paint the status line: what is open, and at what magnification.
fn status(
    surface: &mut Surface,
    view: &View,
    layout: &Layout,
    theme: &Theme,
    scale: Scale,
    font: BitmapFont,
) {
    let bounds = layout.status();
    if bounds.is_empty() {
        return;
    }
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let ink = Color::from(theme.palette().on_surface_muted);
    let baseline = centred_baseline(bounds, font);
    let zoom = percent(view.viewport().zoom());
    let zoom_width = font.text_width(&zoom);
    let room = bounds.width.saturating_sub(gap * 3 + zoom_width);
    font.draw_text(
        surface,
        bounds.left().saturating_add(to_i32(gap)),
        baseline,
        font.truncate_to_width(&summary(view), room),
        ink,
    );
    font.draw_text(
        surface,
        bounds
            .left()
            .saturating_add(to_i32(bounds.width.saturating_sub(gap + zoom_width))),
        baseline,
        &zoom,
        ink,
    );
}

/// The status line's one-line summary of what is open.
pub(crate) fn summary(view: &View) -> String {
    let Some(document) = view.document() else {
        return view
            .refusal()
            .map_or_else(|| OPENING.to_string(), ToString::to_string);
    };
    let natural = document.natural();
    let mut line = format!(
        "{} — {} {} x {}",
        document.name,
        format_name(document.format()),
        natural.0,
        natural.1
    );
    if document.info.count > 1 {
        let _ = write!(
            line,
            " — {} {} of {}",
            entry_word(document.info.animated).to_ascii_lowercase(),
            document.index() + 1,
            document.info.count
        );
    }
    let _ = write!(line, " — {}", bytes(document.bytes));
    line
}

/// What a container's entries are called: a played animation has frames, a
/// document has pages.
const fn entry_word(animated: bool) -> &'static str {
    if animated {
        "Frame"
    } else {
        "Page"
    }
}

/// The name a format is shown under.
///
/// Exhaustive on purpose: [`ViewFormat`] is the view protocol's own
/// vocabulary, so a format added to it must be given a name here rather than
/// falling through to a placeholder.
const fn format_name(format: ViewFormat) -> &'static str {
    match format {
        ViewFormat::Png => "PNG",
        ViewFormat::Jpeg => "JPEG",
        ViewFormat::Gif => "GIF",
        ViewFormat::Bmp => "BMP",
        ViewFormat::Ico => "Icon",
        ViewFormat::Sprite => "Sprite",
        ViewFormat::Tiff => "TIFF",
        ViewFormat::Webp => "WEBP",
        ViewFormat::Svg => "SVG",
    }
}

/// A byte count in the units a user reads, to one decimal place above a
/// kibibyte.
pub(crate) fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    const STEP: u64 = 1_024;
    if count < STEP {
        return format!("{count} B");
    }
    let mut whole = count;
    let mut remainder = 0;
    let mut unit = 0;
    while whole >= STEP && unit + 1 < UNITS.len() {
        remainder = whole % STEP;
        whole /= STEP;
        unit += 1;
    }
    // The tenth comes from the remainder of the *last* division, so it is
    // taken from the bytes themselves rather than from an already-rounded
    // quotient — and, being under one step, it cannot overflow the scaling
    // however large the file is.
    format!("{whole}.{} {}", remainder * 10 / STEP, UNITS[unit])
}

/// A zoom in parts per thousand, as the percentage a user reads.
pub(crate) fn percent(per_mille: u32) -> String {
    if per_mille.is_multiple_of(10) {
        return format!("{}%", per_mille / 10);
    }
    format!("{}.{}%", per_mille / 10, per_mille % 10)
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
    rect.top()
        .saturating_add(to_i32(rect.height.saturating_sub(font.glyph_height()) / 2))
}

#[cfg(test)]
#[path = "paint_tests.rs"]
mod tests;
