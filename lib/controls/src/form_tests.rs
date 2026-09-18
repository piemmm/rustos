//! Unit tests for the form-field family (spec §11.41).
//!
//! These cover the three contracts the family exists to keep — room given out
//! control, label, description; a row's authority shared into the control in
//! its slot; the owner placing an expanded choice list — plus the slot column
//! every group's controls line up in, the settle point a durable change is
//! made on, the distinct rendering of a stated absence, the fail-closed
//! refusals, the damage a pointer crossing one row reports, and both built-in
//! themes with the heavier-contrast path.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::button::{Button, ButtonContent};
use crate::combo::ComboBox;
use crate::damage::sink;
use crate::form::{FieldAction, FieldControl, FieldGroup, FieldGroupAction, FieldLayout, FieldRow};
use crate::selector::Toggle;
use crate::state::{AuthorityState, ControlState, SelectionState, ValidationState};
use crate::testkit::{control_font, high_contrast};
use crate::text::TextField;
use crate::value::Slider;

const W: u32 = 320;
const H: u32 = 30;

fn font() -> BitmapFont {
    control_font(&Theme::dark(), Scale::ONE)
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

fn choices(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| String::from(*s)).collect()
}

fn toggle_row(label: &str, on: bool) -> FieldRow {
    FieldRow::new(label, FieldControl::Toggle(Toggle::new("", on)))
}

fn row_surface(row: &FieldRow, theme: &Theme, scale: Scale, w: u32, h: u32) -> Surface {
    let mut surface = Surface::new(w, h).expect("surface");
    let layout = FieldLayout::new(Rect::new(0, 0, w, h), 0);
    let column = row.slot_width(scale, theme).unwrap_or(0);
    row.render(
        &mut surface,
        FieldLayout::new(layout.bounds, column),
        scale,
        theme,
    );
    surface
}

fn denied() -> ControlState {
    let mut state = ControlState::idle();
    state.authority = AuthorityState::Denied;
    state
}

fn press_at(
    row: &mut FieldRow,
    layout: FieldLayout,
    theme: &Theme,
    at: Point,
) -> Option<FieldAction> {
    let scale = Scale::ONE;
    let mut damage = sink();
    row.on_pointer(
        &InputEvent::PointerMoved { to: at },
        layout,
        scale,
        theme,
        &mut damage,
    );
    row.on_pointer(
        &InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        layout,
        scale,
        theme,
        &mut damage,
    );
    row.on_pointer(
        &InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        layout,
        scale,
        theme,
        &mut damage,
    )
}

// --- Room: control, then label, then description -----------------------

#[test]
fn slot_never_takes_more_than_half_the_row_content() {
    let theme = Theme::dark();
    let row = FieldRow::new(
        "Configure IPv4",
        FieldControl::Reading(String::from("a reading far wider than any half-row")),
    );
    let bounds = Rect::new(0, 0, W, H);
    let asked = row
        .slot_width(Scale::ONE, &theme)
        .expect("a reading measures");
    let slot = row
        .slot_rect(FieldLayout::new(bounds, asked), Scale::ONE, &theme)
        .expect("a slot fits");
    assert!(
        slot.width * 2 <= W,
        "slot {} took more than half of {W}",
        slot.width
    );
}

#[test]
fn the_words_are_what_a_narrowing_row_loses() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let labelled = toggle_row("Reduce motion", true);
    let wordless = toggle_row("", true);
    let asked = labelled
        .slot_width(scale, &theme)
        .expect("a toggle measures");

    // The narrowest row that has a slot at all: below this the shared row
    // chrome's own reservation leaves no content area, and the row is chrome.
    let narrowest = (1..=W)
        .find(|w| {
            labelled
                .control_rect(
                    FieldLayout::new(Rect::new(0, 0, *w, H), asked),
                    scale,
                    &theme,
                )
                .is_some()
        })
        .expect("some width seats a slot");
    assert_eq!(
        row_surface(&labelled, &theme, scale, narrowest, H).pixels(),
        row_surface(&wordless, &theme, scale, narrowest, H).pixels(),
        "the slot is served first, so the label is what goes"
    );

    // Given room, the control takes exactly the column it asked for and the
    // label is drawn beside it.
    let roomy = Rect::new(0, 0, W, H);
    assert_eq!(
        labelled
            .control_rect(FieldLayout::new(roomy, asked), scale, &theme)
            .map(|r| r.width),
        Some(asked)
    );
    assert_ne!(
        row_surface(&labelled, &theme, scale, W, H).pixels(),
        row_surface(&wordless, &theme, scale, W, H).pixels(),
        "a row with room draws its label"
    );
}

#[test]
fn the_description_goes_before_the_label_is_cut() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let font = font();
    let label = "Automatically hide the icon bar";
    let description = "The bar returns when the pointer reaches the screen edge";
    let row = toggle_row(label, false).with_description(description);
    let column = row.slot_width(scale, &theme).expect("a toggle measures");

    let tall = H + font.line_height();
    // A width that seats the whole label draws both lines.
    let roomy = Rect::new(0, 0, W * 2, tall);
    let whole = {
        let mut surface = Surface::new(W * 2, tall).expect("surface");
        row.render(&mut surface, FieldLayout::new(roomy, column), scale, &theme);
        surface
    };
    let description_band = (H, tall);
    assert!(
        band_has_ink(&whole, description_band, &theme),
        "a row with room draws its description"
    );

    // Narrowing until the label itself must be cut takes the description with
    // it: a second cut line beneath a cut name is noise.
    let cut_w = column + font.text_width(label) / 2;
    let cut = {
        let mut surface = Surface::new(cut_w, tall).expect("surface");
        row.render(
            &mut surface,
            FieldLayout::new(Rect::new(0, 0, cut_w, tall), column),
            scale,
            &theme,
        );
        surface
    };
    assert!(
        !band_has_ink(&cut, description_band, &theme),
        "an elided label drops the description rather than cutting it too"
    );
}

/// Whether any pixel in the `(top, bottom)` band differs from the row's
/// resting ground — the family's own paint is the only thing that put it
/// there.
fn band_has_ink(surface: &Surface, band: (u32, u32), theme: &Theme) -> bool {
    let ground = premul(theme.palette().surface);
    (band.0..band.1.min(surface.height()))
        .flat_map(|y| (0..surface.width()).map(move |x| (x, y)))
        .any(|(x, y)| surface.get(x, y) != Some(ground))
}

// --- A row's authority is the setting's --------------------------------

#[test]
fn a_denied_row_denies_the_control_in_its_slot() {
    let mut row = toggle_row("Set automatically", true);
    row.set_state(denied());
    let FieldControl::Toggle(toggle) = row.control() else {
        panic!("a toggle slot");
    };
    assert_eq!(toggle.state().authority, AuthorityState::Denied);
    assert!(!toggle.state().is_actionable());
}

#[test]
fn a_denied_row_refuses_the_pointer_and_the_keyboard() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let mut row = toggle_row("Set automatically", true);
    row.set_state(denied());
    let bounds = Rect::new(0, 0, W, H);
    let column = row.slot_width(scale, &theme).expect("a toggle measures");
    let layout = FieldLayout::new(bounds, column);
    let centre = row
        .control_rect(layout, scale, &theme)
        .map(|r| {
            Point::new(
                r.left() + to_i32(r.width) / 2,
                r.top() + to_i32(r.height) / 2,
            )
        })
        .expect("a control rect");

    assert_eq!(press_at(&mut row, layout, &theme, centre), None);
    row.set_focused(true);
    let mut damage = sink();
    assert_eq!(
        row.on_key(
            Key::Char(' '),
            Modifiers::default(),
            layout,
            scale,
            &theme,
            &mut damage
        ),
        None
    );
}

#[test]
fn a_pending_row_stops_the_control_taking_a_new_value() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let mut row = toggle_row("Set automatically", false);
    let mut state = ControlState::idle();
    state.validation = ValidationState::Pending;
    row.set_state(state);
    let FieldControl::Toggle(toggle) = row.control() else {
        panic!("a toggle slot");
    };
    assert!(
        !toggle.state().is_actionable(),
        "a setting mid-decision must not take another value"
    );

    let bounds = Rect::new(0, 0, W, H);
    let column = row.slot_width(scale, &theme).expect("a toggle measures");
    let layout = FieldLayout::new(bounds, column);
    let centre = row
        .control_rect(layout, scale, &theme)
        .map(|r| {
            Point::new(
                r.left() + to_i32(r.width) / 2,
                r.top() + to_i32(r.height) / 2,
            )
        })
        .expect("a control rect");
    assert_eq!(press_at(&mut row, layout, &theme, centre), None);
}

#[test]
fn a_disabled_row_disables_the_control_without_claiming_a_denial() {
    let mut row = toggle_row("Set automatically", true);
    let mut state = ControlState::idle();
    state.enabled = false;
    row.set_state(state);
    let FieldControl::Toggle(toggle) = row.control() else {
        panic!("a toggle slot");
    };
    assert!(!toggle.state().enabled);
    assert_eq!(
        toggle.state().authority,
        AuthorityState::Allowed,
        "a disabled setting is not a refused one"
    );
}

#[test]
fn an_allowed_row_reports_the_flip_its_toggle_asks_for() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let mut row = toggle_row("Set automatically", false);
    let bounds = Rect::new(0, 0, W, H);
    let column = row.slot_width(scale, &theme).expect("a toggle measures");
    let layout = FieldLayout::new(bounds, column);
    let centre = row
        .control_rect(layout, scale, &theme)
        .map(|r| {
            Point::new(
                r.left() + to_i32(r.width) / 2,
                r.top() + to_i32(r.height) / 2,
            )
        })
        .expect("a control rect");
    assert_eq!(
        press_at(&mut row, layout, &theme, centre),
        Some(FieldAction::Set { on: true })
    );
}

// --- The keyboard goes where the action is ------------------------------

#[test]
fn focus_rings_the_control_that_takes_it_and_the_row_that_cannot() {
    let mut actionable = toggle_row("Reduce motion", false);
    actionable.set_focused(true);
    assert!(
        !actionable.state().focus.focused,
        "the ring belongs to the control that acts"
    );
    assert!(actionable.state().focus.in_focus_field);

    let mut reading = FieldRow::new("Uptime", FieldControl::Reading(String::from("4 days")));
    reading.set_focused(true);
    assert!(
        reading.state().focus.focused,
        "a reading has no control to ring, so the row wears it"
    );
    assert!(!reading.state().focus.in_focus_field);
}

// --- The slot column ---------------------------------------------------

#[test]
fn every_control_in_a_group_begins_at_one_x() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let group = FieldGroup::new(
        "APPEARANCE",
        vec![
            toggle_row("Reduce motion", false),
            FieldRow::new(
                "Cursor set",
                FieldControl::Combo(ComboBox::new(choices(&["Alloy", "Contrast"]))),
            ),
            FieldRow::new("Scale", FieldControl::Slider(Slider::new(500))),
        ],
    );
    let bounds = Rect::new(0, 0, W, 200);
    let column = group.slot_column(bounds, scale, &theme);
    assert!(column > 0, "a group of measured controls resolves a column");

    let lefts: Vec<i32> = (0..group.len())
        .map(|i| {
            let rect = group
                .row_rect(i, bounds, scale, &theme)
                .expect("a row rect");
            group.rows()[i]
                .slot_rect(FieldLayout::new(rect, column), scale, &theme)
                .expect("a slot")
                .left()
        })
        .collect();
    assert!(
        lefts.windows(2).all(|w| w[0] == w[1]),
        "slots began at {lefts:?}"
    );
}

#[test]
fn a_group_holding_a_filling_control_gives_it_the_ceiling() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, 200);
    let measured = FieldGroup::new("A", vec![toggle_row("Reduce motion", false)]);
    let filling = FieldGroup::new(
        "A",
        vec![
            toggle_row("Reduce motion", false),
            FieldRow::new("Scale", FieldControl::Slider(Slider::new(500))),
        ],
    );
    assert!(
        filling.slot_column(bounds, scale, &theme) > measured.slot_column(bounds, scale, &theme),
        "a slider takes whatever column it is given, up to the ceiling"
    );
}

#[test]
fn a_rows_state_never_moves_its_own_control() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, H);
    let mut row = toggle_row("Set automatically", true);
    let column = row.slot_width(scale, &theme).expect("a toggle measures");
    let layout = FieldLayout::new(bounds, column);
    let resting = row
        .control_rect(layout, scale, &theme)
        .expect("a control rect");

    for state in [denied(), {
        let mut s = ControlState::idle();
        s.selection = SelectionState::Selected;
        s
    }] {
        row.set_state(state);
        assert_eq!(
            row.control_rect(layout, scale, &theme),
            Some(resting),
            "a bead band is reserved whether or not a bead paints in it"
        );
    }
}

#[test]
fn a_combo_is_sized_by_its_widest_choice_not_its_selection() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let combo = ComboBox::new(choices(&["Off", "A considerably longer choice"]));
    let narrow = combo.clone().with_selected(0);
    let wide = combo.with_selected(1);
    assert_eq!(
        narrow.measured_width(scale, &theme),
        wide.measured_width(scale, &theme),
        "choosing a value must not move the column"
    );
}

// --- The owner places the choice popup ---------------------------------

#[test]
fn an_expanded_slot_reports_its_row_and_anchor() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, 200);
    let mut group = FieldGroup::new(
        "GENERAL",
        vec![
            toggle_row("Reduce motion", false),
            FieldRow::new(
                "Login",
                FieldControl::Combo(ComboBox::new(choices(&["Text", "Graphical"]))),
            ),
        ],
    );
    let column = group.slot_column(bounds, scale, &theme);
    let layout = FieldLayout::new(bounds, column);
    assert_eq!(group.popup_anchor(layout, scale, &theme), None);

    let rect = group
        .row_rect(1, bounds, scale, &theme)
        .expect("a row rect");
    let slot = group.rows()[1]
        .slot_rect(FieldLayout::new(rect, column), scale, &theme)
        .expect("a slot");
    let centre = Point::new(
        slot.left() + to_i32(slot.width) / 2,
        slot.top() + to_i32(slot.height) / 2,
    );
    let mut damage = sink();
    for event in [
        InputEvent::PointerMoved { to: centre },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        group.on_pointer(&event, layout, scale, &theme, &mut damage);
    }
    assert_eq!(
        group.popup_anchor(layout, scale, &theme),
        Some((1, slot)),
        "the owner is told which row to anchor the list to"
    );
}

#[test]
fn a_group_paints_no_list_until_one_is_open() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let popup = Rect::new(0, 0, 120, 60);
    let group = FieldGroup::new(
        "GENERAL",
        vec![FieldRow::new(
            "Login",
            FieldControl::Combo(ComboBox::new(choices(&["Text", "Graphical"]))),
        )],
    );
    let mut surface = Surface::new(120, 60).expect("surface");
    group.render_popup(&mut surface, popup, scale, &theme);
    assert!(
        surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT),
        "a collapsed slot draws no list"
    );
}

// --- A durable change is made on the settle point ----------------------

#[test]
fn a_dragged_slider_settles_once() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, H);
    let mut row = FieldRow::new("Scale", FieldControl::Slider(Slider::new(0)));
    let column = W / 4;
    let layout = FieldLayout::new(bounds, column);
    let rect = row
        .control_rect(layout, scale, &theme)
        .expect("a control rect");
    let y = rect.top() + to_i32(rect.height) / 2;
    let mut damage = sink();

    let mut live = 0;
    let mut settled = 0;
    let mut feed =
        |row: &mut FieldRow, event: InputEvent, live: &mut u32, settled: &mut u32| match row
            .on_pointer(&event, layout, scale, &theme, &mut damage)
        {
            Some(FieldAction::SetValue { .. }) => *live += 1,
            Some(FieldAction::Settled { .. }) => *settled += 1,
            _ => {}
        };
    feed(
        &mut row,
        InputEvent::PointerMoved {
            to: Point::new(rect.left() + 2, y),
        },
        &mut live,
        &mut settled,
    );
    feed(
        &mut row,
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut live,
        &mut settled,
    );
    for step in 1..6 {
        feed(
            &mut row,
            InputEvent::PointerMoved {
                to: Point::new(rect.left() + 2 + step * 10, y),
            },
            &mut live,
            &mut settled,
        );
    }
    feed(
        &mut row,
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        &mut live,
        &mut settled,
    );
    assert!(live > 1, "a drag reads live, {live} samples");
    assert_eq!(settled, 1, "one drag is one durable change");
}

// --- A stated absence is not a reading ---------------------------------

#[test]
fn an_unmeasured_slot_draws_quietly_and_reports_nothing() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let statement = String::from("not measured");
    let reading = FieldRow::new("Memory", FieldControl::Reading(statement.clone()));
    let absent = FieldRow::new("Memory", FieldControl::Unmeasured(statement));

    let muted = premul(theme.palette().on_surface_muted);
    let absent_surface = row_surface(&absent, &theme, scale, W, H);
    assert!(
        has_pixel(&absent_surface, muted),
        "a stated absence is quiet"
    );
    assert_ne!(
        row_surface(&reading, &theme, scale, W, H).pixels(),
        absent_surface.pixels(),
        "a stated absence must not read as a measurement"
    );
}

#[test]
fn a_reading_row_reports_no_action_for_any_input() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, H);
    let mut row = FieldRow::new("Uptime", FieldControl::Reading(String::from("4 days")));
    let column = row.slot_width(scale, &theme).expect("a reading measures");
    let layout = FieldLayout::new(bounds, column);
    assert_eq!(
        press_at(&mut row, layout, &theme, Point::new(to_i32(W) - 10, 10)),
        None
    );
    let mut damage = sink();
    for key in [
        Key::Char(' '),
        Key::Named(NamedKey::Enter),
        Key::Named(NamedKey::Down),
    ] {
        assert_eq!(
            row.on_key(
                key,
                Modifiers::default(),
                layout,
                scale,
                &theme,
                &mut damage
            ),
            None
        );
    }
}

// --- The group's cursor -------------------------------------------------

#[test]
fn up_and_down_walk_the_rows_and_clamp_at_the_ends() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, 200);
    let mut group = FieldGroup::new(
        "A",
        vec![
            toggle_row("One", false),
            toggle_row("Two", false),
            toggle_row("Three", false),
        ],
    );
    let layout = FieldLayout::new(bounds, group.slot_column(bounds, scale, &theme));
    let mut damage = sink();
    let mut press = |key: NamedKey, group: &mut FieldGroup| {
        group.on_key(
            Key::Named(key),
            Modifiers::default(),
            layout,
            scale,
            &theme,
            &mut damage,
        )
    };

    assert_eq!(group.focus(), None);
    press(NamedKey::Down, &mut group);
    assert_eq!(group.focus(), Some(0));
    press(NamedKey::Down, &mut group);
    press(NamedKey::Down, &mut group);
    assert_eq!(group.focus(), Some(2));
    press(NamedKey::Down, &mut group);
    assert_eq!(group.focus(), Some(2), "a group is not a cycling ring");
    press(NamedKey::Up, &mut group);
    assert_eq!(group.focus(), Some(1));
    press(NamedKey::Home, &mut group);
    assert_eq!(group.focus(), Some(0));
    press(NamedKey::End, &mut group);
    assert_eq!(group.focus(), Some(2));
}

#[test]
fn a_text_slot_keeps_home_and_end_but_never_traps_the_cursor() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, 200);
    let mut group = FieldGroup::new(
        "A",
        vec![
            FieldRow::new(
                "Host name",
                FieldControl::Text(TextField::new().with_text("tairix")),
            ),
            toggle_row("Two", false),
        ],
    );
    let layout = FieldLayout::new(bounds, group.slot_column(bounds, scale, &theme));
    let mut damage = sink();
    group.adopt_focus(Some(0));

    group.on_key(
        Key::Named(NamedKey::Home),
        Modifiers::default(),
        layout,
        scale,
        &theme,
        &mut damage,
    );
    assert_eq!(group.focus(), Some(0), "Home moves a caret, not the cursor");
    group.on_key(
        Key::Named(NamedKey::Down),
        Modifiers::default(),
        layout,
        scale,
        &theme,
        &mut damage,
    );
    assert_eq!(group.focus(), Some(1), "Down always moves the cursor");
}

#[test]
fn an_out_of_range_focus_clears_rather_than_holding() {
    let theme = Theme::dark();
    let bounds = Rect::new(0, 0, W, 200);
    let mut group = FieldGroup::new("A", vec![toggle_row("One", false)]);
    let mut damage = sink();
    group.set_focus(Some(7), bounds, Scale::ONE, &theme, &mut damage);
    assert_eq!(group.focus(), None, "fail closed");
    group.adopt_focus(Some(7));
    assert_eq!(group.focus(), None);
}

// --- Damage and layout agreement ---------------------------------------

#[test]
fn a_pointer_crossing_a_row_reports_only_that_row() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, H);
    let mut row = toggle_row("Reduce motion", false);
    let layout = FieldLayout::new(bounds, row.slot_width(scale, &theme).unwrap_or(0));

    let mut enter = sink();
    row.on_pointer(
        &InputEvent::PointerMoved {
            to: Point::new(10, 10),
        },
        layout,
        scale,
        &theme,
        &mut enter,
    );
    assert!(!enter.is_empty(), "a hover enter changes how the row draws");

    let mut inside = sink();
    row.on_pointer(
        &InputEvent::PointerMoved {
            to: Point::new(12, 12),
        },
        layout,
        scale,
        &theme,
        &mut inside,
    );
    assert!(
        inside.is_empty(),
        "motion inside one row is hit-testing input, not a repaint"
    );
}

#[test]
fn a_groups_height_is_what_its_rows_actually_draw() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let rows = vec![
        toggle_row("One", false),
        toggle_row("Two", false).with_description("with a second line"),
    ];
    let group = FieldGroup::new("A", rows.clone());
    let height = group.measured_height(scale, &theme);
    let bounds = Rect::new(0, 0, W, height);
    let drawn: u32 = (0..group.len())
        .map(|i| {
            group
                .row_rect(i, bounds, scale, &theme)
                .expect("every row fits its own measured height")
                .height
        })
        .sum();
    let wanted: u32 = rows.iter().map(|r| r.measured_height(scale, &theme)).sum();
    assert_eq!(drawn, wanted);
}

#[test]
fn a_plate_too_short_for_every_row_omits_the_ones_it_cannot_draw() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let group = FieldGroup::new(
        "A",
        vec![
            toggle_row("One", false),
            toggle_row("Two", false),
            toggle_row("Three", false),
        ],
    );
    let full = group.measured_height(scale, &theme);
    let short = Rect::new(
        0,
        0,
        W,
        full - group.rows()[0].measured_height(scale, &theme),
    );
    assert!(group.row_rect(0, short, scale, &theme).is_some());
    assert_eq!(
        group.row_rect(2, short, scale, &theme),
        None,
        "a row that was not drawn cannot be pressed"
    );
    assert_eq!(
        group.row_at(short, scale, &theme, Point::new(10, to_i32(full) - 4)),
        None
    );
}

#[test]
fn a_degenerate_row_draws_nothing_and_answers_nothing() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let mut row = toggle_row("Reduce motion", false);
    let layout = FieldLayout::new(Rect::new(0, 0, 0, 0), 0);
    let mut surface = Surface::new(4, 4).expect("surface");
    row.render(&mut surface, layout, scale, &theme);
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
    assert_eq!(row.slot_rect(layout, scale, &theme), None);
    assert_eq!(press_at(&mut row, layout, &theme, Point::new(0, 0)), None);
}

// --- Both themes, and the heavier-contrast path -------------------------

#[test]
fn every_appearance_draws_the_family() {
    let scale = Scale::ONE;
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let group = FieldGroup::new(
            "APPEARANCE",
            vec![
                toggle_row("Reduce motion", true).with_description("Animations become instant"),
                FieldRow::new(
                    "Cursor set",
                    FieldControl::Combo(ComboBox::new(choices(&["Alloy", "Contrast"]))),
                ),
                FieldRow::new("Uptime", FieldControl::Reading(String::from("4 days"))),
                FieldRow::new(
                    "Choose",
                    FieldControl::Button(Button::new(
                        ButtonContent::IconLabel {
                            icon: IconKind::Image,
                            label: String::from("Choose Picture…"),
                        },
                        crate::state::ControlRole::Neutral,
                    )),
                ),
            ],
        )
        .with_footnote("Applies to this account only.");
        let height = group.measured_height(scale, &theme);
        let mut surface = Surface::new(W, height).expect("surface");
        let bounds = Rect::new(0, 0, W, height);
        group.render(
            &mut surface,
            FieldLayout::new(bounds, group.slot_column(bounds, scale, &theme)),
            scale,
            &theme,
        );
        assert!(
            surface.pixels().iter().any(|p| *p != Pixel::TRANSPARENT),
            "{} drew nothing",
            theme.name()
        );
    }
}

#[test]
fn a_group_reports_which_row_acted() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let bounds = Rect::new(0, 0, W, 200);
    let mut group = FieldGroup::new(
        "A",
        vec![toggle_row("One", false), toggle_row("Two", false)],
    );
    let column = group.slot_column(bounds, scale, &theme);
    let layout = FieldLayout::new(bounds, column);
    let rect = group
        .row_rect(1, bounds, scale, &theme)
        .expect("a row rect");
    let control = group.rows()[1]
        .control_rect(FieldLayout::new(rect, column), scale, &theme)
        .expect("a control rect");
    let centre = Point::new(
        control.left() + to_i32(control.width) / 2,
        control.top() + to_i32(control.height) / 2,
    );
    let mut damage = sink();
    let mut last = None;
    for event in [
        InputEvent::PointerMoved { to: centre },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        if let Some(action) = group.on_pointer(&event, layout, scale, &theme, &mut damage) {
            last = Some(action);
        }
    }
    assert_eq!(
        last,
        Some(FieldGroupAction {
            row: 1,
            action: FieldAction::Set { on: true }
        })
    );
}
