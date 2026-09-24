//! The Date & Time window's geometry and paint.
//!
//! The six civil fields are two captioned groups of the shared form-field
//! family — the date above, the time below — laid out in the band the
//! [`Dialog`] reserves for its owner's content. The family owns the row
//! chrome, the slot column every field lines up in, and the label/field
//! arithmetic, so this module holds only what is genuinely this window's: how
//! many groups there are, which fields each carries, and the order they are
//! read in.
//!
//! The window's own extent is derived from what those groups measure at the
//! active density and type ladder, so a wider ladder is seated rather than
//! pushed under the action band. A fixed-size window cannot re-grant its frame
//! region, so the extent is the one in force when it opened; a desktop that
//! re-themes afterwards keeps the window it was given.
//!
//! The rows are drawn from the [`Editor`] every frame and route no input of
//! their own: typing is validated per civil field by the editor, which is the
//! only thing that may change a value. The hit test reads the same group
//! layout the paint does, so a press cannot land on a field drawn elsewhere.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_controls::{Button, Dialog, FieldControl, FieldGroup, FieldLayout, FieldRow, TextField};
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::{Editor, Field};

/// The window's width in logical pixels: a label column and a field column,
/// and no wider.
pub const WIN_WIDTH: u32 = 460;

/// The window title, which is also what the desktop lists the app under.
pub const TITLE: &str = "Date & Time";

/// The one line explaining what the fields mean.
///
/// It names UTC because the system keeps no timezone offset, so a reading
/// that looked local would be a claim the machine cannot make.
pub const FORMAT_LINE: &str = "The machine's clock, in UTC.";

/// Index of the closing action in the dialog's band.
pub const CLOSE_ACTION: usize = 0;

/// Index of the setting action.
pub const SET_ACTION: usize = 1;

/// Fields per group: the date in the first, the time in the second.
const PER_GROUP: usize = 3;

/// The two groups' captions, in layout order. The fields each holds are
/// [`Field::ALL`] taken [`PER_GROUP`] at a time, so the order a date is
/// written is the order it is tabbed through.
const CAPTIONS: [&str; 2] = ["DATE", "TIME"];

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
        .with_actions(vec![Button::labelled("Close"), Button::labelled("Set")]);
    match editor.status().message() {
        Some(reason) => base.with_reason(reason),
        None => base,
    }
}

/// Build the two field groups for `editor`: the date above, the time below.
///
/// Rebuilt from the model each frame rather than held as state, so the drawn
/// text and the model can never disagree. The focused field is the only one
/// drawn focused, so exactly one caret shows.
#[must_use]
pub fn groups(editor: &Editor) -> Vec<FieldGroup> {
    CAPTIONS
        .iter()
        .enumerate()
        .map(|(index, caption)| {
            let first = index * PER_GROUP;
            let rows = Field::ALL
                .iter()
                .skip(first)
                .take(PER_GROUP)
                .map(|field| {
                    FieldRow::new(
                        field.label(),
                        FieldControl::Text(
                            TextField::new()
                                .with_text(editor.text(*field))
                                .with_placeholder(String::from(field.label()))
                                .with_max_len(crate::FIELD_MAX),
                        ),
                    )
                })
                .collect();
            let mut group = FieldGroup::new(*caption, rows);
            group.adopt_focus(
                (editor.focus().index() / PER_GROUP == index)
                    .then(|| editor.focus().index() % PER_GROUP),
            );
            group
        })
        .collect()
}

/// The height, in physical pixels, the window's content needs: the dialog's
/// own bands plus both groups and the gap between them.
///
/// Measured from the groups themselves, so the window is sized by what it
/// draws rather than by a figure that a wider type ladder outgrows.
#[must_use]
fn content_height(editor: &Editor, scale: Scale, theme: &Theme) -> u32 {
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let groups = groups(editor);
    let gaps = gap.saturating_mul(u32::try_from(groups.len().saturating_sub(1)).unwrap_or(0));
    let band = band_width(scale, theme);
    groups
        .iter()
        .map(|group| {
            group.measured_height(band, group.slot_column(band, scale, theme), scale, theme)
        })
        .fold(gaps, u32::saturating_add)
}

/// The width of the dialog's content band, which is what the groups are laid
/// out across and so what their wrapped descriptions measure against.
///
/// Taken from the dialog itself rather than re-derived here, so a group is
/// measured against exactly the band it is later drawn in.
#[must_use]
fn band_width(scale: Scale, theme: &Theme) -> u32 {
    Dialog::content_width(scale.scale_length(WIN_WIDTH), scale, theme)
}

/// The window's own rectangle at `scale`, which is where its pixels start.
///
/// The height is what the groups measure, turned into a plate extent by the
/// dialog itself — so the band the groups are then laid out in is exactly the
/// one they were sized for, at any density or type ladder.
#[must_use]
pub fn window_bounds(editor: &Editor, scale: Scale, theme: &Theme) -> Rect {
    let content = content_height(editor, scale, theme);
    Rect::new(
        0,
        0,
        scale.scale_length(WIN_WIDTH),
        dialog(editor).height_for_content(content, scale.scale_length(WIN_WIDTH), scale, theme),
    )
}

/// The layout each group is drawn with within the dialog's content band, in
/// layout order, and only for the groups that fit whole.
///
/// The one layout the paint and the hit test both read, so a press can never
/// land on a group that was not drawn.
#[must_use]
fn group_layouts(editor: &Editor, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<FieldLayout> {
    let Some(band) = dialog(editor).content_rect(bounds, scale, theme) else {
        return Vec::new();
    };
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let mut top = band.top();
    let mut layouts = Vec::new();
    for group in groups(editor) {
        let column = group.slot_column(band.width, scale, theme);
        let height = group.measured_height(band.width, column, scale, theme);
        let bottom = top.saturating_add(i32::try_from(height).unwrap_or(i32::MAX));
        if bottom > band.bottom() {
            break;
        }
        layouts.push(FieldLayout::new(
            Rect::new(band.left(), top, band.width, height),
            column,
        ));
        top = bottom.saturating_add(i32::try_from(gap).unwrap_or(0));
    }
    layouts
}

/// The field the window-local `point` is over, if any.
///
/// The theme is a parameter because the row a point lands on depends on the
/// active type ladder and density, exactly as the paint does — resolving it
/// from anywhere else is how a press comes to land beside the field it looked
/// like it hit.
#[must_use]
pub fn field_at(editor: &Editor, scale: Scale, theme: &Theme, point: Point) -> Option<Field> {
    let bounds = window_bounds(editor, scale, theme);
    let layouts = group_layouts(editor, bounds, scale, theme);
    groups(editor)
        .iter()
        .zip(layouts)
        .enumerate()
        .find_map(|(index, (group, layout))| {
            let row = group.row_at(layout, scale, theme, point)?;
            Field::at(index * PER_GROUP + row)
        })
}

/// Paint the whole window for `editor` at `scale` through `theme` into
/// `surface`, which the caller retains for the life of the window.
pub fn render_into(surface: &mut Surface, editor: &Editor, scale: Scale, theme: &Theme) {
    let bounds = window_bounds(editor, scale, theme);
    dialog(editor).render(surface, bounds, scale, theme);
    for (group, layout) in groups(editor)
        .iter()
        .zip(group_layouts(editor, bounds, scale, theme))
    {
        group.render(surface, layout, scale, theme);
    }
}
