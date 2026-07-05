//! Headless unit tests for the filesystem browser.
//!
//! Every test drives the [`Browser`] against an in-memory [`MockFs`] tree, so
//! the navigation and rendering logic is exercised without a kernel or a real
//! VFS.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_geometry::Rect;
use rustos_raster::Color;
use rustos_theme::Theme;

use crate::browser::Browser;
use crate::entry::Entry;
use crate::error::BrowseError;
use crate::source::DirectorySource;

/// The absolute-path key the mock indexes a directory by, mirroring
/// [`Browser::path`] so tests and the model agree on the path string.
fn key(components: &[String]) -> String {
    if components.is_empty() {
        return "/".to_string();
    }
    let mut path = String::new();
    for component in components {
        path.push('/');
        path.push_str(component);
    }
    path
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
    assert_eq!(names(&browser), ["System", "Users", "Apps", "Storage"]);
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
    browser.open_index(0).expect("enter System");
    assert!(!browser.is_root());
    assert_eq!(browser.path(), "/System");
    assert_eq!(names(&browser), ["Fonts", "Security", "Kernel"]);

    assert_eq!(browser.go_up(), Ok(true));
    assert_eq!(browser.path(), "/");
    assert_eq!(names(&browser), ["System", "Users", "Apps", "Storage"]);
}

#[test]
fn go_up_at_the_root_is_a_no_op() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    assert_eq!(browser.go_up(), Ok(false));
    assert!(browser.is_root());
    assert_eq!(names(&browser), ["System", "Users", "Apps", "Storage"]);
}

#[test]
fn opening_a_regular_file_is_rejected_and_changes_nothing() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(0).expect("enter System");
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
    browser.open_index(0).expect("enter System");
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
    browser.open_index(0).expect("enter System");
    browser.open_index(0).expect("enter Fonts");
    assert_eq!(browser.path(), "/System/Fonts");
    assert!(browser.entries().is_empty());
    assert_eq!(browser.selected_index(), None);
    assert_eq!(browser.selected_entry(), None);
}

#[test]
fn open_selected_descends_into_the_selected_directory() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select(1).expect("select Users");
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
    let surface = crate::render(&browser, &theme, Rect::new(0, 0, 200, 120)).expect("surface");
    assert_eq!(surface.width(), 200);
    assert_eq!(surface.height(), 120);
}

#[test]
fn render_highlights_the_selected_entry_with_the_accent() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.select(1).expect("select Users");
    let theme = Theme::dark();
    let surface = crate::render(&browser, &theme, Rect::new(0, 0, 200, 120)).expect("surface");

    let accent = Color::from(theme.palette().accent).premultiply();
    let raised = Color::from(theme.palette().surface_raised).premultiply();
    // The path bar (top-left) carries the raised role.
    assert_eq!(surface.get(0, 0), Some(raised));
    // Row 0 of the list (the path bar is row 0) is "System"; row 1 is the
    // selected "Users", filled with the accent. Each row is the shared
    // font's glyph height plus the renderer's padding, so the selected fill
    // starts two rows down.
    let row_height = rustos_font::BitmapFont::inconsolata().glyph_height() + 4;
    let selected_y = row_height * 2 + 1;
    assert_eq!(surface.get(199, selected_y), Some(accent));
}

#[test]
fn render_into_a_tiny_viewport_does_not_panic() {
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let theme = Theme::dark();
    // Too short for even the path bar: paints what it can and returns a
    // surface rather than panicking.
    let surface = crate::render(&browser, &theme, Rect::new(0, 0, 4, 3)).expect("surface");
    assert_eq!(surface.width(), 4);
    assert_eq!(surface.height(), 3);
}
