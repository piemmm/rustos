//! Unit tests for the collection controls (spec §11.13–§11.16, §20 checklist).
//!
//! These cover the shared row chrome (hover/selection/pressure rails, the
//! bottom activity Heat Seam, the recovery/complete/denied Signal Bead, the
//! focus ring, and the §13 disposition), the column-alignment invariant (a
//! row's content never shifts when its state changes), table cells (alignment
//! and cell-specific state), the card's three-edge state (leading dominant
//! rail, bottom progress seam, top-trailing count/alert) with footer actions,
//! and the panel's header/content layout, header actions, and anchor notch.

use alloc::vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Contrast, Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::collection::{
    Card, CardAction, CellAlign, ListRow, Panel, PanelAction, PanelEdge, RowAction, TableCell,
    TableRow,
};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PressureKind, PressureState,
    ProgressValue, RecoveryState, SelectionState,
};

const W: u32 = 240;
const H: u32 = 28;

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
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

fn high_contrast() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test High Contrast",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        Contrast::High,
    )
}

fn row_surface(row: &ListRow, theme: &Theme, scale: Scale) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    row.render(&mut surface, Rect::new(0, 0, W, H), scale, theme, font());
    surface
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
    );
    assert!(has_pixel(&big, premul(Theme::dark().palette().on_surface)));
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
