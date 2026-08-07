//! Unit tests for the collection controls (spec §11.13–§11.16, §11.34, §20
//! checklist).
//!
//! These cover the shared row chrome (hover/selection/pressure rails, the
//! bottom activity Heat Seam, the recovery/complete/denied Signal Bead, the
//! focus ring, and the spec §13 disposition), the column-alignment invariant (a
//! row's content never shifts when its state changes), table cells (alignment
//! and cell-specific state), the icon tile's plateless resting look and its
//! state marks, the card's three-edge state (leading dominant rail, bottom
//! progress seam, top-trailing count/alert) with footer actions, and the panel's
//! header/content layout, header actions, and anchor notch.
//!
//! The table-header cases cover the column-span agreement that is the point of
//! the shared column model (a header column and the cell beneath it occupy the
//! same span), the default sort order and its flip once the owner commits it,
//! the fail-closed refusals (a fixed, disabled, or denied column, and an
//! out-of-range `set_sort`), the caret's direction and side, the keyboard model,
//! degenerate bounds, and both themes plus the heavier-contrast path.

use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::collection::{
    Card, CardAction, CellAlign, HeaderAction, HeaderColumn, IconTile, ListRow, Panel, PanelAction,
    PanelEdge, RowAction, SortOrder, TableCell, TableHeader, TableRow,
};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, FocusState, PointerState,
    PressureKind, PressureState, ProgressValue, RecoveryState, SelectionState,
};
use crate::testkit::{control_font, high_contrast};

const W: u32 = 240;
const H: u32 = 28;

fn font() -> BitmapFont {
    control_font(&Theme::dark(), Scale::ONE)
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn region_has(surface: &Surface, xr: (u32, u32), yr: (u32, u32), want: Pixel) -> bool {
    (xr.0..xr.1)
        .flat_map(|x| (yr.0..yr.1).map(move |y| (x, y)))
        .any(|(x, y)| surface.get(x, y) == Some(want))
}

fn row_surface(row: &ListRow, theme: &Theme, scale: Scale) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    row.render(&mut surface, Rect::new(0, 0, W, H), scale, theme, None);
    surface
}

/// A solid square of `color` — the stand-in for an owner's rasterised icon
/// artwork, which a control blits without ever decoding image bytes.
fn artwork(side: u32, color: Color) -> Surface {
    let mut art = Surface::new(side, side).expect("artwork surface");
    art.fill(color);
    art
}

/// A colour no theme palette uses, so finding it in a render can only mean the
/// supplied artwork reached the surface.
const ART: Color = Color::rgb(255, 0, 255);

/// The bounding box `(min_x, min_y, max_x, max_y)` of `want` in `surface`.
fn bbox(surface: &Surface, want: Pixel) -> Option<(u32, u32, u32, u32)> {
    let mut found: Option<(u32, u32, u32, u32)> = None;
    for y in 0..surface.height() {
        for x in 0..surface.width() {
            if surface.get(x, y) != Some(want) {
                continue;
            }
            found = Some(match found {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
    }
    found
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

// --- ListRow (spec §11.13) ---------------------------------------------

#[test]
fn list_row_draws_label_in_both_themes() {
    for theme in [Theme::dark(), Theme::light()] {
        let surface = row_surface(&ListRow::new("Documents"), &theme, Scale::ONE);
        assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    }
}

#[test]
fn list_row_selection_draws_accent_rail_and_tint() {
    let theme = Theme::dark();
    let row = ListRow::new("Item")
        .with_state(ControlState::idle().with_selection(SelectionState::Selected));
    let surface = row_surface(&row, &theme, Scale::ONE);
    // The accent selection rail sits in the inner half of the leading gutter.
    assert!(region_has(
        &surface,
        (3, 6),
        (0, H),
        premul(theme.palette().accent)
    ));
    // The selected row lifts to the raised surface tint.
    assert!(has_pixel(&surface, premul(theme.palette().surface_raised)));
}

#[test]
fn list_row_pressure_draws_semantic_rail() {
    let theme = Theme::dark();
    let row = ListRow::new("Task")
        .with_state(ControlState::idle().with_pressure(PressureState::Under(PressureKind::Cpu)));
    let surface = row_surface(&row, &theme, Scale::ONE);
    assert!(region_has(
        &surface,
        (0, 3),
        (0, H),
        premul(theme.palette().cpu_pressure)
    ));
}

#[test]
fn list_row_pressure_and_selection_read_together() {
    let theme = Theme::dark();
    let state = ControlState::idle()
        .with_selection(SelectionState::Selected)
        .with_pressure(PressureState::Under(PressureKind::Memory));
    let surface = row_surface(&ListRow::new("Both").with_state(state), &theme, Scale::ONE);
    assert!(region_has(
        &surface,
        (0, 3),
        (0, H),
        premul(theme.palette().memory_pressure)
    ));
    assert!(region_has(
        &surface,
        (3, 6),
        (0, H),
        premul(theme.palette().accent)
    ));
}

#[test]
fn list_row_activity_heat_seam_is_proportional() {
    let theme = Theme::dark();
    let accent = premul(theme.palette().accent);
    let half = ListRow::new("x").with_state(
        ControlState::idle().with_activity(ActivityState::Progress(ProgressValue::new(500))),
    );
    let surface = row_surface(&half, &theme, Scale::ONE);
    // The bottom seam covers the left half but not the far right.
    assert!(region_has(&surface, (20, 100), (H - 2, H), accent));
    assert!(!region_has(&surface, (200, W), (H - 2, H), accent));
}

#[test]
fn list_row_complete_shows_success_bead() {
    let theme = Theme::dark();
    let row = ListRow::new("Done")
        .with_state(ControlState::idle().with_activity(ActivityState::Complete));
    let surface = row_surface(&row, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().success)));
}

#[test]
fn list_row_recovery_shows_recovery_bead() {
    let theme = Theme::dark();
    let row =
        ListRow::new("Hung").with_state(ControlState::idle().with_recovery(RecoveryState::Hung));
    let surface = row_surface(&row, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().recovery)));
}

#[test]
fn list_row_denied_shows_lock_bead_and_never_activates() {
    let theme = Theme::dark();
    let mut row = ListRow::new("Locked")
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let surface = row_surface(&row, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().denied)));
    // A denied row is not actionable (fail closed).
    assert_eq!(row.on_pointer(&moved(20, 14), Rect::new(0, 0, W, H)), None);
    assert_eq!(row.on_pointer(&PRESS, Rect::new(0, 0, W, H)), None);
    assert_eq!(row.on_pointer(&RELEASE, Rect::new(0, 0, W, H)), None);
}

#[test]
fn list_row_disabled_mutes_label() {
    let theme = Theme::dark();
    let row = ListRow::new("Off").with_state(ControlState::disabled());
    let surface = row_surface(&row, &theme, Scale::ONE);
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
}

#[test]
fn list_row_focus_draws_ring() {
    let theme = Theme::dark();
    let mut state = ControlState::idle();
    state.focus.focused = true;
    let surface = row_surface(&ListRow::new("Focus").with_state(state), &theme, Scale::ONE);
    // The focus ring is drawn in the active rim colour along the top edge.
    assert!(region_has(
        &surface,
        (0, W),
        (0, 1),
        premul(theme.palette().rim_active)
    ));
}

#[test]
fn list_row_pointer_click_activates() {
    let mut row = ListRow::new("Click");
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(row.on_pointer(&moved(20, 14), bounds), None);
    assert_eq!(row.on_pointer(&PRESS, bounds), None);
    assert_eq!(row.on_pointer(&RELEASE, bounds), Some(RowAction::Activated));
}

#[test]
fn list_row_keyboard_activates_when_focused() {
    let mut row = ListRow::new("Key");
    row.set_focused(true);
    assert_eq!(
        row.on_key(Key::Named(NamedKey::Enter)),
        Some(RowAction::Activated)
    );
    assert_eq!(row.on_key(Key::Char(' ')), Some(RowAction::Activated));
    row.set_focused(false);
    assert_eq!(row.on_key(Key::Named(NamedKey::Enter)), None);
}

#[test]
fn list_row_icon_and_trailing_render() {
    let theme = Theme::dark();
    let row = ListRow::new("Report")
        .with_icon(IconKind::Bell)
        .with_trailing("2 KB");
    let surface = row_surface(&row, &theme, Scale::ONE);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    assert!(has_pixel(
        &surface,
        premul(theme.palette().on_surface_muted)
    ));
}

#[test]
fn list_row_renders_high_contrast_and_scale() {
    let hc = high_contrast();
    let row = ListRow::new("HC")
        .with_state(ControlState::idle().with_selection(SelectionState::Selected));
    // High contrast still draws the accent selection rail.
    let surface = row_surface(&row, &hc, Scale::ONE);
    assert!(has_pixel(&surface, premul(hc.palette().accent)));
    // A doubled scale still renders the label.
    let scale = Scale::from_percent(200).expect("200%");
    let mut big = Surface::new(W * 2, H * 2).expect("surface");
    ListRow::new("Big").render(
        &mut big,
        Rect::new(0, 0, W * 2, H * 2),
        scale,
        &Theme::dark(),
        None,
    );
    assert!(has_pixel(&big, premul(Theme::dark().palette().on_surface)));
}

// --- Owner-supplied artwork --------------------------------------------
//
// A list row, a card, and a taskbar item all draw their icon through the one
// shared slot painter, so the rule is proven here for the collection controls:
// artwork the owner supplies is blitted, its absence falls back to the
// built-in class glyph, and artwork sized differently from the slot is centred
// in it rather than anchored to a corner.

#[test]
fn list_row_blits_supplied_artwork_and_falls_back_to_the_glyph_without_it() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    // An empty label so the only content in the render is the icon itself.
    let row = ListRow::new("").with_icon(IconKind::Text);
    let side = row.icon_side(bounds, Scale::ONE, &theme);
    assert!(side > 0, "the row reserves an icon column");

    let art = artwork(side, ART);
    let mut with = Surface::new(W, H).expect("surface");
    row.render(&mut with, bounds, Scale::ONE, &theme, Some(&art));
    let drawn = bbox(&with, ART.premultiply()).expect("artwork drawn");
    // Slot-sized artwork fills exactly the column the row advertised.
    assert_eq!(drawn.2 + 1 - drawn.0, side);
    assert_eq!(drawn.3 + 1 - drawn.1, side);

    // Without artwork the same row draws its built-in glyph instead…
    let glyph = row_surface(&row, &theme, Scale::ONE);
    assert!(!has_pixel(&glyph, ART.premultiply()));
    assert!(has_pixel(&glyph, premul(theme.palette().on_surface)));
    // …and a row carrying no icon draws neither.
    let bare = row_surface(&ListRow::new(""), &theme, Scale::ONE);
    assert!(!has_pixel(&bare, premul(theme.palette().on_surface)));
}

#[test]
fn a_row_with_no_icon_ignores_supplied_artwork() {
    let theme = Theme::dark();
    let art = artwork(8, ART);
    let mut s = Surface::new(W, H).expect("surface");
    ListRow::new("Documents").render(
        &mut s,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        &theme,
        Some(&art),
    );
    assert!(!has_pixel(&s, ART.premultiply()));
}

// --- Column alignment invariant (spec §11.13) --------------------------

/// The leftmost x column of a `W`×`H` surface that holds a pixel of `want`.
fn first_col(surface: &Surface, want: Pixel) -> Option<u32> {
    (0..W).find(|&x| (0..H).any(|y| surface.get(x, y) == Some(want)))
}

/// A `W`×`H` render of `row` laid out across `columns` at the unscaled
/// desktop, the counterpart of [`row_surface`]/[`header_surface`] so no table
/// case restates the render call it is really asserting about.
fn table_surface(row: &TableRow, theme: &Theme, columns: &[u32]) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    row.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        columns,
    );
    surface
}

#[test]
fn table_row_content_does_not_shift_when_selected() {
    let theme = Theme::dark();
    let on = premul(theme.palette().on_surface);
    let unselected = {
        let row = TableRow::new(vec![TableCell::new("X")]);
        first_col(&table_surface(&row, &theme, &[W]), on).expect("text drawn")
    };
    let selected = {
        let row = TableRow::new(vec![TableCell::new("X")])
            .with_state(ControlState::idle().with_selection(SelectionState::Selected));
        first_col(&table_surface(&row, &theme, &[W]), on).expect("text drawn")
    };
    assert_eq!(
        unselected, selected,
        "a selected row's content must not shift ({unselected} vs {selected})"
    );
}

#[test]
fn a_bead_bearing_rows_inner_column_is_painted_where_an_idle_rows_is() {
    // Regression test, read from the paint rather than from any query: the
    // trailing Signal Bead band is reserved unconditionally, so the *inner*
    // column boundaries a row's declared widths are scaled into must not move
    // when the row starts drawing a bead. Before the fix the band was
    // subtracted only when a bead actually drew, which rescaled every column
    // of a denied row and slid this one eight pixels leftward.
    let theme = Theme::dark();
    let on = premul(theme.palette().on_surface);
    // Only the middle cell carries text, so the leftmost emphasised mark *is*
    // where the second column's content begins (a denied row's own bead draws
    // in the denied colour, never this one).
    let cells = || vec![TableCell::new(""), TableCell::new("B"), TableCell::new("")];
    let at = |row: &TableRow| {
        first_col(&table_surface(row, &theme, &COLUMNS), on).expect("the middle cell drew")
    };
    let idle = at(&TableRow::new(cells()));
    let denied = at(&TableRow::new(cells())
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied)));
    assert_eq!(
        idle, denied,
        "a row that becomes denied must paint its inner columns unmoved ({idle} vs {denied})"
    );
}

#[test]
fn table_row_cell_rects_are_unaffected_by_a_state_that_adds_a_trailing_bead() {
    // Regression test: the trailing Signal Bead band must be reserved
    // unconditionally, so a row that merely becomes denied (and so starts
    // drawing a bead) must not shift any column relative to the same row
    // idle. Before the fix this failed because the bead band was only
    // subtracted from the content width when a bead actually drew.
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let cells = || {
        vec![
            TableCell::new("A"),
            TableCell::new("B"),
            TableCell::new("C"),
        ]
    };
    let idle = TableRow::new(cells());
    let denied = TableRow::new(cells())
        .with_state(ControlState::idle().with_authority(AuthorityState::Denied));

    assert_eq!(
        idle.cell_rects(bounds, Scale::ONE, &theme, &COLUMNS),
        denied.cell_rects(bounds, Scale::ONE, &theme, &COLUMNS),
        "a row that becomes denied must not shift any column"
    );
}

#[test]
fn a_denied_rows_columns_still_align_with_the_header_above_it() {
    // Regression test: the header reserves the identical span through
    // `row_content_span`, so a row's own bead-bearing state must not pull its
    // columns out of step with the header naming them.
    let theme = Theme::dark();
    let header = three_columns();
    let bounds = Rect::new(0, 0, W, H);
    let row = TableRow::new(vec![
        TableCell::new("A"),
        TableCell::new("B"),
        TableCell::new("C"),
    ])
    .with_state(ControlState::idle().with_authority(AuthorityState::Denied));

    let rects = row.cell_rects(bounds, Scale::ONE, &theme, &COLUMNS);
    assert_eq!(rects.len(), COLUMNS.len());
    for (i, rect) in rects.iter().enumerate() {
        let (start, end) =
            column_x_range(&header, &theme, &COLUMNS, i).unwrap_or_else(|| panic!("column {i}"));
        assert_eq!(
            rect.left(),
            xi(start),
            "column {i}'s start must match the header"
        );
        assert_eq!(
            rect.width,
            end - start,
            "column {i}'s width must match the header"
        );
    }
}

#[test]
fn table_row_cell_rects_match_where_render_draws_the_cells() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let row = TableRow::new(vec![
        TableCell::new("Name"),
        TableCell::new("Type"),
        TableCell::numeric("128"),
    ]);
    let rects = row.cell_rects(bounds, Scale::ONE, &theme, &COLUMNS);
    assert_eq!(rects.len(), COLUMNS.len());

    let surface = table_surface(&row, &theme, &COLUMNS);
    let start = first_col(&surface, premul(theme.palette().on_surface)).expect("first cell drawn");
    assert!(
        rects[0].contains(Point::new(xi(start), xi(H / 2))),
        "the first cell's own text must fall inside the rect `cell_rects` reports for it"
    );
}

#[test]
fn table_row_cell_rects_degrades_when_bounds_cannot_seat_them() {
    let theme = Theme::dark();
    let row = TableRow::new(vec![TableCell::new("A"), TableCell::new("B")]);
    let too_small = Rect::new(0, 0, 1, 1);
    assert!(
        row.cell_rects(too_small, Scale::ONE, &theme, &[100, 100])
            .is_empty(),
        "bounds with no room for content produce no cell rects"
    );
    let off_surface = Rect::new(-5, 0, W, H);
    assert!(row
        .cell_rects(off_surface, Scale::ONE, &theme, &[100, 100])
        .is_empty());
}

// --- TableCell / TableRow (spec §11.13–§11.14) -------------------------

#[test]
fn table_cell_alignment_defaults() {
    assert_eq!(TableCell::new("a").align(), CellAlign::Leading);
    assert_eq!(TableCell::numeric("42").align(), CellAlign::Trailing);
    assert_eq!(
        TableCell::new("c").with_align(CellAlign::Center).align(),
        CellAlign::Center
    );
}

#[test]
fn table_row_draws_all_cells() {
    let theme = Theme::dark();
    let row = TableRow::new(vec![
        TableCell::new("Name"),
        TableCell::new("Type"),
        TableCell::numeric("128"),
    ]);
    let surface = table_surface(&row, &theme, &[100, 80, 60]);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn table_cell_specific_state_shows_its_bead() {
    let theme = Theme::dark();
    let row = TableRow::new(vec![
        TableCell::new("ok"),
        TableCell::new("bad")
            .with_state(ControlState::idle().with_authority(AuthorityState::Denied)),
    ]);
    let surface = table_surface(&row, &theme, &[120, 120]);
    assert!(has_pixel(&surface, premul(theme.palette().denied)));
}

// --- A cell's leading identity icon (spec §11.14) ----------------------
//
// The icon is tinted with the very foreground its text is drawn in, so what
// adding it changed reads as the *difference* between two renders rather than
// as a colour of its own. The cases below therefore measure position: where
// the changed pixels are, and how far the text moved.

/// The bounding box `(min_x, min_y, max_x, max_y)` of every pixel that differs
/// between two same-sized renders, or `None` when they are identical.
fn diff_bbox(a: &Surface, b: &Surface) -> Option<(u32, u32, u32, u32)> {
    let mut found: Option<(u32, u32, u32, u32)> = None;
    for y in 0..a.height().min(b.height()) {
        for x in 0..a.width().min(b.width()) {
            if a.get(x, y) == b.get(x, y) {
                continue;
            }
            found = Some(match found {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
    }
    found
}

/// An `i32` surface coordinate as a `u32` (a laid-out column never starts off
/// the left of the surface).
fn xu(v: i32) -> u32 {
    u32::try_from(v).expect("a laid-out column starts on the surface")
}

/// The `(cell rect, padding, icon side)` a one-cell row laid out across the
/// whole width gives its cell under `theme` — read from the row's own
/// `cell_rects` and the same public theme/font metrics the cell sizes its
/// icon slot from, so a case never re-derives the layout it checks.
fn icon_slot(row: &TableRow, theme: &Theme, columns: &[u32]) -> (Rect, u32, u32) {
    let rect = row
        .cell_rects(Rect::new(0, 0, W, H), Scale::ONE, theme, columns)
        .first()
        .copied()
        .expect("the first column is laid out");
    let pad = Scale::ONE
        .scale_length(theme.metrics().control_inset)
        .max(1);
    (rect, pad, font().glyph_height().min(rect.height))
}

#[test]
fn table_cell_icon_defaults() {
    assert_eq!(
        TableCell::new("a").icon(),
        None,
        "a cell carries an icon only when asked for one"
    );
    assert_eq!(
        TableCell::new("a").with_icon(IconKind::Text).icon(),
        Some(IconKind::Text)
    );
    let numeric = TableCell::numeric("42").with_icon(IconKind::Disk);
    assert_eq!(numeric.icon(), Some(IconKind::Disk));
    assert_eq!(numeric.text(), "42");
    assert_eq!(
        numeric.align(),
        CellAlign::Trailing,
        "an icon is not an alignment; it sits ahead of the text either way"
    );
}

#[test]
fn a_cell_icon_draws_ahead_of_its_text_at_every_alignment() {
    let theme = Theme::dark();
    let on = premul(theme.palette().on_surface);
    for align in [CellAlign::Leading, CellAlign::Center, CellAlign::Trailing] {
        let plain = TableRow::new(vec![TableCell::new("Ag").with_align(align)]);
        let iconed = TableRow::new(vec![TableCell::new("Ag")
            .with_align(align)
            .with_icon(IconKind::Text)]);
        let bare = table_surface(&plain, &theme, &[W]);
        let with = table_surface(&iconed, &theme, &[W]);
        let (rect, pad, side) = icon_slot(&iconed, &theme, &[W]);
        let left = xu(rect.left()) + pad;

        // The icon takes its slot plus the gap before the text out of the
        // text's budget, so the text's own trailing ink moves by exactly what
        // each alignment implies: a leading cell by the whole reservation, a
        // centred one by half of it, a trailing one not at all.
        let reserved = side + pad;
        let shift = match align {
            CellAlign::Leading => reserved,
            CellAlign::Center => reserved / 2,
            CellAlign::Trailing => 0,
        };
        let (bare_min, _, bare_max, _) = bbox(&bare, on).expect("the text alone drew");
        let (_, _, with_max, _) = bbox(&with, on).expect("the iconed cell drew");
        assert_eq!(
            with_max,
            bare_max + shift,
            "{align:?}: the icon must displace the text by its reservation, no more"
        );

        let (dx0, _, dx1, _) = diff_bbox(&bare, &with).expect("the icon reached the surface");
        assert!(
            dx0 >= left && dx0 < left + side,
            "{align:?}: the icon draws on the cell's own leading slot ({dx0} outside {left}..{})",
            left + side
        );
        assert!(
            dx1 < xu(rect.left()) + rect.width,
            "{align:?}: nothing the icon draws may overflow its column"
        );
        if align != CellAlign::Trailing {
            continue;
        }
        // A trailing-aligned cell's text stays pinned to its trailing edge, so
        // the icon slot is the *only* thing that changed — proof the icon is
        // ahead of the text rather than drawn over it.
        assert!(
            dx1 < left + side,
            "a trailing cell's icon changes its leading slot alone"
        );
        assert!(
            dx0 < bare_min,
            "and puts ink ahead of where the text alone began"
        );
    }
}

#[test]
fn a_cell_icon_too_big_for_its_column_is_omitted_rather_than_overlapping_the_text() {
    let theme = Theme::dark();
    let on = premul(theme.palette().on_surface);
    let plain = TableRow::new(vec![TableCell::new("ab"), TableCell::new("next")]);
    let iconed = TableRow::new(vec![
        TableCell::new("ab").with_icon(IconKind::Text),
        TableCell::new("next"),
    ]);
    // The declared widths sum to the content width, so the first column is
    // laid out at exactly the width named here.
    let (content, pad, side) = icon_slot(&plain, &theme, &[1]);
    let content_w = content.width;
    // A cell seats its icon only when its own padding, the slot, the gap
    // after it, and the trailing padding all still fit: `3 × pad + side` is
    // the widest column that cannot, and a slot's width past it comfortably
    // can.
    let cramped = pad * 3 + side;
    let roomy = cramped + side;
    assert!(roomy < content_w, "both column widths fit the row");

    let narrow = [cramped, content_w - cramped];
    let bare = table_surface(&plain, &theme, &narrow);
    let with = table_surface(&iconed, &theme, &narrow);
    assert!(
        has_pixel(&bare, on),
        "the cramped column still draws its text, so the icon's absence is the icon's own doing"
    );
    assert_eq!(
        diff_bbox(&bare, &with),
        None,
        "a column too narrow to seat the icon omits it entirely rather than \
         overlapping the text or overflowing the column"
    );

    let wide = [roomy, content_w - roomy];
    let bare = table_surface(&plain, &theme, &wide);
    let with = table_surface(&iconed, &theme, &wide);
    let (rect, _, _) = icon_slot(&iconed, &theme, &wide);
    let (dx0, _, dx1, _) = diff_bbox(&bare, &with).expect("a roomy column draws the icon");
    assert!(
        dx0 >= xu(rect.left()) && dx1 < xu(rect.left()) + rect.width,
        "the icon stays inside its own column, never reaching the next one"
    );
}

#[test]
fn a_cell_icon_reads_in_both_themes_and_under_heavy_contrast() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        // Trailing-aligned, so the text stays put and the only difference
        // between the two renders is the icon itself.
        let plain = TableRow::new(vec![TableCell::numeric("42")]);
        let iconed = TableRow::new(vec![TableCell::numeric("42").with_icon(IconKind::Disk)]);
        let bare = table_surface(&plain, &theme, &[W]);
        let with = table_surface(&iconed, &theme, &[W]);
        // The heavier-contrast path doubles the leading rails, so the content
        // span — and with it the icon slot — moves; reading the slot back from
        // the row keeps the case honest about where it should now be.
        let (rect, pad, side) = icon_slot(&iconed, &theme, &[W]);
        let left = xu(rect.left()) + pad;
        let (dx0, _, dx1, _) = diff_bbox(&bare, &with).expect("the icon reads in every theme");
        assert!(
            dx0 >= left && dx1 < left + side,
            "the icon follows its theme's own content span ({dx0}..={dx1} outside {left}..{})",
            left + side
        );
    }
}

#[test]
fn a_cell_icon_never_changes_the_column_geometry() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let cells = || {
        vec![
            TableCell::new("A"),
            TableCell::new("B"),
            TableCell::numeric("3"),
        ]
    };
    let plain = TableRow::new(cells());
    let iconed = TableRow::new(
        cells()
            .into_iter()
            .map(|cell| cell.with_icon(IconKind::Folder))
            .collect(),
    );
    let rects = iconed.cell_rects(bounds, Scale::ONE, &theme, &COLUMNS);
    assert_eq!(
        plain.cell_rects(bounds, Scale::ONE, &theme, &COLUMNS),
        rects,
        "an icon lives inside a cell; it may not move a column boundary"
    );
    let header = three_columns();
    for (i, rect) in rects.iter().enumerate() {
        let (start, end) =
            column_x_range(&header, &theme, &COLUMNS, i).unwrap_or_else(|| panic!("column {i}"));
        assert_eq!(
            (rect.left(), rect.width),
            (xi(start), end - start),
            "column {i} of an iconed row still spans exactly what the header names"
        );
    }
}

#[test]
fn table_row_selection_and_activation() {
    let theme = Theme::dark();
    let mut row = TableRow::new(vec![TableCell::new("r")]);
    row.set_selected(true);
    assert!(row.is_selected());
    assert!(has_pixel(
        &table_surface(&row, &theme, &[W]),
        premul(theme.palette().accent)
    ));
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(row.on_pointer(&moved(40, 14), bounds), None);
    assert_eq!(row.on_pointer(&PRESS, bounds), None);
    assert_eq!(row.on_pointer(&RELEASE, bounds), Some(RowAction::Activated));
}

// --- TableHeader (spec §11.14) -----------------------------------------

/// The declared column widths a header and its rows are laid out across.
const COLUMNS: [u32; 3] = [100, 80, 60];

/// A `u32` coordinate as an `i32` (test coordinates always fit).
fn xi(v: u32) -> i32 {
    i32::try_from(v).expect("coordinate fits in i32")
}

fn three_columns() -> TableHeader {
    TableHeader::new(vec![
        HeaderColumn::new("Name"),
        HeaderColumn::new("Kind"),
        HeaderColumn::new("Size").with_align(CellAlign::Trailing),
    ])
}

fn header_surface(header: &TableHeader, theme: &Theme, scale: Scale, columns: &[u32]) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    header.render(&mut surface, Rect::new(0, 0, W, H), scale, theme, columns);
    surface
}

/// Whether nothing at all was painted — every pixel is still an untouched
/// surface's.
fn is_blank(surface: &Surface) -> bool {
    let blank = Surface::new(surface.width(), surface.height()).expect("blank surface");
    surface.pixels() == blank.pixels()
}

/// The half-open x range over which `column_at` reports `index`, read from the
/// header's own hit test so a test never re-derives the layout it checks.
fn column_x_range(
    header: &TableHeader,
    theme: &Theme,
    columns: &[u32],
    index: usize,
) -> Option<(u32, u32)> {
    let bounds = Rect::new(0, 0, W, H);
    let hit = |x: u32| {
        header.column_at(
            bounds,
            Scale::ONE,
            theme,
            columns,
            Point::new(xi(x), xi(H / 2)),
        )
    };
    let start = (0..W).find(|&x| hit(x) == Some(index))?;
    let end = (start..W).find(|&x| hit(x) != Some(index)).unwrap_or(W);
    Some((start, end))
}

/// The number of `want` pixels in the upper and lower halves of their own
/// bounding box.
///
/// This is how a caret's direction reads: a chevron's wide base is its flat
/// edge, so one pointing up is bottom-heavy and one pointing down top-heavy.
///
/// The two halves are counted the same number of rows deep from each end, so
/// the comparison measures the shape rather than the split. An odd row count
/// leaves a centre row belonging to neither half: charging it to one side
/// would make a bottom-heavy triangle three rows tall (two, four then five
/// pixels wide) report six above against five below and read as pointing the
/// wrong way.
fn halves(surface: &Surface, want: Pixel) -> Option<(usize, usize)> {
    let (_, y0, _, y1) = bbox(surface, want)?;
    let rows = y1 - y0 + 1;
    let deep = rows / 2;
    let row = |at: u32| {
        (0..surface.width())
            .filter(|&x| surface.get(x, at) == Some(want))
            .count()
    };
    let upper = (y0..y0 + deep).map(row).sum();
    let lower = ((y1 + 1 - deep)..=y1).map(row).sum();
    Some((upper, lower))
}

#[test]
fn header_column_defaults() {
    let column = HeaderColumn::new("Name");
    assert_eq!(column.title(), "Name");
    assert_eq!(column.align(), CellAlign::Leading);
    assert!(
        column.is_sortable(),
        "most columns are meaningful to order by, so a column opts out rather than in"
    );
    assert_eq!(column.state(), ControlState::idle());
    assert!(!HeaderColumn::fixed("Actions").is_sortable());
    assert_eq!(
        HeaderColumn::new("Size")
            .with_align(CellAlign::Trailing)
            .align(),
        CellAlign::Trailing
    );
    let mut column = HeaderColumn::new("Name").with_state(ControlState::disabled());
    assert_eq!(column.state(), ControlState::disabled());
    column.set_state(ControlState::idle());
    assert_eq!(column.state(), ControlState::idle());

    let mut header = three_columns();
    assert_eq!(header.columns().len(), 3);
    assert_eq!(header.sort(), None);
    assert_eq!(header.focus(), None);
    header.columns_mut()[1].set_state(ControlState::disabled());
    assert_eq!(header.columns()[1].state(), ControlState::disabled());
}

#[test]
fn a_header_column_spans_exactly_the_row_cell_beneath_it() {
    let theme = Theme::dark();
    let denied = ControlState::idle().with_authority(AuthorityState::Denied);
    // A denied column marks its own trailing edge, so the two marks pin where
    // columns 1 and 2 end; a leading-aligned glyph pins where column 0 begins.
    let row = TableRow::new(vec![
        TableCell::new("A"),
        TableCell::new("B").with_state(denied),
        TableCell::new("C").with_state(denied),
    ]);
    let header = TableHeader::new(vec![
        HeaderColumn::new("A"),
        HeaderColumn::new("B").with_state(denied),
        HeaderColumn::new("C").with_state(denied),
    ]);
    let beneath = table_surface(&row, &theme, &COLUMNS);
    let above = header_surface(&header, &theme, Scale::ONE, &COLUMNS);

    let mark = premul(theme.palette().denied);
    let row_marks = bbox(&beneath, mark).expect("the row's Authority Marks");
    assert_eq!(
        bbox(&above, mark),
        Some(row_marks),
        "a header column must end exactly where the cell beneath it does"
    );
    assert_eq!(
        first_col(&above, premul(theme.palette().on_surface_muted)),
        first_col(&beneath, premul(theme.palette().on_surface)),
        "…and begin exactly where that cell's content does"
    );
}

#[test]
fn a_first_press_sorts_ascending_and_the_next_flips_the_committed_order() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let mut header = three_columns();
    let (start, end) = column_x_range(&header, &theme, &COLUMNS, 1).expect("column 1 laid out");
    let over = moved(xi(start + (end - start) / 2), xi(H / 2));

    assert_eq!(
        header.on_pointer(&over, bounds, Scale::ONE, &theme, &COLUMNS),
        None
    );
    assert_eq!(
        header.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &COLUMNS),
        None,
        "a press alone commits nothing"
    );
    assert_eq!(
        header.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &COLUMNS),
        Some(HeaderAction::Sort {
            column: 1,
            order: SortOrder::Ascending
        })
    );
    assert_eq!(
        header.sort(),
        None,
        "the header reports the request; only the owner commits it"
    );

    header.set_sort(Some((1, SortOrder::Ascending)));
    header.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &COLUMNS);
    assert_eq!(
        header.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &COLUMNS),
        Some(HeaderAction::Sort {
            column: 1,
            order: SortOrder::Descending
        }),
        "pressing the already-sorted column flips its order"
    );
}

#[test]
fn an_uncommitted_request_is_asked_again_rather_than_assumed_honoured() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let mut header = three_columns();
    let (start, end) = column_x_range(&header, &theme, &COLUMNS, 0).expect("column 0 laid out");
    let over = moved(xi(start + (end - start) / 2), xi(H / 2));
    let ascending = Some(HeaderAction::Sort {
        column: 0,
        order: SortOrder::Ascending,
    });

    header.on_pointer(&over, bounds, Scale::ONE, &theme, &COLUMNS);
    header.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &COLUMNS);
    assert_eq!(
        header.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &COLUMNS),
        ascending
    );
    header.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &COLUMNS);
    assert_eq!(
        header.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &COLUMNS),
        ascending,
        "with no committed sort there is nothing to flip"
    );
}

#[test]
fn a_release_off_the_pressed_column_sorts_nothing() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let mut header = three_columns();
    let first = column_x_range(&header, &theme, &COLUMNS, 0).expect("column 0 laid out");
    let third = column_x_range(&header, &theme, &COLUMNS, 2).expect("column 2 laid out");
    header.on_pointer(
        &moved(xi(first.0 + 2), xi(H / 2)),
        bounds,
        Scale::ONE,
        &theme,
        &COLUMNS,
    );
    header.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &COLUMNS);
    header.on_pointer(
        &moved(xi(third.0 + 2), xi(H / 2)),
        bounds,
        Scale::ONE,
        &theme,
        &COLUMNS,
    );
    assert_eq!(
        header.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &COLUMNS),
        None
    );
}

#[test]
fn a_fixed_disabled_or_denied_column_refuses_to_sort() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let denied = ControlState::idle().with_authority(AuthorityState::Denied);
    let refusing = [
        HeaderColumn::fixed("Actions"),
        HeaderColumn::new("Locked").with_state(ControlState::disabled()),
        HeaderColumn::new("Secret").with_state(denied),
    ];
    for column in refusing {
        let mut header = TableHeader::new(vec![HeaderColumn::new("Name"), column]);
        let (start, end) = column_x_range(&header, &theme, &COLUMNS, 1).expect("column 1 laid out");
        let over = moved(xi(start + (end - start) / 2), xi(H / 2));
        header.on_pointer(&over, bounds, Scale::ONE, &theme, &COLUMNS);
        header.on_pointer(&PRESS, bounds, Scale::ONE, &theme, &COLUMNS);
        assert_eq!(
            header.on_pointer(&RELEASE, bounds, Scale::ONE, &theme, &COLUMNS),
            None
        );
        header.set_focus(Some(1));
        assert_eq!(header.on_key(Key::Named(NamedKey::Enter)), None);
        assert_eq!(header.on_key(Key::Char(' ')), None);
        assert_eq!(header.sort(), None);
    }
}

#[test]
fn a_denied_column_draws_its_authority_mark() {
    let theme = Theme::dark();
    let denied = ControlState::idle().with_authority(AuthorityState::Denied);
    let header = TableHeader::new(vec![
        HeaderColumn::new("Name"),
        HeaderColumn::new("Secret").with_state(denied),
    ]);
    let surface = header_surface(&header, &theme, Scale::ONE, &COLUMNS);
    assert!(
        has_pixel(&surface, premul(theme.palette().denied)),
        "a denied column shows the Authority Mark the row family draws"
    );
}

#[test]
fn set_sort_refuses_an_out_of_range_or_unsortable_column() {
    let mut header = TableHeader::new(vec![HeaderColumn::new("Name"), HeaderColumn::fixed("Act")]);
    header.set_sort(Some((5, SortOrder::Ascending)));
    assert_eq!(header.sort(), None, "an out-of-range column is refused");
    header.set_sort(Some((1, SortOrder::Ascending)));
    assert_eq!(header.sort(), None, "an unsortable column is refused");

    header.set_sort(Some((0, SortOrder::Descending)));
    assert_eq!(header.sort(), Some((0, SortOrder::Descending)));
    header.set_sort(Some((5, SortOrder::Ascending)));
    assert_eq!(
        header.sort(),
        Some((0, SortOrder::Descending)),
        "a refused request must not clamp onto another column, nor clear the real one"
    );
    header.set_sort(None);
    assert_eq!(header.sort(), None);
}

#[test]
fn the_sorted_column_alone_takes_the_emphasised_foreground() {
    let theme = Theme::dark();
    let muted = premul(theme.palette().on_surface_muted);
    let emphasised = premul(theme.palette().on_surface);
    let plain = header_surface(&three_columns(), &theme, Scale::ONE, &COLUMNS);
    assert!(has_pixel(&plain, muted), "titles read muted by default");
    assert!(!has_pixel(&plain, emphasised));

    let mut sorted = three_columns();
    sorted.set_sort(Some((1, SortOrder::Ascending)));
    let surface = header_surface(&sorted, &theme, Scale::ONE, &COLUMNS);
    assert!(has_pixel(&surface, emphasised));
    assert!(
        has_pixel(&surface, muted),
        "the columns that are not sorted stay muted"
    );
}

#[test]
fn a_sort_caret_reads_as_a_caret_at_the_unscaled_desktop() {
    let theme = Theme::dark();
    let emphasised = premul(theme.palette().on_surface);
    // An empty title leaves the caret as the only emphasised mark.
    let mut header = TableHeader::new(vec![HeaderColumn::new("")]);
    header.set_sort(Some((0, SortOrder::Ascending)));
    let surface = header_surface(&header, &theme, Scale::ONE, &[W]);
    let (x0, y0, x1, y1) = bbox(&surface, emphasised)
        .expect("a caret drawn only in coverage fringe is a grey smudge, not a mark");
    assert!(
        x1 - x0 >= 2 && y1 - y0 >= 1,
        "a triangle needs a base and a narrowing run before a direction reads: {} by {}",
        x1 - x0 + 1,
        y1 - y0 + 1
    );
    let (top, bottom) = halves(&surface, emphasised).expect("caret drawn");
    assert!(
        bottom > top,
        "an ascending caret reads upward at the plainest scale too ({top} vs {bottom})"
    );
}

#[test]
fn a_sort_caret_points_up_for_ascending_and_down_for_descending() {
    let theme = Theme::dark();
    let emphasised = premul(theme.palette().on_surface);
    // A denser scale gives the caret enough pixels for its direction to read
    // unambiguously; an empty title leaves it the only emphasised mark. The
    // surface is widened by the same factor, because a triple-density desktop
    // draws a table three times as wide in physical pixels — holding the width
    // at `W` would ask a header for a table 80 logical pixels across, which is
    // narrower than the caret's own square and so rightly draws no caret at
    // all.
    let scale = Scale::from_percent(300).expect("valid scale");
    let wide = W * 3;
    let tall = TableHeader::measured_height(scale, &theme);
    let caret = |order| {
        let mut header = TableHeader::new(vec![HeaderColumn::new("")]);
        header.set_sort(Some((0, order)));
        let mut surface = Surface::new(wide, tall).expect("surface");
        header.render(
            &mut surface,
            Rect::new(0, 0, wide, tall),
            scale,
            &theme,
            &[wide],
        );
        halves(&surface, emphasised).expect("caret drawn")
    };
    let (up_top, up_bottom) = caret(SortOrder::Ascending);
    assert!(
        up_bottom > up_top,
        "an ascending caret points up, so its wide base sits low ({up_top} vs {up_bottom})"
    );
    let (down_top, down_bottom) = caret(SortOrder::Descending);
    assert!(
        down_top > down_bottom,
        "a descending caret points down, so its wide base sits high ({down_top} vs {down_bottom})"
    );
}

#[test]
fn a_sort_caret_sits_on_the_side_the_alignment_implies() {
    let theme = Theme::dark();
    let emphasised = premul(theme.palette().on_surface);
    for (align, leading_side) in [
        (CellAlign::Leading, false),
        (CellAlign::Center, false),
        (CellAlign::Trailing, true),
    ] {
        // An empty title leaves the caret as the only emphasised mark.
        let mut header = TableHeader::new(vec![HeaderColumn::new("").with_align(align)]);
        header.set_sort(Some((0, SortOrder::Ascending)));
        let surface = header_surface(&header, &theme, Scale::ONE, &[W]);
        let (x0, _, x1, _) = bbox(&surface, emphasised).expect("caret drawn");
        let (start, end) = column_x_range(&header, &theme, &[W], 0).expect("column laid out");
        let middle = start + (end - start) / 2;
        if leading_side {
            assert!(
                x1 < middle,
                "a trailing-aligned title hugs its trailing edge, so the caret takes the other side"
            );
        } else {
            assert!(
                x0 > middle,
                "a title read left to right leads toward the caret at its trailing edge"
            );
        }
    }
}

#[test]
fn a_sort_caret_shortens_its_title_rather_than_overlapping_it() {
    let theme = Theme::dark();
    let long = "Modified at a very long moment indeed";
    let count = |header: &TableHeader, want| {
        header_surface(header, &theme, Scale::ONE, &[W])
            .pixels()
            .iter()
            .filter(|&&pixel| pixel == want)
            .count()
    };
    let sorted = |title: &str| {
        let mut header = TableHeader::new(vec![HeaderColumn::new(title)]);
        header.set_sort(Some((0, SortOrder::Ascending)));
        header
    };
    let emphasised = premul(theme.palette().on_surface);
    // The caret is identical in both sorted renders, so the difference is the
    // title's own ink.
    let with_title = count(&sorted(long), emphasised);
    let caret_only = count(&sorted(""), emphasised);
    let beside_caret = with_title.saturating_sub(caret_only);
    assert!(beside_caret > 0, "the title still draws beside the caret");
    assert!(
        beside_caret
            < count(
                &TableHeader::new(vec![HeaderColumn::new(long)]),
                premul(theme.palette().on_surface_muted)
            ),
        "the caret's slot comes out of the title's own width, never over the title"
    );
}

#[test]
fn the_keyboard_moves_focus_across_columns_and_sorts_the_focused_one() {
    let theme = Theme::dark();
    let mut header = three_columns();
    header.on_key(Key::Named(NamedKey::Right));
    assert_eq!(header.focus(), Some(0));
    header.on_key(Key::Named(NamedKey::Right));
    assert_eq!(header.focus(), Some(1));
    header.on_key(Key::Named(NamedKey::End));
    assert_eq!(header.focus(), Some(2));
    header.on_key(Key::Named(NamedKey::Right));
    assert_eq!(header.focus(), Some(0), "…and wraps at the ends");
    header.on_key(Key::Named(NamedKey::Left));
    assert_eq!(header.focus(), Some(2));
    header.on_key(Key::Named(NamedKey::Home));
    assert_eq!(header.focus(), Some(0));

    let ascending = Some(HeaderAction::Sort {
        column: 0,
        order: SortOrder::Ascending,
    });
    assert_eq!(header.on_key(Key::Char(' ')), ascending);
    assert_eq!(header.on_key(Key::Named(NamedKey::Enter)), ascending);
    assert!(
        has_pixel(
            &header_surface(&header, &theme, Scale::ONE, &COLUMNS),
            premul(theme.palette().rim_active)
        ),
        "the focused column wears the shared focus ring"
    );

    header.set_focus(Some(9));
    assert_eq!(header.focus(), None, "an out-of-range focus fails closed");
    assert_eq!(
        header.on_key(Key::Named(NamedKey::Enter)),
        None,
        "with no focused column there is nothing to sort"
    );
    assert_eq!(
        TableHeader::new(vec![]).on_key(Key::Named(NamedKey::Right)),
        None
    );
}

#[test]
fn degenerate_bounds_omit_the_header_rather_than_clipping_it() {
    let theme = Theme::dark();
    let header = three_columns();
    // The last case is narrower than the leading gutter and padding a row
    // reserves, so no column can be laid out at all.
    for (w, h) in [(0, H), (W, 0), (4, H)] {
        let bounds = Rect::new(0, 0, w, h);
        let mut surface = Surface::new(W, H).expect("surface");
        header.render(&mut surface, bounds, Scale::ONE, &theme, &COLUMNS);
        assert!(is_blank(&surface), "a header with no room draws nothing");
        assert_eq!(
            header.column_at(bounds, Scale::ONE, &theme, &COLUMNS, Point::new(1, 1)),
            None
        );
    }
    // An off-surface origin is refused rather than wrapped into the surface.
    let off = Rect::new(-4, -4, W, H);
    let mut surface = Surface::new(W, H).expect("surface");
    header.render(&mut surface, off, Scale::ONE, &theme, &COLUMNS);
    assert!(is_blank(&surface));
    assert_eq!(
        header.column_at(off, Scale::ONE, &theme, &COLUMNS, Point::new(1, 1)),
        None
    );
    // No declared widths means no columns to lay out.
    let empty = header_surface(&header, &theme, Scale::ONE, &[]);
    assert!(is_blank(&empty));
}

#[test]
fn measured_height_never_reads_shorter_than_a_control() {
    let theme = Theme::dark();
    let one = TableHeader::measured_height(Scale::ONE, &theme);
    assert!(one >= Scale::ONE.scale_length(theme.metrics().control_height));
    let dense = Scale::from_percent(200).expect("valid scale");
    assert!(TableHeader::measured_height(dense, &theme) > one);
}

#[test]
fn a_header_reads_in_both_themes_and_under_heavy_contrast() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let mut header = three_columns();
        header.set_sort(Some((2, SortOrder::Descending)));
        header.set_focus(Some(0));
        let surface = header_surface(&header, &theme, Scale::ONE, &COLUMNS);
        assert!(
            has_pixel(&surface, premul(theme.palette().on_surface_muted)),
            "an unsorted title stays muted in every theme"
        );
        assert!(
            has_pixel(&surface, premul(theme.palette().on_surface)),
            "the sorted column and its caret take the emphasised foreground"
        );
        assert!(
            has_pixel(&surface, premul(theme.palette().rim_active)),
            "the focus ring reads in every theme"
        );
    }
}

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_table_header() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, H);
    let mut a = three_columns();
    let mut b = a.clone();
    a.on_pointer(
        &moved(OFF_A.0, OFF_A.1),
        bounds,
        Scale::ONE,
        &theme,
        &COLUMNS,
    );
    b.on_pointer(
        &moved(OFF_B.0, OFF_B.1),
        bounds,
        Scale::ONE,
        &theme,
        &COLUMNS,
    );
    assert_eq!(
        a, b,
        "a coordinate clear of the header is not a drawn property"
    );
    assert_eq!(
        header_surface(&a, &theme, Scale::ONE, &COLUMNS).pixels(),
        header_surface(&b, &theme, Scale::ONE, &COLUMNS).pixels(),
        "…and the two must therefore paint identically"
    );
}

// --- IconTile (spec §11.34) --------------------------------------------

const TW: u32 = 72;
const TH: u32 = 88;

const TILE: Rect = Rect::new(0, 0, TW, TH);

/// Paint a tile with the given state over a surface pre-filled with a colour no
/// palette uses, so any pixel still carrying it is one the tile left alone.
/// That is how "a resting tile paints no plate" is checked: the backdrop
/// survives everywhere the tile did not deliberately draw.
fn tile_over_backdrop(state: ControlState, theme: &Theme, art: Option<&Surface>) -> Surface {
    let mut s = Surface::new(TW, TH).expect("surface");
    s.fill(BEHIND);
    let tile = IconTile::new("Report.txt", IconKind::Text).with_state(state);
    tile.render(&mut s, TILE, Scale::ONE, theme, art);
    s
}

/// A colour no theme palette uses, standing in for whatever lies behind a tile
/// (a window's surface, or the desktop wallpaper).
const BEHIND: Color = Color::rgb(0, 255, 128);

/// How many pixels still show [`BEHIND`] — how much of what lay behind the tile
/// is still visible through it. Counted rather than divided, so the assertions
/// below compare exact integers.
fn behind_pixels(surface: &Surface) -> usize {
    let want = BEHIND.premultiply();
    surface.pixels().iter().filter(|p| **p == want).count()
}

/// A resting tile is a picture and a label over whatever is behind it — no
/// plate, no rim, no rail. This is the whole point of the control: an icon view
/// must read as a field of pictures, not a grid of boxes.
#[test]
fn a_resting_tile_paints_no_plate_over_what_lies_behind_it() {
    for theme in [Theme::dark(), Theme::light()] {
        let s = tile_over_backdrop(ControlState::idle(), &theme, None);
        let palette = theme.palette();
        for (name, plate) in [
            ("rim", palette.rim),
            ("raised surface", palette.surface_raised),
            ("surface", palette.surface),
            ("hover wash", palette.surface_hover),
            ("accent", palette.accent),
        ] {
            assert!(
                !has_pixel(&s, premul(plate)),
                "a resting tile drew a {name} plate"
            );
        }
        // Most of the tile is still the backdrop: only the glyph and the label
        // put ink on it.
        assert!(
            behind_pixels(&s) * 2 > s.pixels().len(),
            "a resting tile covered too much of what lay behind it"
        );
        // It did draw its content, tinted with the shared surface foreground.
        assert!(has_pixel(&s, premul(palette.on_surface)));
    }
}

/// Hover and selection are both panels, but they are not the same panel: a
/// selected tile takes the selection accent *and* flips its label and glyph to
/// the on-accent foreground, so it differs from a hover in contrast and not
/// merely in hue, and the pointer can never imitate selection.
#[test]
fn hover_selection_and_press_paint_distinct_panels() {
    let theme = Theme::dark();
    let palette = theme.palette();

    let hovered = tile_over_backdrop(
        ControlState::idle().with_pointer(PointerState::Hover),
        &theme,
        None,
    );
    assert!(has_pixel(&hovered, premul(palette.surface_hover)));
    assert!(!has_pixel(&hovered, premul(palette.accent)));
    assert!(
        has_pixel(&hovered, premul(palette.on_surface)),
        "a hovered tile keeps the ordinary label contrast"
    );

    let selected = tile_over_backdrop(
        ControlState::idle().with_selection(SelectionState::Selected),
        &theme,
        None,
    );
    assert!(has_pixel(&selected, premul(palette.accent)));
    assert!(!has_pixel(&selected, premul(palette.surface_hover)));
    assert!(
        has_pixel(&selected, premul(palette.on_accent)),
        "a selected tile inverts its label and glyph onto the accent"
    );

    let pressed = tile_over_backdrop(
        ControlState::idle().with_pointer(PointerState::Pressed),
        &theme,
        None,
    );
    assert!(has_pixel(&pressed, premul(palette.surface_pressed)));

    // Each panel covers the tile, so none of them is a mere edge mark: only the
    // rounded corners can leave the backdrop showing.
    for s in [&hovered, &selected, &pressed] {
        assert!(
            behind_pixels(s) * 10 < s.pixels().len(),
            "a state panel left the tile bare"
        );
    }
}

/// Keyboard focus draws the shared Focus Ring, so it reads distinctly from a
/// pointer hover rather than sharing its look.
#[test]
fn a_focused_tile_draws_the_shared_focus_ring() {
    let theme = Theme::dark();
    let focused = tile_over_backdrop(
        ControlState::idle().with_focus(FocusState::FOCUSED),
        &theme,
        None,
    );
    // The ring is on the tile's perimeter, in the active rim colour.
    assert_eq!(
        focused.get(0, TH / 2),
        Some(premul(theme.palette().rim_active))
    );
    let hovered = tile_over_backdrop(
        ControlState::idle().with_pointer(PointerState::Hover),
        &theme,
        None,
    );
    assert!(
        !has_pixel(&hovered, premul(theme.palette().rim_active)),
        "a hover must not imitate the focus ring"
    );
}

/// An authority or recovery state shows its shape-coded Signal Bead, so a
/// denied or unhealthy item is legible without relying on colour.
#[test]
fn a_denied_or_unhealthy_tile_shows_its_signal_bead() {
    let theme = Theme::dark();
    let denied = tile_over_backdrop(
        ControlState::idle().with_authority(AuthorityState::Denied),
        &theme,
        None,
    );
    assert!(has_pixel(&denied, premul(theme.palette().denied)));
    let hung = tile_over_backdrop(
        ControlState::idle().with_recovery(RecoveryState::Hung),
        &theme,
        None,
    );
    assert!(has_pixel(&hung, premul(theme.palette().recovery)));
}

/// A disabled tile mutes its label rather than vanishing.
#[test]
fn a_disabled_tile_mutes_its_label() {
    let theme = Theme::dark();
    let s = tile_over_backdrop(ControlState::disabled(), &theme, None);
    assert!(has_pixel(&s, premul(theme.palette().on_surface_muted)));
}

/// Paint a tile with `art` in its picture slot and report the artwork's
/// bounding box, so two placements can be compared without reaching into the
/// tile's private geometry.
fn tile_artwork_bbox(theme: &Theme, art: &Surface) -> (u32, u32, u32, u32) {
    let s = tile_over_backdrop(ControlState::idle(), theme, Some(art));
    bbox(&s, ART.premultiply()).expect("artwork drawn")
}

#[test]
fn a_tile_blits_supplied_artwork_and_falls_back_to_the_glyph_without_it() {
    let theme = Theme::dark();
    let side = IconTile::icon_side(TILE, Scale::ONE, &theme);
    assert!(side > 0, "the tile reserves a picture slot");

    // Slot-sized artwork fills exactly the slot the tile advertised, so an
    // owner's cache can rasterise at that side and trust the placement.
    let drawn = tile_artwork_bbox(&theme, &artwork(side, ART));
    assert_eq!(drawn.2 + 1 - drawn.0, side);
    assert_eq!(drawn.3 + 1 - drawn.1, side);

    // Without artwork the same tile draws its built-in glyph instead, so a
    // system with no artwork on disk still shows a meaningful icon.
    let glyph = tile_over_backdrop(ControlState::idle(), &theme, None);
    assert!(!has_pixel(&glyph, ART.premultiply()));
    assert!(has_pixel(&glyph, premul(theme.palette().on_surface)));
}

#[test]
fn tile_artwork_sized_differently_from_the_slot_is_centred_in_it() {
    let theme = Theme::dark();
    let side = IconTile::icon_side(TILE, Scale::ONE, &theme);
    assert!(side > 8, "the slot has room to be over- and under-shot");

    // The slot itself, then artwork four pixels smaller and four larger.
    let slot = tile_artwork_bbox(&theme, &artwork(side, ART));
    let small = tile_artwork_bbox(&theme, &artwork(side - 4, ART));
    let large = tile_artwork_bbox(&theme, &artwork(side + 4, ART));

    // Undersized artwork sits wholly inside the slot, inset on every side
    // rather than pinned to its leading corner.
    assert!(small.0 > slot.0 && small.1 > slot.1);
    assert!(small.2 < slot.2 && small.3 < slot.3);
    // Oversized artwork overhangs the slot on both sides instead of spilling
    // from one corner.
    assert!(large.0 < slot.0 && large.2 > slot.2);
    assert!(large.1 < slot.1 && large.3 > slot.3);
    // All three share the slot's centre, to within the odd pixel of a size
    // that cannot be split evenly.
    for placed in [small, large] {
        let dx = i64::from(placed.0 + placed.2) - i64::from(slot.0 + slot.2);
        let dy = i64::from(placed.1 + placed.3) - i64::from(slot.1 + slot.3);
        assert!(dx.abs() <= 1 && dy.abs() <= 1, "{placed:?} off centre");
    }
}

/// The tile leaves the label the lower part of its bounds: the picture never
/// grows into the space the name needs, however tall the tile is.
#[test]
fn the_picture_slot_leaves_the_label_its_share_of_the_tile() {
    let theme = Theme::dark();
    for height in [40, TH, 200] {
        let bounds = Rect::new(0, 0, TW, height);
        let side = IconTile::icon_side(bounds, Scale::ONE, &theme);
        assert!(
            side * 5 <= height * 3,
            "a {height}-pixel tile gave its picture {side} pixels"
        );
    }
}

/// A tile too small for a picture, or off-surface, draws nothing and never
/// panics — every accessor stays total.
#[test]
fn a_degenerate_tile_draws_nothing() {
    let theme = Theme::dark();
    for bounds in [
        Rect::new(0, 0, 0, 0),
        Rect::new(0, 0, 1, 1),
        Rect::new(-40, -40, 8, 8),
    ] {
        let mut s = Surface::new(TW, TH).expect("surface");
        s.fill(BEHIND);
        IconTile::new("x", IconKind::Text).render(&mut s, bounds, Scale::ONE, &theme, None);
        assert_eq!(
            IconTile::icon_side(bounds, Scale::ONE, &theme),
            0,
            "a degenerate tile claimed a picture slot"
        );
        assert_eq!(
            behind_pixels(&s),
            s.pixels().len(),
            "a degenerate tile painted something"
        );
    }
}

/// Nothing a tile draws escapes its bounds, so a view can lay tiles edge to
/// edge — and bound the whole grid's paint to the area it owns — without a tile
/// bleeding onto its neighbour.
///
/// Checked at several tile heights, including ones too short for a whole line of
/// text beneath the picture: such a tile drops its label rather than writing it
/// over the tile below.
#[test]
fn a_tile_paints_only_within_its_bounds() {
    let theme = Theme::dark();
    let state = ControlState::idle()
        .with_selection(SelectionState::Selected)
        .with_focus(FocusState::FOCUSED);
    let want = BEHIND.premultiply();
    for height in [TH, font().line_height() * 2, font().line_height() + 2, 12] {
        let mut s = Surface::new(TW * 3, height * 3).expect("surface");
        s.fill(BEHIND);
        let inner = Rect::new(
            i32::try_from(TW).expect("small"),
            i32::try_from(height).expect("small"),
            TW,
            height,
        );
        IconTile::new("A very long name that cannot fit", IconKind::Folder)
            .with_state(state)
            .render(&mut s, inner, Scale::ONE, &theme, None);
        for y in 0..s.height() {
            for x in 0..s.width() {
                let inside = (TW..TW * 2).contains(&x) && (height..height * 2).contains(&y);
                if !inside {
                    assert_eq!(
                        s.get(x, y),
                        Some(want),
                        "a {height}-pixel tile painted ({x}, {y})"
                    );
                }
            }
        }
    }
}

// --- Card (spec §11.15) ------------------------------------------------

const CW: u32 = 220;
const CH: u32 = 140;

fn card_surface(card: &Card, theme: &Theme) -> Surface {
    let mut s = Surface::new(CW, CH).expect("surface");
    card.render(&mut s, Rect::new(0, 0, CW, CH), Scale::ONE, theme);
    s
}

#[test]
fn card_draws_plate_and_leading_dominant_rail() {
    let theme = Theme::dark();
    let card = Card::new("Backup").with_role(ControlRole::Primary);
    let s = card_surface(&card, &theme);
    assert!(has_pixel(&s, premul(theme.palette().surface_raised)));
    assert!(has_pixel(&s, premul(theme.palette().rim)));
    // The leading dominant rail (accent for a primary card) is on the edge.
    assert!(region_has(
        &s,
        (1, 4),
        (20, CH - 20),
        premul(theme.palette().accent)
    ));
}

#[test]
fn card_bottom_edge_carries_progress() {
    let theme = Theme::dark();
    let card = Card::new("Copy").with_state(
        ControlState::idle().with_activity(ActivityState::Progress(ProgressValue::FULL)),
    );
    let s = card_surface(&card, &theme);
    assert!(region_has(
        &s,
        (40, CW - 40),
        (CH - 4, CH - 1),
        premul(theme.palette().accent)
    ));
}

#[test]
fn card_count_badge_and_alert_bead() {
    let theme = Theme::dark();
    let counted = card_surface(&Card::new("Inbox").with_count(7), &theme);
    // The count pill is an accent plate with on-accent digits, top-trailing.
    assert!(region_has(
        &counted,
        (CW / 2, CW),
        (0, 40),
        premul(theme.palette().accent)
    ));
    assert!(has_pixel(&counted, premul(theme.palette().on_accent)));
    // With no count, an alert state shows its bead instead.
    let alert = card_surface(
        &Card::new("Fault").with_state(ControlState::idle().with_recovery(RecoveryState::Hung)),
        &theme,
    );
    assert!(region_has(
        &alert,
        (CW / 2, CW),
        (0, 40),
        premul(theme.palette().recovery)
    ));
}

#[test]
fn card_body_renders() {
    let theme = Theme::dark();
    let s = card_surface(&Card::new("T").with_body("a detail line"), &theme);
    assert!(has_pixel(&s, premul(theme.palette().on_surface_muted)));
}

#[test]
fn card_footer_action_activates_by_pointer() {
    let theme = Theme::dark();
    let mut card = Card::new("Job").with_footer(vec![Button::labelled("Run")]);
    let bounds = Rect::new(0, 0, CW, CH);
    // The single footer button spans the bottom content row; click within it.
    assert_eq!(
        card.on_pointer(&moved(100, 110), bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(card.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(CardAction::FooterActivated { index: 0 })
    );
    // The footer buttons keep their own state.
    let _ = card.footer();
    let _ = &theme;
}

#[test]
fn card_footer_action_activates_by_keyboard() {
    let mut card = Card::new("Job").with_footer(vec![Button::new(
        ButtonContent::Label(alloc::string::String::from("Go")),
        ControlRole::Primary,
    )]);
    card.footer_mut()[0].set_focused(true);
    assert_eq!(
        card.on_key(Key::Named(NamedKey::Enter)),
        Some(CardAction::FooterActivated { index: 0 })
    );
}

/// A card with one footer button, and a point in its body that no footer
/// rectangle covers — read from the card's own footer layout, so the body
/// point cannot drift out of agreement with where the footer really is.
fn card_with_footer(theme: &Theme) -> (Card, Rect, Point) {
    let card = Card::new("Job")
        .with_body("a cause")
        .with_footer(vec![Button::labelled("Run")]);
    let bounds = Rect::new(0, 0, CW, CH);
    let body = Point::new(to_i32(CW / 2), to_i32(CH / 4));
    assert!(
        card.footer_rects(bounds, Scale::ONE, theme)
            .iter()
            .all(|rect| !rect.contains(body)),
        "the chosen body point must not sit on a footer button"
    );
    (card, bounds, body)
}

#[test]
fn card_body_press_reports_pressed() {
    let theme = Theme::dark();
    let (mut card, bounds, body) = card_with_footer(&theme);
    assert_eq!(
        card.on_pointer(&moved(body.x, body.y), bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(card.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(CardAction::Pressed)
    );
}

#[test]
fn card_body_press_released_outside_reports_nothing() {
    let theme = Theme::dark();
    let (mut card, bounds, body) = card_with_footer(&theme);
    let _ = card.on_pointer(&moved(body.x, body.y), bounds, Scale::ONE, &theme);
    assert_eq!(card.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    // Leaving the card before releasing cancels the press.
    let _ = card.on_pointer(
        &moved(to_i32(CW) + 20, to_i32(CH) + 20),
        bounds,
        Scale::ONE,
        &theme,
    );
    assert_eq!(card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme), None);
    // The cancelled latch leaves nothing behind: returning and releasing
    // again without a fresh press reports nothing either.
    let _ = card.on_pointer(&moved(body.x, body.y), bounds, Scale::ONE, &theme);
    assert_eq!(card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme), None);
}

#[test]
fn card_footer_click_reports_footer_and_never_pressed() {
    let theme = Theme::dark();
    let (mut card, bounds, body) = card_with_footer(&theme);
    let footer = card.footer_rects(bounds, Scale::ONE, &theme);
    let rect = footer.first().copied().expect("one footer rectangle");
    let on_button = Point::new(
        rect.origin.x + to_i32(rect.width / 2),
        rect.origin.y + to_i32(rect.height / 2),
    );
    let _ = card.on_pointer(&moved(on_button.x, on_button.y), bounds, Scale::ONE, &theme);
    assert_eq!(card.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(CardAction::FooterActivated { index: 0 })
    );
    // The footer press never armed the body latch, so moving onto the body
    // and releasing cannot yield a stale press.
    let _ = card.on_pointer(&moved(body.x, body.y), bounds, Scale::ONE, &theme);
    assert_eq!(card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme), None);
}

#[test]
fn card_disabled_or_denied_body_press_reports_nothing() {
    let theme = Theme::dark();
    for state in [
        ControlState::disabled(),
        ControlState::idle().with_authority(AuthorityState::Denied),
    ] {
        let (card, bounds, body) = card_with_footer(&theme);
        let mut card = card.with_state(state);
        let _ = card.on_pointer(&moved(body.x, body.y), bounds, Scale::ONE, &theme);
        assert_eq!(card.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
        assert_eq!(card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme), None);
    }
}

#[test]
fn card_press_leaves_the_card_equal_and_pixel_identical() {
    let theme = Theme::dark();
    let (mut card, bounds, body) = card_with_footer(&theme);
    let resting = card.clone();
    let before = card_surface(&card, &theme);
    let _ = card.on_pointer(&moved(body.x, body.y), bounds, Scale::ONE, &theme);
    assert_eq!(card.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    // Mid-press: the body latch and pointer are hit-test state, so the card
    // still compares equal and still draws the same pixels.
    assert_eq!(card, resting);
    assert_eq!(card_surface(&card, &theme).pixels(), before.pixels());
    assert_eq!(
        card.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(CardAction::Pressed)
    );
    assert_eq!(card, resting);
    assert_eq!(card_surface(&card, &theme).pixels(), before.pixels());
}

#[test]
fn card_renders_in_light_theme() {
    let s = card_surface(&Card::new("Light"), &Theme::light());
    assert!(has_pixel(&s, premul(Theme::light().palette().on_surface)));
}

// --- Panel (spec §11.16) -----------------------------------------------

const PW: u32 = 200;
const PH: u32 = 160;

fn panel_surface(panel: &Panel, theme: &Theme) -> Surface {
    let mut s = Surface::new(PW, PH).expect("surface");
    panel.render(&mut s, Rect::new(0, 0, PW, PH), Scale::ONE, theme);
    s
}

#[test]
fn panel_layout_splits_header_and_content() {
    let theme = Theme::dark();
    let panel = Panel::new("Settings");
    let bounds = Rect::new(0, 0, PW, PH);
    let header = panel
        .header_rect(bounds, Scale::ONE, &theme)
        .expect("header");
    let content = panel
        .content_rect(bounds, Scale::ONE, &theme)
        .expect("content");
    assert!(header.height > 0 && content.height > 0);
    // The content sits strictly below the header.
    assert!(content.top() >= header.top() + i32::try_from(header.height).unwrap());
}

#[test]
fn panel_draws_title_and_header_bead() {
    let theme = Theme::dark();
    let panel = Panel::new("Status")
        .with_header_state(ControlState::idle().with_activity(ActivityState::Complete));
    let s = panel_surface(&panel, &theme);
    assert!(has_pixel(&s, premul(theme.palette().on_surface)));
    assert!(has_pixel(&s, premul(theme.palette().success)));
}

#[test]
fn panel_anchor_edge_points_at_invoker() {
    let panel_top = Panel::new("p").with_anchor(Point::new(100, -20));
    let panel_bottom = Panel::new("p").with_anchor(Point::new(100, 500));
    let panel_left = Panel::new("p").with_anchor(Point::new(-20, 80));
    let panel_right = Panel::new("p").with_anchor(Point::new(500, 80));
    let panel_inside = Panel::new("p").with_anchor(Point::new(100, 80));
    let bounds = Rect::new(0, 0, PW, PH);
    assert_eq!(panel_top.anchor_edge(bounds), Some(PanelEdge::Top));
    assert_eq!(panel_bottom.anchor_edge(bounds), Some(PanelEdge::Bottom));
    assert_eq!(panel_left.anchor_edge(bounds), Some(PanelEdge::Left));
    assert_eq!(panel_right.anchor_edge(bounds), Some(PanelEdge::Right));
    assert_eq!(panel_inside.anchor_edge(bounds), Some(PanelEdge::Bottom));
    assert_eq!(Panel::new("p").anchor_edge(bounds), None);
}

#[test]
fn panel_notch_draws_in_rim_colour() {
    let theme = Theme::dark();
    let panel = Panel::new("p").with_anchor(Point::new(100, 500));
    let s = panel_surface(&panel, &theme);
    // The bottom notch protrudes below the plate in the rim colour.
    assert!(region_has(
        &s,
        (0, PW),
        (PH - 8, PH),
        premul(theme.palette().rim)
    ));
}

#[test]
fn panel_header_action_activates() {
    let theme = Theme::dark();
    let mut panel = Panel::new("Tools").with_actions(vec![Button::new(
        ButtonContent::Icon(IconKind::Bell),
        ControlRole::Neutral,
    )]);
    let bounds = Rect::new(0, 0, PW, PH);
    let rect = {
        let rects = panel_action_rects(&panel, bounds, &theme);
        rects[0]
    };
    let cx = rect.left() + i32::try_from(rect.width).unwrap() / 2;
    let cy = rect.top() + i32::try_from(rect.height).unwrap() / 2;
    assert_eq!(
        panel.on_pointer(&moved(cx, cy), bounds, Scale::ONE, &theme),
        None
    );
    assert_eq!(panel.on_pointer(&PRESS, bounds, Scale::ONE, &theme), None);
    assert_eq!(
        panel.on_pointer(&RELEASE, bounds, Scale::ONE, &theme),
        Some(PanelAction::HeaderActivated { index: 0 })
    );
}

/// The single right-aligned header action square, derived the same way the
/// panel lays it out, so the test can aim a click at it.
fn panel_action_rects(panel: &Panel, bounds: Rect, theme: &Theme) -> alloc::vec::Vec<Rect> {
    let header = panel
        .header_rect(bounds, Scale::ONE, theme)
        .expect("header");
    let pad = 10;
    let side = header.height - pad;
    let right = u32::try_from(header.left()).unwrap() + header.width - pad;
    let top = u32::try_from(header.top()).unwrap() + (header.height - side) / 2;
    vec![Rect::new(
        i32::try_from(right - side).unwrap(),
        i32::try_from(top).unwrap(),
        side,
        side,
    )]
}

#[test]
fn panel_renders_in_light_theme() {
    let s = panel_surface(&Panel::new("Light"), &Theme::light());
    assert!(has_pixel(&s, premul(Theme::light().palette().on_surface)));
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// Two samples clear of the row, so only the recorded coordinate differs.
const OFF_A: (i32, i32) = (400, 60);
const OFF_B: (i32, i32) = (460, 70);

/// The state a pressed row *shows*, without holding the press latch.
fn shown_pressed() -> ControlState {
    let mut state = ControlState::idle();
    state.pointer = crate::state::PointerState::Pressed;
    state
}

/// The two equal columns the render-equivalence cases lay a table row out
/// across.
const PAIR: [u32; 2] = [120, 120];

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_list_row() {
    let theme = Theme::dark();
    let mut a = ListRow::new("Documents");
    let mut b = a.clone();
    a.on_pointer(&moved(OFF_A.0, OFF_A.1), Rect::new(0, 0, W, H));
    b.on_pointer(&moved(OFF_B.0, OFF_B.1), Rect::new(0, 0, W, H));
    assert_eq!(
        a, b,
        "a coordinate clear of the row is not a drawn property"
    );
    assert_eq!(
        row_surface(&a, &theme, Scale::ONE).pixels(),
        row_surface(&b, &theme, Scale::ONE).pixels(),
        "…and the two must therefore paint identically"
    );

    let mut latched = ListRow::new("Documents");
    latched.on_pointer(&moved(20, 14), Rect::new(0, 0, W, H));
    latched.on_pointer(&PRESS, Rect::new(0, 0, W, H));
    let mut shown = ListRow::new("Documents");
    shown.set_state(shown_pressed());
    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        row_surface(&latched, &theme, Scale::ONE).pixels(),
        row_surface(&shown, &theme, Scale::ONE).pixels(),
        "…and the two must therefore paint identically"
    );
}

#[test]
fn hit_test_bookkeeping_is_invisible_to_a_table_row() {
    let theme = Theme::dark();
    let cells = || vec![TableCell::new("Name"), TableCell::numeric("128")];

    let mut a = TableRow::new(cells());
    let mut b = a.clone();
    a.on_pointer(&moved(OFF_A.0, OFF_A.1), Rect::new(0, 0, W, H));
    b.on_pointer(&moved(OFF_B.0, OFF_B.1), Rect::new(0, 0, W, H));
    assert_eq!(
        a, b,
        "a coordinate clear of the row is not a drawn property"
    );
    assert_eq!(
        table_surface(&a, &theme, &PAIR).pixels(),
        table_surface(&b, &theme, &PAIR).pixels(),
        "…and the two must therefore paint identically"
    );

    let mut latched = TableRow::new(cells());
    latched.on_pointer(&moved(20, 14), Rect::new(0, 0, W, H));
    latched.on_pointer(&PRESS, Rect::new(0, 0, W, H));
    let mut shown = TableRow::new(cells());
    shown.set_state(shown_pressed());
    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        table_surface(&latched, &theme, &PAIR).pixels(),
        table_surface(&shown, &theme, &PAIR).pixels(),
        "…and the two must therefore paint identically"
    );
}

#[test]
fn a_table_row_joins_the_focus_field_without_taking_the_ring() {
    let cells = || vec![TableCell::new("Name"), TableCell::numeric("128")];

    let plain = TableRow::new(cells());
    let mut member = TableRow::new(cells());
    member.set_in_focus_field(true);
    let mut ringed = TableRow::new(cells());
    ringed.set_focused(true);

    assert!(member.state().focus.in_focus_field);
    assert!(
        !member.state().focus.focused,
        "membership is not the ring: the row's own action button holds that"
    );
    assert!(
        !ringed.state().focus.in_focus_field,
        "…and the ring is not membership: a composer states each one itself"
    );

    // Membership is part of the row's compared state, exactly as it is for a
    // list row, so a composition whose focus moved onto a row's actions is
    // not mistaken for the one before it and does get shown again.
    assert_ne!(plain, member);
    assert_ne!(ringed, member);

    let mut cleared = member.clone();
    cleared.set_in_focus_field(false);
    assert_eq!(cleared, plain, "leaving the field restores the resting row");
}

#[test]
fn both_row_kinds_can_join_a_focus_field() {
    let mut list = ListRow::new("Name");
    let mut table = TableRow::new(vec![TableCell::new("Name")]);
    list.set_in_focus_field(true);
    table.set_in_focus_field(true);
    assert_eq!(
        list.state().focus,
        table.state().focus,
        "one row family, one focus contract: a section may use either kind"
    );
}
