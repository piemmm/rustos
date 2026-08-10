//! Host unit tests for the viewer's view engine and its pointer-driven
//! [`Viewer`] composition.

use super::*;
use alloc::vec;
use tairix_controls::{damage, ScrollPart};
use tairix_geometry::Point;
use tairix_input::PointerButton;
use tairix_theme::ThemeRegistry;

/// Feed one pointer `event` at the initial window size, as its own input
/// round.
fn feed(
    viewer: &mut Viewer,
    event: &InputEvent,
    theme: &Theme,
    scale: Scale,
) -> ViewerPointerOutcome {
    viewer.on_pointer(
        event,
        WIN_WIDTH,
        WIN_HEIGHT,
        theme,
        scale,
        &mut damage::sink(),
    )
}

#[test]
fn content_lines_split_on_line_feeds_and_bound_rows_and_cols() {
    let lines = content_lines(b"one\ntwo\nthree", 8, 80);
    assert_eq!(lines, vec!["one", "two", "three"]);
    // The row bound truncates the tail, never panicking.
    assert_eq!(content_lines(b"a\nb\nc", 2, 80), vec!["a", "b"]);
    // The column bound drops each line's overflow.
    assert_eq!(content_lines(b"abcdef", 8, 3), vec!["abc"]);
    // Empty input shows nothing (not one empty line).
    assert!(content_lines(b"", 8, 80).is_empty());
}

#[test]
fn content_lines_sanitise_every_non_printable_byte() {
    // Control bytes, CR, tab, DEL, and non-ASCII all become the
    // placeholder: untrusted content never reaches the renderer raw.
    let lines = content_lines(b"a\x1b[31mb\r\tc\x7f\xffd", 8, 80);
    assert_eq!(lines, vec!["a.[31mb..c..d"]);
}

#[test]
fn renderers_produce_window_sized_surfaces() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let scale = Scale::ONE;
    let status =
        render_status("No file chosen.", theme, WIN_WIDTH, WIN_HEIGHT).expect("status renders");
    assert_eq!((status.width(), status.height()), (WIN_WIDTH, WIN_HEIGHT));
    let lines = content_lines(
        b"hello\nworld",
        visible_rows(theme, scale),
        visible_cols(theme, scale),
    );
    let content = render_lines(&lines, theme, WIN_WIDTH, WIN_HEIGHT).expect("content renders");
    assert_eq!((content.width(), content.height()), (WIN_WIDTH, WIN_HEIGHT));
    // The two states draw observably different pixels somewhere.
    assert_ne!(status.pixels(), content.pixels());
}

#[test]
fn renderers_track_an_arbitrary_resized_window() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let scale = Scale::ONE;
    // A resized window: the surface is exactly the reported client size,
    // not the initial one — the viewer draws into whatever the window
    // manager gave it.
    let (w, h) = (WIN_WIDTH * 2, WIN_HEIGHT + 40);
    let status = render_status("resized", theme, w, h).expect("status renders");
    assert_eq!((status.width(), status.height()), (w, h));
    let lines = content_lines(
        b"a\nb\nc",
        visible_rows_for(h, theme, scale),
        visible_cols_for(w, theme, scale),
    );
    let content = render_lines(&lines, theme, w, h).expect("content renders");
    assert_eq!((content.width(), content.height()), (w, h));
    // The minimum floor never yields a zero-extent surface.
    assert!(render_status("x", theme, MIN_WIN_WIDTH, MIN_WIN_HEIGHT).is_some());
}

#[test]
fn view_geometry_is_non_degenerate_and_scales_with_size() {
    let themes = ThemeRegistry::with_builtins();
    let theme = themes.active();
    let scale = Scale::ONE;
    assert!(
        visible_rows(theme, scale) > 4,
        "the window shows several lines"
    );
    assert!(
        visible_cols(theme, scale) > 16,
        "the window shows several columns"
    );
    // A wider/taller window shows strictly more columns/rows; a narrower
    // one strictly fewer — the geometry follows the client size.
    assert!(visible_cols_for(WIN_WIDTH * 2, theme, scale) > visible_cols(theme, scale));
    assert!(visible_rows_for(WIN_HEIGHT * 2, theme, scale) > visible_rows(theme, scale));
    assert!(visible_cols_for(WIN_WIDTH / 2, theme, scale) < visible_cols(theme, scale));
}

#[test]
fn relayout_rewraps_and_keeps_the_reader_near_their_place() {
    // Scrolled a third of the way down a long file.
    let mut v = view(300, 20);
    for _ in 0..100 {
        v.line_down();
    }
    assert_eq!(v.offset(), 100);
    // Resize to a taller window (more rows): the offset is preserved and
    // the larger viewport is honoured.
    let lines: Vec<String> = (0..300).map(|n| alloc::format!("line {n}")).collect();
    v.relayout(lines, 40);
    assert_eq!(v.window_rows(), 40);
    assert_eq!(
        v.offset(),
        100,
        "the reader keeps their place across a resize"
    );
    assert_eq!(v.visible()[0], "line 100");
    // Resize so the window is taller than the whole file: the offset
    // clamps back into range rather than dangling past the content.
    let short: Vec<String> = (0..10).map(|n| alloc::format!("line {n}")).collect();
    v.relayout(short, 40);
    assert_eq!(
        v.offset(),
        0,
        "content shorter than the window pins to the top"
    );
    assert_eq!(v.total_lines(), 10);
}

/// Build a view over `total` numbered lines showing `rows` at once.
fn view(total: usize, rows: usize) -> ScrollView {
    let lines: Vec<String> = (0..total).map(|n| alloc::format!("line {n}")).collect();
    ScrollView::new(lines, rows)
}

#[test]
fn scroll_view_shows_a_window_of_lines_from_the_offset() {
    let mut v = view(100, 10);
    assert_eq!(v.offset(), 0);
    assert_eq!(v.visible().len(), 10);
    assert_eq!(v.visible()[0], "line 0");

    assert!(v.line_down());
    assert_eq!(v.offset(), 1);
    assert_eq!(v.visible()[0], "line 1");

    // A page steps one row shy of a full window so a line stays visible.
    assert!(v.page_down());
    assert_eq!(v.offset(), 1 + 9);
}

#[test]
fn scroll_view_clamps_at_both_ends() {
    let mut v = view(100, 10);
    assert!(!v.line_up(), "already at the top");
    assert!(v.to_bottom());
    // The last row of content is the last row on screen: offset = 100 - 10.
    assert_eq!(v.offset(), 90);
    assert_eq!(v.visible().last().map(String::as_str), Some("line 99"));
    assert!(!v.line_down(), "already at the bottom");
    assert!(v.to_top());
    assert_eq!(v.offset(), 0);
}

#[test]
fn scroll_view_scrolls_by_wheel_ticks_one_line_per_tick_and_clamps() {
    let mut v = view(100, 10);
    // Positive ticks scroll toward the end, one line per tick.
    assert!(v.scroll_ticks(3));
    assert_eq!(v.offset(), 3);
    // Negative ticks scroll back toward the start.
    assert!(v.scroll_ticks(-1));
    assert_eq!(v.offset(), 2);
    // A zero tick moves nothing (fail closed, no guessed distance).
    assert!(!v.scroll_ticks(0));
    assert_eq!(v.offset(), 2);
    // A large or hostile tick count saturates at the last row rather
    // than overshooting, and reports no further movement once pinned.
    assert!(v.scroll_ticks(i32::MAX));
    assert_eq!(v.offset(), 90);
    assert!(!v.scroll_ticks(i32::MAX));
    assert_eq!(v.offset(), 90);
}

#[test]
fn scroll_view_with_fewer_lines_than_rows_is_not_scrollable() {
    let mut v = view(3, 10);
    assert_eq!(v.total_lines(), 3);
    assert!(!v.line_down(), "content fits, so nothing scrolls");
    assert!(!v.page_down());
    assert!(!v.to_bottom());
    assert_eq!(v.offset(), 0);
    assert_eq!(v.visible().len(), 3);
}

#[test]
fn scroll_view_and_window_bars_share_the_same_offset_math() {
    // The viewer's model and a window-manager-style geometry over the same
    // range agree on the offset a thumb position implies — the point of one
    // shared engine.
    let v = view(1000, 20);
    let range = v.model().range();
    assert_eq!(range.content_extent(), 1000);
    assert_eq!(range.viewport_extent(), 20);
    assert_eq!(range.max_offset(), 980);
}

/// The active theme and an unscaled desktop, used throughout the pointer
/// tests below so every one of them lays out identically.
fn theme_and_scale() -> (ThemeRegistry, Scale) {
    (ThemeRegistry::with_builtins(), Scale::ONE)
}

/// A `Viewer` with `total` numbered lines open in a [`WIN_WIDTH`] ×
/// [`WIN_HEIGHT`] window.
fn open_viewer(themes: &ThemeRegistry, scale: Scale, total: usize) -> Viewer {
    let theme = themes.active();
    let mut viewer = Viewer::new();
    let mut bytes = Vec::new();
    for n in 0..total {
        bytes.extend_from_slice(alloc::format!("line {n}\n").as_bytes());
    }
    viewer.open(bytes, WIN_WIDTH, WIN_HEIGHT, theme, scale);
    viewer
}

/// The first point in `bounds`, scanning top to bottom along the bar's
/// vertical centre, that `bar` classifies as `part`. Panics if none is
/// found — a test-only search, never production hit-testing.
fn find_part(
    bar: &ScrollBar,
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
    part: ScrollPart,
) -> Point {
    let x = bounds.left() + to_i32(bounds.width / 2);
    for y in bounds.top()..bounds.bottom() {
        let point = Point::new(x, y);
        if bar.part_at(bounds, point, scale, theme) == part {
            return point;
        }
    }
    panic!("no point in {bounds:?} classifies as {part:?}");
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

fn moved(to: Point) -> InputEvent {
    InputEvent::PointerMoved { to }
}

#[test]
fn layout_reserves_the_scrollbar_gutter_and_the_header_for_the_button() {
    let (themes, scale) = theme_and_scale();
    let theme = themes.active();
    let layout = ViewerLayout::for_window(WIN_WIDTH, WIN_HEIGHT, theme, scale);

    // The header is exactly the standard control height, and the text area
    // starts right below it.
    assert_eq!(layout.header.height, theme.metrics().control_height);
    assert_eq!(layout.text.top(), layout.header.bottom());

    // The scrollbar occupies exactly the theme's own gutter, and the text
    // area is shrunk by precisely that much so text never runs under it.
    assert_eq!(layout.scrollbar.width, theme.metrics().scrollbar_breadth);
    assert_eq!(layout.text.width + layout.scrollbar.width, WIN_WIDTH);
    assert_eq!(layout.scrollbar.left(), layout.text.right());

    // The button sits inside the header, never spilling past its edges.
    assert!(layout.header.top() <= layout.button.top());
    assert!(layout.button.bottom() <= layout.header.bottom());
    assert!(layout.button.right() <= layout.header.right());
}

#[test]
fn a_new_viewer_shows_no_file_chosen_with_an_empty_scrollbar() {
    let viewer = Viewer::new();
    assert_eq!(viewer.status(), Some("No file chosen."));
    assert!(!viewer.has_content());
    assert!(viewer.scroll_view().is_none());
}

#[test]
fn opening_a_file_shows_its_lines_and_clears_the_status() {
    let (themes, scale) = theme_and_scale();
    let viewer = open_viewer(&themes, scale, 5);
    assert_eq!(viewer.status(), None);
    assert!(viewer.has_content());
    assert_eq!(viewer.visible_lines().expect("open file")[0], "line 0");
}

#[test]
fn clicking_the_open_button_reports_the_open_action() {
    let (themes, scale) = theme_and_scale();
    let theme = themes.active();
    let mut viewer = Viewer::new();
    let layout = ViewerLayout::for_window(WIN_WIDTH, WIN_HEIGHT, theme, scale);
    let centre = Point::new(
        layout.button.left() + to_i32(layout.button.width / 2),
        layout.button.top() + to_i32(layout.button.height / 2),
    );

    let hover = feed(&mut viewer, &moved(centre), theme, scale);
    assert!(hover.changed, "hovering the button changes its drawn state");
    assert!(!hover.open_requested);

    let press = feed(&mut viewer, &PRESS, theme, scale);
    assert!(press.changed, "pressing the button changes its drawn state");
    assert!(!press.open_requested, "activation happens on release");

    let release = feed(&mut viewer, &RELEASE, theme, scale);
    assert!(
        release.open_requested,
        "releasing over the button activates it"
    );
}

#[test]
fn a_press_released_outside_the_button_does_not_activate_it() {
    let (themes, scale) = theme_and_scale();
    let theme = themes.active();
    let mut viewer = Viewer::new();
    let layout = ViewerLayout::for_window(WIN_WIDTH, WIN_HEIGHT, theme, scale);
    let centre = Point::new(
        layout.button.left() + to_i32(layout.button.width / 2),
        layout.button.top() + to_i32(layout.button.height / 2),
    );
    // Well outside every control: the top-left corner of the text area.
    let outside = Point::new(layout.text.left(), layout.text.top());

    let _ = feed(&mut viewer, &moved(centre), theme, scale);
    let press = feed(&mut viewer, &PRESS, theme, scale);
    assert!(press.changed);

    let _ = feed(&mut viewer, &moved(outside), theme, scale);
    let release = feed(&mut viewer, &RELEASE, theme, scale);
    assert!(
        !release.open_requested,
        "a release outside the button cancels the press rather than activating it"
    );
}

#[test]
fn hovering_the_button_changes_the_rendered_surface() {
    let (themes, scale) = theme_and_scale();
    let theme = themes.active();
    let mut viewer = Viewer::new();
    let layout = ViewerLayout::for_window(WIN_WIDTH, WIN_HEIGHT, theme, scale);
    let centre = Point::new(
        layout.button.left() + to_i32(layout.button.width / 2),
        layout.button.top() + to_i32(layout.button.height / 2),
    );

    let resting = viewer
        .render(theme, scale, WIN_WIDTH, WIN_HEIGHT)
        .expect("renders");
    let _ = feed(&mut viewer, &moved(centre), theme, scale);
    let hovered = viewer
        .render(theme, scale, WIN_WIDTH, WIN_HEIGHT)
        .expect("renders");
    assert_ne!(
        resting.pixels(),
        hovered.pixels(),
        "hovering the button must change what is drawn"
    );

    // Moving away again restores the resting picture.
    let away = Point::new(layout.text.left(), layout.text.top());
    let _ = feed(&mut viewer, &moved(away), theme, scale);
    let unhovered = viewer
        .render(theme, scale, WIN_WIDTH, WIN_HEIGHT)
        .expect("renders");
    assert_eq!(
        resting.pixels(),
        unhovered.pixels(),
        "moving off the button restores the resting picture"
    );
}

#[test]
fn dragging_the_scrollbar_thumb_scrolls_by_the_expected_offset() {
    let (themes, scale) = theme_and_scale();
    let theme = themes.active();
    let mut viewer = open_viewer(&themes, scale, 1000);
    let layout = ViewerLayout::for_window(WIN_WIDTH, WIN_HEIGHT, theme, scale);
    let model = viewer.scroll_view().expect("open file").model();
    let bar = ScrollBar::new(ScrollOrientation::Vertical, model);
    let geometry = bar
        .geometry(layout.scrollbar, scale, theme)
        .expect("a scrollable file has scrollbar geometry");
    assert!(geometry.draggable(), "1000 lines must be draggable");
    assert_eq!(
        geometry.thumb().start,
        0,
        "the offset starts at zero, so the thumb starts at the track origin"
    );

    // Grab the very first pixel classified as the thumb — the offset is
    // zero, so this is exactly the track origin, giving a zero anchor.
    let press_point = find_part(&bar, layout.scrollbar, scale, theme, ScrollPart::Thumb);
    let _ = feed(&mut viewer, &moved(press_point), theme, scale);
    let _ = feed(&mut viewer, &PRESS, theme, scale);

    let delta = 40;
    let drag_to = Point::new(press_point.x, press_point.y + delta);
    let outcome = feed(&mut viewer, &moved(drag_to), theme, scale);
    assert!(outcome.changed, "dragging the thumb must move the view");
    let _ = feed(&mut viewer, &RELEASE, theme, scale);

    // A zero-anchor drag maps a `delta`-pixel move straight onto the shared
    // geometry's own offset math, so the expected offset comes from the same
    // engine the bar itself used rather than a re-derived pixel formula.
    let expected = geometry.offset_for_drag(delta, 0);
    assert_eq!(
        viewer.scroll_view().expect("open file").offset() as u64,
        expected
    );
    assert!(
        expected > 0,
        "the drag distance must move the view for this to be a real test"
    );
}

#[test]
fn clicking_the_track_pages_the_view() {
    let (themes, scale) = theme_and_scale();
    let theme = themes.active();
    let mut viewer = open_viewer(&themes, scale, 1000);
    let layout = ViewerLayout::for_window(WIN_WIDTH, WIN_HEIGHT, theme, scale);
    let model = viewer.scroll_view().expect("open file").model();
    let bar = ScrollBar::new(ScrollOrientation::Vertical, model);
    let page_step = viewer.scroll_view().expect("open file").window_rows() - 1;

    let after_thumb = find_part(&bar, layout.scrollbar, scale, theme, ScrollPart::TrackAfter);
    let outcome = feed(&mut viewer, &moved(after_thumb), theme, scale);
    assert!(
        outcome.changed,
        "hovering the track changes the drawn state"
    );

    let outcome = feed(&mut viewer, &PRESS, theme, scale);
    assert!(outcome.changed, "a track click must page the view");
    let _ = feed(&mut viewer, &RELEASE, theme, scale);

    assert_eq!(
        viewer.scroll_view().expect("open file").offset(),
        page_step,
        "one click pages by one page step"
    );
}

#[test]
fn the_wheel_still_scrolls_an_open_file() {
    let (themes, scale) = theme_and_scale();
    let mut viewer = open_viewer(&themes, scale, 100);
    assert_eq!(viewer.scroll_view().expect("open file").offset(), 0);

    assert!(viewer.scroll_ticks(5));
    assert_eq!(viewer.scroll_view().expect("open file").offset(), 5);

    assert!(viewer.scroll_ticks(-2));
    assert_eq!(viewer.scroll_view().expect("open file").offset(), 3);
}

#[test]
fn show_status_clears_content_and_empties_the_scrollbar() {
    let (themes, scale) = theme_and_scale();
    let mut viewer = open_viewer(&themes, scale, 50);
    assert!(viewer.has_content());

    viewer.show_status("Pick refused.");
    assert_eq!(viewer.status(), Some("Pick refused."));
    assert!(!viewer.has_content());
    assert!(viewer.scroll_view().is_none());
}
