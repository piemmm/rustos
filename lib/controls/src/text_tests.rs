//! Unit tests for the text-entry family (spec §20 checklist).
//!
//! These cover the editor (insert/backspace/delete, caret navigation and
//! selection, character limit, Ctrl+A), the pointer caret placement and
//! selection drag, the read-only / denied / disabled distinction, the
//! validation rim and inline message, dark/light and high-contrast coverage,
//! scale, and the search field's magnifier chrome, query-active tint, and
//! Escape-clear behaviour.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Contrast, Rgba, Theme};

use crate::state::{AuthorityState, ControlState, ValidationState};
use crate::text::{SearchField, TextAction, TextField};

const W: u32 = 200;
const H: u32 = 28;

fn font() -> BitmapFont {
    BitmapFont::inconsolata()
}

fn premul(rgba: Rgba) -> Pixel {
    Color::from(rgba).premultiply()
}

fn bounds() -> Rect {
    Rect::new(0, 0, W, H)
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

const NONE_MODS: Modifiers = Modifiers {
    shift: false,
    ctrl: false,
    alt: false,
    meta: false,
};
const SHIFT: Modifiers = Modifiers {
    shift: true,
    ctrl: false,
    alt: false,
    meta: false,
};
const CTRL: Modifiers = Modifiers {
    shift: false,
    ctrl: true,
    alt: false,
    meta: false,
};

fn field_surface(field: &TextField, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    field.render(&mut surface, bounds(), Scale::ONE, theme, font());
    surface
}

fn search_surface(field: &SearchField, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    field.render(&mut surface, bounds(), Scale::ONE, theme, font());
    surface
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
}

/// A theme identical to [`Theme::dark`] but with [`Contrast::High`].
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

/// Type a string into a focused, editable field a character at a time.
fn type_str(field: &mut TextField, text: &str) {
    for ch in text.chars() {
        field.on_key(Key::Char(ch), NONE_MODS);
    }
}

// --- Editing -----------------------------------------------------------------

#[test]
fn typing_inserts_and_reports_edits() {
    let mut field = TextField::new();
    field.set_focused(true);
    assert_eq!(
        field.on_key(Key::Char('h'), NONE_MODS),
        Some(TextAction::Edited)
    );
    type_str(&mut field, "i!");
    assert_eq!(field.text(), "hi!");
}

#[test]
fn backspace_and_delete_remove_characters() {
    let mut field = TextField::new().with_text("hello");
    field.set_focused(true);
    // Caret is at the end after with_text; backspace removes 'o'.
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Backspace), NONE_MODS),
        Some(TextAction::Edited)
    );
    assert_eq!(field.text(), "hell");
    // Home then forward-delete removes 'h'.
    field.on_key(Key::Named(NamedKey::Home), NONE_MODS);
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Delete), NONE_MODS),
        Some(TextAction::Edited)
    );
    assert_eq!(field.text(), "ell");
}

#[test]
fn backspace_at_start_and_delete_at_end_do_nothing() {
    let mut field = TextField::new().with_text("x");
    field.set_focused(true);
    field.on_key(Key::Named(NamedKey::Home), NONE_MODS);
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Backspace), NONE_MODS),
        None
    );
    field.on_key(Key::Named(NamedKey::End), NONE_MODS);
    assert_eq!(field.on_key(Key::Named(NamedKey::Delete), NONE_MODS), None);
    assert_eq!(field.text(), "x");
}

#[test]
fn caret_moves_and_inserts_in_the_middle() {
    let mut field = TextField::new().with_text("ac");
    field.set_focused(true);
    field.on_key(Key::Named(NamedKey::Left), NONE_MODS);
    type_str(&mut field, "b");
    assert_eq!(field.text(), "abc");
}

#[test]
fn selection_then_typing_replaces() {
    let mut field = TextField::new().with_text("abc");
    field.set_focused(true);
    // Select the whole buffer, then type replaces it.
    assert_eq!(field.on_key(Key::Char('a'), CTRL), None);
    type_str(&mut field, "Z");
    assert_eq!(field.text(), "Z");
}

#[test]
fn shift_arrow_selects_and_backspace_deletes_selection() {
    let mut field = TextField::new().with_text("abcd");
    field.set_focused(true);
    // Select the last two characters with Shift+Left twice.
    field.on_key(Key::Named(NamedKey::Left), SHIFT);
    field.on_key(Key::Named(NamedKey::Left), SHIFT);
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Backspace), NONE_MODS),
        Some(TextAction::Edited)
    );
    assert_eq!(field.text(), "ab");
}

#[test]
fn character_limit_is_enforced() {
    let mut field = TextField::new().with_max_len(3);
    field.set_focused(true);
    type_str(&mut field, "abcdef");
    assert_eq!(field.text(), "abc");
}

#[test]
fn with_text_truncates_to_limit() {
    let field = TextField::new().with_max_len(2).with_text("abcd");
    assert_eq!(field.text(), "ab");
}

#[test]
fn multibyte_editing_stays_on_boundaries() {
    let mut field = TextField::new().with_text("café");
    field.set_focused(true);
    // Backspace removes the 'é' (a two-byte scalar) cleanly.
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Backspace), NONE_MODS),
        Some(TextAction::Edited)
    );
    assert_eq!(field.text(), "caf");
}

#[test]
fn enter_submits_and_escape_cancels() {
    let mut field = TextField::new().with_text("hi");
    field.set_focused(true);
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Enter), NONE_MODS),
        Some(TextAction::Submitted)
    );
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Escape), NONE_MODS),
        Some(TextAction::Cancelled)
    );
    // A plain text field's Escape does not clear its text.
    assert_eq!(field.text(), "hi");
}

// --- Authority / read-only / disabled ---------------------------------------

#[test]
fn disabled_field_ignores_input() {
    let mut field = TextField::new();
    field.set_state(ControlState::disabled());
    field.set_focused(true);
    assert_eq!(field.on_key(Key::Char('x'), NONE_MODS), None);
    assert_eq!(field.text(), "");
}

#[test]
fn denied_field_keeps_value_and_ignores_edits() {
    let mut field = TextField::new().with_text("secret");
    field.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    field.set_focused(true);
    assert_eq!(field.on_key(Key::Char('x'), NONE_MODS), None);
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Backspace), NONE_MODS),
        None
    );
    assert_eq!(field.text(), "secret");
}

#[test]
fn read_only_field_navigates_but_refuses_edits() {
    let mut field = TextField::new().with_text("value").read_only(true);
    field.set_focused(true);
    assert_eq!(field.on_key(Key::Char('x'), NONE_MODS), None);
    assert_eq!(
        field.on_key(Key::Named(NamedKey::Backspace), NONE_MODS),
        None
    );
    // Navigation still works (no action, but no panic and no change).
    assert_eq!(field.on_key(Key::Named(NamedKey::Home), NONE_MODS), None);
    assert_eq!(field.text(), "value");
    assert!(field.is_read_only());
}

#[test]
fn denied_field_draws_lock_bead() {
    let theme = Theme::dark();
    let mut field = TextField::new().with_text("x");
    field.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    let surface = field_surface(&field, &theme);
    assert!(
        has_pixel(&surface, premul(theme.palette().denied)),
        "a denied field shows the denied Authority Mark"
    );
}

#[test]
fn read_only_reads_differently_from_disabled() {
    let theme = Theme::dark();
    let ro = TextField::new().with_text("x").read_only(true);
    let mut disabled = TextField::new().with_text("x");
    disabled.set_state(ControlState::disabled());
    // The read-only plate is the recessed surface; the disabled plate is too,
    // but the read-only text stays full-contrast while the disabled text is
    // muted, so the two are distinguishable.
    let ro_surface = field_surface(&ro, &theme);
    let dis_surface = field_surface(&disabled, &theme);
    assert!(has_pixel(&ro_surface, premul(theme.palette().on_surface)));
    assert!(has_pixel(
        &dis_surface,
        premul(theme.palette().on_surface_muted)
    ));
}

// --- Validation --------------------------------------------------------------

#[test]
fn invalid_field_shows_danger_rim() {
    let theme = Theme::dark();
    let mut field = TextField::new().with_text("bad");
    field.set_state(ControlState::idle().with_validation(ValidationState::Invalid));
    let surface = field_surface(&field, &theme);
    assert!(
        has_pixel(&surface, premul(theme.palette().danger)),
        "an invalid field shows a danger rim segment"
    );
}

#[test]
fn warning_field_shows_warning_rim() {
    let theme = Theme::dark();
    let mut field = TextField::new().with_text("meh");
    field.set_state(ControlState::idle().with_validation(ValidationState::Warning));
    let surface = field_surface(&field, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().warning)));
}

#[test]
fn inline_message_is_drawn_below_when_there_is_room() {
    let theme = Theme::dark();
    // A tall bounds leaves room for the message row under the field row.
    let mut surface = Surface::new(W, 80).expect("surface");
    let mut field = TextField::new().with_text("x").with_message("required");
    field.set_state(ControlState::idle().with_validation(ValidationState::Invalid));
    field.render(
        &mut surface,
        Rect::new(0, 0, W, 80),
        Scale::ONE,
        &theme,
        font(),
    );
    // The message row (below the standard control height) is painted danger.
    let control_h = Scale::ONE.scale_length(theme.metrics().control_height);
    let mut found = false;
    for y in control_h..80 {
        for x in 0..W {
            if surface.get(x, y) == Some(premul(theme.palette().danger)) {
                found = true;
            }
        }
    }
    assert!(
        found,
        "the inline validation message is drawn below the field"
    );
}

// --- Pointer -----------------------------------------------------------------

#[test]
fn click_focuses_caret_and_typing_inserts_there() {
    let theme = Theme::dark();
    let mut field = TextField::new().with_text("aaaa");
    field.set_focused(true);
    // Click near the far left to place the caret at the start.
    field.on_pointer(&moved(1, 14), bounds(), Scale::ONE, &theme, font());
    field.on_pointer(&PRESS, bounds(), Scale::ONE, &theme, font());
    field.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme, font());
    type_str(&mut field, "Z");
    assert!(
        field.text().starts_with('Z'),
        "clicking at the start places the caret there: {}",
        field.text()
    );
}

#[test]
fn drag_selects_a_range_then_typing_replaces_it() {
    let theme = Theme::dark();
    let mut field = TextField::new().with_text("abcdef");
    field.set_focused(true);
    let advance = font().advance();
    // Press at the start, drag several cells right, release: selects a run.
    field.on_pointer(&moved(1, 14), bounds(), Scale::ONE, &theme, font());
    field.on_pointer(&PRESS, bounds(), Scale::ONE, &theme, font());
    let far = 3 * i32::try_from(advance).unwrap() + 2;
    field.on_pointer(&moved(far, 14), bounds(), Scale::ONE, &theme, font());
    field.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme, font());
    type_str(&mut field, "Z");
    assert!(
        field.text().starts_with('Z') && field.text().ends_with("def"),
        "a drag-selection is replaced by typing: {}",
        field.text()
    );
}

// --- Theme / scale -----------------------------------------------------------

#[test]
fn renders_in_dark_and_light_without_panic() {
    for theme in [Theme::dark(), Theme::light()] {
        let mut field = TextField::new().with_text("hello");
        field.set_focused(true);
        let surface = field_surface(&field, &theme);
        assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
    }
}

#[test]
fn focused_field_draws_focus_ring() {
    let theme = Theme::dark();
    let mut focused = TextField::new();
    focused.set_focused(true);
    let surface = field_surface(&focused, &theme);
    assert!(
        has_pixel(&surface, premul(theme.palette().rim_active)),
        "a focused field draws the active focus ring"
    );
}

#[test]
fn high_contrast_thickens_the_rim() {
    let normal = Theme::dark();
    let heavy = high_contrast();
    let field = TextField::new().with_text("x");
    let normal_rim = count_color(
        &field_surface(&field, &normal),
        premul(normal.palette().rim),
    );
    let heavy_rim = count_color(&field_surface(&field, &heavy), premul(heavy.palette().rim));
    assert!(
        heavy_rim > normal_rim,
        "high contrast draws a thicker rim ({heavy_rim} vs {normal_rim})"
    );
}

fn count_color(surface: &Surface, want: Pixel) -> usize {
    surface.pixels().iter().filter(|&&p| p == want).count()
}

#[test]
fn renders_at_double_scale_without_panic() {
    let theme = Theme::dark();
    let mut surface = Surface::new(W * 2, H * 2).expect("surface");
    let mut field = TextField::new().with_text("scaled");
    field.set_focused(true);
    field.render(
        &mut surface,
        Rect::new(0, 0, W * 2, H * 2),
        Scale::from_percent(200).expect("scale"),
        &theme,
        font(),
    );
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn degenerate_bounds_do_not_panic() {
    let theme = Theme::dark();
    let mut surface = Surface::new(4, 4).expect("surface");
    let field = TextField::new().with_text("too big for me");
    field.render(
        &mut surface,
        Rect::new(0, 0, 4, 4),
        Scale::ONE,
        &theme,
        font(),
    );
    // No assertion beyond "did not panic".
}

// --- SearchField -------------------------------------------------------------

#[test]
fn search_escape_clears_query_then_cancels() {
    let mut search = SearchField::new().with_text("query");
    search.set_focused(true);
    // First Escape clears the non-empty query and reports an edit.
    assert_eq!(
        search.on_key(Key::Named(NamedKey::Escape), NONE_MODS),
        Some(TextAction::Edited)
    );
    assert_eq!(search.text(), "");
    assert!(!search.has_query());
    // A second Escape (now empty) cancels.
    assert_eq!(
        search.on_key(Key::Named(NamedKey::Escape), NONE_MODS),
        Some(TextAction::Cancelled)
    );
}

#[test]
fn search_typing_builds_a_query() {
    let mut search = SearchField::new();
    search.set_focused(true);
    for ch in "abc".chars() {
        search.on_key(Key::Char(ch), NONE_MODS);
    }
    assert_eq!(search.text(), "abc");
    assert!(search.has_query());
}

#[test]
fn search_magnifier_is_accent_when_query_present() {
    let theme = Theme::dark();
    let empty = SearchField::new();
    let active = SearchField::new().with_text("q");
    let empty_surface = search_surface(&empty, &theme);
    let active_surface = search_surface(&active, &theme);
    // The active search glyph is drawn in the accent colour; the empty one is
    // not (it is drawn muted).
    let accent = premul(theme.palette().accent);
    assert!(
        !has_pixel(&empty_surface, accent),
        "an empty search field's magnifier is quiet"
    );
    assert!(
        has_pixel(&active_surface, accent),
        "an active search field's magnifier reads as accent"
    );
}

#[test]
fn search_click_places_caret_after_the_magnifier() {
    let theme = Theme::dark();
    let mut search = SearchField::new().with_text("aaaa");
    search.set_focused(true);
    // A click well to the right lands somewhere in the text without panic.
    search.on_pointer(&moved(80, 14), bounds(), Scale::ONE, &theme, font());
    search.on_pointer(&PRESS, bounds(), Scale::ONE, &theme, font());
    search.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme, font());
    // Typing at that caret keeps the buffer well-formed.
    search.on_key(Key::Char('Z'), NONE_MODS);
    assert!(search.text().contains('Z'));
}

#[test]
fn search_renders_in_light_without_panic() {
    let theme = Theme::light();
    let search = SearchField::new().with_text("find");
    let surface = search_surface(&search, &theme);
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}
