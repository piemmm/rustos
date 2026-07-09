//! The I/O-free session state: the lazily populated directory tree, the
//! two panes, cursors and scrolling, sorting, and the hidden-entries
//! toggle.
//!
//! Every effect goes through the injected [`Fs`] seam; the model never
//! performs I/O itself. A directory is read only when it is first shown or
//! expanded — never a whole-volume scan, so browsing costs the working set
//! and a 100 TB volume costs no more than the directories actually opened.
//! A refused listing surfaces its [`Errno`](rustos_abi::Errno) on the
//! message line and leaves
//! the previous state (cursor, listing, expansion) untouched — fail closed,
//! never a crash, never a fabricated entry.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::FileKind;

use crate::fs::{Fs, FsEntry, VolumeSpace};

/// Which pane holds the keyboard focus.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Pane {
    /// The directory-tree pane (left).
    Tree,
    /// The file pane listing the selected directory (right).
    Files,
}

/// The modal surface covering (or annotating) the panes, if any.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum Overlay {
    /// The panes are shown plainly.
    #[default]
    None,
    /// The sort menu is awaiting its selection key.
    SortMenu,
    /// The help overlay covers the panes.
    Help,
}

/// The column the file pane is ordered by.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SortKey {
    /// Entry name, the default.
    Name,
    /// Filename extension (the text after the last dot), then name.
    Extension,
    /// Byte size, then name.
    Size,
    /// Modification stamp, then name.
    Modified,
}

/// One directory in the tree pane.
#[derive(Clone, Debug)]
pub struct DirNode {
    /// The directory's name (a single component; `/` for the root).
    pub name: String,
    /// The directory's full path.
    pub path: String,
    /// Whether the node's children are currently shown.
    pub expanded: bool,
    /// The child directories, or `None` while never yet read (lazy).
    pub children: Option<Vec<DirNode>>,
}

/// One visible row of the tree pane: the flattened view of the expanded
/// tree, produced by [`Model::tree_rows`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeRow {
    /// Nesting depth (0 for the root).
    pub depth: usize,
    /// The directory's name.
    pub name: String,
    /// The directory's full path.
    pub path: String,
    /// Whether the row is currently expanded.
    pub expanded: bool,
}

/// The whole session state the renderer draws and the key handler mutates.
pub struct Model {
    /// The tree of directories, populated lazily as nodes expand.
    pub root: DirNode,
    /// The focused pane.
    pub pane: Pane,
    /// Cursor index into [`Model::tree_rows`].
    pub tree_cursor: usize,
    /// First tree row shown (scrolling).
    pub tree_scroll: usize,
    /// Path of the directory the file pane lists.
    pub files_dir: String,
    /// The file pane's entries, sorted per the active key.
    pub files: Vec<FsEntry>,
    /// Cursor index into the visible file entries.
    pub file_cursor: usize,
    /// First file row shown (scrolling).
    pub file_scroll: usize,
    /// Active sort column.
    pub sort_key: SortKey,
    /// `true` for descending order.
    pub sort_desc: bool,
    /// Whether dotfile entries are shown.
    pub show_hidden: bool,
    /// The one-line message surface (errors, notices).
    pub message: Option<String>,
    /// Free/total space of the volume backing the listed directory.
    pub space: Option<VolumeSpace>,
    /// The modal surface currently shown, if any.
    pub overlay: Overlay,
    /// The bundle's rendered help text, shown by the `?` overlay.
    pub help_text: String,
    /// Set when the session should end.
    pub quit: bool,
}

impl Model {
    /// Build the session rooted at `root_path`, reading the root listing
    /// through `fs`.
    ///
    /// # Errors
    ///
    /// The [`rustos_abi::Errno`] of the initial root listing: a session
    /// that cannot list its starting directory fails loudly at startup
    /// rather than presenting an empty view.
    pub fn new(
        fs: &mut dyn Fs,
        root_path: &str,
        help_text: String,
    ) -> Result<Self, rustos_abi::Errno> {
        let mut model = Self {
            root: DirNode {
                name: String::from(root_path),
                path: String::from(root_path),
                expanded: true,
                children: None,
            },
            pane: Pane::Tree,
            tree_cursor: 0,
            tree_scroll: 0,
            files_dir: String::from(root_path),
            files: Vec::new(),
            file_cursor: 0,
            file_scroll: 0,
            sort_key: SortKey::Name,
            sort_desc: false,
            show_hidden: false,
            message: None,
            space: None,
            overlay: Overlay::None,
            help_text,
            quit: false,
        };
        let entries = fs.list_dir(root_path)?;
        model.root.children = Some(child_dirs_of(root_path, &entries));
        model.files = entries;
        model.sort_files();
        model.space = fs.volume_space(root_path);
        Ok(model)
    }

    /// The flattened, currently visible tree rows (expanded nodes only,
    /// hidden names filtered per the toggle).
    #[must_use]
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.push_rows(&self.root, 0, &mut rows);
        rows
    }

    fn push_rows(&self, node: &DirNode, depth: usize, rows: &mut Vec<TreeRow>) {
        rows.push(TreeRow {
            depth,
            name: node.name.clone(),
            path: node.path.clone(),
            expanded: node.expanded,
        });
        if !node.expanded {
            return;
        }
        if let Some(children) = &node.children {
            for child in children {
                if self.show_hidden || !child.name.starts_with('.') {
                    self.push_rows(child, depth + 1, rows);
                }
            }
        }
    }

    /// The file-pane entries after the hidden-names filter.
    #[must_use]
    pub fn visible_files(&self) -> Vec<&FsEntry> {
        self.files
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .collect()
    }

    /// Re-sort the file listing per the active key and direction.
    /// Directories always group before files, mirroring the tree.
    pub fn sort_files(&mut self) {
        let key = self.sort_key;
        let desc = self.sort_desc;
        self.files.sort_by(|a, b| {
            let group = b.kind.is_dir().cmp(&a.kind.is_dir());
            if group != core::cmp::Ordering::Equal {
                return group;
            }
            let ordered = match key {
                SortKey::Name => a.name.cmp(&b.name),
                SortKey::Extension => extension(&a.name)
                    .cmp(extension(&b.name))
                    .then_with(|| a.name.cmp(&b.name)),
                SortKey::Size => a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name)),
                SortKey::Modified => a
                    .modified
                    .cmp(&b.modified)
                    .then_with(|| a.name.cmp(&b.name)),
            };
            if desc {
                ordered.reverse()
            } else {
                ordered
            }
        });
    }

    /// Surface `errno` on the message line, spelling out the operation.
    pub fn report(&mut self, what: &str, errno: rustos_abi::Errno) {
        self.message = Some(format!("{what}: {errno:?}"));
    }
}

/// The child `DirNode`s of `parent` among `entries` (directories only),
/// name-ordered so the tree is stable regardless of driver order.
pub(crate) fn child_dirs_of(parent: &str, entries: &[FsEntry]) -> Vec<DirNode> {
    let mut dirs: Vec<DirNode> = entries
        .iter()
        .filter(|e| e.kind == FileKind::Directory)
        .map(|e| DirNode {
            name: e.name.clone(),
            path: join(parent, &e.name),
            expanded: false,
            children: None,
        })
        .collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    dirs
}

/// Join a directory path and a child name without doubling separators.
#[must_use]
pub fn join(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The filename extension: the text after the last dot, empty when none.
fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    }
}
