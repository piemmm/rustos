//! The taskbar itself: its placement configuration and live state.
//!
//! [`Taskbar`] ties the two permanent leading launcher buttons (Library and
//! Files), the [`LibraryPopup`], the [`TaskList`], and the
//! [`NotificationArea`] to a [`TaskbarConfig`] (which screen edge, how thick,
//! and the per-region extents) and the active [`Theme`]. It produces a
//! [`BarLayout`] on demand from the current state and answers pointer hits
//! for input routing.
//!
//! The bar owns a copy of the active theme so its layout, hit-testing, and
//! painting all read one definition — the radius a hit-test assumes and the
//! radius the painter draws can never disagree. It also carries a repaint
//! latch: state changes that alter only pixels (a hover, a popup scroll) set
//! it, and the embedder drains it with [`take_repaint`](Taskbar::take_repaint)
//! to re-present exactly when something changed.

use tairix_controls::{ControlRole, IconButton, PointerState};
use tairix_geometry::{Point, Scale};
use tairix_icon::IconKind;
use tairix_theme::Theme;

use crate::clock::Clock;
use crate::edge::Edge;
use crate::layout::{BarLayout, Hit};
use crate::library::{LibraryLayout, LibraryPopup};
use crate::notifications::NotificationArea;
use crate::tasks::TaskList;

/// Where the taskbar sits and how big each region is.
///
/// The screen dimensions are *physical* pixels (the real framebuffer). The
/// extents and `thickness` are *logical* pixels authored at the reference
/// density (`tairix_geometry::REFERENCE_DPI`); the desktop's [`Scale`]
/// converts them to physical pixels at layout time, so the bar stays a
/// comfortable physical size across panel densities.
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
    /// Main-axis length of each leading launcher button (Library, Files).
    pub launcher_extent: u32,
    /// Main-axis length of each task slot.
    pub task_extent: u32,
    /// Main-axis length of each notification icon.
    pub icon_extent: u32,
    /// Main-axis length of the clock.
    pub clock_extent: u32,
}

impl TaskbarConfig {
    /// This configuration with every *logical* extent and the thickness
    /// converted to physical pixels at `scale`, leaving the physical screen
    /// dimensions and the edge untouched.
    ///
    /// [`BarLayout::compute`](crate::layout::BarLayout::compute) uses this so
    /// the logical→physical conversion is the one in
    /// [`Scale::scale_length`], never re-derived here.
    #[must_use]
    pub fn scaled(&self, scale: Scale) -> Self {
        Self {
            edge: self.edge,
            screen_width: self.screen_width,
            screen_height: self.screen_height,
            thickness: scale.scale_length(self.thickness),
            launcher_extent: scale.scale_length(self.launcher_extent),
            task_extent: scale.scale_length(self.task_extent),
            icon_extent: scale.scale_length(self.icon_extent),
            clock_extent: scale.scale_length(self.clock_extent),
        }
    }

    /// A conventional horizontal bottom bar for a `screen_width` ×
    /// `screen_height` screen, using the house-style extents.
    #[must_use]
    pub const fn bottom_bar(screen_width: u32, screen_height: u32) -> Self {
        Self {
            edge: Edge::Bottom,
            screen_width,
            screen_height,
            thickness: 40,
            launcher_extent: 48,
            task_extent: 160,
            icon_extent: 24,
            clock_extent: 80,
        }
    }
}

/// The taskbar: placement configuration plus the leading launcher buttons,
/// the program-library popup, the task list, and the notification area,
/// themed by the active [`Theme`].
///
/// The bar does **not** own a UI scale: the desktop density belongs to the
/// output, so the scale is supplied by the compositor at layout, hit-test,
/// and render time. A runtime DPI change is therefore
/// transparent to the taskbar model — the bar is simply laid out and
/// re-presented at the new density, with no state to update here.
#[derive(Clone, Debug)]
pub struct Taskbar {
    config: TaskbarConfig,
    theme: Theme,
    library_button: IconButton,
    files_button: IconButton,
    library: LibraryPopup,
    tasks: TaskList,
    notifications: NotificationArea,
    clock: Clock,
    repaint: bool,
}

impl Taskbar {
    /// Build a taskbar for `config`, adopting `theme` as its active theme.
    ///
    /// The two permanent leading buttons are seeded here: the Library button
    /// (accent-filled — the desktop's one primary invoker) and the quiet
    /// Files button. They are fixed: nothing can reorder or remove them.
    #[must_use]
    pub fn new(config: TaskbarConfig, theme: &Theme) -> Self {
        Self {
            config,
            theme: theme.clone(),
            library_button: IconButton::new(IconKind::Library, ControlRole::Primary),
            files_button: IconButton::new(IconKind::Folder, ControlRole::Neutral),
            library: LibraryPopup::new(),
            tasks: TaskList::new(),
            notifications: NotificationArea::new(),
            clock: Clock::new(),
            repaint: false,
        }
    }

    /// The placement configuration.
    #[must_use]
    pub const fn config(&self) -> &TaskbarConfig {
        &self.config
    }

    /// The active theme the bar lays out and paints with.
    #[must_use]
    pub const fn theme(&self) -> &Theme {
        &self.theme
    }

    /// The corner radius the window manager applies to the bar.
    #[must_use]
    pub fn corner_radius(&self) -> u32 {
        self.theme.metrics().taskbar_corner_radius
    }

    /// The Library launcher button, for painting.
    #[must_use]
    pub const fn library_button(&self) -> &IconButton {
        &self.library_button
    }

    /// The Files launcher button, for painting.
    #[must_use]
    pub const fn files_button(&self) -> &IconButton {
        &self.files_button
    }

    /// The program-library popup.
    #[must_use]
    pub const fn library(&self) -> &LibraryPopup {
        &self.library
    }

    /// The program-library popup, mutably — how the session hands it the
    /// resolved catalog ([`LibraryPopup::set_catalog`]).
    pub fn library_mut(&mut self) -> &mut LibraryPopup {
        &mut self.library
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

    /// Adopt a new theme. The rest of the taskbar's state is unchanged, so a
    /// runtime dark/light switch needs no relayout of the model.
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.repaint = true;
    }

    /// Reposition or resize the bar.
    pub fn set_config(&mut self, config: TaskbarConfig) {
        self.config = config;
    }

    /// Take the repaint latch: `true` when pixel-only state (a hover, a
    /// popup scroll or edit) changed since the last take, so the embedder
    /// re-presents exactly when needed. Reading it clears it.
    pub fn take_repaint(&mut self) -> bool {
        core::mem::take(&mut self.repaint)
    }

    /// Set the repaint latch (see [`take_repaint`](Self::take_repaint)).
    pub(crate) fn request_repaint(&mut self) {
        self.repaint = true;
    }

    /// Compute the bar's geometry for its current task and icon counts at the
    /// desktop `scale` (the compositor's output density).
    #[must_use]
    pub fn layout(&self, scale: Scale) -> BarLayout {
        BarLayout::compute(
            &self.config,
            self.theme.metrics().taskbar_corner_radius,
            scale,
            self.tasks.len(),
            self.notifications.len(),
        )
    }

    /// The taskbar element under `point` at the desktop `scale`, or `None` if
    /// the point misses every region.
    #[must_use]
    pub fn hit_test(&self, point: Point, scale: Scale) -> Option<Hit> {
        self.layout(scale).hit_test(point)
    }

    /// Compute the program-library popup's geometry for the current state.
    ///
    /// The popup opens outward from the Library button on the bar's edge;
    /// the window manager places and rounds it exactly as it does the bar.
    /// Meaningful only while [`LibraryPopup::is_open`]; the caller checks
    /// that.
    #[must_use]
    pub fn library_layout(&self, scale: Scale) -> LibraryLayout {
        let bar = self.layout(scale);
        self.library.layout(
            self.config.edge,
            &bar,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
        )
    }

    /// Open the program-library popup (fresh: search cleared, folders
    /// expanded, cursor at the top) and press the Library button in.
    pub(crate) fn open_library(&mut self) {
        self.library.open();
        self.library_button.set_state(
            self.library_button
                .state()
                .with_pointer(PointerState::Pressed),
        );
        self.repaint = true;
    }

    /// Close the program-library popup and release the Library button.
    pub(crate) fn close_library(&mut self) {
        self.library.close();
        self.library_button
            .set_state(self.library_button.state().with_pointer(PointerState::None));
        self.repaint = true;
    }

    /// Track the pointer for the leading buttons' hover feedback, latching a
    /// repaint when either button's visual state changes.
    ///
    /// While the popup is open the Library button stays visually pressed
    /// (it is "held open"); the Files button hovers as normal.
    pub(crate) fn track_hover(&mut self, point: Point, scale: Scale) {
        let layout = self.layout(scale);
        let library_pointer = if self.library.is_open() {
            PointerState::Pressed
        } else if layout.library.contains(point) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        let files_pointer = if layout.files.contains(point) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        self.repaint |= set_pointer(&mut self.library_button, library_pointer);
        self.repaint |= set_pointer(&mut self.files_button, files_pointer);
    }
}

/// Set `button`'s pointer state, reporting whether it changed.
fn set_pointer(button: &mut IconButton, pointer: PointerState) -> bool {
    let state = button.state();
    if state.pointer == pointer {
        return false;
    }
    button.set_state(state.with_pointer(pointer));
    true
}
