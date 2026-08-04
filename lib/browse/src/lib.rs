//! TAIRiX **shared directory-browser engine** (`plans/APPWIN.md` AW5).
//!
//! The one navigation **model** plus themed **renderer** every directory
//! browser in the system composes: the `files.app` bundle's windowed file
//! manager and the desktop session's **trusted file picker**
//! (`plans/CAPABILITY_USE.md` CU6) both drive exactly this engine, so the
//! two views can never diverge in navigation semantics, listing policy, or
//! look. Both are driven through an injected [`DirectorySource`]:
//!
//! * [`Browser`] holds the current directory's path and entries and a
//!   selection cursor. Descending, climbing to the parent, and refreshing
//!   are transactional and fail closed: the new directory is listed
//!   *before* any state changes, so a refused or failing read leaves the
//!   browser exactly where it was.
//! * [`render()`] paints the path bar and the (scrolling) entry list into
//!   a `lib/raster` [`Surface`](tairix_raster::Surface) using the active
//!   theme's palette and the shared `lib/font` face — the same surface the
//!   compositor places and rounds.
//!
//! # No `/proc`, no fabrication
//!
//! TAIRiX has no `/proc` and no `/sys`. The browser shows exactly the
//! entries its [`DirectorySource`] returns — it never injects a synthetic
//! entry — and it makes no permission decision of its own: the check and
//! the path policy live in the VFS behind the source, under the identity
//! of whichever process composes the engine (the files app's own, the
//! session's for the picker). A directory the caller may not read surfaces
//! a [`BrowseError`] rather than a partial or guessed listing.
//!
//! # Module map
//!
//! * [`entry`] — the [`Entry`]/[`EntryKind`] listing vocabulary.
//! * [`activate`](mod@activate) — the [`Activation`] dispatch-by-kind decision
//!   (descend / launch a bundle / open a file) the manager and picker share.
//! * [`click`](mod@click) — the [`DoubleClickTracker`] pure double-click
//!   detector: the one rule that turns primary presses into single/double
//!   clicks so a pointer double-click activates an item exactly as `Enter`
//!   does (`plans/NEW-FILEMANAGER.md` `FM12`).
//! * [`drag`](mod@drag) — the [`BundleDrag`] pure drag detector: the one rule
//!   that turns a pressed bundle row's motion into the window channel's
//!   drag-offer/withdraw verbs, exactly one offer per gesture
//!   (`plans/NEW-TASKBAR.md` T7).
//! * [`chrome`](mod@chrome) — the file-manager frame model: the [`ToolbarModel`]
//!   command enable/pressed state, the [`ContextMenuModel`] right-click command
//!   enable state, the [`breadcrumbs`] path bar
//!   (`plans/NEW-FILEMANAGER.md` `FM4b`), and the manager-only [`ManagerTool`]
//!   write-tool vocabulary (with the [`ManagerToolModel`] enable state) the
//!   read-only picker never composes.
//! * [`breadcrumb`](mod@breadcrumb) — the drawn path bar's placement: the one
//!   right-anchored crumb layout the painter and the pointer hit-test share.
//! * [`select`](mod@select) — the [`Selection`] multi-entry set (single /
//!   toggle / range / select-all) the management verbs act on.
//! * [`clipboard`](mod@clipboard) — the cut/copy [`Clipboard`] and
//!   [`plan_paste`] paste-target validation (`plans/NEW-FILEMANAGER.md` FM7).
//! * [`delete`](mod@delete) — the delete model: the [`DeletePlan`] naming what
//!   the Delete verb would remove, captured from the selection, and the
//!   [`DeleteWalk`] driven cursor that carries the recursive removal out
//!   depth-first, bounded and interruptible (`plans/NEW-FILEMANAGER.md` `FM7b`).
//! * [`execute`](mod@execute) — the pure paste-execution model: the
//!   [`paste_strategy`] move-vs-copy volume decision, the bounded, resumable
//!   [`CopyCursor`] streaming single-file copy, and the depth-first,
//!   depth-bounded [`CopyWalk`] recursive directory-copy cursor the management
//!   verbs run (`plans/NEW-FILEMANAGER.md` `FM7b`).
//! * [`media`](mod@media) — the one closed content-type [`MediaType`] registry
//!   the manager and picker share: it drives both the file-type
//!   [`IconKind`](tairix_icon::IconKind) glyph and the "Open With…" association
//!   vocabulary (a display hint, never authority), and names each type's
//!   broader type ([`MediaType::parent`]) so association matching can widen.
//! * [`open_with`](mod@open_with) — the type→bundle "Open With…" association
//!   model ([`applications_for`]) over the injected [`BundleSource`] seam,
//!   resolving a file's type through [`media`](mod@media) and matching along
//!   its subclass chain, most specific declaration first.
//! * [`sort`](mod@sort) — the [`SortMode`] and the one shared listing order.
//! * [`trash`](mod@trash) — the recoverable-delete model: the
//!   [`trash_strategy`] same-volume move-vs-unlink decision, the
//!   collision-safe [`trash_dest_path`] destination naming
//!   (`plans/NEW-FILEMANAGER.md` `FM10`), and the [`empty_trash_plan`]
//!   permanent empty-Trash model (`FM11`).
//! * [`error`] — [`BrowseError`], the fail-closed navigation outcomes.
//! * [`source`] — the [`DirectorySource`] seam.
//! * [`browser`] — the [`Browser`] navigation model.
//! * [`layout`](mod@layout) — the [`ListView`]/[`GridView`] item-view geometry
//!   and the [`ViewLayout`] dispatch (the one visible-window/item-rect/hit-test
//!   definition the renderer and the pointer hit-test share), plus [`ViewMode`],
//!   the [`GridFlow`] that also lays the desktop's trailing-edge icon column out
//!   of the very same grid, and the [`GridFill`] policy that decides what a line
//!   does with the space it has left over.
//! * [`format`](mod@format) — the size/date column formatting and the
//!   properties view's date-and-time spelling.
//! * [`properties`](mod@properties) — the [`Properties`] view model: the
//!   display-ready summary of a node's `fs_stat` metadata the file manager's
//!   Properties panel shows (`plans/NEW-FILEMANAGER.md` FM8).
//! * [`mkdir`](mod@mkdir) — the new-folder [`MkdirError`]/[`validate_new_dir_name`]
//!   model and the [`suggest_new_dir_name`] placeholder-name helper, committed
//!   through the `fs_mkdir` seam (`plans/NEW-FILEMANAGER.md` `FM7b`).
//! * [`mode_edit`](mod@mode_edit) — the [`ModeError`]/[`validate_mode`]
//!   permission-change model committed through the `fs_set_mode` seam
//!   (`plans/NEW-FILEMANAGER.md` `FM8b`).
//! * [`owner_edit`](mod@owner_edit) — the [`OwnerError`]/[`validate_owner`]
//!   ownership-change model ([`OwnerChange`]) committed through the privileged
//!   `fs_set_owner` seam (`plans/NEW-FILEMANAGER.md` `FM8b`).
//! * [`progress`](mod@progress) — the [`ProgressModel`] progress + latched-cancel
//!   state of a long delete/copy the file manager drives interleaved with its
//!   event loop (`plans/NEW-FILEMANAGER.md` `FM7b`).
//! * [`places`](mod@places) — the places/devices rail model: the user's fixed
//!   shortcuts plus the mounted volumes, each carrying the storage medium it
//!   really sits on, validated and ordered without touching the filesystem.
//! * [`rename`](mod@rename) — the in-place [`RenameError`]/[`validate_new_name`]
//!   rename model the file manager's first write operation is built on.
//! * [`render`](mod@render) — painting the current directory into a
//!   `Surface`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited
//! `lib/abi` ABI crate and the shared `lib/*` desktop libraries, so this
//! engine never links a kernel, driver, or window-manager crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod activate;
pub mod breadcrumb;
pub mod browser;
pub mod chrome;
pub mod click;
pub mod clipboard;
pub mod delete;
pub mod drag;
pub mod entry;
pub mod error;
pub mod execute;
pub mod format;
pub mod layout;
pub mod media;
pub mod mkdir;
pub mod mode_edit;
pub mod open_with;
pub mod owner_edit;
pub mod places;
pub mod progress;
pub mod properties;
pub mod rename;
pub mod render;
pub mod select;
pub mod sort;
pub mod source;
pub mod trash;
pub mod vfs;

pub use activate::Activation;
pub use browser::Browser;
pub use chrome::{
    apply_command, breadcrumbs, ContextCommand, ContextMenuModel, Crumb, ManagerTool,
    ManagerToolModel, ToolbarCommand, ToolbarModel, CONTEXT_COMMANDS, MANAGER_TOOLS,
    TOOLBAR_COMMANDS,
};
pub use click::{ClickKind, DoubleClickTracker, DOUBLE_CLICK_INTERVAL_NS};
pub use clipboard::{plan_paste, Clipboard, ClipboardOp, PasteError, PasteItem, PastePlan};
pub use delete::{
    DeleteAction, DeleteError, DeletePlan, DeleteTarget, DeleteWalk, MAX_DELETE_DEPTH,
};
pub use drag::{BundleDrag, DRAG_THRESHOLD_PX};
pub use entry::{is_bundle_name, Entry, EntryKind};
pub use error::BrowseError;
pub use execute::{
    paste_strategy, CopyAction, CopyChunk, CopyCursor, CopyError, CopyWalk, CopyWalkError,
    PasteStrategy, VolumeId, COPY_CHUNK_LEN, MAX_COPY_DEPTH,
};
pub use format::{format_date, format_datetime, format_size};
pub use layout::{
    GridFill, GridFlow, GridMetrics, GridView, ListView, SidebarView, ViewLayout, ViewMode,
};
pub use media::{entry_icon_request, media_for_entry, media_for_name, MediaType};
pub use mkdir::{suggest_new_dir_name, validate_new_dir_name, MkdirError, NEW_FOLDER_BASE};
pub use mode_edit::{validate_mode, ModeError};
pub use open_with::{applications_for, association_from_appinfo, AppAssociation, BundleSource};
pub use owner_edit::{validate_owner, OwnerChange, OwnerError};
pub use places::{Place, PlaceKind, Places, Volume, MAX_PLACE_LABEL, WIDEST_FIXED_LABEL};
pub use progress::{ProgressModel, ProgressOp};
pub use properties::Properties;
pub use rename::{validate_new_name, RenameError};
pub use render::{render, ManagerChrome};
pub use select::Selection;
pub use sort::{sort_entries, SortDirection, SortKey, SortMode};
pub use source::DirectorySource;
pub use trash::{
    empty_trash_plan, trash_dest_path, trash_dir, trash_strategy, DeleteDisposition, TrashError,
    TrashStrategy, MAX_TRASH_NAME_ATTEMPTS,
};
pub use vfs::VfsDirectorySource;

/// Window content width of a browser view, in pixels — the one definition
/// the files app's `Run` binary and the session's trusted picker size
/// their windows with, and the QEMU vertical's host-side scan-out
/// assertion measures against (`plans/APPWIN.md` AW3/AW5).
pub const WIN_WIDTH: u32 = 480;

/// Window content height of a browser view, in pixels (see
/// [`WIN_WIDTH`]).
///
/// Sized so the editable Properties popup (the metadata fields plus the
/// labelled owner/group/other × read/write/execute permissions grid) fits at
/// the default window size; the window is resizable, so a user may grow it
/// further.
pub const WIN_HEIGHT: u32 = 480;

/// The deepest directory nesting any of the file manager's recursive
/// component-path filesystem walks will descend, counted in root-first path
/// components.
///
/// A fixed fail-closed *bound*, not a hardware-scaled capacity (§24.4): it caps
/// how far a recursive removal ([`DeleteWalk`]) or a
/// recursive copy ([`CopyWalk`]) descends, so a pathological
/// or adversarial tree can never make the traversal recurse without limit
/// (§26.6). Both walks share this single definition rather than each carrying
/// their own copy of the value (§2.2); a tree deeper than the bound is refused
/// rather than followed. Chosen far beyond any legitimate directory depth while
/// staying comfortably bounded.
pub(crate) const MAX_WALK_DEPTH: usize = 256;

#[cfg(test)]
mod tests;
