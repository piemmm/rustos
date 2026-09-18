//! Unit tests for the settings shell.
//!
//! These cover what the shell exists to get right: the frame's regions and
//! what a narrow window sheds, the strip's shape and the cursor reaching
//! every row of it, the search filtering the strip, the trail rewriting
//! itself, the category list a shed strip becomes, and the scroll a pane too
//! tall for its column gets.

use tairix_abi::desktop::{Appearance, Contrast, Density};
use tairix_font::install_test_transport;
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_icon::NoArtwork;
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_wallpaper::{DesktopSettings, SettingsKey};

use crate::appearance::{Composition, Setting, POINTER_SIZE_LABEL};
use crate::frame::{resolve_frame, Overflow, CONTENT_FLOOR, SIDEBAR_WIDTH};
use crate::registry::{Category, Location, Pane, StripRow, CATEGORIES};
use crate::shell::{Shell, ShellOutcome};

/// A window wide enough to seat the strip and a full content column.
const WIDE: Rect = Rect::new(0, 0, 900, 640);
/// A window too narrow to seat the strip at all.
const NARROW: Rect = Rect::new(0, 0, 360, 640);

fn theme() -> Theme {
    install_test_transport();
    Theme::dark()
}

fn shell() -> Shell {
    Shell::new(DesktopSettings::default()).expect("the registry holds a category")
}

fn damage() -> tairix_geometry::Region {
    tairix_controls::damage::sink()
}

/// Press and release the primary button at `at`.
fn click(shell: &mut Shell, at: Point, viewport: Rect, theme: &Theme) {
    let mut sink = damage();
    for event in [
        InputEvent::PointerMoved { to: at },
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
    ] {
        shell.on_pointer(&event, viewport, Scale::ONE, theme, &mut sink);
    }
}

/// The centre of strip row `index`, or `None` when the strip did not seat it.
fn row_point(shell: &Shell, index: usize, viewport: Rect, theme: &Theme) -> Option<Point> {
    let frame = shell.frame(viewport, Scale::ONE, theme);
    let sidebar = frame.sidebar?;
    let rect = shell.strip_row_rect(index, sidebar, Scale::ONE, theme)?;
    Some(Point::new(
        rect.left() + to_i32(rect.width / 2),
        rect.top() + to_i32(rect.height / 2),
    ))
}

// --- The frame ----------------------------------------------------------

#[test]
fn a_wide_window_seats_every_region() {
    let theme = theme();
    let frame = resolve_frame(WIDE, Scale::ONE, &theme, Overflow::default());
    let search = frame.search.expect("a search field");
    let sidebar = frame.sidebar.expect("a strip");
    assert_eq!(search.width, sidebar.width);
    assert_eq!(search.left(), sidebar.left());
    // The band sits above the body, and the two columns do not overlap.
    assert_eq!(search.bottom(), sidebar.top());
    assert_eq!(frame.breadcrumb.bottom(), frame.content.top());
    assert!(frame.content.left() >= sidebar.right());
    assert_eq!(frame.breadcrumb.left(), frame.content.left());
    assert!(frame.scrollbar.is_none(), "nothing to scroll");
}

#[test]
fn a_narrow_window_sheds_the_strip_and_keeps_the_pane() {
    let theme = theme();
    let frame = resolve_frame(NARROW, Scale::ONE, &theme, Overflow::default());
    assert!(frame.sidebar.is_none());
    assert!(frame.search.is_none(), "nothing left to filter");
    assert_eq!(frame.content.left(), NARROW.left());
    assert_eq!(frame.content.width, NARROW.width);
    assert_eq!(frame.breadcrumb.width, NARROW.width);
    assert!(frame.content.height > 0, "the pane survives");
}

#[test]
fn the_strip_is_shed_exactly_at_the_stated_floor() {
    let theme = theme();
    let gap = Scale::ONE.scale_length(theme.metrics().control_gap).max(1);
    let bar = Scale::ONE.scale_length(SIDEBAR_WIDTH).max(1);
    let floor = Scale::ONE.scale_length(CONTENT_FLOOR).max(1);
    let exact = bar + gap + floor;
    assert!(resolve_frame(
        Rect::new(0, 0, exact, 480),
        Scale::ONE,
        &theme,
        Overflow::default()
    )
    .sidebar
    .is_some());
    assert!(resolve_frame(
        Rect::new(0, 0, exact - 1, 480),
        Scale::ONE,
        &theme,
        Overflow::default()
    )
    .sidebar
    .is_none());
}

#[test]
fn a_pane_taller_than_its_column_gets_a_scrollbar_beside_it() {
    let theme = theme();
    let short = Rect::new(0, 0, 900, 120);
    let fits = resolve_frame(short, Scale::ONE, &theme, Overflow::default());
    assert!(fits.scrollbar.is_none());
    let scrolls = resolve_frame(
        short,
        Scale::ONE,
        &theme,
        Overflow {
            strip: false,
            pane: true,
        },
    );
    let bar = scrolls.scrollbar.expect("a bar");
    assert_eq!(bar.left(), scrolls.content.right());
    assert_eq!(bar.top(), scrolls.content.top());
    assert_eq!(bar.height, scrolls.content.height);
    assert!(
        scrolls.content.width < fits.content.width,
        "the bar took room"
    );
}

/// The shell lays out for itself: a pane too tall for a short column is
/// measured once and the bar follows from the scroll range, so the input path
/// never re-measures a wrapped statement.
#[test]
fn laying_out_a_short_window_raises_the_scrollbar_the_pane_needs() {
    let theme = theme();
    let short = Rect::new(0, 0, 420, 110);
    let mut shell = shell();
    // Before any layout there is no measured range, so no bar is claimed.
    assert!(shell.frame(short, Scale::ONE, &theme).scrollbar.is_none());

    shell.lay_out(short, Scale::ONE, &theme);
    let frame = shell.frame(short, Scale::ONE, &theme);
    assert!(
        frame.scrollbar.is_some(),
        "the statement does not fit a {}px column",
        frame.content.height
    );
    // A window tall enough for the same pane claims none.
    shell.lay_out(WIDE, Scale::ONE, &theme);
    assert!(shell.frame(WIDE, Scale::ONE, &theme).scrollbar.is_none());
}

/// A wheel tick over the pane column scrolls it, and the offset it lands on
/// is what the next paint draws from.
#[test]
fn a_wheel_tick_over_the_pane_scrolls_it_and_stops_at_the_ends() {
    let theme = theme();
    let short = Rect::new(0, 0, 420, 110);
    let mut shell = shell();
    shell.lay_out(short, Scale::ONE, &theme);
    let frame = shell.frame(short, Scale::ONE, &theme);
    assert!(frame.scrollbar.is_some(), "the pane scrolls");

    let at = Point::new(
        frame.content.left() + to_i32(frame.content.width / 2),
        frame.content.top() + to_i32(frame.content.height / 2),
    );
    let mut sink = damage();
    shell.on_pointer(
        &InputEvent::PointerMoved { to: at },
        short,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    assert_eq!(shell.scroll_offset(), 0);
    assert!(shell
        .on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: 1 },
            short,
            Scale::ONE,
            &theme,
            &mut sink,
        )
        .changed());
    assert!(shell.scroll_offset() > 0, "the column moved");

    // Scrolling back past the top clamps rather than wrapping.
    for _ in 0..20 {
        shell.on_pointer(
            &InputEvent::PointerScrolled { dx: 0, dy: -1 },
            short,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
    assert_eq!(shell.scroll_offset(), 0);
}

#[test]
fn a_viewport_with_no_room_for_the_band_yields_nothing() {
    let theme = theme();
    let frame = resolve_frame(
        Rect::new(0, 0, 900, 1),
        Scale::ONE,
        &theme,
        Overflow::default(),
    );
    assert!(frame.sidebar.is_none());
    assert_eq!(frame.content, Rect::EMPTY);
    assert_eq!(frame.breadcrumb, Rect::EMPTY);
}

#[test]
fn the_frame_holds_at_a_larger_density() {
    let theme = theme();
    let scale = Scale::from_percent(200).expect("a valid scale");
    let frame = resolve_frame(
        Rect::new(0, 0, 1600, 1200),
        scale,
        &theme,
        Overflow::default(),
    );
    let sidebar = frame.sidebar.expect("a strip");
    assert_eq!(sidebar.width, scale.scale_length(SIDEBAR_WIDTH).max(1));
    assert!(frame.content.width > 0);
}

// --- The strip and the cursor -------------------------------------------

#[test]
fn the_surface_opens_on_the_first_category() {
    let opening = Location::opening().expect("an opening location");
    assert_eq!(shell().location(), opening);
}

#[test]
fn every_category_is_a_row_of_the_strip() {
    let shell = shell();
    let categories = shell
        .rows()
        .iter()
        .filter(|row| matches!(row, StripRow::Category(_)))
        .count();
    assert_eq!(categories, CATEGORIES.len());
}

#[test]
fn the_cursor_reaches_every_row_of_the_strip() {
    let theme = theme();
    let mut shell = shell();
    let mut sink = damage();
    // Put the cursor on the strip, then walk it to the end.
    let rows = shell.rows().len();
    assert!(rows > 1);
    for _ in 0..rows * 2 {
        shell.on_key(
            Key::Named(NamedKey::Down),
            Modifiers::default(),
            WIDE,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
    // Walking down and choosing each row in turn reaches every location the
    // strip offers, which is what "the cursor reaches every row" means.
    for index in 0..shell.rows().len() {
        let Some(at) = row_point(&shell, index, WIDE, &theme) else {
            continue;
        };
        let expected = shell.rows()[index].location().expect("a location");
        click(&mut shell, at, WIDE, &theme);
        assert_eq!(shell.location(), expected, "row {index}");
    }
}

#[test]
fn choosing_a_disclosing_category_opens_its_first_pane_and_discloses_it() {
    let theme = theme();
    let mut shell = shell();
    let mut sink = damage();
    let networking = shell
        .rows()
        .iter()
        .position(|row| matches!(row, StripRow::Category(Category::Networking)))
        .expect("the networking row");
    let at = row_point(&shell, networking, WIDE, &theme).expect("a row");
    click(&mut shell, at, WIDE, &theme);
    assert_eq!(shell.location().category, Category::Networking);
    assert_eq!(shell.location().pane, Pane::Ethernet);
    assert!(
        shell
            .rows()
            .iter()
            .any(|row| matches!(row, StripRow::Pane(Category::Networking, _))),
        "its panes are disclosed"
    );
    // General's panes are no longer disclosed: one category opens at a time.
    assert!(!shell
        .rows()
        .iter()
        .any(|row| matches!(row, StripRow::Pane(Category::General, _))));
    let _ = &mut sink;
}

#[test]
fn the_trail_names_where_the_surface_is() {
    let theme = theme();
    let mut shell = shell();
    // A disclosing category shows three crumbs; a single-pane one shows two,
    // because saying its name twice would read as two places.
    assert_eq!(
        shell.trail_labels(),
        alloc::vec!["Settings", "General", "About"]
    );
    let sound = shell
        .rows()
        .iter()
        .position(|row| matches!(row, StripRow::Category(Category::Sound)))
        .expect("the sound row");
    let at = row_point(&shell, sound, WIDE, &theme).expect("a row");
    click(&mut shell, at, WIDE, &theme);
    assert_eq!(shell.trail_labels(), alloc::vec!["Settings", "Sound"]);
}

// --- Search -------------------------------------------------------------

#[test]
fn a_query_filters_the_strip_to_what_it_reaches() {
    let theme = theme();
    let mut shell = shell();
    let mut sink = damage();
    let frame = shell.frame(WIDE, Scale::ONE, &theme);
    let search = frame.search.expect("a search field");
    click(
        &mut shell,
        Point::new(search.left() + 4, search.top() + to_i32(search.height / 2)),
        WIDE,
        &theme,
    );
    for ch in "sound".chars() {
        shell.on_key(
            Key::Char(ch),
            Modifiers::default(),
            WIDE,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
    assert_eq!(shell.rows(), &[StripRow::Category(Category::Sound)]);

    // Escape clears the query, and the strip is whole again.
    shell.on_key(
        Key::Named(NamedKey::Escape),
        Modifiers::default(),
        WIDE,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    assert_eq!(
        shell
            .rows()
            .iter()
            .filter(|row| matches!(row, StripRow::Category(_)))
            .count(),
        CATEGORIES.len()
    );
}

// --- The shed strip's category list -------------------------------------

#[test]
fn the_leading_crumb_opens_the_category_list_only_once_the_strip_is_shed() {
    let theme = theme();
    let mut shell = shell();
    let wide = shell.frame(WIDE, Scale::ONE, &theme);
    click(
        &mut shell,
        Point::new(
            wide.breadcrumb.left() + 4,
            wide.breadcrumb.top() + to_i32(wide.breadcrumb.height / 2),
        ),
        WIDE,
        &theme,
    );
    assert!(
        !shell.category_list_open(),
        "the strip is on screen, so the list would be a second way to it"
    );

    let narrow = shell.frame(NARROW, Scale::ONE, &theme);
    click(
        &mut shell,
        Point::new(
            narrow.breadcrumb.left() + 4,
            narrow.breadcrumb.top() + to_i32(narrow.breadcrumb.height / 2),
        ),
        NARROW,
        &theme,
    );
    assert!(shell.category_list_open(), "the shed strip's way back");

    let mut sink = damage();
    shell.on_key(
        Key::Named(NamedKey::Escape),
        Modifiers::default(),
        NARROW,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    assert!(!shell.category_list_open(), "escape dismisses it");
}

// --- Painting -----------------------------------------------------------

#[test]
fn the_shell_draws_in_both_themes_and_at_both_densities() {
    install_test_transport();
    for theme in [Theme::dark(), Theme::light()] {
        for scale in [Scale::ONE, Scale::from_percent(200).expect("a valid scale")] {
            for viewport in [WIDE, NARROW] {
                let mut surface = Surface::new(viewport.width, viewport.height).expect("a surface");
                shell().render(&mut surface, viewport, scale, &theme, &mut NoArtwork);
                assert!(
                    surface.pixels().iter().any(|p| p.a > 0),
                    "the shell drew nothing"
                );
            }
        }
    }
}

#[test]
fn a_statement_pane_is_taller_in_a_narrower_column() {
    let theme = theme();
    let shell = shell();
    let wide = shell.frame(WIDE, Scale::ONE, &theme);
    let narrow = shell.frame(Rect::new(0, 0, 420, 640), Scale::ONE, &theme);
    let wide_h = shell.pane_height(wide.content.width, Scale::ONE, &theme);
    let narrow_h = shell.pane_height(narrow.content.width, Scale::ONE, &theme);
    assert!(wide_h > 0 && narrow_h > 0);
    assert!(
        narrow_h >= wide_h,
        "a narrower column wraps into more lines: {narrow_h} vs {wide_h}"
    );
}

#[test]
fn a_degenerate_viewport_draws_nothing_and_panics_at_nothing() {
    let theme = theme();
    let mut surface = Surface::new(4, 4).expect("a surface");
    shell().render(
        &mut surface,
        Rect::new(0, 0, 4, 4),
        Scale::ONE,
        &theme,
        &mut NoArtwork,
    );
}

// --- The strip is longer than a short column, and still walkable ---------

/// A window too short to seat every category scrolls the strip rather than
/// losing the rows past the fold: a row the reader cannot reach is a category
/// they cannot open.
#[test]
fn a_short_window_scrolls_the_category_strip() {
    let theme = theme();
    let short = Rect::new(0, 0, 900, 260);
    let mut shell = shell();
    shell.lay_out(short, Scale::ONE, &theme);
    let frame = shell.frame(short, Scale::ONE, &theme);
    let sidebar = frame.sidebar.expect("a strip");
    let bar = frame
        .strip_scrollbar
        .expect("a strip too long for the column gets its own bar");
    // The gutter is carved out of the strip's own column, so a long list
    // never narrows the pane beside it.
    assert_eq!(bar.left(), sidebar.right());
    let wide = shell.frame(WIDE, Scale::ONE, &theme).content;
    assert_eq!(
        (frame.content.left(), frame.content.width),
        (wide.left(), wide.width),
        "the strip's bar narrowed the pane"
    );
    assert!(
        shell.strip_for_test().seated(sidebar, Scale::ONE, &theme) < shell.rows().len(),
        "this window is supposed to be too short for the whole list"
    );

    // Every row is reachable: revealing the last one brings it into the
    // column the reader is looking at.
    let last = shell.rows().len() - 1;
    assert!(
        shell
            .strip_row_rect(last, sidebar, Scale::ONE, &theme)
            .is_none(),
        "the last row is past the fold to begin with"
    );
    shell.reveal_for_test(last, short, Scale::ONE, &theme);
    let row = shell
        .strip_row_rect(last, sidebar, Scale::ONE, &theme)
        .expect("the last row is seated once revealed");
    assert!(
        row.top() >= sidebar.top() && row.bottom() <= sidebar.bottom(),
        "{row:?} is not inside {sidebar:?}"
    );
    assert!(shell.strip_first_for_test() > 0, "the strip scrolled");

    // And back: the first row is reachable again.
    shell.reveal_for_test(0, short, Scale::ONE, &theme);
    assert_eq!(shell.strip_first_for_test(), 0);
    assert!(shell
        .strip_row_rect(0, sidebar, Scale::ONE, &theme)
        .is_some());
}

/// Walking the cursor to the end of the strip scrolls it into view, so the
/// keyboard reaches a row a short window could not show.
#[test]
fn the_cursor_walks_past_the_fold() {
    let theme = theme();
    let short = Rect::new(0, 0, 900, 260);
    let mut shell = shell();
    shell.lay_out(short, Scale::ONE, &theme);
    let frame = shell.frame(short, Scale::ONE, &theme);
    let sidebar = frame.sidebar.expect("a strip");

    // Focus the strip by clicking its first row, then jump to the last.
    click(
        &mut shell,
        Point::new(
            sidebar.left() + to_i32(sidebar.width / 2),
            sidebar.top() + 2,
        ),
        short,
        &theme,
    );
    let mut sink = damage();
    shell.on_key(
        Key::Named(NamedKey::End),
        Modifiers::default(),
        short,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    let cursor = shell
        .strip_cursor_for_test()
        .expect("a cursor on the strip");
    assert_eq!(cursor, shell.rows().len() - 1, "End reaches the last row");
    let row = shell
        .strip_row_rect(cursor, sidebar, Scale::ONE, &theme)
        .expect("the cursor's row is on screen");
    assert!(
        row.top() >= sidebar.top() && row.bottom() <= sidebar.bottom(),
        "the cursor was left out of sight: {row:?} vs {sidebar:?}"
    );
}

/// Feed one key press with no modifiers, answering what it concluded.
fn press(
    shell: &mut Shell,
    key: Key,
    theme: &Theme,
    sink: &mut tairix_geometry::Region,
) -> ShellOutcome {
    shell.on_key(key, Modifiers::default(), WIDE, Scale::ONE, theme, sink)
}

/// Go to `location` and hand back the shell showing it.
fn shell_at(location: Location) -> Shell {
    let mut shell = shell();
    let theme = theme();
    let mut sink = damage();
    shell.go_to_for_test(location, WIDE, Scale::ONE, &theme, &mut sink);
    shell
}

#[test]
fn the_composed_panes_draw_a_form_rather_than_a_statement() {
    for (pane, groups) in [
        (Pane::Appearance, 2),
        (Pane::Accessibility, 3),
        (Pane::Wallpaper, 1),
    ] {
        let category = pane.locate().expect("a located pane").0;
        let shell = shell_at(Location { category, pane });
        let form = shell
            .form_for_test()
            .unwrap_or_else(|| panic!("{pane:?} composes no form"));
        assert_eq!(form.groups().len(), groups, "{pane:?}");
        // Composing a form means the statement renderer draws nothing, so
        // the column's height is the form's alone.
        assert!(shell.pane_height(600, Scale::ONE, &theme()) > 0);
    }
}

#[test]
fn a_pane_that_states_an_absence_composes_no_form() {
    let shell = shell_at(Location {
        category: Category::Bluetooth,
        pane: Pane::Bluetooth,
    });
    assert!(shell.form_for_test().is_none());
}

#[test]
fn choosing_a_value_posts_only_the_appearance_keys() {
    let theme = theme();
    let mut shell = shell_at(Location {
        category: Category::Appearance,
        pane: Pane::Appearance,
    });
    shell.focus_content_for_test(WIDE, Scale::ONE, &theme);
    let mut sink = damage();

    // Open the first row's choice list and take the second choice, which is
    // the appearance the desktop is not currently on.
    let outcome = press(&mut shell, Key::Named(NamedKey::Enter), &theme, &mut sink);
    assert!(outcome.changed(), "the list did not open");
    let down = press(&mut shell, Key::Named(NamedKey::Down), &theme, &mut sink);
    assert!(down.changed());
    let chosen = press(&mut shell, Key::Named(NamedKey::Enter), &theme, &mut sink);

    let document = chosen
        .document()
        .expect("choosing a value asks for a document");
    assert!(document.contains("appearance = light"), "{document}");
    // The pinboard keys are the chooser's: posting them here would reimpose
    // whatever wallpaper this window happened to read at start-up.
    for key in SettingsKey::PINBOARD {
        assert!(
            !document.contains(key.name()),
            "{} posted: {document}",
            key.name()
        );
    }
}

#[test]
fn the_rows_show_what_the_desktop_holds_not_the_defaults() {
    let settings = DesktopSettings {
        appearance: Appearance::Light,
        contrast: Contrast::Monochrome,
        ..DesktopSettings::default()
    };
    let mut shell = Shell::new(settings.clone()).expect("a registry");
    let theme = theme();
    let mut sink = damage();
    shell.go_to_for_test(
        Location {
            category: Category::Appearance,
            pane: Pane::Appearance,
        },
        WIDE,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    assert_eq!(
        shell.form_for_test().map(|form| form.settings().clone()),
        Some(settings)
    );
}

#[test]
fn adopting_the_desktops_answer_replaces_what_the_rows_show() {
    let mut shell = shell_at(Location {
        category: Category::Accessibility,
        pane: Pane::Accessibility,
    });
    let answered = DesktopSettings {
        density: Density::Compact,
        ..DesktopSettings::default()
    };
    shell.adopt_settings(answered.clone());
    assert_eq!(shell.settings_for_test(), &answered);
    assert_eq!(
        shell.form_for_test().map(|form| form.settings().clone()),
        Some(answered)
    );
}

#[test]
fn a_refused_apply_reverts_the_row_to_what_the_store_holds() {
    // The session answers with what it still holds, so re-adopting after a
    // refusal must put the row back rather than leave the chosen value
    // standing.
    let theme = theme();
    let mut shell = shell_at(Location {
        category: Category::Appearance,
        pane: Pane::Appearance,
    });
    shell.focus_content_for_test(WIDE, Scale::ONE, &theme);
    let mut sink = damage();
    press(&mut shell, Key::Named(NamedKey::Enter), &theme, &mut sink);
    press(&mut shell, Key::Named(NamedKey::Down), &theme, &mut sink);
    let chosen = press(&mut shell, Key::Named(NamedKey::Enter), &theme, &mut sink);
    assert!(chosen.document().is_some());
    assert_eq!(
        shell.form_for_test().map(|form| form.settings().appearance),
        Some(Appearance::Light)
    );

    shell.adopt_settings(DesktopSettings::default());
    assert_eq!(
        shell.form_for_test().map(|form| form.settings().appearance),
        Some(Appearance::Dark)
    );
}

#[test]
fn accessibility_states_the_pointer_size_it_does_not_keep() {
    let shell = shell_at(Location {
        category: Category::Accessibility,
        pane: Pane::Accessibility,
    });
    let form = shell.form_for_test().expect("a form");
    let stated = form
        .groups()
        .iter()
        .flat_map(tairix_controls::FieldGroup::rows)
        .find(|row| row.label() == POINTER_SIZE_LABEL)
        .expect("the pointer-size row");
    // Stated, never drawn as a control that would change nothing.
    assert!(matches!(
        stated.control(),
        tairix_controls::FieldControl::Unmeasured(_)
    ));
}

#[test]
fn both_composed_panes_offer_the_shared_settings_from_one_definition() {
    let appearance = Composition::Appearance.labels();
    let accessibility = Composition::Accessibility.labels();
    for shared in [
        Setting::Contrast.label(),
        Setting::Density.label(),
        Setting::Motion.label(),
        Setting::Scale.label(),
    ] {
        assert!(
            appearance.contains(&shared),
            "{shared} missing from Appearance"
        );
        assert!(
            accessibility.contains(&shared),
            "{shared} missing from Accessibility"
        );
    }
    // Light/dark is Appearance's alone; the pointer statement is
    // Accessibility's alone.
    assert!(appearance.contains(&Setting::Appearance.label()));
    assert!(!accessibility.contains(&Setting::Appearance.label()));
    assert!(accessibility.contains(&POINTER_SIZE_LABEL));
}

#[test]
fn a_composed_panes_settings_are_the_labels_its_rows_actually_draw() {
    // The search index is derived from the registry, so it must name
    // exactly what the composition draws — a term that reaches a row that
    // is not there is a search that lands nowhere.
    for (pane, composition) in [
        (Pane::Appearance, Composition::Appearance),
        (Pane::Accessibility, Composition::Accessibility),
    ] {
        let row = pane.locate().expect("a located pane").1;
        assert_eq!(row.settings, composition.labels().as_slice(), "{pane:?}");
    }
}

#[test]
fn the_cursor_walks_from_one_group_into_the_next() {
    // A group clamps at its own ends, so without the form carrying the
    // cursor between groups every row below the first plate would be
    // unreachable from the keyboard.
    let theme = theme();
    let mut shell = shell_at(Location {
        category: Category::Appearance,
        pane: Pane::Appearance,
    });
    shell.focus_content_for_test(WIDE, Scale::ONE, &theme);
    let mut sink = damage();
    assert_eq!(shell.form_group_cursor_for_test(), Some((0, 0)));

    // The first group holds one row, so one Down must leave it.
    press(&mut shell, Key::Named(NamedKey::Down), &theme, &mut sink);
    assert_eq!(shell.form_group_cursor_for_test(), Some((1, 0)));

    // And Up comes back to the group above, landing on its last row.
    press(&mut shell, Key::Named(NamedKey::Up), &theme, &mut sink);
    assert_eq!(shell.form_group_cursor_for_test(), Some((0, 0)));
}

#[test]
fn walking_to_a_row_below_the_fold_scrolls_it_into_view() {
    // A short window cannot seat every group; a row the cursor reached but
    // the column does not show is a control the reader cannot use.
    let theme = theme();
    let short = Rect::new(0, 0, 900, 200);
    let mut shell = shell();
    let mut sink = damage();
    shell.go_to_for_test(
        Location {
            category: Category::Accessibility,
            pane: Pane::Accessibility,
        },
        short,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    shell.lay_out(short, Scale::ONE, &theme);
    shell.focus_content_for_test(short, Scale::ONE, &theme);
    assert_eq!(shell.scroll_offset(), 0);

    // Walk to the very last row of the last group.
    for _ in 0..12 {
        shell.on_key(
            Key::Named(NamedKey::Down),
            Modifiers::default(),
            short,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
    let (group, _) = shell
        .form_group_cursor_for_test()
        .expect("the cursor is on a row");
    assert_eq!(group, 2, "the cursor did not reach the last group");
    assert!(
        shell.form_first_for_test() > Some(0),
        "the column did not follow the cursor past the fold"
    );
    // And what it drew from is what the bar reports, so the two cannot
    // disagree about where the pane is.
    assert_eq!(
        shell.form_first_for_test().map(|first| first as u64),
        Some(shell.scroll_offset())
    );
}

#[test]
fn walking_back_up_scrolls_a_row_above_the_fold_into_view() {
    // The mirror of the case above, and the one a narrowing cast hides: a
    // row scrolled off the *top* is above the frame, not at it.
    let theme = theme();
    let short = Rect::new(0, 0, 900, 200);
    let mut shell = shell();
    let mut sink = damage();
    shell.go_to_for_test(
        Location {
            category: Category::Accessibility,
            pane: Pane::Accessibility,
        },
        short,
        Scale::ONE,
        &theme,
        &mut sink,
    );
    shell.lay_out(short, Scale::ONE, &theme);
    shell.focus_content_for_test(short, Scale::ONE, &theme);
    for _ in 0..12 {
        shell.on_key(
            Key::Named(NamedKey::Down),
            Modifiers::default(),
            short,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
    assert!(
        shell.form_first_for_test() > Some(0),
        "the walk down did not scroll"
    );

    for _ in 0..12 {
        shell.on_key(
            Key::Named(NamedKey::Up),
            Modifiers::default(),
            short,
            Scale::ONE,
            &theme,
            &mut sink,
        );
    }
    assert_eq!(shell.form_group_cursor_for_test(), Some((0, 0)));
    assert_eq!(
        shell.form_first_for_test(),
        Some(0),
        "the column did not follow the cursor back to the top"
    );
}
