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

use crate::entry::Entry;
use crate::error::BrowseError;
use crate::layout::ViewMode;
use crate::rename::{validate_new_name, RenameError};
use crate::sort::{sort_entries, SortMode};
use crate::source::DirectorySource;

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
    sort_mode: SortMode,
    view_mode: ViewMode,
    scroll_offset: u64,
    /// Directories visited before the current one, oldest first; the last is
    /// where [`go_back`](Self::go_back) returns to.
    back: VecDeque<Vec<String>>,
    /// Directories stepped away from by [`go_back`](Self::go_back), oldest
    /// first; the last is where [`go_forward`](Self::go_forward) returns to.
    /// Cleared by any fresh navigation, as a browser's forward history is.
    forward: VecDeque<Vec<String>>,
}

impl<S: DirectorySource> Browser<S> {
    /// Open the browser at the filesystem root (`/`), listing its children.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the root directory cannot be listed.
    pub fn open_root(mut source: S) -> Result<Self, BrowseError> {
        let sort_mode = SortMode::default_order();
        let mut entries = source.list(&[]).map_err(BrowseError::Source)?;
        sort_entries(&mut entries, sort_mode);
        Ok(Self {
            source,
            components: Vec::new(),
            entries,
            selected: 0,
            sort_mode,
            view_mode: ViewMode::default(),
            scroll_offset: 0,
            back: VecDeque::new(),
            forward: VecDeque::new(),
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
        Ok(())
    }

    /// Move the selection to the next entry, stopping at the last. A no-op on
    /// an empty directory.
    pub fn select_next(&mut self) {
        if let Some(last) = self.entries.len().checked_sub(1) {
            self.selected = self.selected.saturating_add(1).min(last);
        }
    }

    /// Move the selection to the previous entry, stopping at the first.
    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Re-read the current directory from the source, preserving the selection
    /// where it still points at an entry and clamping it otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the directory can no longer be
    /// listed; the previously loaded entries are left untouched.
    pub fn refresh(&mut self) -> Result<(), BrowseError> {
        let entries = self
            .source
            .list(&self.components)
            .map_err(BrowseError::Source)?;
        self.adopt_entries(entries);
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

    /// Spell the absolute path of a child named `name` in the current
    /// directory, mapping a spelling failure onto the matching
    /// [`RenameError`]. A `name` that already passed [`validate_new_name`] can
    /// only fail here if the *whole* path exceeds the kernel's limit.
    fn child_path(&self, name: &str) -> Result<String, RenameError> {
        let mut components = self.components.clone();
        components.push(String::from(name));
        crate::vfs::absolute_path(&components).map_err(|errno| match errno {
            Errno::LengthOutOfRange => RenameError::TooLong,
            _ => RenameError::Invalid,
        })
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

    /// Navigate to the ancestor `depth` path components deep (root-first) — the
    /// breadcrumb-click primitive. `depth == 0` is the filesystem root and
    /// `depth == components().len()` is the directory already shown.
    ///
    /// Records the move on the back history like any other navigation, and
    /// stays transactional and fail closed: the ancestor is listed before any
    /// state changes, so a refused read leaves the browser exactly where it
    /// was.
    ///
    /// Returns `Ok(true)` after moving and `Ok(false)` when `depth` already
    /// names the current directory or is past its end (no such ancestor) — a
    /// no-op, not an error, exactly as clicking the current-directory crumb is.
    ///
    /// # Errors
    ///
    /// Returns [`BrowseError::Source`] if the ancestor cannot be listed; the
    /// browser stays on the current directory.
    pub fn navigate_to_depth(&mut self, depth: usize) -> Result<bool, BrowseError> {
        if depth >= self.components.len() {
            return Ok(false);
        }
        let target = self.components[..depth].to_vec();
        self.navigate_recording(target)?;
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
        let entries = self.source.list(&target).map_err(BrowseError::Source)?;
        self.back.pop_back();
        let previous = mem::replace(&mut self.components, target);
        Self::push_bounded(&mut self.forward, previous);
        self.adopt_entries(entries);
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
        let entries = self.source.list(&target).map_err(BrowseError::Source)?;
        self.forward.pop_back();
        let previous = mem::replace(&mut self.components, target);
        Self::push_bounded(&mut self.back, previous);
        self.adopt_entries(entries);
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
        let entries = self.source.list(&target).map_err(BrowseError::Source)?;
        let previous = mem::replace(&mut self.components, target);
        Self::push_bounded(&mut self.back, previous);
        self.forward.clear();
        self.adopt_entries(entries);
        Ok(())
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
        // A freshly listed directory is shown from the top; a caller reveals
        // the (clamped) selection again once it knows the live geometry.
        self.scroll_offset = 0;
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
