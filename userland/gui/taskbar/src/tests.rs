//! Headless unit tests for the taskbar layout, model, and rendering.

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_raster::{Color, Pixel, Surface};
use tairix_theme::Theme;

use crate::edge::{Edge, Orientation};
use crate::input::{TaskbarInput, TaskbarResponse};
use crate::layout::{BarLayout, Hit, MenuLayout};
use crate::menu::{LauncherId, MenuAction, MenuEntryId, SessionControl, StartMenu};
use crate::notifications::{IconId, NotificationArea};
use crate::render::TaskbarRenderer;
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

#[test]
fn add_launcher_appends_after_session_controls() {
    let mut menu = StartMenu::with_session_controls();
    let files = menu.add_launcher(LauncherId(42), "Files");
    let terminal = menu.add_launcher(LauncherId(7), "Terminal");

    // Launchers take the next ids after the fixed session ids 1..=4.
    assert_eq!(files, MenuEntryId(5));
    assert_eq!(terminal, MenuEntryId(6));
    assert_eq!(menu.len(), 6);

    // The session controls keep their position, id, and label.
    assert_eq!(menu.entries()[0].id, MenuEntryId(1));
    assert_eq!(menu.entries()[2].label(), "Shut Down");

    // The launchers carry their app-supplied label and a Launch action.
    assert_eq!(menu.entries()[4].label(), "Files");
    assert_eq!(menu.entries()[4].action, MenuAction::Launch(LauncherId(42)));
    assert_eq!(menu.entries()[5].label(), "Terminal");
    assert_eq!(menu.entries()[5].action, MenuAction::Launch(LauncherId(7)));
}

#[test]
fn activating_a_launcher_reports_its_action_and_closes() {
    let mut menu = StartMenu::with_session_controls();
    let id = menu.add_launcher(LauncherId(99), "Browser");
    menu.toggle();
    assert_eq!(menu.activate(id), Some(MenuAction::Launch(LauncherId(99))));
    assert!(!menu.is_open(), "selecting an entry closes the menu");
}

#[test]
fn add_appearance_toggle_appends_after_other_entries() {
    let mut menu = StartMenu::with_session_controls();
    let launcher = menu.add_launcher(LauncherId(42), "Files");
    let toggle = menu.add_appearance_toggle("Switch Theme");

    // The toggle takes the next id after the session controls and launcher.
    assert_eq!(launcher, MenuEntryId(5));
    assert_eq!(toggle, MenuEntryId(6));
    assert_eq!(menu.len(), 6);

    // The session controls keep their fixed ids.
    assert_eq!(menu.entries()[0].id, MenuEntryId(1));

    // The toggle carries its label and the ToggleAppearance action.
    assert_eq!(menu.entries()[5].label(), "Switch Theme");
    assert_eq!(menu.entries()[5].action, MenuAction::ToggleAppearance);
}

#[test]
fn activating_the_appearance_toggle_reports_its_action_and_closes() {
    let mut menu = StartMenu::with_session_controls();
    let id = menu.add_appearance_toggle("Switch Theme");
    menu.toggle();
    assert_eq!(menu.activate(id), Some(MenuAction::ToggleAppearance));
    assert!(!menu.is_open(), "selecting an entry closes the menu");
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

#[test]
fn set_focused_mirrors_and_restores() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.add(TaskId(2), "Browser");
    // Minimise task 1 so set_focused can be seen to restore it.
    tasks.activate(TaskId(1));
    assert_eq!(tasks.activate(TaskId(1)), ActivateOutcome::Minimised);
    assert!(tasks.is_minimised(TaskId(1)));

    // Mirroring the window manager's focus onto task 1 highlights and restores it.
    assert!(tasks.set_focused(Some(TaskId(1))));
    assert_eq!(tasks.focused(), Some(TaskId(1)));
    assert!(!tasks.is_minimised(TaskId(1)));

    // Clearing focus (a desktop press) drops the highlight.
    assert!(tasks.set_focused(None));
    assert_eq!(tasks.focused(), None);
}

#[test]
fn set_focused_fails_closed_on_unknown_id() {
    let mut tasks = TaskList::new();
    tasks.add(TaskId(1), "Editor");
    tasks.set_focused(Some(TaskId(1)));

    assert!(
        !tasks.set_focused(Some(TaskId(99))),
        "an unknown id is rejected"
    );
    assert_eq!(
        tasks.focused(),
        Some(TaskId(1)),
        "the existing highlight is left untouched"
    );
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
    BarLayout::compute(&config, 12, Scale::ONE, tasks, icons)
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
        Scale::ONE,
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
        Scale::ONE,
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
        Scale::ONE,
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
        Scale::ONE,
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
    let layout = BarLayout::compute(&config, 12, Scale::ONE, 1, 0);
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
    // must keep every region within the bar (fail closed).
    let config = TaskbarConfig::bottom_bar(20, 10);
    let layout = BarLayout::compute(&config, 0, Scale::ONE, 3, 3);
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

// ---- DPI / scale ----------------------------------------------------

#[test]
fn doubling_the_scale_doubles_logical_lengths() {
    // A large physical screen so nothing clamps: the logical extents simply
    // double at 200%, while the physical screen dimensions are untouched.
    let config = TaskbarConfig::bottom_bar(4000, 2000);
    let scale = Scale::from_percent(200).expect("200% is in range");
    let layout = BarLayout::compute(&config, 12, scale, 1, 0);

    assert_eq!(layout.bar, Rect::new(0, 1920, 4000, 80));
    assert_eq!(layout.corner_radius, 24);
    assert_eq!(layout.start_button, Rect::new(0, 1920, 96, 80));
    assert_eq!(layout.clock, Rect::new(3840, 1920, 160, 80));
    assert_eq!(layout.tasks[0], Rect::new(96, 1920, 320, 80));
}

#[test]
fn scale_one_is_identical_to_the_unscaled_layout() {
    let config = TaskbarConfig::bottom_bar(1000, 800);
    let scaled = BarLayout::compute(&config, 12, Scale::ONE, 2, 2);
    assert_eq!(scaled, bottom_layout(2, 2));
}

#[test]
fn supplied_scale_relays_the_bar_at_the_new_density() {
    // The bar stores no scale: the desktop density is supplied at layout time
    // by the compositor, so laying the same bar out at a
    // higher scale enlarges every region without any state change.
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(4000, 2000), &theme);
    let unscaled = bar.layout(Scale::ONE).start_button.width;

    let doubled = Scale::from_dpi(tairix_geometry::REFERENCE_DPI * 2).expect("192 DPI");
    assert_eq!(doubled.percent(), 200);
    assert_eq!(bar.layout(doubled).start_button.width, unscaled * 2);
    assert_eq!(
        bar.layout(doubled).corner_radius,
        theme.metrics().taskbar_corner_radius * 2
    );
}

// ---- taskbar (theming integration) ----------------------------------

#[test]
fn taskbar_takes_corner_radius_from_theme() {
    let dark = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &dark);
    assert_eq!(bar.corner_radius(), dark.metrics().taskbar_corner_radius);
    assert_eq!(
        bar.layout(Scale::ONE).corner_radius,
        dark.metrics().taskbar_corner_radius
    );
    assert_eq!(bar.start_menu().len(), 4);
}

#[test]
fn taskbar_layout_tracks_live_counts() {
    let theme = Theme::dark();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    assert_eq!(bar.layout(Scale::ONE).tasks.len(), 0);
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.notifications_mut().add(IconId(1), "icon.network");
    assert_eq!(bar.layout(Scale::ONE).tasks.len(), 1);
    assert_eq!(bar.layout(Scale::ONE).notifications.len(), 1);
    assert_eq!(
        bar.hit_test(Point::new(10, 770), Scale::ONE),
        Some(Hit::StartButton)
    );
}

#[test]
fn apply_theme_switches_corner_radius() {
    let mut metrics = *Theme::dark().metrics();
    metrics.taskbar_corner_radius = 0;
    let squared = Theme::new(
        tairix_theme::ThemeId(42),
        "Square",
        tairix_theme::Appearance::Dark,
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

// ---- rendering ------------------------------------------------------

/// The premultiplied pixel a theme palette role paints as.
fn role(color: tairix_theme::Rgba) -> Pixel {
    Color::from(color).premultiply()
}

/// The painted pixel at screen point `(x, y)`, translated into the bar's
/// local surface space.
fn pixel_at(surface: &Surface, bar: Rect, x: i32, y: i32) -> Pixel {
    let lx = u32::try_from(x - bar.left()).expect("point is right of the bar origin");
    let ly = u32::try_from(y - bar.top()).expect("point is below the bar origin");
    surface.get(lx, ly).expect("point lies inside the bar")
}

/// Whether any pixel inside screen-space `region` was painted `want`.
fn region_has_pixel(surface: &Surface, bar: Rect, region: Rect, want: Pixel) -> bool {
    (region.top()..region.bottom())
        .any(|y| (region.left()..region.right()).any(|x| pixel_at(surface, bar, x, y) == want))
}

#[test]
fn rendered_surface_matches_bar_dimensions() {
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert_eq!(surface.width(), layout.bar.width);
    assert_eq!(surface.height(), layout.bar.height);
}

#[test]
fn background_is_the_raised_surface_colour() {
    let theme = Theme::dark();
    let palette = theme.palette();
    // No tasks: a point in the middle of the empty task region is bare bar.
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert_eq!(
        pixel_at(&surface, bar.layout(Scale::ONE).bar, 500, 780),
        role(palette.surface_raised)
    );
}

#[test]
fn start_button_is_painted_with_the_accent() {
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert_eq!(
        pixel_at(&surface, bar.layout(Scale::ONE).bar, 24, 780),
        role(theme.palette().accent)
    );
}

#[test]
fn focused_task_is_accent_and_others_are_surface() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().add(TaskId(2), "Browser");
    bar.tasks_mut().activate(TaskId(1));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    // tasks[0] = (48,760,160,40); tasks[1] = (208,760,160,40). Sample near
    // each slot's trailing edge, clear of the leading-aligned title glyphs.
    assert_eq!(
        pixel_at(&surface, layout.bar, 190, 780),
        role(palette.accent)
    );
    assert_eq!(
        pixel_at(&surface, layout.bar, 350, 780),
        role(palette.surface)
    );
}

#[test]
fn minimised_task_recedes_into_the_background() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().activate(TaskId(1)); // focus
    bar.tasks_mut().activate(TaskId(1)); // click again -> minimise
    assert!(bar.tasks().is_minimised(TaskId(1)));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    // Sample near the slot's trailing edge, clear of the title glyphs.
    assert_eq!(
        pixel_at(&surface, layout.bar, 190, 780),
        role(palette.surface_raised)
    );
}

#[test]
fn notification_icon_draws_a_glyph_in_the_muted_role() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.notifications_mut().add(IconId(1), "icon.network");

    let layout = bar.layout(Scale::ONE);
    // With one icon: clock starts at 920, so the lone icon slot is
    // notifications[0] = (896,760,24,40).
    let slot = layout.notifications[0];
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    // The glyph paints muted pixels inside its slot ...
    assert!(
        region_has_pixel(&surface, layout.bar, slot, role(palette.on_surface_muted)),
        "the notification glyph draws on_surface_muted artwork in its slot"
    );
    // ... but is artwork, not a flood fill: the raised bar background still
    // shows through around the glyph.
    assert!(
        region_has_pixel(&surface, layout.bar, slot, role(palette.surface_raised)),
        "the glyph does not fill the whole slot; the bar background shows through"
    );
}

#[test]
fn unknown_notification_asset_falls_back_to_a_glyph() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.notifications_mut()
        .add(IconId(7), "icon.totally-unknown");

    let layout = bar.layout(Scale::ONE);
    let slot = layout.notifications[0];
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_has_pixel(&surface, layout.bar, slot, role(palette.on_surface_muted)),
        "an unknown asset draws the generic fallback glyph rather than nothing"
    );
}

#[test]
fn theme_switch_repaints_the_background() {
    let dark = Theme::dark();
    let light = Theme::light();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &dark);
    let layout = bar.layout(Scale::ONE);

    let mut renderer = TaskbarRenderer::new();
    let dark_surface = renderer
        .render(&bar, &dark, Scale::ONE)
        .expect("bar renders");
    let light_surface = renderer
        .render(&bar, &light, Scale::ONE)
        .expect("bar renders");
    assert_eq!(
        pixel_at(&dark_surface, layout.bar, 500, 780),
        role(dark.palette().surface_raised)
    );
    assert_eq!(
        pixel_at(&light_surface, layout.bar, 500, 780),
        role(light.palette().surface_raised)
    );
    assert_ne!(
        dark.palette().surface_raised,
        light.palette().surface_raised,
        "dark and light differ, so the repaint is observable"
    );
}

#[test]
fn renderer_reuses_cached_glyphs_across_frames_and_retints_on_theme_switch() {
    let dark = Theme::dark();
    let light = Theme::light();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &dark);
    bar.notifications_mut().add(IconId(1), "icon.network");
    let layout = bar.layout(Scale::ONE);
    let slot = layout.notifications[0];

    let mut renderer = TaskbarRenderer::new();
    // Two frames at the same theme and scale are pixel-identical: the cached
    // glyph is reused, not re-rasterised differently.
    let first = renderer
        .render(&bar, &dark, Scale::ONE)
        .expect("bar renders");
    let second = renderer
        .render(&bar, &dark, Scale::ONE)
        .expect("bar renders");
    assert_eq!(first.pixels(), second.pixels());

    // A theme switch re-tints the glyph: it now paints the light palette's
    // muted role and no longer the dark one inside its slot.
    let switched = renderer
        .render(&bar, &light, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_has_pixel(
            &switched,
            layout.bar,
            slot,
            role(light.palette().on_surface_muted)
        ),
        "after the switch the glyph draws the light muted role"
    );
    assert_ne!(
        dark.palette().on_surface_muted,
        light.palette().on_surface_muted,
        "dark and light muted roles differ, so the re-tint is observable"
    );
}

// ---- notification icon sets -----------------------------------------

/// An in-memory [`IconAssetSource`] mapping a kind to authored SVG bytes,
/// standing in for the on-disk `/System/Graphics` set in tests.
struct MemoryIcons {
    network: Option<&'static [u8]>,
}

impl tairix_icon::IconAssetSource for MemoryIcons {
    fn asset(&self, kind: tairix_icon::IconKind) -> Option<&[u8]> {
        match kind {
            tairix_icon::IconKind::Network => self.network,
            _ => None,
        }
    }
}

/// A square SVG glyph filled with a vivid green that matches no theme role,
/// so a loaded asset is distinguishable from any built-in tint.
const GREEN_SVG: &[u8] = br##"<svg viewBox="0 0 10 10">
  <polygon points="0,0 10,0 10,10 0,10" fill="#10ff20"/>
</svg>"##;

#[test]
fn loaded_icon_set_draws_its_authored_colours_not_the_theme_tint() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.notifications_mut().add(IconId(1), "icon.network");
    let layout = bar.layout(Scale::ONE);
    let slot = layout.notifications[0];

    let set = tairix_icon::IconSet::from_assets(&MemoryIcons {
        network: Some(GREEN_SVG),
    });
    assert!(
        set.is_loaded(tairix_icon::IconKind::Network),
        "the network asset decoded"
    );

    let mut renderer = TaskbarRenderer::new();
    renderer.set_icons(set);
    let surface = renderer
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");

    let green = Color::rgb(0x10, 0xff, 0x20).premultiply();
    assert!(
        region_has_pixel(&surface, layout.bar, slot, green),
        "the loaded SVG glyph keeps its authored colour"
    );
    assert!(
        !region_has_pixel(&surface, layout.bar, slot, role(palette.on_surface_muted)),
        "an authored asset is not re-tinted to the theme's muted role"
    );
}

#[test]
fn icon_kind_absent_from_a_loaded_set_falls_back_to_the_built_in_glyph() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    // The volume kind is not in the source, so it falls back to the tinted
    // built-in glyph even though the set loaded other kinds.
    bar.notifications_mut().add(IconId(1), "icon.volume");
    let layout = bar.layout(Scale::ONE);
    let slot = layout.notifications[0];

    let set = tairix_icon::IconSet::from_assets(&MemoryIcons {
        network: Some(GREEN_SVG),
    });
    assert!(
        !set.is_loaded(tairix_icon::IconKind::Volume),
        "the volume kind has no asset"
    );

    let mut renderer = TaskbarRenderer::new();
    renderer.set_icons(set);
    let surface = renderer
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_has_pixel(&surface, layout.bar, slot, role(palette.on_surface_muted)),
        "a kind absent from the loaded set keeps its tinted built-in glyph"
    );
}

#[test]
fn installing_a_set_invalidates_the_glyph_cache() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.notifications_mut().add(IconId(1), "icon.network");
    let layout = bar.layout(Scale::ONE);
    let slot = layout.notifications[0];

    let mut renderer = TaskbarRenderer::new();
    // First frame uses the built-in glyph: the muted tint shows, the authored
    // green cannot.
    let before = renderer
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    let green = Color::rgb(0x10, 0xff, 0x20).premultiply();
    assert!(
        region_has_pixel(&before, layout.bar, slot, role(palette.on_surface_muted)),
        "the built-in glyph paints the muted role first"
    );
    assert!(
        !region_has_pixel(&before, layout.bar, slot, green),
        "no authored colour before a set is installed"
    );

    // Installing a loaded set bumps the cache generation, so the next frame
    // re-rasterises from the new set rather than reusing the cached built-in.
    renderer.set_icons(tairix_icon::IconSet::from_assets(&MemoryIcons {
        network: Some(GREEN_SVG),
    }));
    let after = renderer
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_has_pixel(&after, layout.bar, slot, green),
        "after install the cache is invalidated and the loaded glyph draws"
    );
}

// ---- text rendering -------------------------------------------------

#[test]
fn clock_label_paints_foreground_text() {
    let theme = Theme::dark();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.clock_mut().set_label("12:00");

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert!(
        region_has_pixel(
            &surface,
            layout.bar,
            layout.clock,
            role(theme.palette().on_surface)
        ),
        "the clock label draws on_surface text inside the clock region"
    );
}

#[test]
fn empty_clock_paints_no_text() {
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    assert_eq!(bar.clock().label(), "");

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    assert!(
        !region_has_pixel(
            &surface,
            layout.bar,
            layout.clock,
            role(theme.palette().on_surface)
        ),
        "an empty clock label draws no foreground text"
    );
}

#[test]
fn focused_task_title_is_drawn_in_the_on_accent_role() {
    let theme = Theme::dark();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().activate(TaskId(1));

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    let slot = layout.tasks[0];
    assert!(
        region_has_pixel(&surface, layout.bar, slot, role(theme.palette().on_accent)),
        "the focused task title draws on_accent text over the accent slot"
    );
}

#[test]
fn task_title_too_long_for_its_slot_is_truncated_not_overflowing() {
    // A long title in a narrow slot must paint inside the slot and never
    // spill into the slot to its right (fail closed).
    let theme = Theme::dark();
    let config = TaskbarConfig {
        task_extent: 24,
        ..TaskbarConfig::bottom_bar(1000, 800)
    };
    let mut bar = Taskbar::new(config, &theme);
    bar.tasks_mut().add(TaskId(1), "A very long window title");
    bar.tasks_mut().add(TaskId(2), "Second");

    let layout = bar.layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render(&bar, &theme, Scale::ONE)
        .expect("bar renders");
    // The second slot is filled with the plain surface role; its title is
    // "Second" in on_surface. No on_surface text from task 1 may appear in
    // slot 2 before slot 2's own glyphs — assert slot 1's text stays put by
    // checking the gap is clean: the right edge column of slot 0 carries no
    // foreground from an overflow.
    let slot0 = layout.tasks[0];
    let overflow_probe = Rect::new(slot0.right(), slot0.top(), 1, slot0.height);
    assert!(
        !region_has_pixel(
            &surface,
            layout.bar,
            overflow_probe,
            role(theme.palette().on_surface)
        ),
        "task 0's title does not spill past its slot"
    );
}

// ---- input routing --------------------------------------------------

/// A 1000×800 bottom bar with the dark theme, the configuration every
/// hit-testing test above uses.
fn bottom_bar() -> Taskbar {
    Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark())
}

/// Move the pointer to `(x, y)` and press the primary button there.
fn press_at(input: &mut TaskbarInput, bar: &mut Taskbar, x: i32, y: i32) -> TaskbarResponse {
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(x, y),
        },
        bar,
        Scale::ONE,
    );
    input.handle(
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        },
        bar,
        Scale::ONE,
    )
}

#[test]
fn pressing_the_start_button_toggles_the_menu() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    assert!(!bar.start_menu().is_open());

    assert_eq!(
        press_at(&mut input, &mut bar, 10, 770),
        TaskbarResponse::StartMenuToggled { open: true }
    );
    assert!(bar.start_menu().is_open());

    assert_eq!(
        press_at(&mut input, &mut bar, 10, 770),
        TaskbarResponse::StartMenuToggled { open: false }
    );
    assert!(!bar.start_menu().is_open());
}

#[test]
fn pressing_a_task_slot_applies_the_activate_rule() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    bar.tasks_mut().add(TaskId(2), "Browser");
    let mut input = TaskbarInput::new();

    // Slot 0 spans x 48..208; press its middle.
    assert_eq!(
        press_at(&mut input, &mut bar, 100, 770),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Activated
        }
    );
    assert_eq!(bar.tasks().focused(), Some(TaskId(1)));

    // Pressing the focused task again minimises it.
    assert_eq!(
        press_at(&mut input, &mut bar, 100, 770),
        TaskbarResponse::TaskActivated {
            id: TaskId(1),
            outcome: ActivateOutcome::Minimised
        }
    );
    assert!(bar.tasks().is_minimised(TaskId(1)));
}

#[test]
fn pressing_a_notification_icon_reports_its_id() {
    let mut bar = bottom_bar();
    bar.notifications_mut().add(IconId(1), "icon.network");
    bar.notifications_mut().add(IconId(2), "icon.volume");
    let mut input = TaskbarInput::new();

    // With two icons, notifications[0] spans x 872..896.
    assert_eq!(
        press_at(&mut input, &mut bar, 880, 770),
        TaskbarResponse::NotificationActivated { id: IconId(1) }
    );
    // notifications[1] spans x 896..920.
    assert_eq!(
        press_at(&mut input, &mut bar, 900, 770),
        TaskbarResponse::NotificationActivated { id: IconId(2) }
    );
}

#[test]
fn pressing_the_clock_is_reported() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    assert_eq!(
        press_at(&mut input, &mut bar, 950, 770),
        TaskbarResponse::ClockPressed
    );
}

#[test]
fn a_press_that_misses_every_region_changes_nothing() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    // A gap in the (empty) task region, and a point above the bar entirely.
    assert_eq!(
        press_at(&mut input, &mut bar, 500, 770),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        press_at(&mut input, &mut bar, 500, 100),
        TaskbarResponse::Ignored
    );
    assert!(!bar.start_menu().is_open());
}

#[test]
fn non_primary_buttons_and_releases_are_ignored() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    input.handle(
        InputEvent::PointerMoved {
            to: Point::new(10, 770),
        },
        &mut bar,
        Scale::ONE,
    );
    // The secondary button over the start button does not toggle the menu.
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Secondary
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(
        input.handle(
            InputEvent::PointerReleased {
                button: PointerButton::Primary
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert!(!bar.start_menu().is_open());
}

#[test]
fn pointer_motion_tracks_position_without_acting() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    assert_eq!(
        input.handle(
            InputEvent::PointerMoved {
                to: Point::new(950, 770),
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::Ignored
    );
    assert_eq!(input.pointer(), Point::new(950, 770));
    // A press with no further motion acts at the tracked position (the clock).
    assert_eq!(
        input.handle(
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            },
            &mut bar,
            Scale::ONE,
        ),
        TaskbarResponse::ClockPressed
    );
}

// ---- start-menu popup geometry --------------------------------------

#[test]
fn menu_popup_opens_above_a_bottom_bar() {
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    let menu = bar.menu_layout(Scale::ONE);

    // The bar sits at y 760..800 with a 48-wide start button at x 0; the
    // four-entry popup (200×32 rows) opens upward from the bar's top edge.
    assert_eq!(menu.panel, Rect::new(0, 632, 200, 128));
    assert_eq!(menu.corner_radius, theme.metrics().popup_corner_radius);
    assert_eq!(menu.entries.len(), 4);
    assert_eq!(menu.entries[0], Rect::new(0, 632, 200, 32));
    assert_eq!(menu.entries[3], Rect::new(0, 728, 200, 32));
    // The rows stack contiguously and the panel exactly covers them.
    assert_eq!(menu.entries[3].bottom(), menu.panel.bottom());
}

#[test]
fn menu_popup_opens_outward_on_every_edge() {
    let theme = Theme::dark();
    let base = TaskbarConfig::bottom_bar(1000, 800);

    // Top bar: popup drops below the bar (bar height 40).
    let top = Taskbar::new(
        TaskbarConfig {
            edge: Edge::Top,
            ..base
        },
        &theme,
    );
    assert_eq!(
        top.menu_layout(Scale::ONE).panel,
        Rect::new(0, 40, 200, 128)
    );

    // Bottom bar: popup rises above the bar.
    let bottom = Taskbar::new(
        TaskbarConfig {
            edge: Edge::Bottom,
            ..base
        },
        &theme,
    );
    assert_eq!(
        bottom.menu_layout(Scale::ONE).panel,
        Rect::new(0, 632, 200, 128)
    );

    // Left bar (width 40): popup opens to the right, aligned to the top
    // start button.
    let left = Taskbar::new(
        TaskbarConfig {
            edge: Edge::Left,
            ..base
        },
        &theme,
    );
    assert_eq!(
        left.menu_layout(Scale::ONE).panel,
        Rect::new(40, 0, 200, 128)
    );

    // Right bar (bar at x 960): popup opens to the left of the bar.
    let right = Taskbar::new(
        TaskbarConfig {
            edge: Edge::Right,
            ..base
        },
        &theme,
    );
    assert_eq!(
        right.menu_layout(Scale::ONE).panel,
        Rect::new(760, 0, 200, 128)
    );
}

#[test]
fn menu_popup_scales_with_dpi() {
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(4000, 2000), &theme);
    let doubled = Scale::from_percent(200).expect("200% is in range");
    let menu = bar.menu_layout(doubled);

    // The logical 200×32 rows and the 6px popup radius all double at 200%.
    assert_eq!(menu.panel.width, 400);
    assert_eq!(menu.panel.height, 256);
    assert_eq!(menu.entries[0], Rect::new(0, 1664, 400, 64));
    assert_eq!(menu.corner_radius, theme.metrics().popup_corner_radius * 2);
}

#[test]
fn menu_popup_hit_test_resolves_entries() {
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &Theme::dark());
    let menu = bar.menu_layout(Scale::ONE);
    // Middle of the first and last rows.
    assert_eq!(menu.hit_test(Point::new(100, 648)), Some(0));
    assert_eq!(menu.hit_test(Point::new(100, 744)), Some(3));
    // Outside the panel entirely (on the bar, below the popup).
    assert_eq!(menu.hit_test(Point::new(100, 780)), None);
    // To the right of the 200-wide panel.
    assert_eq!(menu.hit_test(Point::new(300, 648)), None);
}

// ---- start-menu popup input routing ---------------------------------

#[test]
fn pressing_a_menu_entry_selects_it_and_closes_the_menu() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    // Open the menu via the start button.
    assert_eq!(
        press_at(&mut input, &mut bar, 10, 770),
        TaskbarResponse::StartMenuToggled { open: true }
    );

    // Press the first entry ("Log Out"); the panel is (0,632,200,128).
    assert_eq!(
        press_at(&mut input, &mut bar, 100, 648),
        TaskbarResponse::MenuEntrySelected {
            id: MenuEntryId(1),
            action: MenuAction::Session(SessionControl::LogOut),
        }
    );
    assert!(
        !bar.start_menu().is_open(),
        "selecting an entry closes the menu"
    );
}

#[test]
fn pressing_outside_the_open_menu_dismisses_it_without_acting() {
    let mut bar = bottom_bar();
    bar.tasks_mut().add(TaskId(1), "Editor");
    let mut input = TaskbarInput::new();
    press_at(&mut input, &mut bar, 10, 770); // open the menu

    // A press on a task slot while the menu is open dismisses the menu and
    // does *not* activate the task (one click does one thing).
    assert_eq!(
        press_at(&mut input, &mut bar, 100, 770),
        TaskbarResponse::StartMenuDismissed
    );
    assert!(!bar.start_menu().is_open());
    assert_eq!(
        bar.tasks().focused(),
        None,
        "the click did not reach the task"
    );
}

#[test]
fn pressing_a_launcher_entry_selects_it_and_closes_the_menu() {
    let mut bar = bottom_bar();
    let files = bar.start_menu_mut().add_launcher(LauncherId(42), "Files");
    let mut input = TaskbarInput::new();
    // Open the menu via the start button.
    assert_eq!(
        press_at(&mut input, &mut bar, 10, 770),
        TaskbarResponse::StartMenuToggled { open: true }
    );

    // With five entries the popup panel is (0,600,200,160); the launcher row
    // (entries[4]) spans y 728..760, so press its middle.
    assert_eq!(
        press_at(&mut input, &mut bar, 100, 744),
        TaskbarResponse::MenuEntrySelected {
            id: files,
            action: MenuAction::Launch(LauncherId(42)),
        }
    );
    assert!(
        !bar.start_menu().is_open(),
        "selecting a launcher closes the menu"
    );
}

#[test]
fn pressing_the_appearance_toggle_reports_it_and_closes_the_menu() {
    let mut bar = bottom_bar();
    let toggle = bar.start_menu_mut().add_appearance_toggle("Switch Theme");
    let mut input = TaskbarInput::new();
    // Open the menu via the start button.
    assert_eq!(
        press_at(&mut input, &mut bar, 10, 770),
        TaskbarResponse::StartMenuToggled { open: true }
    );

    // With five entries the popup panel is (0,600,200,160); the toggle row
    // (entries[4]) spans y 728..760, so press its middle.
    assert_eq!(
        press_at(&mut input, &mut bar, 100, 744),
        TaskbarResponse::MenuEntrySelected {
            id: toggle,
            action: MenuAction::ToggleAppearance,
        }
    );
    assert!(
        !bar.start_menu().is_open(),
        "selecting the toggle closes the menu"
    );
}

#[test]
fn pressing_the_start_button_while_open_toggles_it_shut() {
    let mut bar = bottom_bar();
    let mut input = TaskbarInput::new();
    press_at(&mut input, &mut bar, 10, 770); // open
    assert!(bar.start_menu().is_open());

    // The start button keeps its toggle behaviour even while the menu is open.
    assert_eq!(
        press_at(&mut input, &mut bar, 10, 770),
        TaskbarResponse::StartMenuToggled { open: false }
    );
    assert!(!bar.start_menu().is_open());
}

// ---- start-menu popup rendering -------------------------------------

#[test]
fn render_menu_is_none_when_the_menu_is_closed() {
    let theme = Theme::dark();
    let bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    assert!(!bar.start_menu().is_open());
    assert!(TaskbarRenderer::new()
        .render_menu(&bar, &theme, Scale::ONE)
        .is_none());
}

#[test]
fn render_menu_paints_the_panel_and_entry_labels() {
    let theme = Theme::dark();
    let palette = theme.palette();
    let mut bar = Taskbar::new(TaskbarConfig::bottom_bar(1000, 800), &theme);
    bar.start_menu_mut().toggle();

    let menu = bar.menu_layout(Scale::ONE);
    let surface = TaskbarRenderer::new()
        .render_menu(&bar, &theme, Scale::ONE)
        .expect("an open menu renders");
    assert_eq!(surface.width(), menu.panel.width);
    assert_eq!(surface.height(), menu.panel.height);

    // The panel background is the raised surface colour, and the first
    // entry's label ("Log Out") draws on_surface text inside its row.
    assert_eq!(
        pixel_at(&surface, menu.panel, 190, 700),
        role(palette.surface_raised)
    );
    assert!(
        region_has_pixel(
            &surface,
            menu.panel,
            menu.entries[0],
            role(palette.on_surface)
        ),
        "the first entry's label draws on_surface text in its row"
    );
}

#[test]
fn menu_popup_compute_is_fail_closed_for_an_empty_menu() {
    // A zero-entry popup has a zero-height panel and no rows; it never
    // panics and hit-tests to nothing.
    let layout = MenuLayout::compute(
        Edge::Bottom,
        Rect::new(0, 760, 1000, 40),
        Rect::new(0, 760, 48, 40),
        6,
        Scale::ONE,
        0,
    );
    assert!(layout.panel.is_empty());
    assert!(layout.entries.is_empty());
    assert_eq!(layout.hit_test(Point::new(0, 760)), None);
}
