//! The file-manager frame *model* (`plans/NEW-FILEMANAGER.md` `FM4b`):
//! toolbar command state and breadcrumb path components, derived purely from
//! the shared [`Browser`].
//!
//! `FM4b`'s drawn chrome — the `lib/controls` toolbar, the breadcrumb path bar,
//! and the context menu — is deliberately staged to land *with* the actions
//! its surfaces invoke, so no widget is built ahead of the behaviour it calls.
//! This module is the pure part that can land now: the model behind the two
//! surfaces whose actions *already* exist in the engine (navigation history,
//! climb, refresh, the view toggle, and the sort), host-proven without a
//! kernel exactly as the [`Activation`](crate::activate) and
//! [`open_with`](crate::open_with) models were. The context menu is *not*
//! modelled here — its entries (Rename, Open, Cut/Copy/Paste/Delete, New
//! Folder) land with their own stages, never as speculative surface.
//!
//! * [`ToolbarModel`] reports, from a [`Browser`], whether each
//!   [`ToolbarCommand`] is currently actionable and which view and sort are
//!   active — the enable/disable and pressed state the drawn toolbar paints.
//!   A tool whose action cannot apply renders *disabled*, never hidden, so the
//!   toolbar's shape is stable.
//! * [`breadcrumbs`] turns the current directory's root-first components into
//!   the ordered [`Crumb`]s of the path bar, each carrying the ancestor depth
//!   the drawn crumb binds to
//!   [`navigate_to_depth`](crate::Browser::navigate_to_depth). The terminal
//!   crumb is the current directory (`is_current`), whose jump is a no-op.
//!
//! The model decides *what is offered* and *where a crumb leads*; it performs
//! no navigation or I/O itself, so composing it grants nothing (the read-only
//! picker builds the same model and simply never invokes a write action).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::browser::Browser;
use crate::layout::ViewMode;
use crate::sort::SortMode;
use crate::source::DirectorySource;

/// A file-manager toolbar command whose behaviour already exists in the engine.
///
/// Each variant maps to a `Browser` operation the drawn `lib/controls`
/// `Toolbar` binds an `IconButton` (with a keyboard equivalent) to. New tools
/// — New Folder (`fs_mkdir`), the clipboard verbs — are added to this
/// vocabulary only in the stage that first wires their action, never ahead of
/// it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ToolbarCommand {
    /// Return to the previous directory in the navigation history
    /// ([`go_back`](Browser::go_back)).
    Back,
    /// Advance to the next directory in the navigation history
    /// ([`go_forward`](Browser::go_forward)).
    Forward,
    /// Climb to the parent directory ([`go_up`](Browser::go_up)).
    Up,
    /// Re-read the current directory ([`refresh`](Browser::refresh)).
    Refresh,
    /// Toggle between the list and icon-grid item views
    /// ([`set_view_mode`](Browser::set_view_mode)).
    ToggleView,
    /// Change the listing sort order ([`set_sort_mode`](Browser::set_sort_mode)).
    Sort,
}

/// The complete set of toolbar commands, in their left-to-right toolbar order.
///
/// The drawn chrome iterates this so a new command is one entry here rather
/// than a hand-maintained list duplicated per surface.
pub const TOOLBAR_COMMANDS: &[ToolbarCommand] = &[
    ToolbarCommand::Back,
    ToolbarCommand::Forward,
    ToolbarCommand::Up,
    ToolbarCommand::Refresh,
    ToolbarCommand::ToggleView,
    ToolbarCommand::Sort,
];

/// The enable state and pressed state of the file-manager toolbar, taken from
/// a [`Browser`].
///
/// A snapshot: the drawn toolbar rebuilds it whenever the browser's state
/// changes and paints each [`ToolbarCommand`] enabled or disabled from
/// [`is_enabled`](Self::is_enabled), reflecting the active view and sort from
/// [`view_mode`](Self::view_mode) / [`sort_mode`](Self::sort_mode).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ToolbarModel {
    back: bool,
    forward: bool,
    up: bool,
    view_mode: ViewMode,
    sort_mode: SortMode,
}

impl ToolbarModel {
    /// Build the toolbar state from `browser`.
    ///
    /// Back/Forward reflect the navigation history
    /// ([`can_go_back`](Browser::can_go_back) /
    /// [`can_go_forward`](Browser::can_go_forward)); Up reflects whether there
    /// is a parent to climb to (`!`[`is_root`](Browser::is_root)); Refresh, the
    /// view toggle, and sort are always actionable.
    #[must_use]
    pub fn for_browser<S: DirectorySource>(browser: &Browser<S>) -> Self {
        Self {
            back: browser.can_go_back(),
            forward: browser.can_go_forward(),
            up: !browser.is_root(),
            view_mode: browser.view_mode(),
            sort_mode: browser.sort_mode(),
        }
    }

    /// Whether `command` is currently actionable and should render enabled.
    ///
    /// [`Refresh`](ToolbarCommand::Refresh),
    /// [`ToggleView`](ToolbarCommand::ToggleView), and
    /// [`Sort`](ToolbarCommand::Sort) are always available; the three
    /// navigation commands depend on the browser's history and depth.
    #[must_use]
    pub fn is_enabled(&self, command: ToolbarCommand) -> bool {
        match command {
            ToolbarCommand::Back => self.back,
            ToolbarCommand::Forward => self.forward,
            ToolbarCommand::Up => self.up,
            ToolbarCommand::Refresh | ToolbarCommand::ToggleView | ToolbarCommand::Sort => true,
        }
    }

    /// The item view currently shown — the pressed state of the view toggle.
    #[must_use]
    pub const fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// The listing sort order currently in force — what the sort control shows.
    #[must_use]
    pub const fn sort_mode(&self) -> SortMode {
        self.sort_mode
    }
}

/// One clickable component of the breadcrumb path bar.
///
/// [`label`](Self::label) is the text to draw; [`depth`](Self::depth) is the
/// ancestor depth the crumb binds to
/// [`navigate_to_depth`](Browser::navigate_to_depth) (`0` is the root view).
/// [`is_current`](Self::is_current) marks the terminal crumb — the directory
/// being shown — which the drawn bar renders inactive because jumping to it is
/// a no-op.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Crumb {
    label: String,
    depth: usize,
    is_current: bool,
}

impl Crumb {
    /// The text drawn for this crumb (the root marker or a path component).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The ancestor depth this crumb navigates to
    /// ([`navigate_to_depth`](Browser::navigate_to_depth)); `0` is the root.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Whether this crumb is the current directory (the terminal crumb), whose
    /// navigation is a no-op and which the bar renders inactive.
    #[must_use]
    pub const fn is_current(&self) -> bool {
        self.is_current
    }
}

/// The root breadcrumb's label — the storage-forest root view, spelled as the
/// browser spells the root path (`docs/src/filesystem/drives.md`: `/` is a
/// view, not the root of storage). A single definition so the crumb bar and
/// the path string agree on the root marker.
const ROOT_LABEL: &str = "/";

/// The breadcrumb path bar for `browser`: the root crumb followed by one crumb
/// per path component, root-first.
///
/// The root crumb is [`depth`](Crumb::depth) `0`; component `i` (root-first) is
/// depth `i + 1`; the terminal crumb (depth = the number of components) is the
/// current directory and is flagged [`is_current`](Crumb::is_current). Binding
/// each crumb to [`navigate_to_depth`](Browser::navigate_to_depth) climbs to
/// exactly the ancestor it names — and the current crumb's jump is the
/// documented no-op. At the root the result is the single root crumb, itself
/// current.
///
/// The components are whatever the source lists (the storage-forest view
/// bindings), never a fabricated POSIX tree.
#[must_use]
pub fn breadcrumbs<S: DirectorySource>(browser: &Browser<S>) -> Vec<Crumb> {
    let components = browser.components();
    let current_depth = components.len();
    let mut crumbs = Vec::with_capacity(current_depth + 1);
    crumbs.push(Crumb {
        label: ROOT_LABEL.to_string(),
        depth: 0,
        is_current: current_depth == 0,
    });
    for (index, component) in components.iter().enumerate() {
        let depth = index + 1;
        crumbs.push(Crumb {
            label: component.clone(),
            depth,
            is_current: depth == current_depth,
        });
    }
    crumbs
}
