//! Headless unit tests for the taskbar layout and model.

use rustos_geometry::{Point, Rect};
use rustos_theme::Theme;

use crate::edge::{Edge, Orientation};
use crate::layout::{BarLayout, Hit};
use crate::menu::{MenuAction, MenuEntryId, SessionControl, StartMenu};
use crate::notifications::{IconId, NotificationArea};
use crate::taskbar::{Taskbar, TaskbarConfig};
use crate::tasks::{ActivateOutcome, TaskId, TaskList};

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

// ---- start menu -----------------------------------------------------

#[test]
fn start_menu_holds_only_session_controls() {
    let menu = StartMenu::with_session_controls();
    assert!(!menu.is_open());
    assert_eq!(menu.len(), 4);
    let actions: alloc::vec::Vec<_> = menu.entries().iter().map(|e| e.action).collect();
    assert_eq!(
        actions,
        [
            MenuAction::Session(SessionControl::LogOut),
            MenuAction::Session(SessionControl::Lock),
            MenuAction::Session(SessionControl::ShutDown),
            MenuAction::Session(SessionControl::Restart),
        ]
    );
    assert_eq!(menu.entries()[2].label(), "Shut Down");
}

#[test]
fn start_menu_toggles() {
    let mut menu = StartMenu::with_session_controls();
    assert!(menu.toggle());
    assert!(menu.is_open());
    assert!(!menu.toggle());
    assert!(!menu.is_open());
}

#[test]
fn start_menu_activate_returns_action_and_closes() {
    let mut menu = StartMenu::with_session_controls();
    menu.toggle();
    let id = menu.entries()[0].id;
    assert_eq!(
        menu.activate(id),
        Some(MenuAction::Session(SessionControl::LogOut))
    );
    assert!(!menu.is_open());
}

#[test]
fn start_menu_unknown_entry_is_fail_closed() {
    let mut menu = StartMenu::with_session_controls();
    menu.toggle();
    assert_eq!(menu.activate(MenuEntryId(9999)), None);
    assert!(menu.is_open(), "an unknown id changes nothing");
}

// ---- task list ------------------------------------------------------

#[test]
fn task_add_is_unfocused_and_rejects_duplicates() {
    let mut tasks = TaskList::new();
    assert!(tasks.add(TaskId(1), "Editor"));
    assert!(!tasks.add(TaskId(1), "Editor again"));
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks.focused(), None);
}

#[test]
fn task_activate_focuses_then_minimises() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.add(TaskId(2), "Browser");

    assert_eq!(tasks.activate(TaskId(1)), ActivateOutcome::Activated);
    assert_eq!(tasks.focused(), Some(TaskId(1)));
    assert!(!tasks.is_minimised(TaskId(1)));

    // Clicking the focused task minimises it and drops focus.
    assert_eq!(tasks.activate(TaskId(1)), ActivateOutcome::Minimised);
    assert_eq!(tasks.focused(), None);
    assert!(tasks.is_minimised(TaskId(1)));

    // Clicking a minimised task restores and focuses it.
    assert_eq!(tasks.activate(TaskId(1)), ActivateOutcome::Activated);
    assert!(!tasks.is_minimised(TaskId(1)));
    assert_eq!(tasks.focused(), Some(TaskId(1)));
}

#[test]
fn task_activate_switches_focus() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.add(TaskId(2), "Browser");
    tasks.activate(TaskId(1));
    assert_eq!(tasks.activate(TaskId(2)), ActivateOutcome::Activated);
    assert_eq!(tasks.focused(), Some(TaskId(2)));
}

#[test]
fn task_activate_unknown_is_fail_closed() {
    let mut tasks = TaskList::new();
    assert_eq!(tasks.activate(TaskId(7)), ActivateOutcome::Unknown);
    assert_eq!(tasks.focused(), None);
}

#[test]
fn task_remove_clears_focus() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.activate(TaskId(1));
    assert!(tasks.remove(TaskId(1)));
    assert_eq!(tasks.focused(), None);
    assert!(tasks.is_empty());
    assert!(!tasks.remove(TaskId(1)));
}

// ---- notification area ----------------------------------------------

#[test]
fn notifications_add_remove_and_dedup() {
    let mut area = NotificationArea::new();
    assert!(area.add(IconId(1), "icon.network"));
    assert!(!area.add(IconId(1), "icon.network"));
    assert!(area.add(IconId(2), "icon.volume"));
    assert_eq!(area.len(), 2);
    assert!(area.remove(IconId(1)));
    assert!(!area.remove(IconId(1)));
    assert_eq!(area.len(), 1);
}

// ---- layout ---------------------------------------------------------

fn bottom_layout(tasks: usize, icons: usize) -> BarLayout {
    let config = TaskbarConfig::bottom_bar(1000, 800);
    BarLayout::compute(&config, 12, tasks, icons)
}

#[test]
fn bottom_bar_regions_partition_the_bar() {
    let layout = bottom_layout(2, 2);
    assert_eq!(layout.bar, Rect::new(0, 760, 1000, 40));
    assert_eq!(layout.corner_radius, 12);
    assert_eq!(layout.start_button, Rect::new(0, 760, 48, 40));
    assert_eq!(layout.clock, Rect::new(920, 760, 80, 40));
    assert_eq!(layout.notification_area, Rect::new(872, 760, 48, 40));
    assert_eq!(layout.task_list, Rect::new(48, 760, 824, 40));

    assert_eq!(layout.tasks[0], Rect::new(48, 760, 160, 40));
    assert_eq!(layout.tasks[1], Rect::new(208, 760, 160, 40));
    assert_eq!(layout.notifications[0], Rect::new(872, 760, 24, 40));
    assert_eq!(layout.notifications[1], Rect::new(896, 760, 24, 40));
}

#[test]
fn bottom_bar_hit_testing() {
    let layout = bottom_layout(2, 2);
    assert_eq!(layout.hit_test(Point::new(10, 770)), Some(Hit::StartButton));
    assert_eq!(layout.hit_test(Point::new(100, 770)), Some(Hit::Task(0)));
    assert_eq!(layout.hit_test(Point::new(250, 770)), Some(Hit::Task(1)));
    assert_eq!(
        layout.hit_test(Point::new(880, 770)),
        Some(Hit::Notification(0))
    );
    assert_eq!(
        layout.hit_test(Point::new(900, 770)),
        Some(Hit::Notification(1))
    );
    assert_eq!(layout.hit_test(Point::new(950, 770)), Some(Hit::Clock));
    // A gap in the task-list region hits nothing.
    assert_eq!(layout.hit_test(Point::new(500, 770)), None);
    // Outside the bar entirely.
    assert_eq!(layout.hit_test(Point::new(500, 100)), None);
}

#[test]
fn overflowing_task_is_clipped_empty() {
    // Seven 160px tasks cannot fit the 824px task region: the trailing
    // slots that fall outside the region are empty and so never contain a
    // pointer (`Rect::contains` is false for an empty rectangle).
    let layout = bottom_layout(7, 0);
    assert_eq!(layout.tasks.len(), 7);
    assert!(
        layout
            .tasks
            .last()
            .copied()
            .expect("seven slots")
            .is_empty(),
        "the overflowing task slot is empty"
    );
    let fitting = layout.tasks.iter().filter(|r| !r.is_empty()).count();
    assert!(fitting < 7, "not every task fits the region");
    // The overflowing (empty) slots can never be hit: an empty rectangle
    // contains no point, so hit-testing only ever resolves to a fitting slot.
    assert!(matches!(
        layout.hit_test(Point::new(900, 770)),
        Some(Hit::Task(index)) if index < fitting
    ));
}

#[test]
fn every_edge_pins_the_bar_correctly() {
    let base = TaskbarConfig::bottom_bar(1000, 800);
    let thickness = base.thickness;

    let top = BarLayout::compute(
        &TaskbarConfig {
            edge: Edge::Top,
            ..base
        },
        12,
        0,
        0,
    );
    assert_eq!(top.bar, Rect::new(0, 0, 1000, thickness));

    let bottom = BarLayout::compute(
        &TaskbarConfig {
            edge: Edge::Bottom,
            ..base
        },
        12,
        0,
        0,
    );
    assert_eq!(bottom.bar, Rect::new(0, 760, 1000, thickness));

    let left = BarLayout::compute(
        &TaskbarConfig {
            edge: Edge::Left,
            ..base
        },
        12,
        0,
        0,
    );
    assert_eq!(left.bar, Rect::new(0, 0, thickness, 800));

    let right = BarLayout::compute(
        &TaskbarConfig {
            edge: Edge::Right,
            ..base
        },
        12,
        0,
        0,
    );
    assert_eq!(right.bar, Rect::new(960, 0, thickness, 800));
}

#[test]
fn vertical_bar_lays_regions_along_y() {
    let config = TaskbarConfig {
        edge: Edge::Left,
        ..TaskbarConfig::bottom_bar(1000, 800)
    };
    let layout = BarLayout::compute(&config, 12, 1, 0);
    assert_eq!(layout.start_button, Rect::new(0, 0, 40, 48));
    assert_eq!(layout.clock, Rect::new(0, 720, 40, 80));
    assert_eq!(layout.tasks[0], Rect::new(0, 48, 40, 160));
    assert_eq!(layout.hit_test(Point::new(10, 10)), Some(Hit::StartButton));
    assert_eq!(layout.hit_test(Point::new(10, 100)), Some(Hit::Task(0)));
    assert_eq!(layout.hit_test(Point::new(10, 760)), Some(Hit::Clock));
}

#[test]
fn tiny_screen_keeps_regions_inside_the_bar() {
    // A degenerate screen far smaller than the extents must not panic and
    // must keep every region within the bar (fail closed, §2.9).
    let config = TaskbarConfig::bottom_bar(20, 10);
    let layout = BarLayout::compute(&config, 0, 3, 3);
    let bar = layout.bar;
    for region in [
        layout.start_button,
        layout.clock,
        layout.notification_area,
        layout.task_list,
    ] {
        assert!(region.left() >= bar.left());
        assert!(region.top() >= bar.top());
        assert!(region.right() <= bar.right());
        assert!(region.bottom() <= bar.bottom());
    }
}

// ---- taskbar (theming integration) ----------------------------------

#[test]
fn taskbar_takes_corner_radius_from_theme() {
    let dark = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &dark);
    assert_eq!(bar.corner_radius(), dark.metrics().taskbar_corner_radius);
    assert_eq!(
        bar.layout().corner_radius,
        dark.metrics().taskbar_corner_radius
    );
    assert_eq!(bar.start_menu().len(), 4);
}

#[test]
fn taskbar_layout_tracks_live_counts() {
    let theme = Theme::dark();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    assert_eq!(bar.layout().tasks.len(), 0);
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.notifications_mut().add(IconId(1), "icon.network");
    assert_eq!(bar.layout().tasks.len(), 1);
    assert_eq!(bar.layout().notifications.len(), 1);
    assert_eq!(bar.hit_test(Point::new(10, 770)), Some(Hit::StartButton));
}

#[test]
fn apply_theme_switches_corner_radius() {
    let mut metrics = *Theme::dark().metrics();
    metrics.taskbar_corner_radius = 0;
    let squared = Theme::new(
        rustos_theme::ThemeId(42),
        "Square",
        rustos_theme::Appearance::Dark,
        *Theme::dark().palette(),
        metrics,
        Theme::dark().fonts().clone(),
        Theme::dark().cursors().clone(),
    );
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    assert_eq!(bar.corner_radius(), 12);
    bar.apply_theme(&squared);
    assert_eq!(bar.corner_radius(), 0);
}
