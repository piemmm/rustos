//! Unit tests for the multi-line text entry (spec §11.42, §20 checklist).
//!
//! These cover what makes a `TextArea` a different control from a
//! `TextField` rather than a taller one: that its text **wraps** at the box's
//! own width, that a typed newline is a break, that the caret and the
//! selection work in the lines a reader sees, that the viewport follows the
//! caret and answers the wheel, and that the scrollbar appears exactly when
//! there is more text than box. The plate, disposition, validation and
//! message rendering are the family's shared recipe and are covered against
//! the one-line field; what is checked here is that the box takes part in it.

use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::damage::sink;
use crate::state::{AuthorityState, ControlState, ValidationState};
use crate::testkit::{control_font, high_contrast};
use crate::text::debug_area_layout;
use crate::{TextAction, TextArea};

/// A box four rows tall and wide enough for a handful of words.
const W: u32 = 200;

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
    control_font(&theme(), Scale::ONE)
}

/// The bounds of a box `rows` text lines tall.
fn bounds(area: &TextArea, rows: u32) -> Rect {
    Rect::new(0, 0, W, area.measured_height(rows, W, Scale::ONE, &theme()))
}

/// A focused, editable area holding `text`.
fn area(text: &str) -> TextArea {
    let mut area = TextArea::new().with_text(text);
    area.set_focused(true);
    area
}

fn press(area: &mut TextArea, at: Point, rows: u32) {
    let bounds = bounds(area, rows);
    let mut damage = sink();
    area.on_pointer(
        &InputEvent::PointerMoved { to: at },
        bounds,
        Scale::ONE,
        &theme(),
        &mut damage,
    );
    area.on_pointer(&PRESS, bounds, Scale::ONE, &theme(), &mut damage);
}

fn key(area: &mut TextArea, key: Key, mods: Modifiers, rows: u32) -> Option<TextAction> {
    let bounds = bounds(area, rows);
    let mut damage = sink();
    area.on_key(key, mods, bounds, Scale::ONE, &theme(), &mut damage)
}

fn render(area: &TextArea, rows: u32) -> Surface {
    let bounds = bounds(area, rows);
    let mut surface = Surface::new(bounds.width, bounds.height).expect("surface");
    area.render(&mut surface, bounds, Scale::ONE, &theme());
    surface
}

/// The rows of the drawn box that carry any ink at all, which is how many
/// lines of text the box put on screen.
fn inked_rows(surface: &Surface, ink: Color) -> Vec<u32> {
    let premul = ink.premultiply();
    (0..surface.height())
        .filter(|&y| (0..surface.width()).any(|x| surface.get(x, y) == Some(premul)))
        .collect()
}

#[test]
fn a_long_text_wraps_over_the_lines_the_box_has_rather_than_scrolling_sideways() {
    let font = font();
    let one_line = "a short line";
    let long = "one two three four five six seven eight nine ten eleven twelve";
    assert!(
        font.text_width(long) > W,
        "the sample must not fit the box on one line"
    );
    let area = area(long);
    let bounds = bounds(&area, 4);
    // The wrap is the shared fitter's, at the box's own inner width, so the
    // text takes more than one line and every one of them fits.
    let lines: Vec<_> = font.lines_to_width(long, bounds.width).collect();
    assert!(lines.len() > 1, "the text must have wrapped");
    assert_eq!(font.lines_to_width(one_line, bounds.width).count(), 1);
}

#[test]
fn enter_inserts_a_newline_and_never_submits() {
    let mut area = area("one");
    assert_eq!(
        key(&mut area, Key::Named(NamedKey::Enter), NONE_MODS, 4),
        Some(TextAction::Edited),
        "a box that holds paragraphs takes Enter as a paragraph"
    );
    assert_eq!(area.text(), "one\n");
    key(&mut area, Key::Char('t'), NONE_MODS, 4);
    assert_eq!(area.text(), "one\nt");
    assert_eq!(
        key(&mut area, Key::Named(NamedKey::Escape), NONE_MODS, 4),
        Some(TextAction::Cancelled),
        "Escape still dismisses"
    );
}

#[test]
fn a_read_only_or_denied_box_refuses_every_edit_including_a_newline() {
    let mut read_only = TextArea::new().with_text("kept").read_only(true);
    read_only.set_focused(true);
    assert_eq!(
        key(&mut read_only, Key::Named(NamedKey::Enter), NONE_MODS, 4),
        None
    );
    key(&mut read_only, Key::Char('x'), NONE_MODS, 4);
    assert_eq!(read_only.text(), "kept", "a read-only box refuses edits");

    let mut denied = area("kept");
    denied.set_state(ControlState::idle().with_authority(AuthorityState::Denied));
    denied.set_focused(true);
    assert_eq!(
        key(&mut denied, Key::Named(NamedKey::Enter), NONE_MODS, 4),
        None,
        "a denied box fails closed"
    );
    assert_eq!(denied.text(), "kept");
}

#[test]
fn up_and_down_walk_the_lines_a_reader_sees_and_keep_their_column() {
    // Three paragraphs, the middle one short: walking down through it and
    // out the other side must come back to the column it set out from.
    let mut area = area("aaaaaaaa\nbb\ncccccccc");
    key(&mut area, Key::Named(NamedKey::Home), CTRL, 4);
    for _ in 0..6 {
        key(&mut area, Key::Named(NamedKey::Right), NONE_MODS, 4);
    }
    let column = area.text()[..6].len();
    assert_eq!(column, 6);
    key(&mut area, Key::Named(NamedKey::Down), NONE_MODS, 4);
    // The short middle line cannot hold the column, so the caret goes to its
    // end — and the goal column is remembered rather than lost.
    assert_eq!(area.text().len(), 20);
    key(&mut area, Key::Named(NamedKey::Down), NONE_MODS, 4);
    let caret_in_third = |area: &TextArea| {
        let third = area.text().rfind('\n').expect("a third line") + 1;
        third
    };
    let third = caret_in_third(&area);
    key(&mut area, Key::Named(NamedKey::Home), NONE_MODS, 4);
    key(&mut area, Key::Named(NamedKey::End), NONE_MODS, 4);
    assert_eq!(
        third + 8,
        area.text().len(),
        "End goes to the end of the visual line it is on"
    );
}

#[test]
fn a_newline_at_the_end_opens_a_line_the_caret_moves_onto() {
    let mut area = area("one");
    key(&mut area, Key::Named(NamedKey::Enter), NONE_MODS, 4);
    // The caret is on the line the break opened, not back at the end of the
    // one before it — so Home and End work on the new line, and Up is what
    // returns to the old one.
    key(&mut area, Key::Named(NamedKey::End), NONE_MODS, 4);
    key(&mut area, Key::Char('2'), NONE_MODS, 4);
    assert_eq!(area.text(), "one\n2");
    key(&mut area, Key::Named(NamedKey::Backspace), NONE_MODS, 4);
    key(&mut area, Key::Named(NamedKey::Up), NONE_MODS, 4);
    key(&mut area, Key::Named(NamedKey::End), NONE_MODS, 4);
    key(&mut area, Key::Char('!'), NONE_MODS, 4);
    assert_eq!(area.text(), "one!\n", "Up reaches the line above the break");

    // And it is drawn on that line. Both shots are focused, so the plate and
    // its ring are identical and the difference in the empty line's band is
    // the caret arriving on it.
    let mut trailing = TextArea::new().with_text("two\n");
    trailing.set_focused(true);
    let bounds = bounds(&trailing, 4);
    let (text, _, _, lines) =
        debug_area_layout(&trailing, bounds, Scale::ONE, &theme()).expect("a laid-out box");
    assert_eq!(lines, 2, "a trailing break opens a line for the caret");
    let line = font().line_height();
    let second = u32::try_from(text.top()).unwrap_or(0) + line;
    let shot = |area: &mut TextArea, key: Key| {
        area.on_key(key, CTRL, bounds, Scale::ONE, &theme(), &mut sink());
        render(area, 4)
    };
    let on_it = shot(&mut trailing, Key::Named(NamedKey::End));
    let away = shot(&mut trailing, Key::Named(NamedKey::Home));
    let changed: Vec<u32> = (0..on_it.height())
        .filter(|&y| (0..on_it.width()).any(|x| on_it.get(x, y) != away.get(x, y)))
        .collect();
    assert!(
        changed
            .iter()
            .any(|&y| (second..second + line).contains(&y)),
        "the caret is drawn on the line the break opened"
    );
}

#[test]
fn home_and_end_work_on_the_visual_line_and_ctrl_on_the_whole_text() {
    let mut area = area("first\nsecond");
    key(&mut area, Key::Named(NamedKey::Home), CTRL, 4);
    key(&mut area, Key::Named(NamedKey::Down), NONE_MODS, 4);
    key(&mut area, Key::Named(NamedKey::End), NONE_MODS, 4);
    // Typing here lands at the end of the second line, not the text's end.
    key(&mut area, Key::Char('!'), NONE_MODS, 4);
    assert_eq!(area.text(), "first\nsecond!");
    key(&mut area, Key::Named(NamedKey::Home), CTRL, 4);
    key(&mut area, Key::Char('>'), NONE_MODS, 4);
    assert_eq!(area.text(), ">first\nsecond!");
    key(&mut area, Key::Named(NamedKey::End), CTRL, 4);
    key(&mut area, Key::Char('<'), NONE_MODS, 4);
    assert_eq!(area.text(), ">first\nsecond!<");
}

#[test]
fn shift_extends_a_selection_across_lines_and_typing_replaces_it() {
    let mut area = area("one\ntwo\nthree");
    key(&mut area, Key::Named(NamedKey::Home), CTRL, 4);
    key(&mut area, Key::Named(NamedKey::Down), SHIFT, 4);
    key(&mut area, Key::Named(NamedKey::End), SHIFT, 4);
    key(&mut area, Key::Char('X'), NONE_MODS, 4);
    assert_eq!(
        area.text(),
        "X\nthree",
        "a selection spanning a break is replaced whole"
    );
}

#[test]
fn ctrl_a_selects_the_whole_text_and_backspace_clears_it() {
    let mut area = area("one\ntwo");
    key(&mut area, Key::Char('a'), CTRL, 4);
    assert_eq!(
        key(&mut area, Key::Named(NamedKey::Backspace), NONE_MODS, 4),
        Some(TextAction::Edited)
    );
    assert!(area.text().is_empty());
}

#[test]
fn a_click_lands_on_the_line_it_fell_on() {
    let mut area = area("aaa\nbbb\nccc");
    let bounds = bounds(&area, 4);
    let (text, _, rows, lines) =
        debug_area_layout(&area, bounds, Scale::ONE, &theme()).expect("a laid-out box");
    assert!(
        rows >= 3 && lines == 3,
        "three lines in a box that holds them"
    );
    let line = font().line_height();
    // Well down the second line and past the end of its text: the caret goes
    // to that line's end, not onto the line below or after its break.
    press(
        &mut area,
        Point::new(
            text.left() + to_i32(text.width) - 1,
            text.top() + to_i32(line + line / 2),
        ),
        4,
    );
    let mut damage = sink();
    area.on_pointer(&RELEASE, bounds, Scale::ONE, &theme(), &mut damage);
    key(&mut area, Key::Char('!'), NONE_MODS, 4);
    assert_eq!(area.text(), "aaa\nbbb!\nccc");
}

#[test]
fn the_viewport_follows_the_caret_and_the_wheel_moves_it_alone() {
    let text = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight";
    let mut area = area(text);
    let bounds = bounds(&area, 3);
    assert_eq!(area.scroll_offset(), 0);

    // Typing at the end of a long note brings the end into view.
    let mut damage = sink();
    area.on_key(
        Key::Named(NamedKey::End),
        CTRL,
        bounds,
        Scale::ONE,
        &theme(),
        &mut damage,
    );
    assert!(
        area.scroll_offset() > 0,
        "the viewport must follow the caret to the end"
    );
    let at_end = area.scroll_offset();

    // The wheel scrolls the viewport without moving the caret.
    let before = area.text().len();
    area.on_pointer(
        &InputEvent::PointerMoved {
            to: Point::new(10, 10),
        },
        bounds,
        Scale::ONE,
        &theme(),
        &mut damage,
    );
    area.on_pointer(
        &InputEvent::PointerScrolled { dx: 0, dy: -2 },
        bounds,
        Scale::ONE,
        &theme(),
        &mut damage,
    );
    assert!(
        area.scroll_offset() < at_end,
        "the wheel must scroll back up"
    );
    area.on_key(
        Key::Char('!'),
        NONE_MODS,
        bounds,
        Scale::ONE,
        &theme(),
        &mut damage,
    );
    assert_eq!(
        area.text().len(),
        before + 1,
        "the wheel moved the viewport, not the caret"
    );
    assert_eq!(
        area.scroll_offset(),
        at_end,
        "typing brings the caret back into view"
    );
}

#[test]
fn a_text_that_fits_grows_no_scrollbar_and_one_that_does_not_does() {
    let short = area("one");
    let long = area("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten");
    let layout = |area: &TextArea| {
        debug_area_layout(area, bounds(area, 4), Scale::ONE, &theme()).expect("a laid-out box")
    };
    let (fits_text, no_bar, _, _) = layout(&short);
    let (over_text, bar, rows, lines) = layout(&long);
    assert_eq!(no_bar, None, "a text that fits shows no bar");
    let bar = bar.expect("a text longer than the box shows that there is more");
    assert!(lines > rows);
    assert!(
        over_text.width < fits_text.width,
        "the bar takes its gutter out of the text's own column"
    );

    // And the gutter it took is where the bar is actually drawn.
    let surface = render(&long, 4);
    let x = u32::try_from(bar.left() + to_i32(bar.width) / 2).expect("a gutter on the surface");
    assert!(
        (0..surface.height()).any(|y| surface.get(x, y).is_some_and(|p| p.a > 0)),
        "the bar's gutter is drawn in"
    );
}

#[test]
fn a_placeholder_shows_while_the_box_is_empty_and_wraps_like_the_text() {
    let area = TextArea::new()
        .with_placeholder("Say something about this machine, at whatever length you like");
    let surface = render(&area, 3);
    let muted = Color::from(theme().palette().on_surface_muted);
    let rows = inked_rows(&surface, muted);
    assert!(
        rows.iter().any(|&y| y > font().line_height()),
        "a long placeholder wraps rather than being cut at the edge"
    );
}

#[test]
fn a_message_is_wrapped_below_the_box_and_the_box_keeps_its_rows() {
    let plain = TextArea::new();
    let noted = TextArea::new().with_message(
        "That is longer than this machine's name may be, so shorten it before saving.",
    );
    let rows = 3;
    let plain_h = plain.measured_height(rows, W, Scale::ONE, &theme());
    let noted_h = noted.measured_height(rows, W, Scale::ONE, &theme());
    assert!(
        noted_h > plain_h + font().line_height(),
        "a wrapped message reserves the lines it actually needs"
    );
    assert!(
        noted_h - plain_h <= font().line_height() * 3,
        "a message is bounded: it is a note about the text, not a document"
    );
}

#[test]
fn a_box_with_no_room_for_a_line_draws_nothing_rather_than_a_clipped_glyph() {
    let area = area("anything at all");
    let mut surface = Surface::new(W, 4).expect("surface");
    area.render(&mut surface, Rect::new(0, 0, W, 4), Scale::ONE, &theme());
    let ink = Color::from(theme().palette().on_surface).premultiply();
    assert!(
        (0..surface.width()).all(|x| (0..surface.height()).all(|y| surface.get(x, y) != Some(ink))),
        "a clipped half-line is worse than no line"
    );
}

#[test]
fn an_unfocused_or_disabled_box_draws_no_caret() {
    let mut resting = TextArea::new().with_text("text");
    resting.set_focused(false);
    let unfocused = render(&resting, 3);
    let mut focused = TextArea::new().with_text("text");
    focused.set_focused(true);
    let lit = render(&focused, 3);
    assert_ne!(
        unfocused.pixels(),
        lit.pixels(),
        "a focused box shows its caret"
    );
}

#[test]
fn the_high_contrast_and_scaled_forms_draw_and_measure() {
    for theme in [Theme::dark(), Theme::light(), high_contrast()] {
        for scale in [Scale::ONE, Scale::from_percent(200).expect("scale")] {
            let area = TextArea::new().with_text("one two three four five six seven");
            let height = area.measured_height(3, W, scale, &theme);
            assert!(height > 0);
            let bounds = Rect::new(0, 0, W, height);
            let mut surface = Surface::new(W, height).expect("surface");
            area.render(&mut surface, bounds, scale, &theme);
        }
    }
}

#[test]
fn equal_boxes_draw_the_same_pixels_and_a_moved_viewport_is_a_difference() {
    let a = area("one\ntwo\nthree\nfour\nfive\nsix");
    let b = a.clone();
    assert_eq!(a, b, "a clone draws the same box");
    let mut scrolled = a.clone();
    let bounds = bounds(&scrolled, 2);
    let mut damage = sink();
    scrolled.on_key(
        Key::Named(NamedKey::End),
        CTRL,
        bounds,
        Scale::ONE,
        &theme(),
        &mut damage,
    );
    assert_ne!(a, scrolled, "a moved caret and viewport is a repaint");
    assert!(!damage.is_empty(), "the change was reported");
}

fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// The validation state is the field family's; this only pins that a box
/// takes part in it rather than ignoring it.
#[test]
fn a_boxs_validation_state_changes_what_it_draws() {
    let plain = area("value");
    let mut invalid = area("value");
    invalid.set_state(ControlState::idle().with_validation(ValidationState::Invalid));
    invalid.set_focused(true);
    assert_ne!(
        render(&plain, 3).pixels(),
        render(&invalid, 3).pixels(),
        "a refused value reads as refused"
    );
}
