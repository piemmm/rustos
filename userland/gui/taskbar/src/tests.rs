//! Headless unit tests for the taskbar layout, model, and rendering.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_proglib::{BundlePath, Catalog, DisplayName, EntryId, LibraryCategory, LibraryEntry};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Appearance, Contrast, Theme, ThemeId};

use crate::edge::{Edge, Orientation};
use crate::input::{TaskbarInput, TaskbarResponse};
use crate::layout::Hit;
use crate::library::{folder_label, LibraryFocus, LibraryRow};
use crate::notifications::IconId;
use crate::render::TaskbarRenderer;
use crate::taskbar::{Taskbar, TaskbarConfig};
use crate::tasks::{ActivateOutcome, TaskId, TaskList};

// ---- fixtures --------------------------------------------------------

/// A validated catalog entry for `/Apps/<stem>.app`.
fn entry(stem: &str, name: &str, category: LibraryCategory) -> LibraryEntry {
    LibraryEntry::new(
        EntryId::new(&format!("os.tairix.{stem}")).expect("id"),
        DisplayName::new(name).expect("name"),
        BundlePath::new(&format!("/Apps/{stem}.app")).expect("bundle"),
        category,
        None,
    )
}

/// A catalog declaring one entry per `(stem, name, category)` triple.
fn catalog(entries: &[(&str, &str, LibraryCategory)]) -> Catalog {
    let mut catalog = Catalog::new();
    for &(stem, name, category) in entries {
        catalog.insert(entry(stem, name, category)).expect("fits");
    }
    catalog
}

/// The standard fixture: two Office entries and one Games entry, so the
/// popup shows two folders (taxonomy order: Office before Games) and the
/// Office names sort as Calc before Write.
fn office_and_games() -> Catalog {
    catalog(&[
        ("write", "Write", LibraryCategory::Office),
        ("calc", "Calc", LibraryCategory::Office),
        ("chess", "Chess", LibraryCategory::Games),
    ])
}

/// A bottom-bar taskbar over the standard fixture with its popup closed.
fn bottom_bar() -> Taskbar {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    bar.library_mut().set_catalog(office_and_games());
    bar
}

/// Move the pointer to `(x, y)` and press the primary button there.
fn press_at(input: &mut TaskbarInput, taskbar: &mut Taskbar, x: i32, y: i32) -> TaskbarResponse {
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(x, y),
        },
        taskbar,
        Scale::ONE,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        taskbar,
        Scale::ONE,
    )
}

/// Press (and release) `key` with no modifiers.
fn press_key(input: &mut TaskbarInput, taskbar: &mut Taskbar, key: Key) -> TaskbarResponse {
    input.handle(
        InputEvent::KeyPressed {
            key,
            modifiers: Modifiers::default(),
        },
        taskbar,
        Scale::ONE,
    )
}

/// Open the popup by pressing the Library button, asserting it opened.
fn open_library(input: &mut TaskbarInput, taskbar: &mut Taskbar) {
    let centre = centre_of(taskbar.layout(Scale::ONE).library);
    assert_eq!(
        press_at(input, taskbar, centre.x, centre.y),
        TaskbarResponse::OpenLibrary
    );
    assert!(taskbar.library().is_open());
}

/// The centre of a non-empty rectangle.
fn centre_of(rect: Rect) -> Point {
    assert!(!rect.is_empty(), "cannot take the centre of an empty rect");
    Point::new(
        rect.left() + i32::try_from(rect.width / 2).expect("fits"),
        rect.top() + i32::try_from(rect.height / 2).expect("fits"),
    )
}

/// The row index (into the popup's rows) and screen rect of the first
/// visible row satisfying `want`.
fn visible_row_where(
    taskbar: &Taskbar,
    want: impl Fn(&LibraryRow) -> bool,
) -> Option<(usize, Rect)> {
    let layout = taskbar.library_layout(Scale::ONE);
    layout
        .rows
        .iter()
        .find(|(index, _)| taskbar.library().rows().get(*index).is_some_and(&want))
        .copied()
}

/// A dark theme with `contrast` swapped in, for accessibility renders.
fn dark_with_contrast(contrast: Contrast) -> Theme {
    let base = Theme::dark();
    Theme::new(
        ThemeId(97),
        String::from("dark-hc"),
        Appearance::Dark,
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion(),
        base.density(),
        contrast,
    )
}

// ---- edge -----------------------------------------------------------

#[test]
fn edge_orientation_and_cross_edge() {
    assert_eq!(Edge::Top.orientation(), Orientation::Horizontal);
    assert_eq!(Edge::Bottom.orientation(), Orientation::Horizontal);
    assert_eq!(Edge::Left.orientation(), Orientation::Vertical);
    assert_eq!(Edge::Right.orientation(), Orientation::Vertical);

    assert!(!Edge::Top.at_trailing_cross_edge());
    assert!(Edge::Bottom.at_trailing_cross_edge());
    assert!(!Edge::Left.at_trailing_cross_edge());
    assert!(Edge::Right.at_trailing_cross_edge());
}

// ---- task list ------------------------------------------------------

#[test]
fn tasks_add_unfocused_and_reject_duplicates() {
    let mut tasks = TaskList::new();
    assert!(tasks.add(TaskId(1), "Editor"));
    assert!(!tasks.add(TaskId(1), "Imposter"));
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks.focused(), None);
}

#[test]
fn task_activate_focuses_then_minimises() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    assert_eq!(tasks.activate(TaskId(1)), ActivateOutcome::Activated);
    assert_eq!(tasks.focused(), Some(TaskId(1)));
    assert_eq!(tasks.activate(TaskId(1)), ActivateOutcome::Minimised);
    assert!(tasks.is_minimised(TaskId(1)));
    assert_eq!(tasks.focused(), None);
    assert_eq!(tasks.activate(TaskId(9)), ActivateOutcome::Unknown);
}

#[test]
fn task_remove_clears_focus() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.activate(TaskId(1));
    assert!(tasks.remove(TaskId(1)));
    assert_eq!(tasks.focused(), None);
    assert!(tasks.is_empty());
}

#[test]
fn set_focused_mirrors_and_fails_closed() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    assert!(tasks.set_focused(Some(TaskId(1))));
    assert!(!tasks.set_focused(Some(TaskId(9))), "unknown id refused");
    assert_eq!(tasks.focused(), Some(TaskId(1)));
    assert!(tasks.set_focused(None));
    assert_eq!(tasks.focused(), None);
}

// ---- notifications --------------------------------------------------

#[test]
fn notifications_add_remove_and_dedup() {
    let mut bar = bottom_bar();
    assert!(bar.notifications_mut().add(IconId(1), "icon.network"));
    assert!(!bar.notifications_mut().add(IconId(1), "icon.volume"));
    assert_eq!(bar.notifications().len(), 1);
    assert!(bar.notifications_mut().remove(IconId(1)));
    assert!(bar.notifications().is_empty());
}

// ---- bar layout -----------------------------------------------------

#[test]
fn leading_launchers_partition_the_leading_end() {
    let bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library, Rect::new(0, 760, 48, 40));
    assert_eq!(layout.files, Rect::new(48, 760, 48, 40));
    assert_eq!(layout.task_list.left(), 96);
    assert_eq!(layout.clock.right(), 1000);
}

#[test]
fn hit_testing_resolves_every_region() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.notifications_mut().add(IconId(7), "icon.network");
    let layout = bar.layout(Scale::ONE);

    assert_eq!(
        bar.hit_test(Point::new(10, 780), Scale::ONE),
        Some(Hit::Library)
    );
    assert_eq!(
        bar.hit_test(Point::new(60, 780), Scale::ONE),
        Some(Hit::Files)
    );
    assert_eq!(
        bar.hit_test(centre_of(layout.tasks[0]), Scale::ONE),
        Some(Hit::Task(0))
    );
    assert_eq!(
        bar.hit_test(centre_of(layout.notifications[0]), Scale::ONE),
        Some(Hit::Notification(0))
    );
    assert_eq!(
        bar.hit_test(centre_of(layout.clock), Scale::ONE),
        Some(Hit::Clock)
    );
    assert_eq!(bar.hit_test(Point::new(500, 100), Scale::ONE), None);
    // A gap between the last task slot and the notification area is the
    // bare bar: inside the bar, on no region.
    assert_eq!(bar.hit_test(Point::new(500, 780), Scale::ONE), None);
}

#[test]
fn both_launchers_hit_on_every_edge() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let bar = Taskbar::new(config, &Theme::dark());
        let layout = bar.layout(Scale::ONE);
        assert_eq!(
            layout.hit_test(centre_of(layout.library)),
            Some(Hit::Library),
            "{edge:?}"
        );
        assert_eq!(
            layout.hit_test(centre_of(layout.files)),
            Some(Hit::Files),
            "{edge:?}"
        );
        assert!(
            layout.bar.contains(centre_of(layout.library)),
            "{edge:?}: launcher lies on the bar"
        );
    }
}

#[test]
fn vertical_bar_stacks_launchers_downward() {
    let config = TaskbarConfig {
        edge: Edge::Left,
        ..TaskbarConfig::bottom_bar(1000, 800)
    };
    let bar = Taskbar::new(config, &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.bar, Rect::new(0, 0, 40, 800));
    assert_eq!(layout.library, Rect::new(0, 0, 40, 48));
    assert_eq!(layout.files, Rect::new(0, 48, 40, 48));
}

#[test]
fn launchers_clip_fail_closed_on_a_tiny_screen() {
    // Room for the Library button and only part of Files.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(60, 50), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library.width, 48);
    assert_eq!(layout.files.width, 12, "Files clips to what fits");

    // No room for Files at all: its rect is empty and can never be hit.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(30, 50), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library.width, 30);
    assert!(layout.files.is_empty());
    assert_eq!(layout.hit_test(Point::new(20, 30)), Some(Hit::Library));

    // A zero-sized screen yields empty regions and no hits anywhere.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(0, 0), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert!(layout.library.is_empty());
    assert!(layout.files.is_empty());
    assert_eq!(layout.hit_test(Point::new(0, 0)), None);
}

#[test]
fn overflowing_task_slot_is_clipped_to_empty() {
    let mut bar = bottom_bar();
    for index in 0..10 {
        bar.tasks_mut().add(TaskId(index), "Task");
    }
    let layout = bar.layout(Scale::ONE);
    assert!(layout.tasks[0].width > 0);
    assert!(
        layout.tasks.last().expect("ten slots").is_empty(),
        "a slot past the region clips to empty"
    );
}

#[test]
fn bar_pins_to_all_four_edges() {
    for (edge, expect) in [
        (Edge::Top, Rect::new(0, 0, 1000, 40)),
        (Edge::Bottom, Rect::new(0, 760, 1000, 40)),
        (Edge::Left, Rect::new(0, 0, 40, 800)),
        (Edge::Right, Rect::new(960, 0, 40, 800)),
    ] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let bar = Taskbar::new(config, &Theme::dark());
        assert_eq!(bar.layout(Scale::ONE).bar, expect, "{edge:?}");
    }
}

// ---- DPI / scale ----------------------------------------------------

#[test]
fn doubling_the_scale_doubles_logical_lengths() {
    let bar = bottom_bar();
    let one = bar.layout(Scale::ONE);
    let two = bar.layout(Scale::from_percent(200).expect("a valid scale"));
    assert_eq!(two.library.width, one.library.width * 2);
    assert_eq!(two.files.width, one.files.width * 2);
    assert_eq!(two.bar.height, one.bar.height * 2);
    assert_eq!(two.corner_radius, one.corner_radius * 2);
    // The physical screen is unchanged, so the doubled bar still spans it.
    assert_eq!(two.bar.width, 1000);
}

#[test]
fn hit_testing_follows_the_scale() {
    let bar = bottom_bar();
    let scale = Scale::from_percent(200).expect("a valid scale");
    // At 2x the Library button spans 96 physical pixels.
    assert_eq!(bar.hit_test(Point::new(90, 780), scale), Some(Hit::Library));
    assert_eq!(bar.hit_test(Point::new(100, 780), scale), Some(Hit::Files));
}

// ---- theming --------------------------------------------------------

#[test]
fn corner_radius_comes_from_the_theme() {
    let bar = bottom_bar();
    assert_eq!(
        bar.corner_radius(),
        Theme::dark().metrics().taskbar_corner_radius
    );
    assert_eq!(bar.layout(Scale::ONE).corner_radius, bar.corner_radius());
}

#[test]
fn apply_theme_swaps_the_owned_theme_and_latches_a_repaint() {
    let mut bar = bottom_bar();
    assert!(!bar.take_repaint(), "a fresh bar has nothing pending");
    bar.apply_theme(&Theme::light());
    assert_eq!(bar.theme().id(), Theme::light().id());
    assert!(bar.take_repaint(), "a theme switch needs a repaint");
    assert!(!bar.take_repaint(), "taking the latch clears it");
}

// ---- input: bar ------------------------------------------------------

#[test]
fn library_press_opens_the_popup() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    assert_eq!(bar.library().focus(), LibraryFocus::Search);
    assert_eq!(bar.library().search_text(), "");
}

#[test]
fn library_press_toggles_the_popup_shut() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    assert_eq!(
        press_at(&mut input, &mut bar, 10, 780),
        TaskbarResponse::LibraryDismissed
    );
    assert!(!bar.library().is_open());
}

#[test]
fn files_press_reports_open_files() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    assert_eq!(
        press_at(&mut input, &mut bar, 60, 780),
        TaskbarResponse::OpenFiles
    );
}

#[test]
fn task_press_applies_the_activate_rule() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).tasks[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated,
        }
    );
}

#[test]
fn notification_and_clock_presses_report() {
    let mut bar = bottom_bar();
    bar.notifications_mut().add(IconId(3), "icon.volume");
    let mut input = TaskbarInput::new();
    let layout = bar.layout(Scale::ONE);
    let icon = centre_of(layout.notifications[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, icon.x, icon.y),
        TaskbarResponse::NotificationActivated { id: IconId(3) }
    );
    let clock = centre_of(layout.clock);
    assert_eq!(
        press_at(&mut input, &mut bar, clock.x, clock.y),
        TaskbarResponse::ClockPressed
    );
}

#[test]
fn a_miss_and_non_primary_input_change_nothing() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    assert_eq!(
        press_at(&mut input, &mut bar, 500, 400),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Char('x')),
        TaskbarResponse::Ignored,
        "keys route to the bar only while the popup is open"
    );
    assert!(!bar.library().is_open());
}

#[test]
fn motion_tracks_the_pointer_and_latches_hover_changes() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    bar.take_repaint();

    // Entering the Library button changes its hover state: repaint.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(10, 780),
        },
        &mut bar,
        Scale::ONE,
    );
    assert_eq!(input.pointer(), Point::new(10, 780));
    assert!(bar.take_repaint(), "hover enter repaints");

    // Moving within the same button changes nothing.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(20, 780),
        },
        &mut bar,
        Scale::ONE,
    );
    assert!(!bar.take_repaint(), "no visual change, no repaint");

    // Leaving it changes its hover state back: repaint.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(500, 400),
        },
        &mut bar,
        Scale::ONE,
    );
    assert!(bar.take_repaint(), "hover exit repaints");
}

// ---- input: open popup ----------------------------------------------

#[test]
fn click_away_dismisses_without_acting_on_what_it_hit() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Press on a task slot while the popup is open: one click does one
    // thing — the popup closes and the task is NOT activated.
    let slot = centre_of(bar.layout(Scale::ONE).tasks[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::LibraryDismissed
    );
    assert!(!bar.library().is_open());
    assert_eq!(bar.tasks().focused(), None, "the task was not activated");
}

#[test]
fn secondary_press_outside_dismisses_and_inside_is_claimed() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let panel = bar.library_layout(Scale::ONE).panel;

    // Inside the panel: claimed by the modal popup, which has no context
    // actions yet — nothing happens, the popup stays.
    let inside = centre_of(panel);
    input.handle(
        InputEvent::PointerMoved { to: inside },
        &mut bar,
        Scale::ONE,
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert!(bar.library().is_open());

    // Outside: dismisses, exactly like a primary click-away.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(500, 100),
        },
        &mut bar,
        Scale::ONE,
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::LibraryDismissed
    );
    assert!(!bar.library().is_open());
}

#[test]
fn clicking_an_entry_launches_it() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    let (index, rect) = visible_row_where(
        &bar,
        |row| matches!(row, LibraryRow::Entry { name, .. } if name == "Chess"),
    )
    .expect("Chess is visible");
    let LibraryRow::Entry { id, .. } = bar.library().rows()[index].clone() else {
        panic!("expected an entry row");
    };

    let centre = centre_of(rect);
    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::LibraryLaunch { entry: id }
    );
    assert!(!bar.library().is_open(), "a launch closes the popup");
}

#[test]
fn clicking_a_folder_toggles_its_expansion() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let all_rows = bar.library().rows().len();
    assert_eq!(all_rows, 5, "two folders and three entries");

    let (index, rect) = visible_row_where(&bar, |row| {
        matches!(row, LibraryRow::Folder { category, .. } if *category == LibraryCategory::Office)
    })
    .expect("Office folder is visible");
    assert_eq!(index, 0);

    let centre = centre_of(rect);
    bar.take_repaint();
    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::Ignored,
        "a fold is the popup's own state change"
    );
    assert!(bar.take_repaint(), "the fold repaints");
    assert!(bar.library().is_open());
    assert_eq!(
        bar.library().rows().len(),
        3,
        "Office collapsed: its two entries left the list"
    );
    assert!(matches!(
        bar.library().rows()[0],
        LibraryRow::Folder {
            category: LibraryCategory::Office,
            expanded: false,
            count: 2,
        }
    ));

    // Clicking it again expands it back.
    let (_, rect) = visible_row_where(&bar, |row| {
        matches!(row, LibraryRow::Folder { category, .. } if *category == LibraryCategory::Office)
    })
    .expect("Office folder is still visible");
    let centre = centre_of(rect);
    press_at(&mut input, &mut bar, centre.x, centre.y);
    assert_eq!(bar.library().rows().len(), 5);
}

#[test]
fn wheel_scrolls_the_overflowing_popup() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 300), &Theme::dark());
    let mut entries: Vec<(String, String)> = Vec::new();
    for index in 0..30 {
        entries.push((format!("app{index:02}"), format!("App {index:02}")));
    }
    let mut cat = Catalog::new();
    for (stem, name) in &entries {
        cat.insert(entry(stem, name, LibraryCategory::Utilities))
            .expect("fits");
    }
    bar.library_mut().set_catalog(cat);

    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let layout = bar.library_layout(Scale::ONE);
    assert!(
        layout.visible_rows < bar.library().rows().len(),
        "the fixture overflows the viewport"
    );
    assert!(layout.scrollbar.is_some(), "an overflow shows a scrollbar");
    assert_eq!(layout.rows[0].0, 0);

    bar.take_repaint();
    assert_eq!(
        input.handle(
            InputEvent::PointerScrolled { dx: 0, dy: 1 },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert!(bar.take_repaint(), "a scroll repaints");
    let scrolled = bar.library_layout(Scale::ONE);
    assert_eq!(scrolled.rows[0].0, 1, "the viewport moved down one row");

    // The panel never grows past the space between the bar and the screen
    // edge: it fits entirely above the bar.
    assert!(scrolled.panel.top() >= 0);
    assert!(scrolled.panel.bottom() <= bar.layout(Scale::ONE).bar.top());
}

// ---- popup model ----------------------------------------------------

#[test]
fn rows_follow_taxonomy_order_and_sort_entries_by_name() {
    let bar = bottom_bar();
    let rows = bar.library().rows();
    assert!(matches!(
        rows[0],
        LibraryRow::Folder {
            category: LibraryCategory::Office,
            expanded: true,
            count: 2,
        }
    ));
    assert!(matches!(&rows[1], LibraryRow::Entry { name, .. } if name == "Calc"));
    assert!(matches!(&rows[2], LibraryRow::Entry { name, .. } if name == "Write"));
    assert!(matches!(
        rows[3],
        LibraryRow::Folder {
            category: LibraryCategory::Games,
            expanded: true,
            count: 1,
        }
    ));
    assert!(matches!(&rows[4], LibraryRow::Entry { name, .. } if name == "Chess"));
}

#[test]
fn empty_folders_are_hidden() {
    let bar = bottom_bar();
    assert!(
        !bar.library().rows().iter().any(|row| matches!(
            row,
            LibraryRow::Folder { category, .. } if *category == LibraryCategory::Internet
        )),
        "a folder with no entries is never listed"
    );
}

#[test]
fn every_folder_has_a_label() {
    for category in LibraryCategory::ALL {
        assert!(!folder_label(category).is_empty());
    }
    assert_eq!(folder_label(LibraryCategory::SystemTools), "System Tools");
}

#[test]
fn an_empty_catalog_shows_the_calm_placeholder() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    bar.library_mut().set_catalog(Catalog::new());
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    assert!(bar.library().rows().is_empty());
    assert_eq!(
        bar.library().placeholder(),
        Some("No programs are catalogued")
    );
    // The popup still lays out sanely: chrome plus an empty viewport.
    let layout = bar.library_layout(Scale::ONE);
    assert!(layout.panel.width > 0 && layout.panel.height > 0);
    assert!(layout.rows.is_empty());
}

#[test]
fn reopening_resets_search_expansion_and_cursor() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Filter, collapse-by-navigation, and move the cursor…
    press_key(&mut input, &mut bar, Key::Char('c'));
    assert_eq!(bar.library().search_text(), "c");

    // …then close and reopen: everything is back at the deterministic
    // opening state.
    press_at(&mut input, &mut bar, 500, 100);
    open_library(&mut input, &mut bar);
    assert_eq!(bar.library().search_text(), "");
    assert_eq!(bar.library().rows().len(), 5);
    assert_eq!(bar.library().current(), None);
    assert_eq!(bar.library().focus(), LibraryFocus::Search);
}

// ---- popup keyboard --------------------------------------------------

#[test]
fn typing_filters_case_insensitively_and_enter_launches_first_match() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    press_key(&mut input, &mut bar, Key::Char('c'));
    let names: Vec<&str> = bar
        .library()
        .rows()
        .iter()
        .map(|row| match row {
            LibraryRow::Entry { name, .. } => name.as_str(),
            LibraryRow::Folder { .. } => panic!("a filter lists entries only"),
        })
        .collect();
    assert_eq!(names, ["Calc", "Chess"], "matches sort by name");

    press_key(&mut input, &mut bar, Key::Char('h'));
    assert_eq!(bar.library().search_text(), "ch");
    assert_eq!(bar.library().rows().len(), 1, "only Chess matches 'ch'");

    let response = press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter));
    let TaskbarResponse::LibraryLaunch { entry } = response else {
        panic!("Enter launches the first match, got {response:?}");
    };
    assert_eq!(entry.as_str(), "os.tairix.chess");
    assert!(!bar.library().is_open());
}

#[test]
fn a_filter_matching_nothing_shows_the_no_match_placeholder() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    press_key(&mut input, &mut bar, Key::Char('z'));
    assert!(bar.library().rows().is_empty());
    assert_eq!(bar.library().placeholder(), Some("No matching programs"));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::Ignored,
        "nothing to launch"
    );
}

#[test]
fn escape_clears_the_search_then_dismisses() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    press_key(&mut input, &mut bar, Key::Char('c'));
    assert_eq!(bar.library().search_text(), "c");

    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape)),
        TaskbarResponse::Ignored,
        "the first Escape only clears the filter"
    );
    assert_eq!(bar.library().search_text(), "");
    assert!(bar.library().is_open());
    assert_eq!(bar.library().rows().len(), 5, "the full list is back");

    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape)),
        TaskbarResponse::LibraryDismissed
    );
    assert!(!bar.library().is_open());
}

#[test]
fn arrows_move_the_cursor_and_wrap() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(bar.library().focus(), LibraryFocus::Rows);
    assert_eq!(bar.library().current(), Some(0));

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(bar.library().current(), Some(1));

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Up));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Up));
    assert_eq!(bar.library().current(), Some(4), "Up from the top wraps");

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(bar.library().current(), Some(0), "Down from the end wraps");

    press_key(&mut input, &mut bar, Key::Named(NamedKey::End));
    assert_eq!(bar.library().current(), Some(4));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Home));
    assert_eq!(bar.library().current(), Some(0));
}

#[test]
fn enter_activates_the_cursor_row() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Cursor to the Office folder header; Enter collapses it.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter));
    assert_eq!(bar.library().rows().len(), 3);
    assert_eq!(
        bar.library().current(),
        Some(0),
        "the cursor stays on the folder it folded"
    );

    // Enter again expands; then walk to Chess and launch it.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter));
    assert_eq!(bar.library().rows().len(), 5);
    press_key(&mut input, &mut bar, Key::Named(NamedKey::End));
    let response = press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter));
    let TaskbarResponse::LibraryLaunch { entry } = response else {
        panic!("Enter on an entry launches it, got {response:?}");
    };
    assert_eq!(entry.as_str(), "os.tairix.chess");
}

#[test]
fn left_and_right_fold_climb_and_descend() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Cursor onto the Calc entry (row 1); Left climbs to its folder.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(bar.library().current(), Some(1));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Left));
    assert_eq!(
        bar.library().current(),
        Some(0),
        "Left climbs to the folder"
    );

    // Left on the expanded folder collapses it; Right expands it again;
    // a second Right steps into its first entry.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Left));
    assert!(matches!(
        bar.library().rows()[0],
        LibraryRow::Folder {
            expanded: false,
            ..
        }
    ));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Right));
    assert!(matches!(
        bar.library().rows()[0],
        LibraryRow::Folder { expanded: true, .. }
    ));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Right));
    assert_eq!(bar.library().current(), Some(1), "Right steps to the child");
}

#[test]
fn tab_cycles_focus_and_typing_returns_to_the_search() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    assert_eq!(bar.library().focus(), LibraryFocus::Search);

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Tab));
    assert_eq!(bar.library().focus(), LibraryFocus::Rows);
    assert_eq!(bar.library().current(), Some(0));

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Tab));
    assert_eq!(bar.library().focus(), LibraryFocus::Search);

    // From row focus, typing routes into the search (type-to-filter).
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Tab));
    assert_eq!(bar.library().focus(), LibraryFocus::Rows);
    press_key(&mut input, &mut bar, Key::Char('w'));
    assert_eq!(bar.library().focus(), LibraryFocus::Search);
    assert_eq!(bar.library().search_text(), "w");
    let names: Vec<&str> = bar
        .library()
        .rows()
        .iter()
        .filter_map(|row| match row {
            LibraryRow::Entry { name, .. } => Some(name.as_str()),
            LibraryRow::Folder { .. } => None,
        })
        .collect();
    assert_eq!(names, ["Write"]);
}

#[test]
fn keyboard_navigation_keeps_the_cursor_visible() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 300), &Theme::dark());
    let mut cat = Catalog::new();
    for index in 0..30 {
        cat.insert(entry(
            &format!("app{index:02}"),
            &format!("App {index:02}"),
            LibraryCategory::Utilities,
        ))
        .expect("fits");
    }
    bar.library_mut().set_catalog(cat);
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let visible = bar.library_layout(Scale::ONE).visible_rows;
    assert!(visible >= 2, "the fixture screen holds a few rows");

    // Walk the cursor past the bottom of the viewport: the view follows.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    for _ in 0..visible {
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    }
    let layout = bar.library_layout(Scale::ONE);
    let cursor = bar.library().current().expect("cursor placed");
    assert!(
        layout.rows.iter().any(|&(index, _)| index == cursor),
        "the cursor row stays visible"
    );

    // PageDown / PageUp move by a viewport, clamped to the ends.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::PageDown));
    let after_page = bar.library().current().expect("cursor placed");
    assert!(after_page > cursor);
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Home));
    assert_eq!(bar.library().current(), Some(0));
    assert_eq!(
        bar.library_layout(Scale::ONE).rows[0].0,
        0,
        "view followed home"
    );
}

// ---- popup layout ----------------------------------------------------

#[test]
fn popup_opens_outward_on_every_edge() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let mut bar = Taskbar::new(config, &Theme::dark());
        bar.library_mut().set_catalog(office_and_games());
        let mut input = TaskbarInput::new();
        open_library(&mut input, &mut bar);

        let bar_rect = bar.layout(Scale::ONE).bar;
        let panel = bar.library_layout(Scale::ONE).panel;
        assert!(!panel.is_empty(), "{edge:?}");
        match edge {
            Edge::Top => assert_eq!(panel.top(), bar_rect.bottom(), "{edge:?}"),
            Edge::Bottom => assert_eq!(panel.bottom(), bar_rect.top(), "{edge:?}"),
            Edge::Left => assert_eq!(panel.left(), bar_rect.right(), "{edge:?}"),
            Edge::Right => assert_eq!(panel.right(), bar_rect.left(), "{edge:?}"),
        }
        // The panel stays on screen.
        assert!(panel.left() >= 0 && panel.top() >= 0, "{edge:?}");
        assert!(panel.right() <= 1000 && panel.bottom() <= 800, "{edge:?}");
    }
}

#[test]
fn popup_scales_with_the_desktop_density() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let one = bar.library_layout(Scale::ONE);
    let two = bar.library_layout(Scale::from_percent(200).expect("a valid scale"));
    assert_eq!(two.panel.width, one.panel.width * 2);
    assert_eq!(two.search.height, one.search.height * 2);
    assert_eq!(two.corner_radius, one.corner_radius * 2);
}

#[test]
fn popup_rows_hit_test_to_their_indices() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let layout = bar.library_layout(Scale::ONE);
    for &(index, rect) in &layout.rows {
        assert_eq!(layout.row_at(centre_of(rect)), Some(index));
    }
    assert_eq!(layout.row_at(Point::new(-5, -5)), None);
    // Entry rows are indented beneath their folder header.
    let folder = layout.rows[0].1;
    let entry_rect = layout.rows[1].1;
    assert!(entry_rect.left() > folder.left());
}

#[test]
fn anchor_points_back_at_the_library_button() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let bar_layout = bar.layout(Scale::ONE);
    let layout = bar.library_layout(Scale::ONE);
    assert_eq!(layout.anchor, centre_of(bar_layout.library));
}

// ---- rendering ------------------------------------------------------

/// The premultiplied pixel a theme palette role paints as.
fn role(color: tairix_theme::Rgba) -> Pixel {
    Color::from(color).premultiply()
}

/// The painted pixel at screen point `(x, y)`, translated into the
/// surface's local space via its screen-space `frame`.
fn pixel_at(surface: &Surface, frame: Rect, x: i32, y: i32) -> Pixel {
    let lx = u32::try_from(x - frame.left()).expect("point is right of the frame origin");
    let ly = u32::try_from(y - frame.top()).expect("point is below the frame origin");
    surface.get(lx, ly).expect("point lies inside the frame")
}

/// Whether any pixel inside screen-space `region` was painted `want`.
fn region_has_pixel(surface: &Surface, frame: Rect, region: Rect, want: Pixel) -> bool {
    (region.top()..region.bottom())
        .any(|y| (region.left()..region.right()).any(|x| pixel_at(surface, frame, x, y) == want))
}

/// Whether `region` shows anti-aliased ink of the `want` role composited
/// over `background`: some pixel is a coverage blend of the role over the
/// background (partial *or* full coverage). An anti-aliased glyph edge — or
/// any thin stroke resampled for a non-native DPI scale — may never reach
/// full role coverage, so an exact-role match ([`region_has_pixel`]) is too
/// strict for "a label was drawn here"; a blend on the `background`→role
/// segment is the faithful check.
fn region_has_role_ink(
    surface: &Surface,
    frame: Rect,
    region: Rect,
    want: tairix_theme::Rgba,
    background: Pixel,
) -> bool {
    let fg = role(want);
    (region.top()..region.bottom()).any(|y| {
        (region.left()..region.right())
            .any(|x| is_coverage_blend(pixel_at(surface, frame, x, y), fg, background))
    })
}

/// Whether opaque pixel `p` is a coverage blend of opaque `fg` over opaque
/// `bg` — each channel lies on the `bg`→`fg` segment (±1 for rounding) and
/// `p` differs from the bare background.
fn is_coverage_blend(p: Pixel, fg: Pixel, bg: Pixel) -> bool {
    fn on_segment(value: u8, from: u8, to: u8) -> bool {
        let lo = from.min(to).saturating_sub(1);
        let hi = from.max(to).saturating_add(1);
        value >= lo && value <= hi
    }
    p.a == 255
        && p != bg
        && on_segment(p.r, bg.r, fg.r)
        && on_segment(p.g, bg.g, fg.g)
        && on_segment(p.b, bg.b, fg.b)
}

#[test]
fn rendered_surface_matches_bar_dimensions() {
    let bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert_eq!(surface.width(), layout.bar.width);
    assert_eq!(surface.height(), layout.bar.height);
}

#[test]
fn background_is_the_raised_surface_colour() {
    let bar = bottom_bar();
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert_eq!(
        pixel_at(&surface, bar.layout(Scale::ONE).bar, 500, 780),
        role(Theme::dark().palette().surface_raised)
    );
}

#[test]
fn library_button_paints_the_accent_plate_and_files_stays_quiet() {
    let bar = bottom_bar();
    let theme = Theme::dark();
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    let frame = bar.layout(Scale::ONE).bar;
    let layout = bar.layout(Scale::ONE);

    // The Library button is the accent-filled primary invoker; its plate
    // colour appears inside its slot.
    assert!(region_has_pixel(
        &surface,
        frame,
        layout.library,
        role(theme.palette().accent)
    ));
    // The quiet Files button draws no accent; its folder glyph inks the
    // ordinary foreground over the raised plate.
    assert!(!region_has_pixel(
        &surface,
        frame,
        layout.files,
        role(theme.palette().accent)
    ));
    assert!(region_has_role_ink(
        &surface,
        frame,
        layout.files,
        theme.palette().on_surface,
        role(theme.palette().surface_raised),
    ));
}

#[test]
fn focused_task_is_accent_and_others_are_surface() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().activate(TaskId(1));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    let focused = centre_of(layout.tasks[0]);
    let other = centre_of(layout.tasks[1]);
    assert_eq!(
        pixel_at(&surface, layout.bar, focused.x, focused.y),
        role(theme.palette().accent)
    );
    assert_eq!(
        pixel_at(&surface, layout.bar, other.x, other.y),
        role(theme.palette().surface)
    );
}

#[test]
fn minimised_task_recedes_into_the_bar() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().activate(TaskId(1));
    bar.tasks_mut().minimise(TaskId(1));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    let slot = centre_of(layout.tasks[0]);
    assert_eq!(
        pixel_at(&surface, layout.bar, slot.x, slot.y),
        role(theme.palette().surface_raised)
    );
}

#[test]
fn notification_glyph_draws_in_the_muted_role() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.notifications_mut().add(IconId(1), "icon.network");
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert!(region_has_role_ink(
        &surface,
        layout.bar,
        layout.notifications[0],
        theme.palette().on_surface_muted,
        role(theme.palette().surface_raised),
    ));
}

#[test]
fn unknown_notification_asset_falls_back_to_a_glyph() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.notifications_mut().add(IconId(1), "no-such-asset");
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_has_role_ink(
            &surface,
            layout.bar,
            layout.notifications[0],
            theme.palette().on_surface_muted,
            role(theme.palette().surface_raised),
        ),
        "an unknown asset still draws the placeholder glyph"
    );
}

#[test]
fn theme_switch_repaints_the_bar() {
    let mut bar = bottom_bar();
    let mut renderer = TaskbarRenderer::new();
    let dark = renderer.render(&bar, Scale::ONE).expect("bar renders");
    bar.apply_theme(&Theme::light());
    let light = renderer.render(&bar, Scale::ONE).expect("bar renders");
    let frame = bar.layout(Scale::ONE).bar;
    assert_ne!(
        pixel_at(&dark, frame, 500, 780),
        pixel_at(&light, frame, 500, 780),
        "the bar background follows the palette"
    );
}

#[test]
fn clock_label_paints_and_an_empty_clock_paints_nothing() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    let empty = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert!(
        !region_has_role_ink(
            &empty,
            layout.bar,
            layout.clock,
            theme.palette().on_surface,
            role(theme.palette().surface_raised),
        ),
        "an unset clock draws nothing"
    );

    bar.clock_mut().set_label("12:34");
    let drawn = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert!(region_has_role_ink(
        &drawn,
        layout.bar,
        layout.clock,
        theme.palette().on_surface,
        role(theme.palette().surface_raised),
    ));
}

#[test]
fn long_task_title_never_spills_its_slot() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.tasks_mut()
        .add(TaskId(1), "An enormously long window title that cannot fit");
    bar.tasks_mut().add(TaskId(2), "Next");
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    // The neighbouring slot's fill is untouched by the first task's text:
    // its own centre is the plain surface colour.
    let next = centre_of(layout.tasks[1]);
    assert_eq!(
        pixel_at(&surface, layout.bar, next.x, next.y),
        role(theme.palette().surface)
    );
}

// ---- rendering: the popup -------------------------------------------

#[test]
fn render_library_is_none_while_closed() {
    let bar = bottom_bar();
    assert!(TaskbarRenderer::new()
        .render_library(&bar, Scale::ONE)
        .is_none());
}

#[test]
fn open_popup_renders_panel_rows_and_search() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    let layout = bar.library_layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    assert_eq!(surface.width(), layout.panel.width);
    assert_eq!(surface.height(), layout.panel.height);

    // The panel's content region is the plain surface colour…
    let viewport_gap = Point::new(layout.viewport.left(), layout.viewport.bottom() - 1);
    assert_eq!(
        pixel_at(&surface, layout.panel, viewport_gap.x, viewport_gap.y),
        role(theme.palette().surface)
    );
    // …the first folder row inks its label…
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.rows[0].1,
        theme.palette().on_surface,
        role(theme.palette().surface),
    ));
    // …and the search row paints its field plate distinct from the panel.
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        layout.search,
        role(theme.palette().surface_raised)
    ));
}

#[test]
fn hovered_and_current_rows_show_their_states() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Hover the second row with the pointer.
    let hover_rect = bar.library_layout(Scale::ONE).rows[1].1;
    let hover_centre = centre_of(hover_rect);
    input.handle(
        InputEvent::PointerMoved { to: hover_centre },
        &mut bar,
        Scale::ONE,
    );
    assert_eq!(bar.library().hover(), Some(1));

    // Put the keyboard cursor on the first row.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(bar.library().current(), Some(0));

    let layout = bar.library_layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");

    // The hovered row raises its fill.
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        layout.rows[1].1,
        role(theme.palette().surface_raised)
    ));
    // The current (selected) row draws the accent selection rail in its
    // leading gutter.
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        layout.rows[0].1,
        role(theme.palette().accent)
    ));
}

#[test]
fn empty_library_renders_the_placeholder_ink() {
    let theme = Theme::dark();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    bar.library_mut().set_catalog(Catalog::new());
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    let layout = bar.library_layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.viewport,
        theme.palette().on_surface_muted,
        role(theme.palette().surface),
    ));
}

#[test]
fn overflowing_popup_paints_its_scrollbar() {
    let theme = Theme::dark();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 300), &Theme::dark());
    let mut cat = Catalog::new();
    for index in 0..30 {
        cat.insert(entry(
            &format!("app{index:02}"),
            &format!("App {index:02}"),
            LibraryCategory::Utilities,
        ))
        .expect("fits");
    }
    bar.library_mut().set_catalog(cat);
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    let layout = bar.library_layout(Scale::ONE);
    let scrollbar = layout.scrollbar.expect("an overflow shows a scrollbar");
    let surface = TaskbarRenderer::new()
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        scrollbar,
        role(theme.palette().scroll_track)
    ));
}

#[test]
fn popup_renders_under_both_themes_and_high_contrast() {
    for theme in [
        Theme::dark(),
        Theme::light(),
        dark_with_contrast(Contrast::High),
    ] {
        let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
        bar.library_mut().set_catalog(office_and_games());
        let mut input = TaskbarInput::new();
        open_library(&mut input, &mut bar);

        let layout = bar.library_layout(Scale::ONE);
        let surface = TaskbarRenderer::new()
            .render_library(&bar, Scale::ONE)
            .expect("popup renders");
        assert_eq!(surface.width(), layout.panel.width);
        assert!(
            region_has_role_ink(
                &surface,
                layout.panel,
                layout.rows[0].1,
                theme.palette().on_surface,
                role(theme.palette().surface),
            ),
            "{} paints its rows",
            theme.name()
        );
    }
}

#[test]
fn popup_repaints_after_a_theme_switch() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let renderer = TaskbarRenderer::new();
    let dark = renderer
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    bar.apply_theme(&Theme::light());
    let light = renderer
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    let layout = bar.library_layout(Scale::ONE);
    let inside = Point::new(layout.viewport.left(), layout.viewport.bottom() - 1);
    assert_ne!(
        pixel_at(&dark, layout.panel, inside.x, inside.y),
        pixel_at(&light, layout.panel, inside.x, inside.y)
    );
}
