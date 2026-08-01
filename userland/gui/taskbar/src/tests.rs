//! Headless unit tests for the taskbar layout, model, and rendering.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{
    TrayPermille, TrayPressure, TrayPressureCount, TrayPressureKind, TraySummary, TrayTask,
    TrayTaskName,
};
use tairix_controls::{
    ActivityState, ControlState, PressureKind, PressureState, RecoveryState, Section,
    TaskVisibility, TrayBadgeContent, TrayBadgeTone,
};
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_proglib::{BundlePath, Catalog, DisplayName, EntryId, LibraryCategory, LibraryEntry};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Appearance, Contrast, SignalRole, Theme, ThemeId};

use crate::edge::{Edge, Orientation};
use crate::input::{TaskbarInput, TaskbarResponse, LONG_PRESS_AFTER_NS};
use crate::layout::Hit;
use crate::library::{folder_label, LibraryFocus, LibraryRow};
use crate::menu::MenuSubject;
use crate::notifications::{
    IconId, NotifySeverity, StatusKind, StatusSignal, TransientNotification,
};
use crate::pins::PinView;
use crate::render::TaskbarRenderer;
use crate::repaint::TaskbarRepaint;
use crate::taskbar::{Taskbar, TaskbarConfig};
use crate::tasks::{ActivateOutcome, TaskId, TaskList};
use crate::tray::derive_signal;

/// A fixed monotonic time for tests that do not care about the Switchboard
/// capsule's tap-or-hold timing — every ordinary press-then-release in this
/// module happens "instantly" at this one instant, well under
/// [`crate::input::LONG_PRESS_AFTER_NS`], so it always resolves as a quick
/// click. The long-press tests advance past it explicitly.
const NOW_NS: u64 = 1_000_000_000;

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

/// A bottom-bar taskbar over the standard fixture with its popup closed,
/// handed back settled: seeding the catalog latches the popup and the bar,
/// and that is the fixture's own doing, never what a test measures.
fn bottom_bar() -> Taskbar {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    bar.library_mut().set_catalog(office_and_games());
    let _ = bar.take_repaint();
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
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        taskbar,
        Scale::ONE,
        NOW_NS,
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
        NOW_NS,
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

#[test]
fn previous_task_remembers_the_last_real_handover() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "A");
    tasks.add(TaskId(2), "B");
    tasks.add(TaskId(3), "C");
    assert_eq!(tasks.previous(), None);

    // Focus arriving from the desktop remembers no previous task.
    tasks.set_focused(Some(TaskId(1)));
    assert_eq!(tasks.previous(), None);
    // A handover between tasks records the one that held focus...
    tasks.set_focused(Some(TaskId(2)));
    assert_eq!(tasks.previous(), Some(TaskId(1)));
    // ...a re-focus of the current task does not touch it...
    tasks.set_focused(Some(TaskId(2)));
    assert_eq!(tasks.previous(), Some(TaskId(1)));
    // ...nor does parking focus on the desktop.
    tasks.set_focused(None);
    assert_eq!(tasks.previous(), Some(TaskId(1)));
    // Refocusing from the desktop is a desktop handover again.
    tasks.set_focused(Some(TaskId(3)));
    assert_eq!(tasks.previous(), None);

    // The activate toggle's restore path records the handover too.
    tasks.activate(TaskId(1));
    assert_eq!(tasks.previous(), Some(TaskId(3)));
    // Minimising (activating the focused task) is not a handover.
    tasks.activate(TaskId(1));
    assert_eq!(tasks.previous(), Some(TaskId(3)));
    // A closing task is forgotten rather than resurrected later.
    tasks.remove(TaskId(3));
    assert_eq!(tasks.previous(), None);
}

// ---- notifications --------------------------------------------------

#[test]
fn status_signals_set_and_dedup() {
    let mut bar = bottom_bar();
    bar.set_status_signals(alloc::vec![
        StatusSignal::new(IconId(1), StatusKind::Network),
        StatusSignal::new(IconId(1), StatusKind::Volume), // later duplicate id dropped
        StatusSignal::new(IconId(2), StatusKind::Battery),
    ]);
    let signals = bar.notifications().signals();
    assert_eq!(signals.len(), 2, "the later duplicate id is dropped");
    assert_eq!(signals[0].kind, StatusKind::Network);
    assert_eq!(signals[1].kind, StatusKind::Battery);
    assert_eq!(bar.notifications().signal_count(), 2);
}

#[test]
fn notifications_raise_upsert_and_clear() {
    let mut bar = bottom_bar();
    assert!(bar.raise_notification(TransientNotification::new(
        7,
        1,
        NotifySeverity::Info,
        "Sync",
        "Started",
    )));
    assert_eq!(bar.notifications().notification_count(), 1);
    // Re-raising the same (producer, key) with new content updates in place.
    assert!(bar.raise_notification(TransientNotification::new(
        7,
        1,
        NotifySeverity::Info,
        "Sync",
        "Halfway",
    )));
    assert_eq!(bar.notifications().notification_count(), 1);
    assert_eq!(
        bar.notifications().notification(0).expect("present").body,
        "Halfway"
    );
    // Re-raising byte-identical content reports no change.
    assert!(!bar.raise_notification(TransientNotification::new(
        7,
        1,
        NotifySeverity::Info,
        "Sync",
        "Halfway",
    )));
    // Clearing is idempotent.
    assert!(bar.clear_notification(7, 1));
    assert!(!bar.clear_notification(7, 1));
    assert!(!bar.notifications().has_notifications());
}

#[test]
fn notifications_order_by_severity_then_recency() {
    let mut bar = bottom_bar();
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        1,
        NotifySeverity::Info,
        "a",
        "",
    ));
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        2,
        NotifySeverity::Critical,
        "b",
        "",
    ));
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        3,
        NotifySeverity::Info,
        "c",
        "",
    ));
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        4,
        NotifySeverity::Warning,
        "d",
        "",
    ));
    let titles: Vec<&str> = bar
        .notifications()
        .notifications()
        .map(|note| note.title.as_str())
        .collect();
    // Critical first, then Warning, then the two Info newest-first (c before a).
    assert_eq!(titles, ["b", "d", "c", "a"]);
}

#[test]
fn raising_and_dismissing_a_notification_latches_the_popover_and_the_bar() {
    let mut bar = bottom_bar();
    let _ = bar.take_repaint();

    assert!(bar.raise_notification(TransientNotification::new(
        7,
        1,
        NotifySeverity::Info,
        "Sync",
        "Started",
    )));
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::NOTIFICATIONS | TaskbarRepaint::BAR,
        "a raise shows a card in the popover and updates the bar's icon"
    );

    assert!(bar.clear_notification(7, 1));
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::NOTIFICATIONS | TaskbarRepaint::BAR,
        "a dismiss removes the card and updates the bar's icon"
    );
}

#[test]
fn clear_producer_drops_only_that_producers_notifications() {
    let mut bar = bottom_bar();
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        1,
        NotifySeverity::Info,
        "p1a",
        "",
    ));
    let _ = bar.raise_notification(TransientNotification::new(
        2,
        1,
        NotifySeverity::Info,
        "p2a",
        "",
    ));
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        2,
        NotifySeverity::Info,
        "p1b",
        "",
    ));
    assert!(bar.clear_producer_notifications(1));
    assert_eq!(bar.notifications().notification_count(), 1);
    assert_eq!(
        bar.notifications().notification(0).expect("present").title,
        "p2a"
    );
    assert!(!bar.clear_producer_notifications(9), "absent producer");
}

// ---- bar layout -----------------------------------------------------

#[test]
fn leading_launchers_partition_the_leading_end() {
    let bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library, Rect::new(0, 760, 48, 40));
    assert_eq!(layout.files, Rect::new(48, 760, 48, 40));
    assert_eq!(layout.task_list.left(), 96);
    // The Switchboard capsule owns the very trailing end; the clock ends
    // where it starts.
    assert_eq!(layout.switchboard, Rect::new(956, 760, 44, 40));
    assert_eq!(layout.clock.right(), 956);
}

#[test]
fn hit_testing_resolves_every_region() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.set_status_signals(alloc::vec![StatusSignal::new(
        IconId(7),
        StatusKind::Network
    )]);
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
    assert_eq!(
        bar.hit_test(centre_of(layout.switchboard), Scale::ONE),
        Some(Hit::Switchboard)
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
fn pin_strip_sits_between_launchers_and_tasks() {
    let mut bar = bottom_bar();
    // Initially empty.
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.pin_strip.width, 0);
    assert_eq!(layout.pin_strip.left(), layout.files.right());
    assert_eq!(layout.task_list.left(), layout.pin_strip.right());

    // Add two pins.
    bar.set_pins(alloc::vec![
        PinView::new("Pin 1", IconKind::AppBundle),
        PinView::new("Pin 2", IconKind::AppBundle),
    ]);
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.pins.len(), 2);
    assert_eq!(layout.pin_strip.width, 48 * 2);
    assert_eq!(layout.pin_strip.left(), layout.files.right());
    assert_eq!(layout.pins[0].left(), layout.pin_strip.left());
    assert_eq!(layout.pins[1].left(), layout.pins[0].right());
    assert_eq!(layout.task_list.left(), layout.pin_strip.right());
}

#[test]
fn adding_pins_reflows_the_task_region() {
    let mut bar = bottom_bar();
    let empty_tasks = bar.layout(Scale::ONE).task_list;
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let one_pin_tasks = bar.layout(Scale::ONE).task_list;
    assert!(one_pin_tasks.width < empty_tasks.width);
    assert_eq!(one_pin_tasks.left(), empty_tasks.left() + 48);
}

#[test]
fn pin_slots_clip_fail_closed_on_a_tiny_screen() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(244, 40), &Theme::dark());
    // Launchers (48+48) take 96. Switchboard (44) plus clock (80) take 124.
    // Screen 244. Remaining for pins/tasks: 244 - 220 = 24.
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library.width, 48);
    assert_eq!(layout.files.width, 48);
    assert_eq!(layout.pins[0].width, 24, "pin clips to fit");
    assert!(layout.task_list.is_empty(), "no room for tasks");

    // Even smaller: pin is empty.
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(200, 40), &Theme::dark());
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let layout = bar.layout(Scale::ONE);
    assert!(layout.pins[0].is_empty());
}

#[test]
fn pin_strip_positions_on_all_four_edges() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let mut bar = Taskbar::new(config, &Theme::dark());
        bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
        let layout = bar.layout(Scale::ONE);
        assert!(!layout.pin_strip.is_empty(), "{edge:?}");
        assert_eq!(layout.pins.len(), 1, "{edge:?}");
        match edge.orientation() {
            Orientation::Horizontal => {
                assert_eq!(layout.pin_strip.height, 40);
                assert_eq!(layout.pin_strip.width, 48);
            }
            Orientation::Vertical => {
                assert_eq!(layout.pin_strip.width, 40);
                assert_eq!(layout.pin_strip.height, 48);
            }
        }
    }
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

#[test]
fn pin_drop_index_finds_the_slot_or_append_point() {
    let mut bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    // Initially empty strip. Drop in the task-list band (where the first pin would land).
    assert_eq!(layout.pin_drop_index(Point::new(100, 780)), Some(0));
    // Outside the strip/task band (e.g. on the clock).
    assert_eq!(layout.pin_drop_index(Point::new(950, 780)), None);

    // Add one pin.
    bar.set_pins(alloc::vec![PinView::new("Pin 0", IconKind::AppBundle)]);
    let layout = bar.layout(Scale::ONE);
    let slot = layout.pins[0];
    // Leading half of pin 0 -> Some(0).
    assert_eq!(
        layout.pin_drop_index(Point::new(slot.left() + 10, 780)),
        Some(0)
    );
    // Trailing half of pin 0 -> Some(1).
    assert_eq!(
        layout.pin_drop_index(Point::new(slot.right() - 10, 780)),
        Some(1)
    );
    // Past the pin in the task band -> Some(1).
    assert_eq!(
        layout.pin_drop_index(Point::new(slot.right() + 10, 780)),
        Some(1)
    );
}

#[test]
fn pin_drop_index_works_on_vertical_bars() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    bar.set_config(TaskbarConfig {
        edge: Edge::Left,
        ..*bar.config()
    });
    bar.set_pins(alloc::vec![PinView::new("Pin 0", IconKind::AppBundle)]);
    let layout = bar.layout(Scale::ONE);
    let slot = layout.pins[0];
    // Leading (top) half -> Some(0).
    assert_eq!(
        layout.pin_drop_index(Point::new(20, slot.top() + 10)),
        Some(0)
    );
    // Trailing (bottom) half -> Some(1).
    assert_eq!(
        layout.pin_drop_index(Point::new(20, slot.bottom() - 10)),
        Some(1)
    );
}

#[test]
fn hit_testing_resolves_pins() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let layout = bar.layout(Scale::ONE);
    let centre = centre_of(layout.pins[0]);
    assert_eq!(bar.hit_test(centre, Scale::ONE), Some(Hit::Pin(0)));

    // Pins do not shadow launchers or tasks.
    assert_eq!(
        bar.hit_test(centre_of(layout.library), Scale::ONE),
        Some(Hit::Library)
    );
    assert_eq!(
        bar.hit_test(Point::new(layout.task_list.left() + 10, 780), Scale::ONE),
        None,
        "empty task list hits nothing"
    );
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
fn apply_theme_swaps_the_owned_theme_and_latches_every_surface() {
    let mut bar = bottom_bar();
    assert!(!bar.take_repaint().any(), "a fresh bar has nothing pending");
    bar.apply_theme(&Theme::light());
    assert_eq!(bar.theme().id(), Theme::light().id());
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::ALL,
        "every surface draws from the palette, so a theme switch repaints them all"
    );
    assert!(!bar.take_repaint().any(), "taking the latch clears it");
}

#[test]
fn set_config_latches_every_surface() {
    let mut bar = bottom_bar();
    let _ = bar.take_repaint();
    let mut config = *bar.config();
    config.edge = Edge::Top;
    bar.set_config(config);
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::ALL,
        "every surface is laid out from the config, so a resize or edge move repaints them all"
    );
}

#[test]
fn pin_visibility_derives_from_the_task_list() {
    let mut bar = bottom_bar();
    let entry_id = EntryId::new("os.tairix.editor").unwrap();
    let task_id = TaskId(1);
    bar.set_pins(alloc::vec![
        PinView::new("Editor", IconKind::AppBundle)
            .with_entry(entry_id.clone())
            .with_window(task_id),
        PinView::new("Stale", IconKind::AppBundle).with_window(TaskId(999)),
        PinView::new("None", IconKind::AppBundle),
    ]);

    // Index 0: Matched window but not in task list -> Closed.
    assert_eq!(
        bar.pins().visibility(0, bar.tasks()),
        TaskVisibility::Closed
    );
    // Index 1: Stale window id -> Closed.
    assert_eq!(
        bar.pins().visibility(1, bar.tasks()),
        TaskVisibility::Closed
    );
    // Index 2: No window -> Closed.
    assert_eq!(
        bar.pins().visibility(2, bar.tasks()),
        TaskVisibility::Closed
    );

    // Add the task for the editor.
    bar.tasks_mut().add(task_id, "Editor");
    assert_eq!(
        bar.pins().visibility(0, bar.tasks()),
        TaskVisibility::Running
    );

    // Focus it.
    bar.tasks_mut().set_focused(Some(task_id));
    assert_eq!(
        bar.pins().visibility(0, bar.tasks()),
        TaskVisibility::Active
    );

    // Minimise it.
    bar.tasks_mut().minimise(task_id);
    assert_eq!(
        bar.pins().visibility(0, bar.tasks()),
        TaskVisibility::Minimized
    );
}

#[test]
fn set_pins_clamps_a_stale_hover() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![
        PinView::new("Pin 0", IconKind::AppBundle),
        PinView::new("Pin 1", IconKind::AppBundle),
    ]);
    let layout = bar.layout(Scale::ONE);
    let pin1 = centre_of(layout.pins[1]);
    bar.track_hover(pin1, Scale::ONE);
    assert_eq!(bar.pins().hover(), Some(1));

    // Replace with one pin: hover is clamped to None.
    bar.set_pins(alloc::vec![PinView::new("Pin 0", IconKind::AppBundle)]);
    assert_eq!(bar.pins().hover(), None);
}

#[test]
fn pin_accessors_resolve_correctly() {
    let mut bar = bottom_bar();
    let entry_id = EntryId::new("os.tairix.editor").unwrap();
    let task_id = TaskId(1);
    bar.set_pins(alloc::vec![PinView::new("Editor", IconKind::AppBundle)
        .with_entry(entry_id.clone())
        .with_window(task_id)]);

    assert_eq!(bar.pins().len(), 1);
    assert_eq!(bar.pins().position_of_entry(&entry_id), Some(0));
    assert!(bar.pins().view_for_window(task_id).is_some());
    assert_eq!(bar.pins().get(0).unwrap().label(), "Editor");
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
fn status_icon_press_is_inert_and_clock_reports() {
    let mut bar = bottom_bar();
    bar.set_status_signals(alloc::vec![StatusSignal::new(
        IconId(3),
        StatusKind::Volume
    )]);
    let mut input = TaskbarInput::new();
    let layout = bar.layout(Scale::ONE);
    // A status signal is a live readout, not an action target this stage.
    let icon = centre_of(layout.notifications[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, icon.x, icon.y),
        TaskbarResponse::Ignored
    );
    let clock = centre_of(layout.clock);
    assert_eq!(
        press_at(&mut input, &mut bar, clock.x, clock.y),
        TaskbarResponse::ClockPressed
    );
}

#[test]
fn pin_press_activates_or_applies_task_rule() {
    let mut bar = bottom_bar();
    let task_id = TaskId(1);
    bar.set_pins(alloc::vec![
        PinView::new("Launch", IconKind::AppBundle),
        PinView::new("Run", IconKind::AppBundle).with_window(task_id),
    ]);
    let mut input = TaskbarInput::new();
    let layout = bar.layout(Scale::ONE);

    // Primary press on pin 0 (no window) -> ActivatePin.
    let slot0 = centre_of(layout.pins[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot0.x, slot0.y),
        TaskbarResponse::ActivatePin { index: 0 }
    );

    // Primary press on pin 1 (has window, but window not in task list) -> ActivatePin.
    let slot1 = centre_of(layout.pins[1]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot1.x, slot1.y),
        TaskbarResponse::ActivatePin { index: 1 }
    );

    // Add task for pin 1.
    bar.tasks_mut().add(task_id, "Running");
    // Primary press on pin 1 (has window in list) -> TaskActivated.
    assert_eq!(
        press_at(&mut input, &mut bar, slot1.x, slot1.y),
        TaskbarResponse::TaskActivated {
            id: task_id,
            outcome: ActivateOutcome::Activated,
        }
    );

    // Toggle: Second click on focused pinned window minimises.
    bar.tasks_mut().set_focused(Some(task_id));
    assert_eq!(
        press_at(&mut input, &mut bar, slot1.x, slot1.y),
        TaskbarResponse::TaskActivated {
            id: task_id,
            outcome: ActivateOutcome::Minimised,
        }
    );
}

#[test]
fn secondary_press_on_pin_opens_menu() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).pins[0]);
    input.handle(
        InputEvent::PointerMoved { to: slot },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );

    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            &mut bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::Ignored,
        "secondary press is Ignored but opens the menu"
    );
    assert!(bar.menu().is_open());
    assert_eq!(
        bar.menu().subject(),
        Some(&MenuSubject::Pin {
            index: 0,
            running: false
        })
    );
}

#[test]
fn menu_is_modal_and_dismisses_on_click_away_or_escape() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).pins[0]);
    input.handle(
        InputEvent::PointerMoved { to: slot },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(bar.menu().is_open());

    // Motion over the menu highlights rows. The pointer also leaves the
    // pin slot it started on, so the bar's own hover feedback latches too.
    let menu_layout = bar.menu_layout(Scale::ONE).unwrap();
    let menu_item_0 = Point::new(menu_layout.panel.left() + 5, menu_layout.panel.top() + 5);
    let _ = bar.take_repaint();
    input.handle(
        InputEvent::PointerMoved { to: menu_item_0 },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR | TaskbarRepaint::MENU,
        "leaving the pin repaints the bar, and the new highlight repaints the menu"
    );
    assert_eq!(bar.menu().control().current(), Some(0));

    // Scroll is claimed (Ignored).
    assert_eq!(
        input.handle(
            InputEvent::PointerScrolled { dx: 0, dy: 1 },
            &mut bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::Ignored
    );

    // Re-verify it is open before click-away.
    assert!(bar.menu().is_open());

    // Click away dismisses menu only.
    assert_eq!(
        press_at(&mut input, &mut bar, 500, 100),
        TaskbarResponse::Ignored
    );
    assert!(!bar.menu().is_open());

    // Move pointer back to pin before reopening.
    input.handle(
        InputEvent::PointerMoved { to: slot },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );

    // Reopen and test Escape.
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(bar.menu().is_open());
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape)),
        TaskbarResponse::Ignored
    );
    assert!(!bar.menu().is_open());
}

#[test]
fn keyboard_highlight_moves_in_the_menu_latch_only_the_menu() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).pins[0]);
    input.handle(
        InputEvent::PointerMoved { to: slot },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(bar.menu().is_open());

    // The keyboard never touches pointer hover, so a highlight move this
    // way latches the menu alone — no incidental bar change to compose with.
    let _ = bar.take_repaint();
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(bar.menu().control().current(), Some(0));
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::MENU,
        "a keyboard highlight move repaints only the menu"
    );
}

#[test]
fn menu_keyboard_navigation_chooses_rows() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).pins[0]);
    input.handle(
        InputEvent::PointerMoved { to: slot },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );

    // Down/Down/Enter chooses row 1 ("Unpin").
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::Unpin { index: 0 }
    );
    assert!(!bar.menu().is_open());
}

#[test]
fn entry_menu_offers_pin_or_unpin_and_launches() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Secondary press on an entry row opens the entry menu.
    let (row_index, entry_rect) =
        visible_row_where(&bar, |row| matches!(row, LibraryRow::Entry { .. }))
            .expect("an entry row is visible");
    let entry_id = match bar.library().rows().get(row_index).unwrap() {
        LibraryRow::Entry { id, .. } => id.clone(),
        LibraryRow::Folder { .. } => panic!("not an entry"),
    };
    let inside = centre_of(entry_rect);
    input.handle(
        InputEvent::PointerMoved { to: inside },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(bar.menu().is_open());
    assert!(matches!(
        bar.menu().subject(),
        Some(MenuSubject::Entry { pinned: None, .. })
    ));

    // Move pointer over menu row 0.
    let menu_layout = bar.menu_layout(Scale::ONE).unwrap();
    let menu_item_0 = Point::new(menu_layout.panel.left() + 5, menu_layout.panel.top() + 5);
    input.handle(
        InputEvent::PointerMoved { to: menu_item_0 },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );

    // Press and release to activate.
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    // Choose row 0 ("Open") -> LibraryLaunch and closes popup.
    assert_eq!(
        input.handle(
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            },
            &mut bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::LibraryLaunch {
            entry: entry_id.clone()
        }
    );
    assert!(!bar.menu().is_open());
    assert!(!bar.library().is_open());

    // Pin it and check verb switch.
    bar.set_pins(alloc::vec![
        PinView::new("App", IconKind::AppBundle).with_entry(entry_id.clone())
    ]);
    open_library(&mut input, &mut bar);
    input.handle(
        InputEvent::PointerMoved { to: inside },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(matches!(
        bar.menu().subject(),
        Some(MenuSubject::Entry {
            pinned: Some(0),
            ..
        })
    ));
}

#[test]
fn unpin_from_entry_menu_identifies_the_pin_index() {
    let mut bar = bottom_bar();
    let entry_id = EntryId::new("os.tairix.app").unwrap();
    bar.set_pins(alloc::vec![
        PinView::new("Other", IconKind::AppBundle),
        PinView::new("Target", IconKind::AppBundle).with_entry(entry_id.clone()),
    ]);
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Open menu for the entry.
    bar.menu_routing_mut().open(
        MenuSubject::Entry {
            entry: entry_id,
            pinned: Some(1),
        },
        Rect::EMPTY,
    );

    // Choose row 1 ("Unpin from taskbar").
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::Unpin { index: 1 }
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
            NOW_NS,
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
            NOW_NS,
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
    let _ = bar.take_repaint();

    // Entering the Library button changes its hover state: repaint the bar
    // only — the four other surfaces are untouched by a bar-button hover.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(10, 780),
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(input.pointer(), Point::new(10, 780));
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "hover enter repaints only the bar"
    );

    // Moving within the same button changes nothing.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(20, 780),
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(
        !bar.take_repaint().any(),
        "no visual change, so nothing latches"
    );

    // Leaving it changes its hover state back: repaint the bar only.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(500, 400),
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "hover exit repaints only the bar"
    );
}

// ---- input: open popup ----------------------------------------------

#[test]
fn opening_and_closing_the_library_popup_latches_the_popup_and_the_bar() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let _ = bar.take_repaint();

    let centre = centre_of(bar.layout(Scale::ONE).library);
    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::OpenLibrary
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR,
        "opening presses the bar's Library button in and shows the popup"
    );

    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::LibraryDismissed
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR,
        "closing releases the button and hides the popup"
    );
}

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
fn secondary_press_outside_dismisses_and_folder_rows_are_claimed() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Inside the panel on a folder row: claimed by the modal popup — a
    // folder offers no context actions, so nothing happens and the popup
    // stays.
    let (_, folder_rect) = visible_row_where(&bar, |row| matches!(row, LibraryRow::Folder { .. }))
        .expect("a folder row is visible");
    let inside = centre_of(folder_rect);
    input.handle(
        InputEvent::PointerMoved { to: inside },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            &mut bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::Ignored
    );
    assert!(bar.library().is_open());
    assert!(!bar.menu().is_open(), "a folder row opens no context menu");

    // Outside: dismisses, exactly like a primary click-away.
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(500, 100),
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            &mut bar,
            Scale::ONE,
            NOW_NS,
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
    let _ = bar.take_repaint();
    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::Ignored,
        "a fold is the popup's own state change"
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::LIBRARY,
        "a fold repaints only the popup"
    );
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

    let _ = bar.take_repaint();
    assert_eq!(
        input.handle(
            InputEvent::PointerScrolled { dx: 0, dy: 1 },
            &mut bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::LIBRARY,
        "a scroll repaints only the popup"
    );
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
fn typing_in_the_filter_latches_only_the_popup() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let _ = bar.take_repaint();

    press_key(&mut input, &mut bar, Key::Char('c'));
    let latched = bar.take_repaint();
    assert_eq!(
        latched,
        TaskbarRepaint::LIBRARY,
        "a filter edit repaints the popup only"
    );
    assert!(!latched.menu, "the filter never touches the context menu");
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

/// A dark theme with reduced motion, for the reduced-motion render check.
fn dark_reduced_motion() -> Theme {
    let base = Theme::dark();
    Theme::new(
        ThemeId(96),
        String::from("dark-reduced"),
        Appearance::Dark,
        *base.palette(),
        *base.metrics(),
        base.fonts().clone(),
        base.cursors().clone(),
        base.motion().with_reduced_motion(true),
        base.density(),
        Contrast::Normal,
    )
}

/// Whether `region` shows more than one distinct pixel — proof something was
/// painted there rather than left a flat fill.
fn region_is_varied(surface: &Surface, frame: Rect, region: Rect) -> bool {
    let first = pixel_at(surface, frame, region.left(), region.top());
    (region.top()..region.bottom())
        .any(|y| (region.left()..region.right()).any(|x| pixel_at(surface, frame, x, y) != first))
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
fn focused_task_shows_the_accent_seam_and_others_stay_quiet() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().activate(TaskId(1));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    // The focused task's shared control draws the lower accent seam…
    let seam = Rect::new(
        layout.tasks[0].left(),
        layout.tasks[0].bottom() - 6,
        layout.tasks[0].width,
        6,
    );
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        seam,
        role(theme.palette().accent)
    ));
    // …and the unfocused task shows no accent anywhere in its slot.
    assert!(!region_has_pixel(
        &surface,
        layout.bar,
        layout.tasks[1],
        role(theme.palette().accent)
    ));
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
    // The minimised task's shared control recesses its plate to the flat
    // surface colour, distinct from the raised bar background, and marks it
    // with the muted leading tick.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.tasks[0],
        role(theme.palette().surface)
    ));
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.tasks[0],
        role(theme.palette().on_surface_muted)
    ));
}

#[test]
fn status_signal_glyph_draws_in_the_muted_role() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.set_status_signals(alloc::vec![StatusSignal::new(
        IconId(1),
        StatusKind::Network
    )]);
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
fn pin_and_menu_actions_latch_repaints() {
    let mut bar = bottom_bar();
    let _ = bar.take_repaint();

    // set_pins draws on the bar's own pin strip: bar only.
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);

    // Opening the menu is its own overlay: menu only.
    bar.open_pin_menu(0, Rect::EMPTY);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::MENU);

    // Motion over a pin changes the bar's own hover feedback: bar only.
    let layout = bar.layout(Scale::ONE);
    let pin_centre = centre_of(layout.pins[0]);
    bar.track_hover(pin_centre, Scale::ONE);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);

    // Closing the menu: menu only.
    bar.close_menu();
    assert_eq!(bar.take_repaint(), TaskbarRepaint::MENU);
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
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    // The empty bar beyond the only task's slot carries no text ink: the
    // over-long title was truncated inside the slot, never spilled past it.
    let beyond = Rect::new(
        layout.tasks[0].right() + 1,
        layout.tasks[0].top(),
        layout.notification_area.left().unsigned_abs() - layout.tasks[0].right().unsigned_abs() - 1,
        layout.tasks[0].height,
    );
    assert!(!region_has_pixel(
        &surface,
        layout.bar,
        beyond,
        role(theme.palette().on_surface)
    ));
}

#[test]
fn pins_render_artwork_or_fallback_glyph() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    // Pin 0: Magenta artwork.
    let magenta = Color::rgb(255, 0, 255).premultiply();
    bar.set_pins(alloc::vec![
        PinView::new("Art", IconKind::AppBundle)
            .with_artwork(Surface::filled(16, 16, magenta).unwrap()),
        PinView::new("Glyph", IconKind::AppBundle),
    ]);
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");

    // Pin 0 shows the magenta artwork.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.pins[0],
        magenta
    ));

    // Pin 1 shows the AppBundle glyph (on_surface ink).
    assert!(region_has_role_ink(
        &surface,
        layout.bar,
        layout.pins[1],
        theme.palette().on_surface,
        role(theme.palette().surface_raised),
    ));
}

#[test]
fn active_pin_shows_the_accent_seam() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let task_id = TaskId(1);
    bar.set_pins(alloc::vec![
        PinView::new("App", IconKind::AppBundle).with_window(task_id)
    ]);
    bar.tasks_mut().add(task_id, "App");
    bar.tasks_mut().set_focused(Some(task_id));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");

    // The pin slot (not just the task slot) shows the accent seam.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.pins[0],
        role(theme.palette().accent)
    ));
}

#[test]
fn task_borrows_artwork_from_pin() {
    let mut bar = bottom_bar();
    let task_id = TaskId(1);
    let magenta = Color::rgb(255, 0, 255).premultiply();
    bar.set_pins(alloc::vec![PinView::new("App", IconKind::AppBundle)
        .with_window(task_id)
        .with_artwork(Surface::filled(16, 16, magenta).unwrap()),]);
    bar.tasks_mut().add(task_id, "App");

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");

    // The task slot shows the borrowed magenta artwork.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.tasks[0],
        magenta
    ));
}

#[test]
fn render_menu_paints_the_modal_plate_and_follows_theme() {
    let mut bar = bottom_bar();
    bar.set_pins(alloc::vec![PinView::new("Pin", IconKind::AppBundle)]);
    let renderer = TaskbarRenderer::new();

    // None when closed.
    assert!(renderer.render_menu(&bar, Scale::ONE).is_none());

    // Open and check render.
    bar.menu_routing_mut().open(
        MenuSubject::Pin {
            index: 0,
            running: false,
        },
        Rect::new(100, 760, 48, 40),
    );
    let layout = bar.menu_layout(Scale::ONE).expect("menu layout");
    let dark = renderer
        .render_menu(&bar, Scale::ONE)
        .expect("menu renders");
    assert_eq!(dark.width(), layout.panel.width);
    assert_eq!(dark.height(), layout.panel.height);

    // Plate is raised surface.
    assert!(region_has_pixel(
        &dark,
        layout.panel,
        Rect::new(
            layout.panel.left(),
            layout.panel.top(),
            layout.panel.width,
            layout.panel.height
        ),
        role(Theme::dark().palette().surface_raised)
    ));
    // Contains on_surface ink for labels.
    assert!(region_has_role_ink(
        &dark,
        layout.panel,
        Rect::new(
            layout.panel.left(),
            layout.panel.top(),
            layout.panel.width,
            layout.panel.height
        ),
        Theme::dark().palette().on_surface,
        role(Theme::dark().palette().surface_raised),
    ));

    // Theme switch changes pixels.
    bar.apply_theme(&Theme::light());
    let light = renderer
        .render_menu(&bar, Scale::ONE)
        .expect("menu renders");
    let centre = Point::new(layout.panel.left() + 5, layout.panel.top() + 5);
    assert_ne!(
        pixel_at(&dark, layout.panel, centre.x, centre.y),
        pixel_at(&light, layout.panel, centre.x, centre.y)
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
        NOW_NS,
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

// ---- notification popover -------------------------------------------

#[test]
fn no_popover_until_a_notification_is_raised() {
    let bar = bottom_bar();
    assert!(bar.notifications_layout(Scale::ONE).is_none());
    assert!(TaskbarRenderer::new()
        .render_notifications(&bar, Scale::ONE)
        .is_none());
}

#[test]
fn popover_lays_out_one_card_per_shown_notification() {
    let mut bar = bottom_bar();
    for key in 0..3 {
        let _ = bar.raise_notification(TransientNotification::new(
            1,
            key,
            NotifySeverity::Info,
            "n",
            "",
        ));
    }
    let layout = bar
        .notifications_layout(Scale::ONE)
        .expect("popover laid out");
    assert_eq!(layout.cards.len(), 3);
    // Opens outward above a bottom bar, and clamps within the screen.
    assert!(layout.panel.bottom() <= bar.layout(Scale::ONE).bar.top());
    assert!(layout.panel.left() >= 0 && layout.panel.right() <= 1000);
    // Cards are placed top-to-bottom in display order.
    assert!(layout.cards[0].card.top() < layout.cards[1].card.top());
    assert_eq!(layout.cards[0].index, 0);
}

#[test]
fn popover_caps_the_shown_cards() {
    let mut bar = bottom_bar();
    for key in 0..8 {
        let _ = bar.raise_notification(TransientNotification::new(
            1,
            key,
            NotifySeverity::Info,
            "n",
            "",
        ));
    }
    let layout = bar
        .notifications_layout(Scale::ONE)
        .expect("popover laid out");
    assert_eq!(layout.cards.len(), 4, "at most NOTIF_MAX_CARDS are shown");
}

#[test]
fn popover_fails_closed_on_a_degenerate_screen() {
    // A bar that fills the whole screen leaves no room above it for a card.
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(200, 40), &Theme::dark());
    let _ = bar.raise_notification(TransientNotification::new(
        1,
        1,
        NotifySeverity::Info,
        "n",
        "",
    ));
    let layout = bar
        .notifications_layout(Scale::ONE)
        .expect("still lays out");
    assert!(layout.cards.is_empty(), "no card fits, so none is placed");
    // Rendering a card-less popover still fails closed (no panic).
    let _ = TaskbarRenderer::new().render_notifications(&bar, Scale::ONE);
}

#[test]
fn card_at_maps_a_point_to_its_notification() {
    let mut bar = bottom_bar();
    let _ = bar.raise_notification(TransientNotification::new(
        4,
        9,
        NotifySeverity::Warning,
        "hi",
        "",
    ));
    let layout = bar.notifications_layout(Scale::ONE).expect("popover");
    let card = layout.cards[0].card;
    assert_eq!(layout.card_at(centre_of(card)), Some(0));
    assert!(layout.contains(centre_of(card)));
    assert_eq!(layout.card_at(Point::new(-1, -1)), None);
    assert!(!layout.contains(Point::new(-1, -1)));
}

#[test]
fn pressing_a_card_dismisses_it() {
    let mut bar = bottom_bar();
    let _ = bar.raise_notification(TransientNotification::new(
        4,
        9,
        NotifySeverity::Warning,
        "hi",
        "there",
    ));
    let mut input = TaskbarInput::new();
    let card = centre_of(bar.notifications_layout(Scale::ONE).expect("popover").cards[0].card);
    assert_eq!(
        press_at(&mut input, &mut bar, card.x, card.y),
        TaskbarResponse::DismissNotification {
            producer: 4,
            key: 9,
        }
    );
}

#[test]
fn pressing_popover_chrome_is_claimed_not_routed_to_the_bar() {
    let mut bar = bottom_bar();
    let _ = bar.raise_notification(TransientNotification::new(
        4,
        9,
        NotifySeverity::Warning,
        "hi",
        "there",
    ));
    let layout = bar.notifications_layout(Scale::ONE).expect("popover");
    // The header band at the panel's top edge is chrome, above the first card.
    let chrome = Point::new(layout.panel.left() + 2, layout.panel.top() + 1);
    assert!(layout.card_at(chrome).is_none());
    assert!(layout.contains(chrome));
    let mut input = TaskbarInput::new();
    assert_eq!(
        press_at(&mut input, &mut bar, chrome.x, chrome.y),
        TaskbarResponse::Ignored
    );
}

#[test]
fn render_notifications_paints_a_card_in_every_theme() {
    for theme in [
        Theme::dark(),
        Theme::light(),
        dark_with_contrast(Contrast::High),
        dark_reduced_motion(),
    ] {
        let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
        let _ = bar.raise_notification(TransientNotification::new(
            2,
            5,
            NotifySeverity::Critical,
            "Disk failing",
            "Backup now",
        ));
        let layout = bar.notifications_layout(Scale::ONE).expect("popover");
        let surface = TaskbarRenderer::new()
            .render_notifications(&bar, Scale::ONE)
            .expect("popover renders");
        assert_eq!(surface.width(), layout.panel.width);
        assert_eq!(surface.height(), layout.panel.height);
        // The card region is not a flat fill: the shared card drew its plate,
        // rim, and text there (true across dark/light/high-contrast/reduced).
        assert!(
            region_is_varied(&surface, layout.panel, layout.cards[0].card),
            "the notification card painted content ({})",
            theme.name(),
        );
    }
}

// ---- switchboard tray fixtures ----------------------------------------

/// A summary with `jobs`, `recovery`, and an overall CPU fraction — no
/// pressure and no top task.
fn tray_summary(jobs: u16, recovery: u16, cpu_permille: u16) -> TraySummary {
    TraySummary {
        jobs,
        recovery,
        cpu_busy_permille: TrayPermille::new(cpu_permille).expect("permille"),
        pressure: None,
        top_task: None,
    }
}

/// The dominant-pressure block of a summary.
fn tray_pressure(kind: TrayPressureKind, level: u16, count: u8) -> TrayPressure {
    TrayPressure {
        kind,
        level: TrayPermille::new(level).expect("permille"),
        count: TrayPressureCount::new(count).expect("count"),
    }
}

/// The busiest-task block of a summary.
fn tray_task(name: &str, cpu_permille: u16) -> TrayTask {
    TrayTask {
        name: TrayTaskName::new(name).expect("name"),
        cpu_permille: TrayPermille::new(cpu_permille).expect("permille"),
    }
}

/// Move the pointer to the Switchboard capsule's centre, returning it.
fn hover_switchboard(input: &mut TaskbarInput, taskbar: &mut Taskbar) -> Point {
    let centre = centre_of(taskbar.layout(Scale::ONE).switchboard);
    input.handle(
        InputEvent::PointerMoved { to: centre },
        taskbar,
        Scale::ONE,
        NOW_NS,
    );
    centre
}

/// Move the pointer to `at` and scroll by `(dx, dy)` there.
fn scroll_at(
    input: &mut TaskbarInput,
    taskbar: &mut Taskbar,
    at: Point,
    dx: i32,
    dy: i32,
) -> TaskbarResponse {
    input.handle(
        InputEvent::PointerMoved { to: at },
        taskbar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerScrolled { dx, dy },
        taskbar,
        Scale::ONE,
        NOW_NS,
    )
}

/// Move the pointer to `at` and press the middle button there.
fn middle_press_at(input: &mut TaskbarInput, taskbar: &mut Taskbar, at: Point) -> TaskbarResponse {
    input.handle(
        InputEvent::PointerMoved { to: at },
        taskbar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Middle,
        },
        taskbar,
        Scale::ONE,
        NOW_NS,
    )
}

/// Report the pointer at `to` at monotonic time `at_ns` — the motion sample
/// a real pointing device keeps sending while a button is still held down.
fn moved_at(
    input: &mut TaskbarInput,
    taskbar: &mut Taskbar,
    to: Point,
    at_ns: u64,
) -> TaskbarResponse {
    input.handle(InputEvent::PointerMoved { to }, taskbar, Scale::ONE, at_ns)
}

/// Release the primary button where the pointer already is, at monotonic
/// time `at_ns`.
fn release_at(input: &mut TaskbarInput, taskbar: &mut Taskbar, at_ns: u64) -> TaskbarResponse {
    input.handle(
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        taskbar,
        Scale::ONE,
        at_ns,
    )
}

/// A logical length in surface pixels at the tests' scale.
fn scaled(logical: u32) -> i32 {
    i32::try_from(Scale::ONE.scale_length(logical).max(1)).expect("fits")
}

/// The centre of the open readout's "Open Switchboard" action, placed by the
/// same public control metrics the shared readout lays it out with: the
/// bottom control-height band of the panel's padded interior.
fn readout_action_centre(taskbar: &Taskbar) -> Point {
    let panel = taskbar
        .tray_readout_layout(Scale::ONE)
        .expect("readout expanded")
        .panel;
    let theme = Theme::dark();
    let pad = scaled(theme.metrics().control_inset);
    let height = scaled(theme.metrics().control_height);
    Point::new(centre_of(panel).x, panel.bottom() - pad - height / 2)
}

// ---- switchboard tray layout ------------------------------------------

#[test]
fn switchboard_is_trailing_most_on_every_edge() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let bar = Taskbar::new(config, &Theme::dark());
        let layout = bar.layout(Scale::ONE);
        let (slot_start, slot_end, clock_end, main_end) = match edge.orientation() {
            Orientation::Horizontal => (
                layout.switchboard.left(),
                layout.switchboard.right(),
                layout.clock.right(),
                1000,
            ),
            Orientation::Vertical => (
                layout.switchboard.top(),
                layout.switchboard.bottom(),
                layout.clock.bottom(),
                800,
            ),
        };
        assert_eq!(slot_end, main_end, "{edge:?}: the capsule is trailing-most");
        assert_eq!(
            clock_end, slot_start,
            "{edge:?}: the clock ends at the capsule"
        );
        assert_eq!(
            layout.hit_test(centre_of(layout.switchboard)),
            Some(Hit::Switchboard),
            "{edge:?}"
        );
    }
}

#[test]
fn narrow_screen_collapses_clock_and_icons_before_the_switchboard() {
    // 140 px holds exactly the launchers (96) plus the 44 px capsule: the
    // clock and the notification area collapse to nothing first.
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(140, 800), &Theme::dark());
    bar.set_status_signals(alloc::vec![StatusSignal::new(
        IconId(1),
        StatusKind::Network
    )]);
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.switchboard, Rect::new(96, 760, 44, 40));
    assert!(layout.clock.is_empty());
    assert!(layout.notification_area.is_empty());
    assert!(layout.notifications[0].is_empty());
    assert_eq!(
        layout.hit_test(centre_of(layout.switchboard)),
        Some(Hit::Switchboard)
    );
}

#[test]
fn tiny_screen_clips_the_switchboard_against_the_launchers() {
    // 96 px is exactly the two permanent launchers: every trailing region,
    // the capsule included, fails closed to empty rather than overlaying
    // them.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(96, 800), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert!(layout.switchboard.is_empty());
    assert!(layout.clock.is_empty());
    assert_eq!(layout.hit_test(Point::new(60, 780)), Some(Hit::Files));

    // An absurd sliver clips into the launchers themselves; nothing panics
    // and the empty capsule slot can never be hit.
    let sliver = Taskbar::new(TaskbarConfig::bottom_bar(10, 800), &Theme::dark());
    let slim = sliver.layout(Scale::ONE);
    assert!(slim.switchboard.is_empty());
    assert!(slim.files.is_empty());
    assert!(!slim.library.is_empty());
}

// ---- switchboard tray derive ------------------------------------------

#[test]
fn derive_absent_service_is_calm_idle() {
    let derived = derive_signal(None, 0);
    assert_eq!(derived.state, ControlState::idle());
    assert_eq!(derived.badge, None);
    assert_eq!(derived.label, "System normal");
    assert_eq!(derived.value, None);
}

#[test]
fn derive_calm_previews_the_top_task() {
    let mut summary = tray_summary(0, 0, 500);
    summary.top_task = Some(tray_task("editor", 254));
    let derived = derive_signal(Some(&summary), 0);
    assert_eq!(derived.label, "System normal");
    assert_eq!(derived.badge, None);
    assert_eq!(derived.value.as_deref(), Some("editor — 25% CPU"));

    // Without a top task the calm value is the overall CPU figure.
    let plain = derive_signal(Some(&tray_summary(0, 0, 500)), 0);
    assert_eq!(plain.value.as_deref(), Some("CPU 50%"));
}

#[test]
fn derive_jobs_shows_the_accent_count_and_working_seam() {
    let derived = derive_signal(Some(&tray_summary(3, 0, 100)), 0);
    assert_eq!(derived.label, "Background work");
    assert_eq!(derived.value.as_deref(), Some("3 jobs"));
    let badge = derived.badge.expect("badge");
    assert_eq!(badge.content(), TrayBadgeContent::Count(3));
    assert_eq!(badge.tone(), TrayBadgeTone::Accent);
    assert_eq!(derived.state.activity, ActivityState::Working);
    assert_eq!(derived.state.pressure, PressureState::None);

    let one = derive_signal(Some(&tray_summary(1, 0, 100)), 0);
    assert_eq!(one.value.as_deref(), Some("1 job"));
}

#[test]
fn derive_pressure_outranks_jobs_and_keeps_the_seam() {
    let mut summary = tray_summary(2, 0, 800);
    summary.pressure = Some(tray_pressure(TrayPressureKind::Memory, 730, 2));
    let derived = derive_signal(Some(&summary), 0);
    assert_eq!(derived.label, "Memory pressure");
    assert_eq!(derived.value.as_deref(), Some("73%"));
    let badge = derived.badge.expect("badge");
    assert_eq!(badge.content(), TrayBadgeContent::Count(2));
    assert_eq!(badge.tone(), TrayBadgeTone::Warning);
    // The orthogonal furniture still composes: the jobs keep the working
    // seam while the rail names the dominant pressure.
    assert_eq!(derived.state.activity, ActivityState::Working);
    assert_eq!(
        derived.state.pressure,
        PressureState::Under(PressureKind::Memory)
    );
}

#[test]
fn derive_maps_every_pressure_kind() {
    for (wire, mapped, label) in [
        (TrayPressureKind::Cpu, PressureKind::Cpu, "CPU pressure"),
        (
            TrayPressureKind::Memory,
            PressureKind::Memory,
            "Memory pressure",
        ),
        (TrayPressureKind::Disk, PressureKind::Disk, "Disk pressure"),
        (
            TrayPressureKind::Network,
            PressureKind::Network,
            "Network pressure",
        ),
        (
            TrayPressureKind::Power,
            PressureKind::Power,
            "Power pressure",
        ),
        (
            TrayPressureKind::Thermal,
            PressureKind::Thermal,
            "Thermal pressure",
        ),
    ] {
        let mut summary = tray_summary(0, 0, 0);
        summary.pressure = Some(tray_pressure(wire, 995, 1));
        let derived = derive_signal(Some(&summary), 0);
        assert_eq!(derived.state.pressure, PressureState::Under(mapped));
        assert_eq!(derived.label, label);
        assert_eq!(
            derived.value.as_deref(),
            Some("100%"),
            "995 permille rounds to a whole 100%"
        );
    }
}

#[test]
fn derive_recovery_shows_the_recovery_count() {
    let derived = derive_signal(Some(&tray_summary(0, 2, 100)), 0);
    assert_eq!(derived.label, "Recovery available");
    assert_eq!(derived.value.as_deref(), Some("2 tasks"));
    let badge = derived.badge.expect("badge");
    assert_eq!(badge.content(), TrayBadgeContent::Count(2));
    assert_eq!(badge.tone(), TrayBadgeTone::Recovery);
    assert_eq!(derived.state.recovery, RecoveryState::Recoverable);
}

#[test]
fn derive_hung_outranks_everything_and_composes_the_rest() {
    let mut summary = tray_summary(2, 1, 900);
    summary.pressure = Some(tray_pressure(TrayPressureKind::Cpu, 900, 1));
    summary.top_task = Some(tray_task("miner", 800));
    let derived = derive_signal(Some(&summary), 2);
    assert_eq!(derived.label, "Not responding");
    assert_eq!(derived.value.as_deref(), Some("2 applications"));
    let badge = derived.badge.expect("badge");
    assert_eq!(badge.content(), TrayBadgeContent::Alert);
    assert_eq!(badge.tone(), TrayBadgeTone::Danger);
    // Orthogonal furniture: the hung recovery posture, the working seam,
    // and the CPU rail all compose beneath the dominant alert.
    assert_eq!(derived.state.recovery, RecoveryState::Hung);
    assert_eq!(derived.state.activity, ActivityState::Working);
    assert_eq!(
        derived.state.pressure,
        PressureState::Under(PressureKind::Cpu)
    );

    let one = derive_signal(Some(&tray_summary(0, 0, 0)), 1);
    assert_eq!(one.value.as_deref(), Some("1 application"));
    assert_eq!(one.state.recovery, RecoveryState::Hung);
}

// ---- switchboard tray model -------------------------------------------

#[test]
fn tray_feeds_latch_repaint_only_on_change() {
    let mut bar = bottom_bar();
    let _ = bar.take_repaint();
    // The readout is collapsed throughout, so only the bar's capsule
    // latches — the readout is not being presented to need one.
    bar.set_tray_summary(Some(tray_summary(1, 0, 100)));
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);
    bar.set_tray_summary(Some(tray_summary(1, 0, 100)));
    assert!(
        !bar.take_repaint().any(),
        "an identical summary changes nothing"
    );
    bar.set_tray_unresponsive(2);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);
    bar.set_tray_unresponsive(2);
    assert!(!bar.take_repaint().any());
    bar.set_tray_summary(None);
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "service loss reverts the capsule"
    );
}

#[test]
fn tray_update_keeps_the_hovered_readout_open() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);
    assert!(bar.tray().is_expanded(), "hover expands the readout");
    let _ = bar.take_repaint();
    bar.set_tray_summary(Some(tray_summary(4, 0, 100)));
    assert!(
        bar.tray().is_expanded(),
        "a live update never collapses the readout"
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR | TaskbarRepaint::READOUT,
        "the readout is open, so a tray update repaints it alongside the bar"
    );
}

// ---- switchboard tray interactions ------------------------------------

#[test]
fn scroll_over_the_capsule_cycles_tasks() {
    let mut bar = bottom_bar();
    for id in 1..=3 {
        bar.tasks_mut().add(TaskId(id), format!("T{id}"));
    }
    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);

    // No focused task: scrolling forward starts at the first entry.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 1),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated
        }
    );
    // Forward again advances...
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 1),
        TaskbarResponse::TaskActivated {
            id: TaskId(2),
            outcome: ActivateOutcome::Activated
        }
    );
    // ...and backward returns (dx is the fallback when dy is zero).
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, -1, 0),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated
        }
    );
    // Backward from the first entry wraps to the last.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, -2),
        TaskbarResponse::TaskActivated {
            id: TaskId(3),
            outcome: ActivateOutcome::Activated
        }
    );
    // Forward from the last wraps to the first.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 3),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated
        }
    );
    // A zero-delta scroll and a scroll elsewhere change nothing.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 0),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        scroll_at(&mut input, &mut bar, Point::new(500, 780), 0, 1),
        TaskbarResponse::Ignored
    );
}

#[test]
fn scroll_with_no_focus_and_no_tasks_fails_closed() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);
    // No tasks at all: nothing to cycle, nothing changes.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 1),
        TaskbarResponse::Ignored
    );

    bar.tasks_mut().add(TaskId(7), "Solo");
    bar.tasks_mut().add(TaskId(8), "Duo");
    // No focused task: scrolling backward starts at the last entry.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, -1),
        TaskbarResponse::TaskActivated {
            id: TaskId(8),
            outcome: ActivateOutcome::Activated
        }
    );
}

#[test]
fn scroll_over_the_open_readout_also_cycles() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);
    let readout = bar.tray_readout_layout(Scale::ONE).expect("expanded");
    let inside = centre_of(readout.panel);
    assert_eq!(
        scroll_at(&mut input, &mut bar, inside, 0, 1),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated
        }
    );
}

#[test]
fn middle_press_switches_to_the_previous_task() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().activate(TaskId(1));
    bar.tasks_mut().activate(TaskId(2));
    assert_eq!(bar.tasks().previous(), Some(TaskId(1)));

    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated
        }
    );
    // The switch itself was a handover: pressing again toggles back.
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::TaskActivated {
            id: TaskId(2),
            outcome: ActivateOutcome::Activated
        }
    );
    // Elsewhere the middle button stays inert.
    assert_eq!(
        middle_press_at(&mut input, &mut bar, Point::new(500, 780)),
        TaskbarResponse::Ignored
    );
}

#[test]
fn middle_press_fails_closed_without_a_previous_task() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().activate(TaskId(1));
    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);
    // Focus arrived from the desktop: there is no previous task yet.
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::Ignored
    );

    // A remembered task that closed is forgotten, never resurrected.
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().activate(TaskId(2));
    assert_eq!(bar.tasks().previous(), Some(TaskId(1)));
    bar.tasks_mut().remove(TaskId(1));
    assert_eq!(bar.tasks().previous(), None);
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::Ignored
    );
}

#[test]
fn a_quick_press_on_the_capsule_opens_the_task_section() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let capsule = hover_switchboard(&mut input, &mut bar);

    // The press only arms the tap-or-hold gesture; the release resolves it,
    // so exactly one response is reported for the one click.
    assert_eq!(
        press_at(&mut input, &mut bar, capsule.x, capsule.y),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::OpenSwitchboard {
            section: Section::Tasks
        }
    );
    // The gesture is spent: a stray second release opens nothing.
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::Ignored
    );
}

#[test]
fn a_long_press_on_the_capsule_opens_recovery_exactly_once() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let capsule = hover_switchboard(&mut input, &mut bar);
    assert_eq!(
        press_at(&mut input, &mut bar, capsule.x, capsule.y),
        TaskbarResponse::Ignored
    );

    // A sample taken while the press is held, but a nanosecond short of the
    // threshold, resolves nothing.
    assert_eq!(
        moved_at(
            &mut input,
            &mut bar,
            capsule,
            NOW_NS + LONG_PRESS_AFTER_NS - 1
        ),
        TaskbarResponse::Ignored
    );
    // The first sample past it opens Recovery, without waiting for the lift.
    assert_eq!(
        moved_at(&mut input, &mut bar, capsule, NOW_NS + LONG_PRESS_AFTER_NS),
        TaskbarResponse::OpenSwitchboard {
            section: Section::Recovery
        }
    );
    // Already fired: neither a further sample nor the release fires again.
    assert_eq!(
        moved_at(
            &mut input,
            &mut bar,
            capsule,
            NOW_NS + LONG_PRESS_AFTER_NS * 2
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS + LONG_PRESS_AFTER_NS * 2),
        TaskbarResponse::Ignored
    );
}

#[test]
fn a_hold_released_with_no_intervening_sample_still_opens_recovery() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let capsule = hover_switchboard(&mut input, &mut bar);
    let _ = press_at(&mut input, &mut bar, capsule.x, capsule.y);
    // A perfectly still finger sends no motion, so the release is the first
    // event that can resolve the hold — and it resolves it as a hold.
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS + LONG_PRESS_AFTER_NS),
        TaskbarResponse::OpenSwitchboard {
            section: Section::Recovery
        }
    );
}

#[test]
fn a_press_dragged_off_the_capsule_opens_nothing() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let capsule = hover_switchboard(&mut input, &mut bar);
    let _ = press_at(&mut input, &mut bar, capsule.x, capsule.y);

    let away = Point::new(500, 400);
    assert_eq!(
        moved_at(&mut input, &mut bar, away, NOW_NS),
        TaskbarResponse::Ignored
    );
    // Cancelled: neither holding past the threshold nor releasing revives
    // it, and neither does dragging back onto the capsule (fail closed).
    assert_eq!(
        moved_at(&mut input, &mut bar, away, NOW_NS + LONG_PRESS_AFTER_NS),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        moved_at(&mut input, &mut bar, capsule, NOW_NS + LONG_PRESS_AFTER_NS),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS + LONG_PRESS_AFTER_NS),
        TaskbarResponse::Ignored
    );
}

#[test]
fn the_readout_safe_action_opens_switchboard() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);
    let action = readout_action_centre(&bar);

    assert_eq!(
        press_at(&mut input, &mut bar, action.x, action.y),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::OpenSwitchboard {
            section: Section::Tasks
        }
    );
    assert!(
        bar.tray().is_expanded(),
        "the readout stays open under the pointer"
    );
}

#[test]
fn a_press_inside_the_readout_away_from_its_action_is_claimed_inert() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);
    let panel = bar.tray_readout_layout(Scale::ONE).expect("hover").panel;
    let pad = scaled(Theme::dark().metrics().control_inset);
    let label = Point::new(panel.left() + pad, panel.top() + pad);

    assert_eq!(
        press_at(&mut input, &mut bar, label.x, label.y),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::Ignored
    );
    assert!(
        bar.tray().is_expanded(),
        "the inert claim never collapses the readout"
    );
}

#[test]
fn a_press_elsewhere_on_the_bar_arms_no_capsule_gesture() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let clock = centre_of(bar.layout(Scale::ONE).clock);
    assert_eq!(
        press_at(&mut input, &mut bar, clock.x, clock.y),
        TaskbarResponse::ClockPressed
    );
    // Only the capsule arms the tap-or-hold gesture: this click's release
    // opens nothing, however long it was held.
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS + LONG_PRESS_AFTER_NS),
        TaskbarResponse::Ignored
    );
}

// ---- switchboard tray readout geometry ---------------------------------

#[test]
fn readout_expands_on_hover_and_collapses_off() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    assert!(bar.tray_readout_layout(Scale::ONE).is_none());
    hover_switchboard(&mut input, &mut bar);
    let readout = bar.tray_readout_layout(Scale::ONE).expect("hover expands");
    assert_eq!(
        readout.corner_radius,
        Theme::dark().metrics().popup_corner_radius
    );
    assert!(readout.contains(centre_of(readout.panel)));
    assert!(!readout.contains(Point::new(-1, -1)));
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(500, 400),
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(
        bar.tray_readout_layout(Scale::ONE).is_none(),
        "hover off collapses"
    );
}

#[test]
fn readout_opens_outward_on_every_edge_and_stays_on_screen() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let mut bar = Taskbar::new(config, &Theme::dark());
        let mut input = TaskbarInput::new();
        hover_switchboard(&mut input, &mut bar);
        let bar_rect = bar.layout(Scale::ONE).bar;
        let readout = bar.tray_readout_layout(Scale::ONE).expect("hover expands");
        match edge {
            Edge::Bottom => assert_eq!(readout.panel.bottom(), bar_rect.top(), "opens above"),
            Edge::Top => assert_eq!(readout.panel.top(), bar_rect.bottom(), "opens below"),
            Edge::Left => assert_eq!(readout.panel.left(), bar_rect.right(), "opens rightward"),
            Edge::Right => assert_eq!(readout.panel.right(), bar_rect.left(), "opens leftward"),
        }
        assert!(
            readout.panel.left() >= 0 && readout.panel.right() <= 1000,
            "{edge:?} clamps horizontally"
        );
        assert!(
            readout.panel.top() >= 0 && readout.panel.bottom() <= 800,
            "{edge:?} clamps vertically"
        );
    }
}

// ---- switchboard tray rendering ----------------------------------------

#[test]
fn capsule_paints_in_its_slot() {
    let bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_is_varied(&surface, layout.bar, layout.switchboard),
        "the capsule drew its plate and glyph in the slot"
    );
}

#[test]
fn pressure_paints_the_dominant_kind_rail() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let mut summary = tray_summary(0, 0, 0);
    summary.pressure = Some(tray_pressure(TrayPressureKind::Memory, 700, 1));
    bar.set_tray_summary(Some(summary));
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.switchboard,
        role(theme.palette().signal(SignalRole::Memory))
    ));
}

#[test]
fn jobs_paint_the_heat_seam_along_the_slot_bottom() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.set_tray_summary(Some(tray_summary(2, 0, 0)));
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, Scale::ONE)
        .expect("bar renders");
    // The working seam runs across the slot's lower edge — probe only the
    // bottom band, well away from the top-corner badge.
    let slot = layout.switchboard;
    let band = Rect::new(slot.left(), slot.bottom() - 4, slot.width, 4);
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        band,
        role(theme.palette().accent)
    ));
}

#[test]
fn badge_tones_follow_the_dominant_state() {
    let theme = Theme::dark();
    let hung = {
        let mut bar = bottom_bar();
        bar.set_tray_unresponsive(1);
        bar
    };
    let pressured = {
        let mut bar = bottom_bar();
        let mut summary = tray_summary(0, 0, 0);
        summary.pressure = Some(tray_pressure(TrayPressureKind::Disk, 500, 3));
        bar.set_tray_summary(Some(summary));
        bar
    };
    let working = {
        let mut bar = bottom_bar();
        bar.set_tray_summary(Some(tray_summary(5, 0, 0)));
        bar
    };
    let recoverable = {
        let mut bar = bottom_bar();
        bar.set_tray_summary(Some(tray_summary(0, 3, 0)));
        bar
    };
    for (bar, want, what) in [
        (&hung, theme.palette().danger, "hung shows the danger badge"),
        (
            &pressured,
            theme.palette().warning,
            "pressure shows the warning badge",
        ),
        (
            &working,
            theme.palette().accent,
            "jobs show the accent badge",
        ),
        (
            &recoverable,
            theme.palette().recovery,
            "recovery shows the recovery badge",
        ),
    ] {
        let layout = bar.layout(Scale::ONE);
        let surface = TaskbarRenderer::new()
            .render(bar, Scale::ONE)
            .expect("bar renders");
        assert!(
            region_has_pixel(&surface, layout.bar, layout.switchboard, role(want)),
            "{what}"
        );
    }
}

#[test]
fn capsule_renders_across_themes_and_high_contrast() {
    // The shared controls are static renderers, so reduced motion is honoured
    // by construction: the same still frame is what a reduced-motion desktop
    // presents.
    for theme in [
        Theme::dark(),
        Theme::light(),
        dark_with_contrast(Contrast::High),
        dark_reduced_motion(),
    ] {
        let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
        let mut summary = tray_summary(2, 0, 400);
        summary.pressure = Some(tray_pressure(TrayPressureKind::Memory, 600, 2));
        bar.set_tray_summary(Some(summary));
        let layout = bar.layout(Scale::ONE);
        let surface = TaskbarRenderer::new()
            .render(&bar, Scale::ONE)
            .expect("bar renders");
        assert!(
            region_is_varied(&surface, layout.bar, layout.switchboard),
            "{} paints the capsule",
            theme.name()
        );
        assert!(
            region_has_pixel(
                &surface,
                layout.bar,
                layout.switchboard,
                role(theme.palette().warning)
            ),
            "{} paints the warning badge",
            theme.name()
        );
    }
}

#[test]
fn collapsed_readout_renders_nothing() {
    let bar = bottom_bar();
    assert!(TaskbarRenderer::new()
        .render_tray_readout(&bar, Scale::ONE)
        .is_none());
}

#[test]
fn readout_renders_the_state_and_value_lines() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let mut summary = tray_summary(0, 0, 300);
    summary.top_task = Some(tray_task("editor", 250));
    bar.set_tray_summary(Some(summary));
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);

    let layout = bar.tray_readout_layout(Scale::ONE).expect("expanded");
    let surface = TaskbarRenderer::new()
        .render_tray_readout(&bar, Scale::ONE)
        .expect("readout renders");
    assert_eq!(surface.width(), layout.panel.width);
    assert_eq!(surface.height(), layout.panel.height);
    // The plate carries the state-name ink over the raised surface.
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.panel,
        theme.palette().on_surface,
        role(theme.palette().surface_raised),
    ));
}

// ---- TaskbarRepaint ---------------------------------------------------

#[test]
fn taskbar_repaint_none_and_all_are_the_expected_extremes() {
    assert!(!TaskbarRepaint::NONE.any());
    assert!(TaskbarRepaint::ALL.any());
    assert_eq!(TaskbarRepaint::default(), TaskbarRepaint::NONE);
    assert_eq!(
        TaskbarRepaint::BAR
            | TaskbarRepaint::LIBRARY
            | TaskbarRepaint::MENU
            | TaskbarRepaint::NOTIFICATIONS
            | TaskbarRepaint::READOUT,
        TaskbarRepaint::ALL
    );
}

#[test]
fn taskbar_repaint_single_surface_constants_set_only_that_field() {
    assert_eq!(
        TaskbarRepaint::BAR,
        TaskbarRepaint {
            bar: true,
            ..TaskbarRepaint::NONE
        }
    );
    assert_eq!(
        TaskbarRepaint::LIBRARY,
        TaskbarRepaint {
            library: true,
            ..TaskbarRepaint::NONE
        }
    );
    assert_eq!(
        TaskbarRepaint::MENU,
        TaskbarRepaint {
            menu: true,
            ..TaskbarRepaint::NONE
        }
    );
    assert_eq!(
        TaskbarRepaint::NOTIFICATIONS,
        TaskbarRepaint {
            notifications: true,
            ..TaskbarRepaint::NONE
        }
    );
    assert_eq!(
        TaskbarRepaint::READOUT,
        TaskbarRepaint {
            readout: true,
            ..TaskbarRepaint::NONE
        }
    );
    for single in [
        TaskbarRepaint::BAR,
        TaskbarRepaint::LIBRARY,
        TaskbarRepaint::MENU,
        TaskbarRepaint::NOTIFICATIONS,
        TaskbarRepaint::READOUT,
    ] {
        assert!(single.any());
        assert_ne!(single, TaskbarRepaint::NONE);
    }
}

#[test]
fn taskbar_repaint_bit_or_composes_without_losing_either_side() {
    let composed = TaskbarRepaint::BAR | TaskbarRepaint::MENU;
    assert_eq!(
        composed,
        TaskbarRepaint {
            bar: true,
            library: false,
            menu: true,
            notifications: false,
            readout: false,
        }
    );

    let mut assigned = TaskbarRepaint::NONE;
    assigned |= TaskbarRepaint::LIBRARY;
    assigned |= TaskbarRepaint::READOUT;
    assert_eq!(assigned, TaskbarRepaint::LIBRARY | TaskbarRepaint::READOUT);

    // Composing is idempotent: latching the same surface twice changes
    // nothing further.
    assert_eq!(
        TaskbarRepaint::BAR | TaskbarRepaint::BAR,
        TaskbarRepaint::BAR
    );
}

// ---- repaint latch: sub-model accessors -------------------------------

#[test]
fn tasks_mut_latches_the_bar() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "task slots draw on the bar and nowhere else"
    );
}

#[test]
fn library_mut_latches_the_popup_and_the_bar() {
    let mut bar = bottom_bar();
    bar.library_mut().set_catalog(office_and_games());
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR,
        "the same borrow can also open or close the popup, which redraws the Library button"
    );
}

#[test]
fn clock_mut_latches_the_bar() {
    let mut bar = bottom_bar();
    bar.clock_mut().set_label("12:34");
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "the clock draws on the bar and nowhere else"
    );
}

#[test]
fn handing_out_a_mutable_sub_model_latches_even_when_nothing_is_changed() {
    let mut bar = bottom_bar();

    let _ = bar.tasks_mut();
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);

    let _ = bar.clock_mut();
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);

    let _ = bar.library_mut();
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::LIBRARY | TaskbarRepaint::BAR,
        "the bar cannot see into a borrow, so handing one out must assume a change"
    );
}

#[test]
fn reading_through_an_immutable_accessor_latches_nothing() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    let _ = bar.take_repaint();

    assert_eq!(bar.tasks().len(), 1);
    assert!(!bar.library().is_open());
    assert!(!bar.menu().is_open());
    assert!(bar.clock().label().is_empty());
    assert!(!bar.tray().is_expanded());
    assert_eq!(bar.notifications().signal_count(), 0);
    assert_eq!(bar.pins().len(), 0);
    assert_eq!(bar.task_hover(), None);

    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::NONE,
        "reading redraws nothing, so it latches nothing"
    );
}

#[test]
fn routing_a_pointer_sample_over_the_open_popup_latches_only_what_changed() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let layout = bar.library_layout(Scale::ONE);
    let row = centre_of(layout.rows[0].1);
    input.handle(
        InputEvent::PointerMoved { to: row },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    let _ = bar.take_repaint();

    // The same row again: the router reaches into the popup, but nothing it
    // draws changed, so the 1 ms popup render must not be asked for.
    input.handle(
        InputEvent::PointerMoved { to: row },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::NONE,
        "routing borrows the popup mutably, but only a real change may latch it"
    );
}
