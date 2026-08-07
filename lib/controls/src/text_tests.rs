//! Unit tests for the text-entry family (spec §20 checklist).
//!
//! These cover the editor (insert/backspace/delete, caret navigation and
//! selection, character limit, Ctrl+A), the pointer caret placement and
//! selection drag, the read-only / denied / disabled distinction, the
//! validation rim and inline message, dark/light and high-contrast coverage,
//! scale, and the search field's magnifier chrome, query-active tint, and
//! Escape-clear behaviour.
//!
//! The masked (secret) mode has its own section: that it draws one bead per
//! character and never the characters themselves, that its pointer hit test
//! lands on cell boundaries, that it edits exactly like a plain field, and
//! the credential hygiene it promises — a buffer that never reallocates
//! while it fills, an erase that leaves no plaintext behind, and a debug
//! dump that reports a length instead of a password.

use alloc::format;
use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Rgba, Theme};

use crate::state::{AuthorityState, ControlState, ValidationState};
use crate::testkit::{control_font, high_contrast};
use crate::text::{
    debug_buffer_identity, debug_bytes, debug_secret_cell_layout, debug_zeroize, zeroize_range,
    SearchField, TextAction, TextField,
};

const W: u32 = 200;
const H: u32 = 28;

fn font() -> BitmapFont {
    control_font(&Theme::dark(), Scale::ONE)
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
    field.render(&mut surface, bounds(), Scale::ONE, theme);
    surface
}

fn search_surface(field: &SearchField, theme: &Theme) -> Surface {
    let mut surface = Surface::new(W, H).expect("surface");
    field.render(&mut surface, bounds(), Scale::ONE, theme);
    surface
}

fn has_pixel(surface: &Surface, want: Pixel) -> bool {
    surface.pixels().contains(&want)
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
    field.render(&mut surface, Rect::new(0, 0, W, 80), Scale::ONE, &theme);
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
    field.on_pointer(&moved(1, 14), bounds(), Scale::ONE, &theme);
    field.on_pointer(&PRESS, bounds(), Scale::ONE, &theme);
    field.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme);
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
    let advance = font().cell_width();
    // Press at the start, drag several cells right, release: selects a run.
    field.on_pointer(&moved(1, 14), bounds(), Scale::ONE, &theme);
    field.on_pointer(&PRESS, bounds(), Scale::ONE, &theme);
    let far = 3 * i32::try_from(advance).unwrap() + 2;
    field.on_pointer(&moved(far, 14), bounds(), Scale::ONE, &theme);
    field.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme);
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
    );
    assert!(has_pixel(&surface, premul(theme.palette().on_surface)));
}

#[test]
fn degenerate_bounds_do_not_panic() {
    let theme = Theme::dark();
    let mut surface = Surface::new(4, 4).expect("surface");
    let field = TextField::new().with_text("too big for me");
    field.render(&mut surface, Rect::new(0, 0, 4, 4), Scale::ONE, &theme);
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
    search.on_pointer(&moved(80, 14), bounds(), Scale::ONE, &theme);
    search.on_pointer(&PRESS, bounds(), Scale::ONE, &theme);
    search.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme);
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

// --- Secret (masked) mode ----------------------------------------------------

/// A theme identical to [`Theme::dark`] but with reduced motion requested.
fn reduced_motion() -> Theme {
    let base = Theme::dark();
    Theme::new(
        base.id(),
        "Test Reduced Motion",
        base.appearance(),
        *base.palette(),
        *base.metrics(),
        *base.fonts(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        base.contrast(),
    )
}

/// The number of separate horizontal runs of `want` on row `y` — one per mark
/// drawn there, so a row through the bead centres counts the beads.
fn row_runs(surface: &Surface, y: u32, want: Pixel) -> usize {
    let mut runs = 0;
    let mut inside = false;
    for x in 0..W {
        let hit = surface.get(x, y) == Some(want);
        if hit && !inside {
            runs += 1;
        }
        inside = hit;
    }
    runs
}

/// The most marks any single row of `surface` holds: the bead count of a
/// masked field, read off whichever row runs through the beads' centres
/// without the test having to know which row that is.
fn max_row_runs(surface: &Surface, want: Pixel) -> usize {
    (0..H)
        .map(|y| row_runs(surface, y, want))
        .max()
        .unwrap_or(0)
}

/// The bead cell layout (first cell's surface x, per-cell advance) a masked
/// field of the standard test bounds draws with.
fn cell_layout(theme: &Theme) -> (u32, u32) {
    debug_secret_cell_layout(bounds(), Scale::ONE, theme).expect("cell layout")
}

#[test]
fn secret_mode_reports_itself_and_bounds_the_buffer() {
    let mut field = TextField::new().secret(4);
    assert!(field.is_secret());
    assert!(!TextField::new().is_secret(), "a plain field is not masked");
    field.set_focused(true);
    type_str(&mut field, "abcdef");
    assert_eq!(
        field.text(),
        "abcd",
        "typing past the bound inserts nothing"
    );
    // The bound also holds against a wholesale replacement.
    field.set_text("zyxwvu");
    assert_eq!(field.text(), "zyxw");
}

#[test]
fn filling_a_secret_field_to_its_limit_never_reallocates() {
    const LIMIT: usize = 16;
    let mut field = TextField::new().secret(LIMIT);
    field.set_focused(true);
    let (before_ptr, before_cap) = debug_buffer_identity(&field);
    assert!(
        before_cap >= LIMIT * 4,
        "the bound reserves the worst case UTF-8 needs: {before_cap}"
    );
    // Fill with the widest scalar UTF-8 can encode, so the buffer reaches the
    // worst case its reservation was sized for. A growth here would leave a
    // copy of everything typed so far in the block it moved out of.
    for _ in 0..LIMIT {
        field.on_key(Key::Char('😀'), NONE_MODS);
    }
    assert_eq!(field.text().chars().count(), LIMIT);
    let (after_ptr, after_cap) = debug_buffer_identity(&field);
    assert_eq!(before_ptr, after_ptr, "the buffer never moved");
    assert_eq!(before_cap, after_cap, "…and never grew");
}

#[test]
fn zeroize_range_overwrites_its_bytes_without_changing_the_length() {
    let mut text = String::from("abcdef");
    zeroize_range(&mut text, 2..4);
    assert_eq!(text.as_bytes(), &b"ab\0\0ef"[..]);
    assert_eq!(text.len(), 6, "the erase writes in place");
}

#[test]
fn dropping_a_filled_secret_field_erases_its_buffer() {
    let mut field = TextField::new().secret(12);
    field.set_text("hunter2");
    assert_eq!(debug_bytes(&field).as_slice(), &b"hunter2"[..]);
    // Dropping the field runs exactly this erase on its way out. A released
    // allocation cannot be read back in a crate that forbids `unsafe`, so the
    // erase is asserted here on the live buffer, through the very method the
    // drop calls, and the field is then dropped for real.
    debug_zeroize(&mut field);
    let erased = debug_bytes(&field);
    assert_eq!(erased.len(), 7, "the erase leaves the length alone");
    assert!(erased.iter().all(|&b| b == 0), "…and no byte survives it");
    drop(field);
}

#[test]
fn replacing_a_secret_erases_the_one_it_replaces() {
    let mut field = TextField::new().secret(12);
    field.set_text("hunter2");
    field.set_text("pw");
    let bytes = debug_bytes(&field);
    assert_eq!(bytes.as_slice(), &b"pw"[..]);
    assert!(
        !field.text().contains("hunter"),
        "the replaced credential is gone, not merely hidden behind a shorter length"
    );
}

#[test]
fn a_secret_fields_debug_output_redacts_its_buffer() {
    let field = TextField::new().secret(16).with_text("hunter2");
    let dump = format!("{field:?}");
    assert!(
        !dump.contains("hunter2"),
        "a debug dump must not carry the credential: {dump}"
    );
    assert!(
        dump.contains("7 chars"),
        "…it reports the length instead: {dump}"
    );
}

#[test]
fn a_plain_fields_debug_output_still_shows_its_text() {
    let field = TextField::new().with_text("hunter2");
    assert!(format!("{field:?}").contains("hunter2"));
}

#[test]
fn a_secret_field_draws_exactly_one_bead_per_character() {
    const SAMPLE: &str = "abcde";
    let theme = Theme::dark();
    for count in 0..=SAMPLE.chars().count() {
        let field = TextField::new().secret(8).with_text(&SAMPLE[..count]);
        let surface = field_surface(&field, &theme);
        assert_eq!(
            max_row_runs(&surface, premul(theme.palette().on_surface)),
            count,
            "a {count}-character secret draws {count} beads"
        );
    }
}

#[test]
fn a_secret_fields_render_never_depends_on_which_characters_it_holds() {
    let theme = Theme::dark();
    let narrow = field_surface(&TextField::new().secret(8).with_text("iiii"), &theme);
    let wide = field_surface(&TextField::new().secret(8).with_text("WWWW"), &theme);
    let multibyte = field_surface(&TextField::new().secret(8).with_text("😀😀😀😀"), &theme);
    assert_eq!(
        narrow.pixels(),
        wide.pixels(),
        "same length, same pixels — the drawn run cannot report glyph widths"
    );
    assert_eq!(
        narrow.pixels(),
        multibyte.pixels(),
        "…and it counts characters, not bytes"
    );
}

#[test]
fn a_secret_field_never_draws_the_glyphs_a_plain_one_would() {
    let theme = Theme::dark();
    let secret = field_surface(&TextField::new().secret(8).with_text("WWWW"), &theme);
    let plain = field_surface(&TextField::new().with_text("WWWW"), &theme);
    assert_ne!(
        secret.pixels(),
        plain.pixels(),
        "a masked field shows beads where a plain one shows its content"
    );
}

#[test]
fn an_empty_secret_field_still_shows_its_placeholder() {
    let theme = Theme::dark();
    let muted = premul(theme.palette().on_surface_muted);
    let empty = TextField::new().secret(8).with_placeholder("Password");
    let filled = TextField::new()
        .secret(8)
        .with_placeholder("Password")
        .with_text("pw");
    assert!(
        has_pixel(&field_surface(&empty, &theme), muted),
        "a placeholder is not a secret"
    );
    assert!(
        !has_pixel(&field_surface(&filled, &theme), muted),
        "…and it gives way once there is something to hide"
    );
}

#[test]
fn a_secret_fields_caret_stands_between_bead_cells() {
    let theme = Theme::dark();
    let (text_x0, advance) = cell_layout(&theme);
    let caret = premul(theme.palette().on_surface);
    let mut field = TextField::new().secret(8);
    field.set_focused(true);
    type_str(&mut field, "abc");
    // The caret spans the whole row while a bead only covers its middle, so
    // the field's top row shows the caret alone.
    assert_eq!(
        field_surface(&field, &theme).get(text_x0 + 3 * advance, 0),
        Some(caret),
        "typing three characters leaves the caret in the fourth cell"
    );
    field.on_key(Key::Named(NamedKey::Home), NONE_MODS);
    assert_eq!(
        field_surface(&field, &theme).get(text_x0, 0),
        Some(caret),
        "Home returns it to the first cell"
    );
    field.on_key(Key::Named(NamedKey::End), NONE_MODS);
    assert_eq!(
        field_surface(&field, &theme).get(text_x0 + 3 * advance, 0),
        Some(caret),
        "End returns it to the last"
    );
}

#[test]
fn a_secret_fields_selection_covers_whole_bead_cells() {
    let theme = Theme::dark();
    let (text_x0, advance) = cell_layout(&theme);
    let accent = premul(theme.palette().accent);
    let mut field = TextField::new().secret(8).with_text("abcd");
    field.set_focused(true);
    field.on_key(Key::Named(NamedKey::Left), SHIFT);
    field.on_key(Key::Named(NamedKey::Left), SHIFT);
    let surface = field_surface(&field, &theme);
    let first = (0..W).find(|&x| surface.get(x, 0) == Some(accent));
    let width = (0..W)
        .filter(|&x| surface.get(x, 0) == Some(accent))
        .count();
    assert_eq!(
        first,
        Some(text_x0 + 2 * advance),
        "the highlight starts on the third cell's boundary"
    );
    assert_eq!(
        u32::try_from(width).expect("width"),
        2 * advance,
        "…and covers exactly the two selected cells"
    );
}

#[test]
fn clicking_a_secret_field_places_the_caret_on_a_cell_boundary() {
    let theme = Theme::dark();
    let (text_x0, advance) = cell_layout(&theme);
    let mut field = TextField::new().secret(8).with_text("abcde");
    field.set_focused(true);
    let x = i32::try_from(text_x0 + 2 * advance).expect("cell x");
    field.on_pointer(&moved(x, 14), bounds(), Scale::ONE, &theme);
    field.on_pointer(&PRESS, bounds(), Scale::ONE, &theme);
    field.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme);
    type_str(&mut field, "Z");
    assert_eq!(
        field.text(),
        "abZcde",
        "a click on the third cell puts the caret before the third character"
    );
}

#[test]
fn dragging_a_secret_field_selects_whole_cells_and_typing_replaces_them() {
    let theme = Theme::dark();
    let (text_x0, advance) = cell_layout(&theme);
    let mut field = TextField::new().secret(8).with_text("abcdef");
    field.set_focused(true);
    let start = i32::try_from(text_x0).expect("cell x");
    let end = i32::try_from(text_x0 + 3 * advance).expect("cell x");
    field.on_pointer(&moved(start, 14), bounds(), Scale::ONE, &theme);
    field.on_pointer(&PRESS, bounds(), Scale::ONE, &theme);
    field.on_pointer(&moved(end, 14), bounds(), Scale::ONE, &theme);
    field.on_pointer(&RELEASE, bounds(), Scale::ONE, &theme);
    type_str(&mut field, "Z");
    assert_eq!(
        field.text(),
        "Zdef",
        "the drag selected the first three cells and typing replaced them"
    );
}

#[test]
fn a_secret_field_edits_exactly_like_a_plain_one() {
    let mut secret = TextField::new().secret(16);
    let mut plain = TextField::new().with_max_len(16);
    secret.set_focused(true);
    plain.set_focused(true);
    let script = [
        (Key::Char('h'), NONE_MODS),
        (Key::Char('u'), NONE_MODS),
        (Key::Char('n'), NONE_MODS),
        (Key::Char('t'), NONE_MODS),
        (Key::Named(NamedKey::Backspace), NONE_MODS),
        (Key::Named(NamedKey::Home), NONE_MODS),
        (Key::Named(NamedKey::Delete), NONE_MODS),
        (Key::Named(NamedKey::Right), NONE_MODS),
        (Key::Char('X'), NONE_MODS),
        (Key::Named(NamedKey::End), NONE_MODS),
        (Key::Named(NamedKey::Left), SHIFT),
        (Key::Named(NamedKey::Left), SHIFT),
        (Key::Char('Z'), NONE_MODS),
        (Key::Char('a'), CTRL),
        (Key::Char('Q'), NONE_MODS),
        (Key::Named(NamedKey::Enter), NONE_MODS),
        (Key::Named(NamedKey::Escape), NONE_MODS),
    ];
    for (key, mods) in script {
        assert_eq!(
            secret.on_key(key, mods),
            plain.on_key(key, mods),
            "the same key reports the same action in either mode"
        );
        assert_eq!(
            secret.text(),
            plain.text(),
            "…and leaves the same buffer behind"
        );
    }
    assert_eq!(secret.text(), "Q");
}

#[test]
fn a_secret_field_beads_in_dark_light_and_high_contrast() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        let field = TextField::new().secret(8).with_text("pw");
        let surface = field_surface(&field, &theme);
        assert_eq!(
            max_row_runs(&surface, premul(theme.palette().on_surface)),
            2,
            "every theme draws the same two beads in its own foreground"
        );
        assert!(
            has_pixel(&surface, premul(theme.palette().rim)),
            "…over the same plate and rim a plain field draws"
        );
    }
}

#[test]
fn reduced_motion_does_not_change_a_secret_field() {
    let field = TextField::new().secret(8).with_text("pw");
    let normal = field_surface(&field, &Theme::dark());
    let reduced = field_surface(&field, &reduced_motion());
    assert_eq!(
        normal.pixels(),
        reduced.pixels(),
        "a masked field has no animation for the motion policy to change"
    );
}

// --- Render-equivalence equality (the host's repaint gate) ----------------

/// Two samples clear of the field, so only the recorded coordinate differs.
const OFF_A: (i32, i32) = (400, 60);
const OFF_B: (i32, i32) = (460, 70);

#[test]
fn pointer_position_alone_never_changes_a_text_field_render() {
    let theme = Theme::dark();
    let mut a = TextField::new().with_text("hello");
    let mut b = a.clone();
    a.on_pointer(&moved(OFF_A.0, OFF_A.1), bounds(), Scale::ONE, &theme);
    b.on_pointer(&moved(OFF_B.0, OFF_B.1), bounds(), Scale::ONE, &theme);

    assert_eq!(
        a, b,
        "where the pointer last was is hit-testing state; the caret it \
         places lives in the editor and is still compared"
    );
    let sa = field_surface(&a, &theme);
    let sb = field_surface(&b, &theme);
    assert_eq!(
        sa.pixels(),
        sb.pixels(),
        "…and the two must therefore paint identically"
    );
}

#[test]
fn selection_drag_latch_alone_never_changes_a_text_field_render() {
    let theme = Theme::dark();
    // An empty field maps every press to byte zero, so the press moves no
    // caret and creates no selection: the drag latch is the only difference.
    let mut dragging = TextField::new();
    dragging.on_pointer(&moved(4, 14), bounds(), Scale::ONE, &theme);
    dragging.on_pointer(&PRESS, bounds(), Scale::ONE, &theme);

    let mut shown = TextField::new();
    shown.on_pointer(&moved(4, 14), bounds(), Scale::ONE, &theme);
    let mut pressed = ControlState::idle();
    pressed.pointer = crate::state::PointerState::Pressed;
    shown.set_state(pressed);

    assert_eq!(
        dragging, shown,
        "whether a press is still extending a selection is bookkeeping"
    );
    let sa = field_surface(&dragging, &theme);
    let sb = field_surface(&shown, &theme);
    assert_eq!(
        sa.pixels(),
        sb.pixels(),
        "…and the two must therefore paint identically"
    );
}

#[test]
fn pointer_position_alone_never_changes_a_search_field_render() {
    let theme = Theme::dark();
    let mut a = SearchField::new();
    let mut b = a.clone();
    a.on_pointer(&moved(OFF_A.0, OFF_A.1), bounds(), Scale::ONE, &theme);
    b.on_pointer(&moved(OFF_B.0, OFF_B.1), bounds(), Scale::ONE, &theme);

    assert_eq!(a, b);
    let sa = search_surface(&a, &theme);
    let sb = search_surface(&b, &theme);
    assert_eq!(sa.pixels(), sb.pixels());
}

#[test]
fn hover_and_typing_each_change_a_text_field_render() {
    let theme = Theme::dark();
    let resting = TextField::new();

    let mut hovered = resting.clone();
    hovered.on_pointer(&moved(4, 14), bounds(), Scale::ONE, &theme);
    assert_ne!(resting, hovered, "a hover highlight is visible");

    let typed = TextField::new().with_text("a");
    assert_ne!(resting, typed, "the text is visible");
}
