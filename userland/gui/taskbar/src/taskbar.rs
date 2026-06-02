//! The taskbar itself: its placement configuration and live state.
//!
//! [`Taskbar`] ties the [`StartMenu`], the [`TaskList`], and the
//! [`NotificationArea`] to a [`TaskbarConfig`] (which screen edge, how thick,
//! and the per-region extents) and the active theme's corner radius. It
//! produces a [`BarLayout`] on demand from the current state and answers
//! pointer hits for input routing.

use rustos_geometry::Point;
use rustos_theme::Theme;

use crate::clock::Clock;
use crate::edge::Edge;
use crate::layout::{BarLayout, Hit};
use crate::menu::StartMenu;
use crate::notifications::NotificationArea;
use crate::tasks::TaskList;

/// Where the taskbar sits and how big each region is, in physical pixels.
///
/// Extents are measured along the bar's main axis (width for a horizontal
/// bar, height for a vertical one); `thickness` is the cross-axis size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TaskbarConfig {
    /// The screen edge the bar is pinned to.
    pub edge: Edge,
    /// Screen width in pixels.
    pub screen_width: u32,
    /// Screen height in pixels.
    pub screen_height: u32,
    /// Bar thickness (height for a horizontal bar, width for a vertical one).
    pub thickness: u32,
    /// Main-axis length of the start-menu button.
    pub start_button_extent: u32,
    /// Main-axis length of each task slot.
    pub task_extent: u32,
    /// Main-axis length of each notification icon.
    pub icon_extent: u32,
    /// Main-axis length of the clock.
    pub clock_extent: u32,
}

impl TaskbarConfig {
    /// A conventional horizontal bottom bar for a `screen_width` ×
    /// `screen_height` screen, using the house-style extents.
    #[must_use]
    pub const fn bottom_bar(screen_width: u32, screen_height: u32) -> Self {
        Self {
            edge: Edge::Bottom,
            screen_width,
            screen_height,
            thickness: 40,
            start_button_extent: 48,
            task_extent: 160,
            icon_extent: 24,
            clock_extent: 80,
        }
    }
}

/// The taskbar: placement configuration plus the start menu, task list, and
/// notification area, themed by a corner radius.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Taskbar {
    config: TaskbarConfig,
    corner_radius: u32,
    start_menu: StartMenu,
    tasks: TaskList,
    notifications: NotificationArea,
    clock: Clock,
}

impl Taskbar {
    /// Build a taskbar for `config`, taking its corner radius from `theme`
    /// and pre-populating the start menu with the session controls.
    #[must_use]
    pub fn new(config: TaskbarConfig, theme: &Theme) -> Self {
        Self {
            config,
            corner_radius: theme.metrics().taskbar_corner_radius,
            start_menu: StartMenu::with_session_controls(),
            tasks: TaskList::new(),
            notifications: NotificationArea::new(),
            clock: Clock::new(),
        }
    }

    /// The placement configuration.
    #[must_use]
    pub const fn config(&self) -> &TaskbarConfig {
        &self.config
    }

    /// The corner radius the window manager applies to the bar (`AGENTS.md`
    /// §2.2).
    #[must_use]
    pub const fn corner_radius(&self) -> u32 {
        self.corner_radius
    }

    /// The start menu.
    #[must_use]
    pub const fn start_menu(&self) -> &StartMenu {
        &self.start_menu
    }

    /// The start menu, mutably.
    pub fn start_menu_mut(&mut self) -> &mut StartMenu {
        &mut self.start_menu
    }

    /// The running-task list.
    #[must_use]
    pub const fn tasks(&self) -> &TaskList {
        &self.tasks
    }

    /// The running-task list, mutably.
    pub fn tasks_mut(&mut self) -> &mut TaskList {
        &mut self.tasks
    }

    /// The notification area.
    #[must_use]
    pub const fn notifications(&self) -> &NotificationArea {
        &self.notifications
    }

    /// The notification area, mutably.
    pub fn notifications_mut(&mut self) -> &mut NotificationArea {
        &mut self.notifications
    }

    /// The clock.
    #[must_use]
    pub const fn clock(&self) -> &Clock {
        &self.clock
    }

    /// The clock, mutably, so the caller can update its label.
    pub fn clock_mut(&mut self) -> &mut Clock {
        &mut self.clock
    }

    /// Adopt a new theme, updating the corner radius the window manager will
    /// apply. The rest of the taskbar's state is unchanged, so a runtime
    /// dark/light switch needs no relayout of the model (`AGENTS.md` §10).
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.corner_radius = theme.metrics().taskbar_corner_radius;
    }

    /// Reposition or resize the bar.
    pub fn set_config(&mut self, config: TaskbarConfig) {
        self.config = config;
    }

    /// Compute the bar's geometry for its current task and icon counts.
    #[must_use]
    pub fn layout(&self) -> BarLayout {
        BarLayout::compute(
            &self.config,
            self.corner_radius,
            self.tasks.len(),
            self.notifications.len(),
        )
    }

    /// The taskbar element under `point`, or `None` if the point misses
    /// every region.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<Hit> {
        self.layout().hit_test(point)
    }
}
