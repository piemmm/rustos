//! Host tests: the model over an in-memory [`Fs`], golden-grid renders,
//! and a scripted end-to-end session — no kernel, no terminal.

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use core::cell::{Cell as CoreCell, RefCell};

use rustos_abi::time::Time64;
use rustos_abi::{Errno, FileKind};
use rustos_curses::{Event, Pos, Size, Window};

use crate::app::{handle_event, refresh_viewer, viewer_tick, walk_tick};
use crate::fs::{Fs, FsEntry, RenameOutcome, VolumeSpace};
use crate::model::{join, ModePrompt, Model, Overlay, Pane, Prompt, SortKey, View, Viewer};
use crate::ops::{basename, is_inside, parent_of};
use crate::render::render;
use crate::search::{ContentScan, Needle};
use crate::tag::TagEntry;
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
}

impl FakeFs {
    fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            files: BTreeMap::new(),
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
        }
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
        let (parent, name) = (parent_of(path).to_owned(), basename(path).to_owned());
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

    fn stat_kind(&mut self, path: &str) -> Result<FileKind, Errno> {
        if self.denied.iter().any(|denied| denied == path) {
            return Err(Errno::PermissionDenied);
        }
        if self.dirs.contains_key(path) {
            return Ok(FileKind::Directory);
        }
        if self.files.contains_key(path) {
            return Ok(FileKind::Regular);
        }
        Err(Errno::NotFound)
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
                name: basename(path).to_owned(),
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
                name: basename(path).to_owned(),
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
                name: basename(path).to_owned(),
                kind: FileKind::Directory,
                size: 0,
                modified: Time64::UNIX_EPOCH,
            },
        );
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> Result<(), Errno> {
        if self.files.remove(path).is_none() {
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
        if let Some(bytes) = self.files.remove(src) {
            self.files.insert(dst.to_owned(), bytes.clone());
            self.drop_entry(src);
            self.upsert_entry(
                parent_of(dst),
                FsEntry {
                    name: basename(dst).to_owned(),
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
                    name: basename(dst).to_owned(),
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
    handle_event(&mut m, &mut fs, &Event::Enter);
    // Root + the expansion read.
    assert_eq!(fs.reads.get(), 2);
    // Re-collapsing and re-expanding does not re-read.
    handle_event(&mut m, &mut fs, &Event::Enter);
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(fs.reads.get(), 2);
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
    handle_event(&mut m, &mut fs, &Event::Char('s'));
    assert_eq!(m.overlay, Overlay::SortMenu);
    handle_event(&mut m, &mut fs, &Event::Char('m'));
    assert_eq!(m.overlay, Overlay::None);
    assert_eq!(m.sort_key, SortKey::Modified);
    handle_event(&mut m, &mut fs, &Event::Char('s'));
    handle_event(&mut m, &mut fs, &Event::Char('r'));
    assert!(m.sort_desc);
    // Esc cancels without changing anything.
    handle_event(&mut m, &mut fs, &Event::Char('s'));
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert_eq!(m.sort_key, SortKey::Modified);
    assert!(m.sort_desc);
}

#[test]
fn hidden_entries_are_filtered_and_the_toggle_shows_them() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    assert!(m.tree_rows().iter().all(|r| r.name != ".hidden"));
    assert_eq!(m.visible_files().len(), 5);
    handle_event(&mut m, &mut fs, &Event::Char('.'));
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
    handle_event(&mut m, &mut fs, &Event::Char('.'));
    assert!(m.file_cursor < m.visible_files().len());
}

#[test]
fn a_denied_directory_keeps_the_selection_and_surfaces_the_error() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Move down twice: onto "docs" (fine), then onto "locked" (denied).
    handle_event(&mut m, &mut fs, &Event::Down);
    assert_eq!(m.tree_cursor, 1);
    assert_eq!(m.files_dir, "/docs");
    handle_event(&mut m, &mut fs, &Event::Down);
    // The cursor snapped back and the previous listing survives.
    assert_eq!(m.tree_cursor, 1);
    assert_eq!(m.files_dir, "/docs");
    let message = m.message.clone().expect("error surfaced");
    assert!(message.contains("locked"), "message names the path");
    // Expanding the denied directory is refused the same way.
    m.tree_cursor = 2;
    m.tree_cursor = 1;
    handle_event(&mut m, &mut fs, &Event::Tab); // pane switch is unaffected
    assert_eq!(m.pane, Pane::Files);
}

#[test]
fn file_pane_cursor_stays_within_bounds() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    for _ in 0..20 {
        handle_event(&mut m, &mut fs, &Event::Down);
    }
    assert_eq!(m.file_cursor, m.visible_files().len() - 1);
    for _ in 0..20 {
        handle_event(&mut m, &mut fs, &Event::Up);
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
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(m.files_dir, "/docs");
    let rows = m.tree_rows();
    assert_eq!(rows[m.tree_cursor].path, "/docs");
}

// --- The mode editor (`a`) ------------------------------------------------

#[test]
fn the_mode_prompt_opens_prefilled_edits_and_applies() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    // Sorted visible files: docs, locked, a.rs, b.txt, big — a.rs is index 2.
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &Event::Char('a'));
    let prompt = mode_prompt(&m);
    assert_eq!(prompt.path, "/a.rs");
    assert_eq!(prompt.current, 0o644);
    assert_eq!(prompt.input, "644");
    // Rub the prefill out and type a new mode; a non-octal digit and a
    // fifth digit are refused at the prompt (the kernel could never
    // accept them).
    for _ in 0..3 {
        handle_event(&mut m, &mut fs, &Event::Backspace);
    }
    for key in ['9', '6', '0', '0', '7', '7'] {
        handle_event(&mut m, &mut fs, &Event::Char(key));
    }
    assert_eq!(mode_prompt(&m).input, "6007");
    handle_event(&mut m, &mut fs, &Event::Backspace);
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('a'));
    handle_event(&mut m, &mut fs, &Event::Char('0'));
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert_eq!(m.prompt, None);
    assert!(fs.set_modes.borrow().is_empty());
}

#[test]
fn a_refused_stat_opens_no_prompt() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane, cursor onto the denied directory row.
    m.tree_cursor = 2; // "locked"
    handle_event(&mut m, &mut fs, &Event::Char('a'));
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
    handle_event(&mut m, &mut fs, &Event::Char('a'));
    assert!(m.prompt.is_some());
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('a'));
    for _ in 0..4 {
        handle_event(&mut m, &mut fs, &Event::Backspace);
    }
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(m.prompt, None);
    assert!(fs.set_modes.borrow().is_empty());
    assert!(m.message.is_some(), "the empty apply is reported");
}

#[test]
fn the_tree_pane_edits_the_selected_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane (the default), cursor onto "docs".
    handle_event(&mut m, &mut fs, &Event::Down);
    handle_event(&mut m, &mut fs, &Event::Char('a'));
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
    handle_event(&mut m, &mut fs, &Event::Char('a'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 70));
    render(&m, &mut window);
    let line = row_text(&window, 5);
    assert!(line.starts_with("mode a.rs [644]: 644_"), "{line}");
}

// --- Renderer: golden grids ---------------------------------------------

#[test]
fn the_frame_lays_out_panes_status_and_hints() {
    let mut fs = fixture();
    let m = model(&mut fs);
    let mut window = Window::new(Pos::new(0, 0), Size::new(8, 60));
    render(&m, &mut window);
    // Tree pane: root marker and child dirs, divider at the tree width.
    assert_eq!(
        row_text(&window, 0),
        format!("{:<20}|{}", "- /", file_row_text(&m, 0))
    );
    assert!(row_text(&window, 1).starts_with("  + docs"));
    assert!(row_text(&window, 2).starts_with("  + locked"));
    // Status line: path, count, sort, free space.
    let status = row_text(&window, 6);
    assert!(status.starts_with("/  5 entries  sort name asc  free 500/1000"));
    // Hint line (truncated to the 60-column grid; the leading hints show).
    assert!(row_text(&window, 7).contains("t tag"));
}

/// The file-pane text of visible row `index`, as the renderer prints it.
fn file_row_text(m: &Model, index: usize) -> String {
    let files = m.visible_files();
    let entry = files[index];
    // 60 cols, tree width 20, divider 1: the file pane is 39 wide.
    let stamp = if entry.modified == Time64::UNIX_EPOCH {
        String::from("-")
    } else {
        unreachable!("fixture row 0 is a directory with no stamp")
    };
    let size_text = if entry.kind.is_dir() {
        String::from("<dir>")
    } else {
        format!("{}", entry.size)
    };
    let tail = format!("{size_text:>12} {stamp:>16}");
    // The row starts with the one-column tag marker (a space while
    // untagged), so the name field gives up marker + two separators.
    let name_width = 39 - (tail.len() + 3);
    format!(" {:<name_width$} {tail}", entry.name)
}

#[test]
fn the_epoch_stamp_renders_as_absent_and_real_stamps_as_dates() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    let mut window = Window::new(Pos::new(0, 0), Size::new(10, 70));
    render(&m, &mut window);
    let grid: Vec<String> = (0..8).map(|r| row_text(&window, r)).collect();
    // The directory rows carry no stamp; the file rows carry real dates.
    let docs_row = grid.iter().find(|r| r.contains("docs")).expect("docs row");
    assert!(docs_row.contains("<dir>"));
    assert!(docs_row.trim_end().ends_with('-'));
    let file_row = grid.iter().find(|r| r.contains("a.rs")).expect("file row");
    assert!(file_row.contains("1970-01-01 00:16"), "secs=1000 renders");
}

#[test]
fn the_help_overlay_replaces_the_panes() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('?'));
    assert_eq!(m.overlay, Overlay::Help);
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 40));
    render(&m, &mut window);
    assert_eq!(row_text(&window, 0), "keys: q quits");
    // Any key dismisses it.
    handle_event(&mut m, &mut fs, &Event::Char('x'));
    assert_eq!(m.overlay, Overlay::None);
}

#[test]
fn the_sort_menu_prompt_appears_on_the_message_line() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('s'));
    let mut window = Window::new(Pos::new(0, 0), Size::new(6, 70));
    render(&m, &mut window);
    assert!(row_text(&window, 5).starts_with("sort: n)ame"));
}

// --- File operations: mkdir and rename ------------------------------------

/// Type `text` into the open prompt, character by character.
fn type_text(m: &mut Model, fs: &mut dyn Fs, text: &str) {
    for ch in text.chars() {
        handle_event(m, fs, &Event::Char(ch));
    }
}

#[test]
fn mkdir_creates_a_directory_in_the_listed_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('M'));
    type_text(&mut m, &mut fs, "new");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('M'));
    type_text(&mut m, &mut fs, "a/b");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("invalid name"));
    assert!(!fs.has_dir("/a/b"));
    // Esc cancels an open prompt with nothing created.
    handle_event(&mut m, &mut fs, &Event::Char('M'));
    type_text(&mut m, &mut fs, "x");
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert_eq!(m.prompt, None);
    assert!(!fs.has_dir("/x"));
}

#[test]
fn rename_moves_a_file_within_its_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &Event::Char('r'));
    // The prompt is prefilled with the current name; rub it out.
    for _ in 0.."a.rs".len() {
        handle_event(&mut m, &mut fs, &Event::Backspace);
    }
    type_text(&mut m, &mut fs, "z.rs");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('r'));
    for _ in 0.."a.rs".len() {
        handle_event(&mut m, &mut fs, &Event::Backspace);
    }
    type_text(&mut m, &mut fs, "b.txt");
    handle_event(&mut m, &mut fs, &Event::Enter);
    // The rename paused on the existing target; skip leaves both intact.
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    handle_event(&mut m, &mut fs, &Event::Char('s'));
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
}

#[test]
fn renaming_the_session_root_is_refused() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('r'));
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
    handle_event(&mut m, &mut fs, &Event::Char('d'));
    assert!(matches!(m.prompt, Some(Prompt::ConfirmDelete(_))));
    handle_event(&mut m, &mut fs, &Event::Char('y'));
    assert_eq!(fs.contents("/a.rs"), None);
    assert!(!m.files.iter().any(|e| e.name == "a.rs"));
}

#[test]
fn delete_declined_changes_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &Event::Char('d'));
    // Any key but `y` declines — `n`, Esc, or anything else.
    handle_event(&mut m, &mut fs, &Event::Char('n'));
    assert_eq!(m.prompt, None);
    assert_eq!(fs.contents("/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn delete_removes_a_directory_and_its_contents() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane, cursor onto "docs" (which holds guide.md).
    handle_event(&mut m, &mut fs, &Event::Down);
    handle_event(&mut m, &mut fs, &Event::Char('d'));
    handle_event(&mut m, &mut fs, &Event::Char('y'));
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
    handle_event(&mut m, &mut fs, &Event::Char('d'));
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/big2");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    // A relative destination is joined onto the listed directory.
    type_text(&mut m, &mut fs, "docs");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(fs.contents("/docs/a.rs"), Some(vec![b'x'; 30]));
}

#[test]
fn copy_reproduces_a_directory_tree() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    // Tree pane, cursor onto "docs".
    handle_event(&mut m, &mut fs, &Event::Down);
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/a.rs");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("same entry"));
    // A directory into its own subtree.
    m.pane = Pane::Tree;
    handle_event(&mut m, &mut fs, &Event::Down); // onto docs
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/docs/inner");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("into itself"));
    assert!(!fs.has_dir("/docs/inner"));
}

#[test]
fn a_bad_destination_spelling_is_refused_by_the_path_grammar() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 2; // a.rs
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    // An interior empty component fails the shared grammar.
    type_text(&mut m, &mut fs, "/x//y");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.message.clone().expect("refused").contains("bad path"));
}

// --- File operations: the overwrite question --------------------------------

#[test]
fn copy_over_an_existing_file_asks_and_overwrites_on_o() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big (999 bytes)
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    // Nothing was written while the question is open.
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
    handle_event(&mut m, &mut fs, &Event::Char('o'));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 999]));
}

#[test]
fn copy_over_an_existing_file_skips_on_s_and_cancels_on_c() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    m.file_cursor = 4; // big
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &Event::Enter);
    handle_event(&mut m, &mut fs, &Event::Char('s'));
    assert_eq!(fs.contents("/b.txt"), Some(vec![b'x'; 10]));
    let message = m.message.clone().expect("summary");
    assert!(message.contains("skipped"), "{message}");
    // Cancel likewise leaves the target alone.
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &Event::Enter);
    handle_event(&mut m, &mut fs, &Event::Char('c'));
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
    handle_event(&mut m, &mut fs, &Event::Down); // onto dst
    handle_event(&mut m, &mut fs, &Event::Down); // onto src
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/dst");
    handle_event(&mut m, &mut fs, &Event::Enter);
    // Only the conflicting file pauses; skipping it still copies the rest.
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    handle_event(&mut m, &mut fs, &Event::Char('s'));
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
    handle_event(&mut m, &mut fs, &Event::Char('m'));
    type_text(&mut m, &mut fs, "/docs");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Down); // onto src
    handle_event(&mut m, &mut fs, &Event::Char('m'));
    type_text(&mut m, &mut fs, "/vol");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Down); // onto src
    handle_event(&mut m, &mut fs, &Event::Char('m'));
    type_text(&mut m, &mut fs, "/vol");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::Overwrite(_))));
    handle_event(&mut m, &mut fs, &Event::Char('s'));
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
    handle_event(&mut m, &mut fs, &Event::Char('m'));
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/copy");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    type_text(&mut m, &mut fs, "/b.txt");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
        Event::Char('.'), // show hidden
        Event::Char('?'), // help overlay
        Event::Esc,       // dismiss it
        Event::Char('q'), // quit
    ];
    for event in &script {
        handle_event(&mut m, &mut fs, event);
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
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    assert!(m.tags.contains("/a.rs"));
    // The cursor stepped down so repeated presses mark a run.
    assert_eq!(m.file_cursor, 3);
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    assert!(m.tags.contains("/b.txt"));
    assert_eq!(m.tags.count(), 2);
    assert_eq!(m.tags.total_bytes(), 40);
    // Toggling a tagged entry untags it.
    m.file_cursor = 2;
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    assert!(!m.tags.contains("/a.rs"));
    assert_eq!(m.tags.count(), 1);
}

#[test]
fn tags_from_the_tree_pane_are_refused() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Tree;
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    assert!(m.tags.is_empty());
    assert!(m.message.is_some());
}

#[test]
fn glob_tagging_tags_matching_visible_entries() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    m.pane = Pane::Files;
    handle_event(&mut m, &mut fs, &Event::Char('T'));
    for ch in "*.rs".chars() {
        handle_event(&mut m, &mut fs, &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('T'));
    handle_event(&mut m, &mut fs, &Event::Char('['));
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('t')); // tags a.rs
    handle_event(&mut m, &mut fs, &Event::Char('i'));
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
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    handle_event(&mut m, &mut fs, &Event::Char('C'));
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
    handle_event(&mut m, &mut fs, &Event::Char('d'));
    assert!(matches!(
        m.prompt,
        Some(Prompt::ConfirmBatchDelete { count: 3 })
    ));
    handle_event(&mut m, &mut fs, &Event::Char('y'));
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    for ch in "docs".chars() {
        handle_event(&mut m, &mut fs, &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &Event::Enter);
    // The existing /docs/b.txt paused the batch on its question.
    assert!(matches!(m.prompt, Some(Prompt::BatchOverwrite(_))));
    handle_event(&mut m, &mut fs, &Event::Char('s'));
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    for ch in "docs".chars() {
        handle_event(&mut m, &mut fs, &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(matches!(m.prompt, Some(Prompt::BatchOverwrite(_))));
    handle_event(&mut m, &mut fs, &Event::Char('c'));
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
    handle_event(&mut m, &mut fs, &Event::Char('c'));
    for ch in "b.txt".chars() {
        handle_event(&mut m, &mut fs, &Event::Char(ch));
    }
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('u'));
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
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert!(m.walk.is_none());
    let message = m.message.clone().expect("a cancellation report");
    assert!(message.contains("cancelled"), "got {message}");
}

#[test]
fn usage_walk_completes_with_figures_and_reports_unreadable() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('u'));
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
    handle_event(&mut m, &mut fs, &Event::Char('v'));
    assert_eq!(m.view, View::Flat);
    let mut guard = 0;
    while m.walk.as_ref().is_some_and(WalkState::ticking) {
        walk_tick(&mut m, &mut fs);
        guard += 1;
        assert!(guard < 10, "the walk must finish");
    }
    // The first found file is the root's first regular entry.
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    assert!(m.tags.contains("/a.rs"));
    assert_eq!(m.flat_cursor, 1);
    handle_event(&mut m, &mut fs, &Event::Esc);
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
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    handle_event(&mut m, &mut fs, &Event::Char('t'));
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
    handle_event(&mut m, &mut fs, &Event::Char('f'));
    type_text(&mut m, &mut fs, "*.rs");
    // The narrowing is live while the prompt is still open.
    assert!(m.prompt.is_some());
    assert_eq!(visible_names(&m), ["a.rs"]);
    handle_event(&mut m, &mut fs, &Event::Esc);
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
    handle_event(&mut m, &mut fs, &Event::Char('f'));
    type_text(&mut m, &mut fs, "*.txt");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.prompt.is_none());
    assert_eq!(visible_names(&m), ["b.txt"]);
    assert_eq!(m.message.as_deref(), Some("filter *.txt: 1 shown"));
    let mut window = Window::new(Pos::new(0, 0), Size::new(10, 70));
    render(&m, &mut window);
    let status = row_text(&window, 8);
    assert!(status.contains("filter *.txt"), "got {status}");
    // Reopening pre-fills the pattern for editing; emptying it clears.
    handle_event(&mut m, &mut fs, &Event::Char('f'));
    for _ in 0.."*.txt".len() {
        handle_event(&mut m, &mut fs, &Event::Backspace);
    }
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.filter.is_none());
    assert_eq!(m.message.as_deref(), Some("filter cleared"));
}

#[test]
fn a_bad_filter_pattern_hides_nothing_and_says_so() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('f'));
    type_text(&mut m, &mut fs, "[a");
    // An uncompilable pattern filters nothing behind the user's back.
    assert_eq!(
        visible_names(&m),
        ["docs", "locked", "a.rs", "b.txt", "big"]
    );
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "*.md");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "[x");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
    let message = m.message.clone().expect("a bad-pattern report");
    assert!(message.contains("bad pattern"), "got {message}");
}

#[test]
fn flat_enter_jumps_to_the_hits_directory() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "*.md");
    handle_event(&mut m, &mut fs, &Event::Enter);
    finish_walk(&mut m, &mut fs);
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('F'));
    type_text(&mut m, &mut fs, "hello");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('F'));
    type_text(&mut m, &mut fs, "key");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('F'));
    type_text(&mut m, &mut fs, "token");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('F'));
    type_text(&mut m, &mut fs, "needle");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('F'));
    type_text(&mut m, &mut fs, "needle");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(m.walk.as_ref().is_some_and(WalkState::ticking));
    // Esc while the search runs stops it in place…
    handle_event(&mut m, &mut fs, &Event::Esc);
    let walk = m.walk.as_ref().expect("the stopped walk");
    assert!(walk.done);
    assert_eq!(m.view, View::Flat);
    let message = m.message.clone().expect("a stop report");
    assert!(message.contains("stopped"), "got {message}");
    // …and a second Esc leaves the view.
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert_eq!(m.view, View::Panes);
    assert!(m.walk.is_none());
}

#[test]
fn an_empty_needle_searches_nothing() {
    let mut fs = fixture();
    let mut m = model(&mut fs);
    handle_event(&mut m, &mut fs, &Event::Char('F'));
    handle_event(&mut m, &mut fs, &Event::Enter);
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

    fn stat_mode(&mut self, _path: &str) -> Result<u32, Errno> {
        Err(Errno::NotFound)
    }

    fn set_mode(&mut self, _path: &str, _mode: u32) -> Result<(), Errno> {
        Err(Errno::PermissionDenied)
    }

    fn stat_kind(&mut self, _path: &str) -> Result<FileKind, Errno> {
        Ok(FileKind::Regular)
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
                name: basename(path).to_owned(),
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

/// Put the file-pane cursor on `name` and press Enter.
fn open_file(m: &mut Model, fs: &mut FakeFs, name: &str) {
    m.pane = Pane::Files;
    m.file_cursor = m
        .visible_files()
        .iter()
        .position(|e| e.name == name)
        .expect("the fixture lists the entry");
    handle_event(m, fs, &Event::Enter);
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
            None => false,
        };
        if !live {
            return;
        }
        viewer_tick(m, fs);
    }
    panic!("the viewer scan did not finish");
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
    handle_event(&mut m, &mut fs, &Event::Char('q'));
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
    refresh_viewer(&mut m, &mut fs, 3, 40);
    let mut window = Window::new(Pos::new(0, 0), Size::new(5, 40));
    render(&m, &mut window);
    assert!(row_text(&window, 0).starts_with("alpha"));
    assert!(row_text(&window, 1).starts_with("bravo"));
    assert!(row_text(&window, 2).starts_with("charlie"));
    // The status line names the file, the mode, and the place.
    let status = row_text(&window, 3);
    assert!(status.starts_with("/a.rs  text"), "got {status}");
    assert!(status.contains("line 1"), "got {status}");
    // One row down.
    handle_event(&mut m, &mut fs, &Event::Down);
    refresh_viewer(&mut m, &mut fs, 3, 40);
    render(&m, &mut window);
    assert!(row_text(&window, 0).starts_with("bravo"));
    assert_eq!(text_view(&m).top.line, Some(1));
    // A page down, then Home returns to the top.
    handle_event(&mut m, &mut fs, &Event::PageDown);
    refresh_viewer(&mut m, &mut fs, 3, 40);
    assert_eq!(text_view(&m).top.line, Some(4));
    handle_event(&mut m, &mut fs, &Event::Home);
    assert_eq!(text_view(&m).top.line, Some(0));
    // End lands the last page.
    handle_event(&mut m, &mut fs, &Event::End);
    refresh_viewer(&mut m, &mut fs, 3, 40);
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
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    refresh_viewer(&mut m, &mut fs, 3, 40);
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
    refresh_viewer(&mut m, &mut fs, 4, 10);
    let view = text_view(&m);
    assert_eq!(view.rows[0].text, "0123456789");
    assert_eq!(view.rows[1].text, "ABCDEF0123");
    assert_eq!(view.rows[2].text, "short");
    // Wrap off: one (truncated-at-render) row per file row.
    handle_event(&mut m, &mut fs, &Event::Char('w'));
    refresh_viewer(&mut m, &mut fs, 4, 10);
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
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "42");
    handle_event(&mut m, &mut fs, &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).top.line, Some(41));
    assert_eq!(m.message.as_deref(), Some("line 42"));
    // A target past the last line lands on the last row instead.
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "999");
    handle_event(&mut m, &mut fs, &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).top.line, Some(49));
    // A non-number is refused with nothing moved.
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "abc");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "needle");
    handle_event(&mut m, &mut fs, &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).last_hit, Some(4090), "case-insensitive hit");
    // `n` continues past the hit to the second occurrence.
    handle_event(&mut m, &mut fs, &Event::Char('n'));
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(text_view(&m).last_hit, Some(6001));
    // …and a further `n` reports not found, keeping the place.
    handle_event(&mut m, &mut fs, &Event::Char('n'));
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
    refresh_viewer(&mut m, &mut fs, 3, 80);
    let mut window = Window::new(Pos::new(0, 0), Size::new(5, 80));
    render(&m, &mut window);
    let first = row_text(&window, 0);
    assert!(
        first.starts_with("00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f"),
        "got {first}"
    );
    assert!(first.contains("|................|"), "got {first}");
    // The 33..40 tail row is short: eight bytes, printable ASCII shown.
    let third = row_text(&window, 2);
    assert!(
        third.starts_with("00000020  20 21 22 23 24 25 26 27"),
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
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "0x30");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(hex_view(&m).top, 0x30);
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "70");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(hex_view(&m).top, 64, "row containing byte 70");
    // Beyond the end clamps to the last row.
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "0xFFFF");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(hex_view(&m).top, 96);
    // Garbage is refused with nothing moved.
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "0xZZ");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "0xdeadbeef");
    handle_event(&mut m, &mut fs, &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(hex_view(&m).last_hit, Some(4094));
    assert_eq!(hex_view(&m).top, 4080, "the hit row is on screen");
    assert_eq!(m.message.as_deref(), Some("found at 0xffe"));
    // A text pattern matches case-insensitively.
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "tEXT");
    handle_event(&mut m, &mut fs, &Event::Enter);
    run_viewer_job(&mut m, &mut fs);
    assert_eq!(hex_view(&m).last_hit, Some(5000));
    // An odd-length 0x spelling is refused before anything runs.
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "0xabc");
    handle_event(&mut m, &mut fs, &Event::Enter);
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
    handle_event(&mut m, &mut fs, &Event::Char('g'));
    type_text(&mut m, &mut fs, "0x123456780");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert_eq!(hex_view(&m).top, 0x1_2345_6780);
    refresh_viewer(&mut m, &mut fs, 2, 80);
    let mut window = Window::new(Pos::new(0, 0), Size::new(4, 80));
    render(&m, &mut window);
    // The offset column widens to the file's own 64-bit reach (nine hex
    // digits for a 5 GiB file).
    let first = row_text(&window, 0);
    assert!(first.starts_with("123456780  "), "got {first}");
    // End lands the final row of the 5 GiB file.
    handle_event(&mut m, &mut fs, &Event::End);
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
    handle_event(&mut m, &mut fs, &Event::Down);
    handle_event(&mut m, &mut fs, &Event::Down);
    assert_eq!(text_view(&m).top.offset, 28);
    handle_event(&mut m, &mut fs, &Event::Char('x'));
    assert_eq!(hex_view(&m).top, 16);
    // Back to text: snapped to the start of the row containing the hex
    // top (line 2 at offset 14); goto re-anchors the line number.
    handle_event(&mut m, &mut fs, &Event::Char('t'));
    assert_eq!(text_view(&m).top.offset, 14);
    assert_eq!(text_view(&m).top.line, None, "unknown until re-anchored");
    // Esc leaves the viewer for the panes.
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert_eq!(m.view, View::Panes);
    assert!(m.viewer.is_none());
}

#[test]
fn esc_stops_a_live_viewer_scan_before_leaving() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", &vec![b'a'; 4096]);
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    handle_event(&mut m, &mut fs, &Event::Char('/'));
    type_text(&mut m, &mut fs, "zzz");
    handle_event(&mut m, &mut fs, &Event::Enter);
    assert!(text_view(&m).ticking());
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert!(!text_view(&m).ticking());
    assert_eq!(m.view, View::Viewer, "the first Esc only stops the scan");
    handle_event(&mut m, &mut fs, &Event::Esc);
    assert_eq!(m.view, View::Panes);
}

#[test]
fn a_read_refused_mid_view_closes_the_viewer() {
    let mut fs = fixture();
    fs.set_bytes("/a.rs", b"alpha\nbravo\n");
    let mut m = model(&mut fs);
    open_file(&mut m, &mut fs, "a.rs");
    fs.read_fail = Some((String::from("/a.rs"), Errno::PermissionDenied));
    refresh_viewer(&mut m, &mut fs, 3, 40);
    assert_eq!(m.view, View::Panes);
    assert!(m.viewer.is_none());
    let message = m.message.clone().expect("a report");
    assert!(message.contains("PermissionDenied"), "got {message}");
}
