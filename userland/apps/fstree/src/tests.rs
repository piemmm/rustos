//! Host tests: the model over an in-memory [`Fs`], golden-grid renders,
//! and a scripted end-to-end session — no kernel, no terminal.

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use core::cell::Cell as CoreCell;

use rustos_abi::time::Time64;
use rustos_abi::{Errno, FileKind};
use rustos_curses::{Event, Pos, Size, Window};

use crate::app::handle_event;
use crate::fs::{Fs, FsEntry, VolumeSpace};
use crate::model::{Model, Overlay, Pane, SortKey};
use crate::render::render;

/// An in-memory filesystem: per-path listings, a denied set, and a count
/// of listings served (so laziness is observable).
struct FakeFs {
    dirs: BTreeMap<String, Vec<FsEntry>>,
    denied: Vec<String>,
    reads: CoreCell<usize>,
    space: Option<VolumeSpace>,
}

impl FakeFs {
    fn new() -> Self {
        Self {
            dirs: BTreeMap::new(),
            denied: Vec::new(),
            reads: CoreCell::new(0),
            space: Some(VolumeSpace {
                free_bytes: 500,
                total_bytes: 1000,
            }),
        }
    }

    fn dir(mut self, path: &str, entries: Vec<FsEntry>) -> Self {
        self.dirs.insert(path.to_owned(), entries);
        self
    }

    fn deny(mut self, path: &str) -> Self {
        self.denied.push(path.to_owned());
        self
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
}

fn model(fs: &mut FakeFs) -> Model {
    Model::new(fs, "/", String::from("keys: q quits")).expect("root lists")
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
    assert!(row_text(&window, 7).contains("s sort"));
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
    let name_width = 39 - (tail.len() + 1);
    format!("{:<name_width$} {tail}", entry.name)
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
