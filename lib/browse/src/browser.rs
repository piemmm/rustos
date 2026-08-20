//! The filesystem-browser navigation model.
//!
//! A [`Browser`] holds the directory it is currently showing — an absolute
//! path and the entries the [`DirectorySource`] returned for it — plus a
//! selection cursor for keyboard navigation. It descends into a child
//! directory, climbs back to the parent, and re-reads the current directory,
//! taking the path policy and the permission decision from the source's
//! VFS rather than re-implementing them here.
//!
//! Every move is **transactional and fail-closed**: the
//! browser computes the new path, asks the source to list it, and only adopts
//! the new path *and* its entries if that read succeeds. A refused or failing
//! read leaves the browser exactly where it was.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem;

use tairix_abi::Errno;

use crate::activate::Activation;
use crate::clipboard::{Clipboard, ClipboardOp};
use crate::delete::DeletePlan;
use crate::entry::{resolve_target, Entry, EntryKind, LinkTarget, Occupancy};
use crate::error::BrowseError;
use crate::layout::ViewMode;
use crate::mkdir::{validate_new_dir_name, MkdirError};
use crate::mode_edit::{validate_mode, ModeError};
use crate::owner_edit::{validate_owner, OwnerChange, OwnerError};
use crate::rename::{validate_new_name, RenameError};
use crate::select::Selection;
use crate::sort::{sort_entries, SortMode};
use crate::source::{DirectorySource, Listing};
use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::ScrollBar;

/// The most directories the back and forward navigation stacks each retain.
///
/// Navigation history is a UX convenience, not a hardware-scaled resource, so
/// this is a deliberate defensive cap rather than a discovered capacity: it
/// bounds the memory a long browsing session can accumulate from the user's
/// own back/forward moves. When the cap is reached the *oldest* location is
/// dropped, so history always retains the most recent moves and never grows
/// without bound. A generous limit keeps the ceiling well clear of any
/// realistic session, so it is never a surprising "tiny" cut-off.
pub(crate) const HISTORY_MAX: usize = 256;

/// A live view of one directory, with a selection cursor.
///
/// `S` is the injected [`DirectorySource`]; on a running system it is backed
/// by the VFS, and in tests by an in-memory tree.
#[derive(Clone, Debug)]
pub struct Browser<S: DirectorySource> {
    source: S,
    components: Vec<String>,
    entries: Vec<Entry>,
    selected: usize,
    selection: Selection,
    sort_mode: SortMode,
    view_mode: ViewMode,
    scroll_offset: u64,
    /// The interactive vertical scrollbar's transient state (thumb-drag
    /// capture, held end/track button, hover, focus) for the right-edge
    /// gutter. The authoritative offset stays [`scroll_offset`](Self::scroll_offset);
    /// the bar's model is re-synced from the live geometry each time it is
    /// painted or fed a pointer event ([`render`](mod@crate::render)), so this
    /// holds interaction state only, never a second copy of the offset.
    scrollbar: ScrollBar,
    /// Directories visited before the current one, oldest first; the last is
    /// where [`go_back`](Self::go_back) returns to.
    back: VecDeque<Vec<String>>,
    /// Directories stepped away from by [`go_back`](Self::go_back), oldest
    /// first; the last is where [`go_forward`](Self::go_forward) returns to.
    /// Cleared by any fresh navigation, as a browser's forward history is.
    forward: VecDeque<Vec<String>>,
    /// The navigation waiting on its listing, if any.
    ///
    /// Nothing else in this struct moves while it is set: the location, the
    /// entries, and both histories are still the ones on screen, so a listing
    /// that never arrives leaves the view exactly where it was and a refused one
    /// is reported in place. [`resume`](Self::resume) is what commits it.
    pending: Option<Pending>,
}

/// A navigation whose listing has not arrived yet: where it is going, and which
/// history move committing it owes.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Pending {
    target: Vec<String>,
    step: Step,
}

/// Which history move a [`Pending`] navigation commits.
///
/// The move is deliberately *not* applied when the navigation is started, so a
/// pending listing has changed nothing a caller can observe. Committing applies
/// exactly what the synchronous path applied, from one place, so the two cannot
/// diverge.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Step {
    /// A fresh navigation: record the departure, clear the forward history.
    Fresh,
    /// [`go_back`](Browser::go_back): pop the back history, push the departure
    /// forward.
    Back,
    /// [`go_forward`](Browser::go_forward): pop the forward history, push the
    /// departure back.
    Forward,
    /// A re-read of the directory already shown: no history change.
    Reload,
}

impl<S: DirectorySource> Browser<S> {
    /// Open the browser at the filesystem root (`/`), listing its children.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the root directory cannot be listed.
    pub fn open_root(source: S) -> Result<Self, BrowseError> {
        Self::open_at(source, Vec::new())
    }

    /// Open the browser at the directory named by root-first `components`,
    /// listing its children (an empty slice is the root `/`, so
    /// [`open_root`](Self::open_root) is exactly `open_at(source, [])`).
    ///
    /// The browser starts *at* that directory: [`components`](Self::components)
    /// is the given path and [`go_up`](Self::go_up) climbs toward the root from there, with
    /// an empty back/forward history exactly as a fresh open has. This is how a
    /// consumer opens where the user expects to start — the trusted file picker
    /// opens at the user's home rather than dumping them at the storage-forest
    /// root — without a second navigation model (§2.2): the same listing, sort,
    /// and selection path `open_root` uses.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if that directory cannot be listed
    /// (an unreadable, missing, or malformed path) — so a caller can fall back
    /// to a directory it can list (e.g. the root) rather than open nothing.
    pub fn open_at(mut source: S, components: Vec<String>) -> Result<Self, BrowseError> {
        let sort_mode = SortMode::default_order();
        // A source that reads elsewhere opens empty and listing: the browser is
        // usable (its location is known, its chrome draws) and the entries
        // arrive with the first `resume`.
        let listed = source.list(&components).map_err(BrowseError::Source)?;
        let pending = match listed {
            Listing::Ready(_) => None,
            Listing::Pending => Some(Pending {
                target: components.clone(),
                step: Step::Reload,
            }),
        };
        let mut entries = match listed {
            Listing::Ready(entries) => entries,
            Listing::Pending => Vec::new(),
        };
        sort_entries(&mut entries, sort_mode);
        let mut selection = Selection::new();
        if !entries.is_empty() {
            selection.single(0);
        }
        Ok(Self {
            source,
            components,
            entries,
            selected: 0,
            selection,
            sort_mode,
            view_mode: ViewMode::default(),
            scroll_offset: 0,
            scrollbar: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::new(0, 0, 0), 1, 1),
            ),
            back: VecDeque::new(),
            forward: VecDeque::new(),
            pending,
        })
    }

    /// Which item view the browser is showing.
    #[must_use]
    pub const fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Switch the item view between list and grid.
    ///
    /// A pure toggle: the selection stays on the same entry and the listing is
    /// untouched. The scroll offset resets to the top because its unit differs
    /// between the two views (list rows vs. grid rows); a caller reveals the
    /// selection again through [`reveal_selection`](crate::render::reveal_selection).
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        if mode != self.view_mode {
            self.view_mode = mode;
            self.scroll_offset = 0;
        }
    }

    /// The desired first-visible line (list rows or grid rows, depending on
    /// the [`view_mode`](Self::view_mode)). It is clamped against the live
    /// geometry when the view is painted or hit-tested, so it is only ever a
    /// *request* the layout normalises — never an out-of-range value.
    #[must_use]
    pub const fn scroll_offset(&self) -> u64 {
        self.scroll_offset
    }

    /// The interactive scrollbar's transient state, for the renderer to draw
    /// its live hover/drag treatment.
    #[must_use]
    pub const fn scrollbar(&self) -> &ScrollBar {
        &self.scrollbar
    }

    /// Mutable access to the interactive scrollbar, for the pointer-routing
    /// helper ([`render::scroll_pointer`](crate::render::scroll_pointer)) to
    /// feed it events. The offset it requests is applied through
    /// [`set_scroll_offset`](Self::set_scroll_offset), so the browser stays the
    /// one owner of the authoritative offset.
    #[must_use]
    pub fn scrollbar_mut(&mut self) -> &mut ScrollBar {
        &mut self.scrollbar
    }

    /// Set the desired first-visible line. The value is stored verbatim and
    /// clamped lazily by the layout; callers that know the geometry use the
    /// [`render`](mod@crate::render) scroll helpers instead of poking this raw.
    pub fn set_scroll_offset(&mut self, offset: u64) {
        self.scroll_offset = offset;
    }

    /// The order the current listing is shown in.
    #[must_use]
    pub const fn sort_mode(&self) -> SortMode {
        self.sort_mode
    }

    /// Re-order the current listing by `mode`, keeping the selection on the
    /// same entry where it still exists and clamping it otherwise.
    ///
    /// A no-op when `mode` is already in effect. The re-order is a pure
    /// rearrangement of the entries already loaded — it never re-reads the
    /// directory, so it cannot fail or change *which* entries are shown, only
    /// their order (the picker and the manager stay one shared order).
    pub fn set_sort_mode(&mut self, mode: SortMode) {
        if mode == self.sort_mode {
            return;
        }
        self.sort_mode = mode;
        let anchor = self.selected_entry().cloned();
        sort_entries(&mut self.entries, mode);
        match anchor.and_then(|anchor| self.entries.iter().position(|e| *e == anchor)) {
            Some(index) => self.selected = index,
            None => self.clamp_selection(),
        }
        // The selection is index-based, so a reorder invalidates any
        // multi-selection; collapse it to the (preserved) focused entry.
        self.reset_selection_to_focus();
    }

    /// The current directory's path components, root-first. Empty at the root.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// The current directory as an absolute path string (`/`, `/System`,
    /// `/System/Fonts`, …).
    #[must_use]
    pub fn path(&self) -> String {
        crate::vfs::spell_absolute_path(&self.components)
    }

    /// `true` if the browser is showing the filesystem root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    /// The entries of the current directory, in the source's order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Learn whether each plain directory in `range` holds anything, so the
    /// renderer can draw the empty/non-empty folder cue.
    ///
    /// Occupancy costs a directory read per child, so the caller decides what
    /// it is worth: pass the indices actually on screen
    /// ([`render::visible_range`](crate::render::visible_range)) and the cost
    /// is bounded by the window, not by the listing — a hundred-thousand-entry
    /// directory probes only the rows it draws. There is no hidden sweep and
    /// no built-in budget.
    ///
    /// Only an entry that [needs one](Entry::needs_occupancy_probe) is probed:
    /// a file has no children, a bundle is a sealed unit drawing its own icon,
    /// and an already-answered entry is never asked twice — including one
    /// whose probe was refused, which stays
    /// [`Indeterminate`](Occupancy::Indeterminate) rather than becoming a
    /// per-frame syscall. Indices past the end of the listing are ignored. A
    /// fresh listing resets every answer, so a reload re-probes.
    pub fn resolve_occupancy(&mut self, range: core::ops::Range<usize>) {
        let end = range.end.min(self.entries.len());
        for index in range.start..end {
            let Some(entry) = self.entries.get(index) else {
                continue;
            };
            if !entry.needs_occupancy_probe() {
                continue;
            }
            self.components.push(String::from(entry.name()));
            let answer = self.source.has_children(&self.components);
            self.components.pop();
            let occupancy = match answer {
                Ok(true) => Occupancy::NonEmpty,
                Ok(false) => Occupancy::Empty,
                Err(_) => Occupancy::Indeterminate,
            };
            if let Some(entry) = self.entries.get_mut(index) {
                entry.set_occupancy(occupancy);
            }
        }
    }

    /// The index of the selected entry, or `None` when the directory is empty.
    #[must_use]
    pub fn selected_index(&self) -> Option<usize> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// The selected entry, or `None` when the directory is empty.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&Entry> {
        self.selected_index().map(|i| &self.entries[i])
    }

    /// The selected entry's name, or `None` when the directory is empty — the
    /// name the in-place rename editor starts from.
    #[must_use]
    pub fn selected_name(&self) -> Option<&str> {
        self.selected_entry().map(Entry::name)
    }

    /// Spell the validated absolute path of the selected entry — the node a
    /// read-only `fs_stat` (the Properties view) or an open acts on — or
    /// `None` when the directory is empty.
    ///
    /// Uses the one shared path spelling ([`crate::vfs::absolute_path`]), so
    /// the stat/open can
    /// never name a different node than the browser shows; a name that cannot
    /// be spelled as a valid, bounded absolute path is a fail-closed
    /// [`BrowseError::Source`]. The engine only *names* the target — reading
    /// its metadata stays in the caller's own capability-checked tail under
    /// the user's identity, so composing this grants nothing and the
    /// read-only picker builds the same path.
    #[must_use]
    pub fn selected_target_path(&self) -> Option<Result<String, BrowseError>> {
        let name = String::from(self.selected_name()?);
        Some(self.child_target_path(&name))
    }

    /// Move the selection to `index`.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::NoSuchEntry`] if `index` is out of range; the
    /// selection is unchanged.
    pub fn select(&mut self, index: usize) -> Result<(), BrowseError> {
        if index >= self.entries.len() {
            return Err(BrowseError::NoSuchEntry);
        }
        self.selected = index;
        self.selection.single(index);
        Ok(())
    }

    /// Move the focus to the next entry, stopping at the last, and select it
    /// alone (an unmodified keyboard move collapses any multi-selection). A
    /// no-op on an empty directory.
    pub fn select_next(&mut self) {
        if let Some(last) = self.entries.len().checked_sub(1) {
            self.selected = self.selected.saturating_add(1).min(last);
            self.selection.single(self.selected);
        }
    }

    /// Move the focus to the previous entry, stopping at the first, and select
    /// it alone. A no-op on an empty directory.
    pub fn select_previous(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.selection.single(self.selected);
    }

    /// The set of entries currently selected in this listing — the members the
    /// management verbs (cut / copy / delete) act on. A superset of the single
    /// focused entry only while a multi-selection is in force; a fresh listing
    /// collapses it back to the focus.
    #[must_use]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Whether the entry at `index` is in the current [`selection`](Self::selection).
    #[must_use]
    pub fn is_selected(&self, index: usize) -> bool {
        self.selection.contains(index)
    }

    /// Toggle the entry at `index` in the selection (a `Ctrl`-click) and move
    /// the focus to it.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::NoSuchEntry`] if `index` is out of range; the
    /// selection and focus are unchanged.
    pub fn toggle_selection(&mut self, index: usize) -> Result<(), BrowseError> {
        if index >= self.entries.len() {
            return Err(BrowseError::NoSuchEntry);
        }
        self.selected = index;
        self.selection.toggle(index);
        Ok(())
    }

    /// Extend the selection to the contiguous range between its anchor and
    /// `index` (a `Shift`-click) and move the focus to `index`.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::NoSuchEntry`] if `index` is out of range; the
    /// selection and focus are unchanged.
    pub fn extend_selection_to(&mut self, index: usize) -> Result<(), BrowseError> {
        if index >= self.entries.len() {
            return Err(BrowseError::NoSuchEntry);
        }
        self.selected = index;
        self.selection.range_to(index);
        Ok(())
    }

    /// Select every entry in the current listing (Select All). The focus is
    /// left where it was; an empty directory stays with an empty selection.
    pub fn select_all(&mut self) {
        self.selection.select_all(self.entries.len());
    }

    /// Drop the whole selection (leaving the focus cursor where it is).
    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// The absolute root-first component paths of the currently selected
    /// entries, in listing order — the source paths a [`clipboard`](Self::clipboard)
    /// captures for a move or copy.
    #[must_use]
    pub fn selected_component_paths(&self) -> Vec<Vec<String>> {
        self.selection
            .iter()
            .filter_map(|index| self.entries.get(index))
            .map(|entry| {
                let mut path = self.components.clone();
                path.push(String::from(entry.name()));
                path
            })
            .collect()
    }

    /// Capture the current selection onto a cut/copy [`Clipboard`] for `op`, or
    /// `None` when nothing is selected.
    ///
    /// The clipboard holds the selected entries' absolute paths, so it stays
    /// valid after the user navigates elsewhere to paste. Building it grants no
    /// authority — the move/copy the app later performs is the user's own
    /// capability-checked filesystem operation.
    #[must_use]
    pub fn clipboard(&self, op: ClipboardOp) -> Option<Clipboard> {
        Clipboard::new(op, self.selected_component_paths())
    }

    /// Capture the current selection into a [`DeletePlan`] naming what a Delete
    /// would remove, or `None` when nothing is selected.
    ///
    /// Each target carries its absolute path (so it names exactly the node the
    /// browser shows) and whether it is directory-backed on disk (so the app
    /// removes it with [`UnlinkFlags::DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY)
    /// and recurses, rather than unlinking a leaf file). Building the plan
    /// grants no authority — the `fs_unlink` the app later performs is the
    /// user's own capability-checked filesystem operation, so the read-only
    /// picker composes the same [`Browser`] and never builds one.
    #[must_use]
    pub fn plan_delete(&self) -> Option<DeletePlan> {
        let targets = self
            .selection
            .iter()
            .filter_map(|index| self.entries.get(index))
            .map(|entry| {
                let mut path = self.components.clone();
                path.push(String::from(entry.name()));
                (path, entry.is_directory_backed())
            })
            .collect();
        DeletePlan::new(targets)
    }

    /// Re-read the current directory from the source, preserving the selection
    /// where it still points at an entry and clamping it otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the directory can no longer be
    /// listed; the previously loaded entries are left untouched.
    pub fn refresh(&mut self) -> Result<(), BrowseError> {
        let target = self.components.clone();
        self.begin(target, Step::Reload)?;
        Ok(())
    }

    /// Rename the selected entry to `new_name`, applying the change through the
    /// injected `rename` seam and re-reading the directory on success.
    ///
    /// `rename` receives the absolute source and destination paths and
    /// performs the capability-checked `fs_rename` under the caller's own
    /// identity — the engine adds no authority of its own (the trusted picker
    /// composes the same [`Browser`] and never calls this). The seam returns
    /// the kernel boundary's [`Errno`] on refusal.
    ///
    /// Transactional and fail closed: the name is validated
    /// ([`validate_new_name`]) *before* any syscall, and a VFS refusal leaves
    /// the listing exactly as it was. On success the directory is re-listed
    /// and the selection follows the entry to its new name; a rename that
    /// equals the current name is a no-op ([`RenameError::Unchanged`]) that
    /// touches neither the VFS nor the view.
    ///
    /// # Errors
    ///
    /// A [`RenameError`]: a spelling/clash/unchanged failure decided before the
    /// syscall, [`RenameError::Refused`] when the VFS refuses the move, or
    /// [`RenameError::Source`] when the post-rename re-list fails.
    pub fn rename_selected<R>(&mut self, new_name: &str, rename: R) -> Result<(), RenameError>
    where
        R: FnOnce(&str, &str) -> Result<(), Errno>,
    {
        let current = self
            .selected_name()
            .ok_or(RenameError::NoSelection)
            .map(String::from)?;
        validate_new_name(new_name, &current, &self.entries)?;

        let from = self.child_path(&current)?;
        let to = self.child_path(new_name)?;
        rename(&from, &to).map_err(RenameError::Refused)?;

        self.refresh()
            .map_err(|err| RenameError::Source(err.source_errno().unwrap_or(Errno::NotFound)))?;
        if let Some(index) = self.entries.iter().position(|e| e.name() == new_name) {
            self.selected = index;
        }
        Ok(())
    }

    /// Spell the validated absolute path of a child named `name` in the
    /// current directory — the one child-path spelling every write verb
    /// (rename, create, launch/open) shares, so a verb can never name a
    /// different node than the browser shows (§2.2). Surfaces the kernel's own
    /// [`Errno`] on a spelling failure for each caller to map onto its own
    /// error type.
    fn spell_child(&self, name: &str) -> Result<String, Errno> {
        let mut components = self.components.clone();
        components.push(String::from(name));
        crate::vfs::absolute_path(&components)
    }

    /// Spell the absolute path of a child named `name` in the current
    /// directory, mapping a spelling failure onto the matching
    /// [`RenameError`]. A `name` that already passed [`validate_new_name`] can
    /// only fail here if the *whole* path exceeds the kernel's limit.
    fn child_path(&self, name: &str) -> Result<String, RenameError> {
        self.spell_child(name).map_err(|errno| match errno {
            Errno::LengthOutOfRange => RenameError::TooLong,
            _ => RenameError::Invalid,
        })
    }

    /// Create a new folder named `name` in the current directory, applying the
    /// create through the injected `mkdir` seam and re-reading the directory on
    /// success.
    ///
    /// `mkdir` receives the new folder's absolute path and performs the
    /// capability-checked `fs_mkdir` under the caller's own identity — the
    /// engine adds no authority of its own (the trusted picker composes the
    /// same [`Browser`] and never calls this). The seam returns the kernel
    /// boundary's [`Errno`] on refusal.
    ///
    /// Transactional and fail closed: the name is validated
    /// ([`validate_new_dir_name`]) *before* any syscall, and a VFS refusal
    /// leaves the listing exactly as it was. On success the directory is
    /// re-listed and the selection follows onto the new folder, ready for the
    /// inline rename the app opens on it.
    ///
    /// # Errors
    ///
    /// A [`MkdirError`]: a spelling/clash failure decided before the syscall,
    /// [`MkdirError::Refused`] when the VFS refuses the create, or
    /// [`MkdirError::Source`] when the post-create re-list fails.
    pub fn create_directory<M>(&mut self, name: &str, mkdir: M) -> Result<(), MkdirError>
    where
        M: FnOnce(&str) -> Result<(), Errno>,
    {
        validate_new_dir_name(name, &self.entries)?;

        let path = self.spell_child(name).map_err(|errno| match errno {
            Errno::LengthOutOfRange => MkdirError::TooLong,
            _ => MkdirError::Invalid,
        })?;
        mkdir(&path).map_err(MkdirError::Refused)?;

        self.refresh()
            .map_err(|err| MkdirError::Source(err.source_errno().unwrap_or(Errno::NotFound)))?;
        if let Some(index) = self.entries.iter().position(|e| e.name() == name) {
            self.selected = index;
        }
        Ok(())
    }

    /// Change the selected node's permission mode to `mode`, applying the
    /// change through the injected `set_mode` seam.
    ///
    /// `set_mode` receives the node's absolute path and the new mode and
    /// performs the capability-checked `fs_set_mode` under the caller's own
    /// identity — the engine adds no authority of its own (the trusted picker
    /// composes the same [`Browser`] and never calls this). The seam returns
    /// the kernel boundary's [`Errno`] on refusal.
    ///
    /// Transactional and fail closed: the mode is validated
    /// ([`validate_mode`]) *before* any syscall — a word carrying a bit above
    /// [`FS_MODE_MASK`](tairix_abi::fs::FS_MODE_MASK) is refused rather than
    /// masked into a different mode — and the target path is spelled through
    /// the one shared [`crate::vfs::absolute_path`], so the change can never
    /// name a different node than the browser shows. The listing carries no
    /// mode, so a success re-reads nothing here; the caller re-stats the node
    /// to refresh its Properties view. A VFS refusal leaves the node's mode
    /// exactly as it was.
    ///
    /// # Errors
    ///
    /// A [`ModeError`]: [`ModeError::NoSelection`] on an empty directory,
    /// [`ModeError::Invalid`] for an out-of-range mode (both decided before the
    /// syscall), [`ModeError::Path`] when the node cannot be named, or
    /// [`ModeError::Refused`] when the VFS refuses the change.
    pub fn set_mode_selected<F>(&mut self, mode: u32, set_mode: F) -> Result<(), ModeError>
    where
        F: FnOnce(&str, u32) -> Result<(), Errno>,
    {
        let path = match self.selected_target_path() {
            None => return Err(ModeError::NoSelection),
            Some(Err(err)) => {
                return Err(ModeError::Path(
                    err.source_errno().unwrap_or(Errno::NotFound),
                ))
            }
            Some(Ok(path)) => path,
        };
        validate_mode(mode)?;
        set_mode(&path, mode).map_err(ModeError::Refused)?;
        Ok(())
    }

    /// Change the selected node's owning user and/or group, applying the
    /// change through the injected `set_owner` seam (the `chown(2)` /
    /// `chgrp(2)` shape).
    ///
    /// `set_owner` receives the node's absolute path and the new `(uid, gid)`,
    /// each carrying [`FS_OWNER_UNCHANGED`](tairix_abi::fs::FS_OWNER_UNCHANGED)
    /// for a field the `change` leaves alone, and performs the `fs_set_owner`
    /// syscall under the caller's own identity. Unlike the other write verbs,
    /// this one is **privileged**: the kernel's secured VFS requires
    /// `CAP_FS_CHOWN` to reassign the owner or to set a group the caller is
    /// not a member of, and strips the set-*id* bits on any change. The engine
    /// adds no authority of its own and makes none of that policy decision —
    /// the trusted read-only picker composes the same [`Browser`] and never
    /// calls this — so a caller lacking the privilege sees the kernel's own
    /// [`Errno::PermissionDenied`] as [`OwnerError::Refused`].
    ///
    /// Transactional and fail closed: the change is validated
    /// ([`validate_owner`]) *before* any syscall — a field set to the reserved
    /// sentinel as an explicit target is refused rather than misread — and the
    /// target path is spelled through the one shared
    /// [`crate::vfs::absolute_path`], so the change can never name a different
    /// node than the browser shows. The listing carries no ownership, so a
    /// success re-reads nothing here; the caller re-stats the node to refresh
    /// its Properties view. A VFS refusal leaves the node's ownership exactly
    /// as it was.
    ///
    /// # Errors
    ///
    /// An [`OwnerError`]: [`OwnerError::NoSelection`] on an empty directory,
    /// [`OwnerError::Invalid`] for a sentinel-as-target (both decided before
    /// the syscall), [`OwnerError::Path`] when the node cannot be named, or
    /// [`OwnerError::Refused`] when the VFS refuses the change (including the
    /// missing-`CAP_FS_CHOWN` denial).
    pub fn set_owner_selected<F>(
        &mut self,
        change: OwnerChange,
        set_owner: F,
    ) -> Result<(), OwnerError>
    where
        F: FnOnce(&str, u32, u32) -> Result<(), Errno>,
    {
        let path = match self.selected_target_path() {
            None => return Err(OwnerError::NoSelection),
            Some(Err(err)) => {
                return Err(OwnerError::Path(
                    err.source_errno().unwrap_or(Errno::NotFound),
                ))
            }
            Some(Ok(path)) => path,
        };
        validate_owner(change)?;
        let uid = change.uid.unwrap_or(tairix_abi::fs::FS_OWNER_UNCHANGED);
        let gid = change.gid.unwrap_or(tairix_abi::fs::FS_OWNER_UNCHANGED);
        set_owner(&path, uid, gid).map_err(OwnerError::Refused)?;
        Ok(())
    }

    /// Descend into the selected entry, which must be a directory.
    ///
    /// # Errors
    ///
    /// * [`BrowseError::NoSuchEntry`] if the directory is empty.
    /// * [`BrowseError::NotADirectory`] if the selection is a regular file.
    /// * [`BrowseError::Source`] if the child directory cannot be listed; the
    ///   browser stays on the current directory.
    pub fn open_selected(&mut self) -> Result<(), BrowseError> {
        let index = self.selected_index().ok_or(BrowseError::NoSuchEntry)?;
        self.open_index(index)
    }

    /// Descend into the entry at `index`, which must be a directory.
    ///
    /// # Errors
    ///
    /// * [`BrowseError::NoSuchEntry`] if `index` is out of range.
    /// * [`BrowseError::NotADirectory`] if the entry is a regular file.
    /// * [`BrowseError::Source`] if the child directory cannot be listed; the
    ///   browser stays on the current directory.
    pub fn open_index(&mut self, index: usize) -> Result<(), BrowseError> {
        let entry = self.entries.get(index).ok_or(BrowseError::NoSuchEntry)?;
        if !entry.is_directory() {
            return Err(BrowseError::NotADirectory);
        }

        // Build the child path and list it *before* mutating any state, so a
        // failed read leaves the browser exactly where it was.
        let mut child = self.components.clone();
        child.push(String::from(entry.name()));
        self.navigate_recording(child)
    }

    /// Activate the selected entry — the double-click / `Enter` decision.
    ///
    /// Dispatches by kind through the shared [`Activation`] decision so the
    /// file manager and the trusted picker act identically: a directory is
    /// descended into (as [`open_selected`](Self::open_selected) does) and a
    /// bundle or file is *named* for the caller to launch or open (the engine
    /// performs neither — it holds no such authority).
    ///
    /// # Errors
    ///
    /// * [`BrowseError::NoSuchEntry`] if there is no selection (an empty
    ///   directory).
    /// * [`BrowseError::Source`] if a descended directory cannot be listed, or
    ///   a bundle/file target cannot be named as a valid absolute path; the
    ///   browser stays on the current directory in either case.
    pub fn activate_selected(&mut self) -> Result<Activation, BrowseError> {
        let index = self.selected_index().ok_or(BrowseError::NoSuchEntry)?;
        self.activate_index(index)
    }

    /// Activate the entry at `index` — the pointer-hit form of
    /// [`activate_selected`](Self::activate_selected).
    ///
    /// # Errors
    ///
    /// * [`BrowseError::NoSuchEntry`] if `index` is out of range.
    /// * [`BrowseError::Source`] as for [`activate_selected`](Self::activate_selected).
    pub fn activate_index(&mut self, index: usize) -> Result<Activation, BrowseError> {
        let entry = self.entries.get(index).ok_or(BrowseError::NoSuchEntry)?;
        let kind = entry.kind();
        let name = String::from(entry.name());
        // A link is activated as what it names, and the *path* it is
        // activated by depends on which: a directory is descended through the
        // link (the browser's location then reads as the user navigated), a
        // file is opened through it (the kernel follows the final link on
        // open), but a bundle must be launched by its **resolved** path —
        // the app-load gate's store rule judges the path it is handed, and a
        // link in the user's own directory is not in a store.
        let launch = entry
            .target()
            .map(|target| resolve_target(&self.spelled_path(), target));
        match kind {
            EntryKind::Directory | EntryKind::Link(LinkTarget::Directory) => {
                self.open_index(index)?;
                Ok(Activation::Descended)
            }
            EntryKind::Bundle => Ok(Activation::LaunchBundle {
                path: self.child_target_path(&name)?,
            }),
            EntryKind::Link(LinkTarget::Bundle) => Ok(Activation::LaunchBundle {
                path: launch.ok_or(BrowseError::Source(Errno::NotFound))?,
            }),
            EntryKind::File | EntryKind::Link(LinkTarget::File) => Ok(Activation::OpenFile {
                path: self.child_target_path(&name)?,
            }),
            // A link that resolves to nothing has nothing to activate; the
            // caller reports the refusal rather than opening the link's own
            // bytes, which are a path and not content.
            EntryKind::Link(LinkTarget::Dangling) => Err(BrowseError::Source(Errno::NotFound)),
        }
    }

    /// The absolute path of the directory currently shown — the base a link's
    /// relative target resolves against.
    fn spelled_path(&self) -> String {
        crate::vfs::spell_absolute_path(&self.components)
    }

    /// Spell the validated absolute path of a child named `name` in the current
    /// directory — the target a launch or open acts on.
    ///
    /// Uses the one shared path spelling ([`crate::vfs::absolute_path`]) so the
    /// named target can never differ from what the VFS fetch would read, and a
    /// name that cannot be spelled as a valid, bounded absolute path is a
    /// fail-closed [`BrowseError::Source`] — the same outcome descending into
    /// such a name already produces.
    fn child_target_path(&self, name: &str) -> Result<String, BrowseError> {
        self.spell_child(name).map_err(BrowseError::Source)
    }

    /// Climb to the parent directory, listing it.
    ///
    /// Returns `Ok(true)` after moving up and `Ok(false)` when already at the
    /// root (there is no parent — not an error).
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the parent cannot be listed; the
    /// browser stays on the current directory.
    pub fn go_up(&mut self) -> Result<bool, BrowseError> {
        if self.components.is_empty() {
            return Ok(false);
        }

        let mut parent = self.components.clone();
        parent.pop();
        self.navigate_recording(parent)?;
        Ok(true)
    }

    /// Navigate to the directory named by root-first `components`, listing it
    /// and recording the move on the back history like any other navigation —
    /// the jump-to-an-arbitrary-location primitive (the file manager's "go to
    /// Trash" location uses it to reach `Library/Trash` from wherever the user
    /// is).
    ///
    /// Unlike [`go_up`](Self::go_up) (which climbs to the immediate parent) and
    /// [`open_index`](Self::open_index)
    /// (which only *descends* into a listed child), this reaches any location
    /// the source can list, so a caller that knows a path it wants to show —
    /// not necessarily on the current directory's spine — can go straight
    /// there without a second navigation model (§2.2). It records history and
    /// clears the forward stack exactly as a fresh navigation does.
    ///
    /// Returns `Ok(true)` after moving and `Ok(false)` when `components`
    /// already names the current directory — a no-op, not an error.
    ///
    /// Transactional and fail closed: the target is listed *before* any state
    /// or history changes, so an unlistable location (missing, unreadable, or
    /// its capability refused) leaves the browser — and its history — exactly
    /// where they were.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if `components` cannot be listed.
    pub fn navigate_to(&mut self, components: Vec<String>) -> Result<bool, BrowseError> {
        if components == self.components {
            return Ok(false);
        }
        self.navigate_recording(components)?;
        Ok(true)
    }

    /// Whether there is a previous directory [`go_back`](Self::go_back) can
    /// return to — the enable state of a Back toolbar control.
    #[must_use]
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether there is a directory [`go_forward`](Self::go_forward) can step
    /// to — the enable state of a Forward toolbar control.
    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Return to the previously visited directory, listing it and pushing the
    /// current directory onto the forward history.
    ///
    /// Returns `Ok(true)` after moving and `Ok(false)` when there is no back
    /// history (not an error). Transactional and fail closed: the target is
    /// listed before any state or history changes.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the previous directory can no longer
    /// be listed; the browser and its history stay exactly as they were.
    pub fn go_back(&mut self) -> Result<bool, BrowseError> {
        let Some(target) = self.back.back().cloned() else {
            return Ok(false);
        };
        self.begin(target, Step::Back)?;
        Ok(true)
    }

    /// Step to the directory most recently left by [`go_back`](Self::go_back),
    /// listing it and pushing the current directory back onto the back history.
    ///
    /// Returns `Ok(true)` after moving and `Ok(false)` when there is no forward
    /// history (not an error). Transactional and fail closed.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the forward directory can no longer
    /// be listed; the browser and its history stay exactly as they were.
    pub fn go_forward(&mut self) -> Result<bool, BrowseError> {
        let Some(target) = self.forward.back().cloned() else {
            return Ok(false);
        };
        self.begin(target, Step::Forward)?;
        Ok(true)
    }

    /// List `target` and adopt it as the current directory, recording the
    /// directory being left on the back history and clearing the forward
    /// history (a fresh navigation, as in any browser).
    ///
    /// Transactional: `target` is listed *before* any state or history
    /// changes, so a refused or failing read leaves the browser — and its
    /// history — exactly where they were.
    fn navigate_recording(&mut self, target: Vec<String>) -> Result<(), BrowseError> {
        self.begin(target, Step::Fresh)
    }

    /// Ask the source for `target` and either commit the move at once or record
    /// it as pending.
    ///
    /// The one place a navigation starts, whichever gesture asked for it, so
    /// "listed then moved" and "recorded then committed" are decided once. A
    /// refusal changes nothing at all — not the location, not the entries, not
    /// either history — and a pending navigation replaces any earlier one, so a
    /// user clicking twice goes where they last clicked rather than queueing.
    fn begin(&mut self, target: Vec<String>, step: Step) -> Result<(), BrowseError> {
        match self.source.list(&target).map_err(BrowseError::Source)? {
            Listing::Ready(entries) => {
                self.pending = None;
                self.commit(target, step, entries);
                Ok(())
            }
            Listing::Pending => {
                self.pending = Some(Pending { target, step });
                Ok(())
            }
        }
    }

    /// Apply the history move `step` owes, adopt `target` as the location, and
    /// adopt `entries`.
    fn commit(&mut self, target: Vec<String>, step: Step, entries: Vec<Entry>) {
        match step {
            Step::Reload => {}
            Step::Fresh => {
                let previous = mem::replace(&mut self.components, target);
                Self::push_bounded(&mut self.back, previous);
                self.forward.clear();
            }
            Step::Back => {
                self.back.pop_back();
                let previous = mem::replace(&mut self.components, target);
                Self::push_bounded(&mut self.forward, previous);
            }
            Step::Forward => {
                self.forward.pop_back();
                let previous = mem::replace(&mut self.components, target);
                Self::push_bounded(&mut self.back, previous);
            }
        }
        self.adopt_entries(entries);
    }

    /// Whether a navigation is waiting on its listing.
    ///
    /// A view draws its "listing…" cue from this. The location and entries it
    /// shows are still the ones it had, so the cue is the only thing that
    /// changes while a read is in flight.
    #[must_use]
    pub const fn is_listing(&self) -> bool {
        self.pending.is_some()
    }

    /// Where the pending navigation is going, or `None` when nothing is
    /// pending.
    #[must_use]
    pub fn listing_target(&self) -> Option<&[String]> {
        self.pending
            .as_ref()
            .map(|pending| pending.target.as_slice())
    }

    /// Ask the source again for the pending navigation's listing, committing it
    /// if it has arrived.
    ///
    /// `Ok(true)` when the move committed (the caller repaints), `Ok(false)`
    /// when nothing was pending or the answer has still not arrived. This is
    /// what an embedder calls on the wake that says its worker finished — it is
    /// never a poll, and calling it with nothing pending costs one branch.
    ///
    /// # Errors
    ///
    /// [`BrowseError::Source`] if the directory can no longer be listed. The
    /// pending navigation is dropped and the browser stays exactly where it
    /// was, so a refused listing is reported in place rather than stranding the
    /// view in a directory it could not read.
    pub fn resume(&mut self) -> Result<bool, BrowseError> {
        let Some(pending) = self.pending.take() else {
            return Ok(false);
        };
        match self.source.list(&pending.target) {
            Ok(Listing::Ready(entries)) => {
                self.commit(pending.target, pending.step, entries);
                Ok(true)
            }
            Ok(Listing::Pending) => {
                self.pending = Some(pending);
                Ok(false)
            }
            Err(errno) => Err(BrowseError::Source(errno)),
        }
    }

    /// Push `location` onto `stack`, dropping the oldest entries to keep the
    /// stack within [`HISTORY_MAX`] so navigation history cannot grow without
    /// bound.
    fn push_bounded(stack: &mut VecDeque<Vec<String>>, location: Vec<String>) {
        stack.push_back(location);
        while stack.len() > HISTORY_MAX {
            stack.pop_front();
        }
    }

    /// Replace the loaded entries — ordered by the current sort mode — and
    /// clamp the selection into the new range.
    fn adopt_entries(&mut self, mut entries: Vec<Entry>) {
        sort_entries(&mut entries, self.sort_mode);
        self.entries = entries;
        self.clamp_selection();
        // The selection's indices refer to the previous listing; a fresh
        // directory collapses it to the (clamped) focused entry.
        self.reset_selection_to_focus();
        // A freshly listed directory is shown from the top; a caller reveals
        // the (clamped) selection again once it knows the live geometry.
        self.scroll_offset = 0;
    }

    /// Collapse the multi-selection to the single focused entry, or clear it on
    /// an empty directory — the invariant restored after every listing change,
    /// since selection indices only make sense for the listing they were made
    /// in.
    fn reset_selection_to_focus(&mut self) {
        if self.entries.is_empty() {
            self.selection.clear();
        } else {
            self.selection.single(self.selected);
        }
    }

    /// Clamp the selection cursor into the current entry range (to the last
    /// entry, or to `0` when the directory is empty).
    fn clamp_selection(&mut self) {
        self.selected = match self.entries.len().checked_sub(1) {
            Some(last) => self.selected.min(last),
            None => 0,
        };
    }
}
