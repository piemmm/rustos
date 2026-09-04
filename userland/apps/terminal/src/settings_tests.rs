//! Unit tests for the in-window settings sheet.
//!
//! Every geometric probe reads the sheet's *own* layout (`panel_bounds`,
//! `bands`, `scrolled_model`, `laid_out_rows`, `split_row`, `footer_split`)
//! rather than restating it, so a test can never assert against a rectangle
//! the sheet does not actually draw or hit-test.

use alloc::vec::Vec;

use tairix_controls::damage;
use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
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

/// How many of `outcomes` are `wanted`.
fn count(outcomes: &[SheetOutcome], wanted: SheetOutcome) -> usize {
    outcomes.iter().filter(|got| **got == wanted).count()
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
/// One key press with no modifiers, reporting into `damage`.
fn key_into(sheet: &mut Settings, viewport: Rect, key: Key, damage: &mut Region) -> SheetOutcome {
    sheet.on_key(key, Modifiers::default(), viewport, SCALE, &theme(), damage)
}

/// One key press with no modifiers, for a test that does not read the report.
fn key(sheet: &mut Settings, viewport: Rect, key: Key) -> SheetOutcome {
    key_into(sheet, viewport, key, &mut damage::sink())
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
        SheetOutcome::Settled,
        "a chosen radio is one whole interaction, so it settles"
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
        SheetOutcome::Settled
    );
    assert_eq!(sheet.profile().font_size_px, MAX_FONT_SIZE_PX);

    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Home)),
        SheetOutcome::Settled
    );
    assert_eq!(sheet.profile().font_size_px, MIN_FONT_SIZE_PX);

    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Right)),
        SheetOutcome::Settled
    );
    let stepped = sheet.profile().font_size_px;
    assert!(
        (MIN_FONT_SIZE_PX..=MAX_FONT_SIZE_PX).contains(&stepped) && stepped > MIN_FONT_SIZE_PX,
        "one line step moves the size up and stays in range, got {stepped}"
    );
}

/// The regression the live/settled split exists for: dragging a slider changes
/// the profile on every sample and asks to be **written** only when the drag
/// ends. Reporting a settled edit per sample cost one IPC round trip to the
/// configuration service and one disk commit per pointer motion, with the
/// window frozen for each of them.
#[test]
fn dragging_the_text_size_settles_once_however_many_samples_it_takes() {
    let mut sheet = sheet();
    let row = visible_row(&sheet, CLIENT, Focus::TextSize);
    let mut outcomes = alloc::vec::Vec::new();
    outcomes.push(press_at(&mut sheet, CLIENT, slider_point(row, 0)));
    for permille in [200, 400, 600, 800, 1000] {
        outcomes.push(sheet.on_pointer(
            &moved(slider_point(row, permille)),
            CLIENT,
            SCALE,
            &theme(),
            &mut damage::sink(),
        ));
    }
    assert!(
        count(&outcomes, SheetOutcome::Edited) > 1,
        "every sample of the drag is applied live"
    );
    assert_eq!(
        count(&outcomes, SheetOutcome::Settled),
        0,
        "nothing is written while the drag continues"
    );

    outcomes.push(sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage::sink()));
    assert_eq!(
        count(&outcomes, SheetOutcome::Settled),
        1,
        "the release settles exactly once"
    );
    assert_eq!(sheet.profile().font_size_px, MAX_FONT_SIZE_PX);
}

/// A press and release with no motion between them is a track click: the value
/// is applied and then settled, so a single click still saves.
#[test]
fn clicking_the_text_size_track_settles_the_value_it_jumped_to() {
    let mut sheet = sheet();
    let row = visible_row(&sheet, CLIENT, Focus::TextSize);
    let point = slider_point(row, 1000);
    assert_eq!(
        press_at(&mut sheet, CLIENT, point),
        SheetOutcome::Edited,
        "the press applies the value it jumped to"
    );
    assert_eq!(
        sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage::sink()),
        SheetOutcome::Settled
    );
    assert_eq!(sheet.profile().font_size_px, MAX_FONT_SIZE_PX);
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
        SheetOutcome::Settled
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
    sheet.swatches.adopt_selected(1);
    sheet.sync_channel_sliders();

    focus_on(&mut sheet, CLIENT, Focus::Channel(2));
    assert_eq!(
        key(&mut sheet, CLIENT, Key::Named(NamedKey::Home)),
        SheetOutcome::Settled
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

/// The other half of the reported slider freeze: what the transparency and
/// blur sliders *report* is a small part of the sheet, so the retained picture
/// ([`crate::sheet::SheetScreen`]) has something worth scoping a repaint to.
/// The sheet used to be re-rendered whole into a freshly allocated surface on
/// every sample of a drag, and the reports below were discarded.
#[test]
fn dragging_an_effect_slider_reports_a_small_part_of_the_sheet() {
    let mut sheet = sheet();
    select_effects_tab(&mut sheet, CLIENT);
    // Every effect slider, opacity and blur included: none of them may claim
    // the sheet.
    for index in 0..EffectKey::COUNT {
        let row = visible_row(&sheet, CLIENT, Focus::Effect(index));
        let mut damage = damage::sink();
        sheet.on_pointer(
            &moved(slider_point(row, 0)),
            CLIENT,
            SCALE,
            &theme(),
            &mut damage,
        );
        sheet.on_pointer(&PRESS, CLIENT, SCALE, &theme(), &mut damage);
        for permille in [200, 400, 600, 800, 1000] {
            sheet.on_pointer(
                &moved(slider_point(row, permille)),
                CLIENT,
                SCALE,
                &theme(),
                &mut damage,
            );
        }
        sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage);

        let reported = damage.bounds();
        assert!(
            !reported.is_empty(),
            "effect slider {index} must report what it changed"
        );
        let area = u64::from(reported.width) * u64::from(reported.height);
        let sheet_area = u64::from(CLIENT.width) * u64::from(CLIENT.height);
        assert!(
            area * 4 < sheet_area,
            "a whole drag of effect slider {index} reported {reported:?}, \
             which is not a small part of {CLIENT:?}"
        );
    }
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
        SheetOutcome::Settled
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
        SheetOutcome::Settled,
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

/// The other half of the rule: a round that *did* report says so, and a tab
/// switch reports the body it replaced.
///
/// The strip reports only the two plates whose selection changed, so a sheet
/// that painted just that left every row of the tab it came from standing —
/// the Appearance controls stayed on screen under the Effects tab until an
/// unrelated hover happened to redraw them.
#[test]
fn switching_tabs_reports_the_body_it_replaced() {
    let mut sheet = sheet();
    let mut damage = damage::sink();
    let before = sheet.tabs.selected();
    let (tabs, body, scrollbar, _) = bands(&sheet, CLIENT);
    let strip = tabs.expect("the tab strip is laid out");
    let body = body.expect("the body band is laid out");
    let scrollbar = scrollbar.expect("the scrollbar band is laid out");
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
    let reported = damage.bounds();
    assert_eq!(
        reported.intersection(&body),
        body,
        "every row the new tab draws must be repainted"
    );
    assert_eq!(
        reported.intersection(&scrollbar),
        scrollbar,
        "the bar is re-clamped against the new tab's extent"
    );
}

/// The same rule from the keyboard, which reaches the strip with no rectangle
/// of its own to hit-test against.
#[test]
fn switching_tabs_by_key_reports_the_body_it_replaced() {
    let mut sheet = sheet();
    let body = body(&sheet, CLIENT);
    focus_on(&mut sheet, CLIENT, Focus::Tabs);

    let mut damage = damage::sink();
    sheet.on_key(
        Key::Named(NamedKey::Right),
        Modifiers::default(),
        CLIENT,
        SCALE,
        &theme(),
        &mut damage,
    );
    sheet.on_key(
        Key::Named(NamedKey::Enter),
        Modifiers::default(),
        CLIENT,
        SCALE,
        &theme(),
        &mut damage,
    );
    assert_eq!(sheet.tabs.selected(), Some(EFFECTS_TAB));
    assert_eq!(damage.bounds().intersection(&body), body);
}

/// A press moves the drawn focus ring, not just the field the keyboard reads.
///
/// Nothing synced the ring on this path, so clicking a row left it drawn on
/// whatever held focus before while every key went to the row just clicked.
#[test]
fn a_pressed_row_takes_the_focus_ring() {
    let mut sheet = sheet();
    let row = row_rect(&sheet, CLIENT, Focus::Scheme(1)).expect("the second scheme row is seated");
    let at = Point::new(
        row.left() + to_i32(row.width / 4),
        row.top() + to_i32(row.height / 2),
    );

    let mut damage = damage::sink();
    sheet.on_pointer(&moved(at), CLIENT, SCALE, &theme(), &mut damage);
    sheet.on_pointer(&PRESS, CLIENT, SCALE, &theme(), &mut damage);
    sheet.on_pointer(&RELEASE, CLIENT, SCALE, &theme(), &mut damage);

    assert_eq!(sheet.focus, Focus::Scheme(1));
    assert!(
        sheet.scheme_radios[1].state().focus.focused,
        "the row the keyboard now edits is the row drawing the ring"
    );
    assert!(
        !sheet.scheme_radios[0].state().focus.focused,
        "and it is the only one"
    );
    assert_eq!(
        damage.bounds().intersection(&row),
        row,
        "the ring it arrived on is redrawn"
    );
}

/// A value the sheet writes back into a control is drawn twice — as the
/// control's own state and as the label beside it — so the whole row is the
/// scope, not the control's rectangle.
///
/// Focus is moved before the report is measured, because a focus arrival
/// reports the row too and would mask the missing report.
#[test]
fn a_keyed_edit_reports_the_label_beside_the_control() {
    let mut sheet = sheet();
    focus_on(&mut sheet, CLIENT, Focus::TextSize);
    let row = row_rect(&sheet, CLIENT, Focus::TextSize).expect("the text-size row is seated");

    let mut damage = damage::sink();
    assert_eq!(
        key_into(&mut sheet, CLIENT, Key::Named(NamedKey::Home), &mut damage),
        SheetOutcome::Settled
    );
    assert_eq!(sheet.profile().font_size_px, MIN_FONT_SIZE_PX);
    assert_eq!(
        damage.bounds().intersection(&row),
        row,
        "the label spells the value out, so it is redrawn with the knob"
    );
}

/// The same rule for the effects tab, whose labels carry a percentage.
#[test]
fn a_keyed_effect_edit_reports_its_label() {
    let mut sheet = sheet();
    select_effects_tab(&mut sheet, CLIENT);
    focus_on(&mut sheet, CLIENT, Focus::Effect(0));
    let row = row_rect(&sheet, CLIENT, Focus::Effect(0)).expect("the first effect row is seated");

    let mut damage = damage::sink();
    key_into(&mut sheet, CLIENT, Key::Named(NamedKey::Home), &mut damage);
    assert_eq!(damage.bounds().intersection(&row), row);
}

/// Choosing another well re-points all three channel sliders, which is the
/// sheet's own write into controls it did not touch.
#[test]
fn selecting_a_well_reports_the_channel_rows_it_repoints() {
    let mut sheet = sheet();
    // The channel rows sit below the swatch grid, so the body is scrolled to
    // its end to seat them before anything is asserted about their pixels.
    focus_on(&mut sheet, CLIENT, Focus::Scroll);
    key(&mut sheet, CLIENT, Key::Named(NamedKey::End));
    focus_on(&mut sheet, CLIENT, Focus::Swatches);
    let seated: Vec<Rect> = (0..3)
        .filter_map(|index| row_rect(&sheet, CLIENT, Focus::Channel(index)))
        .collect();
    assert!(!seated.is_empty(), "at least one channel row is on screen");

    let mut damage = damage::sink();
    key_into(&mut sheet, CLIENT, Key::Named(NamedKey::Right), &mut damage);
    let reported = damage.bounds();
    for row in seated {
        assert_eq!(
            reported.intersection(&row),
            row,
            "a slider now showing another well's channel is redrawn"
        );
    }
}

/// Scrolling moves every row, and the bar reports only its own thumb.
#[test]
fn scrolling_reports_the_body_whose_rows_moved() {
    let mut sheet = sheet();
    let body = body(&sheet, CLIENT);
    focus_on(&mut sheet, CLIENT, Focus::Scroll);
    let before = sheet
        .scrolled_model(Some(body), SCALE, &theme(), font())
        .offset();

    let mut damage = damage::sink();
    key_into(&mut sheet, CLIENT, Key::Named(NamedKey::End), &mut damage);
    assert_ne!(
        sheet
            .scrolled_model(Some(body), SCALE, &theme(), font())
            .offset(),
        before,
        "the body really scrolled"
    );
    assert_eq!(damage.bounds().intersection(&body), body);
}

/// Choosing a scheme from the keyboard moves the dot between two radios, and
/// neither the radio nor the key path has a rectangle of its own to report.
#[test]
fn a_keyed_scheme_choice_reports_both_dots() {
    let mut sheet = sheet();
    let (lit, custom) = (scheme_row(&sheet), custom_scheme_row());
    assert_ne!(lit, custom, "the custom scheme is not the one lit");

    focus_on(&mut sheet, CLIENT, Focus::Scheme(custom));
    let leaving = row_rect(&sheet, CLIENT, Focus::Scheme(lit)).expect("the lit row is seated");
    let arriving =
        row_rect(&sheet, CLIENT, Focus::Scheme(custom)).expect("the custom row is seated");

    let mut damage = damage::sink();
    assert_eq!(
        key_into(&mut sheet, CLIENT, Key::Char(' '), &mut damage),
        SheetOutcome::Settled
    );
    assert_eq!(sheet.profile().scheme, Scheme::Custom);

    let reported = damage.bounds();
    assert_eq!(
        reported.intersection(&leaving),
        leaving,
        "the dot that emptied must be redrawn"
    );
    assert_eq!(
        reported.intersection(&arriving),
        arriving,
        "the dot that filled must be redrawn"
    );
}

/// The custom editor's caption reads off the same field the radios do, so a
/// scheme choice redraws it too.
#[test]
fn a_keyed_scheme_choice_reports_the_editor_caption() {
    let mut sheet = sheet();
    // The editor sits below the radios, so the body is scrolled to seat it;
    // Tab traversal does not scroll, so the radio stays reachable.
    focus_on(&mut sheet, CLIENT, Focus::Scroll);
    key(&mut sheet, CLIENT, Key::Named(NamedKey::End));
    let caption = row_rect(&sheet, CLIENT, Focus::Swatches).expect("the editor row is seated");
    focus_on(&mut sheet, CLIENT, Focus::Scheme(custom_scheme_row()));

    let mut damage = damage::sink();
    key_into(&mut sheet, CLIENT, Key::Char(' '), &mut damage);
    assert_eq!(sheet.profile().scheme, Scheme::Custom);
    assert_eq!(damage.bounds().intersection(&caption), caption);
}

/// The row index of the scheme the sheet's profile currently names.
fn scheme_row(sheet: &Settings) -> usize {
    Scheme::ALL
        .iter()
        .position(|scheme| *scheme == sheet.profile().scheme)
        .expect("some scheme is lit")
}

/// The row index of the custom scheme.
fn custom_scheme_row() -> usize {
    Scheme::ALL
        .iter()
        .position(|scheme| *scheme == Scheme::Custom)
        .expect("the custom scheme is offered")
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
