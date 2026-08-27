//! Headless unit tests for the taskbar layout, model, and rendering.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{
    CommandSection, TrayPermille, TrayPressure, TrayPressureCount, TrayPressureKind, TraySummary,
    TrayTask, TrayTaskName,
};
use tairix_abi::window_ipc::{
    AppMenu, AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuRow, APP_MENU_MAX_ROWS,
};
use tairix_controls::{
    damage, ground_fill, plate_border, ActivityState, ChromeLayer, ControlState, MenuItem,
    MenuMark, PressureKind, PressureState, RecoveryState, TrayBadgeContent, TrayBadgeTone,
};
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{IconArtwork, IconKind, IconPicture, IconRequest, IconSet, NoArtwork};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton, PointerFocus};
use tairix_proglib::{BundlePath, Catalog, DisplayName, EntryId, LibraryCategory, LibraryEntry};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::{Appearance, Contrast, Rgba, SignalRole, Theme, ThemeId};

use tairix_log::{Event, Sink};
use tairix_reclaim::{CacheBudget, PressureBand, ReclaimCache, ReportedPressure};

/// The seat every renderer under test belongs to.
const TEST_SEAT: u64 = 1;

/// A modest display backing (roughly 1280x720x4) so the derived budget is
/// representative of a real output rather than degenerate.
const TEST_FB_BYTES: usize = 1280 * 720 * 4;

/// Discards audit records; a test asserts on cache state, not on log
/// output.
struct NullSink;

impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

static TEST_SINK: NullSink = NullSink;

/// The band the shared [`test_icon_cache`] is governed by. Held at normal
/// for its whole life: a test that needs a different band owns its own
/// gauge (see [`pressured_renderer`]), because these tests run in
/// parallel and a shared mutable band would let one test's pressure
/// change decide another test's result.
static NORMAL_PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// A glyph cache at normal pressure, built through the real desktop
/// policy so the tests exercise the shipping budget derivation.
fn test_icon_cache() -> ReclaimCache<IconKind, Surface, IconEpoch> {
    NORMAL_PRESSURE.report(PressureBand::Normal);
    icon_cache(TEST_SEAT, TEST_FB_BYTES, &NORMAL_PRESSURE, &TEST_SINK)
}

/// A renderer governed by `gauge`, which the caller owns exclusively, so
/// a test may deepen the band without touching any other test's.
fn pressured_renderer(gauge: &'static ReportedPressure) -> TaskbarRenderer {
    gauge.report(PressureBand::Normal);
    TaskbarRenderer::new(icon_cache(TEST_SEAT, TEST_FB_BYTES, gauge, &TEST_SINK))
}

use crate::apps::{AppIdentity, AppSlot};
use crate::edge::{Edge, Orientation};
use crate::input::{TaskbarInput, TaskbarResponse, LONG_PRESS_AFTER_NS};
use crate::layout::Hit;
use crate::library::{folder_label, LibraryFocus, LibraryRow};
use crate::menu::{EntryRow, MenuSubject, INFO_ROW_LABEL};
use crate::notifications::{
    IconId, NotifySeverity, StatusKind, StatusSignal, TransientNotification,
};
use crate::picker::{PickerEntry, PICKER_CLOSE_GRACE_NS, PICKER_MIN_WINDOWS, PICKER_OPEN_DELAY_NS};
use crate::render::{icon_cache, IconEpoch, TaskbarRenderer};
use crate::repaint::TaskbarRepaint;
use crate::taskbar::{Taskbar, TaskbarConfig};
use crate::tasks::{TaskId, TaskList};
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

/// A validated icon-bar menu row label.
fn menu_label(text: &str) -> AppMenuLabel {
    AppMenuLabel::new(text).expect("a short label")
}

/// An enabled, unmarked chooseable row.
fn item(id: u16, label: &str) -> AppMenuRow {
    AppMenuRow::Item {
        id: AppMenuItemId::new(id).expect("a non-zero id"),
        label: menu_label(label),
        enabled: true,
        mark: AppMenuMark::None,
    }
}

/// The menu a well-behaved application declares: an action, a rule, *Quit*,
/// and the session-drawn *About* row.
fn declared_menu() -> AppMenu {
    let mut menu = AppMenu::EMPTY;
    menu.push(item(1, "New window")).expect("fits");
    menu.push(AppMenuRow::Separator).expect("fits");
    menu.push(item(2, "Quit")).expect("fits");
    menu.push(AppMenuRow::About).expect("fits");
    menu
}

/// An application slot for `label`, no windows and no declaration — what
/// the session resolves for a process that put nothing on the bar itself.
fn app(label: &str) -> AppSlot {
    AppSlot::new(label, IconKind::AppBundle)
}

/// The identity a signed manifest states for `name`, with both optional
/// fields present.
fn identity(name: &str) -> AppIdentity {
    AppIdentity {
        name: String::from(name),
        version: String::from("1.2.3"),
        purpose: Some(String::from("Does one thing well")),
        author: Some(String::from("TAIRiX")),
    }
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

/// Open the program-library popup, then open the context menu on its first
/// visible entry row, and report the entry that row names.
fn open_entry_menu(input: &mut TaskbarInput, taskbar: &mut Taskbar) -> EntryId {
    open_library(input, taskbar);
    let (row, rect) = visible_row_where(taskbar, |row| matches!(row, LibraryRow::Entry { .. }))
        .expect("an entry row is visible");
    let entry = match taskbar.library().rows().get(row).expect("the row exists") {
        LibraryRow::Entry { id, .. } => id.clone(),
        LibraryRow::Folder { .. } => panic!("not an entry"),
    };
    let inside = centre_of(rect);
    input.handle(
        InputEvent::PointerMoved { to: inside },
        taskbar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        taskbar,
        Scale::ONE,
        NOW_NS,
    );
    assert!(taskbar.menu().is_open());
    assert_eq!(
        taskbar.menu().subject(),
        Some(&MenuSubject::Entry {
            entry: entry.clone()
        })
    );
    entry
}

/// Click `row` of the open entry menu and report what the bar answered.
///
/// The row is found by the label its one definition gives it, so a
/// reordering moves the click with it rather than aiming at a stale index.
fn choose_entry_row(
    input: &mut TaskbarInput,
    taskbar: &mut Taskbar,
    row: EntryRow,
) -> TaskbarResponse {
    let layout = taskbar
        .menu_layout(Scale::ONE)
        .expect("the open menu lays out");
    let control = taskbar.menu().control();
    let index = control
        .items()
        .iter()
        .position(|item| item.label() == row.label())
        .expect("the row is drawn");
    let rect = control
        .row_rect(index, layout.panel, Scale::ONE, taskbar.theme())
        .expect("the row lays out");
    let over = centre_of(rect);
    input.handle(
        InputEvent::PointerMoved { to: over },
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
    );
    input.handle(
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        },
        taskbar,
        Scale::ONE,
        NOW_NS,
    )
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
        *base.fonts(),
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
fn minimising_drops_focus_and_focusing_restores() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    assert!(tasks.set_focused(Some(TaskId(1))));
    assert_eq!(tasks.focused(), Some(TaskId(1)));

    assert!(tasks.minimise(TaskId(1)));
    assert!(tasks.is_minimised(TaskId(1)));
    assert_eq!(tasks.focused(), None);
    assert!(
        tasks.minimise(TaskId(1)),
        "already minimised stays minimised"
    );

    // Focusing a window restores it — which is what choosing its cell in
    // the hover picker does.
    assert!(tasks.set_focused(Some(TaskId(1))));
    assert!(!tasks.is_minimised(TaskId(1)));

    assert!(!tasks.minimise(TaskId(9)), "an unknown id fails closed");
    assert!(!tasks.is_minimised(TaskId(9)));
}

#[test]
fn task_remove_clears_focus() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.set_focused(Some(TaskId(1)));
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

    // Restoring a minimised window is a handover like any other.
    tasks.set_focused(Some(TaskId(1)));
    assert_eq!(tasks.previous(), Some(TaskId(3)));
    tasks.minimise(TaskId(1));
    assert_eq!(
        tasks.previous(),
        Some(TaskId(3)),
        "minimising is not a handover"
    );
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
fn the_leading_launcher_partitions_the_leading_end() {
    let bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    // The bar stands off the screen by the 5 px taskbar margin it floats in
    // (x 5..995, top 800 − 5 − 40); every region then sits inside the bar's
    // 1 px rim, so the content strip runs x 6..994 and is 38 px thick.
    assert_eq!(layout.bar, Rect::new(5, 755, 990, 40));
    assert_eq!(layout.library, Rect::new(6, 756, 48, 38));
    // The rule sits a control gap (8) past the Library button, one border
    // thick, inset a control padding (10) from both long edges of the 38 px
    // content strip; the application strip follows the whole 17 px gutter.
    assert_eq!(layout.separator, Rect::new(62, 766, 1, 18));
    assert_eq!(layout.app_strip.left(), 71);
    // The Switchboard capsule owns the trailing end of that strip; the clock
    // ends where it starts.
    assert_eq!(layout.switchboard, Rect::new(950, 756, 44, 38));
    assert_eq!(layout.clock.right(), 950);
}

#[test]
fn hit_testing_resolves_every_region() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
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
        bar.hit_test(centre_of(layout.apps[0]), Scale::ONE),
        Some(Hit::App(0))
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
    // A gap between the last application slot and the notification area is
    // the bare bar: inside the bar, on no region.
    assert_eq!(bar.hit_test(Point::new(500, 780), Scale::ONE), None);
}

#[test]
fn the_launcher_hits_on_every_edge() {
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
        assert!(
            layout.bar.contains(centre_of(layout.library)),
            "{edge:?}: launcher lies on the bar"
        );
    }
}

#[test]
fn vertical_bar_places_the_launcher_at_the_top() {
    let config = TaskbarConfig {
        edge: Edge::Left,
        ..TaskbarConfig::bottom_bar(1000, 800)
    };
    let bar = Taskbar::new(config, &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    // A left bar floats off the top, bottom, and left screen edges by the
    // 5 px margin; its 40 px thickness is untouched. The launcher sits inside
    // its 1 px rim, so it starts at 6 and is 38 px broad, and the application
    // strip begins one whole gutter below it.
    assert_eq!(layout.bar, Rect::new(5, 5, 40, 790));
    assert_eq!(layout.library, Rect::new(6, 6, 38, 48));
    assert_eq!(layout.app_strip.top(), 71);
}

/// `rect`'s `(main start, main end, cross start, cross end)` on a bar
/// running along `orientation`, so one assertion serves both bar axes.
fn axes(rect: Rect, orientation: Orientation) -> (i32, i32, i32, i32) {
    match orientation {
        Orientation::Horizontal => (rect.left(), rect.right(), rect.top(), rect.bottom()),
        Orientation::Vertical => (rect.top(), rect.bottom(), rect.left(), rect.right()),
    }
}

#[test]
fn the_separator_divides_the_library_from_everything_after_it() {
    let theme = Theme::dark();
    let rule_thickness =
        i32::try_from(theme.metrics().border_thickness).expect("a one-pixel border");
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let bar = Taskbar::new(config, &theme);
        let layout = bar.layout(Scale::ONE);
        let orientation = edge.orientation();
        let (rule_start, rule_end, rule_near, rule_far) = axes(layout.separator, orientation);
        let (_, library_end, ..) = axes(layout.library, orientation);
        let (strip_start, ..) = axes(layout.app_strip, orientation);
        let (.., bar_near, bar_far) = axes(layout.bar, orientation);

        assert_eq!(
            rule_end - rule_start,
            rule_thickness,
            "{edge:?}: one border thick along the bar"
        );
        assert!(
            library_end <= rule_start && rule_end <= strip_start,
            "{edge:?}: the rule lies between the launcher and the strip and overlaps neither"
        );
        assert!(
            bar_near < rule_near && rule_far < bar_far,
            "{edge:?}: the rule is inset from both long edges, clear of the rounded ends"
        );
    }
}

#[test]
fn the_separator_gutter_shifts_everything_after_the_launcher() {
    let theme = Theme::dark();
    let metrics = theme.metrics();
    let gutter =
        i32::try_from(metrics.border_thickness + metrics.control_gap * 2).expect("a modest gutter");
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
    let layout = bar.layout(Scale::ONE);
    let launcher = i32::try_from(layout.library.width).expect("a modest launcher");

    let margin = i32::try_from(metrics.taskbar_margin).expect("a modest margin");
    // The leading end of the *content*: the margin the bar floats in, then
    // the bar's own rim the regions sit inside.
    let leading =
        margin + i32::try_from(plate_border(&theme, Scale::ONE)).expect("a modest border");
    assert_eq!(
        layout.library.left(),
        leading,
        "the library keeps the leading end"
    );
    assert_eq!(layout.app_strip.left(), leading + launcher + gutter);
    assert_eq!(layout.apps[0].left(), layout.app_strip.left());
}

#[test]
fn a_press_on_the_separator_reaches_the_bare_bar() {
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let mut bar = Taskbar::new(config, &Theme::dark());
        let point = centre_of(bar.layout(Scale::ONE).separator);
        assert_eq!(
            bar.hit_test(point, Scale::ONE),
            None,
            "{edge:?}: the rule is decoration, not a region"
        );
        let mut input = TaskbarInput::new();
        assert_eq!(
            press_at(&mut input, &mut bar, point.x, point.y),
            TaskbarResponse::Ignored,
            "{edge:?}: pressing the rule opens nothing"
        );
    }
}

#[test]
fn the_separator_keeps_its_place_at_every_scale() {
    let bar = bottom_bar();
    for percent in [50, 100, 200, 400] {
        let scale = Scale::from_percent(percent).expect("a valid scale");
        let layout = bar.layout(scale);
        let rule = layout.separator;
        assert!(rule.width >= 1, "{percent}%: the rule never scales away");
        assert!(rule.left() >= layout.library.right(), "{percent}%");
        assert!(rule.right() <= layout.app_strip.left(), "{percent}%");
        assert!(
            layout.bar.top() < rule.top() && rule.bottom() < layout.bar.bottom(),
            "{percent}%: inset from both long edges"
        );
        assert!(
            TaskbarRenderer::new(test_icon_cache())
                .render(&bar, scale, &mut NoArtwork)
                .is_some(),
            "{percent}%: the bar still renders"
        );
    }
}

#[test]
fn a_bar_too_thin_to_inset_the_rule_drops_it_and_keeps_the_flow() {
    // Thinner than two control insets: there is no room for a rule that does
    // not run edge to edge, so none is laid out.
    let config = TaskbarConfig {
        thickness: 12,
        ..TaskbarConfig::bottom_bar(1000, 800)
    };
    let bar = Taskbar::new(config, &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert!(layout.separator.is_empty());
    assert_eq!(
        layout.app_strip.left(),
        71,
        "the flow keeps its places whether or not the rule is drawn"
    );
    assert_eq!(layout.hit_test(Point::new(61, 790)), None);
    assert!(
        TaskbarRenderer::new(test_icon_cache())
            .render(&bar, Scale::ONE, &mut NoArtwork)
            .is_some(),
        "an undrawable rule never fails the bar"
    );
}

#[test]
fn the_launcher_outranks_everything_after_it_on_a_tiny_screen() {
    // 79 px, less the two 5 px margins and the bar's two 1 px rims, leaves a
    // 67 px content strip: room for the whole Library button and the 17 px
    // separator gutter, and 2 px for everything after them. The launcher
    // clips last, so it is whole and the strip is what collapses; of the
    // trailing regions the Switchboard capsule outranks the clock and the
    // icons, so the 2 px that remain are its.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(79, 50), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library.width, 48);
    assert_eq!(layout.separator, Rect::new(62, 16, 1, 18));
    assert!(layout.app_strip.is_empty());
    assert!(layout.clock.is_empty());
    assert_eq!(layout.switchboard.width, 2);

    // No room for the rule or anything after it: every region past the
    // launcher is empty and none can ever be hit.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(30, 50), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library.width, 18);
    assert!(layout.separator.is_empty());
    assert!(layout.app_strip.is_empty());
    assert!(layout.switchboard.is_empty());
    assert_eq!(layout.hit_test(Point::new(20, 30)), Some(Hit::Library));

    // A zero-sized screen yields empty regions and no hits anywhere.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(0, 0), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert!(layout.library.is_empty());
    assert!(layout.separator.is_empty());
    assert!(layout.app_strip.is_empty());
    assert_eq!(layout.hit_test(Point::new(0, 0)), None);
}

#[test]
fn a_bar_too_small_for_its_rim_keeps_its_content_inside_itself() {
    // The rim the regions sit inside is spent from the bar's own extent, so
    // a bar with barely any extent keeps its content rather than the rim.
    // Whatever survives, nothing may fall outside the bar and nothing may
    // panic — at any scale, on any edge.
    let theme = Theme::dark();
    for percent in [50, 100, 200, 400] {
        let scale = Scale::from_percent(percent).expect("a valid scale");
        for (screen_w, screen_h) in [(0, 0), (1, 1), (8, 8), (30, 50)] {
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                let config = TaskbarConfig {
                    edge,
                    ..TaskbarConfig::bottom_bar(screen_w, screen_h)
                };
                let mut bar = Taskbar::new(config, &theme);
                bar.set_apps(alloc::vec![app("Editor")]);
                let layout = bar.layout(scale);
                let frame = layout.bar;
                let at = alloc::format!("{percent}% {screen_w}x{screen_h} {edge:?}");

                assert_eq!(
                    frame.intersection(&Rect::new(0, 0, screen_w, screen_h)),
                    frame,
                    "{at}: the bar stays on the screen"
                );
                for (label, region) in [
                    ("library", layout.library),
                    ("separator", layout.separator),
                    ("application strip", layout.app_strip),
                    ("application slot", layout.apps[0]),
                    ("notification area", layout.notification_area),
                    ("clock", layout.clock),
                    ("switchboard", layout.switchboard),
                ] {
                    if region.is_empty() {
                        continue;
                    }
                    assert_eq!(
                        region.intersection(&frame),
                        region,
                        "{at}: the {label} stays inside the bar"
                    );
                }
                // Painting a bar this small must not panic, and one that has
                // pixels at all still produces them.
                let painted = TaskbarRenderer::new(test_icon_cache())
                    .render(&bar, scale, &mut NoArtwork)
                    .is_some();
                assert!(
                    frame.is_empty() || painted,
                    "{at}: a bar with pixels still paints"
                );
            }
        }
    }
}

#[test]
fn overflowing_app_slot_is_clipped_to_empty() {
    let mut bar = bottom_bar();
    // The strip of this screen holds 15 whole 48 px slots, so 24 running
    // applications put the later slots well past its trailing edge.
    let apps: Vec<AppSlot> = (0..24).map(|_| app("App")).collect();
    bar.set_apps(apps);
    let layout = bar.layout(Scale::ONE);
    assert!(layout.apps[0].width > 0);
    assert!(
        layout.apps.last().expect("24 slots").is_empty(),
        "a slot past the region clips to empty"
    );
}

#[test]
fn an_app_slot_is_a_square_the_launchers_share() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("A window title far wider than any slot")]);
    for percent in [100, 200] {
        let scale = Scale::from_percent(percent).expect("a valid scale");
        let layout = bar.layout(scale);
        let slot = layout.apps[0];
        assert_eq!(
            (slot.width, slot.height),
            (layout.library.width, layout.library.height),
            "{percent}%: an icon-only slot takes a launcher's square, so the bar reads as one strip"
        );
        assert!(
            bar.app_icon_side(scale) > 0 && bar.app_icon_side(scale) <= slot.width,
            "{percent}%: and draws its icon inside it"
        );
    }
}

#[test]
fn the_app_strip_spans_the_launcher_to_the_trailing_end() {
    let mut bar = bottom_bar();
    // Empty: the strip is still the whole region between the launcher's
    // gutter and the trailing group, so a first application has somewhere to
    // land.
    let layout = bar.layout(Scale::ONE);
    assert!(layout.apps.is_empty());
    assert!(layout.app_strip.left() > layout.separator.right());
    assert_eq!(layout.app_strip.right(), layout.notification_area.left());

    bar.set_apps(alloc::vec![app("One"), app("Two")]);
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.apps.len(), 2);
    assert!(layout.app_strip.left() > layout.separator.right());
    assert_eq!(layout.apps[0].left(), layout.app_strip.left());
    assert_eq!(layout.apps[1].left(), layout.apps[0].right());
    assert_eq!(layout.apps[0].width, 48);
}

#[test]
fn app_slots_clip_fail_closed_on_a_tiny_screen() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(213, 40), &Theme::dark());
    // The Library launcher (48) plus the separator gutter (17) takes 65.
    // Switchboard (44) plus clock (80) take 124. Screen 213, less the two
    // 5 px margins the bar floats in and the two 1 px rims its content sits
    // inside, leaves 201. Remaining for the application strip: 201 - 189 = 12.
    bar.set_apps(alloc::vec![app("App")]);
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.library.width, 48);
    assert_eq!(layout.apps[0].width, 12, "the slot clips to fit");

    // Even smaller: the slot is empty and can never be hit.
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(201, 40), &Theme::dark());
    bar.set_apps(alloc::vec![app("App")]);
    let layout = bar.layout(Scale::ONE);
    assert!(layout.apps[0].is_empty());
    assert!(
        layout.app_strip.is_empty(),
        "with no room for a slot there is no strip either, so nothing can be hit"
    );
}

#[test]
fn app_strip_positions_on_all_four_edges() {
    let theme = Theme::dark();
    // Across the bar the strip spans the thickness less the two rims it sits
    // inside; along the bar each slot is one application extent.
    let border = plate_border(&theme, Scale::ONE);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let mut bar = Taskbar::new(config, &theme);
        bar.set_apps(alloc::vec![app("App")]);
        let layout = bar.layout(Scale::ONE);
        assert!(!layout.app_strip.is_empty(), "{edge:?}");
        assert_eq!(layout.apps.len(), 1, "{edge:?}");
        match edge.orientation() {
            Orientation::Horizontal => {
                assert_eq!(layout.app_strip.height, layout.bar.height - border * 2);
                assert_eq!(layout.apps[0].width, 48);
            }
            Orientation::Vertical => {
                assert_eq!(layout.app_strip.width, layout.bar.width - border * 2);
                assert_eq!(layout.apps[0].height, 48);
            }
        }
    }
}

#[test]
fn bar_pins_to_all_four_edges() {
    // Each bar stands off the three screen edges it faces by the 5 px
    // taskbar margin; only the side facing the work area keeps its place,
    // and the 40 px thickness is unchanged.
    for (edge, expect) in [
        (Edge::Top, Rect::new(5, 5, 990, 40)),
        (Edge::Bottom, Rect::new(5, 755, 990, 40)),
        (Edge::Left, Rect::new(5, 5, 40, 790)),
        (Edge::Right, Rect::new(955, 5, 40, 790)),
    ] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let bar = Taskbar::new(config, &Theme::dark());
        assert_eq!(bar.layout(Scale::ONE).bar, expect, "{edge:?}");
    }
}

// ---- the wallpaper margin -------------------------------------------

/// Saturating `u32` → `i32` for coordinate arithmetic in the assertions.
fn coord(value: u32) -> i32 {
    i32::try_from(value).expect("a test screen fits in an i32")
}

#[test]
fn the_bar_stands_off_the_screen_edges_it_faces_at_every_scale() {
    let theme = Theme::dark();
    let (screen_w, screen_h) = (1000, 800);
    for percent in [50, 100, 200, 400] {
        let scale = Scale::from_percent(percent).expect("a valid scale");
        let gap = coord(scale.scale_length(theme.metrics().taskbar_margin));
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            let config = TaskbarConfig {
                edge,
                ..TaskbarConfig::bottom_bar(screen_w, screen_h)
            };
            let thickness = scale.scale_length(config.thickness);
            let bar = Taskbar::new(config, &theme).layout(scale).bar;
            let at = |side: i32, want: i32, which: &str| {
                assert_eq!(side, want, "{edge:?} at {percent}%: the {which} side");
            };
            match edge {
                Edge::Top | Edge::Bottom => {
                    at(bar.left(), gap, "leading");
                    at(bar.right(), coord(screen_w) - gap, "trailing");
                    if edge == Edge::Top {
                        at(bar.top(), gap, "faced");
                    } else {
                        at(bar.bottom(), coord(screen_h) - gap, "faced");
                    }
                    assert_eq!(
                        bar.height, thickness,
                        "{edge:?} at {percent}%: the work-area side keeps its thickness"
                    );
                }
                Edge::Left | Edge::Right => {
                    at(bar.top(), gap, "leading");
                    at(bar.bottom(), coord(screen_h) - gap, "trailing");
                    if edge == Edge::Left {
                        at(bar.left(), gap, "faced");
                    } else {
                        at(bar.right(), coord(screen_w) - gap, "faced");
                    }
                    assert_eq!(
                        bar.width, thickness,
                        "{edge:?} at {percent}%: the work-area side keeps its thickness"
                    );
                }
            }
        }
    }
}

#[test]
fn the_wallpaper_gap_belongs_to_no_control() {
    let mut bar = bottom_bar();
    let (screen_w, screen_h) = (bar.config().screen_width, bar.config().screen_height);
    let layout = bar.layout(Scale::ONE);
    let mid_y = layout.bar.top() + coord(layout.bar.height / 2);
    // One pixel outside each of the three sides the bar stands off.
    for (which, point) in [
        ("below", Point::new(500, layout.bar.bottom())),
        ("left of", Point::new(layout.bar.left() - 1, mid_y)),
        ("right of", Point::new(layout.bar.right(), mid_y)),
    ] {
        assert!(
            point.x >= 0 && point.x < coord(screen_w) && point.y >= 0 && point.y < coord(screen_h),
            "{which} the bar: the probe is on screen, in the gap the margin leaves"
        );
        assert!(!layout.bar.contains(point), "{which} the bar");
        assert_eq!(layout.hit_test(point), None, "{which} the bar");
        assert_eq!(
            bar.hit_test(point, Scale::ONE),
            None,
            "{which} the bar, through the taskbar"
        );
        let mut input = TaskbarInput::new();
        assert_eq!(
            press_at(&mut input, &mut bar, point.x, point.y),
            TaskbarResponse::Ignored,
            "a press {which} the bar reaches the wallpaper"
        );
    }
}

#[test]
fn a_popup_opens_against_the_bar_and_clears_the_gap() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let frame = bar.layout(Scale::ONE).bar;
    let panel = bar.library_layout(Scale::ONE).panel;
    assert!(
        panel.bottom() <= frame.top(),
        "the popup opens above the bar, not over it"
    );
    assert!(panel.left() >= frame.left(), "clear of the leading gap");
    assert!(panel.right() <= frame.right(), "clear of the trailing gap");
}

#[test]
fn a_side_bar_readout_stays_within_the_bars_span() {
    // The Switchboard sits at the trailing end of a side bar, so its readout
    // is the popover most likely to be pushed past the bar — into the gap the
    // bar leaves at the bottom of the screen.
    for edge in [Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let mut bar = Taskbar::new(config, &Theme::dark());
        let mut summary = tray_summary(0, 0, 300);
        summary.top_task = Some(tray_task("editor", 250));
        bar.set_tray_summary(Some(summary));
        let mut input = TaskbarInput::new();
        hover_switchboard(&mut input, &mut bar);

        let frame = bar.layout(Scale::ONE).bar;
        let panel = bar
            .tray_readout_layout(Scale::ONE)
            .expect("hover expands")
            .panel;
        assert!(panel.top() >= frame.top(), "{edge:?}: clear of the top gap");
        assert!(
            panel.bottom() <= frame.bottom(),
            "{edge:?}: clear of the bottom gap"
        );
    }
}

#[test]
fn a_screen_too_small_for_the_margin_keeps_the_bar_rather_than_the_gap() {
    for percent in [100, 400] {
        let scale = Scale::from_percent(percent).expect("a valid scale");
        for (w, h) in [(0, 0), (1, 1), (8, 8), (30, 50), (77, 50)] {
            let bar = Taskbar::new(TaskbarConfig::bottom_bar(w, h), &Theme::dark());
            let layout = bar.layout(scale);
            let rect = layout.bar;
            let at = alloc::format!("{w}x{h} at {percent}%");
            assert!(
                rect.left() >= 0 && rect.top() >= 0,
                "{at}: no negative origin"
            );
            assert!(rect.right() <= coord(w), "{at}: never past the right edge");
            assert!(
                rect.bottom() <= coord(h),
                "{at}: never past the bottom edge"
            );
            assert_eq!(
                rect.is_empty(),
                w == 0 || h == 0,
                "{at}: a screen with room keeps a bar, the margin never takes its last pixel"
            );
            assert_eq!(layout.hit_test(Point::new(-1, -1)), None, "{at}");
        }
    }
}

// ---- DPI / scale ----------------------------------------------------

#[test]
fn doubling_the_scale_doubles_logical_lengths() {
    let bar = bottom_bar();
    let one = bar.layout(Scale::ONE);
    let two = bar.layout(Scale::from_percent(200).expect("a valid scale"));
    assert_eq!(two.library.width, one.library.width * 2);
    assert_eq!(two.clock.width, one.clock.width * 2);
    assert_eq!(two.bar.height, one.bar.height * 2);
    assert_eq!(two.corner_radius, one.corner_radius * 2);
    // The physical screen is unchanged, so the doubled bar still spans it —
    // less the margin it floats in, which doubles with everything else (5
    // logical pixels at each end becomes 10 physical).
    assert_eq!(two.bar.width, 980);
    assert_eq!(two.bar.left(), 10);
}

#[test]
fn hit_testing_follows_the_scale() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
    let scale = Scale::from_percent(200).expect("a valid scale");
    // At 2x the bar starts 10 physical pixels in (the doubled margin) and its
    // content 2 further (the doubled rim), the Library button spans 96
    // physical pixels and the separator gutter 34, so the application strip
    // starts at 142 and the gutter before it is bare.
    assert_eq!(bar.hit_test(Point::new(90, 780), scale), Some(Hit::Library));
    assert_eq!(bar.hit_test(Point::new(120, 780), scale), None);
    assert_eq!(bar.hit_test(Point::new(142, 780), scale), Some(Hit::App(0)));
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
fn set_apps_clamps_a_stale_hover() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("One"), app("Two")]);
    let layout = bar.layout(Scale::ONE);
    let second = centre_of(layout.apps[1]);
    bar.track_hover(Some(second), Scale::ONE, &mut damage::sink());
    assert_eq!(bar.apps().hover(), Some(1));

    // Replace with one slot: the hover is clamped away rather than left
    // naming a slot that is gone.
    bar.set_apps(alloc::vec![app("One")]);
    assert_eq!(bar.apps().hover(), None);
}

#[test]
fn app_slot_accessors_report_what_the_session_resolved() {
    let mut bar = bottom_bar();
    let art = Surface::filled(16, 16, Color::rgb(255, 0, 255).premultiply()).expect("artwork");
    bar.set_apps(alloc::vec![app("Editor")
        .with_artwork(art)
        .with_windows(alloc::vec![TaskId(1), TaskId(2)])
        .with_declaration(declared_menu(), true)
        .with_identity(identity("Editor"))]);

    assert_eq!(bar.apps().len(), 1);
    assert!(!bar.apps().is_empty());
    let slot = bar.apps().get(0).expect("one slot");
    assert_eq!(slot.label(), "Editor");
    assert_eq!(slot.icon(), IconKind::AppBundle);
    assert!(slot.artwork().is_some());
    assert_eq!(slot.windows(), &[TaskId(1), TaskId(2)]);
    assert!(slot.handles_default());
    assert_eq!(slot.menu(), &declared_menu());
    assert_eq!(slot.identity(), &identity("Editor"));
}

#[test]
fn an_undeclared_app_slot_carries_its_label_as_its_whole_identity() {
    // A process the session gave a slot because it owns a window, without a
    // declaration of its own: no menu, no default action, and an identity
    // that states only the name — never a fabricated version or author.
    let slot = app("Unattributed");
    assert!(slot.menu().is_empty());
    assert!(!slot.handles_default());
    assert!(slot.windows().is_empty());
    assert_eq!(slot.identity().name, "Unattributed");
    assert_eq!(slot.identity().version, "");
    assert_eq!(slot.identity().purpose, None);
    assert_eq!(slot.identity().author, None);
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
fn a_click_on_a_declared_default_action_reaches_the_application() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Terminal")
        .with_windows(alloc::vec![TaskId(1)])
        .with_declaration(declared_menu(), true)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::AppDefault { app: 0 },
        "the application declared it handles the click, so it gets it"
    );
    assert_eq!(
        bar.tasks().focused(),
        None,
        "the session raises nothing behind the application's back"
    );
}

#[test]
fn a_click_with_no_default_action_raises_the_applications_window() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(4), "Widgets");
    bar.set_apps(alloc::vec![app("Widgets")
        .with_windows(alloc::vec![TaskId(4)])
        .with_declaration(declared_menu(), false)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::AppRaise { app: 0 }
    );
}

#[test]
fn a_click_on_an_application_with_no_windows_and_no_action_does_nothing() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![
        app("Idle").with_declaration(declared_menu(), false)
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::Ignored,
        "nothing to raise and nothing declared: the honest answer is nothing"
    );
}

#[test]
fn a_second_click_on_the_focused_application_never_minimises_it() {
    // A slot is an application, not a window: the old click-to-minimise
    // toggle is gone, and minimising lives on the title bar and in the
    // picker instead.
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().set_focused(Some(TaskId(1)));
    bar.set_apps(alloc::vec![
        app("Editor").with_windows(alloc::vec![TaskId(1)])
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::AppRaise { app: 0 }
    );
    assert!(!bar.tasks().is_minimised(TaskId(1)));
    assert_eq!(bar.tasks().focused(), Some(TaskId(1)));
}

#[test]
fn status_icon_and_clock_presses_are_claimed_and_inert() {
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
    // The clock is the other one: a left click on it acts on nothing and
    // opens nothing. Its menu answers a secondary press.
    let clock = centre_of(layout.clock);
    assert_eq!(
        press_at(&mut input, &mut bar, clock.x, clock.y),
        TaskbarResponse::Ignored
    );
    assert!(
        !bar.menu().is_open(),
        "a left click on the clock opened a menu"
    );
}

/// Seat one application on the bar and open the menu it declared with a
/// secondary press on its slot, returning the slot's centre.
fn open_app_menu(input: &mut TaskbarInput, bar: &mut Taskbar) -> Point {
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    input.handle(
        InputEvent::PointerMoved { to: slot },
        bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            },
            bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::Ignored,
        "opening a menu acts on nothing by itself"
    );
    slot
}

/// A bar with one application whose declared menu is [`declared_menu`].
fn bar_with_declared_app() -> Taskbar {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Terminal")
        .with_declaration(declared_menu(), true)
        .with_identity(identity("Terminal"))]);
    let _ = bar.take_repaint();
    bar
}

#[test]
fn secondary_press_on_an_app_slot_opens_the_menu_it_declared() {
    let mut bar = bar_with_declared_app();
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);
    assert!(bar.menu().is_open());
    assert_eq!(
        bar.menu().subject(),
        Some(&MenuSubject::App {
            index: 0,
            menu: declared_menu(),
            identity: identity("Terminal"),
        })
    );
}

#[test]
fn an_application_that_declared_no_menu_opens_nothing() {
    let mut bar = bottom_bar();
    // A process the session gave a slot for its windows alone: no
    // declaration, so a secondary press is claimed and shows no plate.
    bar.set_apps(alloc::vec![app("Unattributed")]);
    let _ = bar.take_repaint();
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);
    assert!(
        !bar.menu().is_open(),
        "the bar never invents a menu on an application's behalf"
    );
    assert!(
        bar.menu_layout(Scale::ONE).is_none(),
        "and there is nothing to present"
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "only the slot's own hover feedback changed"
    );
}

#[test]
fn the_menu_draws_exactly_the_rows_the_application_declared() {
    let mut menu = AppMenu::EMPTY;
    menu.push(item(1, "New window")).expect("fits");
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(2).expect("non-zero"),
        label: menu_label("Wrap lines"),
        enabled: true,
        mark: AppMenuMark::Check,
    })
    .expect("fits");
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(3).expect("non-zero"),
        label: menu_label("Green screen"),
        enabled: true,
        mark: AppMenuMark::Radio,
    })
    .expect("fits");
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(4).expect("non-zero"),
        label: menu_label("Paste"),
        enabled: false,
        mark: AppMenuMark::None,
    })
    .expect("fits");
    menu.push(AppMenuRow::Separator).expect("fits");
    menu.push(AppMenuRow::Submenu {
        label: menu_label("Profile"),
        enabled: true,
    })
    .expect("fits");
    menu.push_under(item(5, "Amber"), 5).expect("fits");
    menu.push(AppMenuRow::About).expect("fits");

    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Terminal")
        .with_declaration(menu, true)
        .with_identity(identity("Terminal"))]);
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);

    // Every top-level declared row draws, in declaration order, with the
    // enablement and mark the application asked for; the declared separator
    // opens the group its next row begins rather than becoming a row; the
    // submenu's own child is not a top-level row; and the About row is the
    // one row whose label and submenu are the bar's.
    assert_eq!(
        bar.menu().control().items(),
        &[
            MenuItem::new("New window"),
            MenuItem::new("Wrap lines").with_mark(MenuMark::Check),
            MenuItem::new("Green screen").with_mark(MenuMark::Radio),
            MenuItem::new("Paste").with_state(ControlState::disabled()),
            MenuItem::new("Profile")
                .with_submenu(true)
                .with_group_break(true),
            MenuItem::new(INFO_ROW_LABEL).with_submenu(true),
        ]
    );
}

#[test]
fn a_menu_at_the_row_cap_draws_every_row_it_declared() {
    // The format bound is a bound, not a truncation point: a menu that fills
    // it draws all of its rows, and nothing beyond it can be declared.
    let mut menu = AppMenu::EMPTY;
    for row in 0..APP_MENU_MAX_ROWS {
        let id = u16::try_from(row + 1).expect("a small id");
        menu.push(item(id, "Row")).expect("fits");
    }
    assert_eq!(menu.len(), APP_MENU_MAX_ROWS);
    assert!(menu.push(item(99, "Overflow")).is_err(), "the cap holds");

    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Busy")
        .with_declaration(menu, false)
        .with_identity(identity("Busy"))]);
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);
    assert_eq!(bar.menu().control().items().len(), APP_MENU_MAX_ROWS);

    // The last row is still reachable and reports its own id.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::End));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::AppMenuChosen {
            app: 0,
            item: AppMenuItemId::new(u16::try_from(APP_MENU_MAX_ROWS).expect("small"))
                .expect("non-zero"),
        }
    );
}

#[test]
fn a_disabled_declared_row_cannot_be_chosen() {
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Item {
        id: AppMenuItemId::new(9).expect("non-zero"),
        label: menu_label("Paste"),
        enabled: false,
        mark: AppMenuMark::None,
    })
    .expect("fits");
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Terminal")
        .with_declaration(menu, true)
        .with_identity(identity("Terminal"))]);
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::Ignored,
        "a disabled row never acts"
    );
    assert!(bar.menu().is_open(), "and never closes the menu either");
}

#[test]
fn choosing_a_declared_row_relays_the_applications_own_id() {
    let mut bar = bar_with_declared_app();
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);

    // Down/Enter chooses the first declared row, "New window" (id 1).
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::AppMenuChosen {
            app: 0,
            item: AppMenuItemId::new(1).expect("non-zero"),
        }
    );
    assert!(!bar.menu().is_open(), "a choice closes the menu");

    // Down/Down/Enter chooses "Quit" (id 2) — the separator between them is
    // not a row, so the second press lands on the command after it.
    open_app_menu(&mut input, &mut bar);
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::AppMenuChosen {
            app: 0,
            item: AppMenuItemId::new(2).expect("non-zero"),
        }
    );
}

#[test]
fn a_declared_submenu_opens_and_its_rows_report_their_own_ids() {
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Submenu {
        label: menu_label("Profile"),
        enabled: true,
    })
    .expect("fits");
    menu.push_under(item(7, "Amber"), 0).expect("fits");
    menu.push_under(item(8, "Green"), 0).expect("fits");
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Terminal")
        .with_declaration(menu, true)
        .with_identity(identity("Terminal"))]);
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);

    // Right opens the submenu the highlighted row hangs off itself.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Right));
    let submenu = bar.menu().submenu().expect("the submenu is open");
    assert_eq!(
        submenu
            .items()
            .iter()
            .map(MenuItem::label)
            .collect::<Vec<_>>(),
        alloc::vec!["Amber", "Green"]
    );
    let layout = bar.menu_layout(Scale::ONE).expect("menu layout");
    assert!(!layout.child.is_empty(), "and lays out beside the plate");
    assert_eq!(layout.bounds(), layout.panel.union(&layout.child));

    // Down/Down/Enter inside it chooses "Green" (id 8).
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::AppMenuChosen {
            app: 0,
            item: AppMenuItemId::new(8).expect("non-zero"),
        }
    );
    assert!(!bar.menu().is_open());
}

#[test]
fn escape_inside_a_declared_submenu_closes_only_the_submenu() {
    let mut menu = AppMenu::EMPTY;
    menu.push(AppMenuRow::Submenu {
        label: menu_label("Profile"),
        enabled: true,
    })
    .expect("fits");
    menu.push_under(item(7, "Amber"), 0).expect("fits");
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Terminal")
        .with_declaration(menu, true)
        .with_identity(identity("Terminal"))]);
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Right));
    assert!(bar.menu().submenu().is_some());

    press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape));
    assert!(bar.menu().submenu().is_none(), "one key, one step back");
    assert!(bar.menu().is_open());
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape));
    assert!(!bar.menu().is_open());
}

#[test]
fn about_opens_the_manifest_attested_information_panel() {
    let mut bar = bar_with_declared_app();
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);

    // The About row is the last of the three top-level rows.
    press_key(&mut input, &mut bar, Key::Named(NamedKey::End));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Right));
    let facts = bar.menu().info_panel().expect("the panel is open");
    assert_eq!(
        facts
            .facts()
            .iter()
            .map(|fact| (fact.label(), fact.value()))
            .collect::<Vec<_>>(),
        alloc::vec![
            ("Name", "Terminal"),
            ("Version", "1.2.3"),
            ("Purpose", "Does one thing well"),
            ("Author", "TAIRiX"),
        ],
        "the panel states the signed manifest, never what the process claims"
    );
    assert!(
        bar.menu().submenu().is_none(),
        "the panel is facts, not a menu of rows"
    );
    let layout = bar.menu_layout(Scale::ONE).expect("menu layout");
    assert!(!layout.child.is_empty());

    // A pointer over the panel is claimed and offers nothing to choose.
    let inside = centre_of(layout.child);
    assert_eq!(
        press_at(&mut input, &mut bar, inside.x, inside.y),
        TaskbarResponse::Ignored
    );
    assert!(bar.menu().is_open());

    // The panel is attached to the menu: dismissing the menu takes it away.
    assert_eq!(
        press_at(&mut input, &mut bar, 500, 100),
        TaskbarResponse::Ignored
    );
    assert!(!bar.menu().is_open());
    assert!(
        bar.menu().info_panel().is_none(),
        "the panel disappears with the menu that carried it"
    );
}

#[test]
fn a_manifest_without_purpose_or_author_states_only_what_it_has() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Sparse")
        .with_declaration(declared_menu(), false)
        .with_identity(AppIdentity {
            name: String::from("Sparse"),
            version: String::from("0.1"),
            purpose: None,
            author: None,
        })]);
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);
    press_key(&mut input, &mut bar, Key::Named(NamedKey::End));
    press_key(&mut input, &mut bar, Key::Named(NamedKey::Right));
    let facts = bar.menu().info_panel().expect("the panel is open");
    assert_eq!(
        facts
            .facts()
            .iter()
            .map(|fact| (fact.label(), fact.value()))
            .collect::<Vec<_>>(),
        alloc::vec![("Name", "Sparse"), ("Version", "0.1")],
        "an omitted field is absent, never a blank row"
    );
}

#[test]
fn menu_is_modal_and_dismisses_on_click_away_or_escape() {
    let mut bar = bar_with_declared_app();
    let mut input = TaskbarInput::new();
    let slot = open_app_menu(&mut input, &mut bar);
    assert!(bar.menu().is_open());

    // Motion over the menu highlights rows. The pointer also leaves the
    // application slot it started on, so the bar's own hover feedback
    // latches too.
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
        "leaving the slot repaints the bar, and the new highlight repaints the menu"
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

    // Move pointer back to the slot before reopening.
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
    let mut bar = bar_with_declared_app();
    let mut input = TaskbarInput::new();
    open_app_menu(&mut input, &mut bar);
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
fn entry_menu_launches_the_row_it_was_opened_on() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let entry_id = open_entry_menu(&mut input, &mut bar);

    // Launching from the entry menu behaves exactly like launching from the
    // row itself, and closes the popup.
    assert_eq!(
        choose_entry_row(&mut input, &mut bar, EntryRow::Open),
        TaskbarResponse::LibraryLaunch { entry: entry_id }
    );
    assert!(!bar.menu().is_open());
    assert!(!bar.library().is_open());
}

#[test]
fn entry_menu_asks_the_session_for_a_desktop_shortcut() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let entry_id = open_entry_menu(&mut input, &mut bar);

    // The bar writes nothing: it names the entry and the session, which
    // holds the filesystem capability, makes the link. The popup closes for
    // the same reason a launch closes it — it is modal, and the shortcut
    // appears on the desktop behind it.
    assert_eq!(
        choose_entry_row(&mut input, &mut bar, EntryRow::Shortcut),
        TaskbarResponse::CreateDesktopShortcut { entry: entry_id }
    );
    assert!(!bar.menu().is_open());
    assert!(!bar.library().is_open());
}

#[test]
fn entry_menu_offers_exactly_the_two_rows_it_defines() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_entry_menu(&mut input, &mut bar);

    let labels: Vec<&str> = bar
        .menu()
        .control()
        .items()
        .iter()
        .map(MenuItem::label)
        .collect();
    assert_eq!(
        labels,
        alloc::vec![EntryRow::Open.label(), EntryRow::Shortcut.label()],
        "the rows the menu draws are the ones it defines, in order"
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

// ---- the hover window picker ------------------------------------------

/// One picker cell per window the list holds, captioned with its title.
fn cells(bar: &Taskbar, app: usize) -> Vec<PickerEntry> {
    bar.apps()
        .get(app)
        .expect("a slot")
        .windows()
        .iter()
        .map(|&id| {
            let title = bar
                .tasks()
                .entries()
                .iter()
                .find(|entry| entry.id == id)
                .map_or("", |entry| entry.title.as_str());
            PickerEntry::new(id, title)
        })
        .collect()
}

/// The bar cannot see the window stack, so it cannot tell a clock the user is
/// looking at from a clock a window is drawn over. The desktop's seat can, and
/// says so: a [`PointerFocus::Left`] drops every hover the bar is drawing.
///
/// The picker is the sharpest case, and it cuts both ways. It hangs a gap away
/// from the bar, so a pointer travelling from the slot to a cell leaves the
/// bar's surfaces *on the way there* — taking the panel down on that crossing
/// would make choosing a window impossible. But a pointer that has genuinely
/// settled elsewhere must not leave a panel of window thumbnails floating over
/// whatever the user is now working in. So leaving starts the grace, and the
/// clock ends it.
#[test]
fn a_pointer_that_left_the_bar_drops_its_hover_and_lets_the_picker_go() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    dwell_on(&mut input, &mut bar, slot);
    assert!(bar.picker().is_open());
    assert_eq!(bar.apps().hover(), Some(0));
    let _ = bar.take_repaint();

    // Nothing has moved: the pointer is still at the slot's own centre.
    input.set_pointer_focus(PointerFocus::Left, &mut bar, Scale::ONE);

    assert_eq!(bar.apps().hover(), None, "the slot stayed lit");
    assert!(
        bar.picker().is_open(),
        "the panel went down on the crossing towards it"
    );
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR,
        "only the slot that unlit repaints; the panel is unchanged"
    );

    // The grace is the picker's whole remaining lease.
    let left_ns = NOW_NS + PICKER_OPEN_DELAY_NS;
    assert_eq!(
        input.park_deadline_ns(left_ns, u64::MAX),
        PICKER_CLOSE_GRACE_NS,
        "the park is shortened to exactly the grace"
    );
    assert_eq!(
        input.tick(&mut bar, left_ns + PICKER_CLOSE_GRACE_NS - 1),
        TaskbarResponse::Ignored
    );
    assert!(bar.picker().is_open(), "it went a moment early");

    assert_eq!(
        input.tick(&mut bar, left_ns + PICKER_CLOSE_GRACE_NS),
        TaskbarResponse::Ignored,
        "taking a panel down asks nothing of the embedder"
    );
    assert!(!bar.picker().is_open(), "the picker was left open");
    assert_eq!(bar.take_repaint(), TaskbarRepaint::PICKER);

    // With nothing left pending the bar arms no timer at all.
    assert_eq!(input.park_deadline_ns(left_ns, u64::MAX), u64::MAX);
}

/// The pointer can arrive without moving — the window above the bar closed —
/// and the hover under it has to appear. What must *not* appear is the hover
/// picker: a window closing is not a gesture, and a popover that opens because
/// something else vanished is one nobody asked for. The next real motion opens
/// it.
#[test]
fn a_pointer_that_entered_the_bar_hovers_without_opening_a_hover_surface() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    let _ = bar.take_repaint();

    input.set_pointer_focus(PointerFocus::Entered { at: slot }, &mut bar, Scale::ONE);

    assert_eq!(bar.apps().hover(), Some(0), "the slot under it is hovered");
    assert_eq!(input.pointer(), slot, "and the position was adopted");
    assert!(
        !bar.picker().is_open(),
        "an arrival is not a gesture: it opened a hover surface"
    );
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);
    assert_eq!(
        input.park_deadline_ns(NOW_NS, u64::MAX),
        u64::MAX,
        "an arrival arms no dwell either"
    );

    // A real motion over the same slot is a gesture, and does ask — once the
    // pointer has rested there.
    dwell_on(&mut input, &mut bar, slot);
    assert_eq!(bar.picker().app(), Some(0));
}

/// The Switchboard capsule's readout opens *above* the bar, so a window that
/// covers the bar need not cover the readout: it is the case where a stranded
/// hover surface is plainly visible. It collapses with the pointer that opened
/// it.
#[test]
fn a_pointer_that_left_the_bar_collapses_the_capsules_readout() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);
    assert!(bar.tray().is_expanded());
    let _ = bar.take_repaint();

    input.set_pointer_focus(PointerFocus::Left, &mut bar, Scale::ONE);

    assert!(!bar.tray().is_expanded(), "the readout stayed expanded");
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR | TaskbarRepaint::READOUT
    );
    assert!(bar.tray_readout_layout(Scale::ONE).is_none());
}

#[test]
fn hovering_one_window_asks_for_no_picker() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Only");
    bar.set_apps(alloc::vec![
        app("Editor").with_windows(alloc::vec![TaskId(1)])
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        moved_at(&mut input, &mut bar, slot, NOW_NS),
        TaskbarResponse::Ignored,
        "one window is no choice, so sweeping the bar pops nothing up"
    );
    assert!(!bar.picker().is_open());
    assert!(bar.picker_layout(Scale::ONE).is_none());
}

/// A pointer resting on a multi-window slot asks for the picker — but only
/// after it has rested. A sweep across the bar on the way to something else
/// has asked for nothing, which is the whole reason the dwell exists.
#[test]
fn resting_on_two_windows_asks_for_the_picker_once_the_dwell_elapses() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);

    assert_eq!(
        moved_at(&mut input, &mut bar, slot, NOW_NS),
        TaskbarResponse::Ignored,
        "arriving on the slot asks for nothing yet"
    );
    assert_eq!(
        input.dwelling_app(),
        Some(0),
        "but the embedder is told whose thumbnails to prepare"
    );
    assert_eq!(
        input.park_deadline_ns(NOW_NS, u64::MAX),
        PICKER_OPEN_DELAY_NS,
        "and the park is shortened to the dwell"
    );

    // A hand that jitters mid-dwell does not restart it.
    let mid_ns = NOW_NS + PICKER_OPEN_DELAY_NS / 2;
    assert_eq!(
        moved_at(&mut input, &mut bar, slot, mid_ns),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        input.park_deadline_ns(mid_ns, u64::MAX),
        PICKER_OPEN_DELAY_NS - PICKER_OPEN_DELAY_NS / 2,
        "the deadline is the original rest's, not a fresh one"
    );

    // The clock resolves it: the pointer need not move again.
    assert_eq!(
        input.tick(&mut bar, NOW_NS + PICKER_OPEN_DELAY_NS),
        TaskbarResponse::ShowWindowPicker { app: 0 }
    );
    // The bar owns no window pixels, so it asks and waits: nothing is open
    // until the embedder answers with the cells.
    assert!(!bar.picker().is_open());
    assert_eq!(input.dwelling_app(), None, "the dwell is spent");

    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    assert!(bar.picker().is_open());
    assert_eq!(bar.picker().app(), Some(0));
    assert_eq!(bar.picker().entries().len(), 2);
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR | TaskbarRepaint::PICKER,
        "the slot took the pointer's wash on the way in, and the picker opened"
    );

    // A further sample over the same slot asks again for nothing.
    assert_eq!(
        moved_at(&mut input, &mut bar, slot, NOW_NS + PICKER_OPEN_DELAY_NS),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        input.park_deadline_ns(NOW_NS + PICKER_OPEN_DELAY_NS, u64::MAX),
        u64::MAX,
        "an open picker under the pointer arms no timer"
    );
}

/// A pointer that crosses a multi-window slot without stopping opens nothing:
/// the dwell is a rest, and moving on cancels it.
#[test]
fn sweeping_across_a_slot_opens_nothing() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);

    let _ = moved_at(&mut input, &mut bar, slot, NOW_NS);
    // Off the slot again well inside the dwell.
    let _ = moved_at(&mut input, &mut bar, Point::new(500, 400), NOW_NS + 1);

    assert_eq!(input.dwelling_app(), None);
    assert_eq!(
        input.park_deadline_ns(NOW_NS + 1, u64::MAX),
        u64::MAX,
        "nothing is pending, so nothing wakes the desktop"
    );
    assert_eq!(
        input.tick(&mut bar, NOW_NS + PICKER_OPEN_DELAY_NS * 4),
        TaskbarResponse::Ignored
    );
    assert!(!bar.picker().is_open());
}

#[test]
fn the_picker_refuses_fewer_cells_than_a_choice() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Only");
    bar.set_apps(alloc::vec![
        app("Editor").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    let _ = bar.take_repaint();

    // Fewer than PICKER_MIN_WINDOWS cells: a picker with nothing to choose
    // is not a picker, so it is refused and nothing repaints.
    assert_eq!(PICKER_MIN_WINDOWS, 2);
    bar.show_window_picker(
        0,
        alloc::vec![PickerEntry::new(TaskId(1), "Only")],
        Scale::ONE,
    );
    assert!(!bar.picker().is_open());
    bar.show_window_picker(0, Vec::new(), Scale::ONE);
    assert!(!bar.picker().is_open());
    assert_eq!(bar.take_repaint(), TaskbarRepaint::NONE);

    // An unknown application is refused for the same reason.
    bar.show_window_picker(
        9,
        alloc::vec![
            PickerEntry::new(TaskId(1), "A"),
            PickerEntry::new(TaskId(2), "B")
        ],
        Scale::ONE,
    );
    assert!(!bar.picker().is_open());
}

#[test]
fn the_picker_lays_a_cell_out_per_window_and_stays_on_screen() {
    let mut bar = bottom_bar();
    for id in 1..=3 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![app("Terminal").with_windows(alloc::vec![
        TaskId(1),
        TaskId(2),
        TaskId(3)
    ])]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);

    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let mut edged = bar.clone();
        edged.set_config(TaskbarConfig {
            edge,
            ..*bar.config()
        });
        edged.show_window_picker(0, cells(&edged, 0), Scale::ONE);
        let layout = edged.picker_layout(Scale::ONE).expect("open");
        let screen = Rect::new(0, 0, 1000, 800);
        assert_eq!(
            layout.panel.intersection(&screen),
            layout.panel,
            "{edge:?}: the plate stays on the screen"
        );
        assert_eq!(layout.cells.len(), 3, "{edge:?}");
        for (index, cell) in layout.cells.iter().enumerate() {
            assert!(!cell.is_empty(), "{edge:?}: cell {index} fits");
            assert_eq!(
                cell.intersection(&layout.panel),
                *cell,
                "{edge:?}: cell {index} lies inside the plate"
            );
        }
        assert_eq!(
            layout.corner_radius,
            Theme::dark().metrics().popup_corner_radius,
            "{edge:?}: the plate and the window round together"
        );
    }
}

/// A window's cell is never laid out where it cannot be clicked. Cells wrap
/// into a grid sized to the space beside the bar, and a grid with more rows
/// than that space shows scrolls — so *every* window of an application with
/// far more of them than fit across the screen can be reached.
#[test]
fn every_window_is_reachable_however_many_there_are() {
    let mut bar = bottom_bar();
    // The most windows one client may hold open, so the worst case the
    // window channel admits is the case this pins.
    let windows: Vec<TaskId> = (1..=32).map(TaskId).collect();
    for &id in &windows {
        bar.tasks_mut().add(id, "W");
    }
    bar.set_apps(alloc::vec![app("Terminal").with_windows(windows.clone())]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let mut input = TaskbarInput::new();

    let layout = bar.picker_layout(Scale::ONE).expect("open");
    let screen = Rect::new(0, 0, 1000, 800);
    assert_eq!(
        layout.panel.intersection(&screen),
        layout.panel,
        "the plate stays on the screen"
    );
    assert!(
        layout.columns > 1 && layout.visible_rows > 1,
        "the cells wrapped into a grid, not a strip: {layout:?}"
    );
    assert!(
        layout.scrollbar.is_some(),
        "a grid taller than the space beside the bar states its scroll"
    );

    // Every cell that is laid out lies inside the plate, and no two overlap.
    for (index, cell) in layout.cells.iter().enumerate() {
        if cell.is_empty() {
            continue;
        }
        assert_eq!(
            cell.intersection(&layout.panel),
            *cell,
            "cell {index} lies inside the plate"
        );
        for (other, second) in layout.cells.iter().enumerate().skip(index + 1) {
            assert!(
                second.is_empty() || second.intersection(cell).is_empty(),
                "cells {index} and {other} overlap"
            );
        }
    }

    // Scrolling reaches every one of them: the pointer rests on the panel and
    // the wheel walks the grid to its end.
    let mut reached = alloc::vec![false; windows.len()];
    let _ = moved_at(&mut input, &mut bar, centre_of(layout.panel), NOW_NS);
    for _ in 0..windows.len() {
        let layout = bar.picker_layout(Scale::ONE).expect("open");
        for (index, cell) in layout.cells.iter().enumerate() {
            if cell.is_empty() {
                continue;
            }
            assert_eq!(
                bar.picker().cell_at(&layout, centre_of(*cell)),
                Some(index),
                "cell {index} is hittable at its own centre"
            );
            if let Some(seen) = reached.get_mut(index) {
                *seen = true;
            }
        }
        assert_eq!(
            input.handle(
                InputEvent::PointerScrolled { dx: 0, dy: 1 },
                &mut bar,
                Scale::ONE,
                NOW_NS,
            ),
            TaskbarResponse::Ignored,
            "a wheel tick over the panel is the grid's, not the task list's"
        );
    }
    assert!(
        reached.iter().all(|&seen| seen),
        "these windows could never be selected: {:?}",
        reached
            .iter()
            .enumerate()
            .filter(|(_, &seen)| !seen)
            .map(|(index, _)| index)
            .collect::<Vec<usize>>()
    );
}

/// A screen with room for barely one cell still lays that one out, keeps it
/// inside the plate, and scrolls to the rest rather than laying out cells
/// nobody can click.
#[test]
fn a_grid_too_big_for_the_screen_scrolls_instead_of_clipping() {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(300, 400), &Theme::dark());
    let windows: Vec<TaskId> = (1..=6).map(TaskId).collect();
    for &id in &windows {
        bar.tasks_mut().add(id, "W");
    }
    bar.set_apps(alloc::vec![app("Terminal").with_windows(windows.clone())]);
    bar.show_window_picker(
        0,
        windows
            .iter()
            .map(|&id| PickerEntry::new(id, "W"))
            .collect(),
        Scale::ONE,
    );
    let layout = bar.picker_layout(Scale::ONE).expect("open");
    assert_eq!(
        layout.cells.len(),
        6,
        "one cell per window, laid out or not"
    );
    assert!(
        layout.cells.iter().any(|cell| !cell.is_empty()),
        "the screen holds at least one"
    );
    assert!(
        layout.scrollbar.is_some(),
        "and the rest are reached by scrolling"
    );
    assert_eq!(
        layout.panel.intersection(&Rect::new(0, 0, 300, 400)),
        layout.panel
    );
    for cell in layout.cells.iter().filter(|cell| !cell.is_empty()) {
        assert_eq!(cell.intersection(&layout.panel), *cell);
    }
    // A cell outside the visible rows is never the answer to a hit test.
    let empty_index = layout
        .cells
        .iter()
        .position(Rect::is_empty)
        .expect("a scrolled-away cell");
    for point in [Point::ORIGIN, centre_of(layout.panel)] {
        assert_ne!(bar.picker().cell_at(&layout, point), Some(empty_index));
    }
}

/// A grid re-columned under a scrolled panel — the desktop's density changed
/// — must not leave a first row past its own last one, which would lay out no
/// cell at all and show a blank plate nothing could be chosen from.
#[test]
fn a_grid_re_columned_under_a_scrolled_panel_still_lays_cells_out() {
    let mut bar = bottom_bar();
    let windows: Vec<TaskId> = (1..=32).map(TaskId).collect();
    for &id in &windows {
        bar.tasks_mut().add(id, "W");
    }
    bar.set_apps(alloc::vec![app("Terminal").with_windows(windows.clone())]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let mut input = TaskbarInput::new();

    // Scroll to the end of the grid at this density.
    let panel = centre_of(bar.picker_layout(Scale::ONE).expect("open").panel);
    let _ = moved_at(&mut input, &mut bar, panel, NOW_NS);
    for _ in 0..windows.len() {
        let _ = input.handle(
            InputEvent::PointerScrolled { dx: 0, dy: 1 },
            &mut bar,
            Scale::ONE,
            NOW_NS,
        );
    }

    // Coarser densities shrink the cells, so the grid holds more of them per
    // row and has fewer rows than the offset names.
    for percent in [25, 35, 50, 75] {
        let scale = Scale::from_percent(percent).expect("a scale");
        let layout = bar.picker_layout(scale).expect("open");
        assert!(
            layout.cells.iter().any(|cell| !cell.is_empty()),
            "at {percent}% the panel laid out no cell at all: {layout:?}"
        );
    }
}

/// A dwell counts for the *slot* the pointer rested on. If the strip is
/// re-pushed while it runs — an application opened or closed a window — the
/// index it named may now be somebody else's, and their windows are not what
/// the user asked to see.
#[test]
fn a_dwell_whose_slot_moved_under_it_opens_nothing() {
    let mut bar = bottom_bar();
    for id in 1..=4 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)]),
        app("Editor").with_windows(alloc::vec![TaskId(3), TaskId(4)]),
    ]);
    let mut input = TaskbarInput::new();
    let second = centre_of(bar.layout(Scale::ONE).apps[1]);
    let _ = moved_at(&mut input, &mut bar, second, NOW_NS);
    assert_eq!(input.dwelling_app(), Some(1));

    // The leading application exits, so what was slot 1 is now slot 0 and the
    // pointer rests over nothing.
    bar.set_apps(alloc::vec![
        app("Editor").with_windows(alloc::vec![TaskId(3), TaskId(4)])
    ]);
    assert_eq!(
        input.tick(&mut bar, NOW_NS + PICKER_OPEN_DELAY_NS),
        TaskbarResponse::Ignored,
        "the dwell opened a picker for a slot that had moved"
    );
    assert!(!bar.picker().is_open());
}

/// A thumbnail the embedder scaled after the picker opened lands in that
/// window's cell and repaints the panel alone.
#[test]
fn a_late_thumbnail_lands_in_its_own_cell() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let _ = bar.take_repaint();
    assert!(
        bar.picker().entries()[1].thumbnail().is_none(),
        "it opened on the glyph"
    );

    let (width, height) = bar.picker_thumbnail_size(Scale::ONE);
    let scaled = Surface::filled(width, height, Color::rgba(0, 200, 40, 255).premultiply())
        .expect("allocates");
    assert!(bar.set_picker_thumbnail(1, scaled));

    assert_eq!(
        bar.picker().entries()[1]
            .thumbnail()
            .map(|art| (art.width(), art.height())),
        Some((width, height))
    );
    assert_eq!(bar.take_repaint(), TaskbarRepaint::PICKER);
    assert!(
        !bar.set_picker_thumbnail(9, Surface::new(1, 1).expect("allocates")),
        "a cell that does not exist takes nothing"
    );
}

#[test]
fn hovering_a_cell_highlights_only_the_picker() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let layout = bar.picker_layout(Scale::ONE).expect("open");
    let mut input = TaskbarInput::new();
    let _ = bar.take_repaint();

    let cell = centre_of(layout.cells[1]);
    assert_eq!(
        moved_at(&mut input, &mut bar, cell, NOW_NS),
        TaskbarResponse::Ignored
    );
    assert_eq!(bar.picker().hover(), Some(1));
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::PICKER,
        "moving the highlight repaints the picker alone"
    );
    // A second sample on the same cell changes nothing further.
    assert_eq!(
        moved_at(&mut input, &mut bar, cell, NOW_NS),
        TaskbarResponse::Ignored
    );
    assert_eq!(bar.take_repaint(), TaskbarRepaint::NONE);
}

#[test]
fn pressing_a_cell_chooses_that_window_and_closes_the_picker() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.tasks_mut().minimise(TaskId(2));
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let layout = bar.picker_layout(Scale::ONE).expect("open");
    let mut input = TaskbarInput::new();

    let cell = centre_of(layout.cells[1]);
    assert_eq!(
        press_at(&mut input, &mut bar, cell.x, cell.y),
        TaskbarResponse::WindowChosen { id: TaskId(2) }
    );
    assert!(!bar.picker().is_open(), "a choice closes the picker");
    assert_eq!(bar.tasks().focused(), Some(TaskId(2)));
    assert!(
        !bar.tasks().is_minimised(TaskId(2)),
        "choosing a minimised window restores it"
    );
}

#[test]
fn pressing_the_pickers_own_chrome_is_claimed_and_does_nothing() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let layout = bar.picker_layout(Scale::ONE).expect("open");
    let mut input = TaskbarInput::new();

    // The plate's own rim: inside the panel, on no cell. The press is
    // claimed so it never falls through to the slot beneath.
    let rim = Point::new(layout.panel.left(), layout.panel.top());
    assert_eq!(bar.picker().cell_at(&layout, rim), None);
    assert_eq!(
        press_at(&mut input, &mut bar, rim.x, rim.y),
        TaskbarResponse::Ignored
    );
    assert!(!bar.picker().is_open(), "and the picker closes with it");
    assert_eq!(bar.tasks().focused(), None);
}

#[test]
fn the_picker_takes_no_keyboard() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let mut input = TaskbarInput::new();
    // A hover surface holds no keyboard: the focused window keeps its keys,
    // and the picker goes when the pointer does.
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape)),
        TaskbarResponse::Ignored
    );
    assert!(bar.picker().is_open());
}

/// Leaving the slot gives the picker its grace, and the pointer reaching the
/// panel inside that grace keeps it: that crossing is how a window is chosen,
/// so it cannot be what dismisses the panel.
#[test]
fn leaving_the_slot_closes_the_picker_after_the_grace() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let mut input = TaskbarInput::new();

    // Off the slot, in the gap between the bar and the panel.
    let _ = moved_at(&mut input, &mut bar, Point::new(500, 400), NOW_NS);
    assert!(
        bar.picker().is_open(),
        "it went on the first sample outside"
    );
    assert_eq!(
        input.park_deadline_ns(NOW_NS, u64::MAX),
        PICKER_CLOSE_GRACE_NS
    );

    // Reaching a cell inside the grace cancels the close outright.
    let cell = bar
        .picker_layout(Scale::ONE)
        .expect("a layout")
        .cells
        .first()
        .copied()
        .expect("a cell");
    let _ = moved_at(&mut input, &mut bar, centre_of(cell), NOW_NS + 1);
    assert_eq!(
        input.park_deadline_ns(NOW_NS + 1, u64::MAX),
        u64::MAX,
        "the pointer is on the panel: nothing is pending"
    );
    assert_eq!(bar.picker().hover(), Some(0), "and the cell lit");

    // Leaving for good does close it, once the grace runs out.
    let left_ns = NOW_NS + 2;
    let _ = moved_at(&mut input, &mut bar, Point::new(500, 400), left_ns);
    let _ = input.tick(&mut bar, left_ns + PICKER_CLOSE_GRACE_NS);
    assert!(!bar.picker().is_open());
}

#[test]
fn losing_the_second_window_closes_the_picker_with_it() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    assert!(bar.picker().is_open());
    let _ = bar.take_repaint();

    // The application closed one of its two windows, so the session hands
    // the bar a fresh strip: the picker has nothing left to choose between
    // and must not survive showing windows that are gone.
    bar.tasks_mut().remove(TaskId(2));
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1)])
    ]);
    assert!(!bar.picker().is_open());
    assert!(bar.picker_layout(Scale::ONE).is_none());
    assert_eq!(
        bar.take_repaint(),
        TaskbarRepaint::BAR | TaskbarRepaint::PICKER
    );
}

#[test]
fn the_picker_survives_a_strip_update_that_keeps_the_choice() {
    let mut bar = bottom_bar();
    for id in 1..=3 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![app("Terminal").with_windows(alloc::vec![
        TaskId(1),
        TaskId(2),
        TaskId(3)
    ])]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    bar.tasks_mut().remove(TaskId(3));
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    assert!(
        bar.picker().is_open(),
        "two windows is still a choice, so the picker stays"
    );
}

#[test]
fn a_modal_surface_keeps_the_picker_from_opening_underneath_it() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![app("Terminal")
        .with_windows(alloc::vec![TaskId(1), TaskId(2)])
        .with_declaration(declared_menu(), true)]);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);

    // With the declared menu up, a sample over the slot asks for no picker:
    // the user could not reach one under a modal surface.
    open_app_menu(&mut input, &mut bar);
    assert_eq!(
        moved_at(&mut input, &mut bar, slot, NOW_NS),
        TaskbarResponse::Ignored
    );
    assert!(!bar.picker().is_open());

    // Same with the library popup.
    bar.close_menu();
    open_library(&mut input, &mut bar);
    assert!(matches!(
        moved_at(&mut input, &mut bar, slot, NOW_NS),
        TaskbarResponse::Ignored
    ));
    assert!(!bar.picker().is_open());
}

#[test]
fn clicking_the_slot_closes_the_picker_and_acts_on_the_application() {
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![app("Terminal")
        .with_windows(alloc::vec![TaskId(1), TaskId(2)])
        .with_declaration(declared_menu(), true)]);
    bar.show_window_picker(0, cells(&bar, 0), Scale::ONE);
    let mut input = TaskbarInput::new();
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::AppDefault { app: 0 },
        "the user decided on the application rather than one of its windows"
    );
    assert!(!bar.picker().is_open());
}

#[test]
fn render_picker_paints_a_thumbnail_or_the_applications_glyph() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    for id in 1..=2 {
        bar.tasks_mut().add(TaskId(id), format!("Window {id}"));
    }
    bar.set_apps(alloc::vec![
        app("Terminal").with_windows(alloc::vec![TaskId(1), TaskId(2)])
    ]);
    let renderer = TaskbarRenderer::new(test_icon_cache());
    assert!(
        renderer.render_picker(&bar, Scale::ONE).is_none(),
        "a closed picker draws nothing"
    );

    let magenta = Color::rgb(255, 0, 255).premultiply();
    bar.show_window_picker(
        0,
        alloc::vec![
            PickerEntry::new(TaskId(1), "Shell")
                .with_thumbnail(Surface::filled(64, 40, magenta).expect("a frame")),
            PickerEntry::new(TaskId(2), "Logs"),
        ],
        Scale::ONE,
    );
    let layout = bar.picker_layout(Scale::ONE).expect("open");
    let surface = renderer
        .render_picker(&bar, Scale::ONE)
        .expect("picker renders");
    assert_eq!(surface.width(), layout.panel.width);
    assert_eq!(surface.height(), layout.panel.height);

    // Cell 0 shows the window's own frame…
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        layout.cells[0],
        magenta
    ));
    // …and cell 1, which has no frame yet, draws the application's glyph
    // rather than a hole.
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.cells[1],
        theme.palette().on_surface,
        floating_ground(&theme, theme.palette().surface_raised),
    ));
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
    bar.set_apps(alloc::vec![app("Editor")
        .with_windows(alloc::vec![TaskId(1)])
        .with_declaration(declared_menu(), true)]);
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);

    // Press on an application slot while the popup is open: one click does
    // one thing — the popup closes and the application is not acted on.
    let slot = centre_of(bar.layout(Scale::ONE).apps[0]);
    assert_eq!(
        press_at(&mut input, &mut bar, slot.x, slot.y),
        TaskbarResponse::LibraryDismissed
    );
    assert!(!bar.library().is_open());
    assert_eq!(
        bar.tasks().focused(),
        None,
        "the application was not acted on"
    );
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
    // The press arms the drag rather than launching, so the click concludes
    // on the release that ends it without travelling.
    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::Ignored
    );
    assert!(bar.library().is_open(), "the press alone launches nothing");
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::LibraryLaunch { entry: id }
    );
    assert!(!bar.library().is_open(), "a launch closes the popup");
}

/// Open the popup and press the visible "Chess" entry row, returning its
/// catalog identifier and the press point.
///
/// An entry row launches on the *release* that ends the press, so a press
/// alone is where every click gesture below starts.
fn press_chess_row(input: &mut TaskbarInput, bar: &mut Taskbar) -> (EntryId, Point) {
    open_library(input, bar);
    let (index, rect) = visible_row_where(
        bar,
        |row| matches!(row, LibraryRow::Entry { name, .. } if name == "Chess"),
    )
    .expect("Chess is visible");
    let LibraryRow::Entry { id, .. } = bar.library().rows()[index].clone() else {
        panic!("expected an entry row");
    };
    let centre = centre_of(rect);
    assert_eq!(
        press_at(input, bar, centre.x, centre.y),
        TaskbarResponse::Ignored
    );
    (id, centre)
}

#[test]
fn a_press_that_barely_moves_is_still_a_click() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let (id, press) = press_chess_row(&mut input, &mut bar);

    // Jitter within the pressed row is still that row's click.
    assert_eq!(
        moved_at(
            &mut input,
            &mut bar,
            Point::new(press.x + 1, press.y),
            NOW_NS
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::LibraryLaunch { entry: id }
    );
}

#[test]
fn a_release_away_from_the_pressed_row_launches_nothing() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let (_, press) = press_chess_row(&mut input, &mut bar);

    // The pointer left the row it pressed, so the release completes no
    // click — and the row it ended over is not launched either.
    let elsewhere = Point::new(press.x, press.y + 200);
    let _ = moved_at(&mut input, &mut bar, elsewhere, NOW_NS);
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::Ignored
    );
    assert!(bar.library().is_open());
}

#[test]
fn a_rebuild_under_a_held_press_launches_nothing() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    let (id, press) = press_chess_row(&mut input, &mut bar);

    // Typing filters the list while the button is still down, so the row the
    // press was keyed to is gone. The armed press must go with it: releasing
    // now would otherwise launch whatever moved into that position, which is
    // not the program the user pressed.
    press_key(&mut input, &mut bar, Key::Char('w'));
    assert_eq!(bar.library().search_text(), "w");
    assert!(
        !bar.library()
            .rows()
            .iter()
            .any(|row| matches!(row, LibraryRow::Entry { id: row_id, .. } if *row_id == id)),
        "the filter dropped the pressed entry"
    );

    let _ = moved_at(&mut input, &mut bar, press, NOW_NS);
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::Ignored,
        "a rebuilt list has nothing armed to launch"
    );
}

#[test]
fn a_folder_header_acts_on_the_press_and_arms_nothing() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let (_, rect) = visible_row_where(&bar, |row| {
        matches!(row, LibraryRow::Folder { category, .. } if *category == LibraryCategory::Office)
    })
    .expect("the Office folder is visible");

    // A header folds on the press, so there is nothing armed for the
    // release to complete.
    let centre = centre_of(rect);
    assert_eq!(
        press_at(&mut input, &mut bar, centre.x, centre.y),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        release_at(&mut input, &mut bar, NOW_NS),
        TaskbarResponse::Ignored
    );
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

/// The ground a floating surface lays down for the palette role it wears
/// solid — the fill the compositor's backdrop blur reads through.
///
/// Taken through the rule the bar paints with rather than restated, so a test
/// cannot drift from the theme's authored weight. Pass `surface_raised` for
/// the bar and the surfaces raised like it (the context menu, the tray
/// readout), `surface` for a `Panel` (the library popup, the notification
/// popover), and `rim` for the edge any of them wears, exactly as each
/// control does.
fn floating_ground(theme: &Theme, fill: Rgba) -> Pixel {
    role(ground_fill(
        &theme.clone().floating(),
        fill,
        ChromeLayer::Ground,
    ))
}

/// The plate a control raised on floating chrome lays down — a search field,
/// a button: a step more solid than the ground under it.
fn floating_plate(theme: &Theme, fill: Rgba) -> Pixel {
    role(ground_fill(
        &theme.clone().floating(),
        fill,
        ChromeLayer::Plate,
    ))
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
        *base.fonts(),
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert_eq!(surface.width(), layout.bar.width);
    assert_eq!(surface.height(), layout.bar.height);
}

#[test]
fn background_is_the_floating_chrome_fill() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let bar = bottom_bar();
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let frame = bar.layout(Scale::ONE).bar;

    // The bar is see-through: its ground is the raised surface let through at
    // the theme's chrome alpha, so the desktop behind it reads through the
    // backdrop the compositor blurs.
    let ground = floating_ground(&theme, palette.surface_raised);
    assert_eq!(pixel_at(&surface, frame, 500, 780), ground);
    assert!(
        ground.a < 255,
        "a covering ground is what makes the bar not see-through"
    );

    // A bare stretch of bar, well clear of every control: nowhere on it is
    // the opaque plate colour the bar used to be filled with.
    let bare = Rect::new(400, frame.top(), 200, frame.height);
    assert!(
        !region_has_pixel(&surface, frame, bare, role(palette.surface_raised)),
        "the bar is never filled opaque"
    );
    assert!(
        region_has_pixel(&surface, frame, bare, ground),
        "the bare bar shows its chrome ground"
    );
}

#[test]
fn the_bar_edge_is_the_rim_and_its_interior_the_ground() {
    for theme in [Theme::dark(), Theme::light()] {
        let mut bar = bottom_bar();
        bar.apply_theme(&theme);
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render(&bar, Scale::ONE, &mut NoArtwork)
            .expect("bar renders");
        let frame = bar.layout(Scale::ONE).bar;
        let border = coord(plate_border(&theme, Scale::ONE));
        let rim = floating_ground(&theme, theme.palette().rim);
        let ground = floating_ground(&theme, theme.palette().surface_raised);
        let mid_x = frame.left() + coord(frame.width / 2);
        let mid_y = frame.top() + coord(frame.height / 2);
        let name = theme.name();

        for (edge_label, (ex, ey), (ix, iy)) in [
            ("top", (mid_x, frame.top()), (mid_x, frame.top() + border)),
            (
                "bottom",
                (mid_x, frame.bottom() - 1),
                (mid_x, frame.bottom() - 1 - border),
            ),
            (
                "leading",
                (frame.left(), mid_y),
                (frame.left() + border, mid_y),
            ),
            (
                "trailing",
                (frame.right() - 1, mid_y),
                (frame.right() - 1 - border, mid_y),
            ),
        ] {
            assert_eq!(
                pixel_at(&surface, frame, ex, ey),
                rim,
                "{name}: the {edge_label} edge is the theme's rim"
            );
            assert_eq!(
                pixel_at(&surface, frame, ix, iy),
                ground,
                "{name}: one border past the {edge_label} edge is the bar's ground"
            );
        }
    }
}

#[test]
fn the_bar_rim_lightens_a_dark_theme_and_darkens_a_light_one() {
    // The claim is made on the painted pixels rather than the palette: the
    // edge steps up from the ground on a dark theme and down on a light one,
    // which is the one "lightened" edge read correctly either way.
    for (theme, lighter) in [(Theme::dark(), true), (Theme::light(), false)] {
        let mut bar = bottom_bar();
        bar.apply_theme(&theme);
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render(&bar, Scale::ONE, &mut NoArtwork)
            .expect("bar renders");
        let frame = bar.layout(Scale::ONE).bar;
        let border = coord(plate_border(&theme, Scale::ONE));
        let mid_x = frame.left() + coord(frame.width / 2);
        let edge = pixel_at(&surface, frame, mid_x, frame.top());
        let inside = pixel_at(&surface, frame, mid_x, frame.top() + border);
        let name = theme.name();

        assert_eq!(
            edge.r > inside.r && edge.g > inside.g && edge.b > inside.b,
            lighter,
            "{name}: the rim is lighter than the ground exactly on a dark theme"
        );
        assert_eq!(
            edge.r < inside.r && edge.g < inside.g && edge.b < inside.b,
            !lighter,
            "{name}: the rim is darker than the ground exactly on a light theme"
        );
        assert_eq!(
            edge.a, inside.a,
            "{name}: the rim is the surface's own edge, so it takes the ground's weight"
        );
    }
}

#[test]
fn the_bar_rim_stays_see_through() {
    for theme in [Theme::dark(), Theme::light()] {
        let mut bar = bottom_bar();
        bar.apply_theme(&theme);
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render(&bar, Scale::ONE, &mut NoArtwork)
            .expect("bar renders");
        let frame = bar.layout(Scale::ONE).bar;
        let mid_x = frame.left() + coord(frame.width / 2);
        let edge = pixel_at(&surface, frame, mid_x, frame.top());
        let name = theme.name();

        assert!(
            edge.a < 255,
            "{name}: a solid rim is a hard line the wallpaper cannot reach through"
        );
        assert_eq!(
            edge.a,
            theme.palette().chrome_alpha,
            "{name}: the rim is let through at the theme's chrome weight"
        );
        assert_ne!(
            edge,
            role(theme.palette().rim),
            "{name}: the rim is laid down at that weight, never solid"
        );
    }
}

#[test]
fn the_bar_rim_is_one_border_thick_and_scales() {
    let theme = Theme::dark();
    let bar = bottom_bar();
    let rim = floating_ground(&theme, theme.palette().rim);
    let ground = floating_ground(&theme, theme.palette().surface_raised);
    let mut thicknesses = Vec::new();

    for percent in [100, 200] {
        let scale = Scale::from_percent(percent).expect("a valid scale");
        let border = plate_border(&theme, scale);
        thicknesses.push(border);
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render(&bar, scale, &mut NoArtwork)
            .expect("bar renders");
        let frame = bar.layout(scale).bar;
        let mid_x = frame.left() + coord(frame.width / 2);

        for step in 0..border {
            assert_eq!(
                pixel_at(&surface, frame, mid_x, frame.top() + coord(step)),
                rim,
                "{percent}%: row {step} of the rim"
            );
        }
        assert_eq!(
            pixel_at(&surface, frame, mid_x, frame.top() + coord(border)),
            ground,
            "{percent}%: the rim stops after one border thickness"
        );
    }

    assert!(
        thicknesses[1] > thicknesses[0],
        "the rim is a scaled length, not a fixed pixel count"
    );
}

#[test]
fn the_bar_rim_follows_the_rounded_corner_rather_than_squaring_off() {
    let theme = Theme::dark();
    let bar = bottom_bar();
    let layout = bar.layout(Scale::ONE);
    let frame = layout.bar;
    let radius = coord(layout.corner_radius);
    assert!(radius > 0, "a square bar would prove nothing about the rim");
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let rim = floating_ground(&theme, theme.palette().rim);

    for (label, x, y) in [
        ("leading top", frame.left(), frame.top()),
        ("trailing top", frame.right() - 1, frame.top()),
        ("leading bottom", frame.left(), frame.bottom() - 1),
        ("trailing bottom", frame.right() - 1, frame.bottom() - 1),
    ] {
        assert_eq!(
            pixel_at(&surface, frame, x, y),
            Pixel::TRANSPARENT,
            "{label}: the corner is cut away, so the rim curves with it"
        );
    }
    assert_eq!(
        pixel_at(&surface, frame, frame.left() + radius, frame.top()),
        rim,
        "past the corner arc the rim resumes along the edge"
    );
}

#[test]
fn a_hovered_or_pressed_slot_never_washes_over_the_bar_rim() {
    // A slot's wash is content, and content is laid out inside the bar's rim,
    // so the pointer can never rub the bar's edge off. Every pixel of both
    // long edges across the slot's span is swept rather than one sample; the
    // rounded ends are left out because the cut is what the rim does there,
    // and its own test owns that.
    for theme in [Theme::dark(), Theme::light()] {
        let palette = theme.palette();
        let rim = floating_ground(&theme, palette.rim);
        let name = theme.name();
        // An application slot wears the pointer's wash and nothing else —
        // the strip has no held state of its own — so a press on one still
        // reads as a hover.
        for (slot_label, on_strip, held) in [
            ("library", false, palette.surface_pressed),
            ("application", true, palette.surface_hover),
        ] {
            for (state, wash) in [
                ("hovered", floating_plate(&theme, palette.surface_hover)),
                ("pressed", floating_plate(&theme, held)),
            ] {
                let mut bar = bottom_bar();
                bar.apply_theme(&theme);
                bar.set_apps(alloc::vec![app("App")]);
                let layout = bar.layout(Scale::ONE);
                let frame = layout.bar;
                let slot = if on_strip {
                    layout.apps[0]
                } else {
                    layout.library
                };
                let centre = centre_of(slot);
                if state == "pressed" {
                    let _ = press_at(&mut TaskbarInput::new(), &mut bar, centre.x, centre.y);
                } else {
                    bar.track_hover(Some(centre), Scale::ONE, &mut damage::sink());
                }
                let surface = TaskbarRenderer::new(test_icon_cache())
                    .render(&bar, Scale::ONE, &mut NoArtwork)
                    .expect("bar renders");

                assert!(
                    region_has_pixel(&surface, frame, slot, wash),
                    "{name}: the {state} {slot_label} inks its wash inside its slot"
                );

                let radius = coord(layout.corner_radius);
                let (x_lo, x_hi) = (
                    slot.left().max(frame.left() + radius),
                    slot.right().min(frame.right() - radius),
                );
                let (y_lo, y_hi) = (
                    slot.top().max(frame.top() + radius),
                    slot.bottom().min(frame.bottom() - radius),
                );
                assert!(
                    x_lo < x_hi && y_lo < y_hi,
                    "{name}: the {slot_label} slot clears the rounded ends"
                );
                let along = u32::try_from(x_hi - x_lo).expect("a positive span");
                let across = u32::try_from(y_hi - y_lo).expect("a positive span");

                let mut strips = alloc::vec![
                    ("top", Rect::new(x_lo, frame.top(), along, 1)),
                    ("bottom", Rect::new(x_lo, frame.bottom() - 1, along, 1)),
                ];
                if slot.left() == frame.left() + coord(plate_border(&theme, Scale::ONE)) {
                    strips.push(("leading", Rect::new(frame.left(), y_lo, 1, across)));
                }

                for (edge_label, strip) in strips {
                    for y in strip.top()..strip.bottom() {
                        for x in strip.left()..strip.right() {
                            assert_eq!(
                                pixel_at(&surface, frame, x, y),
                                rim,
                                "{name}: the {state} {slot_label} covers the bar's \
                                 {edge_label} rim at ({x}, {y})"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn every_icon_on_the_strip_rests_bare_on_the_bar() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
    let theme = Theme::dark();
    let palette = theme.palette();
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let layout = bar.layout(Scale::ONE);
    let frame = layout.bar;

    // No icon on the strip is "the" primary action of it: the launcher and
    // every application slot are quiet peers seated in the bar. Each inks its
    // glyph straight onto the bar's own fill, with no role colour and no
    // perimeter of its own — so the strip reads as one bar rather than a row
    // of boxes.
    for (label, slot) in [("library", layout.library), ("app slot", layout.apps[0])] {
        assert!(
            region_has_role_ink(
                &surface,
                frame,
                slot,
                palette.on_surface,
                floating_ground(&theme, palette.surface_raised),
            ),
            "the {label} glyph inks the ordinary foreground on the bar"
        );
        assert!(
            !region_has_pixel(&surface, frame, slot, role(palette.accent)),
            "the {label} slot carries no role fill"
        );
        assert!(
            !region_has_pixel(&surface, frame, slot, role(palette.rim)),
            "the {label} slot carries no rim"
        );
        assert!(
            !region_has_pixel(&surface, frame, slot, role(palette.rim_active)),
            "the {label} slot carries no reactive edge at rest"
        );
    }
}

#[test]
fn the_separator_rule_is_painted_in_the_border_colour() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
    let theme = Theme::dark();
    let palette = theme.palette();
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let layout = bar.layout(Scale::ONE);
    let frame = layout.bar;
    let rule = layout.separator;

    assert!(
        region_has_pixel(&surface, frame, rule, role(palette.border)),
        "the rule inks the separator colour where it is laid out"
    );
    let beside = Rect::new(rule.right(), rule.top(), 1, rule.height);
    assert!(
        !region_has_pixel(&surface, frame, beside, role(palette.border)),
        "one pixel of rule, then the bar's own fill"
    );
    let over = Rect::new(
        rule.left(),
        frame.top(),
        1,
        u32::try_from(rule.top() - frame.top()).expect("the rule is inset from the long edge"),
    );
    assert!(
        !region_has_pixel(&surface, frame, over, role(palette.border)),
        "the rule stops short of the bar's edge"
    );
    for (label, slot) in [("library", layout.library), ("app slot", layout.apps[0])] {
        assert!(
            !region_has_pixel(&surface, frame, slot, role(palette.border)),
            "the rule sits between the launcher and the strip, marking neither: {label}"
        );
    }
}

#[test]
fn hovering_the_launcher_washes_only_that_slot_and_draws_no_edge() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
    let theme = Theme::dark();
    let palette = theme.palette();
    let layout = bar.layout(Scale::ONE);
    bar.track_hover(
        Some(centre_of(layout.library)),
        Scale::ONE,
        &mut damage::sink(),
    );
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    // The pointer's only mark is a lighter grey wash under the icon it is on.
    // It is a plate raised on the bar, so it reads through to the blurred
    // desktop like the bar does rather than punching a solid block in it.
    let wash = floating_plate(&theme, palette.surface_hover);
    assert!(region_has_pixel(&surface, layout.bar, layout.library, wash));
    assert!(wash.a < 255, "a solid wash is a hole in the glass");
    assert_ne!(
        wash,
        floating_ground(&theme, palette.surface_raised),
        "a wash the bar's own weight and colour states nothing"
    );
    assert!(
        !region_has_pixel(
            &surface,
            layout.bar,
            layout.library,
            role(palette.rim_active)
        ),
        "a hovered bar icon never grows an edge"
    );
    // Its neighbour is untouched: the wash belongs to one slot, not the strip.
    assert!(!region_has_pixel(
        &surface,
        layout.bar,
        layout.apps[0],
        wash
    ));
}

#[test]
fn the_library_button_reads_as_held_down_while_its_popup_is_open() {
    let mut bar = bottom_bar();
    bar.set_apps(alloc::vec![app("Editor")]);
    let theme = Theme::dark();
    let palette = theme.palette();
    let layout = bar.layout(Scale::ONE);
    bar.open_library();
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    // Held open compresses the plate rather than outlining it, so the state is
    // legible on a bar where nothing wears an edge.
    let held = floating_plate(&theme, palette.surface_pressed);
    assert!(region_has_pixel(&surface, layout.bar, layout.library, held));
    assert!(!region_has_pixel(
        &surface,
        layout.bar,
        layout.apps[0],
        held
    ));
}

#[test]
fn an_app_slot_carries_no_presence_or_focus_mark() {
    // An icon-bar slot is an application, not a window: there is no running
    // bar, no focus seam, and no recessed minimised plate under it. Only the
    // pointer's own wash distinguishes one slot from another.
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().set_focused(Some(TaskId(1)));
    bar.set_apps(alloc::vec![
        app("Editor").with_windows(alloc::vec![TaskId(1)]),
        app("Idle"),
    ]);

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    for (label, slot) in [
        ("the focused application's", layout.apps[0]),
        ("an idle application's", layout.apps[1]),
    ] {
        assert!(
            !region_has_pixel(&surface, layout.bar, slot, role(theme.palette().accent)),
            "{label} slot draws no accent mark"
        );
        assert!(
            !region_has_pixel(&surface, layout.bar, slot, role(theme.palette().surface)),
            "{label} slot draws no recessed plate"
        );
    }
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert!(region_has_role_ink(
        &surface,
        layout.bar,
        layout.notifications[0],
        theme.palette().on_surface_muted,
        floating_ground(&theme, theme.palette().surface_raised),
    ));
}

/// A bar showing one status signal of each kind the built-in set draws,
/// so a render rasterises several distinct glyphs into the cache.
fn bar_with_status_signals() -> Taskbar {
    let mut bar = bottom_bar();
    bar.set_status_signals(alloc::vec![
        StatusSignal::new(IconId(1), StatusKind::Network),
        StatusSignal::new(IconId(2), StatusKind::Volume),
        StatusSignal::new(IconId(3), StatusKind::Battery),
    ]);
    bar
}

#[test]
fn a_glyph_is_rasterised_once_per_epoch_and_retained() {
    let bar = bar_with_status_signals();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());

    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let after_first = renderer.cache_len();
    assert!(after_first > 0, "glyphs must be retained across frames");
    let rasterisations = renderer.cache_stats().misses();

    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert_eq!(renderer.cache_len(), after_first);
    assert_eq!(
        renderer.cache_stats().misses(),
        rasterisations,
        "a repaint at the same epoch must not re-rasterise"
    );
}

#[test]
fn a_scale_change_invalidates_every_cached_glyph() {
    let bar = bar_with_status_signals();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let rasterisations = renderer.cache_stats().misses();
    assert!(rasterisations > 0);

    let doubled = Scale::from_percent(200).expect("scale");
    let _ = renderer
        .render(&bar, doubled, &mut NoArtwork)
        .expect("bar renders");
    assert_eq!(
        renderer.cache_stats().invalidations(),
        1,
        "a glyph rasterised for one pixel size is wrong at another"
    );
    assert!(renderer.cache_stats().misses() > rasterisations);
}

#[test]
fn installing_a_different_icon_set_invalidates_every_cached_glyph() {
    let bar = bar_with_status_signals();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert!(renderer.cache_len() > 0);

    renderer.set_icons(IconSet::builtin());
    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert_eq!(renderer.cache_stats().invalidations(), 1);
}

#[test]
fn the_glyph_cache_never_exceeds_its_derived_budget() {
    let bar = bar_with_status_signals();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let hard = CacheBudget::from_backing(TEST_FB_BYTES).hard();

    // Every scale is a fresh epoch, so this rasterises a great many
    // distinct glyphs through the one cache.
    for percent in 50..250u32 {
        let scale = Scale::from_percent(percent).expect("scale");
        let _ = renderer
            .render(&bar, scale, &mut NoArtwork)
            .expect("bar renders");
        assert!(
            renderer.cache_bytes() <= hard,
            "the cache must stay inside its budget at every step"
        );
    }
}

#[test]
fn no_band_drops_the_glyph_cache_below_its_reserve() {
    // One bar's worth of rasterised glyphs is far inside the shared UI
    // reserve, so no band takes it: the bar is redrawn on every clock tick
    // and every hover, and re-rasterising each glyph per frame is what
    // pressure must not add.
    static GAUGE: ReportedPressure = ReportedPressure::unknown();
    let bar = bar_with_status_signals();
    let mut renderer = pressured_renderer(&GAUGE);
    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let held = renderer.cache_bytes();
    let entries = renderer.cache_len();
    assert!(entries > 0 && held > 0);

    for band in [
        PressureBand::Mild,
        PressureBand::Moderate,
        PressureBand::Severe,
        PressureBand::Critical,
    ] {
        GAUGE.report(band);
        assert_eq!(renderer.trim(), 0, "{band:?} took the reserve");
        assert_eq!(renderer.cache_len(), entries, "{band:?}");
        assert_eq!(renderer.cache_bytes(), held, "{band:?}");
    }

    // And the bar still paints correctly at the deepest band.
    let surface = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert_eq!(surface.width(), bar.layout(Scale::ONE).bar.width);
}

#[test]
fn teardown_releases_every_rasterised_glyph() {
    let bar = bar_with_status_signals();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let _ = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert!(renderer.cache_len() > 0);

    renderer.teardown();
    assert_eq!(renderer.cache_len(), 0);
    assert_eq!(renderer.cache_bytes(), 0);
    assert_eq!(renderer.cache_stats().teardowns(), 1);
}

#[test]
fn theme_switch_repaints_the_bar() {
    let mut bar = bottom_bar();
    let mut renderer = TaskbarRenderer::new(test_icon_cache());
    let dark = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    bar.apply_theme(&Theme::light());
    let light = renderer
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    let frame = bar.layout(Scale::ONE).bar;
    assert_ne!(
        pixel_at(&dark, frame, 500, 780),
        pixel_at(&light, frame, 500, 780),
        "the bar background follows the palette"
    );
}

#[test]
fn app_strip_and_menu_actions_latch_repaints() {
    let mut bar = bottom_bar();
    let _ = bar.take_repaint();

    // set_apps draws on the bar's own strip: bar only.
    bar.set_apps(alloc::vec![
        app("App").with_declaration(declared_menu(), true)
    ]);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::BAR);

    // Opening the menu is its own overlay: menu only.
    bar.open_app_menu(0, Rect::EMPTY);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::MENU);

    // Motion over a slot changes the bar's own hover feedback: bar only.
    let layout = bar.layout(Scale::ONE);
    let slot = centre_of(layout.apps[0]);
    bar.track_hover(Some(slot), Scale::ONE, &mut damage::sink());
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
    let empty = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert!(
        !region_has_role_ink(
            &empty,
            layout.bar,
            layout.clock,
            theme.palette().on_surface,
            floating_ground(&theme, theme.palette().surface_raised),
        ),
        "an empty label draws nothing"
    );

    // An unset wall clock is not an empty label: the clock is pressable and
    // its menu is where a time is set, so the placeholder must be visible.
    bar.clock_mut().set_label(crate::clock::UNSET_LABEL);
    let unset = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert!(
        region_has_role_ink(
            &unset,
            layout.bar,
            layout.clock,
            theme.palette().on_surface,
            floating_ground(&theme, theme.palette().surface_raised),
        ),
        "the unset placeholder is drawn"
    );

    bar.clock_mut().set_label("12:34");
    let drawn = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");
    assert!(region_has_role_ink(
        &drawn,
        layout.bar,
        layout.clock,
        theme.palette().on_surface,
        floating_ground(&theme, theme.palette().surface_raised),
    ));
}

#[test]
fn an_app_slot_draws_its_icon_and_no_label_beside_it() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let label = "An enormously long application name that cannot fit";
    bar.set_apps(alloc::vec![app(label)]);
    let layout = bar.layout(Scale::ONE);
    let slot = layout.apps[0];
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    // The drawn icon, centred in the slot, with a pixel of slack either side
    // for the renderer's odd-remainder rounding.
    let side = bar.app_icon_side(Scale::ONE);
    let inset = i32::try_from(slot.width.saturating_sub(side) / 2).expect("a slot-sized inset");
    let band = Rect::new(slot.left() + inset - 1, slot.top(), side + 2, slot.height);
    let bar_fill = floating_ground(&theme, theme.palette().surface_raised);
    assert!(
        region_has_role_ink(
            &surface,
            layout.bar,
            band,
            theme.palette().on_surface,
            bar_fill,
        ),
        "the application icon is drawn"
    );

    for beside in [
        Rect::new(
            slot.left(),
            slot.top(),
            (band.left() - slot.left()).unsigned_abs(),
            slot.height,
        ),
        Rect::new(
            band.right(),
            slot.top(),
            (slot.right() - band.right()).unsigned_abs(),
            slot.height,
        ),
    ] {
        assert!(beside.width > 0, "there is bar to the side of the icon");
        assert!(
            !region_has_role_ink(
                &surface,
                layout.bar,
                beside,
                theme.palette().on_surface,
                bar_fill,
            ),
            "and no title ink anywhere beside it"
        );
    }

    // The label is the model's own data — a context surface reads it — never
    // decoration on the slot.
    assert_eq!(bar.apps().get(0).expect("one slot").label(), label);
}

#[test]
fn app_slots_render_artwork_or_the_fallback_glyph() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let magenta = Color::rgb(255, 0, 255).premultiply();
    bar.set_apps(alloc::vec![
        app("Art").with_artwork(Surface::filled(16, 16, magenta).unwrap()),
        app("Glyph"),
    ]);
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    // Slot 0 shows the magenta artwork the session rasterised for it.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.apps[0],
        magenta
    ));

    // Slot 1 shows the AppBundle glyph (on_surface ink).
    assert!(region_has_role_ink(
        &surface,
        layout.bar,
        layout.apps[1],
        theme.palette().on_surface,
        floating_ground(&theme, theme.palette().surface_raised),
    ));
}

#[test]
fn each_app_slot_draws_its_own_application_artwork() {
    let mut bar = bottom_bar();
    let magenta = Color::rgb(255, 0, 255).premultiply();
    let cyan = Color::rgb(0, 255, 255).premultiply();
    bar.set_apps(alloc::vec![
        app("Magenta").with_artwork(Surface::filled(16, 16, magenta).unwrap()),
        app("Cyan").with_artwork(Surface::filled(16, 16, cyan).unwrap()),
    ]);

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    // Each slot shows its own application's picture and only its own: one
    // application's icon on every slot would be the defect this closes.
    for (slot, own, other) in [(0, magenta, cyan), (1, cyan, magenta)] {
        assert!(region_has_pixel(
            &surface,
            layout.bar,
            layout.apps[slot],
            own
        ));
        assert!(!region_has_pixel(
            &surface,
            layout.bar,
            layout.apps[slot],
            other
        ));
    }
}

#[test]
fn an_app_slot_with_no_resolved_artwork_keeps_the_shared_application_glyph() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    // A process this desktop cannot attribute to a bundle: the session
    // resolves no picture for it, and the slot must still read as an
    // application rather than as a blank plate.
    bar.set_apps(alloc::vec![app("Unattributed")]);

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    assert!(region_has_role_ink(
        &surface,
        layout.bar,
        layout.apps[0],
        theme.palette().on_surface,
        floating_ground(&theme, theme.palette().surface_raised),
    ));
}

/// A stand-in for the shipped `/System/Graphics` icon set: it answers the
/// kinds it was built with as a flat-coloured square and records every
/// question the renderer asks, so a test can assert both *which* kinds the
/// bar looks up and that what came back reached the pixels.
struct FakeArtwork {
    held: Vec<(IconKind, Surface)>,
    asked: Vec<(IconKind, u32)>,
}

impl FakeArtwork {
    /// A store holding one flat-coloured square per listed kind.
    fn new(held: &[(IconKind, Color)]) -> Self {
        Self {
            held: held
                .iter()
                .filter_map(|&(kind, colour)| {
                    Surface::filled(16, 16, colour.premultiply()).map(|art| (kind, art))
                })
                .collect(),
            asked: Vec::new(),
        }
    }

    /// Whether the renderer asked for `kind` at a slot of non-zero size.
    fn asked_for(&self, kind: IconKind) -> bool {
        self.asked
            .iter()
            .any(|&(asked, side)| asked == kind && side > 0)
    }
}

impl IconArtwork for FakeArtwork {
    fn artwork(&mut self, request: IconRequest<'_>, side: u32) -> Option<IconPicture<'_>> {
        let kind = request.icon_kind();
        self.asked.push((kind, side));
        self.held
            .iter()
            .find(|(held, _)| *held == kind)
            .map(|(_, art)| IconPicture::Artwork(art))
    }
}

#[test]
fn the_launcher_button_draws_its_shipped_artwork() {
    let bar = bottom_bar();
    let library_colour = Color::rgb(255, 0, 255);
    let mut artwork = FakeArtwork::new(&[(IconKind::Library, library_colour)]);

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut artwork)
        .expect("bar renders");

    assert!(
        artwork.asked_for(IconKind::Library),
        "the Library button resolves the library artwork"
    );
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.library,
        library_colour.premultiply()
    ));
}

/// The trailing capsule is an icon on the bar like any other, so it resolves
/// its kind's shipped artwork rather than always drawing the built-in glyph.
#[test]
fn the_switchboard_capsule_draws_its_shipped_artwork() {
    let bar = bottom_bar();
    let capsule_colour = Color::rgb(255, 0, 255);
    let mut artwork = FakeArtwork::new(&[(IconKind::Switchboard, capsule_colour)]);

    let layout = bar.layout(Scale::ONE);
    assert!(!layout.switchboard.is_empty(), "the capsule is on the bar");
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut artwork)
        .expect("bar renders");

    assert!(
        artwork.asked_for(IconKind::Switchboard),
        "the capsule resolves the switchboard artwork"
    );
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.switchboard,
        capsule_colour.premultiply()
    ));
}

#[test]
fn an_application_slot_falls_back_to_its_kinds_artwork_before_the_glyph() {
    let mut bar = bottom_bar();
    let own = Color::rgb(255, 0, 255);
    let bundle = Color::rgb(0, 255, 0);
    bar.set_apps(alloc::vec![
        app("Own").with_artwork(Surface::filled(16, 16, own.premultiply()).expect("artwork")),
        app("Shipped"),
    ]);
    let mut artwork = FakeArtwork::new(&[(IconKind::AppBundle, bundle)]);

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut artwork)
        .expect("bar renders");

    // A slot carrying its application's own icon keeps it: the shipped class
    // artwork is the fallback, never an override.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.apps[0],
        own.premultiply()
    ));
    // The slot with no icon of its own shows the shipped app-bundle artwork.
    assert!(region_has_pixel(
        &surface,
        layout.bar,
        layout.apps[1],
        bundle.premultiply()
    ));
}

#[test]
fn a_bar_with_no_artwork_at_all_still_draws_every_element() {
    let theme = Theme::dark();
    let mut bar = bar_with_status_signals();
    bar.set_apps(alloc::vec![app("App")]);
    bar.clock_mut().set_label("12:34");

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
        .expect("bar renders");

    // Every slot the bar draws carries ink: with no shipped assets at all
    // the built-in glyphs keep a freshly installed system usable.
    for (label, rect) in [
        ("library", layout.library),
        ("application", layout.apps[0]),
        ("status signal", layout.notifications[0]),
        ("clock", layout.clock),
    ] {
        assert!(
            region_has_role_ink(
                &surface,
                layout.bar,
                rect,
                theme.palette().on_surface,
                floating_ground(&theme, theme.palette().surface_raised),
            ) || region_has_role_ink(
                &surface,
                layout.bar,
                rect,
                theme.palette().on_surface_muted,
                floating_ground(&theme, theme.palette().surface_raised),
            ),
            "the {label} slot must never be blank without artwork"
        );
    }
}

#[test]
fn render_menu_paints_the_modal_plate_and_follows_theme() {
    let mut bar = bar_with_declared_app();
    let renderer = TaskbarRenderer::new(test_icon_cache());

    // None when closed.
    assert!(renderer.render_menu(&bar, Scale::ONE).is_none());

    // Open and check render.
    bar.open_app_menu(0, Rect::new(100, 760, 48, 40));
    let layout = bar.menu_layout(Scale::ONE).expect("menu layout");
    let dark = renderer
        .render_menu(&bar, Scale::ONE)
        .expect("menu renders");
    assert_eq!(dark.width(), layout.panel.width);
    assert_eq!(dark.height(), layout.panel.height);

    // The plate is floating chrome, not the opaque raised fill a menu wears
    // inside a window.
    assert!(region_has_pixel(
        &dark,
        layout.panel,
        Rect::new(
            layout.panel.left(),
            layout.panel.top(),
            layout.panel.width,
            layout.panel.height
        ),
        floating_ground(&Theme::dark(), Theme::dark().palette().surface_raised)
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
        floating_ground(&Theme::dark(), Theme::dark().palette().surface_raised),
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
    assert!(TaskbarRenderer::new(test_icon_cache())
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    assert_eq!(surface.width(), layout.panel.width);
    assert_eq!(surface.height(), layout.panel.height);

    // The panel's content region is the floating chrome ground…
    let viewport_gap = Point::new(layout.viewport.left(), layout.viewport.bottom() - 1);
    let ground = floating_ground(&theme, theme.palette().surface);
    assert_eq!(
        pixel_at(&surface, layout.panel, viewport_gap.x, viewport_gap.y),
        ground
    );
    // …the first folder row inks its label over that same ground, because a
    // resting row is the surface it sits in rather than a plate on it…
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.rows[0].1,
        theme.palette().on_surface,
        ground,
    ));
    // …and the search row is a plate raised on it: a step more solid, so it
    // reads as a field rather than dissolving into the panel, while the
    // backdrop still shows through.
    let field = floating_plate(&theme, theme.palette().surface_raised);
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        layout.search,
        field
    ));
    assert!(
        field.a > ground.a && field.a < 255,
        "the field is a step more solid than its panel, not opaque"
    );
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");

    // The hovered row takes the pointer wash — the shared hover fill, not the
    // raised fill a *selected* row lifts to, so the pointer never imitates
    // selection. A row is part of the panel rather than a plate on it, so the
    // wash carries the panel's own weight and the desktop still reads through.
    let wash = floating_ground(&theme, theme.palette().surface_hover);
    assert!(region_has_pixel(
        &surface,
        layout.panel,
        layout.rows[1].1,
        wash
    ));
    assert!(wash.a < 255, "a solid highlight is a hole in the glass");
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.viewport,
        theme.palette().on_surface_muted,
        floating_ground(&theme, theme.palette().surface),
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    // The channel is recessed into the popup rather than raised on it, so it
    // carries the panel's own weight: an overflowing library shows a bar, not
    // an opaque strip down its frosted edge.
    let channel = floating_ground(&theme, theme.palette().scroll_track);
    assert!(region_has_pixel(&surface, layout.panel, scrollbar, channel));
    assert!(
        !region_has_pixel(
            &surface,
            layout.panel,
            scrollbar,
            role(theme.palette().scroll_track)
        ),
        "the channel covers the desktop behind the popup"
    );
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
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render_library(&bar, Scale::ONE)
            .expect("popup renders");
        assert_eq!(surface.width(), layout.panel.width);
        assert!(
            region_has_role_ink(
                &surface,
                layout.panel,
                layout.rows[0].1,
                theme.palette().on_surface,
                floating_ground(&theme, theme.palette().surface),
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
    let renderer = TaskbarRenderer::new(test_icon_cache());
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

/// An open popup over `count` Utilities entries on a short screen, so the
/// list overflows its viewport and only some rows are shown.
fn overflowing_library(count: usize) -> Taskbar {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 300), &Theme::dark());
    let mut cat = Catalog::new();
    for index in 0..count {
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
    bar
}

/// The rows the popup's viewport currently shows that are launchable
/// entries — exactly the rows that may want owner-supplied artwork.
fn shown_entry_rows(bar: &Taskbar) -> Vec<usize> {
    bar.library_layout(Scale::ONE)
        .rows
        .iter()
        .filter(|&&(index, _)| {
            matches!(
                bar.library().rows().get(index),
                Some(LibraryRow::Entry { .. })
            )
        })
        .map(|&(index, _)| index)
        .collect()
}

/// The rows the popup asks the owner to resolve artwork for.
fn requested_rows(bar: &Taskbar) -> Vec<usize> {
    bar.library()
        .visible_icon_requests(&bar.library_layout(Scale::ONE), Scale::ONE, &Theme::dark())
        .iter()
        .map(|req| req.row)
        .collect()
}

#[test]
fn the_popup_asks_for_artwork_only_for_the_entry_rows_it_shows() {
    let bar = overflowing_library(30);
    let layout = bar.library_layout(Scale::ONE);
    assert!(
        layout.visible_rows < bar.library().rows().len(),
        "the fixture overflows the viewport"
    );

    let requests = bar
        .library()
        .visible_icon_requests(&layout, Scale::ONE, &Theme::dark());
    let shown = shown_entry_rows(&bar);
    assert!(!shown.is_empty(), "the fixture shows entry rows");
    assert_eq!(
        requests.iter().map(|req| req.row).collect::<Vec<_>>(),
        shown,
        "one request per shown entry row, and none for a row off screen"
    );
    assert!(
        requests.iter().all(|req| req.side > 0),
        "every request names the pixel side its row draws at"
    );
    // The request carries the entry the row launches, so the owner resolves
    // the right bundle's icon.
    for req in &requests {
        let Some(LibraryRow::Entry { id, .. }) = bar.library().rows().get(req.row) else {
            panic!("a request must name an entry row");
        };
        assert_eq!(&req.entry, id);
    }
}

#[test]
fn scrolling_the_popup_asks_only_for_the_newly_shown_rows() {
    let mut bar = overflowing_library(30);
    let before = requested_rows(&bar);

    let mut input = TaskbarInput::new();
    input.handle(
        InputEvent::PointerScrolled { dx: 0, dy: 1 },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );

    let after = requested_rows(&bar);
    assert_ne!(before, after, "scrolling changes which rows are shown");
    assert_eq!(
        after,
        shown_entry_rows(&bar),
        "after a scroll the requests still track the viewport exactly"
    );
    let newly: Vec<usize> = after
        .iter()
        .copied()
        .filter(|row| !before.contains(row))
        .collect();
    assert_eq!(
        newly.len(),
        1,
        "one row scrolled into view, so exactly one is newly asked for"
    );
    assert!(
        newly[0] > *before.last().expect("rows were requested"),
        "the newly asked-for row is the one that came into view below"
    );
}

#[test]
fn a_rebuild_drops_stale_row_artwork() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let magenta = Color::rgb(255, 0, 255).premultiply();
    bar.library_mut()
        .set_row_artwork(1, Surface::filled(16, 16, magenta));
    assert!(bar.library().row_artwork(1).is_some());

    // Re-cataloguing re-indexes the rows, so artwork keyed to the old
    // indices must not survive and draw the wrong application's icon.
    bar.library_mut().set_catalog(office_and_games());
    assert!(bar.library().row_artwork(1).is_none());

    // An index past the end is ignored rather than panicking.
    bar.library_mut()
        .set_row_artwork(9_999, Surface::filled(16, 16, magenta));
    assert!(bar.library().row_artwork(9_999).is_none());
}

#[test]
fn a_popup_row_draws_its_artwork_and_a_row_without_it_draws_the_glyph() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let magenta = Color::rgb(255, 0, 255).premultiply();
    let (row, _) = visible_row_where(&bar, |row| matches!(row, LibraryRow::Entry { .. }))
        .expect("an entry row is shown");
    bar.library_mut()
        .set_row_artwork(row, Surface::filled(16, 16, magenta));

    let layout = bar.library_layout(Scale::ONE);
    let rect = layout
        .rows
        .iter()
        .find(|&&(index, _)| index == row)
        .map(|&(_, rect)| rect)
        .expect("the row is laid out");
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_library(&bar, Scale::ONE)
        .expect("popup renders");
    assert!(region_has_pixel(&surface, layout.panel, rect, magenta));

    // Every other shown row still inks its built-in glyph, so a library
    // with no resolved artwork is never a column of blank slots.
    for &(index, other) in &layout.rows {
        if index == row {
            continue;
        }
        assert!(
            region_has_role_ink(
                &surface,
                layout.panel,
                other,
                theme.palette().on_surface,
                floating_ground(&theme, theme.palette().surface),
            ),
            "row {index} must draw without artwork"
        );
    }
}

// ---- notification popover -------------------------------------------

#[test]
fn no_popover_until_a_notification_is_raised() {
    let bar = bottom_bar();
    assert!(bar.notifications_layout(Scale::ONE).is_none());
    assert!(TaskbarRenderer::new(test_icon_cache())
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
    let _ = TaskbarRenderer::new(test_icon_cache()).render_notifications(&bar, Scale::ONE);
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
        let surface = TaskbarRenderer::new(test_icon_cache())
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
        power_capable: false,
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

/// Rest the pointer on `at` until the picker's dwell elapses, so the router
/// asks for the picker, and answer with the cells the way the session does.
///
/// The dwell is the whole point of the opening delay, so every test that
/// wants an *open* picker goes through it rather than reaching past it.
fn dwell_on(input: &mut TaskbarInput, taskbar: &mut Taskbar, at: Point) {
    assert_eq!(
        moved_at(input, taskbar, at, NOW_NS),
        TaskbarResponse::Ignored,
        "arriving is not resting"
    );
    let TaskbarResponse::ShowWindowPicker { app } =
        input.tick(taskbar, NOW_NS + PICKER_OPEN_DELAY_NS)
    else {
        panic!("the dwell elapsed without asking for a picker");
    };
    taskbar.show_window_picker(app, cells(taskbar, app), Scale::ONE);
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
    let theme = Theme::dark();
    let border = i32::try_from(plate_border(&theme, Scale::ONE)).expect("a modest border");
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        let config = TaskbarConfig {
            edge,
            ..TaskbarConfig::bottom_bar(1000, 800)
        };
        let bar = Taskbar::new(config, &theme);
        let layout = bar.layout(Scale::ONE);
        // Trailing-most on the *bar*, which stands off the screen by the
        // margin it floats in and seats its regions inside its own rim —
        // both read from the bar rather than restated here.
        let (slot_start, slot_end, clock_end, main_end) = match edge.orientation() {
            Orientation::Horizontal => (
                layout.switchboard.left(),
                layout.switchboard.right(),
                layout.clock.right(),
                layout.bar.right(),
            ),
            Orientation::Vertical => (
                layout.switchboard.top(),
                layout.switchboard.bottom(),
                layout.clock.bottom(),
                layout.bar.bottom(),
            ),
        };
        assert_eq!(
            slot_end,
            main_end - border,
            "{edge:?}: the capsule is trailing-most, up against the rim"
        );
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
    // 109 px, less the two 5 px margins the bar floats in and the two 1 px
    // rims its content sits inside, leaves 97: the leading end (48 of
    // launcher plus the 17 px separator gutter) and what remains for the
    // capsule. The clock and the notification area collapse to nothing
    // first, and the capsule clips into the leftover 32 px rather than
    // overlaying the launcher.
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(109, 800), &Theme::dark());
    bar.set_status_signals(alloc::vec![StatusSignal::new(
        IconId(1),
        StatusKind::Network
    )]);
    let layout = bar.layout(Scale::ONE);
    assert_eq!(layout.switchboard, Rect::new(71, 756, 32, 38));
    assert!(layout.clock.is_empty());
    assert!(layout.notification_area.is_empty());
    assert!(layout.notifications[0].is_empty());
    assert_eq!(
        layout.hit_test(centre_of(layout.switchboard)),
        Some(Hit::Switchboard)
    );
}

#[test]
fn tiny_screen_clips_the_switchboard_against_the_launcher() {
    // 77 px is exactly the permanent launcher and the separator gutter after
    // it: every region beyond, the capsule included, fails closed to empty
    // rather than overlaying it.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(77, 800), &Theme::dark());
    let layout = bar.layout(Scale::ONE);
    assert!(layout.switchboard.is_empty());
    assert!(layout.clock.is_empty());
    assert!(layout.app_strip.is_empty());
    assert_eq!(layout.hit_test(Point::new(20, 780)), Some(Hit::Library));

    // An absurd sliver clips into the launcher itself; nothing panics and
    // neither the empty capsule slot nor the empty rule can be hit.
    let sliver = Taskbar::new(TaskbarConfig::bottom_bar(10, 800), &Theme::dark());
    let slim = sliver.layout(Scale::ONE);
    assert!(slim.switchboard.is_empty());
    assert!(slim.separator.is_empty());
    assert!(slim.app_strip.is_empty());
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
        TaskbarResponse::WindowChosen { id: TaskId(1) }
    );
    // Forward again advances...
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 1),
        TaskbarResponse::WindowChosen { id: TaskId(2) }
    );
    // ...and backward returns (dx is the fallback when dy is zero).
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, -1, 0),
        TaskbarResponse::WindowChosen { id: TaskId(1) }
    );
    // Backward from the first entry wraps to the last.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, -2),
        TaskbarResponse::WindowChosen { id: TaskId(3) }
    );
    // Forward from the last wraps to the first.
    assert_eq!(
        scroll_at(&mut input, &mut bar, capsule, 0, 3),
        TaskbarResponse::WindowChosen { id: TaskId(1) }
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
        TaskbarResponse::WindowChosen { id: TaskId(8) }
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
        TaskbarResponse::WindowChosen { id: TaskId(1) }
    );
}

#[test]
fn middle_press_switches_to_the_previous_task() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().set_focused(Some(TaskId(1)));
    bar.tasks_mut().set_focused(Some(TaskId(2)));
    assert_eq!(bar.tasks().previous(), Some(TaskId(1)));

    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::WindowChosen { id: TaskId(1) }
    );
    // The switch itself was a handover: pressing again toggles back.
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::WindowChosen { id: TaskId(2) }
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
    bar.tasks_mut().set_focused(Some(TaskId(1)));
    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);
    // Focus arrived from the desktop: there is no previous task yet.
    assert_eq!(
        middle_press_at(&mut input, &mut bar, capsule),
        TaskbarResponse::Ignored
    );

    // A remembered task that closed is forgotten, never resurrected.
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().set_focused(Some(TaskId(2)));
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
            section: CommandSection::Tasks
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
            section: CommandSection::Recovery
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
            section: CommandSection::Recovery
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
            section: CommandSection::Tasks
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
        TaskbarResponse::Ignored
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render(&bar, Scale::ONE, &mut NoArtwork)
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
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render(bar, Scale::ONE, &mut NoArtwork)
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
        let surface = TaskbarRenderer::new(test_icon_cache())
            .render(&bar, Scale::ONE, &mut NoArtwork)
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
    assert!(TaskbarRenderer::new(test_icon_cache())
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
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_tray_readout(&bar, Scale::ONE)
        .expect("readout renders");
    assert_eq!(surface.width(), layout.panel.width);
    assert_eq!(surface.height(), layout.panel.height);
    // The plate carries the state-name ink over the floating chrome ground.
    assert!(region_has_role_ink(
        &surface,
        layout.panel,
        layout.panel,
        theme.palette().on_surface,
        floating_ground(&theme, theme.palette().surface_raised),
    ));
}

/// The readout's *Open Switchboard* button is furniture standing on the
/// glass, not part of it: a step more solid than the ground it sits on, so it
/// reads as pressable, while the blurred desktop still shows through it.
#[test]
fn the_readout_action_is_a_plate_raised_on_the_floating_ground() {
    let theme = Theme::dark();
    let mut bar = bottom_bar();
    bar.set_tray_summary(Some(tray_summary(0, 0, 300)));
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);

    let layout = bar.tray_readout_layout(Scale::ONE).expect("expanded");
    let surface = TaskbarRenderer::new(test_icon_cache())
        .render_tray_readout(&bar, Scale::ONE)
        .expect("readout renders");

    let ground = floating_ground(&theme, theme.palette().surface_raised);
    let plate = floating_plate(&theme, theme.palette().surface_raised);
    assert!(
        plate.a > ground.a && plate.a < 255,
        "a raised plate is a step more solid than its ground, never opaque"
    );
    assert!(
        region_has_pixel(&surface, layout.panel, layout.panel, plate),
        "the action button wears no plate of its own"
    );
    assert!(
        !region_has_pixel(
            &surface,
            layout.panel,
            layout.panel,
            role(theme.palette().surface_raised)
        ),
        "nothing on the readout is filled opaque"
    );
}

// ---- floating chrome --------------------------------------------------

#[test]
fn every_popup_the_bar_opens_grounds_itself_in_the_floating_chrome() {
    let theme = Theme::dark();
    let renderer = TaskbarRenderer::new(test_icon_cache());
    // Each popup grounds in the colour role it wears solid: a `Panel` its own
    // surface, a menu and the readout the raised one. What is under test is
    // that all four let the backdrop through at the theme's one weight.
    let grounded = |label: &str, surface: &Surface, frame: Rect, region: Rect, fill| {
        let ground = floating_ground(&theme, fill);
        assert!(ground.a < 255, "the {label} ground covers");
        assert!(
            region_has_pixel(surface, frame, region, ground),
            "the {label} popup grounds itself in the translucent chrome the \
             compositor's backdrop blur reads through"
        );
    };

    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_library(&mut input, &mut bar);
    let library = bar.library_layout(Scale::ONE);
    grounded(
        "library",
        &renderer.render_library(&bar, Scale::ONE).expect("renders"),
        library.panel,
        library.viewport,
        theme.palette().surface,
    );

    let mut bar = bar_with_declared_app();
    bar.open_app_menu(0, Rect::new(100, 760, 48, 40));
    let menu = bar.menu_layout(Scale::ONE).expect("menu layout");
    grounded(
        "context menu",
        &renderer.render_menu(&bar, Scale::ONE).expect("renders"),
        menu.panel,
        menu.panel,
        theme.palette().surface_raised,
    );

    let mut bar = bottom_bar();
    let _ = bar.raise_notification(TransientNotification::new(
        2,
        5,
        NotifySeverity::Info,
        "Ready",
        "All done",
    ));
    let notifications = bar.notifications_layout(Scale::ONE).expect("popover");
    grounded(
        "notification",
        &renderer
            .render_notifications(&bar, Scale::ONE)
            .expect("renders"),
        notifications.panel,
        notifications.panel,
        theme.palette().surface,
    );

    let mut bar = bottom_bar();
    bar.set_tray_summary(Some(tray_summary(0, 0, 300)));
    let mut input = TaskbarInput::new();
    hover_switchboard(&mut input, &mut bar);
    let readout = bar.tray_readout_layout(Scale::ONE).expect("expanded");
    grounded(
        "Switchboard readout",
        &renderer
            .render_tray_readout(&bar, Scale::ONE)
            .expect("renders"),
        readout.panel,
        readout.panel,
        theme.palette().surface_raised,
    );
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
            | TaskbarRepaint::PICKER
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
            picker: false,
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
    assert_eq!(bar.apps().len(), 0);
    assert_eq!(bar.apps().hover(), None);
    assert!(!bar.picker().is_open());

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

// ---- the system quick-actions menu ------------------------------------

/// A taskbar whose catalog holds the terminal bundle the *Task Shell* row
/// launches, so that row is actionable.
fn bar_with_task_shell() -> Taskbar {
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    let mut catalog = office_and_games();
    catalog
        .insert(entry("terminal", "Terminal", LibraryCategory::Utilities))
        .expect("fits");
    bar.library_mut().set_catalog(catalog);
    let _ = bar.take_repaint();
    bar
}

/// A published summary that attests the service can power the machine.
fn power_capable_summary() -> TraySummary {
    let mut summary = tray_summary(0, 0, 100);
    summary.power_capable = true;
    summary
}

/// Move the pointer to `at` and press the secondary button there.
fn secondary_press_at(
    input: &mut TaskbarInput,
    taskbar: &mut Taskbar,
    at: Point,
) -> TaskbarResponse {
    input.handle(
        InputEvent::PointerMoved { to: at },
        taskbar,
        Scale::ONE,
        NOW_NS,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        taskbar,
        Scale::ONE,
        NOW_NS,
    )
}

/// Open the system menu by right-clicking the Switchboard capsule,
/// asserting it opened.
fn open_system_menu(input: &mut TaskbarInput, taskbar: &mut Taskbar) {
    let capsule = centre_of(taskbar.layout(Scale::ONE).switchboard);
    assert_eq!(
        secondary_press_at(input, taskbar, capsule),
        TaskbarResponse::Ignored,
        "opening the menu acts on nothing by itself"
    );
    assert!(taskbar.menu().is_open());
}

/// Choose the row at `index` with the keyboard: walk Down to it from the
/// top, then press Enter.
fn choose_row(input: &mut TaskbarInput, taskbar: &mut Taskbar, index: usize) -> TaskbarResponse {
    for _ in 0..=index {
        press_key(input, taskbar, Key::Named(NamedKey::Down));
    }
    assert_eq!(
        taskbar.menu().control().current(),
        Some(index),
        "the keyboard highlight reached row {index}"
    );
    press_key(input, taskbar, Key::Named(NamedKey::Enter))
}

#[test]
fn the_row_table_renders_its_labels_groups_and_roles() {
    let mut bar = bar_with_task_shell();
    bar.set_tray_summary(Some(power_capable_summary()));
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    let rows: Vec<(&str, bool, tairix_controls::ControlRole)> = bar
        .menu()
        .control()
        .items()
        .iter()
        .map(|item| (item.label(), item.is_group_break(), item.role()))
        .collect();
    assert_eq!(
        rows,
        alloc::vec![
            (
                "About This System",
                false,
                tairix_controls::ControlRole::Neutral
            ),
            (
                "System Monitor",
                false,
                tairix_controls::ControlRole::Neutral
            ),
            ("Task Shell", false, tairix_controls::ControlRole::Neutral),
            (
                "Light Appearance",
                true,
                tairix_controls::ControlRole::Neutral
            ),
            (
                "Dark Appearance",
                false,
                tairix_controls::ControlRole::Neutral
            ),
            ("Lock Screen", true, tairix_controls::ControlRole::Neutral),
            ("Log Out", false, tairix_controls::ControlRole::Neutral),
            ("Restart", false, tairix_controls::ControlRole::Destructive),
            (
                "Shut Down",
                false,
                tairix_controls::ControlRole::Destructive
            ),
        ]
    );
}

#[test]
fn the_active_appearance_row_carries_the_check_and_is_not_actionable() {
    // Every row index is a direct index into the command table, whichever
    // appearance is active: a group break draws a divider above a row, it is
    // never a row of its own.
    for (appearance, active_row, inactive_row) in
        [(Appearance::Dark, 4, 3), (Appearance::Light, 3, 4)]
    {
        let mut bar = Taskbar::new(
            TaskbarConfig::bottom_bar(1000, 800),
            &if appearance == Appearance::Dark {
                Theme::dark()
            } else {
                Theme::light()
            },
        );
        let mut input = TaskbarInput::new();
        open_system_menu(&mut input, &mut bar);
        let items = bar.menu().control().items();

        let active = &items[active_row];
        assert_eq!(active.state().activity, ActivityState::Complete);
        assert!(
            !active.state().is_actionable(),
            "the appearance already in use cannot be chosen again ({appearance:?})"
        );
        assert_eq!(active.reason(), Some("Already in use"));

        let inactive = &items[inactive_row];
        assert!(
            inactive.state().is_actionable(),
            "the other appearance is the one worth choosing ({appearance:?})"
        );
        assert_ne!(inactive.state().activity, ActivityState::Complete);
        assert_eq!(inactive.reason(), None);
    }
}

#[test]
fn a_secondary_press_elsewhere_on_the_bar_opens_no_system_menu() {
    let mut bar = bar_with_task_shell();
    let mut input = TaskbarInput::new();
    // The Library button is a live control that carries no menu of its own.
    let library = centre_of(bar.layout(Scale::ONE).library);

    assert_eq!(
        secondary_press_at(&mut input, &mut bar, library),
        TaskbarResponse::Ignored
    );
    assert!(
        !bar.menu().is_open(),
        "only the capsule opens the system menu"
    );
}

#[test]
fn the_system_menu_is_modal_and_a_click_away_dismisses_without_acting() {
    let mut bar = bar_with_task_shell();
    bar.set_tray_summary(Some(power_capable_summary()));
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    // A press well clear of the plate: it dismisses the menu and nothing
    // else acts on it.
    let away = Point::new(5, 5);
    assert_eq!(
        press_at(&mut input, &mut bar, away.x, away.y),
        TaskbarResponse::Ignored
    );
    assert!(!bar.menu().is_open());
}

#[test]
fn escape_dismisses_the_system_menu_without_acting() {
    let mut bar = bar_with_task_shell();
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Escape)),
        TaskbarResponse::Ignored
    );
    assert!(!bar.menu().is_open());
}

#[test]
fn keyboard_navigation_reaches_every_row_and_enter_activates_the_highlighted_one() {
    let mut bar = bar_with_task_shell();
    bar.set_tray_summary(Some(power_capable_summary()));
    // Every row of the table offered, so the walk really does visit them all.
    bar.set_switch_user_available(true);
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    // Down visits each row in turn, including the non-actionable one: a
    // group break is a divider drawn above a row, never a row to skip.
    for expected in 0..crate::system::ROWS.len() {
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Down));
        assert_eq!(bar.menu().control().current(), Some(expected));
    }

    // Enter on the last row (Shut Down) activates exactly it.
    assert_eq!(
        press_key(&mut input, &mut bar, Key::Named(NamedKey::Enter)),
        TaskbarResponse::ConfirmSystemPower {
            action: tairix_abi::PowerAction::PowerOff,
        }
    );
    assert!(!bar.menu().is_open(), "activating closes the menu");
}

#[test]
fn every_row_maps_to_exactly_its_expected_response() {
    let expected = alloc::vec![
        TaskbarResponse::OpenSwitchboard {
            section: CommandSection::System,
        },
        TaskbarResponse::OpenSwitchboard {
            section: CommandSection::Tasks,
        },
        TaskbarResponse::LibraryLaunch {
            entry: EntryId::new("os.tairix.terminal").expect("id"),
        },
        TaskbarResponse::SetAppearance {
            appearance: Appearance::Light,
        },
        // The dark row is the one in use under the dark theme, so it is not
        // actionable and reports nothing; the light row above it is.
        TaskbarResponse::Ignored,
        TaskbarResponse::LockSession,
        TaskbarResponse::SwitchUser,
        TaskbarResponse::LogOut,
        TaskbarResponse::ConfirmSystemPower {
            action: tairix_abi::PowerAction::Restart,
        },
        TaskbarResponse::ConfirmSystemPower {
            action: tairix_abi::PowerAction::PowerOff,
        },
    ];
    assert_eq!(
        expected.len(),
        crate::system::ROWS.len(),
        "the table and the expectations describe the same menu"
    );

    for (index, want) in expected.into_iter().enumerate() {
        let mut bar = bar_with_task_shell();
        bar.set_tray_summary(Some(power_capable_summary()));
        bar.set_elevation_available(true);
        bar.set_switch_user_available(true);
        let mut input = TaskbarInput::new();
        open_system_menu(&mut input, &mut bar);
        assert_eq!(
            choose_row(&mut input, &mut bar, index),
            want,
            "row {index} ({})",
            crate::system::ROWS[index].label
        );
    }
}

#[test]
fn an_unpermitted_power_row_is_denied_with_the_authority_mark_and_a_reason() {
    // The service published, and said it cannot power the machine.
    let mut bar = bar_with_task_shell();
    bar.set_tray_summary(Some(tray_summary(0, 0, 100)));
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    for row in [7, 8] {
        let item = &bar.menu().control().items()[row];
        assert_eq!(
            item.state().authority,
            tairix_controls::AuthorityState::NeedsCapability,
            "{} carries the Authority Mark",
            item.label()
        );
        assert!(!item.state().is_actionable());
        assert_eq!(
            item.reason(),
            Some("The system service cannot power this machine")
        );
    }

    // Choosing one reports nothing: a refused row acts on nothing at all.
    assert_eq!(
        choose_row(&mut input, &mut bar, 7),
        TaskbarResponse::Ignored
    );
    assert!(
        bar.menu().is_open(),
        "a non-actionable row neither acts nor closes the menu"
    );
}

#[test]
fn the_power_rows_are_denied_when_no_authority_has_been_published() {
    // No summary at all — the service has not published yet, or has died.
    // Silence is not permission.
    let mut bar = bar_with_task_shell();
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    for row in [7, 8] {
        let item = &bar.menu().control().items()[row];
        assert_eq!(
            item.state().authority,
            tairix_controls::AuthorityState::NeedsCapability,
            "{} fails closed before anything is attested",
            item.label()
        );
        assert!(!item.state().is_actionable());
    }

    // A summary that arrives claiming the authority permits the rows, and a
    // service that then dies withdraws them again.
    bar.close_menu();
    bar.set_tray_summary(Some(power_capable_summary()));
    open_system_menu(&mut input, &mut bar);
    assert!(bar.menu().control().items()[7].state().is_actionable());

    bar.close_menu();
    bar.set_tray_summary(None);
    open_system_menu(&mut input, &mut bar);
    assert!(!bar.menu().control().items()[7].state().is_actionable());
}

#[test]
fn the_lock_row_is_denied_until_the_session_attests_it_can_prompt() {
    // Nothing attested yet: a bar that was never told offers no lock,
    // because a lock with no way back is a trap, not a security measure.
    let mut bar = bar_with_task_shell();
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    let item = &bar.menu().control().items()[5];
    assert_eq!(item.label(), "Lock Screen");
    assert_eq!(
        item.state().authority,
        tairix_controls::AuthorityState::NeedsCapability,
        "the lock row fails closed before the session attests"
    );
    assert!(!item.state().is_actionable());
    assert_eq!(
        item.reason(),
        Some("This session has no password prompt to unlock with")
    );
    assert_eq!(
        choose_row(&mut input, &mut bar, 5),
        TaskbarResponse::Ignored,
        "a lock that could never be undone is never emitted"
    );

    // The session attests, and the row becomes the real command.
    bar.close_menu();
    bar.set_elevation_available(true);
    open_system_menu(&mut input, &mut bar);
    let item = &bar.menu().control().items()[5];
    assert!(item.state().is_actionable());
    assert_eq!(item.reason(), None);
    assert_eq!(
        choose_row(&mut input, &mut bar, 5),
        TaskbarResponse::LockSession
    );
}

#[test]
fn the_switch_user_row_is_absent_until_the_session_can_be_resumed() {
    // Nothing attested: a desktop with no wake mailbox cannot be resumed,
    // so the row is left out rather than offered and then refused.
    let mut bar = bar_with_task_shell();
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    let items = bar.menu().control().items();
    assert_eq!(items.len(), crate::system::ROWS.len() - 1);
    assert!(items.iter().all(|item| item.label() != "Switch User…"));
    // The rows below it close up, and the command mapping closes up with
    // them, so nothing below is reachable at a stale index.
    assert_eq!(items[5].label(), "Lock Screen");
    assert_eq!(items[6].label(), "Log Out");
    assert_eq!(choose_row(&mut input, &mut bar, 6), TaskbarResponse::LogOut);

    // The session attests it bound the mailbox, and the row appears.
    bar.close_menu();
    bar.set_switch_user_available(true);
    open_system_menu(&mut input, &mut bar);

    let item = &bar.menu().control().items()[6];
    assert_eq!(item.label(), "Switch User…");
    assert!(item.state().is_actionable());
    assert_eq!(item.reason(), None);
    assert_eq!(
        choose_row(&mut input, &mut bar, 6),
        TaskbarResponse::SwitchUser
    );
}

#[test]
fn attesting_the_wake_mailbox_latches_only_the_menu_surface() {
    let mut bar = bar_with_task_shell();
    let _ = bar.take_repaint();

    bar.set_switch_user_available(true);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::MENU);

    // Re-attesting the same answer changes no pixel anywhere.
    bar.set_switch_user_available(true);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::NONE);
}

#[test]
fn attesting_the_broker_latches_only_the_menu_surface() {
    let mut bar = bar_with_task_shell();
    let _ = bar.take_repaint();

    bar.set_elevation_available(true);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::MENU);

    // Re-attesting the same answer changes no pixel anywhere.
    bar.set_elevation_available(true);
    assert_eq!(bar.take_repaint(), TaskbarRepaint::NONE);
}

// ---- the clock's menu --------------------------------------------------

/// Open the clock's menu with a secondary press on the clock.
fn open_clock_menu(input: &mut TaskbarInput, bar: &mut Taskbar) {
    let clock = centre_of(bar.layout(Scale::ONE).clock);
    input.handle(
        InputEvent::PointerMoved { to: clock },
        bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: tairix_input::PointerButton::Secondary,
            },
            bar,
            Scale::ONE,
            NOW_NS,
        ),
        TaskbarResponse::Ignored,
        "opening a menu acts on nothing by itself"
    );
}

#[test]
fn the_clock_menu_states_the_reading_the_bar_is_drawing() {
    let mut bar = bottom_bar();
    bar.clock_mut().set_label("09:41");
    let mut input = TaskbarInput::new();
    open_clock_menu(&mut input, &mut bar);

    let items = bar.menu().control().items();
    assert_eq!(items.len(), crate::clock_menu::ROWS.len());
    assert_eq!(items[0].label(), "09:41");
    // A statement, not a command: choosing it asks for nothing.
    assert_eq!(
        choose_row(&mut input, &mut bar, 0),
        TaskbarResponse::Ignored
    );
}

#[test]
fn an_unset_clock_menu_says_so_rather_than_showing_a_fabricated_time() {
    // Nothing has set the wall clock this boot, so the bar draws its
    // placeholder — which a heading repeating it would read as a time.
    let mut bar = bottom_bar();
    bar.clock_mut().set_label(crate::clock::UNSET_LABEL);
    let mut input = TaskbarInput::new();
    open_clock_menu(&mut input, &mut bar);

    assert_eq!(
        bar.menu().control().items()[0].label(),
        crate::clock_menu::READING_UNSET_LABEL
    );
}

#[test]
fn the_set_time_row_is_denied_until_the_session_attests_a_broker() {
    // Nothing attested yet: setting the clock needs an account that may,
    // and this session has nothing to authenticate one against.
    let mut bar = bottom_bar();
    bar.clock_mut().set_label("09:41");
    let mut input = TaskbarInput::new();
    open_clock_menu(&mut input, &mut bar);

    let item = &bar.menu().control().items()[1];
    assert_eq!(item.label(), crate::clock_menu::SET_ROW_LABEL);
    assert_eq!(
        item.state().authority,
        tairix_controls::AuthorityState::NeedsCapability,
        "the set-time row fails closed before the session attests"
    );
    assert!(!item.state().is_actionable());
    assert_eq!(item.reason(), Some(crate::clock_menu::REASON_NO_BROKER));
    assert_eq!(
        choose_row(&mut input, &mut bar, 1),
        TaskbarResponse::Ignored,
        "a command that could only fail is never emitted"
    );

    // The session attests, and the row becomes the real command.
    bar.close_menu();
    bar.set_elevation_available(true);
    open_clock_menu(&mut input, &mut bar);
    let item = &bar.menu().control().items()[1];
    assert!(item.state().is_actionable());
    assert_eq!(item.reason(), None);
    assert_eq!(
        choose_row(&mut input, &mut bar, 1),
        TaskbarResponse::SetDateTime
    );
}

#[test]
fn a_primary_press_on_the_clock_opens_no_menu() {
    // The reported defect: a left click on the clock popped its menu up.
    // A menu is what a secondary press asks for, here as everywhere else on
    // the desktop; a primary press on a reading is claimed and inert.
    let mut bar = bottom_bar();
    bar.set_elevation_available(true);
    let mut input = TaskbarInput::new();
    let clock = centre_of(bar.layout(Scale::ONE).clock);
    assert_eq!(
        press_at(&mut input, &mut bar, clock.x, clock.y),
        TaskbarResponse::Ignored
    );
    assert!(
        !bar.menu().is_open(),
        "a left click on the clock opened its menu"
    );
    // The clock still has its menu, and it is still the same one: the
    // secondary press that asks for it reaches the set-time command.
    open_clock_menu(&mut input, &mut bar);
    assert_eq!(
        choose_row(&mut input, &mut bar, 1),
        TaskbarResponse::SetDateTime
    );
}

#[test]
fn a_launch_row_whose_bundle_is_absent_is_disabled_and_emits_nothing() {
    // The standard fixture has no terminal bundle.
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    open_system_menu(&mut input, &mut bar);

    let item = &bar.menu().control().items()[2];
    assert_eq!(item.label(), "Task Shell");
    assert!(!item.state().is_actionable());
    assert_eq!(item.reason(), Some("Not installed"));

    assert_eq!(
        choose_row(&mut input, &mut bar, 2),
        TaskbarResponse::Ignored,
        "a launch that must fail is never emitted"
    );
}

#[test]
fn opening_the_system_menu_latches_only_the_menu_surface() {
    let mut bar = bar_with_task_shell();
    let mut input = TaskbarInput::new();
    let capsule = centre_of(bar.layout(Scale::ONE).switchboard);

    // Settle the hover the move itself causes, so what is measured is the
    // press alone.
    input.handle(
        InputEvent::PointerMoved { to: capsule },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    let _ = bar.take_repaint();

    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        },
        &mut bar,
        Scale::ONE,
        NOW_NS,
    );
    assert_eq!(bar.take_repaint(), TaskbarRepaint::MENU);
}
