//! The surface's one titled-block anatomy: a bordered, slightly-lighter
//! plate under a small-caps accent title with a hairline rule
//! (`plans/switchboard/02-cpu.png`, `01-tasks.png`, `08-recovery.png`).
//!
//! Every section is built from blocks — a pane's hero and detail blocks, the
//! Tasks census tiles, a fault card and its detail and timeline blocks — and
//! the boards draw all of them the same way. Defining that once is what stops
//! three sections reading as three products.
//!
//! Not a control: [`tairix_controls::Panel`] is a different anatomy (a header
//! band at control height, a dominant rail, a signal bead, an actions row) and
//! is shared with the terminal, the taskbar and the file manager, so retuning
//! it here would retune them. This is composition over the shared plate
//! primitives instead.

use tairix_controls::{inset, paint_surface_plate, plate_border, ChromeLayer};
use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

/// Paint a block's plate over `bounds`, answering the rectangle its content
/// draws inside — or [`None`] when `bounds` is too small to seat one.
///
/// The plate is what separates one block's readings from its neighbour's: a
/// hairline rim at the shared plate border, and a ground one step lighter than
/// the section it stands on. It counts as a raised plate rather than as part of
/// the surface, so on a floating theme it reads as an object on the glass
/// instead of dissolving into it.
pub(super) fn plate(
    surface: &mut Surface,
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<Rect> {
    let (x, y) = (
        u32::try_from(bounds.left()).ok()?,
        u32::try_from(bounds.top()).ok()?,
    );
    let (w, h) = (bounds.width, bounds.height);
    let radius = scale
        .scale_length(theme.metrics().control_corner_radius)
        .min(w / 2)
        .min(h / 2);
    paint_surface_plate(
        surface,
        (x, y, w, h),
        (radius, plate_border(theme, scale)),
        theme,
        (theme.palette().surface_raised, ChromeLayer::Plate),
    )?;
    content_rect(bounds, scale, theme)
}

/// The rectangle a block's content occupies inside a plate over `bounds`,
/// without drawing anything.
///
/// [`plate`] answers this after painting; a caller that must know where a
/// block's content lands *before* it has a surface — hit-testing a command,
/// placing a keyboard ring — reads it here, so the layout and the paint can
/// never disagree about where a row sits.
pub(super) fn content_rect(bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
    let (x, y) = (
        u32::try_from(bounds.left()).ok()?,
        u32::try_from(bounds.top()).ok()?,
    );
    let (ix, iy, iw, ih) = inset(
        x,
        y,
        bounds.width,
        bounds.height,
        content_inset(scale, theme),
    )?;
    Some(Rect::new(to_i32(ix), to_i32(iy), iw, ih))
}

/// The rectangle a titled block's content occupies: its plate's interior less
/// the band the title and its rule claim.
///
/// The counterpart of [`plate`] followed by [`title`], for the same reason
/// [`content_rect`] is the counterpart of [`plate`] — a titled container's
/// commands are hit-tested and focused before anything is drawn.
pub(super) fn titled_content(bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
    let inner = content_rect(bounds, scale, theme)?;
    let band = title_height(scale, theme);
    let height = inner.height.checked_sub(band)?;
    (height > 0).then(|| {
        Rect::new(
            inner.left(),
            inner.top().saturating_add(to_i32(band)),
            inner.width,
            height,
        )
    })
}

/// How tall a title and its hairline rule stand: the header line, then the
/// rule with half a control gap either side of it.
///
/// [`title`] advances by exactly this, so a caller that reserves the band and
/// the paint that fills it read one definition.
pub(super) fn title_height(scale: Scale, theme: &Theme) -> u32 {
    let gap = scale.scale_length(theme.metrics().control_gap).max(1) / 2;
    title_font(theme, scale)
        .line_height()
        .saturating_add(gap.saturating_mul(2))
        .saturating_add(plate_border(theme, scale))
}

/// The face a block's title is set in: the role a header over a list takes,
/// which is bold and below body size.
fn title_font(theme: &Theme, scale: Scale) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::SectionHeader, scale)
}

/// How far a plate's content sits inside the bounds the plate was drawn over:
/// its rim, then the theme's control padding.
///
/// The flow that lays a block's rows out reads the same figure as [`plate`],
/// so the rows land inside the plate the same paint drew rather than at its
/// edge.
pub(super) fn content_inset(scale: Scale, theme: &Theme) -> u32 {
    plate_border(theme, scale)
        .saturating_add(scale.scale_length(theme.metrics().control_inset).max(1))
}

/// Draw a block's title at the top of `rect` and the hairline rule under it,
/// answering the y its content starts at.
///
/// The section-header role — bold, below body size — is what a group heading
/// over a list is set in, and the accent is what makes a block's name read as
/// a label rather than as one more reading. The rule is the plate's own
/// separator, so a title that names self-plating content ([`bare_title`])
/// draws none.
pub(super) fn title(
    surface: &mut Surface,
    rect: Rect,
    scale: Scale,
    theme: &Theme,
    text: &str,
) -> i32 {
    let y = bare_title(surface, rect, scale, theme, text);
    let thickness = plate_border(theme, scale);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1) / 2;
    let top = y.saturating_add(to_i32(gap));
    if let (Ok(x), Ok(ry)) = (u32::try_from(rect.left()), u32::try_from(top)) {
        if top.saturating_add(to_i32(thickness)) <= rect.bottom() {
            surface.fill_rect(
                x,
                ry,
                rect.width,
                thickness,
                Color::from(theme.palette().border),
            );
        }
    }
    top.saturating_add(to_i32(thickness.saturating_add(gap)))
}

/// Draw a block's title with no rule under it, answering the y its content
/// starts at.
///
/// What a block whose body brings its own plates uses — the per-core grid,
/// the Tasks census — because the cells' own rims already separate the title
/// from the readings, and a rule as well would be a line the boards do not
/// draw.
pub(super) fn bare_title(
    surface: &mut Surface,
    rect: Rect,
    scale: Scale,
    theme: &Theme,
    text: &str,
) -> i32 {
    let font = title_font(theme, scale);
    // A block's name is model text of any length; drawn untruncated it lands
    // beside the block rather than inside it.
    font.draw_text(
        surface,
        rect.left(),
        rect.top(),
        font.truncate_to_width(text, rect.width),
        Color::from(theme.palette().accent),
    );
    rect.top()
        .saturating_add(to_i32(font.line_height().min(rect.height)))
}

#[cfg(test)]
#[path = "block_tests.rs"]
mod tests;
