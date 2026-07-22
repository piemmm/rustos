//! Headless unit tests for the filesystem browser.
//!
//! Every test drives the [`Browser`] against an in-memory [`MockFs`] tree, so
//! the navigation and rendering logic is exercised without a kernel or a real
//! VFS.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_geometry::Rect;
use tairix_raster::Color;
use tairix_theme::Theme;

use crate::browser::Browser;
use crate::entry::Entry;
use crate::error::BrowseError;
use crate::source::DirectorySource;

/// The absolute-path key the mock indexes a directory by — the one shared
/// spelling, so tests, the model, and the VFS engine agree on the path
/// string.
fn key(components: &[String]) -> String {
    crate::vfs::spell_absolute_path(components)
}

/// An in-memory directory tree with an optional set of unreadable paths.
struct MockFs {
    dirs: BTreeMap<String, Vec<Entry>>,
    denied: BTreeSet<String>,
    root_reads: usize,
    root_after_refresh: Option<Vec<Entry>>,
}

impl MockFs {
    /// The-shaped fixture: the four top-level directories, a populated
    /// `/System`, an empty `/System/Fonts`, and a `/System/Security` that
    /// exists but is unreadable (capability-gated).
    fn fixture() -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert(
            "/".to_string(),
            vec![
                Entry::directory("System"),
                Entry::directory("Users"),
                Entry::directory("Apps"),
                Entry::directory("Storage"),
            ],
        );
        dirs.insert(
            "/System".to_string(),
            vec![
                Entry::directory("Fonts"),
                Entry::directory("Security"),
                Entry::file("Kernel"),
            ],
        );
        dirs.insert("/System/Fonts".to_string(), Vec::new());
        dirs.insert("/Users".to_string(), vec![Entry::directory("alice")]);

        let mut denied = BTreeSet::new();
        denied.insert("/System/Security".to_string());

        Self {
            dirs,
            denied,
            root_reads: 0,
            root_after_refresh: None,
        }
    }
}

impl DirectorySource for MockFs {
    fn list(&mut self, components: &[String]) -> Result<Vec<Entry>, Errno> {
        let path = key(components);
        if self.denied.contains(&path) {
            return Err(Errno::PermissionDenied);
        }
        if path == "/" {
            self.root_reads += 1;
            if self.root_reads > 1 {
                if let Some(after) = &self.root_after_refresh {
                    return Ok(after.clone());
                }
            }
        }
        self.dirs.get(&path).cloned().ok_or(Errno::NotFound)
    }
}

fn names(browser: &Browser<MockFs>) -> Vec<&str> {
    browser.entries().iter().map(Entry::name).collect()
}

#[test]
fn open_root_lists_the_four_top_level_directories() {
    let browser = Browser::open_root(MockFs::fixture()).expect("root lists");
    assert!(browser.is_root());
    assert_eq!(browser.path(), "/");
    // The source lists the four in insertion order; the browser shows them in
    // the shared default order (directories, then case-insensitive by name).
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
    assert_eq!(browser.selected_index(), Some(0));
}

#[test]
fn open_root_fails_closed_when_the_root_is_unreadable() {
    let mut fs = MockFs::fixture();
    fs.denied.insert("/".to_string());
    let result = Browser::open_root(fs);
    assert_eq!(
        result.err(),
        Some(BrowseError::Source(Errno::PermissionDenied))
    );
}

#[test]
fn descend_and_climb_track_the_path_and_entries() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Sorted root order is [Apps, Storage, System, Users]; System is index 2.
    browser.open_index(2).expect("enter System");
    assert!(!browser.is_root());
    assert_eq!(browser.path(), "/System");
    assert_eq!(names(&browser), ["Fonts", "Security", "Kernel"]);

    assert_eq!(browser.go_up(), Ok(true));
    assert_eq!(browser.path(), "/");
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn go_up_at_the_root_is_a_no_op() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    assert_eq!(browser.go_up(), Ok(false));
    assert!(browser.is_root());
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn opening_a_regular_file_is_rejected_and_changes_nothing() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // Index 2 under /System is the regular file "Kernel".
    assert_eq!(browser.open_index(2), Err(BrowseError::NotADirectory));
    assert_eq!(browser.path(), "/System");
    assert_eq!(names(&browser), ["Fonts", "Security", "Kernel"]);
}

#[test]
fn opening_an_out_of_range_index_is_rejected() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    assert_eq!(browser.open_index(99), Err(BrowseError::NoSuchEntry));
    assert!(browser.is_root());
}

#[test]
fn descending_into_an_unreadable_directory_fails_closed() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // Index 1 under /System is "Security", which exists but is denied: the
    // read fails and the browser stays on /System with its entries intact.
    assert_eq!(
        browser.open_index(1),
        Err(BrowseError::Source(Errno::PermissionDenied))
    );
    assert_eq!(browser.path(), "/System");
    assert_eq!(names(&browser), ["Fonts", "Security", "Kernel"]);
}

#[test]
fn an_empty_directory_has_no_selection() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    browser.open_index(0).expect("enter Fonts");
    assert_eq!(browser.path(), "/System/Fonts");
    assert!(browser.entries().is_empty());
    assert_eq!(browser.selected_index(), None);
    assert_eq!(browser.selected_entry(), None);
}

#[test]
fn open_selected_descends_into_the_selected_directory() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Sorted root order is [Apps, Storage, System, Users]; Users is index 3.
    browser.select(3).expect("select Users");
    browser.open_selected().expect("enter Users");
    assert_eq!(browser.path(), "/Users");
    assert_eq!(names(&browser), ["alice"]);
}

#[test]
fn selection_movement_clamps_at_both_ends() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select_previous();
    assert_eq!(browser.selected_index(), Some(0));
    for _ in 0..10 {
        browser.select_next();
    }
    assert_eq!(browser.selected_index(), Some(3));
    assert_eq!(browser.select(99), Err(BrowseError::NoSuchEntry));
    assert_eq!(browser.selected_index(), Some(3));
}

#[test]
fn refresh_clamps_a_stale_selection_into_the_new_listing() {
    // The root shrinks to a single entry the next time it is read, modelling
    // the directory changing underneath the browser.
    let mut fs = MockFs::fixture();
    fs.root_after_refresh = Some(vec![Entry::directory("System")]);
    let mut browser = Browser::open_root(fs).expect("root");
    browser.select(3).expect("select Storage");

    browser.refresh().expect("refresh");
    assert_eq!(names(&browser), ["System"]);
    assert_eq!(browser.selected_index(), Some(0));
}

#[test]
fn render_produces_a_surface_the_size_of_the_viewport() {
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let theme = Theme::dark();
    let surface = crate::render(
        &browser,
        &theme,
        tairix_font::BitmapFont::inconsolata(),
        Rect::new(0, 0, 200, 120),
    )
    .expect("surface");
    assert_eq!(surface.width(), 200);
    assert_eq!(surface.height(), 120);
}

#[test]
fn render_gives_the_selected_entry_the_shared_selection_chrome() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select(1).expect("select second entry");
    let theme = Theme::dark();
    let surface = crate::render(
        &browser,
        &theme,
        tairix_font::BitmapFont::inconsolata(),
        Rect::new(0, 0, 200, 120),
    )
    .expect("surface");

    let accent = Color::from(theme.palette().accent).premultiply();
    let raised = Color::from(theme.palette().surface_raised).premultiply();
    let base = Color::from(theme.palette().surface).premultiply();
    // The path bar (top-left) carries the raised role.
    assert_eq!(surface.get(0, 0), Some(raised));

    // The list is drawn through the shared `TableRow` chrome: the path bar is
    // the top row, so entry index 0 is one row down and the selected entry
    // index 1 is two rows down. The selected row lifts to the raised surface
    // and shows the accent *selection rail* in its leading gutter (not a full
    // accent fill), and an unselected row stays the base surface — the one
    // selection look every collection view shares. We sample inside the
    // content column (x = 100), clear of the leading rail gutter and of the
    // reserved right-edge scrollbar gutter.
    let row_height = tairix_font::BitmapFont::inconsolata().glyph_height() + 4;
    let unselected_y = row_height + 1;
    let selected_y = row_height * 2 + 1;
    // The unselected row's body is the base surface.
    assert_eq!(surface.get(100, unselected_y), Some(base));
    // The selected row's body lifts to the raised surface.
    assert_eq!(surface.get(100, selected_y), Some(raised));
    // The accent selection rail sits in the selected row's leading gutter.
    let has_accent_rail = (0..20).any(|x| surface.get(x, selected_y) == Some(accent));
    assert!(
        has_accent_rail,
        "the selected row shows the shared accent selection rail"
    );
}

#[test]
fn render_into_a_tiny_viewport_does_not_panic() {
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let theme = Theme::dark();
    // Too short for even the path bar: paints what it can and returns a
    // surface rather than panicking.
    let surface = crate::render(
        &browser,
        &theme,
        tairix_font::BitmapFont::inconsolata(),
        Rect::new(0, 0, 4, 3),
    )
    .expect("surface");
    assert_eq!(surface.width(), 4);
    assert_eq!(surface.height(), 3);
}

// --- The VFS engine ------------------------------------------------------
//
// `VfsDirectorySource` is the production source; these tests drive it (and
// a `Browser` over it) against in-memory *encoded* `DirEntry` streams — the
// exact bytes a kernel `fs_readdir` transfer produces — so the spelling,
// decode, and refusal branches are all host-proven.

use tairix_abi::fs::{DirEntry, FileKind, FS_PATH_MAX};
use tairix_abi::time::Time64;

use crate::vfs::{absolute_path, entries_from_dir_stream, VfsDirectorySource};

/// Encode `(name, kind)` children as one packed `DirEntry` stream.
fn encoded_stream(children: &[(&[u8], FileKind)]) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let mut off = 0;
    for (name, kind) in children {
        off += DirEntry {
            kind: *kind,
            size: 0,
            allocated: 0,
            modified: Time64::UNIX_EPOCH,
            name,
        }
        .encode_into(&mut buf[off..])
        .expect("fits");
    }
    buf.truncate(off);
    buf
}

/// A source over an in-memory path → encoded-stream tree.
fn tree_source(
    dirs: BTreeMap<String, Vec<u8>>,
) -> VfsDirectorySource<impl FnMut(&str) -> Result<Vec<u8>, Errno>> {
    VfsDirectorySource::new(move |path: &str| dirs.get(path).cloned().ok_or(Errno::NotFound))
}

#[test]
fn absolute_path_spells_root_and_nested_directories() {
    assert_eq!(absolute_path(&[]).expect("root"), "/");
    assert_eq!(
        absolute_path(&["System".to_string(), "Fonts".to_string()]).expect("nested"),
        "/System/Fonts"
    );
}

#[test]
fn absolute_path_refuses_malformed_components() {
    for bad in ["", ".", "..", "a/b", "nul\0byte"] {
        assert_eq!(
            absolute_path(&[bad.to_string()]),
            Err(Errno::OutOfRange),
            "component {bad:?} must be refused before any syscall"
        );
    }
}

#[test]
fn absolute_path_enforces_the_kernel_path_bound() {
    let long = "a".repeat(FS_PATH_MAX);
    assert_eq!(
        absolute_path(&[long]),
        Err(Errno::LengthOutOfRange),
        "a spelled path over FS_PATH_MAX must never reach the kernel"
    );
}

#[test]
fn entries_from_dir_stream_maps_names_and_kinds_in_order() {
    let stream = encoded_stream(&[
        (b"Logs", FileKind::Directory),
        (b"motd.txt", FileKind::Regular),
    ]);
    let entries = entries_from_dir_stream(&stream).expect("valid stream");
    assert_eq!(
        entries,
        vec![Entry::directory("Logs"), Entry::file("motd.txt")]
    );
}

#[test]
fn entries_from_dir_stream_refuses_a_non_utf8_name_whole() {
    let stream = encoded_stream(&[(b"ok", FileKind::Regular), (b"\xff\xfe", FileKind::Regular)]);
    assert_eq!(entries_from_dir_stream(&stream), Err(Errno::OutOfRange));
}

#[test]
fn entries_from_dir_stream_refuses_a_truncated_stream_whole() {
    let mut stream = encoded_stream(&[(b"ok", FileKind::Regular)]);
    stream.extend_from_slice(&[0u8; 3]);
    assert_eq!(entries_from_dir_stream(&stream), Err(Errno::BufferTooSmall));
}

#[test]
fn a_browser_navigates_the_vfs_source_end_to_end() {
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        encoded_stream(&[(b"System", FileKind::Directory)]),
    );
    dirs.insert(
        "/System".to_string(),
        encoded_stream(&[
            (b"Fonts", FileKind::Directory),
            (b"motd.txt", FileKind::Regular),
        ]),
    );
    dirs.insert("/System/Fonts".to_string(), encoded_stream(&[]));

    let mut browser = Browser::open_root(tree_source(dirs)).expect("root opens");
    assert_eq!(browser.entries(), &[Entry::directory("System")]);

    browser.open_index(0).expect("descend into /System");
    assert_eq!(browser.path(), "/System");
    assert_eq!(
        browser.entries(),
        &[Entry::directory("Fonts"), Entry::file("motd.txt")]
    );

    browser.open_index(0).expect("descend into /System/Fonts");
    assert_eq!(browser.path(), "/System/Fonts");
    assert!(browser.entries().is_empty());

    assert!(browser.go_up().expect("climb back"));
    assert_eq!(browser.path(), "/System");
}

#[test]
fn entry_index_at_mirrors_the_rendered_rows() {
    use crate::render::{entry_index_at, row_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let browser = Browser::open_root(MockFs::fixture()).expect("root opens");
    let row = row_height(font);
    // A window wide enough for content beside the scrollbar gutter, the path
    // bar plus three entry rows tall. Clicks land in the content column (x=4).
    let vp = |h: u32| Rect::new(0, 0, 200, h);
    let at = |b: &Browser<MockFs>, h: u32, y: u32| {
        entry_index_at(
            b,
            font,
            &theme,
            vp(h),
            Point::new(4, i32::try_from(y).unwrap()),
        )
    };
    let viewport_height = row * 4;

    // The path bar resolves to no entry; the first list row is entry 0.
    assert_eq!(at(&browser, viewport_height, 0), None);
    assert_eq!(at(&browser, viewport_height, row - 1), None);
    assert_eq!(at(&browser, viewport_height, row), Some(0));
    assert_eq!(at(&browser, viewport_height, row * 2 + row / 2), Some(1));
    // A row past the listing's end and a coordinate outside the viewport
    // resolve to nothing rather than a clamped guess.
    let last = u32::try_from(browser.entries().len()).expect("a tiny fixture listing");
    assert_eq!(at(&browser, row * (last + 2), row * (last + 1)), None);
    assert_eq!(at(&browser, viewport_height, viewport_height), None);
    // A degenerate viewport (path bar only) has no clickable rows.
    assert_eq!(at(&browser, row, row), None);
    // A click in the reserved scrollbar gutter resolves to no row.
    assert_eq!(
        entry_index_at(
            &browser,
            font,
            &theme,
            vp(viewport_height),
            Point::new(199, i32::try_from(row).unwrap())
        ),
        None
    );
}

#[test]
fn entry_index_at_accounts_for_the_scroll_anchor() {
    use crate::render::{entry_index_at, reveal_selection, row_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root opens");
    let row = row_height(font);
    // Two visible entry rows; select the last entry and reveal it so the list
    // scrolls to keep it on the bottom row — exactly what the app does.
    let viewport_height = row * 3;
    let vp = Rect::new(0, 0, 200, viewport_height);
    let last = browser.entries().len() - 1;
    browser.select(last).expect("selectable");
    reveal_selection(&mut browser, font, &theme, vp);
    // The bottom visible row is the selected (last) entry; the row above
    // it is its predecessor — exactly what `render` draws.
    assert_eq!(
        entry_index_at(
            &browser,
            font,
            &theme,
            vp,
            Point::new(4, i32::try_from(row * 2).unwrap())
        ),
        Some(last)
    );
    assert_eq!(
        entry_index_at(
            &browser,
            font,
            &theme,
            vp,
            Point::new(4, i32::try_from(row).unwrap())
        ),
        Some(last - 1)
    );
}

#[test]
fn a_missing_directory_surfaces_the_fetch_refusal() {
    let mut source = tree_source(BTreeMap::new());
    assert_eq!(
        source.list(&["System".to_string()]),
        Err(Errno::NotFound),
        "the engine adds no authority and fabricates no listing"
    );
}

// --- FM1: richer entries, bundle recognition, and the shared sort --------

use crate::entry::{is_bundle_name, EntryKind};
use crate::sort::{sort_entries, SortDirection, SortKey, SortMode};

/// Encode `(name, kind, size, modified)` children as one packed `DirEntry`
/// stream — the metadata-carrying sibling of [`encoded_stream`].
fn encoded_stream_meta(children: &[(&[u8], FileKind, u64, Time64)]) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let mut off = 0;
    for (name, kind, size, modified) in children {
        off += DirEntry {
            kind: *kind,
            size: *size,
            allocated: 0,
            modified: *modified,
            name,
        }
        .encode_into(&mut buf[off..])
        .expect("fits");
    }
    buf.truncate(off);
    buf
}

#[test]
fn entries_carry_size_and_modified_from_the_stream() {
    let modified = Time64::new(1_700_000_000, 500).expect("canonical");
    let stream = encoded_stream_meta(&[
        (b"data.bin", FileKind::Regular, 4096, modified),
        (b"Docs", FileKind::Directory, 0, Time64::from_secs(-100)),
    ]);
    let entries = entries_from_dir_stream(&stream).expect("valid");
    // The stream order is preserved here (the browser applies the sort).
    assert_eq!(entries[0].name(), "data.bin");
    assert_eq!(entries[0].size(), 4096);
    assert_eq!(entries[0].modified(), modified);
    assert_eq!(entries[0].kind(), EntryKind::File);
    assert_eq!(entries[1].name(), "Docs");
    assert_eq!(entries[1].size(), 0);
    assert_eq!(entries[1].modified(), Time64::from_secs(-100));
    assert_eq!(entries[1].kind(), EntryKind::Directory);
}

#[test]
fn a_bad_record_still_refuses_the_whole_listing() {
    // A directory record whose kind byte is corrupt fails the whole stream:
    // the metadata path never shows a partial listing (fail closed).
    let mut stream = encoded_stream_meta(&[(b"ok", FileKind::Regular, 1, Time64::UNIX_EPOCH)]);
    let mut bad = encoded_stream_meta(&[(b"x", FileKind::Regular, 0, Time64::UNIX_EPOCH)]);
    bad[0] = 9;
    stream.extend_from_slice(&bad);
    assert_eq!(entries_from_dir_stream(&stream), Err(Errno::OutOfRange));
}

#[test]
fn is_bundle_name_matches_only_a_named_dot_app() {
    assert!(is_bundle_name("Example.app"));
    assert!(is_bundle_name("Text Editor.app"));
    // Case-insensitive suffix so a volume's casing does not hide a bundle.
    assert!(is_bundle_name("Thing.APP"));
    // A base name is required: the bare suffix is not a bundle.
    assert!(!is_bundle_name(".app"));
    assert!(!is_bundle_name("app"));
    assert!(!is_bundle_name("notes.txt"));
    assert!(!is_bundle_name(""));
    assert!(!is_bundle_name("Example.apple"));
}

#[test]
fn a_dot_app_directory_is_a_bundle_not_a_folder_to_descend() {
    let stream = encoded_stream(&[
        (b"Editor.app", FileKind::Directory),
        (b"plain", FileKind::Directory),
        // A regular file that merely ends in .app is a file, not a bundle:
        // only a *directory* named <Name>.app is a bundle.
        (b"report.app", FileKind::Regular),
    ]);
    let entries = entries_from_dir_stream(&stream).expect("valid");
    assert_eq!(entries[0].kind(), EntryKind::Bundle);
    assert!(entries[0].is_bundle());
    assert!(!entries[0].is_directory(), "a bundle is not descended into");
    assert_eq!(entries[1].kind(), EntryKind::Directory);
    assert!(entries[1].is_directory());
    assert_eq!(entries[2].kind(), EntryKind::File);
}

#[test]
fn a_browser_refuses_to_descend_into_a_bundle() {
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        encoded_stream(&[(b"Editor.app", FileKind::Directory)]),
    );
    let mut browser = Browser::open_root(tree_source(dirs)).expect("root opens");
    // The bundle is modelled as a sealed unit: the browser opens nothing
    // itself (launching is the app layer's job), so descending is refused.
    assert_eq!(browser.open_index(0), Err(BrowseError::NotADirectory));
    assert!(browser.is_root());
}

/// A short listing spanning both groups and mixed case, in a deliberately
/// unsorted source order.
fn mixed_listing() -> Vec<Entry> {
    vec![
        Entry::new("banana.txt", EntryKind::File, 30, Time64::from_secs(300)),
        Entry::directory("Zebra"),
        Entry::new("Apple.txt", EntryKind::File, 10, Time64::from_secs(100)),
        Entry::directory("apricot"),
        Entry::new("Editor.app", EntryKind::Bundle, 0, Time64::from_secs(200)),
        Entry::new("cherry.txt", EntryKind::File, 20, Time64::from_secs(400)),
    ]
}

#[test]
fn default_sort_is_directories_first_then_case_insensitive_name() {
    let mut entries = mixed_listing();
    sort_entries(&mut entries, SortMode::default_order());
    let ordered: Vec<&str> = entries.iter().map(Entry::name).collect();
    // Directories first (apricot < Zebra, case-folded); then files and the
    // bundle together, case-insensitively by name.
    assert_eq!(
        ordered,
        [
            "apricot",
            "Zebra",
            "Apple.txt",
            "banana.txt",
            "cherry.txt",
            "Editor.app",
        ]
    );
}

#[test]
fn sort_by_size_descending_keeps_directories_first() {
    let mut entries = mixed_listing();
    sort_entries(
        &mut entries,
        SortMode {
            key: SortKey::Size,
            direction: SortDirection::Descending,
        },
    );
    let ordered: Vec<&str> = entries.iter().map(Entry::name).collect();
    // Directories still lead (grouping is fixed); among the rest, largest
    // size first, with the two zero-size entries settled by the name tiebreak.
    assert_eq!(
        ordered,
        [
            "apricot",
            "Zebra",
            "banana.txt",
            "cherry.txt",
            "Apple.txt",
            "Editor.app",
        ]
    );
}

#[test]
fn sort_by_modified_orders_within_the_file_group() {
    let mut entries = mixed_listing();
    sort_entries(
        &mut entries,
        SortMode {
            key: SortKey::Modified,
            direction: SortDirection::Ascending,
        },
    );
    let files: Vec<&str> = entries
        .iter()
        .filter(|e| !e.is_directory())
        .map(Entry::name)
        .collect();
    // Earliest modified first: Apple(100) < Editor(200) < banana(300) < cherry(400).
    assert_eq!(
        files,
        ["Apple.txt", "Editor.app", "banana.txt", "cherry.txt"]
    );
}

#[test]
fn sort_of_an_empty_listing_is_a_no_op() {
    let mut entries: Vec<Entry> = Vec::new();
    sort_entries(&mut entries, SortMode::default_order());
    assert!(entries.is_empty());
}

#[test]
fn set_sort_mode_keeps_the_selection_on_the_same_entry() {
    // A source whose entries only sort differently under each mode.
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        encoded_stream_meta(&[
            (b"a.txt", FileKind::Regular, 300, Time64::UNIX_EPOCH),
            (b"b.txt", FileKind::Regular, 100, Time64::UNIX_EPOCH),
            (b"c.txt", FileKind::Regular, 200, Time64::UNIX_EPOCH),
        ]),
    );
    let mut browser = Browser::open_root(tree_source(dirs)).expect("root");
    // Default (name asc): [a, b, c]; select "b".
    assert_eq!(browser.select(1), Ok(()));
    assert_eq!(browser.selected_entry().map(Entry::name), Some("b.txt"));

    browser.set_sort_mode(SortMode {
        key: SortKey::Size,
        direction: SortDirection::Ascending,
    });
    // Now [b(100), c(200), a(300)]; the selection followed "b" to index 0.
    let names: Vec<&str> = browser.entries().iter().map(Entry::name).collect();
    assert_eq!(names, ["b.txt", "c.txt", "a.txt"]);
    assert_eq!(browser.selected_entry().map(Entry::name), Some("b.txt"));
    assert_eq!(browser.selected_index(), Some(0));
    assert_eq!(browser.sort_mode().key, SortKey::Size);
}

#[test]
fn set_sort_mode_is_a_no_op_when_the_mode_is_unchanged() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select(2).expect("select");
    let before: Vec<String> = browser
        .entries()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    browser.set_sort_mode(SortMode::default_order());
    let after: Vec<String> = browser
        .entries()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    assert_eq!(before, after);
    assert_eq!(browser.selected_index(), Some(2));
}

// --- FM2b: the view toggle, the icon grid, and the drawn scrollbar -------

use crate::layout::ViewMode;

/// A browser over a root of `n` regular files (`f0`, `f1`, …), enough to
/// scroll a modest window.
fn many_files(n: usize) -> Browser<MockFs> {
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        (0..n)
            .map(|i| Entry::file(alloc::format!("f{i:03}")))
            .collect(),
    );
    let fs = MockFs {
        dirs,
        denied: BTreeSet::new(),
        root_reads: 0,
        root_after_refresh: None,
    };
    Browser::open_root(fs).expect("root opens")
}

#[test]
fn the_view_mode_defaults_to_list_and_toggles_preserving_selection() {
    let mut browser = many_files(20);
    assert_eq!(browser.view_mode(), ViewMode::List);
    browser.select(7).expect("selectable");
    let names_before: Vec<String> = browser
        .entries()
        .iter()
        .map(|e| e.name().to_string())
        .collect();

    browser.set_view_mode(ViewMode::Grid);
    assert_eq!(browser.view_mode(), ViewMode::Grid);
    // The selection stays on the same entry and the listing is untouched.
    assert_eq!(browser.selected_index(), Some(7));
    let names_after: Vec<String> = browser
        .entries()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    assert_eq!(names_before, names_after);
    // Switching unit resets the scroll to the top.
    assert_eq!(browser.scroll_offset(), 0);
    // Toggling back is symmetric.
    browser.set_view_mode(ViewMode::List);
    assert_eq!(browser.view_mode(), ViewMode::List);
    assert_eq!(browser.selected_index(), Some(7));
}

#[test]
fn wheel_scroll_moves_the_offset_and_clamps_at_the_ends() {
    use crate::render::{row_height, scroll_lines};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(20);
    let row = row_height(font);
    // The path bar plus four visible rows.
    let vp = Rect::new(0, 0, 200, row * 5);

    // Scrolling up at the top does nothing (already clamped).
    assert!(!scroll_lines(&mut browser, font, &theme, vp, -1));
    assert_eq!(browser.scroll_offset(), 0);
    // Scrolling down moves one line per tick.
    assert!(scroll_lines(&mut browser, font, &theme, vp, 3));
    assert_eq!(browser.scroll_offset(), 3);
    // Scrolling far past the end clamps to the last full page (20 rows, four
    // visible → max offset 16) and reports no further movement beyond it.
    assert!(scroll_lines(&mut browser, font, &theme, vp, 1000));
    assert_eq!(browser.scroll_offset(), 16);
    assert!(!scroll_lines(&mut browser, font, &theme, vp, 5));
    assert_eq!(browser.scroll_offset(), 16);
}

#[test]
fn the_drawn_scrollbar_reflects_the_scroll_offset() {
    use crate::render::{row_height, scroll_lines, scroll_model};
    use tairix_controls::{ScrollBar, ScrollOrientation};
    use tairix_geometry::Scale;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(40);
    let row = row_height(font);
    let vp = Rect::new(0, 0, 200, row * 6);
    // Twenty times more content than the viewport shows: the bar is a real,
    // draggable thumb, not a full-track placeholder.
    let model = scroll_model(&browser, font, &theme, vp);
    assert!(model.range().is_scrollable());

    let bar_bounds = Rect::new(184, i32::try_from(row).unwrap(), 16, row * 5);
    let bar = ScrollBar::new(ScrollOrientation::Vertical, model);
    let geometry = bar
        .geometry(bar_bounds, Scale::ONE, &theme)
        .expect("a live bar");
    assert!(geometry.draggable());
    let top_thumb = geometry.thumb().start;

    // Scroll to the end; the drawn thumb moves to the bottom of its travel.
    scroll_lines(&mut browser, font, &theme, vp, 1000);
    let bar = ScrollBar::new(
        ScrollOrientation::Vertical,
        scroll_model(&browser, font, &theme, vp),
    );
    let end_geometry = bar
        .geometry(bar_bounds, Scale::ONE, &theme)
        .expect("a live bar");
    assert!(end_geometry.thumb().start > top_thumb);
    assert_eq!(end_geometry.thumb().start, end_geometry.travel());
}

#[test]
fn the_grid_view_renders_and_hit_tests_the_first_tile() {
    use crate::render::{entry_index_at, row_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(20);
    browser.set_view_mode(ViewMode::Grid);
    let header = row_height(font);
    // A window wide and tall enough for several tiles.
    let vp = Rect::new(0, 0, 400, 400);
    let surface = crate::render(&browser, &theme, font, vp).expect("grid surface");
    assert_eq!(surface.width(), 400);

    // A click just inside the first tile (past the header) resolves to entry 0.
    assert_eq!(
        entry_index_at(
            &browser,
            font,
            &theme,
            vp,
            Point::new(4, i32::try_from(header + 4).unwrap())
        ),
        Some(0)
    );
    // A click on the header resolves to nothing.
    assert_eq!(
        entry_index_at(&browser, font, &theme, vp, Point::new(4, 0)),
        None
    );
}

// --- File-type icon classification (FM3) -------------------------------

mod icon_classifier {
    use tairix_abi::time::Time64;
    use tairix_icon::IconKind;

    use crate::entry::{Entry, EntryKind};
    use crate::icon::{icon_for, icon_for_name};

    fn bundle(name: &str) -> Entry {
        Entry::new(name, EntryKind::Bundle, 0, Time64::UNIX_EPOCH)
    }

    #[test]
    fn kind_decides_before_extension() {
        // A directory is a folder and a bundle an app tile regardless of any
        // extension-looking name; the file table is only consulted for files.
        assert_eq!(icon_for(&Entry::directory("Documents")), IconKind::Folder);
        assert_eq!(icon_for(&bundle("Editor.app")), IconKind::AppBundle);
        // A directory named like an archive is still a folder.
        assert_eq!(icon_for(&Entry::directory("backup.zip")), IconKind::Folder);
    }

    #[test]
    fn known_extensions_map_to_their_class() {
        assert_eq!(icon_for(&Entry::file("notes.txt")), IconKind::Text);
        assert_eq!(icon_for(&Entry::file("main.rs")), IconKind::Text);
        assert_eq!(icon_for(&Entry::file("photo.PNG")), IconKind::Image);
        assert_eq!(icon_for(&Entry::file("logo.svg")), IconKind::Image);
        assert_eq!(icon_for(&Entry::file("dump.tar.gz")), IconKind::Archive);
        assert_eq!(icon_for(&Entry::file("shell.rxe")), IconKind::Executable);
        assert_eq!(icon_for(&Entry::file("mod.wasm")), IconKind::Executable);
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(icon_for_name("READ.MD"), IconKind::Text);
        assert_eq!(icon_for_name("A.ZiP"), IconKind::Archive);
    }

    #[test]
    fn unknown_and_extensionless_fall_back_to_generic_file() {
        assert_eq!(icon_for_name("blob.qwerty"), IconKind::File);
        assert_eq!(icon_for_name("Makefile"), IconKind::File);
        // A dotfile whose only dot starts the name has no extension.
        assert_eq!(icon_for_name(".profile"), IconKind::File);
        // A trailing dot with nothing after it is not an extension.
        assert_eq!(icon_for_name("archive."), IconKind::File);
        // The last extension wins for a multi-part name.
        assert_eq!(icon_for_name("a.txt.zip"), IconKind::Archive);
    }
}
