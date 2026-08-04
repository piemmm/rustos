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

use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::collection::{
    Card, CardAction, CellAlign, IconTile, ListRow, Panel, PanelAction, PanelEdge, RowAction,
    TableCell, TableRow,
};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, FocusState, PointerState,
    PressureKind, PressureState, ProgressValue, RecoveryState, SelectionState,
};
use crate::testkit::high_contrast;

const W: u32 = 240;
const H: u32 = 28;

fn font() -> BitmapFont {
    BitmapFont::console()
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
    row.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        scale,
        theme,
        font(),
        None,
    );
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
        font(),
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
    let side = row.icon_side(bounds, Scale::ONE, &theme, font());
    assert!(side > 0, "the row reserves an icon column");

    let art = artwork(side, ART);
    let mut with = Surface::new(W, H).expect("surface");
    row.render(&mut with, bounds, Scale::ONE, &theme, font(), Some(&art));
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
        font(),
        Some(&art),
    );
    assert!(!has_pixel(&s, ART.premultiply()));
}

// --- Column alignment invariant (spec §11.13) --------------------------

/// The leftmost x column of a `W`×`H` surface that holds a pixel of `want`.
fn first_col(surface: &Surface, want: Pixel) -> Option<u32> {
    (0..W).find(|&x| (0..H).any(|y| surface.get(x, y) == Some(want)))
}

#[test]
fn table_row_content_does_not_shift_when_selected() {
    let theme = Theme::dark();
    let on = premul(theme.palette().on_surface);
    let unselected = {
        let row = TableRow::new(vec![TableCell::new("X")]);
        let mut s = Surface::new(W, H).expect("surface");
        row.render(
            &mut s,
            Rect::new(0, 0, W, H),
            Scale::ONE,
            &theme,
            font(),
            &[W],
        );
        first_col(&s, on).expect("text drawn")
    };
    let selected = {
        let row = TableRow::new(vec![TableCell::new("X")])
            .with_state(ControlState::idle().with_selection(SelectionState::Selected));
        let mut s = Surface::new(W, H).expect("surface");
        row.render(
            &mut s,
            Rect::new(0, 0, W, H),
            Scale::ONE,
            &theme,
            font(),
            &[W],
        );
        first_col(&s, on).expect("text drawn")
    };
    assert_eq!(
        unselected, selected,
        "a selected row's content must not shift ({unselected} vs {selected})"
    );
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
    let mut s = Surface::new(W, H).expect("surface");
    row.render(
        &mut s,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        &theme,
        font(),
        &[100, 80, 60],
    );
    assert!(has_pixel(&s, premul(theme.palette().on_surface)));
}

#[test]
fn table_cell_specific_state_shows_its_bead() {
    let theme = Theme::dark();
    let row = TableRow::new(vec![
        TableCell::new("ok"),
        TableCell::new("bad")
            .with_state(ControlState::idle().with_authority(AuthorityState::Denied)),
    ]);
    let mut s = Surface::new(W, H).expect("surface");
    row.render(
        &mut s,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        &theme,
        font(),
        &[120, 120],
    );
    assert!(has_pixel(&s, premul(theme.palette().denied)));
}

#[test]
fn table_row_selection_and_activation() {
    let theme = Theme::dark();
    let mut row = TableRow::new(vec![TableCell::new("r")]);
    let mut s = Surface::new(W, H).expect("surface");
    row.set_selected(true);
    assert!(row.is_selected());
    row.render(
        &mut s,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        &theme,
        font(),
        &[W],
    );
    assert!(has_pixel(&s, premul(theme.palette().accent)));
    let bounds = Rect::new(0, 0, W, H);
    assert_eq!(row.on_pointer(&moved(40, 14), bounds), None);
    assert_eq!(row.on_pointer(&PRESS, bounds), None);
    assert_eq!(row.on_pointer(&RELEASE, bounds), Some(RowAction::Activated));
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
    tile.render(&mut s, TILE, Scale::ONE, theme, font(), art);
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
        IconTile::new("x", IconKind::Text).render(&mut s, bounds, Scale::ONE, &theme, font(), None);
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
            .render(&mut s, inner, Scale::ONE, &theme, font(), None);
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
    card.render(&mut s, Rect::new(0, 0, CW, CH), Scale::ONE, theme, font());
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
    panel.render(&mut s, Rect::new(0, 0, PW, PH), Scale::ONE, theme, font());
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

fn table_surface(row: &TableRow, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    row.render(
        &mut surface,
        Rect::new(0, 0, W, H),
        Scale::ONE,
        theme,
        font(),
        &[120, 120],
    );
    surface
}

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
        table_surface(&a, &theme).pixels(),
        table_surface(&b, &theme).pixels(),
        "…and the two must therefore paint identically"
    );

    let mut latched = TableRow::new(cells());
    latched.on_pointer(&moved(20, 14), Rect::new(0, 0, W, H));
    latched.on_pointer(&PRESS, Rect::new(0, 0, W, H));
    let mut shown = TableRow::new(cells());
    shown.set_state(shown_pressed());
    assert_eq!(latched, shown, "the press latch is not a drawn property");
    assert_eq!(
        table_surface(&latched, &theme).pixels(),
        table_surface(&shown, &theme).pixels(),
        "…and the two must therefore paint identically"
    );
}
