//! The Date & Time window's geometry and paint.
//!
//! Every length is authored in *logical* pixels at the reference density and
//! converted through the one shared [`Scale`], so the window is the same
//! shape at any desktop UI scale and no arithmetic is repeated here.
//!
//! The rectangles are computed in exactly one place ([`field_rect`],
//! [`window_bounds`]), which the paint, the hit test, and the host tests all
//! read — a second copy could drift and put the caret somewhere the field is
//! not drawn.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{Button, Dialog, TextField};
use tairix_geometry::{Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::{Editor, Field};

/// The window's width in logical pixels: three field columns and their
/// insets, and no wider.
pub const WIN_WIDTH: u32 = 460;

/// The window's height in logical pixels: the title, the format line, two
/// rows of fields, the status line, and the action band.
pub const WIN_HEIGHT: u32 = 268;

/// The window title, which is also what the desktop lists the app under.
pub const TITLE: &str = "Date & Time";

/// The one line explaining what the fields mean.
///
/// It names UTC because the system keeps no timezone offset, so a reading
/// that looked local would be a claim the machine cannot make.
pub const FORMAT_LINE: &str = "The machine's clock, in UTC.";

/// Left and right inset of the field grid, in logical pixels.
const INSET: u32 = 18;

/// Gap between field columns, in logical pixels.
const COL_GAP: u32 = 10;

/// One field's height in logical pixels.
const FIELD_HEIGHT: u32 = 32;

/// Top of the first field row within the window, in logical pixels.
const FIELD_TOP: u32 = 88;

/// Vertical distance between the two field rows, in logical pixels.
const ROW_PITCH: u32 = 46;

/// Field columns per row: the date on the first row, the time on the second.
const COLUMNS: u32 = 3;

/// Index of the closing action in the dialog's band.
pub const CLOSE_ACTION: usize = 0;

/// Index of the setting action.
pub const SET_ACTION: usize = 1;

/// The window's own rectangle at `scale`, which is where its pixels start.
#[must_use]
pub fn window_bounds(scale: Scale) -> Rect {
    Rect::new(
        0,
        0,
        scale.scale_length(WIN_WIDTH),
        scale.scale_length(WIN_HEIGHT),
    )
}

/// The physical rectangle of `field` in the window's own space.
///
/// The fields sit in a three-column grid: year, month, day on the first row
/// and hour, minute, second on the second, which is the order a date is
/// written and so the order a user expects to tab through.
#[must_use]
pub fn field_rect(scale: Scale, field: Field) -> Rect {
    let index = u32::try_from(field.index()).unwrap_or(0);
    let column = index % COLUMNS;
    let row = index / COLUMNS;
    let inset = scale.scale_length(INSET);
    let gap = scale.scale_length(COL_GAP);
    let usable = scale
        .scale_length(WIN_WIDTH)
        .saturating_sub(inset.saturating_mul(2))
        .saturating_sub(gap.saturating_mul(COLUMNS - 1));
    let width = usable / COLUMNS;
    let left = inset + column * (width + gap);
    let top = scale.scale_length(FIELD_TOP + row * ROW_PITCH);
    Rect::new(
        i32::try_from(left).unwrap_or(i32::MAX),
        i32::try_from(top).unwrap_or(i32::MAX),
        width,
        scale.scale_length(FIELD_HEIGHT),
    )
}

/// The field the window-local point `(x, y)` is over, if any.
#[must_use]
pub fn field_at(scale: Scale, x: i32, y: i32) -> Option<Field> {
    Field::ALL
        .into_iter()
        .find(|field| field_rect(scale, *field).contains(tairix_geometry::Point::new(x, y)))
}

/// Build the dialog chrome for `editor`: the title, the format line, the
/// status beneath, and the two actions.
///
/// The status is carried as the dialog's inline reason, so a refusal is
/// stated in the window itself rather than only on `stderr` — the user who
/// pressed Set is the one who needs to know it did not happen.
#[must_use]
pub fn dialog(editor: &Editor) -> Dialog {
    let base = Dialog::new(TITLE)
        .with_message(FORMAT_LINE)
        .with_actions(alloc::vec![
            Button::labelled("Close"),
            Button::labelled("Set"),
        ]);
    match editor.status().message() {
        Some(reason) => base.with_reason(reason),
        None => base,
    }
}

/// Build the six fields for `editor`, in [`Field::ALL`] order.
///
/// Rebuilt from the model each frame rather than held as state, so the
/// drawn text and the model can never disagree. The focused field is the
/// only one drawn focused, so exactly one caret shows.
#[must_use]
pub fn fields(editor: &Editor) -> Vec<TextField> {
    Field::ALL
        .into_iter()
        .map(|field| {
            let mut control = TextField::new()
                .with_text(editor.text(field))
                .with_placeholder(String::from(field.label()))
                .with_max_len(crate::FIELD_MAX);
            control.set_focused(editor.focus() == field);
            control
        })
        .collect()
}

/// Paint the whole window for `editor` at `scale` through `theme`.
///
/// `None` when a surface that size cannot be allocated — the caller keeps
/// the frame already on screen rather than presenting nothing.
#[must_use]
pub fn render(editor: &Editor, scale: Scale, theme: &Theme) -> Option<Surface> {
    let bounds = window_bounds(scale);
    let mut surface = Surface::new(bounds.width, bounds.height)?;
    dialog(editor).render(&mut surface, bounds, scale, theme);
    for (field, control) in Field::ALL.into_iter().zip(fields(editor)) {
        control.render(&mut surface, field_rect(scale, field), scale, theme);
    }
    Some(surface)
}
