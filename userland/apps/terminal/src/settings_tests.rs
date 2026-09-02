//! Unit tests for the in-window settings sheet.
//!
//! Every geometric probe reads the sheet's *own* layout (`panel_bounds`,
//! `bands`, `scrolled_model`, `laid_out_rows`, `split_row`, `footer_split`)
//! rather than restating it, so a test can never assert against a rectangle
//! the sheet does not actually draw or hit-test.

use tairix_controls::damage;
use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::effects::{EffectKey, Effects, FULL, MIN_OPACITY};
use crate::profile::{Profile, MAX_FONT_SIZE_PX, MIN_FONT_SIZE_PX};
use crate::scheme::Scheme;

use super::{footer_split, panel_bounds, split_row, Focus, Settings, SheetOutcome, EFFECTS_TAB};

const SCALE: Scale = Scale::ONE;

/// The client rectangle a 640x480 screen leaves once window furniture is
/// taken — the smallest screen the sheet must stay usable on.
const CLIENT: Rect = Rect::new(0, 0, 608, 435);

/// A viewport far too small for the sheet's rows.
const TINY: Rect = Rect::new(0, 0, 120, 80);

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};

const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

fn theme() -> Theme {
    Theme::dark()
}

fn font() -> BitmapFont {
    BitmapFont::monospace(13)
}

fn sheet() -> Settings {
    Settings::new(&Profile::default())
}

fn moved(at: Point) -> InputEvent {
    InputEvent::PointerMoved { to: at }
}

fn surface(viewport: Rect) -> Surface {
    Surface::new(viewport.width, viewport.height).expect("a test surface")
}

// --- Probes onto the sheet's own layout -----------------------------------

/// The sheet's four content bands for `viewport`.
fn bands(
    sheet: &Settings,
    viewport: Rect,
) -> (Option<Rect>, Option<Rect>, Option<Rect>, Option<Rect>) {
    let bounds = panel_bounds(viewport, SCALE);
    let content = sheet
        .panel
        .content_rect(bounds, SCALE, &theme())
        .expect("the panel has a content rectangle");
    sheet.bands(content, SCALE, &theme())
}

/// The scrollable body band for `viewport`.
fn body(sheet: &Settings, viewport: Rect) -> Rect {
    bands(sheet, viewport).1.expect("the body band is laid out")
}

/// The rectangle of `row` as the sheet lays it out, or `None` when it is
/// scrolled out of the body.
fn row_rect(sheet: &Settings, viewport: Rect, row: Focus) -> Option<Rect> {
    let body = body(sheet, viewport);
    let offset = sheet
        .scrolled_model(Some(body), SCALE, &theme(), font())
        .offset();
    sheet
        .laid_out_rows(body, offset, SCALE, &theme(), font())
        .into_iter()
        .find(|(laid, _)| *laid == row)
        .map(|(_, rect)| rect)
}

/// The rectangle of `row`, insisting it is currently visible.
fn visible_row(sheet: &Settings, viewport: Rect, row: Focus) -> Rect {
    row_rect(sheet, viewport, row).unwrap_or_else(|| panic!("{row:?} is laid out in the body"))
}

/// The *Restore defaults* and *Done* button rectangles.
fn footer_buttons(sheet: &Settings, viewport: Rect) -> (Rect, Rect) {
    let footer = bands(sheet, viewport)
        .3
        .expect("the footer band is laid out");
    let (restore, done) = footer_split(footer, SCALE);
    (
        restore.expect("the restore button is laid out"),
        done.expect("the done button is laid out"),
    )
}

/// A point `permille` of the way along a slider row's control column.
fn slider_point(row: Rect, permille: u32) -> Point {
    let (_, control) = split_row(row, SCALE);
    let along = to_i32(control.width.saturating_mul(permille.min(1000)) / 1000);
    let x = (control.left() + along).min(control.right() - 1);
    Point::new(x, row.top() + to_i32(row.height) / 2)
}

/// The centre of `rect`.
fn centre(rect: Rect) -> Point {
    Point::new(
        rect.left() + to_i32(rect.width) / 2,
        rect.top() + to_i32(rect.height) / 2,
    )
}

// --- Gestures --------------------------------------------------------------

/// Move the pointer to `at` and press, reporting the press outcome — the
/// gesture a slider commits on.
fn press_at(sheet: &mut Settings, viewport: Rect, at: Point) -> SheetOutcome {
    sheet.on_pointer(&moved(at), viewport, SCALE, &theme(), &mut damage::sink());
    sheet.on_pointer(&PRESS, viewport, SCALE, &theme(), &mut damage::sink())
}

/// A complete primary click at `at`, reporting the release outcome — the
/// gesture a radio or button commits on.
fn click_at(sheet: &mut Settings, viewport: Rect, at: Point) -> SheetOutcome {
    press_at(sheet, viewport, at);
    sheet.on_pointer(&RELEASE, viewport, SCALE, &theme(), &mut damage::sink())
}

/// One unmodified key press.
fn key(sheet: &mut Settings, viewport: Rect, key: Key) -> SheetOutcome {
    sheet.on_key(
        key,
        Modifiers::default(),
        viewport,
        SCALE,
        &theme(),
        &mut damage::sink(),
    )
}

/// One Shift-modified key press.
fn shift_key(sheet: &mut Settings, viewport: Rect, key: Key) -> SheetOutcome {
    let modifiers = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    sheet.on_key(
        key,
        modifiers,
        viewport,
        SCALE,
        &theme(),
        &mut damage::sink(),
    )
}

/// Tab forward until `target` holds focus.
fn focus_on(sheet: &mut Settings, viewport: Rect, target: Focus) {
    for _ in 0..=sheet.focus_order().len() {
        if sheet.focus == target {
            return;
        }
        key(sheet, viewport, Key::Named(NamedKey::Tab));
    }
    panic!("Tab traversal never reached {target:?}");
}

/// Select the Effects tab from the keyboard alone (focus opens on the strip).
fn select_effects_tab(sheet: &mut Settings, viewport: Rect) {
    focus_on(sheet, viewport, Focus::Tabs);
    key(sheet, viewport, Key::Named(NamedKey::Right));
    key(sheet, viewport, Key::Named(NamedKey::Enter));
    assert_eq!(sheet.tabs.selected(), Some(EFFECTS_TAB));
}

/// Every effect value, in [`EffectKey::ALL`] order.
fn effect_values(effects: Effects) -> [u16; EffectKey::COUNT] {
    EffectKey::ALL.map(|key| key.of(effects))
}

// --- Rendering -------------------------------------------------------------

#[test]
fn renders_at_the_small_screen_client_budget() {
    let sheet = sheet();
    let mut surface = surface(CLIENT);
    sheet.render(&mut surface, CLIENT, SCALE, &theme());
    assert!(surface.pixels().iter().any(|pixel| pixel.a > 0));
}

#[test]
fn renders_at_a_tiny_viewport_without_panicking() {
    let sheet = sheet();
    let mut surface = surface(TINY);
    sheet.render(&mut surface, TINY, SCALE, &theme());
}

#[test]
fn renders_the_effects_tab_at_both_viewports() {
    for viewport in [CLIENT, TINY] {
        let mut sheet = sheet();
        select_effects_tab(&mut sheet, viewport);
        let mut surface = surface(viewport);
        sheet.render(&mut surface, viewport, SCALE, &theme());
    }
}

#[test]
fn renders_under_the_light_theme_too() {
    let sheet = sheet();
    let mut surface = surface(CLIENT);
    sheet.render(&mut surface, CLIENT, SCALE, &Theme::light());
    assert!(surface.pixels().iter().any(|pixel| pixel.a > 0));
}

#[test]
fn the_panel_is_inset_from_a_viewport_larger_than_it() {
    let wide = Rect::new(0, 0, 1280, 900);
    let bounds = panel_bounds(wide, SCALE);
    assert!(
        bounds.left() > wide.left(),
        "the panel leaves a margin to click out of"
    );
    assert!(bounds.right() < wide.right());
    assert!(bounds.top() > wide.top());
    assert!(bounds.bottom() < wide.bottom());
}

// --- Appearance: the scheme choice ----------------------------------------

#[test]
fn a_scheme_radio_puts_that_scheme_in_force() {
    let mut sheet = sheet();
    let wanted = Scheme::ALL[1];
    assert_ne!(
        sheet.profile().scheme,
        wanted,
        "the test must actually change it"
    );

    let row = visible_row(&sheet, CLIENT, Focus::Scheme(1));
    assert_eq!(
        click_at(&mut sheet, CLIENT, centre(row)),
        SheetOutcome::Edited
    );
    assert_eq!(sheet.profile().scheme, wanted);
    assert!(sheet.scheme_radios[1].is_selected());
    assert!(!sheet.scheme_radios[0].is_selected());
}

#[test]
fn every_scheme_is_offered_as_its_own_radio() {
    let sheet = sheet();
    assert_eq!(sheet.scheme_radios.len(), Scheme::ALL.len());
    for (radio, scheme) in sheet.scheme_radios.iter().zip(Scheme::ALL) {
        assert_eq!(radio.label(), scheme.label());
    }
}

// --- Appearance: the text size ---------------------------------------------

#[test]
fn the_text_size_slider_edits_the_font_size_within_its_bounds() {
    let mut sheet = sheet();
    focus_on(&mut sheet, CLIENT, Focus::TextSize);

    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::End)),
        SheetOutcome::Edited
    );
    assert_eq!(sheet.profile().font_size_px, MAX_FONT_SIZE_PX);

    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Home)),
        SheetOutcome::Edited
    );
    assert_eq!(sheet.profile().font_size_px, MIN_FONT_SIZE_PX);

    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Right)),
        SheetOutcome::Edited
    );
    let stepped = sheet.profile().font_size_px;
    assert!(
        (MIN_FONT_SIZE_PX..=MAX_FONT_SIZE_PX).contains(&stepped) && stepped > MIN_FONT_SIZE_PX,
        "one line step moves the size up and stays in range, got {stepped}"
    );
}

// --- Appearance: the custom-scheme editor ----------------------------------

#[test]
fn a_channel_slider_edits_the_selected_well_of_the_custom_scheme() {
    let mut sheet = sheet();
    let before = sheet.profile().custom;
    assert_eq!(
        sheet.swatches.selected(),
        0,
        "the background well opens selected"
    );

    focus_on(&mut sheet, CLIENT, Focus::Channel(0));
    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::End)),
        SheetOutcome::Edited
    );

    assert_eq!(sheet.profile().custom.background.r, u8::MAX);
    assert_eq!(sheet.profile().custom.background.g, before.background.g);
    assert_eq!(sheet.profile().custom.background.b, before.background.b);
    assert_eq!(
        sheet.profile().custom.foreground,
        before.foreground,
        "only the selected well is edited"
    );
}

#[test]
fn selecting_another_well_repoints_the_channel_sliders() {
    let mut sheet = sheet();
    sheet.swatches.set_selected(1);
    sheet.sync_channel_sliders();

    focus_on(&mut sheet, CLIENT, Focus::Channel(2));
    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Home)),
        SheetOutcome::Edited
    );

    assert_eq!(sheet.profile().custom.foreground.b, 0);
    assert_ne!(
        sheet.profile().custom.background,
        sheet.profile().custom.foreground,
        "the background well was left alone"
    );
}

// --- Effects ---------------------------------------------------------------

#[test]
fn every_effect_slider_edits_only_its_own_profile_field() {
    let defaults = effect_values(Effects::default());
    for index in 0..EffectKey::COUNT {
        let mut sheet = sheet();
        select_effects_tab(&mut sheet, CLIENT);
        let row = visible_row(&sheet, CLIENT, Focus::Effect(index));

        // The end of travel furthest from this effect's own default, so the
        // press is an edit for every slider — one whose default already sits
        // mid-travel included.
        let to = if defaults[index] >= 500 { 0 } else { 1000 };
        assert_eq!(
            press_at(&mut sheet, CLIENT, slider_point(row, to)),
            SheetOutcome::Edited,
            "effect {index} reports the edit"
        );

        let after = effect_values(sheet.profile().effects);
        for (other, value) in after.iter().enumerate() {
            if other == index {
                assert_ne!(*value, defaults[other], "effect {index} moved");
            } else {
                assert_eq!(*value, defaults[other], "effect {other} was left alone");
            }
        }
    }
}

#[test]
fn the_opacity_slider_spans_its_own_floor_to_full() {
    let mut sheet = sheet();
    select_effects_tab(&mut sheet, CLIENT);
    let row = visible_row(&sheet, CLIENT, Focus::Effect(0));

    assert_eq!(
        press_at(&mut sheet, CLIENT, slider_point(row, 0)),
        SheetOutcome::Edited
    );
    assert_eq!(
        sheet.profile().effects.opacity,
        MIN_OPACITY,
        "the low end of the travel is the readable floor, not a dead zone"
    );

    sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage::sink());
    assert_eq!(
        press_at(&mut sheet, CLIENT, slider_point(row, 1000)),
        SheetOutcome::Edited
    );
    assert_eq!(sheet.profile().effects.opacity, FULL);
}

#[test]
fn an_effect_slider_reaches_full_at_the_end_of_its_travel() {
    let mut sheet = sheet();
    select_effects_tab(&mut sheet, CLIENT);
    let row = visible_row(&sheet, CLIENT, Focus::Effect(1));
    assert_eq!(
        press_at(&mut sheet, CLIENT, slider_point(row, 1000)),
        SheetOutcome::Edited
    );
    assert_eq!(sheet.profile().effects.blur, FULL);
}

#[test]
fn the_profile_stays_clamped_after_extreme_values() {
    let mut sheet = sheet();
    select_effects_tab(&mut sheet, CLIENT);
    for index in 0..EffectKey::COUNT {
        let row = visible_row(&sheet, CLIENT, Focus::Effect(index));
        press_at(&mut sheet, CLIENT, slider_point(row, 0));
        sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage::sink());
        press_at(&mut sheet, CLIENT, slider_point(row, 1000));
        sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage::sink());
        press_at(&mut sheet, CLIENT, slider_point(row, 0));
        sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage::sink());
    }
    focus_on(&mut sheet, CLIENT, Focus::Tabs);
    key(&mut sheet, CLIENT, Key::Named(NamedKey::Left));
    key(&mut sheet, CLIENT, Key::Named(NamedKey::Enter));
    focus_on(&mut sheet, CLIENT, Focus::TextSize);
    key(&mut sheet, CLIENT, Key::Named(NamedKey::Home));

    let mut expected = *sheet.profile();
    expected.clamp();
    assert_eq!(
        *sheet.profile(),
        expected,
        "the sheet never leaves a profile unclamped"
    );
    assert!(sheet.profile().effects.opacity >= MIN_OPACITY);
    assert!(sheet.profile().font_size_px >= MIN_FONT_SIZE_PX);
}

// --- The footer ------------------------------------------------------------

#[test]
fn restore_defaults_asks_the_caller_rather_than_resetting_the_sheet() {
    // "Defaults" means the layers beneath the user's own document — the
    // machine's policy, the bundle's shipped defaults — and only the store
    // knows what those say. The sheet therefore reports the request and keeps
    // showing what it has until the caller hands back the profile that
    // actually applies.
    let mut sheet = sheet();
    let row = visible_row(&sheet, CLIENT, Focus::Scheme(1));
    click_at(&mut sheet, CLIENT, centre(row));
    focus_on(&mut sheet, CLIENT, Focus::Channel(0));
    key(&mut sheet, CLIENT, Key::Named(NamedKey::End));
    let edited = *sheet.profile();
    assert_ne!(edited, Profile::default(), "the profile really was edited");

    let (restore, _) = footer_buttons(&sheet, CLIENT);
    assert_eq!(
        click_at(&mut sheet, CLIENT, centre(restore)),
        SheetOutcome::Restore
    );
    assert_eq!(
        *sheet.profile(),
        edited,
        "the sheet does not guess at what the defaults are"
    );
}

#[test]
fn adopting_a_profile_rebuilds_every_control_to_match() {
    // What the caller does once the store has answered: the sheet is told the
    // profile that now applies, and its controls follow.
    let mut sheet = sheet();
    let row = visible_row(&sheet, CLIENT, Focus::Scheme(1));
    click_at(&mut sheet, CLIENT, centre(row));
    assert_ne!(*sheet.profile(), Profile::default());

    sheet.adopt(Profile::default());
    assert_eq!(*sheet.profile(), Profile::default());
    assert!(
        sheet.scheme_radios[0].is_selected(),
        "the controls follow the adopted profile"
    );
}

#[test]
fn the_done_button_dismisses() {
    let mut sheet = sheet();
    let (_, done) = footer_buttons(&sheet, CLIENT);
    assert_eq!(
        click_at(&mut sheet, CLIENT, centre(done)),
        SheetOutcome::Dismissed
    );
}

// --- Dismissal -------------------------------------------------------------

#[test]
fn escape_dismisses() {
    let mut sheet = sheet();
    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Escape)),
        SheetOutcome::Dismissed
    );
}

#[test]
fn escape_dismisses_even_when_the_viewport_cannot_draw_the_sheet() {
    let mut sheet = sheet();
    assert_eq!(
        key(
            &mut sheet,
            Rect::new(0, 0, 1, 1),
            Key::Named(NamedKey::Escape)
        ),
        SheetOutcome::Dismissed
    );
}

#[test]
fn a_press_outside_the_panel_dismisses() {
    let mut sheet = sheet();
    let bounds = panel_bounds(CLIENT, SCALE);
    let outside = Point::new(bounds.left() - 1, bounds.top() - 1);
    assert!(!bounds.contains(outside));
    assert_eq!(
        press_at(&mut sheet, CLIENT, outside),
        SheetOutcome::Dismissed
    );
}

#[test]
fn a_press_inside_the_panel_does_not_dismiss() {
    let mut sheet = sheet();
    let bounds = panel_bounds(CLIENT, SCALE);
    let outcome = press_at(&mut sheet, CLIENT, centre(bounds));
    assert_ne!(outcome, SheetOutcome::Dismissed);
}

// --- The keyboard-only path -------------------------------------------------

#[test]
fn the_keyboard_alone_reaches_and_changes_a_setting() {
    let mut sheet = sheet();
    let wanted = Scheme::ALL[2];
    assert_ne!(sheet.profile().scheme, wanted);

    focus_on(&mut sheet, CLIENT, Focus::Scheme(2));
    assert_eq!(
        key(&mut sheet, CLIENT, Key::Char(' ')),
        SheetOutcome::Edited
    );
    assert_eq!(sheet.profile().scheme, wanted);
}

#[test]
fn the_keyboard_reaches_every_row_of_the_active_tab() {
    let mut sheet = sheet();
    for row in sheet.content_rows() {
        focus_on(&mut sheet, CLIENT, row);
        assert_eq!(sheet.focus, row);
    }
    focus_on(&mut sheet, CLIENT, Focus::Restore);
    focus_on(&mut sheet, CLIENT, Focus::Done);
}

#[test]
fn shift_tab_walks_the_focus_order_backwards() {
    let mut sheet = sheet();
    let order = sheet.focus_order();
    let last = *order.last().expect("the sheet has focusable elements");
    assert_eq!(sheet.focus, Focus::Tabs, "focus opens on the tab strip");
    assert_eq!(
        shift_key(&mut sheet, CLIENT, Key::Named(NamedKey::Tab)),
        SheetOutcome::Changed
    );
    assert_eq!(sheet.focus, last);
}

#[test]
fn a_keyboard_only_session_reaches_a_row_the_body_cannot_show() {
    let mut sheet = sheet();
    let last = *sheet
        .content_rows()
        .last()
        .expect("the appearance tab has rows");
    focus_on(&mut sheet, TINY, last);
    assert_eq!(
        key(&mut sheet, TINY, Key::Named(NamedKey::End)),
        SheetOutcome::Edited,
        "a row unreachable by pointer on a tiny viewport is still editable"
    );
}

/// A pointer sample that redraws nothing must not ask the caller for a
/// repaint: the sheet is one plate in its own window, so a repaint re-renders
/// and re-publishes every pixel of it, and the pointer samples far faster than
/// the sheet changes.
#[test]
fn a_pointer_sample_that_redraws_nothing_asks_for_nothing() {
    let mut sheet = sheet();
    let row = row_rect(&sheet, CLIENT, Focus::Scheme(1)).expect("the first scheme row is shown");
    let at = Point::new(row.left() + to_i32(row.width / 2), row.top() + 1);

    let mut arriving = damage::sink();
    let first = sheet.on_pointer(&moved(at), CLIENT, SCALE, &theme(), &mut arriving);
    assert_eq!(
        first == SheetOutcome::Changed,
        !arriving.is_empty(),
        "a repaint was asked for exactly when something was reported"
    );

    // The same sample again, and one a pixel away inside the same row: the
    // sheet looks precisely as it did.
    for to in [at, Point::new(at.x + 1, at.y)] {
        let mut damage = damage::sink();
        assert_eq!(
            sheet.on_pointer(&moved(to), CLIENT, SCALE, &theme(), &mut damage),
            SheetOutcome::Ignored,
            "a sample that changed nothing asked for a repaint"
        );
        assert!(damage.is_empty(), "and it reported nothing either");
    }
}

/// The other half of the rule: a round that *did* report keeps its whole-plate
/// repaint, because switching tabs replaces the body while the strip is all
/// the tabs control reports.
#[test]
fn switching_tabs_still_asks_for_a_repaint() {
    let mut sheet = sheet();
    let mut damage = damage::sink();
    let before = sheet.tabs.selected();
    let (tabs, ..) = bands(&sheet, CLIENT);
    let strip = tabs.expect("the tab strip is laid out");
    let at = Point::new(
        strip.right() - to_i32(strip.width / 4),
        strip.top() + to_i32(strip.height / 2),
    );

    sheet.on_pointer(&moved(at), CLIENT, SCALE, &theme(), &mut damage);
    sheet.on_pointer(&PRESS, CLIENT, SCALE, &theme(), &mut damage);
    assert_eq!(
        sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage),
        SheetOutcome::Changed
    );
    assert_ne!(sheet.tabs.selected(), before, "the tab really changed");
    assert!(!damage.is_empty());
}

// --- Tabs and scrolling ------------------------------------------------------

#[test]
fn switching_tabs_replaces_the_body_rows() {
    let mut sheet = sheet();
    assert!(sheet.content_rows().contains(&Focus::TextSize));
    select_effects_tab(&mut sheet, CLIENT);
    let rows = sheet.content_rows();
    assert!(!rows.contains(&Focus::TextSize));
    assert_eq!(rows.len(), EffectKey::COUNT);
}

#[test]
fn scrolling_to_the_end_brings_the_last_row_into_the_body() {
    let mut sheet = sheet();
    let last = *sheet
        .content_rows()
        .last()
        .expect("the appearance tab has rows");
    focus_on(&mut sheet, CLIENT, Focus::Scroll);
    key(&mut sheet, CLIENT, Key::Named(NamedKey::End));
    assert!(
        row_rect(&sheet, CLIENT, last).is_some(),
        "the end of the body is reachable"
    );
}

#[test]
fn a_row_scrolled_out_of_the_body_is_not_hit_tested() {
    let mut sheet = sheet();
    let first = Focus::Scheme(0);
    assert!(row_rect(&sheet, CLIENT, first).is_some());
    focus_on(&mut sheet, CLIENT, Focus::Scroll);
    key(&mut sheet, CLIENT, Key::Named(NamedKey::End));
    let body = body(&sheet, CLIENT);
    let offset = sheet
        .scrolled_model(Some(body), SCALE, &theme(), font())
        .offset();
    if offset > 0 {
        assert!(
            row_rect(&sheet, CLIENT, first).is_none(),
            "a row scrolled past the top is not laid out"
        );
    }
}

#[test]
fn every_laid_out_row_lies_wholly_inside_the_body() {
    let sheet = sheet();
    let body = body(&sheet, CLIENT);
    let offset = sheet
        .scrolled_model(Some(body), SCALE, &theme(), font())
        .offset();
    let rows = sheet.laid_out_rows(body, offset, SCALE, &theme(), font());
    assert!(
        !rows.is_empty(),
        "the small-screen budget shows at least one row"
    );
    for (row, rect) in rows {
        assert!(rect.top() >= body.top(), "{row:?} starts inside the body");
        assert!(
            rect.bottom() <= body.bottom(),
            "{row:?} ends inside the body"
        );
    }
}
