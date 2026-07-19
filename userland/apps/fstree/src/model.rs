//! The I/O-free session state: the lazily populated directory tree, the
//! two panes, cursors and scrolling, sorting, and the hidden-entries
//! toggle.
//!
//! Every effect goes through the injected [`Fs`] seam; the model never
//! performs I/O itself. A directory is read only when it is first shown or
//! expanded — never a whole-volume scan, so browsing costs the working set
//! and a 100 TB volume costs no more than the directories actually opened.
//! A refused listing surfaces its [`Errno`](tairix_abi::Errno) on the
//! message line and leaves
//! the previous state (cursor, listing, expansion) untouched — fail closed,
//! never a crash, never a fabricated entry.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::FileKind;
use tairix_glob::Pattern;

use crate::fs::{Fs, FsEntry, VolumeInfo, VolumeSpace};
use crate::ops::FileOp;
use crate::settings::Settings;
use crate::tag::{Batch, TagSet};
use crate::view_disasm::DisasmView;
use crate::view_hex::HexView;
use crate::view_text::TextView;
use crate::walk::WalkState;

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
    /// A batch's per-file failure report ([`Model::report_lines`]) covers
    /// the panes until a key dismisses it.
    Report,
    /// The volume list (`V`): the published storage roots, one openable
    /// per row.
    Volumes,
    /// The settings menu (`S`): the persisted confirmation toggles.
    Settings,
    /// The attributes editor (`a`): the selection's mode bits and
    /// extended attributes ([`Model::attrs`]).
    Attrs,
}

/// Which body the session shows: the two panes, the flattened branch
/// view listing every file under one directory, or a file viewer.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum View {
    /// The tree and file panes.
    #[default]
    Panes,
    /// The flattened branch view (`v`), fed by the live walk.
    Flat,
    /// A full-screen file viewer ([`Model::viewer`]).
    Viewer,
}

/// The open file viewer: text paging, the hex dump, or the disassembly
/// viewer. Entered with Enter on a regular file (the head sample picks
/// the mode) or through the `o` open-as chooser; `x`/`t`/`d` switch
/// between them at the same place.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Viewer {
    /// The streaming text pager.
    Text(TextView),
    /// The offset/hex/ASCII dump.
    Hex(HexView),
    /// The sandbox-decoded container summary / disassembly viewer.
    Disasm(DisasmView),
}

/// Which viewer a prompt belongs to — the goto and search questions ask
/// for different things per viewer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ViewerKind {
    /// The text pager (goto asks a line, search literal text).
    Text,
    /// The hex dump (goto asks an offset, search text or `0x…` bytes).
    Hex,
    /// The disassembly code pane (goto asks an address, search
    /// mnemonic/operand text).
    Disasm,
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
    /// The box-drawing branch prefix drawn before the name — the ancestor
    /// connector bars plus this row's own `├─`/`└─` junction (empty for
    /// the root), so the tree reads like `XTree Gold`'s directory window.
    pub branch: String,
    /// The fold marker before the name: `-` for an expanded branch, `+`
    /// for a collapsed one that has (or may have) subdirectories, and a
    /// space for a directory known to hold none.
    pub fold: char,
}

/// The modal surface the message line carries while a question is open.
/// While present, keys feed the prompt; the panes underneath stay drawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Prompt {
    /// The mode editor (`m` inside the attributes editor).
    Mode(ModePrompt),
    /// The attribute editor's `key=value` line (`n` or Enter inside the
    /// attributes editor).
    AttrEdit(AttrEditPrompt),
    /// A text-line question: a destination path or a new name.
    Input(InputPrompt),
    /// The delete confirmation (`d`).
    ConfirmDelete(ConfirmPrompt),
    /// A paused operation's per-file overwrite question.
    Overwrite(OverwritePrompt),
    /// The batch delete confirmation (`d` with tags).
    ConfirmBatchDelete {
        /// How many entries are tagged, spelled out in the question.
        count: usize,
    },
    /// A paused batch's per-file overwrite question.
    BatchOverwrite(BatchPrompt),
    /// The open-as chooser (`o`): t)ext, he(x), or d)isassembly.
    OpenAs(OpenAsPrompt),
    /// The ISA chooser for a decode with no machine field: x)86-64,
    /// a)arch64, r)iscv64, or w)asm.
    IsaPick(IsaPrompt),
}

/// What the open [`InputPrompt`] asks for, carrying its subject.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputOp {
    /// The destination a copy of the subject lands at (`c`).
    CopyDest {
        /// Full path of the entry being copied.
        src: String,
        /// The entry's name, shown in the prompt.
        name: String,
        /// The entry's kind.
        kind: FileKind,
    },
    /// The destination the subject moves to (`m`).
    MoveDest {
        /// Full path of the entry being moved.
        src: String,
        /// The entry's name, shown in the prompt.
        name: String,
        /// The entry's kind.
        kind: FileKind,
    },
    /// The subject's new name within its directory (`r`).
    RenameTo {
        /// Full path of the entry being renamed.
        src: String,
        /// The entry's current name, shown in the prompt.
        name: String,
        /// The entry's kind.
        kind: FileKind,
    },
    /// The name of a directory created in the listed directory (`M`).
    MkdirName,
    /// A filename-glob pattern tagging every matching visible entry (`T`).
    TagGlob,
    /// The live filename filter's pattern (`f`): each edit re-applies it
    /// to the file pane as typed; Esc restores the filter held before the
    /// prompt opened.
    FilterPattern {
        /// The filter as it stood when the prompt opened, restored on Esc.
        previous: Option<NameFilter>,
    },
    /// A filename-glob pattern searched for below the focused directory
    /// (`/`).
    SearchGlob,
    /// The viewer's goto target: a 1-based line (text), a byte offset,
    /// decimal or `0x`-hex (hex), or an address (disassembly).
    ViewerGoto {
        /// Which open viewer asked.
        kind: ViewerKind,
    },
    /// The viewer's search subject: literal text (text), text / `0x…`
    /// byte sequence (hex), or mnemonic/operand text (disassembly).
    ViewerSearch {
        /// Which open viewer asked.
        kind: ViewerKind,
    },
    /// The text a content search looks for inside files (`F`).
    ContentNeedle {
        /// Whether the search runs over the tagged set (`true`) or the
        /// focused branch, decided when the prompt opens and spelled out
        /// in its question.
        tagged: bool,
    },
    /// The destination directory a batch copy/move lands in (`c`/`m` with
    /// tags).
    BatchDest {
        /// Whether the batch moves (`true`) or copies.
        moving: bool,
        /// How many entries are tagged, spelled out in the prompt.
        count: usize,
    },
}

/// A one-line text question: the operation it feeds and the text typed so
/// far. Printable keys and Backspace edit, Enter submits, Esc cancels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputPrompt {
    /// What the answer is used for, with its subject.
    pub op: InputOp,
    /// The text typed so far.
    pub input: String,
}

/// The delete confirmation (`d`): the entry about to be removed. Only
/// `y`/`Y` proceeds; any other key declines — never an assumed yes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmPrompt {
    /// Full path of the entry to delete.
    pub path: String,
    /// The entry's name, shown in the question.
    pub name: String,
    /// The entry's kind (a directory is deleted with its contents, and
    /// the question says so).
    pub kind: FileKind,
}

/// A paused [`FileOp`] whose next step would overwrite an existing file;
/// the user answers o)verwrite, s)kip, or c)ancel per file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverwritePrompt {
    /// The paused operation; its [`FileOp::conflict`] names the paths.
    pub op: FileOp,
    /// The directories to refresh once the operation ends, carried across
    /// the pause.
    pub refresh: Vec<String>,
}

/// A paused [`Batch`] whose current entry would overwrite an existing
/// file; the user answers o)verwrite, s)kip, or c)ancel — cancel drops
/// the batch's remaining entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchPrompt {
    /// The paused batch; its [`Batch::conflict`] names the paths.
    pub batch: Batch,
    /// The directories to refresh once the batch ends, carried across the
    /// pause.
    pub refresh: Vec<String>,
}

/// The open-as chooser (`o`): the file it opens once a viewer is picked.
/// `t`/`x`/`d` choose, Esc cancels, any other key keeps asking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAsPrompt {
    /// Full path of the file to open.
    pub path: String,
    /// The file's apparent size in bytes.
    pub size: u64,
}

/// The ISA chooser: why it is asking, so the answer lands in the right
/// place. `x`/`a`/`r`/`w` choose, Esc cancels, any other key keeps asking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsaPrompt {
    /// What the chosen ISA is for.
    pub purpose: IsaPurpose,
}

/// What an [`IsaPrompt`]'s answer drives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IsaPurpose {
    /// Force-open an unrecognised file as raw code (`d`/`o`+`d`).
    OpenRaw {
        /// Full path of the file to open.
        path: String,
        /// The file's apparent size in bytes.
        size: u64,
        /// The byte offset the code pane starts at (the place a hex view
        /// handed over; 0 from the panes).
        offset: u64,
    },
    /// Enter a code region of a container that names no ISA (rxe).
    EnterRegion {
        /// The region's index in the open summary.
        index: usize,
    },
    /// Override the open code pane's ISA (`I`).
    Override,
}

/// The last completed file operation the repeat key (`.`) re-applies to
/// the focused selection: the destination directory of a copy or move,
/// or a delete (which still asks per the confirmation setting).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepeatOp {
    /// Copy the selection into the recorded directory.
    CopyInto(String),
    /// Move the selection into the recorded directory.
    MoveInto(String),
    /// Delete the selection.
    Delete,
}

/// The file pane's live filename filter (`f`): the pattern as typed and
/// its compiled form. While the typed text is not (yet) a valid glob —
/// e.g. an unclosed bracket expression mid-edit — `pattern` is `None`,
/// the pane stays unfiltered, and the status line says the pattern is
/// bad; nothing is silently hidden by a filter that could not compile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameFilter {
    /// The pattern as typed.
    pub text: String,
    /// The compiled matcher, or `None` while the text does not compile.
    pub pattern: Option<Pattern>,
}

impl NameFilter {
    /// Compile `text` into the filter it describes: `None` for empty text
    /// (no filter), a filter with `pattern: None` for text that does not
    /// compile.
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        Some(Self {
            text: String::from(text),
            pattern: Pattern::new(text).ok(),
        })
    }

    /// Whether `name` passes the filter. A filter whose pattern did not
    /// compile passes everything — the status line reports the bad
    /// pattern instead of hiding entries behind it.
    #[must_use]
    pub fn admits(&self, name: &str) -> bool {
        self.pattern.as_ref().map_or(true, |p| p.matches(name))
    }
}

/// The extended attributes of one entry, `(key, value)`, in the
/// backing's stable order.
pub type AttrEntries = Vec<(String, Vec<u8>)>;

/// The attributes editor (`a`): the selection's permission bits and its
/// extended attributes, loaded once when the view opens and refreshed
/// after every applied change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttrsView {
    /// Full path of the entry whose attributes are shown.
    pub path: String,
    /// The entry's name, shown in the title.
    pub name: String,
    /// The entry's current permission bits (the `m` editor's subject).
    pub mode: u32,
    /// The visible extended attributes, `(key, value)`, in the backing's
    /// stable order. Values are opaque bytes; the renderer escapes
    /// non-printable content rather than trusting it.
    pub entries: AttrEntries,
    /// Cursor index into [`AttrsView::entries`].
    pub cursor: usize,
    /// Whether the mounted format stores no attributes at all
    /// ([`tairix_abi::Errno::NotSupported`]) — stated honestly in place
    /// of an empty list, with the editing keys inert.
    pub unsupported: bool,
}

/// The attribute editor's modal `key=value` line. While present, keys
/// feed the prompt (printable characters and Backspace edit, Enter
/// applies through the seam, Esc cancels); the attributes view underneath
/// stays drawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttrEditPrompt {
    /// Full path of the entry whose attribute is being edited.
    pub path: String,
    /// The entry's name, shown in the prompt.
    pub name: String,
    /// The `key=value` line typed so far.
    pub input: String,
}

/// The modal mode-editor prompt (`m` inside the attributes editor): the
/// entry being edited and the
/// octal digits typed so far. While present, keys feed the prompt (octal
/// digits and Backspace edit, Enter applies through the seam, Esc
/// cancels); the panes underneath stay drawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModePrompt {
    /// Full path of the entry whose mode is being edited.
    pub path: String,
    /// The entry's name, shown in the prompt.
    pub name: String,
    /// The entry's current permission bits, shown as the reference value.
    pub current: u32,
    /// The octal digits typed so far (starts as the current mode, so Enter
    /// alone re-applies it).
    pub input: String,
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
    /// The file pane's live filename filter (`f`), when one is set.
    pub filter: Option<NameFilter>,
    /// The one-line message surface (errors, notices).
    pub message: Option<String>,
    /// Free/total space of the volume backing the listed directory.
    pub space: Option<VolumeSpace>,
    /// The modal surface currently shown, if any.
    pub overlay: Overlay,
    /// The open prompt, when a question is being asked.
    pub prompt: Option<Prompt>,
    /// The bundle's rendered help text, shown by the `?` overlay.
    pub help_text: String,
    /// The tagged entries (`t`, the glob prompt, invert).
    pub tags: TagSet,
    /// The live walk feeding the flattened view or the usage figures.
    pub walk: Option<WalkState>,
    /// The open file viewer, shown while [`Model::view`] is
    /// [`View::Viewer`].
    pub viewer: Option<Viewer>,
    /// Which body the session shows.
    pub view: View,
    /// Cursor index into the flattened view's entries.
    pub flat_cursor: usize,
    /// First flattened row shown (scrolling).
    pub flat_scroll: usize,
    /// The lines the report overlay shows (a batch's per-file failures).
    pub report_lines: Vec<String>,
    /// The volume list the `V` overlay shows, fetched when it opens.
    pub volumes: Vec<VolumeInfo>,
    /// Cursor index into [`Model::volumes`].
    pub volume_cursor: usize,
    /// The attributes editor's state while the `a` overlay is open.
    pub attrs: Option<AttrsView>,
    /// The last completed file operation, for the repeat key (`.`).
    pub last_op: Option<RepeatOp>,
    /// The persisted session preferences (the confirmation toggles).
    pub settings: Settings,
    /// The user's home directory the settings persist under, when known;
    /// `None` keeps changes session-only (and the menu says so).
    pub settings_home: Option<String>,
    /// Set when the session should end.
    pub quit: bool,
    /// Content rows of the tree window at the last draw, so a page key
    /// moves by exactly one visible window. Zero until the first draw.
    pub tree_page: usize,
    /// Content rows of the file window at the last draw.
    pub file_page: usize,
}

impl Model {
    /// Build the session rooted at `root_path`, reading the root listing
    /// through `fs`.
    ///
    /// # Errors
    ///
    /// The [`tairix_abi::Errno`] of the initial root listing: a session
    /// that cannot list its starting directory fails loudly at startup
    /// rather than presenting an empty view.
    pub fn new(
        fs: &mut dyn Fs,
        root_path: &str,
        help_text: String,
    ) -> Result<Self, tairix_abi::Errno> {
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
            filter: None,
            message: None,
            space: None,
            overlay: Overlay::None,
            prompt: None,
            help_text,
            tags: TagSet::new(),
            walk: None,
            viewer: None,
            view: View::Panes,
            flat_cursor: 0,
            flat_scroll: 0,
            report_lines: Vec::new(),
            volumes: Vec::new(),
            volume_cursor: 0,
            attrs: None,
            last_op: None,
            settings: Settings::default(),
            settings_home: None,
            quit: false,
            tree_page: 0,
            file_page: 0,
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
        self.push_rows(&self.root, 0, "", true, true, &mut rows);
        rows
    }

    /// The child directories of `node` the hidden-names toggle admits, in
    /// listing order — the set the tree draws and connects.
    fn visible_children<'a>(&self, node: &'a DirNode) -> Vec<&'a DirNode> {
        match &node.children {
            Some(children) => children
                .iter()
                .filter(|c| self.show_hidden || !c.name.starts_with('.'))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Flatten `node` into `rows`, threading the ancestor connector prefix
    /// so each row carries its own box-drawing branch. `ancestors` is the
    /// prefix of vertical bars for the levels above; `is_last` is whether
    /// `node` is the last of its siblings; `is_root` suppresses a junction
    /// for the tree's own root.
    fn push_rows(
        &self,
        node: &DirNode,
        depth: usize,
        ancestors: &str,
        is_last: bool,
        is_root: bool,
        rows: &mut Vec<TreeRow>,
    ) {
        let children = self.visible_children(node);
        let has_children = !children.is_empty();
        let branch = if is_root {
            String::new()
        } else {
            let junction = if is_last { "└─" } else { "├─" };
            format!("{ancestors}{junction}")
        };
        let fold = if node.expanded {
            if has_children {
                '-'
            } else {
                ' '
            }
        } else if node.children.is_some() && !has_children {
            // Read and known to hold no (visible) subdirectory.
            ' '
        } else {
            // Expandable: unread, or read with subdirectories to show.
            '+'
        };
        rows.push(TreeRow {
            depth,
            name: node.name.clone(),
            path: node.path.clone(),
            expanded: node.expanded,
            branch,
            fold,
        });
        if !node.expanded || !has_children {
            return;
        }
        let child_ancestors = if is_root {
            String::new()
        } else {
            let bar = if is_last { "  " } else { "│ " };
            format!("{ancestors}{bar}")
        };
        let last = children.len() - 1;
        for (index, child) in children.into_iter().enumerate() {
            self.push_rows(
                child,
                depth + 1,
                &child_ancestors,
                index == last,
                false,
                rows,
            );
        }
    }

    /// The file-pane entries after the hidden-names filter and the live
    /// filename filter (`f`).
    #[must_use]
    pub fn visible_files(&self) -> Vec<&FsEntry> {
        self.files
            .iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .filter(|e| self.filter.as_ref().map_or(true, |f| f.admits(&e.name)))
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
    pub fn report(&mut self, what: &str, errno: tairix_abi::Errno) {
        self.message = Some(format!("{what}: {errno:?}"));
    }
}

/// Rebuild a node's children from a fresh listing, preserving the
/// expansion state and already-read children of every surviving name so a
/// refresh never collapses the branches the user has open.
pub(crate) fn merge_child_dirs(
    parent: &str,
    entries: &[FsEntry],
    mut old: Vec<DirNode>,
) -> Vec<DirNode> {
    let mut dirs = child_dirs_of(parent, entries);
    for node in &mut dirs {
        if let Some(index) = old.iter().position(|o| o.name == node.name) {
            let previous = old.swap_remove(index);
            node.expanded = previous.expanded;
            node.children = previous.children;
        }
    }
    dirs
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
