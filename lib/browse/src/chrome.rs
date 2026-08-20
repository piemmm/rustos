//! The file-manager frame *model* (`plans/NEW-FILEMANAGER.md` `FM4b`): toolbar
//! command state, derived purely from the shared [`Browser`].
//!
//! The model is what a surface asks "is Back enabled here?", so a drawn
//! toolbar and a pointer hit-test answer from one place rather than each
//! re-deriving it. The window itself carries the location — there is no path
//! bar.
//!
//! * [`ToolbarModel`] reports, from a [`Browser`], whether each
//!   [`ToolbarCommand`] is currently actionable and which view and sort are
//!   active — the enable/disable and pressed state the drawn toolbar paints.
//!   A tool whose action cannot apply renders *disabled*, never hidden, so the
//!   toolbar's shape is stable.
//! * [`ContextMenuModel`] reports, from a [`Browser`] and the app's held
//!   clipboard state, whether each [`ContextCommand`] the right-click menu
//!   offers is currently actionable. Only commands the file manager can
//!   actually carry out today are modelled — Open
//!   ([`activate_selected`](crate::Browser::activate_selected)), Open With…
//!   (the [`open_with`](crate::open_with) chooser over a regular file), Pin to
//!   taskbar (the window channel's pin request over a bundle), Rename
//!   ([`rename_selected`](crate::Browser::rename_selected)), Cut/Copy
//!   ([`clipboard`](crate::Browser::clipboard)), Paste
//!   ([`plan_paste`](crate::clipboard::plan_paste)), Properties
//!   ([`Properties`](crate::properties::Properties)), and Delete
//!   ([`plan_delete`](crate::Browser::plan_delete)). New Folder is *not*
//!   modelled here: the drawn menu would have no verb to invoke for it yet, so
//!   it lands with the stage that first wires its behaviour, never as
//!   speculative surface.
//!
//! The model decides *what is offered*; it performs no navigation or I/O
//! itself, so composing it grants nothing (the read-only picker builds the
//! same model and simply never invokes a write action).

use tairix_icon::IconKind;

use crate::browser::Browser;
use crate::entry::{Entry, EntryKind};
use crate::error::BrowseError;
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

impl ToolbarCommand {
    /// The built-in glyph the drawn toolbar paints for this command.
    ///
    /// One definition so the toolbar's icon set and the icon vocabulary can
    /// never drift; adding a command adds its glyph here.
    #[must_use]
    pub const fn icon(self) -> IconKind {
        match self {
            Self::Back => IconKind::NavBack,
            Self::Forward => IconKind::NavForward,
            Self::Up => IconKind::NavUp,
            Self::Refresh => IconKind::Refresh,
            Self::ToggleView => IconKind::ViewToggle,
            Self::Sort => IconKind::Sort,
        }
    }
}

/// Apply the toolbar `command` to `browser`, returning whether the view
/// changed (and must be re-presented).
///
/// Every command is a read-only navigation or presentation action, so the
/// trusted read-only picker composes the same [`Browser`] and can invoke this
/// too — it grants no authority. Back/Forward/Up/Refresh are the browser's own
/// transactional, fail-closed navigation (a refused re-listing leaves the
/// browser exactly where it was); the view toggle and the sort cycle are pure
/// rearrangements that always change the view. The caller reveals the
/// selection and repaints when this reports a change.
///
/// # Errors
///
/// Returns [`BrowseError::Source`] when a navigation command's target
/// directory can no longer be listed; the browser is left untouched.
pub fn apply_command<S: DirectorySource>(
    browser: &mut Browser<S>,
    command: ToolbarCommand,
) -> Result<bool, BrowseError> {
    match command {
        ToolbarCommand::Back => browser.go_back(),
        ToolbarCommand::Forward => browser.go_forward(),
        ToolbarCommand::Up => browser.go_up(),
        ToolbarCommand::Refresh => browser.refresh().map(|()| true),
        ToolbarCommand::ToggleView => {
            browser.set_view_mode(browser.view_mode().toggled());
            Ok(true)
        }
        ToolbarCommand::Sort => {
            browser.set_sort_mode(browser.sort_mode().next());
            Ok(true)
        }
    }
}

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

/// A file-manager **write** tool — a manager-only toolbar action that mutates
/// the filesystem, deliberately kept in a vocabulary separate from the
/// read-only [`ToolbarCommand`] set.
///
/// The shared read-only toolbar ([`ToolbarCommand`] / [`apply_command`]) is
/// composed by *both* the file manager and the trusted read-only file picker,
/// so it must never carry an action that writes. Write tools live here instead:
/// only a write-capable consumer (the file manager) renders them — by handing
/// [`MANAGER_TOOLS`] to the renderer — and dispatches them in its own
/// capability-checked tail, under the user's own identity (no new capability;
/// the per-inode permission model gates the write). The picker hands the
/// renderer no write tools and therefore cannot express one: the separation is
/// enforced by the type system, not a runtime flag.
///
/// New write tools (Delete, the clipboard verbs) join this set only in the
/// stage that first wires their action, never ahead of it — exactly as the
/// [`ToolbarCommand`] set grows.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ManagerTool {
    /// Create a new folder in the current directory
    /// ([`create_directory`](Browser::create_directory)), which the file
    /// manager then opens the inline rename on.
    NewFolder,
    /// Go to the user's Trash — the navigable Trash location
    /// (`plans/NEW-FILEMANAGER.md` `FM11`). The file manager navigates the
    /// [`Browser`] to the `Library/Trash` subtree
    /// ([`navigate_to`](Browser::navigate_to)); it is a manager-only tool (not
    /// the picker's) because a trusted read-only pick has no business managing
    /// the Trash. Always offered — a Trash that cannot be reached is reported
    /// as a refusal, not hidden (§2.24).
    Trash,
    /// Empty the user's Trash — permanently remove its contents
    /// ([`empty_trash_plan`](crate::trash::empty_trash_plan) driven by the
    /// shared [`DeleteWalk`](crate::delete::DeleteWalk)). Offered only when the
    /// current directory *is* the user's Trash and it is non-empty (the
    /// [`ManagerToolModel`] the file manager builds); it renders disabled
    /// elsewhere, never hidden, so the toolbar's shape is stable.
    EmptyTrash,
}

/// The complete set of manager-only write tools, in their left-to-right toolbar
/// order (drawn after the shared read-only [`TOOLBAR_COMMANDS`]).
///
/// The file manager hands this to the renderer; the read-only picker hands an
/// empty slice, so it never draws or dispatches a write tool. New Folder first,
/// then the Trash location and the Empty Trash verb — grouped so the
/// Trash-related tools read together.
pub const MANAGER_TOOLS: &[ManagerTool] = &[
    ManagerTool::NewFolder,
    ManagerTool::Trash,
    ManagerTool::EmptyTrash,
];

impl ManagerTool {
    /// The built-in glyph the drawn toolbar paints for this tool (one
    /// definition, mirroring [`ToolbarCommand::icon`]).
    #[must_use]
    pub const fn icon(self) -> IconKind {
        match self {
            Self::NewFolder => IconKind::NewFolder,
            Self::Trash => IconKind::Trash,
            Self::EmptyTrash => IconKind::EmptyTrash,
        }
    }
}

/// The enable state of the manager-only write tools, since some depend on
/// context the [`Browser`] alone does not carry.
///
/// [`NewFolder`](ManagerTool::NewFolder) and [`Trash`](ManagerTool::Trash) are
/// always actionable, but [`EmptyTrash`](ManagerTool::EmptyTrash) is offered
/// only when the current directory *is* the user's Trash and it is non-empty —
/// a fact the file manager computes from the user's `HOME` (which the engine
/// does not know), so it is threaded in here rather than derived from the
/// browser. A disabled tool renders muted, never hidden, and a click on it
/// resolves to nothing (fail closed, §5.4).
///
/// The trusted read-only picker draws no write tools at all, so it uses
/// [`none`](Self::none); the model then never enables anything.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ManagerToolModel {
    can_empty_trash: bool,
}

impl ManagerToolModel {
    /// Build the model for a file manager whose current directory is (or is
    /// not) the user's non-empty Trash.
    ///
    /// `can_empty_trash` is the file manager's own computed answer to "is the
    /// current directory the user's Trash, and does it hold anything?" — the
    /// one gate on offering [`ManagerTool::EmptyTrash`].
    #[must_use]
    pub const fn new(can_empty_trash: bool) -> Self {
        Self { can_empty_trash }
    }

    /// The model for a consumer that draws no write tools (the trusted
    /// read-only picker): nothing is enabled because nothing is drawn.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            can_empty_trash: false,
        }
    }

    /// Whether `tool` is currently actionable and should render enabled.
    ///
    /// [`NewFolder`](ManagerTool::NewFolder) and [`Trash`](ManagerTool::Trash)
    /// are always available; [`EmptyTrash`](ManagerTool::EmptyTrash) is enabled
    /// only when the model was built for a non-empty Trash (the `can_empty_trash`
    /// argument to [`new`](Self::new)).
    #[must_use]
    pub const fn is_enabled(&self, tool: ManagerTool) -> bool {
        match tool {
            ManagerTool::NewFolder | ManagerTool::Trash => true,
            ManagerTool::EmptyTrash => self.can_empty_trash,
        }
    }
}

/// A file-manager context-menu command whose behaviour already exists in the
/// engine.
///
/// Each variant maps to an operation the drawn `lib/controls` `Menu` binds a
/// `MenuItem` (with a keyboard equivalent, where one exists) to. New verbs —
/// Delete, New Folder (`fs_mkdir`) — are added to this vocabulary only in the
/// stage that first wires their action, never ahead of it, exactly as the
/// [`ToolbarCommand`] set grows: a drawn command whose verb the file manager
/// cannot yet perform would be speculative surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ContextCommand {
    /// Activate the selected entry — descend, launch a bundle, or open a file
    /// ([`activate_selected`](Browser::activate_selected)).
    Open,
    /// Choose an application to open the selected regular file with, from the
    /// installed bundles whose declared associations claim the file's type
    /// ([`applications_for`](crate::open_with::applications_for)). Offered only
    /// for a regular file — a directory descends and a bundle launches itself,
    /// so neither has an application to choose.
    OpenWith,
    /// Rename the selected entry in place
    /// ([`rename_selected`](Browser::rename_selected)).
    Rename,
    /// Capture the selection onto a move clipboard
    /// ([`clipboard`](Browser::clipboard) with [`ClipboardOp::Cut`](crate::clipboard::ClipboardOp::Cut)).
    Cut,
    /// Capture the selection onto a copy clipboard
    /// ([`clipboard`](Browser::clipboard) with [`ClipboardOp::Copy`](crate::clipboard::ClipboardOp::Copy)).
    Copy,
    /// Paste the held clipboard into the current directory
    /// ([`plan_paste`](crate::clipboard::plan_paste)).
    Paste,
    /// Show the selected entry's properties
    /// ([`Properties`](crate::properties::Properties)).
    Properties,
    /// Delete the selected entry — the modal-confirmed recursive removal the
    /// app drives through [`plan_delete`](Browser::plan_delete) and the shared
    /// [`DeleteWalk`](crate::delete::DeleteWalk). Acts on the selection, so it
    /// is offered only when one exists.
    Delete,
}

/// The complete set of context-menu commands, in their top-to-bottom menu
/// order.
///
/// The drawn menu iterates this so a new command is one entry here rather than
/// a hand-maintained list duplicated per surface (mirrors
/// [`TOOLBAR_COMMANDS`]).
pub const CONTEXT_COMMANDS: &[ContextCommand] = &[
    ContextCommand::Open,
    ContextCommand::OpenWith,
    ContextCommand::Rename,
    ContextCommand::Cut,
    ContextCommand::Copy,
    ContextCommand::Paste,
    ContextCommand::Properties,
    ContextCommand::Delete,
];

impl ContextCommand {
    /// The label the drawn menu row shows for this command.
    ///
    /// One definition so the menu text has a single source.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::OpenWith => "Open With\u{2026}",
            Self::Rename => "Rename",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::Properties => "Properties",
            Self::Delete => "Delete",
        }
    }

    /// The keyboard-equivalent caption the drawn menu row shows for this
    /// command, matching the accelerator the app's navigation keys bind, so
    /// the menu advertises exactly the key that also drives the verb.
    #[must_use]
    pub const fn shortcut(self) -> &'static str {
        match self {
            Self::Open => "Enter",
            // Open With… is a pointer-only command (no keyboard accelerator
            // binds it), so it advertises none.
            Self::OpenWith => "",
            Self::Rename => "F2",
            Self::Cut => "Ctrl+X",
            Self::Copy => "Ctrl+C",
            Self::Paste => "Ctrl+V",
            Self::Properties => "Alt+Enter",
            Self::Delete => "Delete",
        }
    }
}

/// The enable state of the file-manager context menu, taken from a [`Browser`]
/// and the app's held clipboard.
///
/// A snapshot: the drawn menu rebuilds it when it is opened and paints each
/// [`ContextCommand`] enabled or disabled from [`is_enabled`](Self::is_enabled),
/// so an inapplicable command renders *disabled*, never hidden, and the menu's
/// shape stays stable. The model performs no action itself — it only reports
/// what is offered — so composing it grants nothing (the read-only picker
/// builds the same model and simply never invokes a write command).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ContextMenuModel {
    /// What kind of entry is selected, through the one shared [`EntryKind`]
    /// classifier — `None` when the directory is empty — so the kind-scoped
    /// rules (Open With… wants a file, Pin to taskbar wants a bundle) read
    /// the same classification every other surface does.
    selection: Option<EntryKind>,
    has_clipboard: bool,
}

impl ContextMenuModel {
    /// Build the context-menu state from `browser` and whether the app
    /// currently holds a cut/copy clipboard (`has_clipboard`).
    ///
    /// The clipboard lives in the app, not the browser
    /// ([`clipboard`](Browser::clipboard) *captures* a fresh one from the
    /// selection rather than storing one), so whether a paste is possible is
    /// the caller's own state, threaded in here.
    #[must_use]
    pub fn for_browser<S: DirectorySource>(browser: &Browser<S>, has_clipboard: bool) -> Self {
        Self {
            selection: browser.selected_entry().map(Entry::kind),
            has_clipboard,
        }
    }

    /// Whether `command` is currently actionable and should render enabled.
    ///
    /// [`Open`](ContextCommand::Open), [`Rename`](ContextCommand::Rename),
    /// [`Cut`](ContextCommand::Cut), [`Copy`](ContextCommand::Copy),
    /// [`Properties`](ContextCommand::Properties), and
    /// [`Delete`](ContextCommand::Delete) act on the selected entry, so
    /// they need a selection (an empty directory offers none).
    /// [`Paste`](ContextCommand::Paste) targets the current directory and needs
    /// only a held clipboard, not a selection.
    #[must_use]
    pub fn is_enabled(&self, command: ContextCommand) -> bool {
        match command {
            ContextCommand::Open
            | ContextCommand::Rename
            | ContextCommand::Cut
            | ContextCommand::Copy
            | ContextCommand::Properties
            | ContextCommand::Delete => self.selection.is_some(),
            // Open With… offers a chooser of applications, which only a regular
            // file has: a directory descends and a bundle launches itself, so
            // neither has an application to pick. A link offers the chooser
            // for what it *names*, because that is what opening it reaches.
            ContextCommand::OpenWith => {
                self.selection.and_then(EntryKind::resolved) == Some(EntryKind::File)
            }
            ContextCommand::Paste => self.has_clipboard,
        }
    }
}
