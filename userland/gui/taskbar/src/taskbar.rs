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

use alloc::vec::Vec;

use tairix_controls::{ControlRole, IconButton, PointerState, TaskbarItem, TaskbarPresentation};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_proglib::EntryId;
use tairix_theme::{TextRole, Theme};

use crate::clock::Clock;
use crate::edge::Edge;
use crate::layout::{BarLayout, Hit};
use crate::library::{LibraryLayout, LibraryPopup};
use crate::menu::{BarMenu, MenuLayout, MenuSubject};
use crate::notifications::NotificationArea;
use crate::pins::{PinStrip, PinView};
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
    /// Main-axis length of each pinned-shortcut slot.
    pub pin_extent: u32,
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
            pin_extent: scale.scale_length(self.pin_extent),
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
            pin_extent: 48,
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
    pins: PinStrip,
    menu: BarMenu,
    tasks: TaskList,
    task_hover: Option<usize>,
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
            pins: PinStrip::new(),
            menu: BarMenu::new(),
            tasks: TaskList::new(),
            task_hover: None,
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

    /// The pin strip.
    #[must_use]
    pub const fn pins(&self) -> &PinStrip {
        &self.pins
    }

    /// Replace the pin strip's resolved views — how the session hands the
    /// bar its pins (and their running-window matches) whenever the store,
    /// the catalog, or the launch table changes.
    pub fn set_pins(&mut self, pins: Vec<PinView>) {
        self.pins.set_pins(pins);
        self.repaint = true;
    }

    /// The bar's context menu (open or closed).
    #[must_use]
    pub const fn menu(&self) -> &BarMenu {
        &self.menu
    }

    /// The bar's context menu, mutably (input routing only).
    pub(crate) fn menu_mut(&mut self) -> &mut BarMenu {
        &mut self.menu
    }

    /// The hovered task-slot index, if the pointer rests on one.
    #[must_use]
    pub const fn task_hover(&self) -> Option<usize> {
        self.task_hover
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

    /// Compute the bar's geometry for its current pin, task, and icon counts
    /// at the desktop `scale` (the compositor's output density).
    #[must_use]
    pub fn layout(&self, scale: Scale) -> BarLayout {
        BarLayout::compute(
            &self.config,
            self.theme.metrics().taskbar_corner_radius,
            scale,
            self.pins.len(),
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

    /// Compute the context menu's geometry, or `None` while it is closed.
    ///
    /// The row text is measured with the same body font the renderer paints
    /// with (one role, one scale), so the width the layout reserves and the
    /// width the paint fills can never disagree.
    #[must_use]
    pub fn menu_layout(&self, scale: Scale) -> Option<MenuLayout> {
        let font = BitmapFont::for_role(self.theme.fonts(), TextRole::Body, scale);
        self.menu.layout(
            self.config.edge,
            self.config.screen_width,
            self.config.screen_height,
            scale,
            &self.theme,
            font,
        )
    }

    /// The pixel side a pinned shortcut's icon paints at, at the desktop
    /// `scale` — the session rasterises per-application artwork at exactly
    /// this size, through the same control geometry the renderer paints
    /// with, so the artwork and the slot can never disagree.
    #[must_use]
    pub fn pin_icon_side(&self, scale: Scale) -> u32 {
        let scaled = self.config.scaled(scale);
        let bounds = Rect::new(0, 0, scaled.pin_extent.max(1), scaled.thickness.max(1));
        let font = BitmapFont::for_role(self.theme.fonts(), TextRole::Body, scale);
        TaskbarItem::new("", IconKind::AppBundle)
            .with_presentation(TaskbarPresentation::Icon)
            .icon_side(bounds, scale, &self.theme, font)
    }

    /// Open the context menu for the pin at `index`, anchored at its slot.
    pub(crate) fn open_pin_menu(&mut self, index: usize, anchor: Rect) {
        let running = self
            .pins
            .get(index)
            .and_then(PinView::window)
            .is_some_and(|id| self.tasks.entries().iter().any(|entry| entry.id == id));
        self.menu.open(MenuSubject::Pin { index, running }, anchor);
        self.repaint = true;
    }

    /// Open the context menu for a program-library entry row, anchored at
    /// that row.
    pub(crate) fn open_entry_menu(&mut self, entry: EntryId, anchor: Rect) {
        let pinned = self.pins.position_of_entry(&entry);
        self.menu.open(MenuSubject::Entry { entry, pinned }, anchor);
        self.repaint = true;
    }

    /// Close the context menu without acting.
    pub(crate) fn close_menu(&mut self) {
        self.menu.close();
        self.repaint = true;
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

    /// Track the pointer for the bar's hover feedback — the leading
    /// buttons, the pin slots, and the task slots — latching a repaint when
    /// any visual state changes.
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
        let pin_hover = layout.pins.iter().position(|slot| slot.contains(point));
        self.repaint |= self.pins.set_hover(pin_hover);
        let task_hover = layout.tasks.iter().position(|slot| slot.contains(point));
        if self.task_hover != task_hover {
            self.task_hover = task_hover;
            self.repaint = true;
        }
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
