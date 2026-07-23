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
use crate::clipboard::{plan_paste, Clipboard, ClipboardOp, PasteError};
use crate::entry::Entry;
use crate::error::BrowseError;
use crate::execute::{
    paste_strategy, CopyCursor, CopyError, PasteStrategy, VolumeId, COPY_CHUNK_LEN,
};
use crate::select::Selection;
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
    /// Paths that list successfully once and then fail closed on every later
    /// read, modelling a directory that becomes unreadable underneath the
    /// browser (e.g. its capability is revoked between visits).
    deny_after_first: BTreeSet<String>,
    /// How many times each path has been listed, so the read-count-dependent
    /// behaviours below can trigger.
    reads: BTreeMap<String, usize>,
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
            deny_after_first: BTreeSet::new(),
            reads: BTreeMap::new(),
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
        let reads = self.reads.entry(path.clone()).or_insert(0);
        *reads += 1;
        let count = *reads;
        if count > 1 && self.deny_after_first.contains(&path) {
            return Err(Errno::PermissionDenied);
        }
        if path == "/" && count > 1 {
            if let Some(after) = &self.root_after_refresh {
                return Ok(after.clone());
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
        &[],
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
    let font = tairix_font::BitmapFont::inconsolata();
    let row_height = font.glyph_height() + 4;
    let header = crate::render::chrome_height(font, &theme);
    let surface = crate::render(
        &browser,
        &theme,
        font,
        Rect::new(0, 0, 200, header + row_height * 3),
        &[],
    )
    .expect("surface");

    let accent = Color::from(theme.palette().accent).premultiply();
    let raised = Color::from(theme.palette().surface_raised).premultiply();
    let base = Color::from(theme.palette().surface).premultiply();
    // The chrome strip (toolbar over the path bar, top-left) carries the
    // raised role.
    assert_eq!(surface.get(0, 0), Some(raised));

    // The list is drawn through the shared `TableRow` chrome below the toolbar
    // and path bar, so entry index 0 is the first content row and the selected
    // entry index 1 the next. The selected row lifts to the raised surface
    // and shows the accent *selection rail* in its leading gutter (not a full
    // accent fill), and an unselected row stays the base surface — the one
    // selection look every collection view shares. We sample inside the
    // content column (x = 100), clear of the leading rail gutter and of the
    // reserved right-edge scrollbar gutter.
    // Entry rows begin below the chrome (toolbar + path bar): entry 0 is the
    // first content row and the selected entry 1 the second.
    let unselected_y = header + 1;
    let selected_y = header + row_height + 1;
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
        &[],
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
    // Each component is a valid single name (well within the per-name bound),
    // so the *whole-path* FS_PATH_MAX bound is what trips: enough 250-byte
    // components that the spelled path runs past FS_PATH_MAX.
    let component = "a".repeat(250);
    let deep: Vec<String> = core::iter::repeat(component)
        .take(FS_PATH_MAX / 250 + 1)
        .collect();
    assert_eq!(
        absolute_path(&deep),
        Err(Errno::LengthOutOfRange),
        "a spelled path over FS_PATH_MAX must never reach the kernel"
    );
}

#[test]
fn absolute_path_refuses_an_over_long_component() {
    // A single component past the per-name bound is refused as a malformed
    // component, before the whole-path length is even considered.
    let huge = "a".repeat(300);
    assert_eq!(absolute_path(&[huge]), Err(Errno::OutOfRange));
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
    use crate::render::{chrome_height, entry_index_at, row_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let browser = Browser::open_root(MockFs::fixture()).expect("root opens");
    let row = row_height(font);
    // The chrome (toolbar + path bar) reserved above the first entry row.
    let header = chrome_height(font, &theme);
    // A window wide enough for content beside the scrollbar gutter, the chrome
    // plus several entry rows tall. Clicks land in the content column (x=4).
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
    let viewport_height = header + row * 4;

    // The chrome resolves to no entry; the first list row is entry 0.
    assert_eq!(at(&browser, viewport_height, 0), None);
    assert_eq!(at(&browser, viewport_height, header - 1), None);
    assert_eq!(at(&browser, viewport_height, header), Some(0));
    assert_eq!(
        at(&browser, viewport_height, header + row + row / 2),
        Some(1)
    );
    // A row past the listing's end and a coordinate outside the viewport
    // resolve to nothing rather than a clamped guess.
    let last = u32::try_from(browser.entries().len()).expect("a tiny fixture listing");
    assert_eq!(
        at(&browser, header + row * (last + 1), header + row * last),
        None
    );
    assert_eq!(at(&browser, viewport_height, viewport_height), None);
    // A degenerate viewport (chrome only) has no clickable rows.
    assert_eq!(at(&browser, header, header), None);
    // A click in the reserved scrollbar gutter resolves to no row.
    assert_eq!(
        entry_index_at(
            &browser,
            font,
            &theme,
            vp(viewport_height),
            Point::new(199, i32::try_from(header).unwrap())
        ),
        None
    );
}

#[test]
fn entry_index_at_accounts_for_the_scroll_anchor() {
    use crate::render::{chrome_height, entry_index_at, reveal_selection, row_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root opens");
    let row = row_height(font);
    let header = chrome_height(font, &theme);
    // Two visible entry rows below the chrome; select the last entry and reveal
    // it so the list scrolls to keep it on the bottom row — as the app does.
    let viewport_height = header + row * 2;
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
            Point::new(4, i32::try_from(header + row).unwrap())
        ),
        Some(last)
    );
    assert_eq!(
        entry_index_at(
            &browser,
            font,
            &theme,
            vp,
            Point::new(4, i32::try_from(header).unwrap())
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

// --- FM4b: the clickable breadcrumb path bar -----------------------------

#[test]
fn breadcrumb_layout_left_aligns_when_the_trail_fits() {
    use crate::breadcrumb::layout;
    // widths 10/20/30, pad 4, sep 6: full = 10+6+20+6+30 = 72 ≤ usable
    // (200-8 = 192), so the strip starts at `pad` and runs left to right.
    let placed = layout(&[10, 20, 30], 200, 4, 6);
    assert_eq!(placed.len(), 3);
    assert_eq!((placed[0].x, placed[0].width), (4, 10));
    assert_eq!((placed[1].x, placed[1].width), (20, 20));
    assert_eq!((placed[2].x, placed[2].width), (46, 30));
}

#[test]
fn breadcrumb_layout_right_anchors_when_the_trail_overflows() {
    use crate::breadcrumb::layout;
    // The same strip (full = 72) in a 40-wide bar (usable = 32) cannot fit,
    // so the trail slides left: the leading crumbs go off-screen (negative x)
    // and the terminal crumb's right edge sits flush at bar_width - pad = 36.
    let placed = layout(&[10, 20, 30], 40, 4, 6);
    assert_eq!(placed[0].x, -36);
    assert_eq!(placed[1].x, -20);
    assert_eq!(placed[2].x, 6);
    assert_eq!(placed[2].x + i32::try_from(placed[2].width).unwrap(), 36);
}

#[test]
fn breadcrumb_layout_is_empty_for_no_crumbs() {
    assert!(crate::breadcrumb::layout(&[], 200, 4, 6).is_empty());
}

#[test]
fn breadcrumb_crumb_at_resolves_labels_and_rejects_gaps() {
    use crate::breadcrumb::{crumb_at, layout};
    let placed = layout(&[10, 20, 30], 200, 4, 6);
    // A column inside a crumb resolves to it; the right edge is exclusive.
    assert_eq!(crumb_at(&placed, 5, 200), Some(0));
    assert_eq!(crumb_at(&placed, 13, 200), Some(0));
    assert_eq!(crumb_at(&placed, 14, 200), None); // separator gap
    assert_eq!(crumb_at(&placed, 20, 200), Some(1));
    assert_eq!(crumb_at(&placed, 75, 200), Some(2));
    assert_eq!(crumb_at(&placed, 76, 200), None); // just past the last crumb
                                                  // A negative column and a column at/after the bar width never resolve.
    assert_eq!(crumb_at(&placed, -1, 200), None);
    assert_eq!(crumb_at(&placed, 200, 200), None);
}

#[test]
fn breadcrumb_crumb_at_ignores_crumbs_clipped_off_the_left() {
    use crate::breadcrumb::{crumb_at, layout};
    // In the overflow case only the terminal crumb ([6, 36)) is on screen;
    // the off-screen ancestors (negative x) never answer a click.
    let placed = layout(&[10, 20, 30], 40, 4, 6);
    assert_eq!(crumb_at(&placed, 10, 40), Some(2));
    assert_eq!(crumb_at(&placed, 5, 40), None); // gap before the visible crumb
    assert_eq!(crumb_at(&placed, 0, 40), None); // where an off-screen crumb ends
}

#[test]
fn render_crumb_at_mirrors_the_drawn_path_bar() {
    use crate::breadcrumb::SEPARATOR;
    use crate::render::{chrome_height, crumb_at, toolbar_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    // The path bar sits below the toolbar strip; a click lands on a crumb only
    // within that band, so hit-test at its vertical middle.
    let bar_top = toolbar_height(&theme);
    let bar_y = i32::try_from(bar_top + 1).unwrap();
    let vp = Rect::new(0, 0, 200, chrome_height(font, &theme) * 4);
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root opens");

    // A click in the toolbar strip (above the path bar) is never a crumb.
    assert_eq!(crumb_at(&browser, font, &theme, vp, Point::new(4, 0)), None);

    // At the root the only crumb is the current directory, which is inert:
    // no click in the path bar resolves to a navigable crumb.
    assert_eq!(
        crumb_at(&browser, font, &theme, vp, Point::new(4, bar_y)),
        None
    );

    // Descend into /System; now "/" is a navigable ancestor at depth 0 and
    // "System" is the inert current crumb.
    let sys = browser
        .entries()
        .iter()
        .position(|e| e.name() == "System")
        .expect("fixture has System");
    browser.open_index(sys).expect("descend into /System");
    assert_eq!(browser.path(), "/System");
    // The root crumb is drawn at the left inset (x = 4).
    assert_eq!(
        crumb_at(&browser, font, &theme, vp, Point::new(4, bar_y)),
        Some(0)
    );
    // A click on the current "System" crumb (drawn after "/" and the
    // separator) is inert.
    let system_x =
        4 + i32::try_from(font.text_width("/") + font.text_width(SEPARATOR)).unwrap() + 1;
    assert_eq!(
        crumb_at(&browser, font, &theme, vp, Point::new(system_x, bar_y)),
        None
    );
    // A click below the path bar row is never a crumb (it is the item area).
    assert_eq!(
        crumb_at(
            &browser,
            font,
            &theme,
            vp,
            Point::new(4, i32::try_from(chrome_height(font, &theme)).unwrap())
        ),
        None
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
        deny_after_first: BTreeSet::new(),
        reads: BTreeMap::new(),
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
    // The chrome (toolbar + path bar) plus four visible rows.
    let vp = Rect::new(
        0,
        0,
        200,
        crate::render::chrome_height(font, &theme) + row * 4,
    );

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
    use crate::render::entry_index_at;
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(20);
    browser.set_view_mode(ViewMode::Grid);
    let header = crate::render::chrome_height(font, &theme);
    // A window wide and tall enough for several tiles.
    let vp = Rect::new(0, 0, 400, 400);
    let surface = crate::render(&browser, &theme, font, vp, &[]).expect("grid surface");
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

// --- FM4: navigation history and breadcrumb navigation -----------------

#[test]
fn descending_records_back_history_and_go_back_returns() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    assert!(!browser.can_go_back());
    assert!(!browser.can_go_forward());

    // Sorted root order is [Apps, Storage, System, Users]; System is index 2.
    browser.open_index(2).expect("enter System");
    assert_eq!(browser.path(), "/System");
    // The directory left behind is now on the back history.
    assert!(browser.can_go_back());
    assert!(!browser.can_go_forward());

    // Back returns to the root and offers the visited directory forward again.
    assert_eq!(browser.go_back(), Ok(true));
    assert_eq!(browser.path(), "/");
    assert!(browser.is_root());
    assert!(!browser.can_go_back());
    assert!(browser.can_go_forward());
    // The listing came back in the shared sorted order.
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);

    // Forward steps back into the directory we came from.
    assert_eq!(browser.go_forward(), Ok(true));
    assert_eq!(browser.path(), "/System");
    assert!(browser.can_go_back());
    assert!(!browser.can_go_forward());
}

#[test]
fn go_back_and_go_forward_are_no_ops_with_empty_history() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    assert_eq!(browser.go_back(), Ok(false));
    assert_eq!(browser.go_forward(), Ok(false));
    assert!(browser.is_root());
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn go_up_records_history_like_any_navigation() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // Climbing up records /System so Back can return into it.
    assert_eq!(browser.go_up(), Ok(true));
    assert_eq!(browser.path(), "/");
    assert!(browser.can_go_back());
    assert_eq!(browser.go_back(), Ok(true));
    assert_eq!(browser.path(), "/System");
}

#[test]
fn a_fresh_navigation_clears_the_forward_history() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    assert_eq!(browser.go_back(), Ok(true));
    assert!(browser.can_go_forward());

    // A new descent (root → Users) abandons the forward branch, exactly as a
    // web browser's forward history is discarded when you take a new turn.
    browser.open_index(3).expect("enter Users");
    assert_eq!(browser.path(), "/Users");
    assert!(!browser.can_go_forward());
    assert!(browser.can_go_back());
    assert_eq!(browser.go_back(), Ok(true));
    assert_eq!(browser.path(), "/");
}

#[test]
fn navigate_to_depth_climbs_to_a_breadcrumb_ancestor() {
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        encoded_stream(&[(b"System", FileKind::Directory)]),
    );
    dirs.insert(
        "/System".to_string(),
        encoded_stream(&[(b"Fonts", FileKind::Directory)]),
    );
    dirs.insert("/System/Fonts".to_string(), encoded_stream(&[]));
    let mut browser = Browser::open_root(tree_source(dirs)).expect("root");

    browser.open_index(0).expect("into /System");
    browser.open_index(0).expect("into /System/Fonts");
    assert_eq!(browser.path(), "/System/Fonts");
    assert_eq!(browser.components().len(), 2);

    // Clicking the current-directory crumb (depth == len) is a no-op; clicking
    // past the end (no such ancestor) is likewise a no-op, not an error.
    assert_eq!(browser.navigate_to_depth(2), Ok(false));
    assert_eq!(browser.navigate_to_depth(99), Ok(false));
    assert_eq!(browser.path(), "/System/Fonts");

    // Clicking the "System" crumb (depth 1) climbs to that ancestor and
    // records the move so Back returns to where we were.
    assert_eq!(browser.navigate_to_depth(1), Ok(true));
    assert_eq!(browser.path(), "/System");
    assert!(browser.can_go_back());

    // Clicking the root crumb (depth 0) goes all the way to the root.
    assert_eq!(browser.navigate_to_depth(0), Ok(true));
    assert_eq!(browser.path(), "/");
    assert!(browser.is_root());

    assert_eq!(browser.go_back(), Ok(true));
    assert_eq!(browser.path(), "/System");
}

#[test]
fn go_back_is_transactional_when_the_previous_directory_becomes_unreadable() {
    // /System lists once (on the way in) and is refused on every later read,
    // modelling its capability being revoked while we are inside /System/Fonts.
    let mut fs = MockFs::fixture();
    fs.deny_after_first.insert("/System".to_string());
    let mut browser = Browser::open_root(fs).expect("root");
    browser.open_index(2).expect("enter System");
    browser.open_index(0).expect("enter Fonts");
    assert_eq!(browser.path(), "/System/Fonts");

    // Back to the now-unreadable /System fails closed: the browser and its
    // history are left exactly as they were, so Back can still be retried.
    assert_eq!(
        browser.go_back(),
        Err(BrowseError::Source(Errno::PermissionDenied))
    );
    assert_eq!(browser.path(), "/System/Fonts");
    assert!(browser.can_go_back());
    assert!(!browser.can_go_forward());
}

#[test]
fn navigation_history_is_bounded_and_drops_the_oldest() {
    use crate::browser::HISTORY_MAX;

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Alternate root → /System → root … more times than the history bound, so
    // the back stack is driven well past its cap. Each move records exactly
    // one location on the back stack (and clears forward, so forward stays
    // empty throughout the building phase).
    for _ in 0..(HISTORY_MAX + 50) {
        if browser.is_root() {
            browser.open_index(2).expect("enter System");
        } else {
            assert_eq!(browser.go_up(), Ok(true));
        }
    }

    // However far we drove it, the retained history is capped at HISTORY_MAX:
    // exactly that many Back steps succeed before it is exhausted, proving the
    // oldest locations were dropped rather than the stack growing unbounded.
    let mut steps = 0usize;
    while browser.go_back().expect("readable both ways") {
        steps += 1;
    }
    assert_eq!(steps, HISTORY_MAX);
    assert!(!browser.can_go_back());
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

// --- In-place rename (FM5) ---------------------------------------------
//
// The rename model is host-proven end to end over `MockFs`: the injected
// `rename` seam records the two paths it is asked to move between (or refuses),
// and `MockFs::root_after_refresh` supplies the post-rename listing the commit
// re-reads — so validation, the transactional VFS call, and the refresh all run
// without a kernel.

mod rename_model {
    use core::cell::RefCell;

    use alloc::string::ToString;

    use tairix_abi::Errno;
    use tairix_geometry::Rect;
    use tairix_theme::Theme;

    use super::{names, MockFs};
    use crate::browser::Browser;
    use crate::entry::Entry;
    use crate::rename::{validate_new_name, RenameError};

    /// A `MockFs` whose root re-reads as `after` once a commit refreshes it.
    fn fs_with_refreshed_root(after: alloc::vec::Vec<Entry>) -> MockFs {
        let mut fs = MockFs::fixture();
        fs.root_after_refresh = Some(after);
        fs
    }

    #[test]
    fn commit_moves_the_selected_item_and_refreshes_onto_the_new_name() {
        // Root sorts to [Apps, Storage, System, Users]; rename Apps -> Downloads.
        let fs = fs_with_refreshed_root(alloc::vec![
            Entry::directory("Downloads"),
            Entry::directory("Storage"),
            Entry::directory("System"),
            Entry::directory("Users"),
        ]);
        let mut browser = Browser::open_root(fs).expect("root");
        browser.select(0).expect("select Apps");
        assert_eq!(browser.selected_name(), Some("Apps"));

        let seen = RefCell::new(None);
        let result = browser.rename_selected("Downloads", |from, to| {
            *seen.borrow_mut() = Some((from.to_string(), to.to_string()));
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(
            *seen.borrow(),
            Some(("/Apps".to_string(), "/Downloads".to_string()))
        );
        // The listing refreshed and the selection followed the entry.
        assert_eq!(names(&browser), ["Downloads", "Storage", "System", "Users"]);
        assert_eq!(browser.selected_name(), Some("Downloads"));
    }

    #[test]
    fn an_invalid_name_is_refused_before_any_syscall() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");

        for (name, expected) in [
            ("", RenameError::Empty),
            (".", RenameError::Reserved),
            ("..", RenameError::Reserved),
            ("a/b", RenameError::Separator),
            ("bad:name", RenameError::Invalid),
        ] {
            let result = browser.rename_selected(name, |_, _| {
                panic!("the VFS must not be touched for an invalid name");
            });
            assert_eq!(result, Err(expected));
        }
        // The listing is untouched.
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
        assert_eq!(browser.selected_name(), Some("Apps"));
    }

    #[test]
    fn a_clash_with_an_existing_sibling_is_refused_before_any_syscall() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        let result = browser.rename_selected("System", |_, _| {
            panic!("a clashing rename must not reach the VFS");
        });
        assert_eq!(result, Err(RenameError::Clash));
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
    }

    #[test]
    fn renaming_to_the_same_name_is_a_no_op() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        let result = browser.rename_selected("Apps", |_, _| {
            panic!("an unchanged rename must not reach the VFS");
        });
        assert_eq!(result, Err(RenameError::Unchanged));
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
    }

    #[test]
    fn a_vfs_refusal_is_surfaced_and_leaves_the_listing_unchanged() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        let result = browser.rename_selected("Downloads", |_, _| Err(Errno::PermissionDenied));
        assert_eq!(result, Err(RenameError::Refused(Errno::PermissionDenied)));
        // No refresh happened: the original listing and selection stand.
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
        assert_eq!(browser.selected_name(), Some("Apps"));
    }

    #[test]
    fn an_empty_directory_reports_no_selection() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        // Enter the empty /System/Fonts.
        browser.open_index(2).expect("enter System");
        browser
            .open_index(0)
            .expect("enter the empty Fonts directory");
        assert_eq!(browser.selected_name(), None);
        let result = browser.rename_selected("x", |_, _| panic!("nothing to rename"));
        assert_eq!(result, Err(RenameError::NoSelection));
    }

    #[test]
    fn validate_new_name_is_pure_and_covers_the_model_rules() {
        let siblings = alloc::vec![Entry::directory("Apps"), Entry::file("notes.txt")];
        assert_eq!(validate_new_name("Documents", "Apps", &siblings), Ok(()));
        assert_eq!(
            validate_new_name("Apps", "Apps", &siblings),
            Err(RenameError::Unchanged)
        );
        assert_eq!(
            validate_new_name("notes.txt", "Apps", &siblings),
            Err(RenameError::Clash)
        );
    }

    #[test]
    fn every_rename_error_has_a_nonempty_message() {
        for err in [
            RenameError::NoSelection,
            RenameError::Empty,
            RenameError::Reserved,
            RenameError::Separator,
            RenameError::Invalid,
            RenameError::TooLong,
            RenameError::Clash,
            RenameError::Unchanged,
            RenameError::Refused(Errno::PermissionDenied),
            RenameError::Source(Errno::NotFound),
        ] {
            assert!(!err.message().is_empty());
        }
    }

    #[test]
    fn selection_rect_locates_the_selected_row_and_is_none_when_empty() {
        let font = tairix_font::BitmapFont::inconsolata();
        let theme = Theme::dark();
        let viewport = Rect::new(0, 0, 200, 200);

        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(1).expect("select second entry");
        let rect = crate::render::selection_rect(&browser, font, &theme, viewport)
            .expect("a selected row has a rectangle");
        // It lies within the window and below the one-row path-bar header.
        let header = crate::render::row_height(font);
        assert!(rect.origin.y >= i32::try_from(header).unwrap());
        assert!(rect.width > 0 && rect.height > 0);

        // The empty /System/Fonts has no selection, hence no rectangle.
        browser.open_index(2).expect("enter System");
        browser.open_index(0).expect("enter Fonts");
        assert_eq!(
            crate::render::selection_rect(&browser, font, &theme, viewport),
            None
        );
    }
}

// --- FM7b: the new-folder model (validate + commit a directory create) -----
//
// The `mkdir` model runs end to end over the `MockFs` fixture, so every
// validation, transactional, and fail-closed branch of `create_directory` runs
// in `cargo test` without a kernel.

mod mkdir_model {
    use core::cell::RefCell;

    use alloc::string::ToString;

    use tairix_abi::Errno;

    use super::{names, MockFs};
    use crate::browser::Browser;
    use crate::entry::Entry;
    use crate::mkdir::{validate_new_dir_name, MkdirError};

    /// A `MockFs` whose root re-reads as `after` once a commit refreshes it.
    fn fs_with_refreshed_root(after: alloc::vec::Vec<Entry>) -> MockFs {
        let mut fs = MockFs::fixture();
        fs.root_after_refresh = Some(after);
        fs
    }

    #[test]
    fn commit_creates_the_folder_and_follows_the_selection_onto_it() {
        // Root sorts to [Apps, Storage, System, Users]; create Downloads.
        let fs = fs_with_refreshed_root(alloc::vec![
            Entry::directory("Apps"),
            Entry::directory("Downloads"),
            Entry::directory("Storage"),
            Entry::directory("System"),
            Entry::directory("Users"),
        ]);
        let mut browser = Browser::open_root(fs).expect("root");

        let seen = RefCell::new(None);
        let result = browser.create_directory("Downloads", |path| {
            *seen.borrow_mut() = Some(path.to_string());
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(*seen.borrow(), Some("/Downloads".to_string()));
        // The listing refreshed and the selection landed on the new folder,
        // ready for the app's inline rename.
        assert_eq!(
            names(&browser),
            ["Apps", "Downloads", "Storage", "System", "Users"]
        );
        assert_eq!(browser.selected_name(), Some("Downloads"));
    }

    #[test]
    fn an_invalid_name_is_refused_before_any_syscall() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        for (name, expected) in [
            ("", MkdirError::Empty),
            (".", MkdirError::Reserved),
            ("..", MkdirError::Reserved),
            ("a/b", MkdirError::Separator),
            ("bad:name", MkdirError::Invalid),
        ] {
            let result = browser.create_directory(name, |_| {
                panic!("the VFS must not be touched for an invalid name");
            });
            assert_eq!(result, Err(expected));
        }
        // The listing is untouched.
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
    }

    #[test]
    fn a_clash_with_an_existing_sibling_is_refused_before_any_syscall() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        let result = browser.create_directory("System", |_| {
            panic!("a clashing create must not reach the VFS");
        });
        assert_eq!(result, Err(MkdirError::Clash));
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
    }

    #[test]
    fn a_vfs_refusal_is_surfaced_and_leaves_the_listing_unchanged() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        let result = browser.create_directory("Downloads", |_| Err(Errno::PermissionDenied));
        assert_eq!(result, Err(MkdirError::Refused(Errno::PermissionDenied)));
        // No refresh happened: the original listing stands.
        assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
    }

    #[test]
    fn a_create_in_an_empty_directory_needs_no_selection() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        // Enter the empty /System/Fonts — nothing is selected there.
        browser.open_index(2).expect("enter System");
        browser
            .open_index(0)
            .expect("enter the empty Fonts directory");
        assert_eq!(browser.selected_name(), None);

        let seen = RefCell::new(None);
        let result = browser.create_directory("New Folder", |path| {
            *seen.borrow_mut() = Some(path.to_string());
            Ok(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(*seen.borrow(), Some("/System/Fonts/New Folder".to_string()));
    }

    #[test]
    fn a_failed_post_create_relist_is_surfaced() {
        let mut fs = MockFs::fixture();
        // The root lists once (open_root) and then refuses, modelling a
        // directory that becomes unreadable between the create and the refresh.
        fs.deny_after_first.insert("/".to_string());
        let mut browser = Browser::open_root(fs).expect("root");
        let result = browser.create_directory("Downloads", |_| Ok(()));
        assert_eq!(result, Err(MkdirError::Source(Errno::PermissionDenied)));
    }

    #[test]
    fn validate_new_dir_name_is_pure_and_covers_the_model_rules() {
        let siblings = alloc::vec![Entry::directory("Apps"), Entry::file("notes.txt")];
        assert_eq!(validate_new_dir_name("Documents", &siblings), Ok(()));
        assert_eq!(
            validate_new_dir_name("Apps", &siblings),
            Err(MkdirError::Clash)
        );
        assert_eq!(
            validate_new_dir_name("notes.txt", &siblings),
            Err(MkdirError::Clash)
        );
        assert_eq!(validate_new_dir_name("", &siblings), Err(MkdirError::Empty));
        assert_eq!(
            validate_new_dir_name("..", &siblings),
            Err(MkdirError::Reserved)
        );
    }

    #[test]
    fn every_mkdir_error_has_a_nonempty_message() {
        for err in [
            MkdirError::Empty,
            MkdirError::Reserved,
            MkdirError::Separator,
            MkdirError::Invalid,
            MkdirError::TooLong,
            MkdirError::Clash,
            MkdirError::Refused(Errno::PermissionDenied),
            MkdirError::Source(Errno::NotFound),
        ] {
            assert!(!err.message().is_empty());
        }
    }
}

// --- FM8b: the permission-edit model (validate + commit a new mode) --------
//
// The `mode_edit` model runs end to end over the `MockFs` fixture, so every
// validation, transactional, and fail-closed branch of `set_mode_selected`
// runs in `cargo test` without a kernel.

mod mode_edit_model {
    use core::cell::RefCell;

    use alloc::string::ToString;

    use tairix_abi::fs::FS_MODE_MASK;
    use tairix_abi::Errno;

    use super::MockFs;
    use crate::browser::Browser;
    use crate::mode_edit::{validate_mode, ModeError};

    #[test]
    fn commit_applies_the_mode_to_the_selected_node() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        // Sorted root order is [Apps, Storage, System, Users]; select Apps.
        browser.select(0).expect("select Apps");

        let seen = RefCell::new(None);
        let result = browser.set_mode_selected(0o750, |path, mode| {
            *seen.borrow_mut() = Some((path.to_string(), mode));
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(*seen.borrow(), Some(("/Apps".to_string(), 0o750)));
        // The listing is unchanged: a mode change touches no `Entry` field.
        assert_eq!(
            super::names(&browser),
            ["Apps", "Storage", "System", "Users"]
        );
    }

    #[test]
    fn a_mode_carrying_a_bit_above_the_mask_is_refused_before_any_syscall() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        // A file-type bit (above the 0o7777 permission word) is not settable.
        let result = browser.set_mode_selected(FS_MODE_MASK + 1, |_, _| {
            panic!("the VFS must not be touched for an invalid mode");
        });
        assert_eq!(result, Err(ModeError::Invalid));
    }

    #[test]
    fn a_vfs_refusal_is_surfaced_and_the_node_is_unchanged() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        let result = browser.set_mode_selected(0o644, |_, _| Err(Errno::PermissionDenied));
        assert_eq!(result, Err(ModeError::Refused(Errno::PermissionDenied)));
        // The listing stands, exactly as before the refused change.
        assert_eq!(
            super::names(&browser),
            ["Apps", "Storage", "System", "Users"]
        );
    }

    #[test]
    fn an_empty_directory_reports_no_selection() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        // Enter the empty /System/Fonts.
        browser.open_index(2).expect("enter System");
        browser
            .open_index(0)
            .expect("enter the empty Fonts directory");
        assert_eq!(browser.selected_name(), None);
        let result = browser.set_mode_selected(0o644, |_, _| panic!("nothing to change"));
        assert_eq!(result, Err(ModeError::NoSelection));
    }

    #[test]
    fn validate_mode_accepts_the_whole_mask_and_refuses_above_it() {
        // Every bit of the settable permission word is accepted, including the
        // setuid/setgid/sticky bits.
        assert_eq!(validate_mode(0), Ok(()));
        assert_eq!(validate_mode(FS_MODE_MASK), Ok(()));
        assert_eq!(validate_mode(0o755), Ok(()));
        // One bit above the mask fails closed, never masked into a lesser word.
        assert_eq!(validate_mode(FS_MODE_MASK + 1), Err(ModeError::Invalid));
        assert_eq!(validate_mode(0xFFFF_F000), Err(ModeError::Invalid));
    }

    #[test]
    fn every_mode_error_has_a_nonempty_message() {
        for err in [
            ModeError::NoSelection,
            ModeError::Invalid,
            ModeError::Path(Errno::NotFound),
            ModeError::Refused(Errno::PermissionDenied),
        ] {
            assert!(!err.message().is_empty());
        }
    }
}

// --- FM6a: activating an entry (descend / launch a bundle / open a file) --

use crate::activate::Activation;

/// A tree source whose root holds a plain subdirectory, an application
/// bundle, and a regular file — the three activation kinds side by side.
fn activation_source() -> VfsDirectorySource<impl FnMut(&str) -> Result<Vec<u8>, Errno>> {
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        encoded_stream(&[
            (b"Docs", FileKind::Directory),
            (b"Editor.app", FileKind::Directory),
            (b"notes.txt", FileKind::Regular),
        ]),
    );
    dirs.insert("/Docs".to_string(), encoded_stream(&[]));
    tree_source(dirs)
}

#[test]
fn activating_a_directory_descends_into_it() {
    let mut browser = Browser::open_root(activation_source()).expect("root");
    // Default order: the directory first, then the bundle and the file.
    assert_eq!(browser.selected_name(), Some("Docs"));
    assert_eq!(browser.activate_selected(), Ok(Activation::Descended));
    // The engine performed the navigation itself: the listing changed.
    assert_eq!(browser.path(), "/Docs");
    assert!(!browser.is_root());
}

#[test]
fn activating_a_bundle_names_it_for_launch_without_descending() {
    let mut browser = Browser::open_root(activation_source()).expect("root");
    browser.select(1).expect("select Editor.app");
    assert_eq!(browser.selected_name(), Some("Editor.app"));
    // A bundle is a sealed unit: the engine names it for the launcher and does
    // not descend — the browser stays exactly where it was.
    assert_eq!(
        browser.activate_selected(),
        Ok(Activation::LaunchBundle {
            path: "/Editor.app".to_string()
        })
    );
    assert!(browser.is_root());
    assert_eq!(browser.selected_name(), Some("Editor.app"));
}

#[test]
fn activating_a_file_names_it_for_open_without_descending() {
    let mut browser = Browser::open_root(activation_source()).expect("root");
    browser.select(2).expect("select notes.txt");
    assert_eq!(
        browser.activate_selected(),
        Ok(Activation::OpenFile {
            path: "/notes.txt".to_string()
        })
    );
    assert!(browser.is_root());
    assert_eq!(browser.selected_name(), Some("notes.txt"));
}

#[test]
fn activate_index_spells_a_nested_target_path() {
    let mut dirs = BTreeMap::new();
    dirs.insert(
        "/".to_string(),
        encoded_stream(&[(b"System", FileKind::Directory)]),
    );
    dirs.insert(
        "/System".to_string(),
        encoded_stream(&[(b"motd.txt", FileKind::Regular)]),
    );
    let mut browser = Browser::open_root(tree_source(dirs)).expect("root");
    browser.open_index(0).expect("enter /System");
    // The named target is spelled through the one shared path spelling, so it
    // reflects the current directory, not just the leaf name.
    assert_eq!(
        browser.activate_index(0),
        Ok(Activation::OpenFile {
            path: "/System/motd.txt".to_string()
        })
    );
}

#[test]
fn activating_with_no_selection_is_refused() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Descend into the empty /System/Fonts, which has no selection.
    browser.open_index(2).expect("enter System");
    browser.open_index(0).expect("enter the empty Fonts");
    assert_eq!(browser.selected_name(), None);
    assert_eq!(browser.activate_selected(), Err(BrowseError::NoSuchEntry));
}

#[test]
fn activating_an_out_of_range_index_is_refused() {
    let mut browser = Browser::open_root(activation_source()).expect("root");
    assert_eq!(browser.activate_index(99), Err(BrowseError::NoSuchEntry));
    // The browser is untouched by the refused activation.
    assert!(browser.is_root());
}

#[test]
fn activating_an_unreadable_directory_fails_closed_and_stays_put() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // /System/Security exists but is capability-gated (unreadable).
    let names_before: Vec<String> = names(&browser).iter().map(ToString::to_string).collect();
    let security = browser
        .entries()
        .iter()
        .position(|e| e.name() == "Security")
        .expect("Security is listed");
    assert_eq!(
        browser.activate_index(security),
        Err(BrowseError::Source(Errno::PermissionDenied))
    );
    // The descent failed before any state changed: the browser is still on
    // /System, showing the same entries.
    assert_eq!(browser.path(), "/System");
    assert_eq!(names(&browser), names_before);
}

// --- open_with: the "Open With…" type→bundle association model (FM6b) ---

use crate::open_with::{applications_for, mime_for_name, AppAssociation, BundleSource};

/// An in-memory installed-bundle store, the test backing for [`BundleSource`].
struct MockBundleStore {
    bundles: Vec<AppAssociation>,
    denied: bool,
}

impl BundleSource for MockBundleStore {
    fn installed_bundles(&mut self) -> Result<Vec<AppAssociation>, Errno> {
        if self.denied {
            return Err(Errno::PermissionDenied);
        }
        Ok(self.bundles.clone())
    }
}

#[test]
fn mime_for_name_classifies_each_content_class() {
    assert_eq!(mime_for_name("notes.txt"), Some("text/plain"));
    assert_eq!(mime_for_name("main.rs"), Some("text/plain"));
    assert_eq!(mime_for_name("README.md"), Some("text/markdown"));
    assert_eq!(mime_for_name("data.json"), Some("application/json"));
    assert_eq!(mime_for_name("photo.png"), Some("image/png"));
    assert_eq!(mime_for_name("scan.jpeg"), Some("image/jpeg"));
    assert_eq!(mime_for_name("logo.svg"), Some("image/svg+xml"));
    assert_eq!(mime_for_name("backup.tar"), Some("application/x-tar"));
    assert_eq!(mime_for_name("bundle.tgz"), Some("application/gzip"));
    assert_eq!(mime_for_name("tool.rxe"), Some("application/x-tairix-rxe"));
    assert_eq!(mime_for_name("mod.wasm"), Some("application/wasm"));
}

#[test]
fn mime_for_name_is_case_insensitive_on_the_extension() {
    assert_eq!(mime_for_name("PHOTO.PNG"), Some("image/png"));
    assert_eq!(mime_for_name("Notes.TxT"), Some("text/plain"));
}

#[test]
fn mime_for_name_fails_closed_on_an_unrecognised_or_absent_extension() {
    // Unknown extension, no extension, a dotfile with no further extension, and
    // a trailing dot all yield no type — never a guess.
    assert_eq!(mime_for_name("mystery.xyz"), None);
    assert_eq!(mime_for_name("Makefile"), None);
    assert_eq!(mime_for_name(".profile"), None);
    assert_eq!(mime_for_name("archive."), None);
    assert_eq!(mime_for_name(""), None);
}

#[test]
fn handles_matches_a_declared_type_case_insensitively() {
    let assoc = AppAssociation::new(
        "viewer",
        "/System/Apps/viewer.app",
        vec!["text/plain".to_string(), "text/markdown".to_string()],
    );
    assert!(assoc.handles("text/plain"));
    assert!(assoc.handles("TEXT/PLAIN"));
    assert!(!assoc.handles("image/png"));
    assert_eq!(assoc.name(), "viewer");
    assert_eq!(assoc.bundle_path(), "/System/Apps/viewer.app");
    assert_eq!(assoc.mime_types(), ["text/plain", "text/markdown"]);
}

/// A store with a text viewer, an image viewer, and a "studio" that claims
/// both — the shapes the match / bundle / none cases need.
fn open_with_store() -> MockBundleStore {
    MockBundleStore {
        bundles: vec![
            AppAssociation::new(
                "viewer",
                "/System/Apps/viewer.app",
                vec!["text/plain".to_string()],
            ),
            AppAssociation::new(
                "images",
                "/Apps/images.app",
                vec!["image/png".to_string(), "image/jpeg".to_string()],
            ),
            AppAssociation::new(
                "studio",
                "/Apps/studio.app",
                vec!["text/plain".to_string(), "image/png".to_string()],
            ),
        ],
        denied: false,
    }
}

#[test]
fn applications_for_offers_every_bundle_that_claims_the_type_in_order() {
    let mut store = open_with_store();
    let bundles = store.installed_bundles().expect("enumerate");
    // A text file is offered the text viewer and the studio, in enumeration
    // order — never the image-only bundle.
    let names: Vec<&str> = applications_for("notes.txt", &bundles)
        .iter()
        .map(|b| b.name())
        .collect();
    assert_eq!(names, ["viewer", "studio"]);
}

#[test]
fn applications_for_offers_a_single_matching_bundle() {
    let mut store = open_with_store();
    let bundles = store.installed_bundles().expect("enumerate");
    let matches = applications_for("scan.jpeg", &bundles);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name(), "images");
    assert_eq!(matches[0].bundle_path(), "/Apps/images.app");
}

#[test]
fn applications_for_is_empty_when_no_bundle_claims_a_known_type() {
    let mut store = open_with_store();
    let bundles = store.installed_bundles().expect("enumerate");
    // A recognised type (gzip archive) that no installed bundle handles is an
    // honest "no application" answer, not a fabricated default.
    assert!(applications_for("backup.tgz", &bundles).is_empty());
}

#[test]
fn applications_for_is_empty_for_an_unrecognised_type() {
    let mut store = open_with_store();
    let bundles = store.installed_bundles().expect("enumerate");
    // The file's type cannot be derived, so nothing is offered even though
    // bundles exist.
    assert!(applications_for("mystery.xyz", &bundles).is_empty());
    assert!(applications_for("Makefile", &bundles).is_empty());
}

#[test]
fn bundle_source_propagates_a_refused_enumeration() {
    let mut store = MockBundleStore {
        bundles: Vec::new(),
        denied: true,
    };
    assert_eq!(
        store.installed_bundles().err(),
        Some(Errno::PermissionDenied)
    );
}

// ---------------------------------------------------------------------------
// FM7 — multi-selection and the cut/copy clipboard (pure engine model).
// ---------------------------------------------------------------------------

/// Build a root-first component path from string literals.
fn comps(parts: &[&str]) -> Vec<String> {
    parts.iter().copied().map(str::to_string).collect()
}

/// The selected indices, low-to-high, as a `Vec` for assertions.
fn selected(selection: &Selection) -> Vec<usize> {
    selection.iter().collect()
}

#[test]
fn selection_single_replaces_and_sets_the_anchor() {
    let mut s = Selection::new();
    s.single(3);
    assert_eq!(selected(&s), [3]);
    assert_eq!(s.anchor(), Some(3));
    s.single(1);
    assert_eq!(selected(&s), [1]);
    assert_eq!(s.anchor(), Some(1));
}

#[test]
fn selection_toggle_adds_then_removes_and_moves_the_anchor() {
    let mut s = Selection::new();
    s.toggle(2);
    assert!(s.contains(2));
    assert_eq!(s.anchor(), Some(2));
    s.toggle(4);
    assert_eq!(selected(&s), [2, 4]);
    assert_eq!(s.anchor(), Some(4));
    s.toggle(2);
    assert_eq!(selected(&s), [4]);
    // Un-selecting still moves the anchor to the acted-on entry.
    assert_eq!(s.anchor(), Some(2));
}

#[test]
fn selection_range_to_covers_both_directions_and_keeps_the_anchor() {
    let mut s = Selection::new();
    s.single(2);
    s.range_to(5);
    assert_eq!(selected(&s), [2, 3, 4, 5]);
    assert_eq!(s.anchor(), Some(2));
    // A second shift-click re-grows from the same anchor, replacing the range.
    s.range_to(0);
    assert_eq!(selected(&s), [0, 1, 2]);
    assert_eq!(s.anchor(), Some(2));
}

#[test]
fn selection_range_to_without_an_anchor_is_a_single_select() {
    let mut s = Selection::new();
    s.range_to(4);
    assert_eq!(selected(&s), [4]);
    assert_eq!(s.anchor(), Some(4));
}

#[test]
fn selection_select_all_selects_the_range_and_empty_stays_empty() {
    let mut s = Selection::new();
    s.select_all(3);
    assert_eq!(selected(&s), [0, 1, 2]);
    assert_eq!(s.anchor(), Some(0));
    s.select_all(0);
    assert!(s.is_empty());
    assert_eq!(s.anchor(), None);
}

#[test]
fn selection_clear_drops_everything() {
    let mut s = Selection::new();
    s.select_all(4);
    s.clear();
    assert!(s.is_empty());
    assert_eq!(s.anchor(), None);
}

#[test]
fn open_root_selects_the_focused_entry() {
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    assert_eq!(selected(browser.selection()), [0]);
    assert!(browser.is_selected(0));
    assert!(!browser.is_selected(1));
}

#[test]
fn select_all_selects_every_entry_in_the_listing() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select_all();
    assert_eq!(selected(browser.selection()), [0, 1, 2, 3]);
}

#[test]
fn toggle_and_extend_build_a_multi_selection() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select(0).expect("focus 0");
    browser.toggle_selection(2).expect("toggle 2");
    assert_eq!(selected(browser.selection()), [0, 2]);
    // Extend grows from the toggle's anchor (2) to 3.
    browser.extend_selection_to(3).expect("extend 3");
    assert_eq!(selected(browser.selection()), [2, 3]);
    assert_eq!(browser.selected_index(), Some(3));
}

#[test]
fn out_of_range_selection_ops_are_refused_and_change_nothing() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select(1).expect("focus 1");
    assert_eq!(browser.toggle_selection(99), Err(BrowseError::NoSuchEntry));
    assert_eq!(
        browser.extend_selection_to(99),
        Err(BrowseError::NoSuchEntry)
    );
    assert_eq!(selected(browser.selection()), [1]);
    assert_eq!(browser.selected_index(), Some(1));
}

#[test]
fn an_unmodified_move_collapses_a_multi_selection() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select_all();
    assert_eq!(browser.selection().len(), 4);
    browser.select_next();
    assert_eq!(selected(browser.selection()), [1]);
}

#[test]
fn navigation_collapses_the_selection_to_the_focus() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select_all();
    // Enter /System (index 2 in the sorted root).
    browser.open_index(2).expect("enter System");
    assert_eq!(browser.path(), "/System");
    assert_eq!(selected(browser.selection()), [0]);
}

#[test]
fn a_reorder_collapses_the_selection_to_the_focus() {
    use crate::sort::{SortDirection, SortKey, SortMode};
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select_all();
    browser.set_sort_mode(SortMode {
        key: SortKey::Name,
        direction: SortDirection::Descending,
    });
    assert_eq!(browser.selection().len(), 1);
}

#[test]
fn an_empty_directory_has_an_empty_selection() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // /System index 0 is Fonts, an empty directory.
    browser.open_index(0).expect("enter Fonts");
    assert_eq!(browser.path(), "/System/Fonts");
    assert!(browser.entries().is_empty());
    assert!(browser.selection().is_empty());
    assert!(browser.clipboard(ClipboardOp::Copy).is_none());
}

#[test]
fn clipboard_captures_the_selected_entries_absolute_paths() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // /System sorted: [Fonts, Security, Kernel]. Select Fonts and Kernel.
    browser.select(0).expect("focus Fonts");
    browser.toggle_selection(2).expect("also Kernel");
    let clipboard = browser.clipboard(ClipboardOp::Cut).expect("clipboard");
    assert_eq!(clipboard.op(), ClipboardOp::Cut);
    assert_eq!(
        clipboard.items(),
        &[comps(&["System", "Fonts"]), comps(&["System", "Kernel"])]
    );
}

#[test]
fn clipboard_is_none_when_nothing_is_selected() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.clear_selection();
    assert!(browser.selection().is_empty());
    assert!(browser.clipboard(ClipboardOp::Copy).is_none());
}

#[test]
fn clipboard_new_refuses_empty_or_root_items() {
    assert!(Clipboard::new(ClipboardOp::Copy, Vec::new()).is_none());
    // A root (empty component) item is not a real entry.
    assert!(Clipboard::new(ClipboardOp::Copy, vec![Vec::new()]).is_none());
    assert!(Clipboard::new(ClipboardOp::Copy, vec![comps(&["a"]), Vec::new()]).is_none());
    let ok = Clipboard::new(ClipboardOp::Copy, vec![comps(&["a"])]).expect("built");
    assert_eq!(ok.len(), 1);
    assert!(!ok.is_empty());
}

#[test]
fn plan_paste_maps_each_source_into_the_target() {
    let clipboard = Clipboard::new(
        ClipboardOp::Cut,
        vec![comps(&["Users", "alice"]), comps(&["System", "Kernel"])],
    )
    .expect("clipboard");
    let plan = plan_paste(&clipboard, &comps(&["Storage"])).expect("plan");
    assert_eq!(plan.op(), ClipboardOp::Cut);
    assert_eq!(plan.items().len(), 2);
    assert_eq!(
        plan.items()[0].source(),
        comps(&["Users", "alice"]).as_slice()
    );
    assert_eq!(
        plan.items()[0].dest(),
        comps(&["Storage", "alice"]).as_slice()
    );
    assert!(!plan.items()[0].overwrites_source());
    assert_eq!(
        plan.items()[1].dest(),
        comps(&["Storage", "Kernel"]).as_slice()
    );
}

#[test]
fn plan_paste_flags_a_paste_back_into_the_same_directory() {
    let clipboard =
        Clipboard::new(ClipboardOp::Copy, vec![comps(&["System", "Fonts"])]).expect("clipboard");
    // Paste into /System, where Fonts already lives.
    let plan = plan_paste(&clipboard, &comps(&["System"])).expect("plan");
    let item = &plan.items()[0];
    assert_eq!(item.dest(), comps(&["System", "Fonts"]).as_slice());
    assert!(item.overwrites_source());
}

#[test]
fn plan_paste_refuses_a_folder_into_itself_or_a_descendant() {
    let clipboard = Clipboard::new(ClipboardOp::Cut, vec![comps(&["System"])]).expect("clipboard");
    // Into itself.
    assert_eq!(
        plan_paste(&clipboard, &comps(&["System"])),
        Err(PasteError::WouldRecurse)
    );
    // Into a descendant.
    assert_eq!(
        plan_paste(&clipboard, &comps(&["System", "Fonts"])),
        Err(PasteError::WouldRecurse)
    );
    // A sibling prefix (`/Systematic`) is not a descendant.
    assert!(plan_paste(&clipboard, &comps(&["Systematic"])).is_ok());
}

#[test]
fn paste_error_message_is_non_empty() {
    assert!(!PasteError::WouldRecurse.to_string().is_empty());
}

// ---- FM7b: the paste-execution model (`execute`) ----

/// Two distinct volume ids so the move-vs-copy tests read clearly.
fn vol(tag: u8) -> VolumeId {
    let mut bytes = [0u8; 16];
    bytes[0] = tag;
    VolumeId::new(bytes)
}

#[test]
fn volume_id_round_trips_its_bytes_and_compares() {
    let bytes = *b"0123456789abcdef";
    assert_eq!(VolumeId::new(bytes).bytes(), bytes);
    assert_eq!(vol(1), vol(1));
    assert_ne!(vol(1), vol(2));
}

#[test]
fn a_copy_always_streams_regardless_of_volume() {
    assert_eq!(
        paste_strategy(ClipboardOp::Copy, vol(1), vol(1)),
        PasteStrategy::Copy
    );
    assert_eq!(
        paste_strategy(ClipboardOp::Copy, vol(1), vol(2)),
        PasteStrategy::Copy
    );
}

#[test]
fn a_cut_within_one_volume_renames() {
    assert_eq!(
        paste_strategy(ClipboardOp::Cut, vol(7), vol(7)),
        PasteStrategy::Rename
    );
}

#[test]
fn a_cut_across_volumes_copies_then_deletes() {
    assert_eq!(
        paste_strategy(ClipboardOp::Cut, vol(1), vol(2)),
        PasteStrategy::CopyThenDelete
    );
}

#[test]
fn an_empty_source_needs_no_chunk_and_is_complete() {
    let cursor = CopyCursor::new(0);
    assert!(cursor.is_complete());
    assert_eq!(cursor.remaining(), 0);
    assert_eq!(cursor.next_chunk(), None);
}

#[test]
fn a_small_source_is_one_short_chunk() {
    let cursor = CopyCursor::new(10);
    let chunk = cursor.next_chunk().expect("a chunk");
    assert_eq!(chunk.offset(), 0);
    assert_eq!(chunk.len(), 10);
    assert!(!chunk.is_empty());
}

#[test]
fn a_large_source_is_walked_in_bounded_chunks_to_completion() {
    let total = COPY_CHUNK_LEN * 2 + 5;
    let mut cursor = CopyCursor::new(total);

    let first = cursor.next_chunk().expect("first");
    assert_eq!(first.offset(), 0);
    assert_eq!(first.len(), COPY_CHUNK_LEN);
    cursor.advance(first.len()).expect("advance first");

    let second = cursor.next_chunk().expect("second");
    assert_eq!(second.offset(), COPY_CHUNK_LEN);
    assert_eq!(second.len(), COPY_CHUNK_LEN);
    cursor.advance(second.len()).expect("advance second");

    let third = cursor.next_chunk().expect("third");
    assert_eq!(third.offset(), COPY_CHUNK_LEN * 2);
    assert_eq!(third.len(), 5);
    cursor.advance(third.len()).expect("advance third");

    assert!(cursor.is_complete());
    assert_eq!(cursor.copied(), total);
    assert_eq!(cursor.next_chunk(), None);
}

#[test]
fn a_short_transfer_advances_only_by_what_moved() {
    let mut cursor = CopyCursor::new(10);
    // The read carried 4 of the 10 bytes it was asked for.
    cursor.advance(4).expect("short advance");
    assert_eq!(cursor.copied(), 4);
    let next = cursor.next_chunk().expect("remainder");
    assert_eq!(next.offset(), 4);
    assert_eq!(next.len(), 6);
    cursor.advance(6).expect("finish");
    assert!(cursor.is_complete());
}

#[test]
fn a_cursor_resumes_from_a_persisted_offset() {
    let mut cursor = CopyCursor::resume(100, 40).expect("resume");
    assert_eq!(cursor.copied(), 40);
    assert_eq!(cursor.remaining(), 60);
    let chunk = cursor.next_chunk().expect("chunk");
    assert_eq!(chunk.offset(), 40);
    assert_eq!(chunk.len(), 60);
    cursor.advance(60).expect("finish");
    assert!(cursor.is_complete());
}

#[test]
fn resuming_past_the_total_is_overrun() {
    assert_eq!(CopyCursor::resume(10, 11), Err(CopyError::Overrun));
    // Resuming exactly at the end is a complete, valid cursor.
    let done = CopyCursor::resume(10, 10).expect("at end");
    assert!(done.is_complete());
    assert_eq!(done.next_chunk(), None);
}

#[test]
fn advancing_past_the_source_length_fails_closed() {
    let mut cursor = CopyCursor::new(10);
    assert_eq!(cursor.advance(11), Err(CopyError::Overrun));
    // The cursor is left untouched by the refused advance.
    assert_eq!(cursor.copied(), 0);
    // A valid advance to the exact end still works afterwards.
    cursor.advance(10).expect("advance to end");
    assert!(cursor.is_complete());
    assert_eq!(cursor.advance(1), Err(CopyError::Overrun));
}

#[test]
fn copy_error_message_is_non_empty() {
    assert!(!CopyError::Overrun.to_string().is_empty());
}

// ---------------------------------------------------------------------------
// FM4b — the toolbar + breadcrumb frame model (pure chrome model).
// ---------------------------------------------------------------------------

#[test]
fn the_toolbar_disables_back_forward_and_up_at_the_root() {
    use crate::chrome::{ToolbarCommand, ToolbarModel};

    // At the root, fresh: no history either way and no parent to climb to, so
    // the three navigation tools render disabled; refresh, view, and sort are
    // always actionable.
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let toolbar = ToolbarModel::for_browser(&browser);
    assert!(!toolbar.is_enabled(ToolbarCommand::Back));
    assert!(!toolbar.is_enabled(ToolbarCommand::Forward));
    assert!(!toolbar.is_enabled(ToolbarCommand::Up));
    assert!(toolbar.is_enabled(ToolbarCommand::Refresh));
    assert!(toolbar.is_enabled(ToolbarCommand::ToggleView));
    assert!(toolbar.is_enabled(ToolbarCommand::Sort));
}

#[test]
fn the_toolbar_enables_back_and_up_after_descending() {
    use crate::chrome::{ToolbarCommand, ToolbarModel};

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    let toolbar = ToolbarModel::for_browser(&browser);
    // We can climb to the parent and go back to the root, but there is nothing
    // ahead of us yet.
    assert!(toolbar.is_enabled(ToolbarCommand::Up));
    assert!(toolbar.is_enabled(ToolbarCommand::Back));
    assert!(!toolbar.is_enabled(ToolbarCommand::Forward));
}

#[test]
fn the_toolbar_enables_forward_only_after_going_back() {
    use crate::chrome::{ToolbarCommand, ToolbarModel};

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    assert_eq!(browser.go_back(), Ok(true));
    let toolbar = ToolbarModel::for_browser(&browser);
    // Back to the root: Forward is now available, Back and Up are not.
    assert!(toolbar.is_enabled(ToolbarCommand::Forward));
    assert!(!toolbar.is_enabled(ToolbarCommand::Back));
    assert!(!toolbar.is_enabled(ToolbarCommand::Up));
}

#[test]
fn the_toolbar_reports_the_active_view_and_sort() {
    use crate::chrome::ToolbarModel;
    use crate::layout::ViewMode;
    use crate::sort::{SortDirection, SortKey, SortMode};

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    let toolbar = ToolbarModel::for_browser(&browser);
    assert_eq!(toolbar.view_mode(), ViewMode::List);
    assert_eq!(toolbar.sort_mode(), SortMode::default_order());

    browser.set_view_mode(ViewMode::Grid);
    let by_size_desc = SortMode {
        key: SortKey::Size,
        direction: SortDirection::Descending,
    };
    browser.set_sort_mode(by_size_desc);
    let toolbar = ToolbarModel::for_browser(&browser);
    assert_eq!(toolbar.view_mode(), ViewMode::Grid);
    assert_eq!(toolbar.sort_mode(), by_size_desc);
}

#[test]
fn toolbar_commands_list_covers_every_variant_once() {
    use crate::chrome::{ToolbarCommand, TOOLBAR_COMMANDS};

    // The drawn chrome iterates TOOLBAR_COMMANDS, so it must hold each command
    // exactly once, in a stable order.
    assert_eq!(
        TOOLBAR_COMMANDS,
        &[
            ToolbarCommand::Back,
            ToolbarCommand::Forward,
            ToolbarCommand::Up,
            ToolbarCommand::Refresh,
            ToolbarCommand::ToggleView,
            ToolbarCommand::Sort,
        ]
    );
}

#[test]
fn the_context_menu_needs_a_selection_for_the_item_commands() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    // An empty directory offers no selection, so every command that acts on a
    // selected entry renders disabled; Paste still depends only on the held
    // clipboard.
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    browser.open_index(0).expect("enter the empty Fonts");
    assert_eq!(browser.selected_name(), None);

    let menu = ContextMenuModel::for_browser(&browser, false);
    for command in [
        ContextCommand::Open,
        ContextCommand::OpenWith,
        ContextCommand::Rename,
        ContextCommand::Cut,
        ContextCommand::Copy,
        ContextCommand::Properties,
    ] {
        assert!(!menu.is_enabled(command), "{command:?} without a selection");
    }
    assert!(!menu.is_enabled(ContextCommand::Paste));
}

#[test]
fn the_context_menu_enables_item_commands_on_a_directory_but_not_open_with() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    // A directory descends on Open; it has no application to "open with".
    let browser = Browser::open_root(activation_source()).expect("root");
    assert_eq!(browser.selected_name(), Some("Docs"));
    let menu = ContextMenuModel::for_browser(&browser, false);
    assert!(menu.is_enabled(ContextCommand::Open));
    assert!(menu.is_enabled(ContextCommand::Rename));
    assert!(menu.is_enabled(ContextCommand::Cut));
    assert!(menu.is_enabled(ContextCommand::Copy));
    assert!(menu.is_enabled(ContextCommand::Properties));
    assert!(!menu.is_enabled(ContextCommand::OpenWith));
}

#[test]
fn the_context_menu_disables_open_with_on_a_bundle() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    // A bundle launches itself; there is no application to choose for it.
    let mut browser = Browser::open_root(activation_source()).expect("root");
    browser.select(1).expect("select Editor.app");
    assert!(browser.selected_entry().expect("bundle").is_bundle());
    let menu = ContextMenuModel::for_browser(&browser, false);
    assert!(menu.is_enabled(ContextCommand::Open));
    assert!(!menu.is_enabled(ContextCommand::OpenWith));
}

#[test]
fn the_context_menu_enables_open_with_only_on_a_file() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    let mut browser = Browser::open_root(activation_source()).expect("root");
    browser.select(2).expect("select notes.txt");
    assert_eq!(browser.selected_name(), Some("notes.txt"));
    let menu = ContextMenuModel::for_browser(&browser, false);
    assert!(menu.is_enabled(ContextCommand::Open));
    assert!(menu.is_enabled(ContextCommand::OpenWith));
}

#[test]
fn the_context_menu_enables_paste_only_when_a_clipboard_is_held() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    // Paste targets the current directory and needs a held clipboard, not a
    // selection: the app threads its own clipboard state in.
    let browser = Browser::open_root(activation_source()).expect("root");
    assert!(!ContextMenuModel::for_browser(&browser, false).is_enabled(ContextCommand::Paste));
    assert!(ContextMenuModel::for_browser(&browser, true).is_enabled(ContextCommand::Paste));
}

#[test]
fn context_commands_list_covers_every_variant_once() {
    use crate::chrome::{ContextCommand, CONTEXT_COMMANDS};

    // The drawn menu iterates CONTEXT_COMMANDS, so it must hold each command
    // exactly once, in a stable order. Delete and New Folder are absent — their
    // engine action does not exist yet, so they are not modelled here.
    assert_eq!(
        CONTEXT_COMMANDS,
        &[
            ContextCommand::Open,
            ContextCommand::OpenWith,
            ContextCommand::Rename,
            ContextCommand::Cut,
            ContextCommand::Copy,
            ContextCommand::Paste,
            ContextCommand::Properties,
        ]
    );
}

#[test]
fn the_breadcrumbs_at_the_root_are_a_single_current_root_crumb() {
    use crate::chrome::breadcrumbs;

    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let crumbs = breadcrumbs(&browser);
    assert_eq!(crumbs.len(), 1);
    assert_eq!(crumbs[0].label(), "/");
    assert_eq!(crumbs[0].depth(), 0);
    assert!(crumbs[0].is_current());
}

#[test]
fn the_breadcrumbs_track_the_components_and_bind_each_to_its_depth() {
    use crate::chrome::breadcrumbs;

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    browser.open_index(0).expect("enter Fonts");
    assert_eq!(browser.path(), "/System/Fonts");

    let crumbs = breadcrumbs(&browser);
    // Root crumb + one per component, root-first.
    assert_eq!(crumbs.len(), 3);
    assert_eq!(crumbs[0].label(), "/");
    assert_eq!(crumbs[0].depth(), 0);
    assert!(!crumbs[0].is_current());
    assert_eq!(crumbs[1].label(), "System");
    assert_eq!(crumbs[1].depth(), 1);
    assert!(!crumbs[1].is_current());
    // The terminal crumb is the directory being shown: it is current and its
    // depth equals the number of components (a navigate_to_depth no-op).
    assert_eq!(crumbs[2].label(), "Fonts");
    assert_eq!(crumbs[2].depth(), 2);
    assert!(crumbs[2].is_current());
    assert_eq!(crumbs[2].depth(), browser.components().len());
}

#[test]
fn a_breadcrumb_depth_climbs_to_exactly_the_ancestor_it_names() {
    use crate::chrome::breadcrumbs;

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    browser.open_index(0).expect("enter Fonts");

    // Binding the "System" crumb (depth 1) to navigate_to_depth climbs to it.
    let system_depth = {
        let crumbs = breadcrumbs(&browser);
        crumbs[1].depth()
    };
    assert_eq!(browser.navigate_to_depth(system_depth), Ok(true));
    assert_eq!(browser.path(), "/System");
}

// ---------------------------------------------------------------------------
// FM4b — the drawn toolbar: command dispatch, glyphs, and pointer resolution.
// ---------------------------------------------------------------------------

#[test]
fn view_mode_toggled_swaps_list_and_grid() {
    use crate::layout::ViewMode;
    assert_eq!(ViewMode::List.toggled(), ViewMode::Grid);
    assert_eq!(ViewMode::Grid.toggled(), ViewMode::List);
}

#[test]
fn sort_mode_next_cycles_through_all_six_modes_and_wraps() {
    use crate::sort::SortMode;
    // The Sort command walks every (key, direction) once, in a fixed order,
    // then returns to the start — a total cycle with no unreachable mode.
    let start = SortMode::default_order();
    let mut mode = start;
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(mode);
        mode = mode.next();
    }
    // Back to the start after six steps.
    assert_eq!(mode, start);
    // All six are distinct.
    for (i, a) in seen.iter().enumerate() {
        for b in &seen[i + 1..] {
            assert_ne!(a, b, "sort cycle repeated a mode early");
        }
    }
    assert_eq!(seen.len(), 6);
}

#[test]
fn toolbar_command_icon_maps_each_command_to_a_distinct_glyph() {
    use crate::chrome::{ToolbarCommand, TOOLBAR_COMMANDS};
    use tairix_icon::IconKind;

    // Each command draws its own glyph and no two share one, so the toolbar
    // reads unambiguously.
    assert_eq!(ToolbarCommand::Back.icon(), IconKind::NavBack);
    assert_eq!(ToolbarCommand::Forward.icon(), IconKind::NavForward);
    assert_eq!(ToolbarCommand::Up.icon(), IconKind::NavUp);
    assert_eq!(ToolbarCommand::Refresh.icon(), IconKind::Refresh);
    assert_eq!(ToolbarCommand::ToggleView.icon(), IconKind::ViewToggle);
    assert_eq!(ToolbarCommand::Sort.icon(), IconKind::Sort);

    let icons: Vec<IconKind> = TOOLBAR_COMMANDS.iter().map(|c| c.icon()).collect();
    for (i, a) in icons.iter().enumerate() {
        for b in &icons[i + 1..] {
            assert_ne!(a, b, "two toolbar commands share a glyph");
        }
    }
}

#[test]
fn apply_command_drives_navigation_view_and_sort() {
    use crate::apply_command;
    use crate::chrome::ToolbarCommand;
    use crate::layout::ViewMode;

    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Back at the root has no history: a no-op, not an error, and no change.
    assert_eq!(apply_command(&mut browser, ToolbarCommand::Back), Ok(false));

    // Toggle view flips list ↔ grid and reports a change.
    assert_eq!(browser.view_mode(), ViewMode::List);
    assert_eq!(
        apply_command(&mut browser, ToolbarCommand::ToggleView),
        Ok(true)
    );
    assert_eq!(browser.view_mode(), ViewMode::Grid);

    // Sort advances to the next mode in the cycle.
    let before = browser.sort_mode();
    assert_eq!(apply_command(&mut browser, ToolbarCommand::Sort), Ok(true));
    assert_eq!(browser.sort_mode(), before.next());

    // Descend, then Up climbs back to the root. (The Sort above re-ordered the
    // listing, so find System by name rather than a fixed index.)
    let sys = browser
        .entries()
        .iter()
        .position(|e| e.name() == "System")
        .expect("fixture has System");
    browser.open_index(sys).expect("enter System");
    assert_eq!(browser.path(), "/System");
    assert_eq!(apply_command(&mut browser, ToolbarCommand::Up), Ok(true));
    assert!(browser.is_root());

    // Back now returns to /System (there is history), reporting a change.
    assert_eq!(apply_command(&mut browser, ToolbarCommand::Back), Ok(true));
    assert_eq!(browser.path(), "/System");
}

#[test]
fn apply_command_refresh_fails_closed_and_leaves_the_browser_put() {
    use crate::apply_command;
    use crate::chrome::ToolbarCommand;

    // The root lists once (at open) and is refused on every later read.
    let mut fs = MockFs::fixture();
    fs.deny_after_first.insert("/".to_string());
    let mut browser = Browser::open_root(fs).expect("root lists once");
    let before: Vec<String> = names(&browser).iter().map(ToString::to_string).collect();

    // Refresh re-reads the root, which now fails closed; the error is surfaced
    // and the previously loaded listing is left exactly as it was.
    assert!(matches!(
        apply_command(&mut browser, ToolbarCommand::Refresh),
        Err(BrowseError::Source(_))
    ));
    assert_eq!(names(&browser), before);
}

#[test]
fn render_toolbar_command_at_resolves_enabled_commands_and_fails_closed() {
    use crate::chrome::ToolbarCommand;
    use crate::render::{chrome_height, toolbar_command_at, toolbar_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, chrome_height(font, &theme) + 40);

    // Scan the toolbar strip's middle row and collect every command a click
    // resolves to, so the test does not depend on each tool's exact pixel x.
    let commands_along_toolbar = |browser: &Browser<MockFs>| -> Vec<ToolbarCommand> {
        let y = i32::try_from(toolbar_height(&theme) / 2).unwrap();
        let mut found = Vec::new();
        for x in 0..vp.width {
            if let Some(cmd) = toolbar_command_at(
                browser,
                &theme,
                vp,
                Point::new(i32::try_from(x).unwrap(), y),
            ) {
                if !found.contains(&cmd) {
                    found.push(cmd);
                }
            }
        }
        found
    };

    // At the root the three navigation tools are disabled, so a click on them
    // resolves to nothing (fail closed); the always-enabled tools resolve.
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let at_root = commands_along_toolbar(&browser);
    assert!(!at_root.contains(&ToolbarCommand::Back));
    assert!(!at_root.contains(&ToolbarCommand::Forward));
    assert!(!at_root.contains(&ToolbarCommand::Up));
    assert!(at_root.contains(&ToolbarCommand::Refresh));
    assert!(at_root.contains(&ToolbarCommand::ToggleView));
    assert!(at_root.contains(&ToolbarCommand::Sort));

    // After descending, Back and Up become enabled and now resolve.
    let mut deep = Browser::open_root(MockFs::fixture()).expect("root");
    deep.open_index(2).expect("enter System");
    let at_deep = commands_along_toolbar(&deep);
    assert!(at_deep.contains(&ToolbarCommand::Back));
    assert!(at_deep.contains(&ToolbarCommand::Up));
    assert!(!at_deep.contains(&ToolbarCommand::Forward));

    // A click below the toolbar strip (the path bar / item area) is never a
    // toolbar command.
    assert_eq!(
        toolbar_command_at(
            &browser,
            &theme,
            vp,
            Point::new(4, i32::try_from(toolbar_height(&theme)).unwrap())
        ),
        None
    );
}

#[test]
fn render_manager_tool_at_resolves_new_folder_disjoint_from_the_read_only_commands() {
    use crate::chrome::{ManagerTool, MANAGER_TOOLS};
    use crate::render::{manager_tool_at, toolbar_command_at, toolbar_height};
    use tairix_geometry::Point;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, crate::render::chrome_height(font, &theme) + 40);
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let y = i32::try_from(toolbar_height(&theme) / 2).unwrap();

    // Scan the toolbar's middle row: the manager write tool resolves somewhere,
    // and no pixel resolves to *both* a read-only command and a write tool —
    // the two hit-tests cover disjoint regions (§2.2).
    let mut saw_new_folder = false;
    for x in 0..vp.width {
        let point = Point::new(i32::try_from(x).unwrap(), y);
        let command = toolbar_command_at(&browser, &theme, vp, point);
        let tool = manager_tool_at(&browser, &theme, vp, point, MANAGER_TOOLS);
        assert!(
            !(command.is_some() && tool.is_some()),
            "a pixel resolved to both a command and a write tool"
        );
        if tool == Some(ManagerTool::NewFolder) {
            saw_new_folder = true;
        }
    }
    assert!(
        saw_new_folder,
        "the New Folder tool is drawn and hit-testable"
    );

    // The read-only picker hands no write tools, so none is ever resolved —
    // the type separation keeps a write action out of the picker entirely.
    for x in 0..vp.width {
        let point = Point::new(i32::try_from(x).unwrap(), y);
        assert_eq!(manager_tool_at(&browser, &theme, vp, point, &[]), None);
    }

    // A click below the toolbar strip is never a write tool either.
    assert_eq!(
        manager_tool_at(
            &browser,
            &theme,
            vp,
            Point::new(4, i32::try_from(toolbar_height(&theme)).unwrap()),
            MANAGER_TOOLS,
        ),
        None
    );
}

#[test]
fn suggest_new_dir_name_disambiguates_against_the_listing() {
    use crate::mkdir::{suggest_new_dir_name, NEW_FOLDER_BASE};

    // An empty (or unrelated) listing gets the plain base name.
    assert_eq!(suggest_new_dir_name(&[]), NEW_FOLDER_BASE);
    assert_eq!(
        suggest_new_dir_name(&[Entry::directory("Documents"), Entry::file("notes.txt")]),
        NEW_FOLDER_BASE
    );

    // The base taken pushes to the first free numeric suffix, and further
    // clashes advance it, so the placeholder never collides with a sibling
    // (which the create would refuse).
    assert_eq!(
        suggest_new_dir_name(&[Entry::directory(NEW_FOLDER_BASE)]),
        "New Folder 2"
    );
    assert_eq!(
        suggest_new_dir_name(&[
            Entry::directory(NEW_FOLDER_BASE),
            Entry::directory("New Folder 2"),
            Entry::directory("New Folder 3"),
        ]),
        "New Folder 4"
    );

    // A gap is filled by the smallest free suffix, not the next after the max.
    assert_eq!(
        suggest_new_dir_name(&[
            Entry::directory(NEW_FOLDER_BASE),
            Entry::directory("New Folder 3"),
        ]),
        "New Folder 2"
    );
}

// --- FM8b: the drawn Properties overlay + selected_target_path ------------

use crate::properties::Properties;
use crate::render::{draw_properties, properties_panel_rect, properties_rows, PROPERTY_ROW_COUNT};
use tairix_abi::fs::{FileId, FileStat};
use tairix_abi::NodeTimes;

/// A `FileStat` fixture with the given kind and mode; a 1.5 KiB file taking
/// 4 KiB on disk, owned by uid 1000 / gid 100, with a kept access time left at
/// the epoch (so the blank-stamp path is exercised).
fn props_stat(kind: FileKind, mode: u32) -> FileStat {
    FileStat {
        kind,
        size: 1536,
        allocated: 4096,
        mode,
        uid: 1000,
        gid: 100,
        id: FileId::NONE,
        times: NodeTimes {
            created: Time64::from_secs(1_609_459_200),
            modified: Time64::from_secs(1_609_459_200 + 3661),
            accessed: Time64::UNIX_EPOCH,
            changed: Time64::from_secs(1_700_000_000),
        },
    }
}

#[test]
fn properties_rows_lists_every_field_in_order_from_the_model() {
    let props = Properties::from_stat(
        "notes.txt",
        crate::entry::EntryKind::File,
        &props_stat(FileKind::Regular, 0o644),
    );
    let rows = properties_rows(&props);
    // Exactly the sized-for field count, and in the documented order.
    assert_eq!(rows.len(), PROPERTY_ROW_COUNT);
    let labels: Vec<&str> = rows.iter().map(|(label, _)| *label).collect();
    assert_eq!(
        labels,
        [
            "Kind",
            "Size",
            "Permissions",
            "Owner",
            "Created",
            "Modified",
            "Accessed",
            "Changed",
        ]
    );
    let value = |name: &str| -> String {
        rows.iter()
            .find(|(label, _)| *label == name)
            .map(|(_, v)| v.clone())
            .expect("field present")
    };
    assert_eq!(value("Kind"), "File");
    assert_eq!(value("Size"), "1.5 KiB (4.0 KiB on disk)");
    assert_eq!(value("Permissions"), "-rw-r--r-- (0644)");
    assert_eq!(value("Owner"), "uid 1000 / gid 100");
    assert_eq!(value("Modified"), "2021-01-01 01:01:01");
    // A stamp the backing does not keep renders blank, never a fabricated
    // wall time.
    assert_eq!(value("Accessed"), "");
}

#[test]
fn properties_rows_reads_a_bundle_as_an_application_yet_a_directory_mode() {
    // A `<Name>.app` bundle is labelled "Application" but is a directory on
    // disk, so the permission string still leads with `d`.
    let props = Properties::from_stat(
        "Editor.app",
        crate::entry::EntryKind::Bundle,
        &props_stat(FileKind::Directory, 0o755),
    );
    let rows = properties_rows(&props);
    let kind = rows.iter().find(|(l, _)| *l == "Kind").unwrap().1.clone();
    let perms = rows
        .iter()
        .find(|(l, _)| *l == "Permissions")
        .unwrap()
        .1
        .clone();
    assert_eq!(kind, "Application");
    assert_eq!(perms, "drwxr-xr-x (0755)");
}

#[test]
fn properties_panel_rect_is_centered_and_clamped_within_the_viewport() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();

    // A generous window: the panel fits and is centered within it.
    let vp = Rect::new(0, 0, 480, 320);
    let rect = properties_panel_rect(vp, font, &theme);
    assert!(rect.width > 0 && rect.height > 0);
    assert!(rect.origin.x >= 0 && rect.origin.y >= 0);
    assert!(rect.origin.x + i32::try_from(rect.width).unwrap() <= i32::try_from(vp.width).unwrap());
    assert!(
        rect.origin.y + i32::try_from(rect.height).unwrap() <= i32::try_from(vp.height).unwrap()
    );
    // Centered: equal margins on each axis (within one pixel of integer split).
    let margin_x = rect.origin.x;
    let right_margin =
        i32::try_from(vp.width).unwrap() - (rect.origin.x + i32::try_from(rect.width).unwrap());
    assert!((margin_x - right_margin).abs() <= 1);

    // A window smaller than the panel would like still yields a drawable rect
    // clamped to the window, never a zero or over-size rectangle (no panic).
    let tiny = Rect::new(0, 0, 20, 16);
    let small = properties_panel_rect(tiny, font, &theme);
    assert!(small.width >= 1 && small.width <= tiny.width);
    assert!(small.height >= 1 && small.height <= tiny.height);
}

#[test]
fn draw_properties_paints_into_the_surface_without_panicking() {
    use tairix_raster::Surface;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);
    let props = Properties::from_stat(
        "Documents",
        crate::entry::EntryKind::Directory,
        &props_stat(FileKind::Directory, 0o755),
    );
    let mut surface = Surface::new(vp.width, vp.height).expect("surface");
    // A blank base to compare against: after drawing the overlay the surface
    // is no longer uniform, proving the panel actually painted.
    let before = surface.pixels().to_vec();
    draw_properties(&mut surface, &props, &theme, font, vp);
    assert_ne!(surface.pixels().to_vec(), before);

    // A degenerate viewport draws nothing and does not panic.
    let mut tiny = Surface::new(2, 2).expect("tiny surface");
    draw_properties(&mut tiny, &props, &theme, font, Rect::new(0, 0, 2, 2));
}

#[test]
fn selected_target_path_spells_the_selected_node_and_is_none_when_empty() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Default sort: the four view-binding directories in name order.
    browser
        .select(
            browser
                .entries()
                .iter()
                .position(|e| e.name() == "System")
                .expect("System listed"),
        )
        .expect("select System");
    assert_eq!(
        browser.selected_target_path(),
        Some(Ok("/System".to_string()))
    );

    // Nested: the path reflects the current directory, not just the leaf.
    let system = browser
        .entries()
        .iter()
        .position(|e| e.name() == "System")
        .expect("System listed");
    browser.open_index(system).expect("descend into System");
    let first = browser.selected_name().expect("a selection").to_string();
    assert_eq!(
        browser.selected_target_path(),
        Some(Ok(alloc::format!("/System/{first}")))
    );

    // The empty /System/Fonts has no selection, hence no target path.
    let mut b2 = Browser::open_root(MockFs::fixture()).expect("root");
    b2.open_index(
        b2.entries()
            .iter()
            .position(|e| e.name() == "System")
            .unwrap(),
    )
    .expect("enter System");
    b2.open_index(
        b2.entries()
            .iter()
            .position(|e| e.name() == "Fonts")
            .unwrap(),
    )
    .expect("enter Fonts");
    assert_eq!(b2.selected_target_path(), None);
}
