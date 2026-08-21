//! Host tests: the model over an in-memory [`Fs`], golden-grid renders,
//! and a scripted end-to-end session — no kernel, no terminal.

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use core::cell::{Cell as CoreCell, RefCell};

use tairix_abi::time::Time64;
use tairix_abi::{
    CapabilityId, Errno, FileKind, LoadHeader, ManifestHeader, RxePermission, Segment,
    LOAD_FLAG_PIE, LOAD_MAGIC, MANIFEST_MAGIC, RXE_PAGE_SIZE,
};
use tairix_curses::{Event, Pos, Size, Window};

use tairix_log::{Event as LogEvent, Sink};
use tairix_path::{join, leaf_name};
use tairix_sandbox::decode::{DecodeService, SymbolRecord, MAX_INPUT};
use tairix_sandbox::loopback::LoopbackLauncher;
use tairix_sandbox::ParserSandbox;

use crate::app::{handle_event, refresh_viewer, viewer_tick, walk_tick};
use crate::fs::{Fs, FsEntry, RenameOutcome, VolumeInfo, VolumeSpace};
use crate::info::{note_hidden_entries, Info};
use crate::model::{ModePrompt, Model, Overlay, Pane, Prompt, RepeatOp, SortKey, View, Viewer};
use crate::ops::{is_inside, parent_of};
use crate::render::render;
use crate::search::{ContentScan, Needle};
use crate::settings::{config_path, Settings};
use crate::tag::{TagEntry, TagRange};
use crate::view_disasm::{Decode, DisasmPane, DisasmView};
use crate::walk::{FlatEntry, WalkPurpose, WalkState, Walker};

/// An in-memory filesystem: per-path listings, per-path file bytes,
/// per-path modes, a denied set (listings and stats), a set-mode denial
/// set, failure injection for the mutating operations, a log of applied
/// mode changes, and a count of listings served (so laziness is
/// observable). Listings are kept in step with the mutations so a refresh
/// observes the operation's effect, exactly as the kernel view would.
struct FakeFs {
    dirs: BTreeMap<String, Vec<FsEntry>>,
    files: BTreeMap<String, Vec<u8>>,
    modes: BTreeMap<String, u32>,
    denied: Vec<String>,
    set_denied: Vec<String>,
    set_modes: RefCell<Vec<(String, u32)>>,
    reads: CoreCell<usize>,
    space: Option<VolumeSpace>,
    /// `read` of this path fails with the errno.
    read_fail: Option<(String, Errno)>,
    /// `write` to this path fails with the errno.
    write_fail: Option<(String, Errno)>,
    /// `rename` reports the cross-volume boundary for these prefixes.
    cross_volume: Vec<String>,
    /// What `list_volumes` reports (empty by default).
    volumes: Vec<VolumeInfo>,
    /// Per-path extended attributes, in insertion order (the backing's
    /// stable order the seam contract requires).
    attrs: BTreeMap<String, Vec<(String, Vec<u8>)>>,
    /// When set, every attribute call answers `NotSupported` — the
    /// FAT32/ext4-class mount the view states honestly.
    attrs_unsupported: bool,
    /// The target every symbolic link in the tree stores, keyed by link
    /// path — the only thing a link holds.
    links: BTreeMap<String, String>,
}

impl FakeFs {
    fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            files: BTreeMap::new(),
            links: BTreeMap::new(),
            modes: BTreeMap::new(),
            denied: Vec::new(),
            set_denied: Vec::new(),
            set_modes: RefCell::new(Vec::new()),
            reads: CoreCell::new(0),
            space: Some(VolumeSpace {
                free_bytes: 500,
                total_bytes: 1000,
            }),
            read_fail: None,
            write_fail: None,
            cross_volume: Vec::new(),
            volumes: Vec::new(),
            attrs: BTreeMap::new(),
            attrs_unsupported: false,
        }
    }

    /// Register the volumes `list_volumes` reports.
    fn volumes(mut self, volumes: Vec<VolumeInfo>) -> Self {
        self.volumes = volumes;
        self
    }

    /// Register an extended attribute on a path.
    fn attr(mut self, path: &str, key: &str, value: &[u8]) -> Self {
        self.attrs
            .entry(path.to_owned())
            .or_default()
            .push((key.to_owned(), value.to_vec()));
        self
    }

    /// Register a directory listing; each regular entry also receives
    /// synthetic file bytes of its listed size, so contents and listings
    /// stay one view.
    fn dir(mut self, path: &str, entries: Vec<FsEntry>) -> Self {
        for entry in &entries {
            if entry.kind == FileKind::Regular {
                let file_path = join(path, &entry.name);
                let size = usize::try_from(entry.size).expect("fixture sizes fit in usize");
                self.files.insert(file_path, vec![b'x'; size]);
            }
        }
        self.dirs.insert(path.to_owned(), entries);
        self
    }

    /// Register a symbolic link at `path` storing `target`. The link's own
    /// name is what `stat_kind` reports; nothing is resolved.
    fn link(mut self, path: &str, target: &str) -> Self {
        self.links.insert(path.to_owned(), target.to_owned());
        self
    }

    /// The target stored for the link at `path`, if any.
    fn link_target(&self, path: &str) -> Option<String> {
        self.links.get(path).cloned()
    }

    fn mode(mut self, path: &str, mode: u32) -> Self {
        self.modes.insert(path.to_owned(), mode);
        self
    }

    fn deny(mut self, path: &str) -> Self {
        self.denied.push(path.to_owned());
        self
    }

    fn deny_set(mut self, path: &str) -> Self {
        self.set_denied.push(path.to_owned());
        self
    }

    fn read_fails(mut self, path: &str, errno: Errno) -> Self {
        self.read_fail = Some((path.to_owned(), errno));
        self
    }

    fn write_fails(mut self, path: &str, errno: Errno) -> Self {
        self.write_fail = Some((path.to_owned(), errno));
        self
    }

    /// Mark the subtree under `prefix` as living on another volume, so a
    /// rename in or out of it reports the cross-device boundary.
    fn cross(mut self, prefix: &str) -> Self {
        self.cross_volume.push(prefix.to_owned());
        self
    }

    /// The volume `path` lives on: the marked prefix containing it, or
    /// the root volume.
    fn volume_of(&self, path: &str) -> &str {
        self.cross_volume
            .iter()
            .find(|prefix| path == *prefix || is_inside(prefix, path))
            .map_or("/", String::as_str)
    }

    /// Replace-or-add `entry` in the listing of `parent`.
    fn upsert_entry(&mut self, parent: &str, entry: FsEntry) {
        if let Some(list) = self.dirs.get_mut(parent) {
            list.retain(|e| e.name != entry.name);
            list.push(entry);
        }
    }

    /// Remove the entry named by `path` from its parent's listing.
    fn drop_entry(&mut self, path: &str) {
        let (parent, name) = (parent_of(path).to_owned(), leaf_name(path).to_owned());
        if let Some(list) = self.dirs.get_mut(&parent) {
            list.retain(|e| e.name != name);
        }
    }

    fn contents(&self, path: &str) -> Option<Vec<u8>> {
        self.files.get(path).cloned()
    }

    fn has_dir(&self, path: &str) -> bool {
        self.dirs.contains_key(path)
    }
}

impl Fs for FakeFs {
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, Errno> {
        self.reads.set(self.reads.get() + 1);
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        self.dirs.get(path).cloned().ok_or(Errno::NotFound)
    }

    fn volume_space(&mut self, _path: &str) -> Option<VolumeSpace> {
        self.space
    }

    fn list_volumes(&mut self) -> Vec<VolumeInfo> {
        self.volumes.clone()
    }

    fn stat_mode(&mut self, path: &str) -> Result<u32, Errno> {
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        self.modes.get(path).copied().ok_or(Errno::NotFound)
    }

    fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Errno> {
        if self.set_denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        if !self.modes.contains_key(path) {
            return Err(Errno::NotFound);
        }
        self.modes.insert(path.to_owned(), mode);
        self.set_modes.borrow_mut().push((path.to_owned(), mode));
        Ok(())
    }

    fn attr_list(&mut self, path: &str) -> Result<Vec<String>, Errno> {
        if self.attrs_unsupported {
            return Err(Errno::NotSupported);
        }
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        Ok(self
            .attrs
            .get(path)
            .map(|set| set.iter().map(|(key, _)| key.clone()).collect())
            .unwrap_or_default())
    }

    fn attr_get(&mut self, path: &str, key: &str) -> Result<Vec<u8>, Errno> {
        if self.attrs_unsupported {
            return Err(Errno::NotSupported);
        }
        self.attrs
            .get(path)
            .and_then(|set| set.iter().find(|(k, _)| k == key))
            .map(|(_, value)| value.clone())
            .ok_or(Errno::NoData)
    }

    fn attr_set(&mut self, path: &str, key: &str, value: &[u8]) -> Result<(), Errno> {
        if self.attrs_unsupported {
            return Err(Errno::NotSupported);
        }
        if self.set_denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        // The real kernel validates the shared key grammar; the fake pins
        // the split the editor relies on (a namespace dot must exist).
        if !key.contains('.') {
            return Err(Errno::OutOfRange);
        }
        let set = self.attrs.entry(path.to_owned()).or_default();
        match set.iter_mut().find(|(k, _)| k == key) {
            Some((_, stored)) => *stored = value.to_vec(),
            None => set.push((key.to_owned(), value.to_vec())),
        }
        Ok(())
    }

    fn attr_remove(&mut self, path: &str, key: &str) -> Result<(), Errno> {
        if self.attrs_unsupported {
            return Err(Errno::NotSupported);
        }
        if self.set_denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        let set = self.attrs.get_mut(path).ok_or(Errno::NoData)?;
        let before = set.len();
        set.retain(|(k, _)| k != key);
        if set.len() == before {
            return Err(Errno::NoData);
        }
        Ok(())
    }

    fn stat_kind(&mut self, path: &str) -> Result<FileKind, Errno> {
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        if self.dirs.contains_key(path) {
            return Ok(FileKind::Directory);
        }
        // The name as typed: a link describes itself, never its target.
        if self.links.contains_key(path) {
            return Ok(FileKind::Symlink);
        }
        if self.files.contains_key(path) {
            return Ok(FileKind::Regular);
        }
        Err(Errno::NotFound)
    }

    fn read_link(&mut self, path: &str) -> Result<String, Errno> {
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        self.links.get(path).cloned().ok_or(Errno::OutOfRange)
    }

    fn create_link(&mut self, target: &str, path: &str) -> Result<(), Errno> {
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        // A new link never replaces an existing name.
        if self.links.contains_key(path)
            || self.files.contains_key(path)
            || self.dirs.contains_key(path)
        {
            return Err(Errno::AlreadyExists);
        }
        self.links.insert(path.to_owned(), target.to_owned());
        self.upsert_entry(
            parent_of(path),
            FsEntry {
                name: leaf_name(path).to_owned(),
                kind: FileKind::Symlink,
                size: target.len() as u64,
                modified: Time64::UNIX_EPOCH,
            },
        );
        Ok(())
    }

    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if let Some((failing, errno)) = &self.read_fail {
            if failing == path {
                return Err(*errno);
            }
        }
        let bytes = self.files.get(path).ok_or(Errno::NotFound)?;
        let start = usize::try_from(offset).map_err(|_| Errno::LengthOutOfRange)?;
        if start >= bytes.len() {
            return Ok(0);
        }
        let take = (bytes.len() - start).min(buf.len());
        buf[..take].copy_from_slice(&bytes[start..start + take]);
        Ok(take)
    }

    fn create(&mut self, path: &str) -> Result<(), Errno> {
        if self.dirs.contains_key(path) {
            return Err(Errno::AlreadyExists);
        }
        self.files.insert(path.to_owned(), Vec::new());
        self.upsert_entry(
            parent_of(path),
            FsEntry {
                name: leaf_name(path).to_owned(),
                kind: FileKind::Regular,
                size: 0,
                modified: Time64::UNIX_EPOCH,
            },
        );
        Ok(())
    }

    fn write(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno> {
        if let Some((failing, errno)) = &self.write_fail {
            if failing == path {
                return Err(*errno);
            }
        }
        let start = usize::try_from(offset).map_err(|_| Errno::LengthOutOfRange)?;
        let file = self.files.get_mut(path).ok_or(Errno::NotFound)?;
        let end = start + bytes.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[start..end].copy_from_slice(bytes);
        let size = file.len() as u64;
        self.upsert_entry(
            parent_of(path),
            FsEntry {
                name: leaf_name(path).to_owned(),
                kind: FileKind::Regular,
                size,
                modified: Time64::UNIX_EPOCH,
            },
        );
        Ok(())
    }

    fn mkdir(&mut self, path: &str) -> Result<(), Errno> {
        if self.dirs.contains_key(path) || self.files.contains_key(path) {
            return Err(Errno::AlreadyExists);
        }
        if !self.dirs.contains_key(parent_of(path)) {
            return Err(Errno::NotFound);
        }
        self.dirs.insert(path.to_owned(), Vec::new());
        self.upsert_entry(
            parent_of(path),
            FsEntry {
                name: leaf_name(path).to_owned(),
                kind: FileKind::Directory,
                size: 0,
                modified: Time64::UNIX_EPOCH,
            },
        );
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), Errno> {
        // The name as typed: a link is unlinked as the leaf it is, and what
        // it names is untouched.
        let removed = self.links.remove(path).is_some() || self.files.remove(path).is_some();
        if !removed {
            return Err(Errno::NotFound);
        }
        self.drop_entry(path);
        Ok(())
    }

    fn remove_dir(&mut self, path: &str) -> Result<(), Errno> {
        match self.dirs.get(path) {
            None => return Err(Errno::NotFound),
            Some(entries) if !entries.is_empty() => return Err(Errno::NotEmpty),
            Some(_) => {}
        }
        self.dirs.remove(path);
        self.drop_entry(path);
        Ok(())
    }

    fn rename(&mut self, src: &str, dst: &str) -> Result<RenameOutcome, Errno> {
        if self.volume_of(src) != self.volume_of(dst) {
            return Ok(RenameOutcome::CrossDevice);
        }
        if self.dirs.contains_key(dst) {
            // The kernel refuses renaming onto an existing directory.
            return Err(Errno::AlreadyExists);
        }
        // A rename moves the *name*, so a link travels as the link it is and
        // what it points at is untouched. A leaf destination is replaced, as
        // the kernel's rename does.
        if let Some(target) = self.links.remove(src) {
            self.files.remove(dst);
            self.links.insert(dst.to_owned(), target.clone());
            self.drop_entry(src);
            self.drop_entry(dst);
            self.upsert_entry(
                parent_of(dst),
                FsEntry {
                    name: leaf_name(dst).to_owned(),
                    kind: FileKind::Symlink,
                    size: target.len() as u64,
                    modified: Time64::UNIX_EPOCH,
                },
            );
            return Ok(RenameOutcome::Renamed);
        }
        if let Some(bytes) = self.files.remove(src) {
            self.links.remove(dst);
            self.files.insert(dst.to_owned(), bytes.clone());
            self.drop_entry(src);
            self.upsert_entry(
                parent_of(dst),
                FsEntry {
                    name: leaf_name(dst).to_owned(),
                    kind: FileKind::Regular,
                    size: bytes.len() as u64,
                    modified: Time64::UNIX_EPOCH,
                },
            );
            return Ok(RenameOutcome::Renamed);
        }
        if self.dirs.contains_key(src) {
            // Re-key the directory and every descendant path in both maps.
            let prefix = alloc::format!("{}/", src.trim_end_matches('/'));
            let rekey = |path: &str| -> Option<String> {
                if path == src {
                    Some(dst.to_owned())
                } else {
                    path.strip_prefix(&prefix)
                        .map(|rest| alloc::format!("{}/{rest}", dst.trim_end_matches('/')))
                }
            };
            let dirs = core::mem::take(&mut self.dirs);
            self.dirs = dirs
                .into_iter()
                .map(|(path, list)| (rekey(&path).unwrap_or(path), list))
                .collect();
            let files = core::mem::take(&mut self.files);
            self.files = files
                .into_iter()
                .map(|(path, bytes)| (rekey(&path).unwrap_or(path), bytes))
                .collect();
            self.drop_entry(src);
            self.upsert_entry(
                parent_of(dst),
                FsEntry {
                    name: leaf_name(dst).to_owned(),
                    kind: FileKind::Directory,
                    size: 0,
                    modified: Time64::UNIX_EPOCH,
                },
            );
            return Ok(RenameOutcome::Renamed);
        }
        Err(Errno::NotFound)
    }
}

fn file(name: &str, size: u64, secs: i64) -> FsEntry {
    FsEntry {
        name: name.to_owned(),
        kind: FileKind::Regular,
        size,
        modified: Time64::from_secs(secs),
    }
}

fn dir(name: &str) -> FsEntry {
    FsEntry {
        name: name.to_owned(),
        kind: FileKind::Directory,
        size: 0,
        modified: Time64::UNIX_EPOCH,
    }
}

/// A symbolic-link *entry* named `name`, whose size is the length of the
/// target it stores — the only thing a link holds.
fn link_entry(name: &str, target: &str) -> FsEntry {
    FsEntry {
        name: name.to_owned(),
        kind: FileKind::Symlink,
        size: target.len() as u64,
        modified: Time64::UNIX_EPOCH,
    }
}

/// The standard fixture: `/` holds `docs/`, `.hidden/`, `b.txt`, `a.rs`,
/// `big`; `/docs` holds `guide.md`; `/.hidden` holds one file; `/locked`
/// is denied.
fn fixture() -> FakeFs {
    FakeFs::new()
        .dir(
            "/",
            vec![
                dir("docs"),
                dir(".hidden"),
                dir("locked"),
                file("b.txt", 10, 2_000),
                file("a.rs", 30, 1_000),
                file("big", 999, 3_000),
            ],
        )
        .dir("/docs", vec![file("guide.md", 5, 4_000)])
        .dir("/.hidden", vec![file("secret", 1, 0)])
        .deny("/locked")
        .mode("/", 0o755)
        .mode("/docs", 0o755)
        .mode("/a.rs", 0o644)
        .mode("/b.txt", 0o600)
        .deny_set("/b.txt")
}

fn model(fs: &mut FakeFs) -> Model {
    Model::new(fs, "/", String::from("keys: q quits")).expect("root lists")
}

/// The open prompt, asserted to be the mode editor.
fn mode_prompt(m: &Model) -> ModePrompt {
    match m.prompt.clone() {
        Some(Prompt::Mode(prompt)) => prompt,
        other => panic!("expected the mode prompt, got {other:?}"),
    }
}

/// The prompt line the given model paints — the last row of a rendered
/// window, which is where every prompt is drawn.
fn prompt_line(m: &Model) -> String {
    let mut window = Window::new(Pos::new(0, 0), Size::new(24, 90));
    render(m, &mut window);
    row_text(&window, 23)
}

/// The whole rendered screen of `m`, as text.
fn painted_text(m: &Model) -> String {
    let mut window = Window::new(Pos::new(0, 0), Size::new(24, 90));
    render(m, &mut window);
    (0..24)
        .map(|row| row_text(&window, row))
        .collect::<alloc::vec::Vec<_>>()
        .join("\n")
}

fn row_text(window: &Window, row: u16) -> String {
    let mut text = String::new();
    if let Some(cells) = window.buffer().row(row) {
        for cell in cells {
            text.push(cell.ch);
        }
    }
    while text.ends_with(' ') {
        text.pop();
    }
    text
}

// --- Model: laziness, sorting, hidden toggle, fail-closed ---------------

#[test]
fn startup_reads_only_the_root() {
    let mut fs = fixture();
    let m = model(&mut fs);
    // One listing: the root. No child directory has been read.
    assert_eq!(fs.reads.get(), 1);
    // The tree shows the root plus its (visible) child dirs, unexpanded.
    let rows = m.tree_rows();
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["/", "docs", "locked"]);
}

#[test]
fn expanding_a_node_reads_it_exactly_once() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.tree_cursor = 1; // "docs"
                       // Right folds the branch open, reading it lazily.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Right);
    // Root + the expansion read.
    assert_eq!(fs.reads.get(), 2);
    // Folding shut and open again does not re-read (children stay cached).
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Left);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Right);
    assert_eq!(fs.reads.get(), 2);
}

// --- Navigation: the XTree Gold window model ----------------------------

#[test]
fn enter_and_esc_move_between_the_windows() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    assert_eq!(m.pane, Pane::Tree);
    // Enter steps from the tree window into the file window.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.pane, Pane::Files);
    // Esc backs out to the tree window.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.pane, Pane::Tree);
    // Esc at the tree window (nothing to back out of) is a no-op.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.pane, Pane::Tree);
    // Tab toggles both ways.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    assert_eq!(m.pane, Pane::Files);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    assert_eq!(m.pane, Pane::Tree);
}

#[test]
fn right_left_and_plus_minus_fold_the_tree() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.tree_cursor = 1; // docs
    assert!(!m.tree_rows()[1].expanded);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('+'));
    assert!(m.tree_rows()[1].expanded);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('-'));
    assert!(!m.tree_rows()[1].expanded);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Right);
    assert!(m.tree_rows()[1].expanded);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Left);
    assert!(!m.tree_rows()[1].expanded);
}

#[test]
fn left_on_a_collapsed_child_steps_to_the_parent() {
    let mut fs = FakeFs::new()
        .dir("/", vec![dir("a")])
        .dir("/a", vec![dir("b"), file("f", 1, 100)])
        .dir("/a/b", Vec::new());
    let mut m = Model::new(&mut fs, "/", String::new()).expect("root lists");
    // Onto "a", fold it open, step onto its child "b".
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    assert_eq!(m.tree_cursor, 1);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Right); // expand a
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down); // onto b
    assert_eq!(m.tree_rows()[m.tree_cursor].name, "b");
    // "b" is a collapsed leaf, so Left jumps up to the parent "a".
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Left);
    assert_eq!(m.tree_rows()[m.tree_cursor].name, "a");
    assert_eq!(m.files_dir, "/a");
    // Left again folds "a" shut (it is expanded).
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Left);
    assert!(!m.tree_rows()[1].expanded);
}

#[test]
fn home_and_end_jump_to_the_file_window_edges() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    let last = m.visible_files().len() - 1;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::End);
    assert_eq!(m.file_cursor, last);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Home);
    assert_eq!(m.file_cursor, 0);
}

#[test]
fn tree_rows_carry_box_drawing_branches_and_fold_markers() {
    let mut fs = fixture();
    let m = model(&mut fs);
    let rows = m.tree_rows();
    // The root has no branch prefix and shows as expanded.
    assert_eq!(rows[0].name, "/");
    assert_eq!(rows[0].branch, "");
    assert_eq!(rows[0].fold, '-');
    // Children carry the ├─/└─ junctions; a collapsed dir shows `+`.
    let docs = rows.iter().find(|r| r.name == "docs").expect("docs");
    assert_eq!(docs.branch, "├─");
    assert_eq!(docs.fold, '+');
    let locked = rows.iter().find(|r| r.name == "locked").expect("locked");
    assert_eq!(locked.branch, "└─");
}

#[test]
fn sort_orders_cover_every_key_and_direction() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    let names = |m: &Model| -> Vec<String> {
        m.visible_files()
            .iter()
            .filter(|e| !e.kind.is_dir())
            .map(|e| e.name.clone())
            .collect()
    };
    // Default: name ascending (directories group first, files after).
    assert_eq!(names(&m), ["a.rs", "b.txt", "big"]);
    // Size ascending.
    m.sort_key = SortKey::Size;
    m.sort_files();
    assert_eq!(names(&m), ["b.txt", "a.rs", "big"]);
    // Modified descending.
    m.sort_key = SortKey::Modified;
    m.sort_desc = true;
    m.sort_files();
    assert_eq!(names(&m), ["big", "b.txt", "a.rs"]);
    // Extension ascending: extensionless first, then rs, then txt.
    m.sort_key = SortKey::Extension;
    m.sort_desc = false;
    m.sort_files();
    assert_eq!(names(&m), ["big", "a.rs", "b.txt"]);
}

#[test]
fn the_sort_menu_selects_keys_and_reverses() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    assert_eq!(m.overlay, Overlay::SortMenu);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    assert_eq!(m.overlay, Overlay::None);
    assert_eq!(m.sort_key, SortKey::Modified);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('r'));
    assert!(m.sort_desc);
    // Esc cancels without changing anything.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.sort_key, SortKey::Modified);
    assert!(m.sort_desc);
}

#[test]
fn hidden_entries_are_filtered_and_the_toggle_shows_them() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    assert!(m.tree_rows().iter().all(|r| r.name != ".hidden"));
    assert_eq!(m.visible_files().len(), 5);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('H'));
    assert!(m.tree_rows().iter().any(|r| r.name == ".hidden"));
    assert_eq!(m.visible_files().len(), 6);
}

#[test]
fn hiding_entries_clamps_the_file_cursor() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.show_hidden = true;
    m.pane = Pane::Files;
    m.file_cursor = m.visible_files().len() - 1;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('H'));
    assert!(m.file_cursor < m.visible_files().len());
}

#[test]
fn a_denied_directory_keeps_the_selection_and_surfaces_the_error() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Move down twice: onto "docs" (fine), then onto "locked" (denied).
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    assert_eq!(m.tree_cursor, 1);
    assert_eq!(m.files_dir, "/docs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    // The cursor snapped back and the previous listing survives.
    assert_eq!(m.tree_cursor, 1);
    assert_eq!(m.files_dir, "/docs");
    let message = m.message.clone().expect("error surfaced");
    assert!(message.contains("locked"), "message names the path");
    // Expanding the denied directory is refused the same way.
    m.tree_cursor = 2;
    m.tree_cursor = 1;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab); // pane switch is unaffected
    assert_eq!(m.pane, Pane::Files);
}

#[test]
fn file_pane_cursor_stays_within_bounds() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    for _ in 0..20 {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    }
    assert_eq!(m.file_cursor, m.visible_files().len() - 1);
    for _ in 0..20 {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Up);
    }
    assert_eq!(m.file_cursor, 0);
}

#[test]
fn entering_a_directory_from_the_file_pane_descends_both_panes() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    // The sorted file pane groups directories first: docs is index 0.
    m.file_cursor = 0;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.files_dir, "/docs");
    let rows = m.tree_rows();
    assert_eq!(rows[m.tree_cursor].path, "/docs");
}

// --- The mode editor (`m` inside the `a` attributes view) ------------------

#[test]
fn the_mode_prompt_opens_prefilled_edits_and_applies() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    // Sorted visible files: docs, locked, a.rs, b.txt, big — a.rs is index 2.
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    let prompt = mode_prompt(&m);
    assert_eq!(prompt.path, "/a.rs");
    assert_eq!(prompt.current, 0o644);
    assert_eq!(prompt.input, "644");
    // Rub the prefill out and type a new mode; a non-octal digit and a
    // fifth digit are refused at the prompt (the kernel could never
    // accept them).
    for _ in 0..3 {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Backspace);
    }
    for key in ['9', '6', '0', '0', '7', '7'] {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    assert_eq!(mode_prompt(&m).input, "6007");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Backspace);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.prompt, None);
    assert_eq!(
        fs.set_modes.borrow().as_slice(),
        &[(String::from("/a.rs"), 0o600)]
    );
    let message = m.message.clone().expect("success reported");
    assert!(message.contains("600"), "{message}");
}

#[test]
fn esc_cancels_the_mode_prompt_without_writing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('0'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.prompt, None);
    assert!(fs.set_modes.borrow().is_empty());
}

#[test]
fn a_refused_stat_opens_no_prompt() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane, cursor onto the denied directory row.
    m.tree_cursor = 2; // "locked"
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    assert_eq!(m.prompt, None);
    let message = m.message.clone().expect("denial surfaced");
    assert!(message.contains("locked"), "{message}");
}

#[test]
fn a_kernel_refusal_of_the_change_is_surfaced_and_nothing_applies() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 3; // b.txt — statable, but the change is denied
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    assert!(m.prompt.is_some());
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.prompt, None);
    assert!(fs.set_modes.borrow().is_empty());
    let message = m.message.clone().expect("denial surfaced");
    assert!(message.contains("PermissionDenied"), "{message}");
}

#[test]
fn an_emptied_prompt_applies_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    for _ in 0..4 {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Backspace);
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.prompt, None);
    assert!(fs.set_modes.borrow().is_empty());
    assert!(m.message.is_some(), "the empty apply is reported");
}

#[test]
fn the_tree_pane_edits_the_selected_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane (the default), cursor onto "docs".
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    let prompt = mode_prompt(&m);
    assert_eq!(prompt.path, "/docs");
    assert_eq!(prompt.current, 0o755);
}

#[test]
fn the_mode_prompt_appears_on_the_message_line() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 70));
    render(&m, &mut window);
    let line = row_text(&window, 5);
    assert!(line.starts_with("mode a.rs [644]: 644_"), "{line}");
}

// --- The attributes editor (`a`) ------------------------------------------

/// The open attributes view, for the editor tests.
fn attrs_view(m: &Model) -> &crate::model::AttrsView {
    assert_eq!(m.overlay, Overlay::Attrs);
    m.attrs.as_ref().expect("attributes view open")
}

#[test]
fn the_attributes_view_lists_mode_and_entries() {
    let mut fs = fixture().attr("/a.rs", "user.comment", b"hi");
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    let view = attrs_view(&m);
    assert_eq!(view.path, "/a.rs");
    assert_eq!(view.mode, 0o644);
    assert!(!view.unsupported);
    assert_eq!(
        view.entries,
        vec![(String::from("user.comment"), b"hi".to_vec())]
    );
    // Esc leaves the view with nothing changed.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.overlay, Overlay::None);
    assert_eq!(m.attrs, None);
}

#[test]
fn a_new_attribute_is_typed_as_key_value_and_applies() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    for key in "user.tag=red".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.prompt, None);
    // The view refreshed with the applied attribute.
    let view = attrs_view(&m);
    assert_eq!(
        view.entries,
        vec![(String::from("user.tag"), b"red".to_vec())]
    );
    let message = m.message.clone().expect("success reported");
    assert!(message.contains("user.tag"), "{message}");
}

#[test]
fn enter_edits_the_selected_attribute_prefilled() {
    let mut fs = fixture().attr("/a.rs", "user.comment", b"hi");
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    match &m.prompt {
        Some(Prompt::AttrEdit(edit)) => assert_eq!(edit.input, "user.comment=hi"),
        other => panic!("attr prompt expected, got {other:?}"),
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('!'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    let view = attrs_view(&m);
    assert_eq!(
        view.entries,
        vec![(String::from("user.comment"), b"hi!".to_vec())]
    );
}

#[test]
fn a_binary_value_prefills_the_key_alone() {
    let mut fs = fixture().attr("/a.rs", "user.blob", b"\xff\x00");
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // Bytes that cannot round-trip through the line are never offered
    // back lossily; only the key pre-fills.
    match &m.prompt {
        Some(Prompt::AttrEdit(edit)) => assert_eq!(edit.input, "user.blob="),
        other => panic!("attr prompt expected, got {other:?}"),
    }
}

#[test]
fn d_removes_the_selected_attribute() {
    let mut fs = fixture().attr("/a.rs", "user.comment", b"hi");
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    let view = attrs_view(&m);
    assert!(view.entries.is_empty());
    let message = m.message.clone().expect("removal reported");
    assert!(message.contains("user.comment"), "{message}");
}

#[test]
fn a_malformed_attribute_line_applies_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    for key in "nodot".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(attrs_view(&m).entries.is_empty());
    let message = m.message.clone().expect("the malformed line is reported");
    assert!(message.contains("key=value"), "{message}");
}

#[test]
fn a_kernel_refusal_of_an_attribute_write_is_surfaced() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 3; // b.txt — statable, but every change is denied
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    for key in "user.tag=red".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(attrs_view(&m).entries.is_empty());
    let message = m.message.clone().expect("denial surfaced");
    assert!(message.contains("PermissionDenied"), "{message}");
}

#[test]
fn a_backing_without_attributes_says_so() {
    let mut fs = fixture();
    fs.attrs_unsupported = true;
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    let view = attrs_view(&m);
    assert!(view.unsupported);
    assert!(view.entries.is_empty());
    // The editing key states the fact instead of opening a doomed prompt;
    // the mode editor still works on such a mount.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    assert_eq!(m.prompt, None);
    let message = m
        .message
        .clone()
        .expect("the unsupported backing is stated");
    assert!(message.contains("not supported"), "{message}");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    assert!(matches!(m.prompt, Some(Prompt::Mode(_))));
}

// --- Renderer: golden grids ---------------------------------------------

#[test]
fn the_frame_lays_out_panes_status_and_hints() {
    let mut fs = fixture();
    let m = model(&mut fs);
    // A grid tall enough for the header and both boxes (body >= 7).
    let mut window = Window::new(Pos::new(0, 0), Size::new(14, 60));
    render(&m, &mut window);
    // Row 0: the XTree disk-statistics header — path, free space, count.
    let header = row_text(&window, 0);
    assert!(header.starts_with(" Path: /"), "got {header}");
    assert!(header.contains("Avail 500 of 1000 bytes"), "got {header}");
    assert!(header.contains("5 items"), "got {header}");
    // The tree window sits in a titled box above the file window.
    assert!(row_text(&window, 1).contains("Directory Tree"));
    // Tree interior: the root, then its branch-connected children with the
    // XTree box-drawing junctions and the collapsed `+` fold marker.
    assert!(
        row_text(&window, 2).starts_with("│- /"),
        "{}",
        row_text(&window, 2)
    );
    assert!(
        row_text(&window, 3).starts_with("│├─+ docs"),
        "{}",
        row_text(&window, 3)
    );
    assert!(
        row_text(&window, 4).starts_with("│└─+ locked"),
        "{}",
        row_text(&window, 4)
    );
    // The file window is a second titled box below the tree.
    assert!(row_text(&window, 7).contains("Files"));
    let first_file = row_text(&window, 8);
    assert!(
        first_file.contains("docs") && first_file.contains("<dir>"),
        "{first_file}"
    );
    // Status band: the active window and the view state (path/free/count
    // ride the header, so the band never repeats them).
    let status = row_text(&window, 12);
    assert!(status.starts_with("Tree  sort name asc"), "got {status}");
    // Command menu: the tree window's XTree-style key list.
    let menu = row_text(&window, 13);
    assert!(
        menu.contains("expand") && menu.contains("files"),
        "got {menu}"
    );
}

#[test]
fn the_epoch_stamp_renders_as_absent_and_real_stamps_as_dates() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    // Tall enough that the file window shows several entries.
    let mut window = Window::new(Pos::new(0, 0), Size::new(16, 70));
    render(&m, &mut window);
    let grid: Vec<String> = (0..14).map(|r| row_text(&window, r)).collect();
    // The directory's file-window row carries `<dir>` and no stamp; the
    // tree also lists `docs`, so key on the `<dir>` marker to find the
    // file-window row.
    let docs_row = grid
        .iter()
        .find(|r| r.contains("docs") && r.contains("<dir>"))
        .expect("docs file row");
    // The stamp is the last content character before the box's right
    // border and its padding.
    assert!(
        docs_row.trim_end_matches(['│', ' ']).ends_with('-'),
        "{docs_row}"
    );
    let file_row = grid.iter().find(|r| r.contains("a.rs")).expect("file row");
    assert!(file_row.contains("1970-01-01 00:16"), "secs=1000 renders");
}

#[test]
fn the_help_overlay_replaces_the_panes() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('?'));
    assert_eq!(m.overlay, Overlay::Help);
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 40));
    render(&m, &mut window);
    assert!(row_text(&window, 1).starts_with("│keys: q quits"));
    // Any key dismisses it.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('x'));
    assert_eq!(m.overlay, Overlay::None);
}

#[test]
fn the_sort_menu_prompt_appears_on_the_message_line() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 70));
    render(&m, &mut window);
    assert!(row_text(&window, 5).starts_with("sort: n)ame"));
}

// --- File operations: mkdir and rename ------------------------------------

/// Type `text` into the open prompt, character by character.
fn type_text(m: &mut Model, fs: &mut dyn Fs, text: &str) {
    for ch in text.chars() {
        handle_event(m, fs, &mut decode(), &Event::Char(ch));
    }
}

#[test]
fn mkdir_creates_a_directory_in_the_listed_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('M'));
    type_text(&mut m, &mut fs, "new");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.prompt, None);
    assert!(fs.has_dir("/new"));
    // The refreshed panes show the new directory.
    assert!(m.files.iter().any(|e| e.name == "new"));
    assert!(m.tree_rows().iter().any(|r| r.path == "/new"));
}

#[test]
fn mkdir_refuses_an_invalid_name_and_esc_cancels() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('M'));
    type_text(&mut m, &mut fs, "a/b");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("invalid name"));
    assert!(!fs.has_dir("/a/b"));
    // Esc cancels an open prompt with nothing created.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('M'));
    type_text(&mut m, &mut fs, "x");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.prompt, None);
    assert!(!fs.has_dir("/x"));
}

#[test]
fn rename_moves_a_file_within_its_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('r'));
    // The prompt is prefilled with the current name; rub it out.
    for _ in 0.."a.rs".len() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Backspace);
    }
    type_text(&mut m, &mut fs, "z.rs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.contents("/a.rs"), None);
    assert_eq!(fs.contents("/z.rs"), Some(vec![b'x'; 30]));
    assert!(m.files.iter().any(|e| e.name == "z.rs"));
}

#[test]
fn rename_onto_an_existing_file_asks_first() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('r'));
    for _ in 0.."a.rs".len() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Backspace);
    }
    type_text(&mut m, &mut fs, "b.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // The rename paused on the existing target; skip leaves both intact.
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
}

#[test]
fn renaming_the_session_root_is_refused() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('r'));
    assert_eq!(m.prompt, None);
    assert!(m.message.clone().expect("refused").contains("session root"));
}

// --- File operations: delete -----------------------------------------------

#[test]
fn delete_removes_a_file_after_confirmation() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    assert!(matches!(m.prompt, Some(Prompt::ConfirmDelete(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('y'));
    assert_eq!(fs.contents("/a.rs"), None);
    assert!(!m.files.iter().any(|e| e.name == "a.rs"));
}

#[test]
fn delete_declined_changes_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    // Any key but `y` declines — `n`, Esc, or anything else.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    assert_eq!(m.prompt, None);
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn delete_removes_a_directory_and_its_contents() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane, cursor onto "docs" (which holds guide.md).
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('y'));
    assert!(!fs.has_dir("/docs"));
    assert_eq!(fs.contents("/docs/guide.md"), None);
    // The file pane climbed to the nearest surviving ancestor.
    assert_eq!(m.files_dir, "/");
    assert!(m.tree_rows().iter().all(|r| r.path != "/docs"));
}

#[test]
fn deleting_the_session_root_is_refused() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    assert_eq!(m.prompt, None);
    assert!(m.message.clone().expect("refused").contains("session root"));
}

// --- File operations: copy --------------------------------------------------

#[test]
fn copy_streams_a_file_to_a_new_absolute_destination() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big (999 bytes)
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/big2");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.contents("/big2"), Some(vec![b'x'; 999]));
    assert_eq!(fs.contents("/big"), Some(vec![b'x'; 999]));
    assert!(m.files.iter().any(|e| e.name == "big2" && e.size == 999));
}

#[test]
fn copy_into_an_existing_directory_lands_under_the_base_name() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    // A relative destination is joined onto the listed directory.
    type_text(&mut m, &mut fs, "docs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.contents("/docs/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn copy_reproduces_a_directory_tree() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane, cursor onto "docs".
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(fs.has_dir("/copy"));
    assert_eq!(fs.contents("/copy/guide.md"), Some(vec![b'x'; 5]));
    // The source is untouched.
    assert_eq!(fs.contents("/docs/guide.md"), Some(vec![b'x'; 5]));
}

#[test]
fn copy_onto_itself_and_into_its_own_subtree_are_refused_before_io() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/a.rs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("same entry"));
    // A directory into its own subtree.
    m.pane = Pane::Tree;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down); // onto docs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/docs/inner");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("into itself"));
    assert!(!fs.has_dir("/docs/inner"));
}

#[test]
fn a_bad_destination_spelling_is_refused_by_the_path_grammar() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    // An interior empty component fails the shared grammar.
    type_text(&mut m, &mut fs, "/x//y");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("bad path"));
}

// --- File operations: the overwrite question --------------------------------

#[test]
fn copy_over_an_existing_file_asks_and_overwrites_on_o() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big (999 bytes)
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    // Nothing was written while the question is open.
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('o'));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 999]));
}

#[test]
fn copy_over_an_existing_file_skips_on_s_and_cancels_on_c() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
    let message = m.message.clone().expect("summary");
    assert!(message.contains("skipped"), "{message}");
    // Cancel likewise leaves the target alone.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
    let message = m.message.clone().expect("summary");
    assert!(message.contains("cancelled"), "{message}");
}

#[test]
fn a_merging_directory_copy_asks_per_conflicting_file() {
    // Copying /src into /dst merges with the existing /dst/src, which
    // already holds a different one.txt.
    let mut fs = FakeFs::new()
        .dir("/", vec![dir("src"), dir("dst")])
        .dir("/src", vec![file("one.txt", 3, 0), file("two.txt", 4, 0)])
        .dir("/dst", vec![dir("src")])
        .dir("/dst/src", vec![file("one.txt", 9, 0)]);
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down); // onto dst
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down); // onto src
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/dst");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // Only the conflicting file pauses; skipping it still copies the rest.
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    assert_eq!(m.prompt, None);
    assert_eq!(fs.contents("/dst/src/one.txt"), Some(vec![b'x'; 9]));
    assert_eq!(fs.contents("/dst/src/two.txt"), Some(vec![b'x'; 4]));
}

// --- File operations: move ---------------------------------------------------

#[test]
fn move_renames_within_one_volume() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    type_text(&mut m, &mut fs, "/docs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.contents("/a.rs"), None);
    assert_eq!(fs.contents("/docs/a.rs"), Some(vec![b'x'; 30]));
    assert!(!m.files.iter().any(|e| e.name == "a.rs"));
}

#[test]
fn move_across_volumes_copies_then_removes_the_source() {
    // /vol is another volume, so the rename honestly reports the boundary
    // and the engine falls back to copy-then-remove.
    let mut fs = FakeFs::new()
        .dir("/", vec![dir("vol"), dir("src")])
        .dir("/vol", vec![])
        .dir("/src", vec![file("data", 7, 0)])
        .cross("/vol");
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down); // onto src
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    type_text(&mut m, &mut fs, "/vol");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(fs.has_dir("/vol/src"));
    assert_eq!(fs.contents("/vol/src/data"), Some(vec![b'x'; 7]));
    assert!(!fs.has_dir("/src"));
}

#[test]
fn a_cross_volume_move_skip_keeps_the_skipped_source() {
    // The conflicting file is skipped, so its source directory is
    // intentionally left holding it — never a failed operation.
    let mut fs = FakeFs::new()
        .dir("/", vec![dir("vol"), dir("src")])
        .dir("/vol", vec![dir("src")])
        .dir("/vol/src", vec![file("data", 9, 0)])
        .dir("/src", vec![file("data", 7, 0)])
        .cross("/vol");
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down); // onto src
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    type_text(&mut m, &mut fs, "/vol");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    // The target kept its bytes, the skipped source survives, and the
    // summary says one entry was skipped.
    assert_eq!(fs.contents("/vol/src/data"), Some(vec![b'x'; 9]));
    assert_eq!(fs.contents("/src/data"), Some(vec![b'x'; 7]));
    assert!(fs.has_dir("/src"));
    let message = m.message.clone().expect("summary");
    assert!(message.contains("skipped"), "{message}");
}

#[test]
fn moving_the_session_root_is_refused() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    assert_eq!(m.prompt, None);
    assert!(m.message.clone().expect("refused").contains("session root"));
}

// --- File operations: failure injection --------------------------------------

#[test]
fn a_read_failure_mid_copy_removes_the_partial_target() {
    let mut fs = fixture().read_fails("/big", Errno::PermissionDenied);
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    let message = m.message.clone().expect("failure surfaced");
    assert!(message.contains("PermissionDenied"), "{message}");
    // The half-written target was removed, and the pane shows no ghost.
    assert_eq!(fs.contents("/copy"), None);
    assert!(!m.files.iter().any(|e| e.name == "copy"));
}

#[test]
fn a_write_failure_mid_copy_removes_the_partial_target() {
    let mut fs = fixture().write_fails("/copy", Errno::NoSpace);
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    let message = m.message.clone().expect("failure surfaced");
    assert!(message.contains("NoSpace"), "{message}");
    assert_eq!(fs.contents("/copy"), None);
}

#[test]
fn the_overwrite_question_appears_on_the_message_line() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 70));
    render(&m, &mut window);
    let line = row_text(&window, 5);
    assert!(line.starts_with("/b.txt exists"), "{line}");
    assert!(line.contains("o)verwrite"), "{line}");
}

// --- End-to-end scripted session -----------------------------------------

#[test]
fn a_scripted_session_browses_sorts_and_quits() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    let script = [
        Event::Down,      // tree cursor onto docs
        Event::Enter,     // expand docs
        Event::Tab,       // focus the file pane
        Event::Down,      // move within /docs
        Event::Char('s'), // sort menu
        Event::Char('e'), // by extension
        Event::Char('H'), // show hidden
        Event::Char('?'), // help overlay
        Event::Esc,       // dismiss it
        Event::Char('q'), // quit
    ];
    for event in &script {
        handle_event(&mut m, &mut fs, &mut decode(), event);
    }
    assert!(m.quit);
    assert_eq!(m.files_dir, "/docs");
    assert_eq!(m.sort_key, SortKey::Extension);
    assert!(m.show_hidden);
    assert_eq!(m.overlay, Overlay::None);
    // The session read exactly the directories it visited: the root, then
    // `/docs` for the file pane and once more for its tree expansion.
    assert_eq!(fs.reads.get(), 3);
}

// --- S3: tagging, batch operations, and the bounded walks ----------------

/// Tag an already-listed entry directly (the key path is exercised by the
/// toggle tests; the batch tests need a specific tag order).
fn tag(m: &mut Model, path: &str, kind: FileKind, size: u64) {
    m.tags.insert(TagEntry {
        path: path.to_owned(),
        kind,
        size,
    });
}

#[test]
fn tag_toggles_and_reports_figures() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    // Visible, name-sorted, dirs first: docs locked a.rs b.txt big.
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    assert!(m.tags.contains("/a.rs"));
    // The cursor stepped down so repeated presses mark a run.
    assert_eq!(m.file_cursor, 3);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    assert!(m.tags.contains("/b.txt"));
    assert_eq!(m.tags.count(), 2);
    assert_eq!(m.tags.total_bytes(), 40);
    // Toggling a tagged entry untags it.
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    assert!(!m.tags.contains("/a.rs"));
    assert_eq!(m.tags.count(), 1);
}

#[test]
fn tags_from_the_tree_pane_are_refused() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Tree;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    assert!(m.tags.is_empty());
    assert!(m.message.is_some());
}

#[test]
fn glob_tagging_tags_matching_visible_entries() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('T'));
    for ch in "*.rs".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.tags.contains("/a.rs"));
    assert_eq!(m.tags.count(), 1);
    let message = m.message.clone().expect("a tagging report");
    assert!(message.contains("tagged 1 matching"), "got {message}");
}

#[test]
fn malformed_glob_reports_and_tags_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('T'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('['));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.tags.is_empty());
    let message = m.message.clone().expect("a refusal");
    assert!(message.starts_with("bad pattern"), "got {message}");
}

#[test]
fn invert_swaps_the_tagged_set() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t')); // tags a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('i'));
    assert!(!m.tags.contains("/a.rs"));
    assert!(m.tags.contains("/docs"));
    assert!(m.tags.contains("/locked"));
    assert!(m.tags.contains("/b.txt"));
    assert!(m.tags.contains("/big"));
    assert_eq!(m.tags.count(), 4);
}

#[test]
fn clear_drops_every_tag() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('C'));
    assert!(m.tags.is_empty());
}

#[test]
fn batch_delete_continues_past_a_failure_and_reports_it_in_order() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tag order fixes the batch (and report) order: b.txt, locked, a.rs.
    tag(&mut m, "/b.txt", FileKind::Regular, 10);
    tag(&mut m, "/locked", FileKind::Directory, 0);
    tag(&mut m, "/a.rs", FileKind::Regular, 30);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    assert!(matches!(
        m.prompt,
        Some(Prompt::ConfirmBatchDelete { count: 3 })
    ));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('y'));
    // The failed entry did not stop the later one.
    assert!(fs.contents("/b.txt").is_none());
    assert!(fs.contents("/a.rs").is_none());
    // The denied directory survives, still listed in the root.
    assert!(fs
        .dirs
        .get("/")
        .is_some_and(|list| list.iter().any(|e| e.name == "locked")));
    assert_eq!(
        m.message.as_deref(),
        Some("batch: deleted 2 of 3 tagged, 1 failed")
    );
    // The report overlay lists exactly the failure, in encounter order.
    assert_eq!(m.overlay, Overlay::Report);
    assert_eq!(
        m.report_lines,
        vec![String::from("/locked: PermissionDenied")]
    );
    // Succeeded entries are untagged; the failed one stays tagged.
    assert!(m.tags.contains("/locked"));
    assert_eq!(m.tags.count(), 1);
}

/// A fixture whose destination directory already holds one of the batch's
/// names, so a copy pauses on the overwrite question.
fn conflict_fixture() -> FakeFs {
    FakeFs::new()
        .dir(
            "/",
            vec![
                dir("docs"),
                file("b.txt", 10, 2_000),
                file("big", 999, 3_000),
            ],
        )
        .dir(
            "/docs",
            vec![file("guide.md", 5, 4_000), file("b.txt", 5, 1_000)],
        )
}

#[test]
fn batch_copy_skip_continues_with_the_rest() {
    let mut fs = conflict_fixture();
    let mut m = model(&mut fs);
    tag(&mut m, "/b.txt", FileKind::Regular, 10);
    tag(&mut m, "/big", FileKind::Regular, 999);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    for ch in "docs".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // The existing /docs/b.txt paused the batch on its question.
    assert!(matches!(m.prompt, Some(Prompt::BatchOverwrite(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('s'));
    assert_eq!(
        m.message.as_deref(),
        Some("batch: copied 2 of 2 tagged, 1 skipped")
    );
    // The skipped target is untouched; the rest copied.
    assert_eq!(fs.contents("/docs/b.txt").map(|b| b.len()), Some(5));
    assert_eq!(fs.contents("/docs/big").map(|b| b.len()), Some(999));
    // Both entries completed (the skip was inside one), so both untag.
    assert!(m.tags.is_empty());
}

#[test]
fn batch_cancel_drops_the_remaining_entries() {
    let mut fs = conflict_fixture();
    let mut m = model(&mut fs);
    tag(&mut m, "/b.txt", FileKind::Regular, 10);
    tag(&mut m, "/big", FileKind::Regular, 999);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    for ch in "docs".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::BatchOverwrite(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    let message = m.message.clone().expect("a batch report");
    assert!(message.ends_with("(cancelled)"), "got {message}");
    // The cancelled entry is reported; the remainder never ran.
    assert_eq!(m.report_lines, vec![String::from("/b.txt: cancelled")]);
    assert!(fs.contents("/docs/big").is_none());
    // Nothing succeeded, so both entries stay tagged for a retry.
    assert_eq!(m.tags.count(), 2);
}

#[test]
fn batch_destination_must_be_an_existing_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    tag(&mut m, "/a.rs", FileKind::Regular, 30);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    for ch in "b.txt".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(
        m.message.as_deref(),
        Some("batch destination must be an existing directory")
    );
    assert_eq!(fs.contents("/b.txt").map(|b| b.len()), Some(10));
}

#[test]
fn walker_reads_at_most_the_tick_budget() {
    let mut fs = fixture();
    let mut walker = Walker::new("/");
    let found = walker.tick(&mut fs, 1);
    // One tick, one directory read — the budget bounds the pass.
    assert_eq!(fs.reads.get(), 1);
    // The root's regular files, in name order.
    let names: Vec<&str> = found.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(names, ["/a.rs", "/b.txt", "/big"]);
    assert!(walker.more());
    let _ = walker.tick(&mut fs, 10);
    assert!(!walker.more());
    // The denied directory is recorded, never silently dropped.
    assert_eq!(
        walker.errors,
        vec![String::from("/locked: PermissionDenied")]
    );
    assert_eq!(walker.files_seen, 5);
    assert_eq!(walker.bytes, 1_045);
    assert_eq!(walker.dirs_seen, 3);
}

/// A root of twenty empty subdirectories, so one bounded tick cannot
/// finish the walk.
fn wide_fixture() -> FakeFs {
    let mut entries = Vec::new();
    let mut fs = FakeFs::new();
    for index in 0..20_u32 {
        let name = format!("d{index:02}");
        entries.push(dir(&name));
        fs = fs.dir(&format!("/{name}"), vec![]);
    }
    fs.dir("/", entries)
}

#[test]
fn usage_walk_ticks_bounded_and_esc_cancels() {
    let mut fs = wide_fixture();
    let mut m = model(&mut fs);
    let before = fs.reads.get();
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('u'));
    assert!(matches!(
        m.walk,
        Some(WalkState {
            purpose: WalkPurpose::Usage,
            ..
        })
    ));
    walk_tick(&mut m, &mut fs);
    // One tick reads at most its directory budget.
    assert_eq!(fs.reads.get() - before, 16);
    assert!(m.walk.is_some());
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert!(m.walk.is_none());
    let message = m.message.clone().expect("a cancellation report");
    assert!(message.contains("cancelled"), "got {message}");
}

#[test]
fn usage_walk_completes_with_figures_and_reports_unreadable() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('u'));
    let mut guard = 0;
    while m.walk.is_some() {
        walk_tick(&mut m, &mut fs);
        guard += 1;
        assert!(guard < 10, "the walk must finish");
    }
    assert_eq!(
        m.message.as_deref(),
        Some("usage of /: 5 files, 1045 bytes, 3 dirs, 1 unreadable")
    );
    assert_eq!(m.overlay, Overlay::Report);
    assert_eq!(
        m.report_lines,
        vec![String::from("/locked: PermissionDenied")]
    );
}

#[test]
fn flat_walk_paginates_and_resumes() {
    let mut fs = fixture();
    let mut walk = WalkState::new("/", WalkPurpose::Flat, 2);
    walk.tick(&mut fs, 1, usize::MAX);
    // The page boundary paused the walk with directories still pending.
    assert!(walk.paused);
    assert!(!walk.done);
    assert!(!walk.ticking());
    walk.resume();
    walk.tick(&mut fs, 10, usize::MAX);
    assert!(walk.done);
    assert_eq!(walk.entries.len(), 5);
}

#[test]
fn flat_view_tags_rows_and_esc_returns_to_the_panes() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('v'));
    assert_eq!(m.view, View::Flat);
    let mut guard = 0;
    while m.walk.as_ref().is_some_and(WalkState::ticking) {
        walk_tick(&mut m, &mut fs);
        guard += 1;
        assert!(guard < 10, "the walk must finish");
    }
    // The first found file is the root's first regular entry.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    assert!(m.tags.contains("/a.rs"));
    assert_eq!(m.flat_cursor, 1);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
    // Tags survive leaving the view.
    assert_eq!(m.tags.count(), 1);
}

#[test]
fn status_line_shows_the_tag_figures() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(10, 70));
    render(&m, &mut window);
    let status = row_text(&window, 8);
    assert!(status.contains("2 tagged (40 bytes)"), "got {status}");
}

// --- S4: the live filename filter (`f`) ---------------------------------

/// Names of the visible file-pane entries.
fn visible_names(m: &Model) -> Vec<String> {
    m.visible_files().iter().map(|e| e.name.clone()).collect()
}

#[test]
fn the_filter_narrows_the_pane_as_typed_and_esc_restores() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('f'));
    type_text(&mut m, &mut fs, "*.rs");
    // The narrowing is live while the prompt is still open.
    assert!(m.prompt.is_some());
    assert_eq!(visible_names(&m), ["a.rs"]);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    // Esc restores the filter held before the prompt opened: none.
    assert!(m.filter.is_none());
    assert_eq!(
        visible_names(&m),
        ["docs", "locked", "a.rs", "b.txt", "big"]
    );
}

#[test]
fn enter_keeps_the_filter_and_the_status_line_shows_it() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('f'));
    type_text(&mut m, &mut fs, "*.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.prompt.is_none());
    assert_eq!(visible_names(&m), ["b.txt"]);
    assert_eq!(m.message.as_deref(), Some("filter *.txt: 1 shown"));
    let mut window = Window::new(Pos::new(0, 0), Size::new(10, 70));
    render(&m, &mut window);
    let status = row_text(&window, 8);
    assert!(status.contains("filter *.txt"), "got {status}");
    // Reopening pre-fills the pattern for editing; emptying it clears.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('f'));
    for _ in 0.."*.txt".len() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Backspace);
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.filter.is_none());
    assert_eq!(m.message.as_deref(), Some("filter cleared"));
}

#[test]
fn a_bad_filter_pattern_hides_nothing_and_says_so() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('f'));
    type_text(&mut m, &mut fs, "[a");
    // An uncompilable pattern filters nothing behind the user's back.
    assert_eq!(
        visible_names(&m),
        ["docs", "locked", "a.rs", "b.txt", "big"]
    );
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    let message = m.message.clone().expect("a bad-pattern report");
    assert!(message.contains("bad pattern"), "got {message}");
    let mut window = Window::new(Pos::new(0, 0), Size::new(10, 70));
    render(&m, &mut window);
    let status = row_text(&window, 8);
    assert!(status.contains("filter [a (bad pattern)"), "got {status}");
}

// --- S4: the branch filename search (`/`) --------------------------------

/// Drive the live walk to completion through the app's tick path.
fn finish_walk(m: &mut Model, fs: &mut FakeFs) {
    let mut guard = 0;
    while m.walk.as_ref().is_some_and(WalkState::ticking) {
        walk_tick(m, fs);
        guard += 1;
        assert!(guard < 100, "the walk must finish");
    }
}

#[test]
fn name_search_lists_matching_paths_below_the_branch() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "*.md");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.view, View::Flat);
    finish_walk(&mut m, &mut fs);
    let walk = m.walk.as_ref().expect("the search walk");
    let paths: Vec<&str> = walk.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["/docs/guide.md"]);
    // The denied directory is still recorded, never silently dropped.
    assert_eq!(walk.walker.errors, ["/locked: PermissionDenied"]);
    let mut window = Window::new(Pos::new(0, 0), Size::new(10, 70));
    render(&m, &mut window);
    let status = row_text(&window, 8);
    assert!(status.contains("search *.md"), "got {status}");
    assert!(status.contains("1 entries"), "got {status}");
}

#[test]
fn a_bad_search_pattern_is_refused_in_the_panes() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "[x");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
    let message = m.message.clone().expect("a bad-pattern report");
    assert!(message.contains("bad pattern"), "got {message}");
}

#[test]
fn flat_enter_jumps_to_the_hits_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "*.md");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    finish_walk(&mut m, &mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
    assert_eq!(m.pane, Pane::Files);
    assert_eq!(m.files_dir, "/docs");
    let files = m.visible_files();
    assert_eq!(files[m.file_cursor].name, "guide.md");
    // The tree cursor landed on the expanded hit directory.
    let rows = m.tree_rows();
    assert_eq!(rows[m.tree_cursor].path, "/docs");
}

// --- S4: the content search (`F`) ----------------------------------------

#[test]
fn needle_matching_is_case_insensitive_and_counts_overlaps() {
    let needle = Needle::new("aa").expect("a needle");
    assert_eq!(needle.count_in(b"AaAa"), 3);
    assert_eq!(needle.count_in(b"bbbb"), 0);
    assert_eq!(needle.count_in(b"a"), 0);
    assert!(Needle::new("").is_none());
}

#[test]
fn content_search_finds_the_needle_case_insensitively() {
    let mut fs = fixture();
    fs.files
        .insert("/a.rs".to_owned(), b"say Hello, World".to_vec());
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('F'));
    type_text(&mut m, &mut fs, "hello");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.view, View::Flat);
    finish_walk(&mut m, &mut fs);
    let walk = m.walk.as_ref().expect("the search walk");
    let rows: Vec<(&str, Option<&str>)> = walk
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.note.as_deref()))
        .collect();
    assert_eq!(rows, [("/a.rs", Some("1 match"))]);
}

#[test]
fn a_match_spanning_a_read_boundary_is_found() {
    let mut fs = FakeFs::new().dir("/", vec![file("data", 12, 0)]);
    fs.files
        .insert("/data".to_owned(), b"abcNEEdleFGh".to_vec());
    let needle = Needle::new("needle").expect("a needle");
    let mut scan = ContentScan::new(needle);
    scan.enqueue(vec![FlatEntry {
        path: String::from("/data"),
        kind: FileKind::Regular,
        size: 12,
        modified: Time64::UNIX_EPOCH,
        note: None,
    }]);
    // A byte budget of 4 splits the file into 4-byte reads, so the
    // occurrence spans two boundaries; the carried tail still finds it.
    let mut matched = Vec::new();
    let mut guard = 0;
    while scan.busy() {
        matched.extend(scan.tick(&mut fs, 4));
        guard += 1;
        assert!(guard < 100, "the scan must finish");
    }
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].note.as_deref(), Some("1 match"));
    assert_eq!(scan.scanned, 1);
}

#[test]
fn a_binary_file_is_reported_as_a_binary_match() {
    let mut fs = fixture();
    fs.files
        .insert("/big".to_owned(), b"\x00\x01key here, key there".to_vec());
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('F'));
    type_text(&mut m, &mut fs, "key");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    finish_walk(&mut m, &mut fs);
    let walk = m.walk.as_ref().expect("the search walk");
    let rows: Vec<(&str, Option<&str>)> = walk
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.note.as_deref()))
        .collect();
    assert_eq!(rows, [("/big", Some("binary, 2 matches"))]);
}

#[test]
fn content_search_over_the_tagged_set_scans_tagged_files_and_dirs() {
    let mut fs = fixture();
    fs.files
        .insert("/b.txt".to_owned(), b"the token lives here".to_vec());
    fs.files
        .insert("/docs/guide.md".to_owned(), b"token".to_vec());
    fs.files.insert("/a.rs".to_owned(), b"token too".to_vec());
    let mut m = model(&mut fs);
    m.tags.insert(TagEntry {
        path: String::from("/b.txt"),
        kind: FileKind::Regular,
        size: 10,
    });
    m.tags.insert(TagEntry {
        path: String::from("/docs"),
        kind: FileKind::Directory,
        size: 0,
    });
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('F'));
    type_text(&mut m, &mut fs, "token");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    finish_walk(&mut m, &mut fs);
    let walk = m.walk.as_ref().expect("the search walk");
    let paths: Vec<&str> = walk.entries.iter().map(|e| e.path.as_str()).collect();
    // The tagged file and the tagged directory's contents — never the
    // untagged /a.rs.
    assert_eq!(paths, ["/b.txt", "/docs/guide.md"]);
}

#[test]
fn a_read_refusal_during_the_scan_is_recorded_not_dropped() {
    let mut fs = fixture().read_fails("/big", Errno::PermissionDenied);
    fs.files.insert("/a.rs".to_owned(), b"needle".to_vec());
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('F'));
    type_text(&mut m, &mut fs, "needle");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    finish_walk(&mut m, &mut fs);
    let walk = m.walk.as_ref().expect("the search walk");
    let paths: Vec<&str> = walk.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["/a.rs"]);
    assert!(
        walk.walker
            .errors
            .iter()
            .any(|line| line == "/big: PermissionDenied"),
        "got {:?}",
        walk.walker.errors
    );
}

#[test]
fn esc_stops_a_live_search_keeping_results_and_esc_again_exits() {
    let mut fs = fixture();
    fs.files.insert("/a.rs".to_owned(), b"needle".to_vec());
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('F'));
    type_text(&mut m, &mut fs, "needle");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.walk.as_ref().is_some_and(WalkState::ticking));
    // Esc while the search runs stops it in place…
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    let walk = m.walk.as_ref().expect("the stopped walk");
    assert!(walk.done);
    assert_eq!(m.view, View::Flat);
    let message = m.message.clone().expect("a stop report");
    assert!(message.contains("stopped"), "got {message}");
    // …and a second Esc leaves the view.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
}

#[test]
fn an_empty_needle_searches_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('F'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
    assert_eq!(m.message.as_deref(), Some("empty text — nothing searched"));
}

// --- The viewers (S5): text paging, hex dump, search, goto ----------------

/// A sparse read-only backing of `size` deterministic bytes, so a file
/// far beyond 4 GiB is pageable without materialising it: byte `i` is
/// `(i / 16) % 251`. Only `read` answers; everything else is refused.
struct SparseFs {
    size: u64,
}

impl Fs for SparseFs {
    fn list_dir(&mut self, _path: &str) -> Result<Vec<FsEntry>, Errno> {
        Err(Errno::NotFound)
    }

    fn volume_space(&mut self, _path: &str) -> Option<VolumeSpace> {
        None
    }

    fn list_volumes(&mut self) -> Vec<VolumeInfo> {
        Vec::new()
    }

    fn stat_mode(&mut self, _path: &str) -> Result<u32, Errno> {
        Err(Errno::NotFound)
    }

    fn set_mode(&mut self, _path: &str, _mode: u32) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn attr_list(&mut self, _path: &str) -> Result<Vec<String>, Errno> {
        Err(Errno::NotSupported)
    }

    fn attr_get(&mut self, _path: &str, _key: &str) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotSupported)
    }

    fn attr_set(&mut self, _path: &str, _key: &str, _value: &[u8]) -> Result<(), Errno> {
        Err(Errno::NotSupported)
    }

    fn attr_remove(&mut self, _path: &str, _key: &str) -> Result<(), Errno> {
        Err(Errno::NotSupported)
    }

    fn stat_kind(&mut self, _path: &str) -> Result<FileKind, Errno> {
        Ok(FileKind::Regular)
    }

    fn read_link(&mut self, _path: &str) -> Result<String, Errno> {
        // Every node in this fixture is a plain byte source, so nothing here
        // is a link to read.
        Err(Errno::OutOfRange)
    }

    fn create_link(&mut self, _target: &str, _path: &str) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn read(&mut self, _path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if offset >= self.size {
            return Ok(0);
        }
        let take = u64::try_from(buf.len()).map_or(usize::MAX, |len| {
            usize::try_from(len.min(self.size - offset)).unwrap_or(0)
        });
        for (i, slot) in buf[..take].iter_mut().enumerate() {
            *slot = (((offset + i as u64) / 16) % 251) as u8;
        }
        Ok(take)
    }

    fn create(&mut self, _path: &str) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn write(&mut self, _path: &str, _offset: u64, _bytes: &[u8]) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn mkdir(&mut self, _path: &str) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn remove_file(&mut self, _path: &str) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn remove_dir(&mut self, _path: &str) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn rename(&mut self, _src: &str, _dst: &str) -> Result<RenameOutcome, Errno> {
        Err(Errno::PermissionDenied)
    }
}

impl FakeFs {
    /// Give the file at `path` these exact bytes (and a matching listing
    /// entry), replacing the synthetic `x` fill.
    fn set_bytes(&mut self, path: &str, bytes: &[u8]) {
        let size = bytes.len() as u64;
        self.files.insert(path.to_owned(), bytes.to_vec());
        self.upsert_entry(
            parent_of(path),
            FsEntry {
                name: leaf_name(path).to_owned(),
                kind: FileKind::Regular,
                size,
                modified: Time64::UNIX_EPOCH,
            },
        );
    }
}

/// A fixture body of `count` numbered lines (`line number N\n`).
fn numbered_lines(count: usize) -> String {
    use core::fmt::Write as _;
    (1..=count).fold(String::new(), |mut body, i| {
        // Writing into a String cannot fail.
        let _ = writeln!(body, "line number {i}");
        body
    })
}

/// The open viewer, asserted to be the disassembly viewer.
fn disasm_view(m: &Model) -> &DisasmView {
    match &m.viewer {
        Some(Viewer::Disasm(view)) => view,
        other => panic!("expected the disassembly viewer, got {other:?}"),
    }
}

/// A minimal valid rxe image whose code segment holds four aarch64 `nop`
/// words placed after the tables, plus an 8-byte data segment.
fn rxe_with_aarch64_nops() -> Vec<u8> {
    let tables = (LoadHeader::WIRE_LEN + 2 * Segment::WIRE_LEN) as u64;
    let code = Segment {
        vaddr: RXE_PAGE_SIZE,
        file_offset: tables,
        file_size: 16,
        mem_size: RXE_PAGE_SIZE,
        permission: RxePermission::ReadExecute,
    };
    let data = Segment {
        vaddr: RXE_PAGE_SIZE * 2,
        file_offset: tables + 16,
        file_size: 8,
        mem_size: RXE_PAGE_SIZE,
        permission: RxePermission::ReadWrite,
    };
    let header = LoadHeader {
        magic: LOAD_MAGIC,
        abi_version: tairix_abi::ABI_VERSION_CURRENT,
        flags: LOAD_FLAG_PIE,
        segment_count: 2,
        needed_count: 0,
        entry: RXE_PAGE_SIZE,
        cfi_tag: [0xA5; 32],
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&header.to_le_bytes());
    bytes.extend_from_slice(&code.to_le_bytes());
    bytes.extend_from_slice(&data.to_le_bytes());
    for _ in 0..4 {
        bytes.extend_from_slice(&0xd503_201f_u32.to_le_bytes());
    }
    bytes.extend_from_slice(&[0u8; 8]);
    bytes
}

/// A wasm module holding one code section with one function body:
/// `i32.const 0`, `drop`, `end`.
fn wasm_with_one_body() -> Vec<u8> {
    let body = [0x00, 0x41, 0x00, 0x1A, 0x0B];
    let body_len = u8::try_from(body.len()).expect("a tiny fixture body");
    let mut bytes = b"\0asm\x01\0\0\0".to_vec();
    bytes.push(10); // code section id
    bytes.push(1 + 1 + body_len); // section size (LEB, small)
    bytes.push(1); // one body
    bytes.push(body_len); // body size
    bytes.extend_from_slice(&body);
    bytes
}

/// A signed manifest requesting the given capabilities.
fn manifest_with(ids: &[CapabilityId]) -> Vec<u8> {
    let header = ManifestHeader {
        magic: MANIFEST_MAGIC,
        abi_version: tairix_abi::ABI_VERSION_CURRENT,
        flags: 0,
        capability_count: u16::try_from(ids.len()).expect("test counts fit"),
        reserved0: 0,
        syscall_table_hash: [0x11; 32],
        signer_pubkey: [0x22; 32],
        signature: [0x33; 64],
    };
    let mut bytes = header.to_le_bytes().to_vec();
    for id in ids {
        bytes.extend_from_slice(&id.as_u16().to_le_bytes());
    }
    bytes
}

/// Open the focused file through the `o` open-as chooser, forcing the
/// disassembly viewer with the given ISA key for a raw fragment.
fn open_raw(m: &mut Model, fs: &mut FakeFs, name: &str, isa_key: char) {
    m.pane = Pane::Files;
    m.file_cursor = m
        .visible_files()
        .iter()
        .position(|e| e.name == name)
        .expect("the fixture lists the entry");
    handle_event(m, fs, &mut decode(), &Event::Char('o'));
    handle_event(m, fs, &mut decode(), &Event::Char('d'));
    assert!(
        matches!(m.prompt, Some(Prompt::IsaPick(_))),
        "expected the ISA chooser, got {:?}",
        m.prompt
    );
    handle_event(m, fs, &mut decode(), &Event::Char(isa_key));
}

/// Put the file-pane cursor on `name` and press Enter.
fn open_file(m: &mut Model, fs: &mut FakeFs, name: &str) {
    m.pane = Pane::Files;
    m.file_cursor = m
        .visible_files()
        .iter()
        .position(|e| e.name == name)
        .expect("the fixture lists the entry");
    handle_event(m, fs, &mut decode(), &Event::Enter);
}

/// The open viewer, asserted to be the text pager.
fn text_view(m: &Model) -> &crate::view_text::TextView {
    match &m.viewer {
        Some(Viewer::Text(view)) => view,
        other => panic!("expected the text viewer, got {other:?}"),
    }
}

/// The open viewer, asserted to be the hex dump.
fn hex_view(m: &Model) -> &crate::view_hex::HexView {
    match &m.viewer {
        Some(Viewer::Hex(view)) => view,
        other => panic!("expected the hex viewer, got {other:?}"),
    }
}

/// Drive the viewer's live scan to completion (bounded, so a broken scan
/// fails the test instead of hanging it).
fn run_viewer_job(m: &mut Model, fs: &mut dyn Fs) {
    for _ in 0..1_000 {
        let live = match &m.viewer {
            Some(Viewer::Text(view)) => view.ticking(),
            Some(Viewer::Hex(view)) => view.ticking(),
            Some(Viewer::Disasm(view)) => view.ticking(),
            None => false,
        };
        if !live {
            return;
        }
        viewer_tick(m, fs, &mut decode());
    }
    panic!("the viewer scan did not finish");
}

/// Discards every event (the loopback happy path logs nothing).
struct NullSink;

impl Sink for NullSink {
    fn write_event(&self, _event: &LogEvent<'_>) {}
}

/// A fresh sandboxed decode seam over the in-process loopback worker —
/// the genuine parser-sandbox request/reply path, no kernel required.
fn decode() -> impl Decode {
    ParserSandbox::new(LoopbackLauncher::new(|| DecodeService), NullSink)
}

#[test]
fn enter_auto_picks_text_for_text_and_hex_for_binary() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", b"fn main() {}\n");
    fs.set_bytes("/big", &[0x7f, b'E', b'L', b'F', 0, 1, 2, 3]);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    assert_eq!(m.view, View::Viewer);
    assert_eq!(text_view(&m).path, "/a.rs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('q'));
    assert_eq!(m.view, View::Panes);
    assert!(m.viewer.is_none());
    open_file(&mut m, &mut fs, "big");
    assert_eq!(hex_view(&m).path, "/big");
}

#[test]
fn a_refused_head_read_opens_nothing() {
    let mut fs = fixture().read_fails("/a.rs", Errno::PermissionDenied);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    assert_eq!(m.view, View::Panes);
    assert!(m.viewer.is_none());
    let message = m.message.clone().expect("a report");
    assert!(message.contains("PermissionDenied"), "got {message}");
}

#[test]
fn the_text_view_renders_pages_and_scrolls() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", b"alpha\nbravo\ncharlie\ndelta\necho\n");
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 40);
    let mut window = Window::new(Pos::new(0, 0), Size::new(7, 40));
    render(&m, &mut window);
    assert!(row_text(&window, 1).starts_with("│alpha"));
    assert!(row_text(&window, 2).starts_with("│bravo"));
    assert!(row_text(&window, 3).starts_with("│charlie"));
    // The status line names the file, the mode, and the place.
    let status = row_text(&window, 5);
    assert!(status.starts_with("/a.rs  text"), "got {status}");
    assert!(status.contains("line 1"), "got {status}");
    // One row down.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 40);
    render(&m, &mut window);
    assert!(row_text(&window, 1).starts_with("│bravo"));
    assert_eq!(text_view(&m).top.line, Some(1));
    // A page down, then Home returns to the top.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::PageDown);
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 40);
    assert_eq!(text_view(&m).top.line, Some(4));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Home);
    assert_eq!(text_view(&m).top.line, Some(0));
    // End lands the last page.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::End);
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 40);
    assert!(text_view(&m).at_end);
}

#[test]
fn control_bytes_and_invalid_utf8_render_visibly() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", b"a\x1b[31mb\xff\tc\r\n");
    let mut m = model(&mut fs);
    // The invalid byte makes the head sample non-text, so Enter picks
    // the hex dump; `t` overrides to the text pager.
    open_file(&mut m, &mut fs, "a.rs");
    hex_view(&m);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 40);
    let view = text_view(&m);
    // The escape byte is a visible dot, the invalid byte the replacement
    // character, the tab an 8-column stop, and the CR of CRLF dropped —
    // nothing reaches the grid as a raw escape.
    assert_eq!(view.rows[0].text, "a·[31mb\u{FFFD}        c");
}

#[test]
fn long_rows_wrap_and_the_toggle_truncates() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", b"0123456789ABCDEF0123\nshort\n");
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    refresh_viewer(&mut m, &mut fs, &mut decode(), 4, 10);
    let view = text_view(&m);
    assert_eq!(view.rows[0].text, "0123456789");
    assert_eq!(view.rows[1].text, "ABCDEF0123");
    assert_eq!(view.rows[2].text, "short");
    // Wrap off: one (truncated-at-render) row per file row.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('w'));
    refresh_viewer(&mut m, &mut fs, &mut decode(), 4, 10);
    let view = text_view(&m);
    assert_eq!(view.rows[0].text, "0123456789ABCDEF0123");
    assert_eq!(view.rows[1].text, "short");
}

#[test]
fn goto_line_scans_to_the_target_and_past_end_lands_last() {
    let mut fs = fixture();
    let body = numbered_lines(50);
    fs.set_bytes("/a.rs", body.as_bytes());
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "42");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).top.line, Some(41));
    assert_eq!(m.message.as_deref(), Some("line 42"));
    // A target past the last line lands on the last row instead.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "999");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).top.line, Some(49));
    // A non-number is refused with nothing moved.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "abc");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.message.as_deref(), Some("goto: not a line number"));
}

#[test]
fn text_search_finds_across_the_read_boundary_and_repeats() {
    let mut fs = fixture();
    // The needle straddles the 4096-byte read window: bytes 4090..4100.
    let mut body = vec![b'.'; 8192];
    for i in (80..8192).step_by(80) {
        body[i] = b'\n';
    }
    body[4090..4100].copy_from_slice(b"NEEDLEfour");
    body[6001..6007].copy_from_slice(b"needle");
    fs.set_bytes("/a.rs", &body);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "needle");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).last_hit, Some(4090), "case-insensitive hit");
    // `n` continues past the hit to the second occurrence.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).last_hit, Some(6001));
    // …and a further `n` reports not found, keeping the place.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(m.message.as_deref(), Some("not found"));
    assert_eq!(text_view(&m).last_hit, Some(6001));
}

#[test]
fn the_hex_view_renders_the_classic_dump() {
    let mut fs = fixture();
    let mut bytes: Vec<u8> = (0_u8..40).collect();
    bytes[0] = 0;
    fs.set_bytes("/big", &bytes);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "big");
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 80);
    let mut window = Window::new(Pos::new(0, 0), Size::new(7, 80));
    render(&m, &mut window);
    let first = row_text(&window, 1);
    assert!(
        first.starts_with("│00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f"),
        "got {first}"
    );
    assert!(first.contains("|................|"), "got {first}");
    // The 33..40 tail row is short: eight bytes, printable ASCII shown.
    let third = row_text(&window, 3);
    assert!(
        third.starts_with("│00000020  20 21 22 23 24 25 26 27"),
        "got {third}"
    );
    assert!(third.contains("| !\"#$%&'|"), "got {third}");
}

#[test]
fn hex_goto_accepts_decimal_and_hex_and_clamps() {
    let mut fs = fixture();
    fs.set_bytes("/big", &[0xAA; 100]);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "big");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "0x30");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(hex_view(&m).top, 0x30);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "70");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(hex_view(&m).top, 64, "row containing byte 70");
    // Beyond the end clamps to the last row.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "0xFFFF");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(hex_view(&m).top, 96);
    // Garbage is refused with nothing moved.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "0xZZ");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(hex_view(&m).top, 96);
    assert_eq!(
        m.message.as_deref(),
        Some("goto: not an offset (decimal or 0x-hex)")
    );
}

#[test]
fn hex_search_finds_bytes_and_text_across_the_window_boundary() {
    let mut fs = fixture();
    let mut bytes = vec![0_u8; 8192];
    bytes[4094..4098].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    bytes[5000..5004].copy_from_slice(b"Text");
    fs.set_bytes("/big", &bytes);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "big");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "0xdeadbeef");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(hex_view(&m).last_hit, Some(4094));
    assert_eq!(hex_view(&m).top, 4080, "the hit row is on screen");
    assert_eq!(m.message.as_deref(), Some("found at 0xffe"));
    // A text pattern matches case-insensitively.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "tEXT");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(hex_view(&m).last_hit, Some(5000));
    // An odd-length 0x spelling is refused before anything runs.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "0xabc");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(
        m.message.as_deref(),
        Some("search: text, or 0x followed by hex byte pairs")
    );
}

#[test]
fn a_file_past_4_gib_pages_with_full_64_bit_offsets() {
    let mut fs = SparseFs {
        size: 5 * 1024 * 1024 * 1024,
    };
    let mut m = model(&mut fixture());
    m.viewer = Some(Viewer::Hex(crate::view_hex::HexView::new(
        "/huge",
        5 * 1024 * 1024 * 1024,
    )));
    m.view = View::Viewer;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    type_text(&mut m, &mut fs, "0x123456780");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(hex_view(&m).top, 0x1_2345_6780);
    refresh_viewer(&mut m, &mut fs, &mut decode(), 2, 80);
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 80));
    render(&m, &mut window);
    // The offset column widens to the file's own 64-bit reach (nine hex
    // digits for a 5 GiB file).
    let first = row_text(&window, 1);
    assert!(first.starts_with("│123456780  "), "got {first}");
    // End lands the final row of the 5 GiB file.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::End);
    let expected_top = (5 * 1024 * 1024 * 1024_u64 - 16) - 16;
    assert_eq!(
        hex_view(&m).top,
        expected_top,
        "last row minus one page row"
    );
}

#[test]
fn the_views_switch_in_place_and_esc_leaves() {
    let mut fs = fixture();
    let body = numbered_lines(20);
    fs.set_bytes("/a.rs", body.as_bytes());
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    // Down to line 3 (offset 28), then to hex: the dump row containing
    // that offset (aligned down to 16).
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    assert_eq!(text_view(&m).top.offset, 28);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('x'));
    assert_eq!(hex_view(&m).top, 16);
    // Back to text: snapped to the start of the row containing the hex
    // top (line 2 at offset 14); goto re-anchors the line number.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('t'));
    assert_eq!(text_view(&m).top.offset, 14);
    assert_eq!(text_view(&m).top.line, None, "unknown until re-anchored");
    // Esc leaves the viewer for the panes.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.view, View::Panes);
    assert!(m.viewer.is_none());
}

#[test]
fn esc_stops_a_live_viewer_scan_before_leaving() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", &vec![b'a'; 4096]);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    type_text(&mut m, &mut fs, "zzz");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(text_view(&m).ticking());
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert!(!text_view(&m).ticking());
    assert_eq!(m.view, View::Viewer, "the first Esc only stops the scan");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.view, View::Panes);
}

#[test]
fn a_read_refused_mid_view_closes_the_viewer() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", b"alpha\nbravo\n");
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    fs.read_fail = Some((String::from("/a.rs"), Errno::PermissionDenied));
    refresh_viewer(&mut m, &mut fs, &mut decode(), 3, 40);
    assert_eq!(m.view, View::Panes);
    assert!(m.viewer.is_none());
    let message = m.message.clone().expect("a report");
    assert!(message.contains("PermissionDenied"), "got {message}");
}

// --- The disassembly viewer (S9) -------------------------------------------

#[test]
fn an_rxe_opens_the_summary_and_disassembles_after_an_isa_choice() {
    let mut fs = fixture();
    fs.set_bytes("/prog", &rxe_with_aarch64_nops());
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "prog");
    let view = disasm_view(&m);
    assert_eq!(view.pane, DisasmPane::Summary);
    let rows = view.summary_rows();
    assert!(rows[0].0.starts_with("format rxe"), "got {}", rows[0].0);
    assert!(
        rows.iter()
            .any(|(t, _)| t.contains("manifest: carried beside")),
        "the rxe summary states the manifest travels beside the image"
    );
    assert!(
        rows.iter().any(|(t, r)| r.is_some() && t.contains("code")),
        "a selectable code region row"
    );
    // Enter asks for the ISA — an rxe image names none — and aarch64
    // decodes the nop words.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::IsaPick(_))));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    refresh_viewer(&mut m, &mut fs, &mut decode(), 6, 60);
    let view = disasm_view(&m);
    assert_eq!(view.pane, DisasmPane::Code);
    assert!(view.rows[0].text.contains("nop"), "got {:?}", view.rows);
    assert!(view.at_end, "four instructions fit one window");
    // Esc returns to the summary page; a second Esc leaves the viewer.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(disasm_view(&m).pane, DisasmPane::Summary);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.view, View::Panes);
}

#[test]
fn the_code_pane_switches_to_hex_at_the_same_file_bytes() {
    let mut fs = fixture();
    fs.set_bytes("/prog", &rxe_with_aarch64_nops());
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "prog");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    let expected = LoadHeader::WIRE_LEN + 2 * Segment::WIRE_LEN;
    assert_eq!(disasm_view(&m).switch_offset(), expected as u64);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('x'));
    // The hex dump aligns its window down to its 16-byte rows.
    assert_eq!(hex_view(&m).top, (expected as u64) & !0xF);
}

#[test]
fn a_wasm_module_knows_its_isa_and_shows_its_ops() {
    let mut fs = fixture();
    fs.set_bytes("/mod.wasm", &wasm_with_one_body());
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "mod.wasm");
    let view = disasm_view(&m);
    let rows = view.summary_rows();
    assert!(rows[0].0.contains("isa wasm"), "got {}", rows[0].0);
    // The module names its ISA, so Enter opens the region directly.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    let view = disasm_view(&m);
    assert_eq!(view.pane, DisasmPane::Code);
    let text: Vec<&str> = view.rows.iter().map(|row| row.text.as_str()).collect();
    assert!(
        text.iter().any(|row| row.contains("i32.const")),
        "got {text:?}"
    );
    assert!(text.iter().any(|row| row.contains("end")), "got {text:?}");
}

#[test]
fn a_standalone_manifest_shows_its_requested_capabilities() {
    let mut fs = fixture();
    fs.set_bytes(
        "/prog.manifest",
        &manifest_with(&[CapabilityId::FS_ACCESS, CapabilityId::NET_RAW]),
    );
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "prog.manifest");
    let rows = disasm_view(&m).summary_rows();
    assert_eq!(rows[0].0, "signed manifest (RXM1)");
    assert!(
        rows.iter().any(|(t, _)| t.contains("CAP_FS_ACCESS")),
        "got {rows:?}"
    );
    assert!(
        rows.iter().any(|(t, _)| t.contains("CAP_NET_RAW")),
        "got {rows:?}"
    );
}

#[test]
fn a_malformed_container_falls_back_to_hex_with_a_notice() {
    let mut fs = fixture();
    let mut bytes = LOAD_MAGIC.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0xFF; 16]);
    fs.set_bytes("/broken", &bytes);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "broken");
    assert_eq!(hex_view(&m).path, "/broken");
    let message = m.message.clone().expect("a notice");
    assert!(message.contains("showing hex"), "got {message}");
}

#[test]
fn an_oversize_container_falls_back_to_hex_without_reading_it() {
    let mut fs = fixture();
    let mut bytes = vec![0u8; MAX_INPUT + 1];
    bytes[0..4].copy_from_slice(&LOAD_MAGIC.to_le_bytes());
    fs.set_bytes("/huge", &bytes);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "huge");
    assert_eq!(hex_view(&m).path, "/huge");
    let message = m.message.clone().expect("a notice");
    assert!(message.contains("too large"), "got {message}");
}

#[test]
fn raw_fragments_disassemble_per_chosen_isa() {
    let mut fs = fixture();
    fs.set_bytes("/blob", &[0x90, 0x90, 0xCC, 0x90]);
    let mut m = model(&mut fs);
    open_raw(&mut m, &mut fs, "blob", 'x');
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    let view = disasm_view(&m);
    assert_eq!(view.pane, DisasmPane::Code);
    assert!(view.rows[0].text.contains("nop"), "got {:?}", view.rows);
    assert!(view.rows[2].text.contains("int3"), "got {:?}", view.rows);
    // The same bytes at another ISA: `I` re-decodes in place.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('I'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('r'));
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    let view = disasm_view(&m);
    assert!(
        !view.rows.is_empty() && !view.rows[0].text.contains("int3"),
        "got {:?}",
        view.rows
    );
}

#[test]
fn paging_walks_region_bounds_and_scrolls_back() {
    let mut fs = fixture();
    fs.set_bytes("/blob", &[0x90; 64]);
    let mut m = model(&mut fs);
    open_raw(&mut m, &mut fs, "blob", 'x');
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    assert_eq!(disasm_view(&m).place.top, 0);
    // A page down advances exactly one screenful of instructions.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::PageDown);
    assert_eq!(disasm_view(&m).place.top, 8);
    // One line up re-synchronises to the previous instruction.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Up);
    assert_eq!(disasm_view(&m).place.top, 7);
    // Up at the region start stays put.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Home);
    assert_eq!(disasm_view(&m).place.top, 0);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Up);
    assert_eq!(disasm_view(&m).place.top, 0);
    // End walks (in the background) to the region's final page.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::End);
    run_viewer_job(&mut m, &mut fs);
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    let view = disasm_view(&m);
    assert_eq!(view.place.top, 56, "the final page starts a screen back");
    assert!(view.at_end);
    // A page down at the end cannot pass the last instruction.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::PageDown);
    assert_eq!(disasm_view(&m).place.top, 63);
}

#[test]
fn the_code_pane_searches_instruction_text_and_jumps_to_addresses() {
    let mut fs = fixture();
    let mut bytes = vec![0x90u8; 32];
    bytes[16] = 0xCC;
    fs.set_bytes("/blob", &bytes);
    let mut m = model(&mut fs);
    open_raw(&mut m, &mut fs, "blob", 'x');
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    // `/` asks for instruction text; the walk lands on the hit.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('/'));
    for key in "int3".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    let view = disasm_view(&m);
    assert_eq!(view.place.top, 16);
    assert_eq!(view.last_hit, Some(16));
    // `n` finds no second hit and says so.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('n'));
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(m.message.as_deref(), Some("not found"));
    // `g` jumps to a typed address.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    for key in "0x8".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(disasm_view(&m).place.top, 8);
    // An address outside every code region is refused with a message.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('g'));
    for key in "0x999".chars() {
        handle_event(&mut m, &mut fs, &mut decode(), &Event::Char(key));
    }
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    let message = m.message.clone().expect("a refusal");
    assert!(message.contains("no code region"), "got {message}");
}

#[test]
fn symbols_label_rows_and_branch_targets() {
    let mut fs = fixture();
    // aarch64: `bl #+8` then three nops; `main` covers the region.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x9400_0002_u32.to_le_bytes());
    for _ in 0..3 {
        bytes.extend_from_slice(&0xd503_201f_u32.to_le_bytes());
    }
    fs.set_bytes("/blob", &bytes);
    let mut m = model(&mut fs);
    open_raw(&mut m, &mut fs, "blob", 'a');
    if let Some(Viewer::Disasm(view)) = &mut m.viewer {
        view.symbols = vec![SymbolRecord {
            name: String::from("main"),
            addr: 0,
            size: 16,
        }];
    }
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    let view = disasm_view(&m);
    assert!(view.rows[0].is_label, "got {:?}", view.rows);
    assert!(view.rows[0].text.contains("<main>:"), "got {:?}", view.rows);
    assert!(!view.rows[1].is_label);
    assert!(
        view.rows[1].text.contains("<main+0x8>"),
        "the branch target symbolises: got {:?}",
        view.rows
    );
}

#[test]
fn the_disasm_pages_render_inside_the_frame() {
    let mut fs = fixture();
    fs.set_bytes("/prog", &rxe_with_aarch64_nops());
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "prog");
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    let mut window = Window::new(Pos::new(0, 0), Size::new(12, 60));
    render(&m, &mut window);
    assert!(row_text(&window, 1).contains("format rxe"));
    let status = row_text(&window, 10);
    assert!(status.contains("disasm"), "got {status}");
    // Into the code pane: instruction rows and the code status line.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('a'));
    refresh_viewer(&mut m, &mut fs, &mut decode(), 8, 60);
    render(&m, &mut window);
    assert!(row_text(&window, 1).contains("nop"));
    let status = row_text(&window, 10);
    assert!(status.contains("aarch64"), "got {status}");
    assert!(status.contains("address 0x1000"), "got {status}");
}

// --- S10: volumes, settings, range tagging, repeat, completion, stdinfo --

/// A two-volume report: the fixture root plus a second mounted volume.
fn volumed_fixture() -> FakeFs {
    fixture()
        .dir("/data", vec![file("d.log", 7, 5_000)])
        .volumes(vec![
            VolumeInfo {
                target: String::from("/"),
                fstype: String::from("arxfs"),
                space: Some(VolumeSpace {
                    free_bytes: 500,
                    total_bytes: 1000,
                }),
            },
            VolumeInfo {
                target: String::from("/data"),
                fstype: String::from("ext4"),
                space: None,
            },
        ])
}

#[test]
fn the_volume_list_opens_navigates_and_reroots_the_session() {
    let mut fs = volumed_fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('V'));
    assert_eq!(m.overlay, Overlay::Volumes);
    assert_eq!(m.volumes.len(), 2);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    assert_eq!(m.volume_cursor, 1);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.overlay, Overlay::None);
    assert_eq!(m.root.path, "/data");
    assert_eq!(m.files_dir, "/data");
    assert_eq!(m.files.len(), 1);
    assert_eq!(m.tree_cursor, 0);
}

#[test]
fn a_refused_volume_root_keeps_the_session_and_the_list() {
    let mut fs = volumed_fixture();
    fs.volumes[1].target = String::from("/locked");
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('V'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Down);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.overlay, Overlay::Volumes, "the list stays open");
    assert_eq!(m.root.path, "/", "the session keeps its root");
    assert!(m.message.as_deref().unwrap_or("").contains("/locked"));
}

#[test]
fn an_empty_volume_report_is_a_message_not_a_screen() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('V'));
    assert_eq!(m.overlay, Overlay::None);
    assert_eq!(m.message.as_deref(), Some("no volumes reported"));
}

#[test]
fn the_volume_list_renders_targets_types_and_space() {
    let mut fs = volumed_fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('V'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(12, 60));
    render(&m, &mut window);
    assert!(row_text(&window, 0).contains("Volumes"));
    assert!(row_text(&window, 1).contains("arxfs"));
    assert!(row_text(&window, 1).contains("free 500/1000"));
    let second = row_text(&window, 2);
    assert!(second.contains("/data") && second.contains("free -"));
}

// --- Settings -------------------------------------------------------------

/// A fixture with a user home whose fixed `Settings/` shape exists.
fn homed_fixture() -> FakeFs {
    fixture()
        .dir("/Users", vec![dir("ada")])
        .dir("/Users/ada", vec![dir("Settings")])
        .dir("/Users/ada/Settings", vec![])
}

#[test]
fn settings_parse_fails_safe_and_encodes_round_trip() {
    // Defaults keep every confirmation on.
    assert_eq!(Settings::parse(""), Settings::default());
    // Garbage, unknown keys, and bad values leave the defaults standing.
    let garbled = Settings::parse("nonsense\nconfirm-delete=maybe\nyes=off\n");
    assert_eq!(garbled, Settings::default());
    // An explicit off turns exactly that confirmation off, and the
    // encoded form parses back to the same settings.
    let settings = Settings::parse("confirm-delete=off\nconfirm-batch-delete=on\n");
    assert!(!settings.confirm_delete && settings.confirm_batch_delete);
    assert_eq!(Settings::parse(&settings.encode()), settings);
}

#[test]
fn settings_toggle_persists_and_reloads() {
    let mut fs = homed_fixture();
    let mut m = model(&mut fs);
    m.settings_home = Some(String::from("/Users/ada"));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('S'));
    assert_eq!(m.overlay, Overlay::Settings);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('1'));
    assert!(!m.settings.confirm_delete);
    assert_eq!(m.message.as_deref(), Some("settings saved"));
    let stored = fs
        .contents(&config_path("/Users/ada"))
        .expect("the config file was written");
    let reloaded = Settings::parse(core::str::from_utf8(&stored).expect("utf-8"));
    assert!(!reloaded.confirm_delete && reloaded.confirm_batch_delete);
    assert_eq!(Settings::load(&mut fs, "/Users/ada"), reloaded);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    assert_eq!(m.overlay, Overlay::None);
}

#[test]
fn a_toggle_without_a_home_stays_session_only_and_says_so() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('S'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('2'));
    assert!(!m.settings.confirm_batch_delete, "the change applies");
    assert!(m.message.as_deref().unwrap_or("").contains("session only"));
}

#[test]
fn confirm_delete_off_deletes_without_a_question() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.settings.confirm_delete = false;
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    assert_eq!(m.prompt, None, "no question was asked");
    assert!(fs.contents("/a.rs").is_none(), "the file is gone");
}

#[test]
fn confirm_batch_delete_off_runs_the_batch_without_a_question() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.settings.confirm_batch_delete = false;
    tag(&mut m, "/a.rs", FileKind::Regular, 30);
    tag(&mut m, "/b.txt", FileKind::Regular, 10);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    assert_eq!(m.prompt, None, "no question was asked");
    assert!(fs.contents("/a.rs").is_none() && fs.contents("/b.txt").is_none());
}

#[test]
fn the_settings_menu_renders_the_toggles() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.settings_home = Some(String::from("/Users/ada"));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('S'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(12, 60));
    render(&m, &mut window);
    assert!(row_text(&window, 0).contains("Settings"));
    assert!(row_text(&window, 1).contains("confirm delete") && row_text(&window, 1).contains("on"));
    assert!(row_text(&window, 2).contains("confirm batch delete"));
    assert!(row_text(&window, 4).contains("/Users/ada/Settings/fstree/"));
}

// --- Range tagging ---------------------------------------------------------

#[test]
fn range_parse_covers_sizes_dates_and_refusals() {
    // Not a range at all: the glob path handles it.
    assert_eq!(TagRange::parse("*.rs"), Ok(None));
    // Size bounds with binary suffixes, either side open.
    let range = TagRange::parse("size:1K..2M")
        .expect("parses")
        .expect("is a range");
    assert!(range.matches(1024, Time64::UNIX_EPOCH));
    assert!(range.matches(2 * 1024 * 1024, Time64::UNIX_EPOCH));
    assert!(!range.matches(1023, Time64::UNIX_EPOCH));
    assert!(!range.matches(2 * 1024 * 1024 + 1, Time64::UNIX_EPOCH));
    let open = TagRange::parse("size:..500")
        .expect("parses")
        .expect("is a range");
    assert!(open.matches(0, Time64::UNIX_EPOCH) && !open.matches(501, Time64::UNIX_EPOCH));
    // Malformed ranges are typed refusals, never globs.
    assert!(TagRange::parse("size:abc..").is_err());
    assert!(TagRange::parse("size:..").is_err());
    assert!(TagRange::parse("size:9..1").is_err());
    assert!(TagRange::parse("date:2024-02-30..").is_err());
    assert!(TagRange::parse("date:2024-06-02..2024-06-01").is_err());
    // A date range includes its end date and never matches the epoch
    // "no stamp" marker.
    let june = TagRange::parse("date:2024-06-01..2024-06-02")
        .expect("parses")
        .expect("is a range");
    let first = Time64::from_secs(19_875 * 86_400 + 60); // 2024-06-01 00:01
    let second_end = Time64::from_secs(19_877 * 86_400 - 1); // 2024-06-02 23:59:59
    let third = Time64::from_secs(19_877 * 86_400); // 2024-06-03 00:00
    assert!(june.matches(0, first));
    assert!(june.matches(0, second_end));
    assert!(!june.matches(0, third));
    assert!(!june.matches(0, Time64::UNIX_EPOCH));
}

#[test]
fn size_range_tagging_tags_matching_visible_entries() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('T'));
    type_text(&mut m, &mut fs, "size:20..");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // a.rs (30) and big (999) qualify; b.txt (10) does not.
    assert!(m.tags.contains("/a.rs") && m.tags.contains("/big"));
    assert!(!m.tags.contains("/b.txt"));
    assert_eq!(m.tags.count(), 2);
}

#[test]
fn date_range_tagging_tags_by_stamp_and_skips_the_epoch_marker() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.show_hidden = true;
    m.pane = Pane::Files;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('T'));
    // Every fixture stamp (1000..4000 secs) is on 1970-01-01; the
    // `.hidden` listing rides the tree, not the file pane, and directory
    // rows carry the epoch marker so they never date-match.
    type_text(&mut m, &mut fs, "date:1970-01-01..1970-01-01");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(m.tags.count(), 3, "a.rs, b.txt, big");
    assert!(!m.tags.contains("/docs"), "no fabricated dates for dirs");
}

#[test]
fn a_malformed_range_reports_and_tags_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('T'));
    type_text(&mut m, &mut fs, "size:1G..1K");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(m.tags.is_empty());
    assert!(m.message.as_deref().unwrap_or("").contains("wrong order"));
}

// --- Repeat (`.`) -----------------------------------------------------------

#[test]
fn dot_repeats_the_last_copy_on_the_new_selection() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/docs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(fs.contents("/docs/a.rs").is_some());
    assert_eq!(m.last_op, Some(RepeatOp::CopyInto(String::from("/docs"))));
    // `.` on the next file repeats the copy into the same directory.
    m.file_cursor = 3; // b.txt
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('.'));
    assert!(fs.contents("/docs/b.txt").is_some());
}

#[test]
fn dot_with_no_history_says_so() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('.'));
    assert_eq!(m.message.as_deref(), Some("nothing to repeat"));
}

#[test]
fn dot_repeats_a_delete_and_still_asks() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('y'));
    assert!(fs.contents("/a.rs").is_none());
    assert_eq!(m.last_op, Some(RepeatOp::Delete));
    // The repeat still asks: the confirmation setting is on.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('.'));
    assert!(matches!(m.prompt, Some(Prompt::ConfirmDelete(_))));
}

// --- Prompt-line completion --------------------------------------------------

#[test]
fn tab_completes_a_unique_destination_and_keeps_directories_open() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "do");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    match &m.prompt {
        Some(Prompt::Input(prompt)) => assert_eq!(prompt.input, "docs/"),
        other => panic!("prompt should stay open, got {other:?}"),
    }
    // The completed directory stays open for further typing; a second
    // Tab inside it completes its file.
    type_text(&mut m, &mut fs, "gui");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    match &m.prompt {
        Some(Prompt::Input(prompt)) => assert_eq!(prompt.input, "docs/guide.md"),
        other => panic!("prompt should stay open, got {other:?}"),
    }
}

#[test]
fn tab_extends_to_the_common_prefix_then_lists() {
    let mut fs = fixture().dir(
        "/notes",
        vec![file("notes.txt", 1, 0), file("notebook", 1, 0)],
    );
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/notes/n");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    match &m.prompt {
        Some(Prompt::Input(prompt)) => assert_eq!(prompt.input, "/notes/note"),
        other => panic!("prompt should stay open, got {other:?}"),
    }
    // Nothing further extends: the candidates are listed instead.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    let listed = m.message.clone().unwrap_or_default();
    assert!(listed.contains("notebook") && listed.contains("notes.txt"));
}

#[test]
fn tab_without_a_match_or_in_a_non_path_prompt_completes_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "zzz");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    assert_eq!(m.message.as_deref(), Some("no completion"));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Esc);
    // A rename prompt asks for a new name, not an existing path: Tab
    // changes nothing.
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('r'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Tab);
    match &m.prompt {
        Some(Prompt::Input(prompt)) => assert_eq!(prompt.input, "a.rs"),
        other => panic!("prompt should stay open, got {other:?}"),
    }
}

// --- The Standard Information Stream (fd 3) ---------------------------------

/// An [`Info`] capturing every record for assertion.
struct CapturedInfo {
    records: Vec<String>,
}

impl Info for CapturedInfo {
    fn info(&mut self, record: &[u8]) {
        self.records
            .push(String::from_utf8_lossy(record).into_owned());
    }
}

#[test]
fn hidden_omissions_are_reported_once_per_change() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    let mut info = CapturedInfo {
        records: Vec::new(),
    };
    let mut last = None;
    // The fixture root hides `.hidden`: one omission record.
    note_hidden_entries(&m, &mut info, &mut last);
    note_hidden_entries(&m, &mut info, &mut last);
    assert_eq!(
        info.records.len(),
        1,
        "one record per change, not per frame"
    );
    let record = &info.records[0];
    assert!(record.contains("\"kind\":\"omission\""));
    assert!(record.contains("fs.hidden_entries_omitted"));
    assert!(record.contains("\"omitted_count\":1"));
    assert!(record.ends_with('\n'));
    // Showing hidden entries omits nothing (and emits nothing new).
    m.show_hidden = true;
    note_hidden_entries(&m, &mut info, &mut last);
    assert_eq!(info.records.len(), 1);
    // Hiding them again is a change: reported once more.
    m.show_hidden = false;
    note_hidden_entries(&m, &mut info, &mut last);
    assert_eq!(info.records.len(), 2);
}

// --- The S10 end-to-end session ---------------------------------------------

/// One scripted session across the stage's surface: tag two files, batch
/// copy them, verify through the volume list, adjust a setting, and quit
/// — every step through the ordinary key loop.
#[test]
fn a_scripted_session_tags_copies_and_visits_the_new_surfaces() {
    let mut fs = volumed_fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    let script = [
        Event::Char('t'), // tag a.rs (cursor steps to b.txt)
        Event::Char('t'), // tag b.txt
        Event::Char('c'), // batch copy prompt
    ];
    for event in &script {
        handle_event(&mut m, &mut fs, &mut decode(), event);
    }
    type_text(&mut m, &mut fs, "/docs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(fs.contents("/docs/a.rs").is_some() && fs.contents("/docs/b.txt").is_some());
    let rest = [
        Event::Char('V'), // the volume list
        Event::Esc,       // close it
        Event::Char('S'), // the settings menu
        Event::Char('1'), // confirm-delete off (no home: session only)
        Event::Esc,       // close it
        Event::Char('q'), // quit
    ];
    for event in &rest {
        handle_event(&mut m, &mut fs, &mut decode(), event);
    }
    assert!(m.quit);
    assert!(!m.settings.confirm_delete);
    assert_eq!(m.overlay, Overlay::None);
}

// --- Session loop: the terminal changes size ----------------------------

/// A tty whose reported geometry changes as the session reads it, so
/// `Screen`'s own detection raises the resize the loop must handle.
struct ResizingTty {
    /// One `(bytes, geometry after this read)` step per read, in order.
    steps: Vec<(Vec<u8>, Size)>,
    size: Size,
    output: Vec<u8>,
}

impl ResizingTty {
    fn new(size: Size, steps: &[(&[u8], Size)]) -> ResizingTty {
        ResizingTty {
            steps: steps
                .iter()
                .rev()
                .map(|(bytes, size)| (bytes.to_vec(), *size))
                .collect(),
            size,
            output: Vec::new(),
        }
    }
}

impl tairix_curses::Tty for ResizingTty {
    fn write(&mut self, bytes: &[u8]) -> tairix_curses::Result<()> {
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    fn read(&mut self) -> tairix_curses::Result<Vec<u8>> {
        match self.steps.pop() {
            Some((bytes, size)) => {
                self.size = size;
                Ok(bytes)
            }
            None => Ok(Vec::new()),
        }
    }

    fn size(&mut self) -> tairix_curses::Result<Option<Size>> {
        Ok(Some(self.size))
    }
}

#[test]
fn the_session_resizes_its_window_and_repaints_at_the_new_size() {
    let entries: Vec<FsEntry> = (1..=12)
        .map(|i| file(&format!("row{i:03}"), 1, 1_000))
        .collect();
    let mut fs = FakeFs::new().dir("/", entries);
    let mut m = model(&mut fs);

    // An eight-row terminal shows a couple of file rows per pane, so
    // `row012` is off-screen. The first read grows the terminal; the
    // second quits.
    let small = Size::new(8, 40);
    let large = Size::new(44, 40);
    let tty = ResizingTty::new(small, &[(b"", large), (b"q", large)]);
    let mut screen =
        tairix_curses::Screen::new(tty, tairix_termcap::TermType::Xterm256Color, small);
    let code = crate::app::run(
        &mut m,
        &mut fs,
        &mut decode(),
        &mut screen,
        &mut crate::info::NullInfo,
    )
    .expect("session ends");
    assert_eq!(code, 0);

    let output = screen.into_tty().output;
    // `row012` only fits once the window has grown to match the screen;
    // a stale eight-row window could never have painted it.
    assert!(
        output.windows(6).any(|w| w == b"row012"),
        "the grown window repainted the whole listing"
    );
}

// --- File operations: symbolic links (`plans/SYMLINKS.md` S4) --------------

/// The link fixture: `/` holds a link to a file, one to a directory, and one
/// that names nothing, beside the ordinary entries a transfer targets.
fn link_fixture() -> FakeFs {
    FakeFs::new()
        .dir(
            "/",
            vec![
                dir("docs"),
                file("a.rs", 30, 1_000),
                file("b.txt", 10, 2_000),
                link_entry("to-file", "/a.rs"),
                link_entry("to-dir", "/docs"),
                link_entry("gone", "../removed"),
            ],
        )
        .dir("/docs", vec![file("guide.md", 5, 4_000)])
        .link("/to-file", "/a.rs")
        .link("/to-dir", "/docs")
        .link("/gone", "../removed")
        .mode("/", 0o755)
}

/// The index of the file-pane row named `name` in the model's listing.
fn row_of(m: &Model, name: &str) -> usize {
    m.files
        .iter()
        .position(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("the listing holds {name}"))
}

#[test]
fn copying_a_link_recreates_it_with_the_same_target() {
    let mut fs = link_fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "to-file");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy-of-link");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // A link, holding the same spelling — never a regular file holding the
    // target's text.
    assert_eq!(fs.stat_kind("/copy-of-link"), Ok(FileKind::Symlink));
    assert_eq!(fs.link_target("/copy-of-link").as_deref(), Some("/a.rs"));
    // Nothing was read or written through the link.
    assert_eq!(fs.contents("/copy-of-link"), None);
    // The source link and its target are untouched.
    assert_eq!(fs.stat_kind("/to-file"), Ok(FileKind::Symlink));
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn copying_a_dangling_link_still_recreates_it() {
    // A link is data: what it names has no bearing on duplicating it.
    let mut fs = link_fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "gone");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/still-gone");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.stat_kind("/still-gone"), Ok(FileKind::Symlink));
    assert_eq!(fs.link_target("/still-gone").as_deref(), Some("../removed"));
}

#[test]
fn copying_a_tree_recreates_the_links_inside_it() {
    let mut fs = FakeFs::new()
        .dir("/", vec![dir("tree"), file("a.rs", 30, 1_000)])
        .dir("/tree", vec![file("f", 3, 0), link_entry("inner", "/a.rs")])
        .link("/tree/inner", "/a.rs")
        .mode("/", 0o755);
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "tree");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/tree-copy");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.contents("/tree-copy/f"), Some(vec![b'x'; 3]));
    assert_eq!(fs.stat_kind("/tree-copy/inner"), Ok(FileKind::Symlink));
    assert_eq!(fs.link_target("/tree-copy/inner").as_deref(), Some("/a.rs"));
}

#[test]
fn deleting_a_link_removes_the_link_and_not_its_target() {
    let mut fs = link_fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "to-dir");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('d'));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('y'));
    assert_eq!(fs.stat_kind("/to-dir"), Err(Errno::NotFound));
    // The directory the link named is still there, with its contents.
    assert!(fs.has_dir("/docs"));
    assert_eq!(fs.contents("/docs/guide.md"), Some(vec![b'x'; 5]));
}

#[test]
fn moving_a_link_moves_the_link() {
    let mut fs = link_fixture().cross("/other").dir("/other", vec![]);
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "to-file");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('m'));
    // A cross-volume move cannot rename, so it recreates the link and
    // removes the original — still never touching the target.
    type_text(&mut m, &mut fs, "/other/moved");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert_eq!(fs.stat_kind("/other/moved"), Ok(FileKind::Symlink));
    assert_eq!(fs.link_target("/other/moved").as_deref(), Some("/a.rs"));
    assert_eq!(fs.stat_kind("/to-file"), Err(Errno::NotFound));
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn a_link_at_the_destination_is_asked_about_and_replaced_not_written_through() {
    let mut fs = link_fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "b.txt");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    type_text(&mut m, &mut fs, "/to-file");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    // The question is asked, and says what would be replaced.
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    assert!(
        prompt_line(&m).contains("symbolic link"),
        "{}",
        prompt_line(&m)
    );
    // Nothing has happened yet: the link stands and its target is intact.
    assert_eq!(fs.stat_kind("/to-file"), Ok(FileKind::Symlink));
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('o'));
    // The *link* was replaced by the copy; the file it named is untouched,
    // which is what "never written through" means.
    assert_eq!(fs.stat_kind("/to-file"), Ok(FileKind::Regular));
    assert_eq!(fs.contents("/to-file"), Some(vec![b'x'; 10]));
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn a_directory_is_never_transferred_onto_a_link() {
    let mut fs = link_fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = row_of(&m, "docs");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Char('c'));
    // A link is a leaf, so a directory cannot merge into it.
    type_text(&mut m, &mut fs, "/gone");
    handle_event(&mut m, &mut fs, &mut decode(), &Event::Enter);
    assert!(
        m.message
            .clone()
            .expect("refused")
            .contains("different kind"),
        "{:?}",
        m.message
    );
    assert_eq!(fs.stat_kind("/gone"), Ok(FileKind::Symlink));
}

#[test]
fn the_file_pane_shows_a_link_as_a_link() {
    let mut fs = link_fixture();
    let m = model(&mut fs);
    // The size column names what the row is; a link's own byte count is the
    // length of the path it stores, which is not a size a reader wants.
    let text = painted_text(&m);
    assert!(text.contains("<link>"), "{text}");
}
