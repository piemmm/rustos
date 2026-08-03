//! Headless unit tests for the filesystem browser.
//!
//! Every test drives the [`Browser`] against an in-memory [`MockFs`] tree, so
//! the navigation and rendering logic is exercised without a kernel or a real
//! VFS.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::Errno;
use tairix_geometry::Rect;
use tairix_icon::{IconArtwork, IconKind, NoArtwork};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use crate::browser::Browser;
use crate::clipboard::{plan_paste, Clipboard, ClipboardOp, PasteError};
use crate::delete::{DeleteAction, DeleteError, DeletePlan, DeleteWalk, MAX_DELETE_DEPTH};
use crate::entry::Entry;
use crate::error::BrowseError;
use crate::execute::{
    paste_strategy, CopyAction, CopyCursor, CopyError, CopyWalk, CopyWalkError, PasteStrategy,
    VolumeId, COPY_CHUNK_LEN, MAX_COPY_DEPTH,
};
use crate::media::MediaType;
use crate::places::{PlaceKind, Places, Volume};
use crate::select::Selection;
use crate::source::DirectorySource;

/// The absolute-path key the mock indexes a directory by — the one shared
/// spelling, so tests, the model, and the VFS engine agree on the path
/// string.
fn key(components: &[String]) -> String {
    crate::vfs::spell_absolute_path(components)
}

// --- The places / devices rail -----------------------------------------

/// A home directory's path components, as the app reads them from the user's
/// own identity.
fn home() -> Vec<String> {
    vec!["Users".to_string(), "ann".to_string()]
}

/// One offered volume.
fn volume(label: &str, target: &str, medium: Option<BlkDeviceClass>) -> Volume {
    Volume {
        label: label.to_string(),
        target: target.to_string(),
        medium,
    }
}

/// Every row's label, in rail order.
fn place_labels(places: &Places) -> Vec<String> {
    places
        .rows()
        .iter()
        .map(|row| row.label().to_string())
        .collect()
}

#[test]
fn the_rail_lists_the_users_places_then_the_volumes_in_one_order() {
    let places = Places::new(
        &home(),
        &[
            volume(
                "Scratch",
                "/Storage/Scratch",
                Some(BlkDeviceClass::SolidState),
            ),
            volume(
                "Backup",
                "/Storage/Backup",
                Some(BlkDeviceClass::Rotational),
            ),
        ],
    );
    // The user's own places first, in their fixed order, then the volumes
    // sorted by label whatever order the mount table paged them out in.
    assert_eq!(
        place_labels(&places),
        [
            "Home",
            "Desktop",
            "Documents",
            "Apps",
            "System",
            "Backup",
            "Scratch"
        ]
    );
    assert_eq!(places.volume_start(), Some(5));
    assert_eq!(places.rows()[0].kind(), PlaceKind::Home);
    assert_eq!(places.rows()[1].kind(), PlaceKind::UserFolder);
    assert_eq!(places.rows()[3].kind(), PlaceKind::SystemRoot);
    assert_eq!(places.rows()[5].kind(), PlaceKind::Volume);
    // The fixed places navigate where their names say, whether or not those
    // directories exist — the model performs no I/O and never checks.
    assert_eq!(places.rows()[0].components(), home().as_slice());
    assert_eq!(
        places.rows()[2].components(),
        ["Users", "ann", "Documents"].map(String::from)
    );
    assert_eq!(places.rows()[3].components(), ["Apps"].map(String::from));
    // With no volumes there is nothing to separate.
    assert_eq!(Places::new(&home(), &[]).volume_start(), None);
    // Without a home there is nothing for the three home rows to hang off, so
    // only the machine-wide roots remain — never a row navigating nowhere.
    assert_eq!(place_labels(&Places::new(&[], &[])), ["Apps", "System"]);
}

#[test]
fn every_storage_medium_draws_its_own_drive_icon() {
    let places = Places::new(
        &[],
        &[
            volume("Disk", "/Storage/Disk", Some(BlkDeviceClass::Rotational)),
            volume("Fast", "/Storage/Fast", Some(BlkDeviceClass::SolidState)),
            volume("Stick", "/Storage/Stick", Some(BlkDeviceClass::Removable)),
            volume("Guest", "/Storage/Guest", Some(BlkDeviceClass::Virtual)),
            volume("Plain", "/Storage/Plain", None),
        ],
    );
    let icons: Vec<(String, IconKind)> = places
        .rows()
        .iter()
        .filter(|row| row.kind() == PlaceKind::Volume)
        .map(|row| (row.label().to_string(), row.icon()))
        .collect();
    // Sorted by label: Disk, Fast, Guest, Plain, Stick. A paravirtual device
    // and an unreported medium both draw the generic drive — never a guess at
    // hardware that was not reported.
    assert_eq!(
        icons,
        [
            ("Disk".to_string(), IconKind::DiskHard),
            ("Fast".to_string(), IconKind::DiskSolidState),
            ("Guest".to_string(), IconKind::Disk),
            ("Plain".to_string(), IconKind::Disk),
            ("Stick".to_string(), IconKind::DiskUsb),
        ]
    );
}

#[test]
fn a_malformed_or_duplicate_volume_is_dropped_never_guessed_at() {
    let over_long = "v".repeat(crate::MAX_PLACE_LABEL + 1);
    let places = Places::new(
        &home(),
        &[
            volume("Good", "/Storage/Good", None),
            // No label to show.
            volume("", "/Storage/Nameless", None),
            // A label longer than a row will ever accept.
            volume(&over_long, "/Storage/Long", None),
            // A label carrying a control character.
            volume("Ba\nd", "/Storage/Control", None),
            // A target that is not an absolute path.
            volume("Relative", "Storage/Relative", None),
            // A second row for a target an accepted row already covers.
            volume("Twin", "/Storage/Good", None),
            // A volume landing on a fixed place's own target.
            volume("Shadow", "/Apps", None),
        ],
    );
    assert_eq!(
        place_labels(&places),
        ["Home", "Desktop", "Documents", "Apps", "System", "Good"]
    );
    // The duplicate never displaced the fixed row it collided with.
    assert_eq!(places.rows()[3].kind(), PlaceKind::SystemRoot);
}

#[test]
fn the_rail_hit_test_inverts_the_layout_exactly_at_the_row_boundaries() {
    let theme = Theme::dark();
    let font = tairix_font::BitmapFont::inconsolata();
    let window = Rect::new(0, 0, 400, 400);
    let places = Places::new(&home(), &[volume("Backup", "/Storage/Backup", None)]);
    let view = crate::render::sidebar_view(window, &theme, font, Some(&places)).expect("rail");

    for index in 0..places.len() {
        let rect = view.row_rect(index).expect("drawn row");
        let top = u32::try_from(rect.origin.y).expect("row top");
        // Both edges of the row resolve to it, and the pixel above its top
        // belongs to whatever is above — never to this row.
        assert_eq!(view.index_at(0, top), Some(index));
        assert_eq!(
            view.index_at(rect.width - 1, top + rect.height - 1),
            Some(index)
        );
        assert_ne!(view.index_at(0, top.wrapping_sub(1)), Some(index));
    }
    // The separation between the user's places and the volumes is not a row.
    let band = view.separator_rect().expect("separator");
    let band_y = u32::try_from(band.origin.y).expect("band top");
    assert_eq!(view.index_at(0, band_y), None);
    // Nothing outside the rail resolves: past its right edge, or below the
    // last row.
    assert_eq!(view.index_at(view.width(), 0), None);
    assert_eq!(view.index_at(0, window.height), None);
    let last = view.row_rect(places.len() - 1).expect("last row");
    let below = u32::try_from(last.origin.y).expect("last top") + last.height;
    assert_eq!(view.index_at(0, below), None);
    // A window with no room for even one row resolves nothing at all.
    let squat = Rect::new(0, 0, 400, 1);
    let tiny = crate::render::sidebar_view(squat, &theme, font, Some(&places)).expect("rail");
    assert_eq!(tiny.index_at(0, 0), None);
}

#[test]
fn the_content_area_is_inset_by_the_rail_and_untouched_without_one() {
    let theme = Theme::dark();
    let font = tairix_font::BitmapFont::inconsolata();
    let window = Rect::new(0, 0, 400, 300);
    let places = Places::new(&home(), &[]);
    let view = crate::render::sidebar_view(window, &theme, font, Some(&places)).expect("rail");
    let rail = view.width();
    assert!(rail > 0);

    let inset = crate::render::content_area(window, &theme, font, Some(&places));
    assert_eq!(inset.origin.x, i32::try_from(rail).expect("rail width"));
    assert_eq!(inset.width, window.width - rail);
    assert_eq!(inset.origin.y, window.origin.y);
    assert_eq!(inset.height, window.height);
    // Everything the view lays out follows the inset, so the scrollbar sits
    // against the window's right edge rather than the rail's.
    let bar = crate::render::scrollbar_bounds(&theme, font, inset).expect("scrollbar");
    assert!(bar.origin.x > inset.origin.x);
    assert!(u32::try_from(bar.origin.x).expect("bar x") + bar.width <= window.width);

    // With no rail the area is the window, byte for byte, so a view without a
    // sidebar is laid out exactly as it was before there was one.
    assert_eq!(
        crate::render::content_area(window, &theme, font, None),
        window
    );
    // An empty rail is no rail at all.
    let empty = Places::default();
    assert!(crate::render::sidebar_view(window, &theme, font, Some(&empty)).is_none());
    assert_eq!(
        crate::render::content_area(window, &theme, font, Some(&empty)),
        window
    );
}

#[test]
fn the_rail_selects_the_row_matching_the_browsers_location() {
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let places = Places::new(&home(), &[]);
    // At the root, no place matches, so nothing is selected.
    assert_eq!(places.index_of(browser.components()), None);
    // Standing on a place selects exactly it.
    assert_eq!(places.index_of(&["Apps".to_string()]), Some(3));
    // A directory *inside* a place is not that place: an exact match only,
    // never a claim that the user is somewhere they are not.
    assert_eq!(
        places.index_of(&["Apps".to_string(), "Notes.app".to_string()]),
        None
    );

    // The selection reaches the drawn rail: the frame differs once the
    // browser stands on a place.
    let theme = Theme::dark();
    let font = tairix_font::BitmapFont::inconsolata();
    let viewport = Rect::new(0, 0, 400, 300);
    let chrome = crate::ManagerChrome {
        tools: &[],
        tool_model: crate::ManagerToolModel::none(),
        sidebar: Some(&places),
    };
    let unselected =
        crate::render(&browser, &theme, font, viewport, &chrome, &mut NoArtwork).expect("surface");
    let mut at_place = Browser::open_root(MockFs::fixture()).expect("root");
    at_place
        .navigate_to(vec!["System".to_string()])
        .expect("navigate to System");
    let selected =
        crate::render(&at_place, &theme, font, viewport, &chrome, &mut NoArtwork).expect("surface");
    let rail = crate::render::sidebar_view(viewport, &theme, font, Some(&places))
        .expect("rail")
        .row_rect(4)
        .expect("the System row");
    let row_y = usize::try_from(rail.origin.y).expect("row top");
    let width = usize::try_from(viewport.width).expect("width");
    let at = row_y * width;
    assert_ne!(
        unselected.pixels()[at..at + usize::try_from(rail.width).expect("row width")],
        selected.pixels()[at..at + usize::try_from(rail.width).expect("row width")]
    );
}

#[test]
fn a_rail_row_carries_every_state_the_control_offers() {
    let mut places = Places::new(&home(), &[volume("Backup", "/Storage/Backup", None)]);
    // Focus, cursor, and hover are the rail's own; each is reachable and each
    // reports whether it actually moved so a caller repaints only when needed.
    assert!(!places.is_focused());
    places.set_focused(true);
    assert!(places.is_focused());
    assert_eq!(places.cursor(), 0);
    assert!(places.move_cursor(1));
    assert_eq!(places.cursor(), 1);
    assert!(places.move_cursor(-5));
    assert_eq!(places.cursor(), 0);
    // Clamped at both ends: a held arrow never wraps round the rail.
    assert!(!places.move_cursor(-1));
    assert!(places.move_cursor(1_000));
    assert_eq!(places.cursor(), places.len() - 1);
    assert!(!places.move_cursor(1));
    // An index the rail does not have is ignored rather than stored.
    places.set_cursor(places.len());
    assert_eq!(places.cursor(), places.len() - 1);

    assert_eq!(places.hovered(), None);
    assert!(places.set_hovered(Some(2)));
    assert_eq!(places.hovered(), Some(2));
    assert!(!places.set_hovered(Some(2)));
    // A row the rail does not have clears the highlight rather than storing
    // an index nothing will ever draw.
    assert!(places.set_hovered(Some(places.len())));
    assert_eq!(places.hovered(), None);

    // Availability is only ever *taken away*, and only for a row that exists.
    assert!(places.rows().iter().all(crate::Place::is_available));
    places.set_unavailable(1);
    assert!(!places.rows()[1].is_available());
    places.set_unavailable(places.len());
    assert_eq!(
        places.rows().iter().filter(|r| !r.is_available()).count(),
        1
    );
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
fn open_at_starts_at_the_named_directory_with_working_climb() {
    // Opening at `/System` starts *there* — its listing, its breadcrumb — and
    // climbing returns toward the root exactly as a descent would have.
    let start = crate::vfs::components_from_absolute_path("/System").expect("valid path");
    let mut browser = Browser::open_at(MockFs::fixture(), start).expect("System lists");
    assert!(!browser.is_root());
    assert_eq!(browser.path(), "/System");
    assert_eq!(names(&browser), ["Fonts", "Security", "Kernel"]);
    assert_eq!(browser.selected_index(), Some(0));
    // A fresh open has no back history, so the first climb goes to the parent
    // rather than a remembered directory.
    assert_eq!(browser.go_up(), Ok(true));
    assert_eq!(browser.path(), "/");
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn open_at_empty_components_is_exactly_open_root() {
    // `open_root` is defined as `open_at(source, [])`; prove the empty path
    // opens the root so the trusted picker's home-or-root fallback is honest.
    let browser = Browser::open_at(MockFs::fixture(), Vec::new()).expect("root lists");
    assert!(browser.is_root());
    assert_eq!(browser.path(), "/");
    assert_eq!(names(&browser), ["Apps", "Storage", "System", "Users"]);
}

#[test]
fn open_at_fails_closed_when_the_directory_is_unreadable() {
    // A start directory that cannot be listed refuses the open (an `Err`), so
    // the caller can fall back to a directory it can name rather than opening
    // an empty or guessed view.
    let start = crate::vfs::components_from_absolute_path("/System/Security").expect("valid path");
    let result = Browser::open_at(MockFs::fixture(), start);
    assert_eq!(
        result.err(),
        Some(BrowseError::Source(Errno::PermissionDenied))
    );
}

#[test]
fn components_from_absolute_path_parses_and_collapses_slashes() {
    use crate::vfs::components_from_absolute_path as parse;
    // The bare root and the empty string carry no components.
    assert_eq!(parse("/"), Ok(Vec::new()));
    assert_eq!(parse(""), Ok(Vec::new()));
    // Leading, trailing, and repeated separators collapse to the same list.
    let want = vec!["Users".to_string(), "root".to_string()];
    assert_eq!(parse("/Users/root"), Ok(want.clone()));
    assert_eq!(parse("/Users/root/"), Ok(want.clone()));
    assert_eq!(parse("//Users//root//"), Ok(want));
    // The parse is the exact inverse of the shared spelling.
    let round = parse("/Users/root").expect("parses");
    assert_eq!(crate::vfs::spell_absolute_path(&round), "/Users/root");
}

#[test]
fn components_from_absolute_path_rejects_a_malformed_segment() {
    use crate::vfs::components_from_absolute_path as parse;
    // `.`/`..` and a resource-reference `:` are not real leaf names, so the
    // whole path is refused rather than silently reinterpreted.
    assert_eq!(parse("/Users/.."), Err(Errno::OutOfRange));
    assert_eq!(parse("/Users/."), Err(Errno::OutOfRange));
    assert_eq!(parse("/disk:backup"), Err(Errno::OutOfRange));
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
        &crate::ManagerChrome::none(),
        &mut NoArtwork,
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
        &crate::ManagerChrome::none(),
        &mut NoArtwork,
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
        &crate::ManagerChrome::none(),
        &mut NoArtwork,
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
    let deep: Vec<String> = core::iter::repeat_n(component, FS_PATH_MAX / 250 + 1).collect();
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
fn scrollbar_bounds_matches_the_reserved_gutter() {
    use crate::render::scrollbar_bounds;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 200, 200);
    let header = crate::render::chrome_height(font, &theme);
    let bounds = scrollbar_bounds(&theme, font, vp).expect("a gutter exists");
    // The bar sits in the reserved right-edge gutter (its right edge is the
    // window's right edge), below the chrome header, and is a real strip wide.
    assert_eq!(bounds.right(), i32::try_from(vp.width).unwrap());
    assert_eq!(bounds.top(), i32::try_from(header).unwrap());
    assert!(bounds.width > 0);
    assert!(bounds.left() > 0 && bounds.left() < i32::try_from(vp.width).unwrap());
    // A window too short for any item area has no gutter.
    assert!(scrollbar_bounds(&theme, font, Rect::new(0, 0, 200, header)).is_none());
}

#[test]
fn scrollbar_click_on_the_increment_button_scrolls_down() {
    use crate::render::{row_height, scroll_pointer, scrollbar_bounds};
    use tairix_geometry::Point;
    use tairix_input::{InputEvent, PointerButton};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(40);
    let row = row_height(font);
    let vp = Rect::new(
        0,
        0,
        200,
        crate::render::chrome_height(font, &theme) + row * 6,
    );
    let bounds = scrollbar_bounds(&theme, font, vp).expect("a gutter exists");
    let cx = bounds.left() + i32::try_from(bounds.width).unwrap() / 2;

    // A press on the increment (down) button at the bottom of the bar steps
    // the offset one line — the arrow button now scrolls the listing.
    let down = Point::new(cx, bounds.bottom() - 1);
    let press = InputEvent::PointerPressed {
        button: PointerButton::Primary,
    };
    assert_eq!(
        scroll_pointer(&mut browser, font, &theme, vp, down, &press),
        Some(true)
    );
    assert_eq!(browser.scroll_offset(), 1);

    // A press away from the gutter is not the scrollbar's: it falls through to
    // the content (the helper reports it did not consume it).
    let off = Point::new(10, bounds.top() + 4);
    assert_eq!(
        scroll_pointer(&mut browser, font, &theme, vp, off, &press),
        None
    );
}

#[test]
fn scrollbar_thumb_drag_scrolls_and_release_ends_the_capture() {
    use crate::render::{row_height, scroll_model, scroll_pointer, scrollbar_bounds};
    use tairix_controls::{ScrollBar, ScrollOrientation, ScrollPart};
    use tairix_geometry::{Point, Scale};
    use tairix_input::{InputEvent, PointerButton};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(60);
    let row = row_height(font);
    let vp = Rect::new(
        0,
        0,
        200,
        crate::render::chrome_height(font, &theme) + row * 6,
    );
    let bounds = scrollbar_bounds(&theme, font, vp).expect("a gutter exists");
    let cx = bounds.left() + i32::try_from(bounds.width).unwrap() / 2;

    // Find a point on the thumb using the same layout the router uses.
    let probe = ScrollBar::new(
        ScrollOrientation::Vertical,
        scroll_model(&browser, font, &theme, vp),
    );
    let thumb_y = (bounds.top()..bounds.bottom())
        .find(|&y| {
            probe.part_at(bounds, Point::new(cx, y), Scale::ONE, &theme) == ScrollPart::Thumb
        })
        .expect("the bar has a draggable thumb");

    // Press the thumb: the drag is captured but nothing has moved yet.
    let press = InputEvent::PointerPressed {
        button: PointerButton::Primary,
    };
    assert_eq!(
        scroll_pointer(
            &mut browser,
            font,
            &theme,
            vp,
            Point::new(cx, thumb_y),
            &press
        ),
        Some(true)
    );
    assert_eq!(browser.scroll_offset(), 0);

    // Dragging the thumb toward the bottom scrolls the listing down.
    let to = Point::new(cx, bounds.bottom() - 2);
    let moved = InputEvent::PointerMoved { to };
    assert_eq!(
        scroll_pointer(&mut browser, font, &theme, vp, to, &moved),
        Some(true)
    );
    assert!(browser.scroll_offset() > 0);

    // Releasing ends the capture; a later move off the bar is no longer the
    // scrollbar's (it reports it consumed nothing).
    let release = InputEvent::PointerReleased {
        button: PointerButton::Primary,
    };
    assert_eq!(
        scroll_pointer(&mut browser, font, &theme, vp, to, &release),
        Some(true)
    );
    let off = InputEvent::PointerMoved {
        to: Point::new(10, bounds.top() + 3),
    };
    assert_eq!(
        scroll_pointer(
            &mut browser,
            font,
            &theme,
            vp,
            Point::new(10, bounds.top() + 3),
            &off
        ),
        None
    );
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
    let surface = crate::render(
        &browser,
        &theme,
        font,
        vp,
        &crate::ManagerChrome::none(),
        &mut NoArtwork,
    )
    .expect("grid surface");
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

/// An artwork lookup that answers every request with one solid-colour square
/// and records what it was asked for, so a render can be proven to have gone
/// through the seam rather than straight to the built-in glyph.
struct RecordingArtwork {
    art: Surface,
    asked: Vec<(IconKind, u32)>,
}

impl RecordingArtwork {
    fn new(side: u32, color: Color) -> Self {
        let mut art = Surface::new(side, side).expect("artwork surface");
        art.fill(color);
        Self {
            art,
            asked: Vec::new(),
        }
    }
}

impl IconArtwork for RecordingArtwork {
    fn artwork(&mut self, kind: IconKind, side: u32) -> Option<&Surface> {
        self.asked.push((kind, side));
        Some(&self.art)
    }
}

/// Whether `surface` shows `color` anywhere.
fn shows(surface: &Surface, color: Color) -> bool {
    let wanted = color.premultiply();
    (0..surface.height()).any(|y| (0..surface.width()).any(|x| surface.get(x, y) == Some(wanted)))
}

#[test]
fn the_grid_resolves_each_tile_through_the_artwork_lookup_and_draws_what_it_returns() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let mut browser = many_files(20);
    browser.set_view_mode(ViewMode::Grid);
    let vp = Rect::new(0, 0, 400, 400);

    // The fixture's names carry no extension, so every tile classifies as the
    // generic content type and asks for that kind's artwork at the card's own
    // icon slot.
    let art_colour = Color::rgb(255, 0, 255);
    let mut artwork = RecordingArtwork::new(24, art_colour);
    let drawn = crate::render(
        &browser,
        &theme,
        font,
        vp,
        &crate::ManagerChrome::none(),
        &mut artwork,
    )
    .expect("grid surface");
    assert!(!artwork.asked.is_empty(), "the grid consults the lookup");
    assert!(artwork
        .asked
        .iter()
        .all(|(kind, side)| *kind == IconKind::File && *side > 0));
    assert!(
        shows(&drawn, art_colour),
        "the supplied artwork reaches the tile"
    );

    // Without a lookup the same grid draws the built-in glyph instead, so the
    // colour above can only have come through the seam.
    let plain = crate::render(
        &browser,
        &theme,
        font,
        vp,
        &crate::ManagerChrome::none(),
        &mut NoArtwork,
    )
    .expect("grid surface");
    assert!(!shows(&plain, art_colour));
}

#[test]
fn the_list_view_is_text_only_and_never_consults_the_artwork_lookup() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let browser = many_files(20);
    let mut artwork = RecordingArtwork::new(24, Color::rgb(255, 0, 255));
    crate::render(
        &browser,
        &theme,
        font,
        Rect::new(0, 0, 400, 400),
        &crate::ManagerChrome::none(),
        &mut artwork,
    )
    .expect("list surface");
    assert!(artwork.asked.is_empty());
}

// --- The grid over the real shipped-artwork cache ------------------------
//
// The file manager draws its grid through the shared reclaim-governed
// `ArtworkCache` bound to a read seam and a *sandboxed* rasterise seam. These
// tests drive that exact composition with fakes for the two seams — no live
// sandbox — so the safety properties are host-proven: the fallback chain is
// total, a reply that cannot be believed is refused, and the cache decodes
// each `(asset, side)` once no matter how many tiles want it.

use alloc::boxed::Box;

use tairix_icon::{
    artwork_cache, icon_artwork_path, ArtworkCache, ArtworkRasteriser, ArtworkReader,
    IconArtworkSource, MAX_ARTWORK_BYTES,
};
use tairix_log::DiscardSink;
use tairix_reclaim::pressure::{PressureBand, ReportedPressure};

/// A reader over an in-memory asset table that records every path it was
/// asked for, so a test can prove which kinds were resolved and that a second
/// tile of the same kind was served from the cache rather than read again.
struct CountingReader {
    assets: BTreeMap<String, Vec<u8>>,
    read: Vec<String>,
}

impl CountingReader {
    fn new() -> Self {
        Self {
            assets: BTreeMap::new(),
            read: Vec::new(),
        }
    }

    /// Ship `bytes` as the raster master for `kind`, at the one shared asset
    /// path the desktop resolves that kind to.
    fn shipping(mut self, kind: IconKind, bytes: Vec<u8>) -> Self {
        self.assets.insert(icon_artwork_path(kind), bytes);
        self
    }
}

impl ArtworkReader for CountingReader {
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        self.read.push(path.to_string());
        self.assets.get(path).cloned()
    }
}

/// A rasteriser that answers with one solid opaque colour at the requested
/// side and counts every decode, standing in for the sandboxed worker.
struct CountingRasteriser {
    color: Color,
    decodes: usize,
}

impl CountingRasteriser {
    fn new(color: Color) -> Self {
        Self { color, decodes: 0 }
    }
}

impl ArtworkRasteriser for CountingRasteriser {
    fn rasterise(&mut self, side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        self.decodes += 1;
        let pixel = [self.color.r, self.color.g, self.color.b, 0xff];
        Some(
            pixel
                .iter()
                .copied()
                .cycle()
                .take((side as usize) * (side as usize) * 4)
                .collect(),
        )
    }
}

/// A rasteriser whose reply is the wrong length, modelling a worker that
/// lies about the geometry it produced.
struct ShortRasteriser;

impl ArtworkRasteriser for ShortRasteriser {
    fn rasterise(&mut self, _side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        Some(vec![0xff; 3])
    }
}

/// A rasteriser that must never run: the caller is required to refuse the
/// input before any decode happens.
struct PanicRasteriser;

impl ArtworkRasteriser for PanicRasteriser {
    fn rasterise(&mut self, _side: u32, _bytes: &[u8]) -> Option<Vec<u8>> {
        panic!("no byte of a missing or over-long asset may reach the decoder");
    }
}

/// The shared artwork cache wired as the file manager wires it, at a normal
/// pressure band so it retains what it decodes.
fn test_artwork_cache() -> ArtworkCache {
    let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
    gauge.report(PressureBand::Normal);
    let sink: &'static DiscardSink = Box::leak(Box::new(DiscardSink));
    artwork_cache("browse.test-artwork", 1, 1920 * 1080 * 4, gauge, sink)
}

/// Render `browser` into `vp` through the artwork lookup `artwork`.
fn grid_surface<S: DirectorySource>(
    browser: &Browser<S>,
    vp: Rect,
    artwork: &mut dyn IconArtwork,
) -> Surface {
    crate::render(
        browser,
        &Theme::dark(),
        tairix_font::BitmapFont::inconsolata(),
        vp,
        &crate::ManagerChrome::none(),
        artwork,
    )
    .expect("grid surface")
}

/// A grid of `n` extension-less files, every one of them the generic content
/// type, so all tiles resolve to the same icon kind.
fn generic_grid(n: usize) -> Browser<MockFs> {
    let mut browser = many_files(n);
    browser.set_view_mode(ViewMode::Grid);
    browser
}

/// A grid whose first `png` entries are PNG images and whose last `txt`
/// entries are plain text, so the two halves resolve to different icon kinds
/// and sort into that order.
fn two_kind_grid(png: usize, txt: usize) -> Browser<MockFs> {
    let mut entries = Vec::new();
    for i in 0..png {
        entries.push(Entry::file(format!("a{i:03}.png")));
    }
    for i in 0..txt {
        entries.push(Entry::file(format!("z{i:03}.txt")));
    }
    let mut dirs = BTreeMap::new();
    dirs.insert("/".to_string(), entries);
    let mut browser = Browser::open_root(MockFs {
        dirs,
        denied: BTreeSet::new(),
        deny_after_first: BTreeSet::new(),
        reads: BTreeMap::new(),
        root_after_refresh: None,
    })
    .expect("root");
    browser.set_view_mode(ViewMode::Grid);
    browser
}

#[test]
fn a_grid_tile_blits_shipped_artwork_and_falls_back_to_the_glyph_without_it() {
    let browser = generic_grid(6);
    let vp = Rect::new(0, 0, 400, 400);
    let art_colour = Color::rgb(0x11, 0x22, 0x33);
    // The all-glyph frame every fallback case below must reproduce exactly.
    let glyphs = grid_surface(&browser, vp, &mut NoArtwork);

    // The system ships artwork for the tile's kind: the decoded pixels reach
    // the tile.
    let mut cache = test_artwork_cache();
    let mut reader = CountingReader::new().shipping(IconKind::File, vec![0xab; 64]);
    let mut rasteriser = CountingRasteriser::new(art_colour);
    let drawn = grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut reader, &mut rasteriser),
    );
    assert_eq!(rasteriser.decodes, 1);
    assert!(shows(&drawn, art_colour), "the shipped artwork is blitted");
    assert_ne!(drawn.pixels(), glyphs.pixels());

    // No asset on disk: nothing is decoded and the tile is the built-in glyph
    // — never a blank tile.
    let mut cache = test_artwork_cache();
    let mut absent = CountingReader::new();
    let missing = grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut absent, &mut PanicRasteriser),
    );
    assert_eq!(missing.pixels(), glyphs.pixels());

    // An asset longer than the shared artwork ceiling is refused *before* the
    // decoder runs — `PanicRasteriser` would fire if a byte of it reached one
    // — and the tile is the glyph again.
    let mut cache = test_artwork_cache();
    let mut oversize =
        CountingReader::new().shipping(IconKind::File, vec![0u8; MAX_ARTWORK_BYTES + 1]);
    let refused = grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut oversize, &mut PanicRasteriser),
    );
    assert_eq!(refused.pixels(), glyphs.pixels());
}

#[test]
fn a_rasteriser_reply_of_the_wrong_length_is_refused_and_never_reaches_the_frame() {
    let browser = generic_grid(6);
    let vp = Rect::new(0, 0, 400, 400);
    let glyphs = grid_surface(&browser, vp, &mut NoArtwork);

    // The worker claims success but hands back three bytes where a full
    // square was promised: the reply is not believed, so the frame is the
    // glyph frame exactly — no partial blit, no torn tile.
    let mut cache = test_artwork_cache();
    let mut reader = CountingReader::new().shipping(IconKind::File, vec![0xab; 64]);
    let drawn = grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut reader, &mut ShortRasteriser),
    );
    assert_eq!(drawn.width(), vp.width);
    assert_eq!(drawn.height(), vp.height);
    assert_eq!(drawn.pixels(), glyphs.pixels());
}

#[test]
fn a_hundred_tiles_of_one_kind_are_read_and_decoded_exactly_once() {
    let browser = generic_grid(100);
    let vp = Rect::new(0, 0, 400, 400);
    let art_colour = Color::rgb(0x11, 0x22, 0x33);

    let mut cache = test_artwork_cache();
    let mut reader = CountingReader::new().shipping(IconKind::File, vec![0xab; 64]);
    let mut rasteriser = CountingRasteriser::new(art_colour);
    let drawn = grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut reader, &mut rasteriser),
    );
    assert!(shows(&drawn, art_colour));
    // Every tile shares one `(asset, side)` key, so a hundred-entry grid
    // costs one read and one decode, not a hundred of each.
    assert_eq!(reader.read.len(), 1);
    assert_eq!(rasteriser.decodes, 1);
}

#[test]
fn scrolling_to_new_entries_decodes_only_the_newly_visible_kinds() {
    let mut browser = two_kind_grid(40, 40);
    let vp = Rect::new(0, 0, 400, 400);
    let png = icon_artwork_path(IconKind::ImagePng);
    let text = icon_artwork_path(IconKind::Text);

    let mut cache = test_artwork_cache();
    let mut reader = CountingReader::new()
        .shipping(IconKind::ImagePng, vec![0xab; 64])
        .shipping(IconKind::Text, vec![0xcd; 64]);
    let mut rasteriser = CountingRasteriser::new(Color::rgb(0x11, 0x22, 0x33));

    // The first page is all images: the text kind is never touched, so a tile
    // scrolled out of view costs nothing.
    grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut reader, &mut rasteriser),
    );
    assert_eq!(reader.read, core::slice::from_ref(&png));
    assert_eq!(rasteriser.decodes, 1);

    // Scroll to the end (the layout clamps the request to the last page):
    // only the kind that just became visible is read and decoded, and the
    // image artwork already held is not resolved again.
    browser.set_scroll_offset(u64::from(u32::MAX));
    grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut reader, &mut rasteriser),
    );
    assert_eq!(reader.read, [png, text]);
    assert_eq!(rasteriser.decodes, 2);
}

#[test]
fn teardown_releases_the_retained_artwork() {
    let browser = generic_grid(6);
    let vp = Rect::new(0, 0, 400, 400);

    let mut cache = test_artwork_cache();
    let mut reader = CountingReader::new().shipping(IconKind::File, vec![0xab; 64]);
    let mut rasteriser = CountingRasteriser::new(Color::rgb(0x11, 0x22, 0x33));
    grid_surface(
        &browser,
        vp,
        &mut IconArtworkSource::new(&mut cache, &mut reader, &mut rasteriser),
    );
    assert!(cache.charged_bytes() > 0, "the decode is retained");

    // Closing the window ends the cache: the decoded pixels are given back.
    cache.teardown();
    assert_eq!(cache.charged_bytes(), 0);
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

// --- FM8b: the ownership-edit model (validate + commit a new owner) --------
//
// The `owner_edit` model runs end to end over the `MockFs` fixture, so every
// validation, transactional, and fail-closed branch of `set_owner_selected`
// runs in `cargo test` without a kernel. The authority rule itself
// (`CAP_FS_CHOWN`, group membership, set-*id* strip) is the kernel's and is
// proven in `kernel/core`; here the engine only names the change and surfaces
// the seam's outcome.

mod owner_edit_model {
    use core::cell::RefCell;

    use alloc::string::ToString;

    use tairix_abi::fs::FS_OWNER_UNCHANGED;
    use tairix_abi::Errno;

    use super::MockFs;
    use crate::browser::Browser;
    use crate::owner_edit::{validate_owner, OwnerChange, OwnerError};

    #[test]
    fn commit_applies_the_owner_to_the_selected_node() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");

        let seen = RefCell::new(None);
        let result = browser.set_owner_selected(
            OwnerChange {
                uid: Some(1000),
                gid: Some(50),
            },
            |path, uid, gid| {
                *seen.borrow_mut() = Some((path.to_string(), uid, gid));
                Ok(())
            },
        );

        assert_eq!(result, Ok(()));
        assert_eq!(*seen.borrow(), Some(("/Apps".to_string(), 1000, 50)));
        // The listing is unchanged: an ownership change touches no `Entry`.
        assert_eq!(
            super::names(&browser),
            ["Apps", "Storage", "System", "Users"]
        );
    }

    #[test]
    fn an_unchanged_field_marshals_the_sentinel() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");

        let seen = RefCell::new(None);
        // Group-only change: the uid field must reach the seam as the
        // reserved "unchanged" sentinel, never a fabricated id.
        let result = browser.set_owner_selected(OwnerChange::group(7), |_, uid, gid| {
            *seen.borrow_mut() = Some((uid, gid));
            Ok(())
        });
        assert_eq!(result, Ok(()));
        assert_eq!(*seen.borrow(), Some((FS_OWNER_UNCHANGED, 7)));
    }

    #[test]
    fn a_sentinel_target_is_refused_before_any_syscall() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        // The reserved sentinel is not a real id: naming it as a target is
        // refused rather than misread as "leave unchanged".
        let result =
            browser.set_owner_selected(OwnerChange::user(FS_OWNER_UNCHANGED), |_, _, _| {
                panic!("the VFS must not be touched for an invalid id");
            });
        assert_eq!(result, Err(OwnerError::Invalid));
    }

    #[test]
    fn a_vfs_refusal_is_surfaced_and_the_node_is_unchanged() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.select(0).expect("select Apps");
        // The missing-`CAP_FS_CHOWN` denial (or any VFS refusal) surfaces as
        // `Refused`, leaving the listing exactly as it was.
        let result = browser
            .set_owner_selected(OwnerChange::user(0), |_, _, _| Err(Errno::PermissionDenied));
        assert_eq!(result, Err(OwnerError::Refused(Errno::PermissionDenied)));
        assert_eq!(
            super::names(&browser),
            ["Apps", "Storage", "System", "Users"]
        );
    }

    #[test]
    fn an_empty_directory_reports_no_selection() {
        let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
        browser.open_index(2).expect("enter System");
        browser
            .open_index(0)
            .expect("enter the empty Fonts directory");
        assert_eq!(browser.selected_name(), None);
        let result =
            browser.set_owner_selected(OwnerChange::user(1), |_, _, _| panic!("nothing to change"));
        assert_eq!(result, Err(OwnerError::NoSelection));
    }

    #[test]
    fn validate_owner_accepts_real_ids_and_refuses_the_sentinel() {
        assert_eq!(validate_owner(OwnerChange::default()), Ok(()));
        assert_eq!(validate_owner(OwnerChange::user(0)), Ok(()));
        assert_eq!(validate_owner(OwnerChange::group(1000)), Ok(()));
        assert_eq!(
            validate_owner(OwnerChange {
                uid: Some(1),
                gid: Some(2)
            }),
            Ok(())
        );
        assert_eq!(
            validate_owner(OwnerChange::user(FS_OWNER_UNCHANGED)),
            Err(OwnerError::Invalid)
        );
        assert_eq!(
            validate_owner(OwnerChange::group(FS_OWNER_UNCHANGED)),
            Err(OwnerError::Invalid)
        );
    }

    #[test]
    fn owner_change_constructors_and_is_empty() {
        assert_eq!(OwnerChange::user(5).uid, Some(5));
        assert_eq!(OwnerChange::user(5).gid, None);
        assert_eq!(OwnerChange::group(9).gid, Some(9));
        assert_eq!(OwnerChange::group(9).uid, None);
        assert!(OwnerChange::default().is_empty());
        assert!(!OwnerChange::user(0).is_empty());
    }

    #[test]
    fn every_owner_error_has_a_nonempty_message() {
        for err in [
            OwnerError::NoSelection,
            OwnerError::Invalid,
            OwnerError::Path(Errno::NotFound),
            OwnerError::Refused(Errno::PermissionDenied),
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

use crate::open_with::{applications_for, AppAssociation, BundleSource};

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

/// The association model derives a file's type through the shared registry, so
/// the type a bundle is matched against is exactly the one the tile draws
/// (the registry's own mapping is proven in `media_tests.rs`).
#[test]
fn the_offered_type_is_the_registry_type() {
    for (name, media) in [
        ("notes.txt", MediaType::TextPlain),
        ("README.md", MediaType::TextMarkdown),
        ("data.json", MediaType::Json),
        ("photo.png", MediaType::ImagePng),
        ("tool.rxe", MediaType::TairixRxe),
    ] {
        let claimant = AppAssociation::new(
            "claimant",
            "/Apps/claimant.app",
            vec![media.as_str().to_string()],
        );
        let offered = applications_for(name, core::slice::from_ref(&claimant));
        assert_eq!(offered.len(), 1, "{name}");
    }
}

#[test]
fn a_file_the_registry_cannot_type_is_offered_nothing() {
    // An unrecognised extension, no extension at all, and a bare dotfile each
    // yield an honest empty answer rather than a guessed default — even from a
    // store whose bundle claims the generic type.
    let catch_all = AppAssociation::new(
        "catch-all",
        "/Apps/catch-all.app",
        vec!["application/octet-stream".to_string()],
    );
    let bundles = [catch_all];
    for name in ["mystery.xyz", "Makefile", ".profile", "archive.", ""] {
        assert!(applications_for(name, &bundles).is_empty(), "{name}");
    }
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

/// An application declaring only the broad `text/plain` type — a plain text
/// editor, the case the subclass chain exists for.
fn plain_text_editor() -> AppAssociation {
    AppAssociation::new("editor", "/Apps/editor.app", vec!["text/plain".to_string()])
}

#[test]
fn a_generic_text_application_opens_a_specific_text_file() {
    let bundles = [plain_text_editor()];
    for name in [
        "notes.txt",
        "main.rs",
        "install.sh",
        "parse.c",
        "parse.h",
        "README.md",
        "rows.csv",
        "data.json",
        "deploy.yaml",
        "layout.xml",
        "index.html",
        "Main.java",
        "logo.svg",
    ] {
        let offered = applications_for(name, &bundles);
        assert_eq!(offered.len(), 1, "{name}");
        assert_eq!(offered[0].name(), "editor", "{name}");
    }
}

#[test]
fn a_generic_text_application_is_not_offered_for_binary_content() {
    // The chain widens a type, it does not open everything: nothing binary
    // subclasses plain text.
    let bundles = [plain_text_editor()];
    for name in [
        "photo.png",
        "release.zip",
        "manual.pdf",
        "tool.rxe",
        "tile.spr",
    ] {
        assert!(applications_for(name, &bundles).is_empty(), "{name}");
    }
}

#[test]
fn a_specific_declaration_outranks_a_generic_one() {
    // The generic editor is enumerated first, so only specificity ranking can
    // put the Rust application ahead of it.
    let bundles = [
        plain_text_editor(),
        AppAssociation::new(
            "rustide",
            "/Apps/rustide.app",
            vec!["text/x-rust".to_string()],
        ),
    ];
    let offered: Vec<&str> = applications_for("main.rs", &bundles)
        .iter()
        .map(|b| b.name())
        .collect();
    assert_eq!(offered, ["rustide", "editor"]);
    // A file the specific application does not claim leaves the answer alone.
    let offered: Vec<&str> = applications_for("notes.txt", &bundles)
        .iter()
        .map(|b| b.name())
        .collect();
    assert_eq!(offered, ["editor"]);
}

#[test]
fn a_two_step_chain_ranks_each_ancestor_in_turn() {
    // An SVG is XML and XML is text, so all three are offered — nearest claim
    // first, whatever order the store enumerated them in.
    let bundles = [
        plain_text_editor(),
        AppAssociation::new(
            "xmltool",
            "/Apps/xmltool.app",
            vec!["application/xml".to_string()],
        ),
        AppAssociation::new("draw", "/Apps/draw.app", vec!["image/svg+xml".to_string()]),
    ];
    let offered: Vec<&str> = applications_for("logo.svg", &bundles)
        .iter()
        .map(|b| b.name())
        .collect();
    assert_eq!(offered, ["draw", "xmltool", "editor"]);
}

#[test]
fn a_bundle_claiming_both_a_type_and_its_ancestor_is_offered_once() {
    let bundles = [
        AppAssociation::new(
            "studio",
            "/Apps/studio.app",
            vec!["text/plain".to_string(), "text/x-rust".to_string()],
        ),
        plain_text_editor(),
    ];
    let offered: Vec<&str> = applications_for("main.rs", &bundles)
        .iter()
        .map(|b| b.name())
        .collect();
    assert_eq!(offered, ["studio", "editor"]);
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

/// Build a minimal, well-formed `AppInfo` wire image (a header, the capability
/// body, then the MIME table) for the [`association_from_appinfo`] tests. The
/// signature is left zero: `association_from_appinfo` reads the declared types
/// as a display hint and never verifies the signature (the signed load gate
/// does that at launch), so an unsigned fixture exercises exactly the decode.
fn build_appinfo(name: &str, mimes: &[&str]) -> Vec<u8> {
    use tairix_abi::{
        AppInfoHeader, ABI_VERSION_CURRENT, APPINFO_MAGIC, BUNDLE_ID_MAX, BUNDLE_NAME_MAX,
        BUNDLE_VERSION_MAX, MIME_ENTRY_LEN, MIME_TYPE_MAX,
    };
    fn inline<const N: usize>(value: &str) -> [u8; N] {
        let mut buf = [0u8; N];
        buf[..value.len()].copy_from_slice(value.as_bytes());
        buf
    }
    let id = "os.tairix.fixture";
    let version = "0.1.0";
    let header = AppInfoHeader {
        magic: APPINFO_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: 0,
        capability_count: 0,
        mime_count: u16::try_from(mimes.len()).expect("mime count fits"),
        id_len: u8::try_from(id.len()).expect("id fits"),
        name_len: u8::try_from(name.len()).expect("name fits"),
        version_len: u8::try_from(version.len()).expect("version fits"),
        library_icon_len: 0,
        library: 0,
        reserved0: [0; 3],
        id: inline::<BUNDLE_ID_MAX>(id),
        name: inline::<BUNDLE_NAME_MAX>(name),
        version: inline::<BUNDLE_VERSION_MAX>(version),
        library_icon: [0; tairix_abi::LIBRARY_ICON_MAX],
        syscall_table_hash: [0; 32],
        content_hash: [0; 32],
        signer_pubkey: [0; 32],
        signature: [0; 64],
    };
    let mut bytes = header.to_le_bytes().to_vec();
    for mime in mimes {
        let mut entry = [0u8; MIME_ENTRY_LEN];
        assert!(mime.len() <= MIME_TYPE_MAX);
        entry[0] = u8::try_from(mime.len()).expect("mime fits");
        entry[1..=mime.len()].copy_from_slice(mime.as_bytes());
        bytes.extend_from_slice(&entry);
    }
    bytes
}

#[test]
fn association_from_appinfo_reads_the_name_and_declared_types() {
    let bytes = build_appinfo("viewer", &["text/plain", "text/markdown"]);
    let assoc = crate::open_with::association_from_appinfo("/System/Apps/viewer.app", &bytes)
        .expect("decodes");
    assert_eq!(assoc.name(), "viewer");
    assert_eq!(assoc.bundle_path(), "/System/Apps/viewer.app");
    assert_eq!(assoc.mime_types(), ["text/plain", "text/markdown"]);
    // It composes with the matcher exactly as the mock store does.
    assert!(applications_for("notes.txt", core::slice::from_ref(&assoc))
        .iter()
        .any(|b| b.name() == "viewer"));
}

#[test]
fn association_from_appinfo_reads_a_bundle_that_declares_no_types() {
    // A pure command declares no associations: it decodes to an empty MIME
    // set (never an error) and is simply never an "open with" candidate.
    let bytes = build_appinfo("printf", &[]);
    let assoc = crate::open_with::association_from_appinfo("/System/Apps/printf.app", &bytes)
        .expect("decodes");
    assert!(assoc.mime_types().is_empty());
    assert!(applications_for("notes.txt", core::slice::from_ref(&assoc)).is_empty());
}

#[test]
fn association_from_appinfo_fails_closed_on_garbage() {
    // A truncated or non-manifest blob is skipped, never offered on a guess.
    assert!(crate::open_with::association_from_appinfo("/x.app", b"not a manifest").is_none());
    assert!(crate::open_with::association_from_appinfo("/x.app", &[]).is_none());
    // A header claiming a MIME entry the body does not carry fails closed too.
    let mut bytes = build_appinfo("viewer", &["text/plain"]);
    let short = bytes.len() - 4;
    bytes.truncate(short);
    assert!(crate::open_with::association_from_appinfo("/x.app", &bytes).is_none());
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

// ---- FM7b: the delete model (`delete`) ----

/// The delete target whose leaf name is `name`, for order-independent
/// assertions (a `plan_delete` orders by listing, not by selection order).
fn delete_target<'a>(
    plan: &'a crate::delete::DeletePlan,
    name: &str,
) -> &'a crate::delete::DeleteTarget {
    plan.targets()
        .iter()
        .find(|t| t.name() == name)
        .expect("a target with that name")
}

#[test]
fn plan_delete_captures_the_selected_targets_and_their_kinds() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // /System sorted: [Fonts (dir), Security (dir), Kernel (file)]. Select the
    // directory Fonts and the file Kernel.
    browser.select(0).expect("focus Fonts");
    browser.toggle_selection(2).expect("also Kernel");

    let plan = browser.plan_delete().expect("a plan");
    assert_eq!(plan.len(), 2);
    assert!(!plan.is_empty());
    assert!(plan.has_directories());

    let fonts = delete_target(&plan, "Fonts");
    assert_eq!(fonts.path(), comps(&["System", "Fonts"]).as_slice());
    assert!(fonts.is_directory());

    let kernel = delete_target(&plan, "Kernel");
    assert_eq!(kernel.path(), comps(&["System", "Kernel"]).as_slice());
    assert!(!kernel.is_directory());
}

#[test]
fn plan_delete_is_none_when_nothing_is_selected() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.clear_selection();
    assert!(browser.selection().is_empty());
    assert!(browser.plan_delete().is_none());
}

#[test]
fn plan_delete_of_only_files_has_no_directories() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    browser.open_index(2).expect("enter System");
    // Select only the regular file Kernel (index 2).
    browser.select(2).expect("focus Kernel");
    let plan = browser.plan_delete().expect("a plan");
    assert_eq!(plan.len(), 1);
    assert!(!plan.has_directories());
    assert!(!plan.targets()[0].is_directory());
}

#[test]
fn plan_delete_marks_a_bundle_as_directory_backed() {
    use tairix_abi::time::Time64;
    let mut fs = MockFs::fixture();
    fs.dirs.insert(
        "/Apps".to_string(),
        vec![
            Entry::new(
                "Example.app",
                crate::entry::EntryKind::Bundle,
                0,
                Time64::UNIX_EPOCH,
            ),
            Entry::file("notes.txt"),
        ],
    );
    let mut browser = Browser::open_root(fs).expect("root");
    // Sorted root order is [Apps, Storage, System, Users]; Apps is index 0.
    browser.open_index(0).expect("enter Apps");
    browser.select_all();

    let plan = browser.plan_delete().expect("a plan");
    assert_eq!(plan.len(), 2);
    // A bundle is directory-backed on disk even though the browser does not
    // descend into it, so it is removed recursively as the directory it is.
    assert!(delete_target(&plan, "Example.app").is_directory());
    assert!(!delete_target(&plan, "notes.txt").is_directory());
    assert!(plan.has_directories());
}

#[test]
fn delete_plan_new_refuses_empty_or_root_targets() {
    // Nothing to delete.
    assert!(DeletePlan::new(Vec::new()).is_none());
    // A root (empty component) target could remove the root itself.
    assert!(DeletePlan::new(vec![(Vec::new(), true)]).is_none());
    // Any root target in the set poisons the whole plan (fail closed).
    assert!(DeletePlan::new(vec![(comps(&["a"]), false), (Vec::new(), true)]).is_none());

    let plan = DeletePlan::new(vec![(comps(&["System", "Kernel"]), false)]).expect("a valid plan");
    assert_eq!(plan.len(), 1);
    let target = &plan.targets()[0];
    assert_eq!(target.name(), "Kernel");
    assert_eq!(target.path(), comps(&["System", "Kernel"]).as_slice());
    assert!(!target.is_directory());
}

// ---- FM7b: the recursive-delete execution model (`DeleteWalk`) ----

/// Drive a [`DeleteWalk`] to completion against an in-memory tree keyed by
/// absolute-path spelling → children `(name, is_directory)`, returning the
/// absolute-path spelling of every node in the order it was removed.
///
/// A directory absent from `tree` lists as empty. Every `expand`/
/// `complete_removal` must succeed — a walk driven strictly in step never
/// errors — so any protocol slip is a test failure, not a swallowed result.
fn drive_delete(plan: &DeletePlan, tree: &BTreeMap<String, Vec<(String, bool)>>) -> Vec<String> {
    let mut walk = DeleteWalk::from_plan(plan);
    let mut order = Vec::new();
    // A generous ceiling so a modelling bug (a walk that never completes) fails
    // the test rather than looping forever.
    for _ in 0..10_000 {
        match walk.next_action() {
            None => return order,
            Some(DeleteAction::List(path)) => {
                let children = tree.get(&key(path)).cloned().unwrap_or_default();
                walk.expand(&children).expect("expand in step");
            }
            Some(DeleteAction::Remove { path, .. }) => {
                order.push(key(path));
                walk.complete_removal().expect("remove in step");
            }
        }
    }
    panic!("delete walk did not complete within the step ceiling");
}

#[test]
fn delete_walk_removes_a_single_file() {
    let plan = DeletePlan::new(vec![(comps(&["System", "Kernel"]), false)]).expect("plan");
    let mut walk = DeleteWalk::from_plan(&plan);

    assert!(!walk.is_complete());
    assert_eq!(walk.removed(), 0);
    match walk.next_action() {
        Some(DeleteAction::Remove { path, is_directory }) => {
            assert_eq!(path, comps(&["System", "Kernel"]).as_slice());
            assert!(!is_directory);
        }
        other => panic!("expected a Remove, got {other:?}"),
    }
    walk.complete_removal().expect("remove the file");
    assert!(walk.is_complete());
    assert_eq!(walk.removed(), 1);
    assert!(walk.next_action().is_none());
}

#[test]
fn delete_walk_lists_an_empty_directory_then_removes_it() {
    let plan = DeletePlan::new(vec![(comps(&["Storage", "empty"]), true)]).expect("plan");
    let mut walk = DeleteWalk::from_plan(&plan);

    // A directory is always listed first, even when it turns out empty.
    match walk.next_action() {
        Some(DeleteAction::List(path)) => {
            assert_eq!(path, comps(&["Storage", "empty"]).as_slice());
        }
        other => panic!("expected a List, got {other:?}"),
    }
    walk.expand(&[]).expect("expand empty");
    // Now the emptied directory is removed as a leaf.
    match walk.next_action() {
        Some(DeleteAction::Remove { path, is_directory }) => {
            assert_eq!(path, comps(&["Storage", "empty"]).as_slice());
            assert!(is_directory);
        }
        other => panic!("expected a Remove, got {other:?}"),
    }
    walk.complete_removal().expect("remove the directory");
    assert!(walk.is_complete());
    assert_eq!(walk.removed(), 1);
}

#[test]
fn delete_walk_removes_contents_before_the_directory_depth_first() {
    // /Storage/tree/{a.txt, sub/{b.txt}}, listed in that order.
    let mut tree: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    tree.insert(
        key(&comps(&["Storage", "tree"])),
        vec![("a.txt".to_string(), false), ("sub".to_string(), true)],
    );
    tree.insert(
        key(&comps(&["Storage", "tree", "sub"])),
        vec![("b.txt".to_string(), false)],
    );

    let plan = DeletePlan::new(vec![(comps(&["Storage", "tree"]), true)]).expect("plan");
    let order = drive_delete(&plan, &tree);

    // Contents before their container, listing order among siblings, and the
    // subtree fully removed before we come back up to the parent.
    assert_eq!(
        order,
        vec![
            key(&comps(&["Storage", "tree", "a.txt"])),
            key(&comps(&["Storage", "tree", "sub", "b.txt"])),
            key(&comps(&["Storage", "tree", "sub"])),
            key(&comps(&["Storage", "tree"])),
        ]
    );
}

#[test]
fn delete_walk_processes_multiple_targets_in_listing_order() {
    let mut tree: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    tree.insert(
        key(&comps(&["Users", "dir"])),
        vec![("inner".to_string(), false)],
    );

    // A file, then a directory, then another file — the plan's listing order.
    let plan = DeletePlan::new(vec![
        (comps(&["Users", "first.txt"]), false),
        (comps(&["Users", "dir"]), true),
        (comps(&["Users", "last.txt"]), false),
    ])
    .expect("plan");
    let order = drive_delete(&plan, &tree);

    assert_eq!(
        order,
        vec![
            key(&comps(&["Users", "first.txt"])),
            key(&comps(&["Users", "dir", "inner"])),
            key(&comps(&["Users", "dir"])),
            key(&comps(&["Users", "last.txt"])),
        ]
    );
}

#[test]
fn delete_walk_expand_refuses_a_tree_deeper_than_the_bound() {
    // A directory target that already sits at the maximum depth: expanding it
    // would name a child one component deeper than the bound.
    let deep: Vec<String> = (0..MAX_DELETE_DEPTH).map(|i| format!("d{i}")).collect();
    assert_eq!(deep.len(), MAX_DELETE_DEPTH);
    let plan = DeletePlan::new(vec![(deep, true)]).expect("plan");
    let mut walk = DeleteWalk::from_plan(&plan);

    assert!(matches!(walk.next_action(), Some(DeleteAction::List(_))));
    assert_eq!(
        walk.expand(&[("child".to_string(), false)]),
        Err(DeleteError::TooDeep)
    );
    // Refused, and the walk is left exactly where it was (fail closed): still a
    // List of the same directory, nothing removed.
    assert!(matches!(walk.next_action(), Some(DeleteAction::List(_))));
    assert_eq!(walk.removed(), 0);
}

#[test]
fn delete_walk_fails_closed_when_driven_out_of_step() {
    // `expand` on a leaf file is out of step.
    let file_plan = DeletePlan::new(vec![(comps(&["System", "Kernel"]), false)]).expect("plan");
    let mut walk = DeleteWalk::from_plan(&file_plan);
    assert!(matches!(
        walk.next_action(),
        Some(DeleteAction::Remove { .. })
    ));
    assert_eq!(walk.expand(&[]), Err(DeleteError::OutOfStep));
    // Unchanged.
    assert!(matches!(
        walk.next_action(),
        Some(DeleteAction::Remove { .. })
    ));

    // `complete_removal` on a directory whose contents were not listed is out
    // of step.
    let dir_plan = DeletePlan::new(vec![(comps(&["Storage", "d"]), true)]).expect("plan");
    let mut walk = DeleteWalk::from_plan(&dir_plan);
    assert!(matches!(walk.next_action(), Some(DeleteAction::List(_))));
    assert_eq!(walk.complete_removal(), Err(DeleteError::OutOfStep));
    assert!(matches!(walk.next_action(), Some(DeleteAction::List(_))));

    // Either driver call on a finished walk is out of step.
    walk.expand(&[]).expect("empty the directory");
    walk.complete_removal().expect("remove it");
    assert!(walk.is_complete());
    assert_eq!(walk.expand(&[]), Err(DeleteError::OutOfStep));
    assert_eq!(walk.complete_removal(), Err(DeleteError::OutOfStep));
}

#[test]
fn delete_walk_holds_its_position_across_an_interruption() {
    let mut tree: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    tree.insert(
        key(&comps(&["Users", "dir"])),
        vec![("x".to_string(), false), ("y".to_string(), false)],
    );
    let plan = DeletePlan::new(vec![(comps(&["Users", "dir"]), true)]).expect("plan");
    let mut walk = DeleteWalk::from_plan(&plan);

    // List, then remove exactly one child, then "stop" (as a Cancel or a
    // preemption would) holding the walk.
    let list = walk.next_action().expect("a step");
    assert!(matches!(list, DeleteAction::List(_)));
    walk.expand(&[("x".to_string(), false), ("y".to_string(), false)])
        .expect("expand");
    if let Some(DeleteAction::Remove { .. }) = walk.next_action() {
        walk.complete_removal().expect("remove x");
    }
    assert_eq!(walk.removed(), 1);
    assert!(!walk.is_complete());

    // Resuming from exactly here removes the remaining child then the directory
    // — no repeat of the child already removed, no skip.
    let mut order = Vec::new();
    while let Some(action) = walk.next_action() {
        match action {
            DeleteAction::List(path) => {
                let children = tree.get(&key(path)).cloned().unwrap_or_default();
                walk.expand(&children).expect("expand");
            }
            DeleteAction::Remove { path, .. } => {
                order.push(key(path));
                walk.complete_removal().expect("remove");
            }
        }
    }
    assert_eq!(
        order,
        vec![
            key(&comps(&["Users", "dir", "y"])),
            key(&comps(&["Users", "dir"])),
        ]
    );
    assert_eq!(walk.removed(), 3);
}

#[test]
fn delete_error_messages_are_non_empty() {
    assert!(!DeleteError::TooDeep.to_string().is_empty());
    assert!(!DeleteError::OutOfStep.to_string().is_empty());
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

// ---- FM7b: the recursive-copy execution model (`CopyWalk`) ----

/// Drive a [`CopyWalk`] to completion against an in-memory *source* tree keyed
/// by absolute-path spelling → children `(name, is_directory)`, returning the
/// absolute *destination*-path spelling of every node in the order the copy
/// performed it (a directory when created, a file when copied).
///
/// A source directory absent from `tree` lists as empty. Every driver call
/// must succeed — a walk driven strictly in step never errors — so any protocol
/// slip is a test failure, not a swallowed result.
fn drive_copy(walk: &mut CopyWalk, tree: &BTreeMap<String, Vec<(String, bool)>>) -> Vec<String> {
    let mut order = Vec::new();
    // A generous ceiling so a modelling bug (a walk that never completes) fails
    // the test rather than looping forever.
    for _ in 0..10_000 {
        match walk.next_action() {
            None => return order,
            Some(CopyAction::MakeDir { dest }) => {
                order.push(key(dest));
                walk.created().expect("created in step");
            }
            Some(CopyAction::List { source }) => {
                let children = tree.get(&key(source)).cloned().unwrap_or_default();
                walk.expand(&children).expect("expand in step");
            }
            Some(CopyAction::CopyFile { dest, .. }) => {
                order.push(key(dest));
                walk.copied_file().expect("copy file in step");
            }
        }
    }
    panic!("copy walk did not complete within the step ceiling");
}

#[test]
fn copy_walk_copies_a_single_file() {
    let mut walk = CopyWalk::from_items(vec![(
        comps(&["Users", "a.txt"]),
        comps(&["Storage", "a.txt"]),
        false,
    )])
    .expect("walk");

    assert!(!walk.is_complete());
    assert_eq!(walk.copied(), 0);
    match walk.next_action() {
        Some(CopyAction::CopyFile { source, dest }) => {
            assert_eq!(source, comps(&["Users", "a.txt"]).as_slice());
            assert_eq!(dest, comps(&["Storage", "a.txt"]).as_slice());
        }
        other => panic!("expected a CopyFile, got {other:?}"),
    }
    walk.copied_file().expect("copy the file");
    assert!(walk.is_complete());
    assert_eq!(walk.copied(), 1);
    assert!(walk.next_action().is_none());
}

#[test]
fn copy_walk_makes_the_destination_directory_before_listing_an_empty_one() {
    let mut walk = CopyWalk::from_items(vec![(
        comps(&["Users", "empty"]),
        comps(&["Storage", "empty"]),
        true,
    )])
    .expect("walk");

    // The destination directory is created first, before its contents are read.
    match walk.next_action() {
        Some(CopyAction::MakeDir { dest }) => {
            assert_eq!(dest, comps(&["Storage", "empty"]).as_slice());
        }
        other => panic!("expected a MakeDir, got {other:?}"),
    }
    walk.created().expect("dest dir made");
    assert_eq!(walk.copied(), 1);
    // Then the source is listed; an empty listing finishes the directory.
    match walk.next_action() {
        Some(CopyAction::List { source }) => {
            assert_eq!(source, comps(&["Users", "empty"]).as_slice());
        }
        other => panic!("expected a List, got {other:?}"),
    }
    walk.expand(&[]).expect("expand empty");
    assert!(walk.is_complete());
    assert_eq!(walk.copied(), 1);
}

#[test]
fn copy_walk_creates_containers_before_contents_depth_first() {
    // /Users/tree/{a.txt, sub/{b.txt}}, listed in that order → /Storage/tree.
    let mut tree: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    tree.insert(
        key(&comps(&["Users", "tree"])),
        vec![("a.txt".to_string(), false), ("sub".to_string(), true)],
    );
    tree.insert(
        key(&comps(&["Users", "tree", "sub"])),
        vec![("b.txt".to_string(), false)],
    );

    let mut walk = CopyWalk::from_items(vec![(
        comps(&["Users", "tree"]),
        comps(&["Storage", "tree"]),
        true,
    )])
    .expect("walk");
    let order = drive_copy(&mut walk, &tree);

    // A container is created before its contents, siblings keep listing order,
    // and a subtree is fully copied before we return to the parent.
    assert_eq!(
        order,
        vec![
            key(&comps(&["Storage", "tree"])),
            key(&comps(&["Storage", "tree", "a.txt"])),
            key(&comps(&["Storage", "tree", "sub"])),
            key(&comps(&["Storage", "tree", "sub", "b.txt"])),
        ]
    );
    // Every node counted once: tree, a.txt, sub, b.txt.
    assert_eq!(walk.copied(), 4);
}

#[test]
fn copy_walk_processes_multiple_items_in_order() {
    let mut tree: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    tree.insert(
        key(&comps(&["Users", "dir"])),
        vec![("inner".to_string(), false)],
    );

    let mut walk = CopyWalk::from_items(vec![
        (
            comps(&["Users", "first.txt"]),
            comps(&["Storage", "first.txt"]),
            false,
        ),
        (comps(&["Users", "dir"]), comps(&["Storage", "dir"]), true),
        (
            comps(&["Users", "last.txt"]),
            comps(&["Storage", "last.txt"]),
            false,
        ),
    ])
    .expect("walk");
    let order = drive_copy(&mut walk, &tree);

    assert_eq!(
        order,
        vec![
            key(&comps(&["Storage", "first.txt"])),
            key(&comps(&["Storage", "dir"])),
            key(&comps(&["Storage", "dir", "inner"])),
            key(&comps(&["Storage", "last.txt"])),
        ]
    );
    assert_eq!(walk.copied(), 4);
}

#[test]
fn copy_walk_expand_refuses_a_tree_deeper_than_the_bound() {
    // A directory source that already sits at the maximum depth: expanding it
    // would name a child one component deeper than the bound.
    let deep: Vec<String> = (0..MAX_COPY_DEPTH).map(|i| format!("d{i}")).collect();
    assert_eq!(deep.len(), MAX_COPY_DEPTH);
    let mut walk =
        CopyWalk::from_items(vec![(deep, comps(&["Storage", "dst"]), true)]).expect("walk");

    // Make the destination, then the list step refuses to descend further.
    assert!(matches!(
        walk.next_action(),
        Some(CopyAction::MakeDir { .. })
    ));
    walk.created().expect("dest dir made");
    assert!(matches!(walk.next_action(), Some(CopyAction::List { .. })));
    assert_eq!(
        walk.expand(&[("child".to_string(), false)]),
        Err(CopyWalkError::TooDeep)
    );
    // Refused, and the walk is left exactly where it was (fail closed): still a
    // List of the same directory.
    assert!(matches!(walk.next_action(), Some(CopyAction::List { .. })));
}

#[test]
fn copy_walk_fails_closed_when_driven_out_of_step() {
    // `created` / `expand` on a leaf-file step are out of step.
    let mut walk = CopyWalk::from_items(vec![(
        comps(&["Users", "a.txt"]),
        comps(&["Storage", "a.txt"]),
        false,
    )])
    .expect("walk");
    assert!(matches!(
        walk.next_action(),
        Some(CopyAction::CopyFile { .. })
    ));
    assert_eq!(walk.created(), Err(CopyWalkError::OutOfStep));
    assert_eq!(walk.expand(&[]), Err(CopyWalkError::OutOfStep));
    assert!(matches!(
        walk.next_action(),
        Some(CopyAction::CopyFile { .. })
    ));

    // `expand` on a not-yet-created directory, and `copied_file` on a directory,
    // are out of step.
    let mut walk = CopyWalk::from_items(vec![(
        comps(&["Users", "d"]),
        comps(&["Storage", "d"]),
        true,
    )])
    .expect("walk");
    assert!(matches!(
        walk.next_action(),
        Some(CopyAction::MakeDir { .. })
    ));
    assert_eq!(walk.expand(&[]), Err(CopyWalkError::OutOfStep));
    assert_eq!(walk.copied_file(), Err(CopyWalkError::OutOfStep));
    assert!(matches!(
        walk.next_action(),
        Some(CopyAction::MakeDir { .. })
    ));

    // Any driver call on a finished walk is out of step.
    walk.created().expect("dest dir made");
    walk.expand(&[]).expect("empty the directory");
    assert!(walk.is_complete());
    assert_eq!(walk.created(), Err(CopyWalkError::OutOfStep));
    assert_eq!(walk.expand(&[]), Err(CopyWalkError::OutOfStep));
    assert_eq!(walk.copied_file(), Err(CopyWalkError::OutOfStep));
}

#[test]
fn copy_walk_holds_its_position_across_an_interruption() {
    let mut tree: BTreeMap<String, Vec<(String, bool)>> = BTreeMap::new();
    tree.insert(
        key(&comps(&["Users", "dir"])),
        vec![("x".to_string(), false), ("y".to_string(), false)],
    );
    let mut walk = CopyWalk::from_items(vec![(
        comps(&["Users", "dir"]),
        comps(&["Storage", "dir"]),
        true,
    )])
    .expect("walk");

    // Make the dest dir, list it, then copy exactly one child and "stop" (as a
    // Cancel or a preemption would) holding the walk.
    assert!(matches!(
        walk.next_action(),
        Some(CopyAction::MakeDir { .. })
    ));
    walk.created().expect("dest dir made");
    assert!(matches!(walk.next_action(), Some(CopyAction::List { .. })));
    walk.expand(&[("x".to_string(), false), ("y".to_string(), false)])
        .expect("expand");
    if let Some(CopyAction::CopyFile { .. }) = walk.next_action() {
        walk.copied_file().expect("copy x");
    }
    // The directory and the first child are done; the second child remains.
    assert_eq!(walk.copied(), 2);
    assert!(!walk.is_complete());

    // Resuming from exactly here copies the remaining child — no repeat of the
    // child already copied, no skip.
    let order = drive_copy(&mut walk, &tree);
    assert_eq!(order, vec![key(&comps(&["Storage", "dir", "y"]))]);
    assert_eq!(walk.copied(), 3);
}

#[test]
fn copy_walk_from_items_fails_closed() {
    // Nothing to copy.
    assert!(CopyWalk::from_items(vec![]).is_none());
    // A source or destination that names the root (an empty component list).
    assert!(CopyWalk::from_items(vec![(Vec::new(), comps(&["Storage", "a"]), false)]).is_none());
    assert!(CopyWalk::from_items(vec![(comps(&["Users", "a"]), Vec::new(), false)]).is_none());
    // A valid item builds a walk.
    assert!(CopyWalk::from_items(vec![(
        comps(&["Users", "a"]),
        comps(&["Storage", "a"]),
        false
    )])
    .is_some());
}

#[test]
fn copy_walk_error_messages_are_non_empty() {
    assert!(!CopyWalkError::TooDeep.to_string().is_empty());
    assert!(!CopyWalkError::OutOfStep.to_string().is_empty());
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
        ContextCommand::PinToTaskbar,
        ContextCommand::Rename,
        ContextCommand::Cut,
        ContextCommand::Copy,
        ContextCommand::Properties,
        ContextCommand::Delete,
    ] {
        assert!(!menu.is_enabled(command), "{command:?} without a selection");
    }
    assert!(!menu.is_enabled(ContextCommand::Paste));
}

#[test]
fn the_context_menu_enables_the_item_commands_on_a_directory() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    // A directory descends on Open; every selection-scoped command is offered,
    // but Open With… is not — a directory has no application to choose — and
    // neither is Pin to taskbar — only a bundle names a pinnable application.
    let browser = Browser::open_root(activation_source()).expect("root");
    assert_eq!(browser.selected_name(), Some("Docs"));
    let menu = ContextMenuModel::for_browser(&browser, false);
    assert!(menu.is_enabled(ContextCommand::Open));
    assert!(!menu.is_enabled(ContextCommand::OpenWith));
    assert!(!menu.is_enabled(ContextCommand::PinToTaskbar));
    assert!(menu.is_enabled(ContextCommand::Rename));
    assert!(menu.is_enabled(ContextCommand::Cut));
    assert!(menu.is_enabled(ContextCommand::Copy));
    assert!(menu.is_enabled(ContextCommand::Properties));
    assert!(menu.is_enabled(ContextCommand::Delete));
}

#[test]
fn the_context_menu_enables_the_item_commands_on_a_bundle() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    // A bundle is a selection like any other: Open launches it, and the
    // selection-scoped commands apply. Open With… is not offered — a bundle
    // launches itself, so there is no application to choose for it — while
    // Pin to taskbar is offered exactly here: a bundle names an installed
    // application the session can pin.
    let mut browser = Browser::open_root(activation_source()).expect("root");
    browser.select(1).expect("select Editor.app");
    assert!(browser.selected_entry().expect("bundle").is_bundle());
    let menu = ContextMenuModel::for_browser(&browser, false);
    assert!(menu.is_enabled(ContextCommand::Open));
    assert!(!menu.is_enabled(ContextCommand::OpenWith));
    assert!(menu.is_enabled(ContextCommand::PinToTaskbar));
    assert!(menu.is_enabled(ContextCommand::Rename));
    assert!(menu.is_enabled(ContextCommand::Properties));
    assert!(menu.is_enabled(ContextCommand::Delete));
    // The drawn row's text: a plain label and no keyboard equivalent — the
    // command is pointer-only, exactly like Open With….
    assert_eq!(ContextCommand::PinToTaskbar.label(), "Pin to taskbar");
    assert_eq!(ContextCommand::PinToTaskbar.shortcut(), "");
}

#[test]
fn the_context_menu_enables_the_item_commands_on_a_file() {
    use crate::chrome::{ContextCommand, ContextMenuModel};

    let mut browser = Browser::open_root(activation_source()).expect("root");
    browser.select(2).expect("select notes.txt");
    assert_eq!(browser.selected_name(), Some("notes.txt"));
    let menu = ContextMenuModel::for_browser(&browser, false);
    assert!(menu.is_enabled(ContextCommand::Open));
    // Open With… is offered only for a regular file, so it is enabled here;
    // Pin to taskbar is offered only for a bundle, so it is not.
    assert!(menu.is_enabled(ContextCommand::OpenWith));
    assert!(!menu.is_enabled(ContextCommand::PinToTaskbar));
    assert!(menu.is_enabled(ContextCommand::Rename));
    assert!(menu.is_enabled(ContextCommand::Cut));
    assert!(menu.is_enabled(ContextCommand::Copy));
    assert!(menu.is_enabled(ContextCommand::Properties));
    assert!(menu.is_enabled(ContextCommand::Delete));
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
    // exactly once, in a stable order. Open With… joined with its FM6b chooser
    // verb; Delete now joins with FM9-c's confirm-and-remove verb; Pin to
    // taskbar joins with the taskbar-pin stage's window-channel verb, beside
    // the launch commands it belongs with. New Folder stays absent — the drawn
    // menu has no verb to invoke for it (it is a toolbar write tool), so it
    // would be speculative surface here.
    assert_eq!(
        CONTEXT_COMMANDS,
        &[
            ContextCommand::Open,
            ContextCommand::OpenWith,
            ContextCommand::PinToTaskbar,
            ContextCommand::Rename,
            ContextCommand::Cut,
            ContextCommand::Copy,
            ContextCommand::Paste,
            ContextCommand::Properties,
            ContextCommand::Delete,
        ]
    );
}

#[test]
fn build_context_menu_labels_each_row_and_mirrors_the_model_enablement() {
    use crate::chrome::{ContextCommand, ContextMenuModel, CONTEXT_COMMANDS};
    use crate::render::build_context_menu;

    // A selected entry, no clipboard: the selection commands are actionable and
    // Paste is disabled. Each drawn row carries its command's label and its
    // actionability mirrors the model, so a disabled command reads muted
    // (present) rather than vanishing.
    let browser = Browser::open_root(activation_source()).expect("root");
    let model = ContextMenuModel::for_browser(&browser, false);
    let menu = build_context_menu(model);
    assert_eq!(menu.len(), CONTEXT_COMMANDS.len());
    for (item, &command) in menu.items().iter().zip(CONTEXT_COMMANDS) {
        assert_eq!(item.label(), command.label());
        assert_eq!(
            item.state().is_actionable(),
            model.is_enabled(command),
            "{command:?} actionability mirrors the model"
        );
    }

    // Paste specifically is disabled with no clipboard and enabled with one.
    let paste_index = CONTEXT_COMMANDS
        .iter()
        .position(|&c| c == ContextCommand::Paste)
        .expect("Paste is modelled");
    assert!(!menu.items()[paste_index].state().is_actionable());
    let with_clip = build_context_menu(ContextMenuModel::for_browser(&browser, true));
    assert!(with_clip.items()[paste_index].state().is_actionable());
}

#[test]
fn context_menu_rect_anchors_at_the_click_and_clamps_within_the_viewport() {
    use crate::chrome::ContextMenuModel;
    use crate::render::{build_context_menu, context_menu_rect};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    // A window comfortably larger than the menu, so a mid-window anchor fits.
    let vp = Rect::new(0, 0, 800, 600);
    let browser = Browser::open_root(activation_source()).expect("root");
    let menu = build_context_menu(ContextMenuModel::for_browser(&browser, true));

    // Placed with its top-left at the click point when the whole menu fits.
    let rect = context_menu_rect(&menu, Point::new(40, 30), vp, font, &theme);
    assert_eq!((rect.origin.x, rect.origin.y), (40, 30));
    assert!(rect.width > 0 && rect.height > 0);

    // A click near the bottom-right corner shifts the menu left/up so the whole
    // menu stays inside the window rather than spilling off it.
    let corner = context_menu_rect(&menu, Point::new(798, 598), vp, font, &theme);
    assert!(
        corner.origin.x + i32::try_from(corner.width).unwrap() <= i32::try_from(vp.width).unwrap()
    );
    assert!(
        corner.origin.y + i32::try_from(corner.height).unwrap()
            <= i32::try_from(vp.height).unwrap()
    );
    assert!(corner.origin.x >= 0 && corner.origin.y >= 0);

    // A window smaller than the menu still yields a drawable clamped rect
    // (no panic), never a zero or over-size rectangle.
    let tiny = Rect::new(0, 0, 10, 8);
    let small = context_menu_rect(&menu, Point::new(3, 3), tiny, font, &theme);
    assert!(small.width >= 1 && small.width <= tiny.width);
    assert!(small.height >= 1 && small.height <= tiny.height);
}

#[test]
fn draw_context_menu_paints_into_the_surface_without_panicking() {
    use crate::chrome::ContextMenuModel;
    use crate::render::{build_context_menu, draw_context_menu};
    use tairix_raster::Surface;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, 400);
    let browser = Browser::open_root(activation_source()).expect("root");
    let menu = build_context_menu(ContextMenuModel::for_browser(&browser, false));

    let mut surface = Surface::new(vp.width, vp.height).expect("surface");
    let before = surface.pixels().to_vec();
    draw_context_menu(&mut surface, &menu, Point::new(20, 20), &theme, font, vp);
    assert_ne!(surface.pixels().to_vec(), before);

    // A degenerate viewport draws nothing and does not panic.
    let mut tiny = Surface::new(2, 2).expect("tiny surface");
    draw_context_menu(
        &mut tiny,
        &menu,
        Point::new(0, 0),
        &theme,
        font,
        Rect::new(0, 0, 2, 2),
    );
}

#[test]
fn context_menu_command_at_mirrors_the_enabled_rows_and_fails_closed() {
    use crate::chrome::{ContextCommand, ContextMenuModel, CONTEXT_COMMANDS};
    use crate::render::{build_context_menu, context_menu_command_at};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, 400);
    // A selection, no clipboard: the item commands are enabled, Paste disabled.
    let browser = Browser::open_root(activation_source()).expect("root");
    let model = ContextMenuModel::for_browser(&browser, false);
    let menu = build_context_menu(model);
    let anchor = Point::new(30, 24);

    // Scanning the whole window, every command the hit-test resolves is an
    // enabled one, and every enabled command is reachable — so the drawn rows
    // and the hit-test cover exactly the model's actionable commands, and a
    // disabled row (Paste) never resolves (fail closed).
    let mut seen: Vec<ContextCommand> = Vec::new();
    let mut y = 0;
    while y < i32::try_from(vp.height).unwrap() {
        let mut x = 0;
        while x < i32::try_from(vp.width).unwrap() {
            if let Some(cmd) =
                context_menu_command_at(&menu, anchor, vp, font, &theme, Point::new(x, y))
            {
                assert!(model.is_enabled(cmd), "resolved a disabled command {cmd:?}");
                if !seen.contains(&cmd) {
                    seen.push(cmd);
                }
            }
            x += 1;
        }
        y += 1;
    }
    for &cmd in CONTEXT_COMMANDS {
        assert_eq!(
            seen.contains(&cmd),
            model.is_enabled(cmd),
            "{cmd:?} reachable iff enabled"
        );
    }

    // A click well outside the menu resolves nothing (fail closed).
    assert_eq!(
        context_menu_command_at(&menu, anchor, vp, font, &theme, Point::new(399, 399)),
        None
    );
}

#[test]
fn context_menu_command_rect_mirrors_the_hit_test_for_each_command() {
    use crate::chrome::{ContextMenuModel, CONTEXT_COMMANDS};
    use crate::render::{build_context_menu, context_menu_command_at, context_menu_command_rect};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, 400);
    // A selection so the item commands (Delete among them) are enabled.
    let browser = Browser::open_root(activation_source()).expect("root");
    let model = ContextMenuModel::for_browser(&browser, false);
    let menu = build_context_menu(model);
    let anchor = Point::new(30, 24);

    // The forward rect of each command centres inside a row that the hit-test
    // resolves back to that same command — so the click point the harness aims
    // at and the app's own hit-test can never disagree (§2.2). A disabled
    // command still has a drawn (muted) row, so its rect exists but the
    // hit-test declines it (fail closed).
    for &command in CONTEXT_COMMANDS {
        let rect = context_menu_command_rect(&menu, anchor, vp, font, &theme, command)
            .expect("every listed command has a drawn row");
        let centre = Point::new(
            rect.left() + i32::try_from(rect.width / 2).unwrap(),
            rect.top() + i32::try_from(rect.height / 2).unwrap(),
        );
        let hit = context_menu_command_at(&menu, anchor, vp, font, &theme, centre);
        if model.is_enabled(command) {
            assert_eq!(hit, Some(command), "{command:?} rect round-trips");
        } else {
            assert_eq!(hit, None, "{command:?} disabled row fails closed");
        }
    }
}

#[test]
fn build_open_with_menu_lists_each_candidate_application_by_name_in_order() {
    use crate::render::build_open_with_menu;

    // The chooser draws one enabled row per candidate, captioned by the
    // bundle's name, in the order `applications_for` returned them.
    let mut store = open_with_store();
    let bundles = store.installed_bundles().expect("enumerate");
    let apps = applications_for("notes.txt", &bundles);
    assert_eq!(apps.len(), 2);

    let menu = build_open_with_menu(&apps);
    assert_eq!(menu.len(), apps.len());
    for (item, app) in menu.items().iter().zip(&apps) {
        assert_eq!(item.label(), app.name());
        // Every candidate is a genuine choice, so no row is disabled.
        assert!(item.state().is_actionable());
    }
}

#[test]
fn open_with_index_at_mirrors_the_rows_and_fails_closed_off_the_menu() {
    use crate::render::{build_open_with_menu, open_with_index_at};

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, 400);
    let mut store = open_with_store();
    let bundles = store.installed_bundles().expect("enumerate");
    let apps = applications_for("notes.txt", &bundles);
    let menu = build_open_with_menu(&apps);
    let anchor = Point::new(30, 24);

    // Scanning the whole window, every index the hit-test resolves is a valid
    // candidate index, and every candidate is reachable exactly once — so the
    // drawn rows and the hit-test cover the same application list (§2.2).
    let mut seen: Vec<usize> = Vec::new();
    let mut y = 0;
    while y < i32::try_from(vp.height).unwrap() {
        let mut x = 0;
        while x < i32::try_from(vp.width).unwrap() {
            if let Some(index) =
                open_with_index_at(&menu, anchor, vp, font, &theme, Point::new(x, y))
            {
                assert!(index < apps.len(), "resolved an out-of-range index {index}");
                if !seen.contains(&index) {
                    seen.push(index);
                }
            }
            x += 1;
        }
        y += 1;
    }
    seen.sort_unstable();
    assert_eq!(seen, (0..apps.len()).collect::<Vec<_>>());

    // A click well outside the menu resolves nothing (fail closed).
    assert_eq!(
        open_with_index_at(&menu, anchor, vp, font, &theme, Point::new(399, 399)),
        None
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
    use crate::chrome::{ManagerTool, ManagerToolModel, MANAGER_TOOLS};
    use crate::render::{manager_tool_at, toolbar_command_at, toolbar_height};
    use tairix_geometry::Point;

    // Every manager tool enabled, so the scan sees the full write-tool set
    // (the Empty Trash tool's own enable state is exercised separately).
    let tool_model = ManagerToolModel::new(true);

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
        let tool = manager_tool_at(&browser, &theme, vp, point, MANAGER_TOOLS, tool_model);
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
        assert_eq!(
            manager_tool_at(&browser, &theme, vp, point, &[], tool_model),
            None
        );
    }

    // A click below the toolbar strip is never a write tool either.
    assert_eq!(
        manager_tool_at(
            &browser,
            &theme,
            vp,
            Point::new(4, i32::try_from(toolbar_height(&theme)).unwrap()),
            MANAGER_TOOLS,
            tool_model,
        ),
        None
    );
}

#[test]
fn render_manager_tool_rect_is_the_forward_mirror_of_manager_tool_at() {
    use crate::chrome::{ManagerTool, ManagerToolModel, MANAGER_TOOLS};
    use crate::render::{manager_tool_at, manager_tool_rect};
    use tairix_geometry::Point;

    let tool_model = ManagerToolModel::new(true);

    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, 200);
    let browser = Browser::open_root(MockFs::fixture()).expect("root");

    // The New Folder tool has a rect, and that rect's centre hit-tests back
    // to exactly the New Folder tool — paint, hit-test, and aim geometry are
    // one definition.
    let rect = manager_tool_rect(&browser, &theme, vp, MANAGER_TOOLS, ManagerTool::NewFolder)
        .expect("the New Folder tool is laid out");
    let centre = Point::new(
        rect.left() + i32::try_from(rect.width).unwrap() / 2,
        rect.top() + i32::try_from(rect.height).unwrap() / 2,
    );
    assert_eq!(
        manager_tool_at(&browser, &theme, vp, centre, MANAGER_TOOLS, tool_model),
        Some(ManagerTool::NewFolder),
    );

    // The read-only picker (no write tools) never lays a write tool out.
    assert_eq!(
        manager_tool_rect(&browser, &theme, vp, &[], ManagerTool::NewFolder),
        None,
    );
}

#[test]
fn render_manager_tool_at_gates_empty_trash_on_the_model() {
    use crate::chrome::{ManagerTool, ManagerToolModel, MANAGER_TOOLS};
    use crate::render::{manager_tool_at, toolbar_height};
    use tairix_geometry::Point;

    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 400, 200);
    let browser = Browser::open_root(MockFs::fixture()).expect("root");
    let y = i32::try_from(toolbar_height(&theme) / 2).unwrap();

    // With the model reporting the current directory is *not* a populated
    // Trash, the Empty Trash tool is drawn (its rect exists) but renders
    // disabled, so a click on it resolves to nothing — fail closed. The
    // always-enabled New Folder tool still resolves.
    let disabled = ManagerToolModel::new(false);
    let mut saw_new_folder = false;
    for x in 0..vp.width {
        let point = Point::new(i32::try_from(x).unwrap(), y);
        let tool = manager_tool_at(&browser, &theme, vp, point, MANAGER_TOOLS, disabled);
        assert_ne!(
            tool,
            Some(ManagerTool::EmptyTrash),
            "a disabled Empty Trash tool must never resolve to an action"
        );
        if tool == Some(ManagerTool::NewFolder) {
            saw_new_folder = true;
        }
    }
    assert!(saw_new_folder, "New Folder stays actionable");

    // With the model reporting a populated Trash, the same pixels resolve the
    // Empty Trash tool — it is enabled in exactly the same place it was drawn.
    let enabled = ManagerToolModel::new(true);
    let mut saw_empty_trash = false;
    for x in 0..vp.width {
        let point = Point::new(i32::try_from(x).unwrap(), y);
        if manager_tool_at(&browser, &theme, vp, point, MANAGER_TOOLS, enabled)
            == Some(ManagerTool::EmptyTrash)
        {
            saw_empty_trash = true;
        }
    }
    assert!(
        saw_empty_trash,
        "an enabled Empty Trash tool is hit-testable"
    );
}

#[test]
fn browser_navigate_to_jumps_to_an_off_spine_location_and_records_history() {
    // Start at `/System`; `/Users` is neither an ancestor nor a child of it,
    // so only the jump-to-arbitrary-location primitive can reach it.
    let start = crate::vfs::components_from_absolute_path("/System").expect("valid path");
    let mut browser = Browser::open_at(MockFs::fixture(), start).expect("System lists");
    assert_eq!(browser.path(), "/System");
    assert!(!browser.can_go_back());

    let moved = browser
        .navigate_to(vec!["Users".to_string()])
        .expect("Users lists");
    assert!(moved);
    assert_eq!(browser.path(), "/Users");
    assert_eq!(names(&browser), ["alice"]);
    // History records the move like any navigation, so Back returns to where
    // the jump started.
    assert!(browser.can_go_back());
    assert!(browser.go_back().expect("back to System"));
    assert_eq!(browser.path(), "/System");
}

#[test]
fn browser_navigate_to_the_current_directory_is_a_no_op() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // Navigating to the directory already shown changes nothing and records no
    // history — a no-op, not an error.
    let moved = browser.navigate_to(Vec::new()).expect("no-op");
    assert!(!moved);
    assert!(!browser.can_go_back());
    assert_eq!(browser.path(), "/");
}

#[test]
fn browser_navigate_to_an_unlistable_location_fails_closed() {
    let mut browser = Browser::open_root(MockFs::fixture()).expect("root");
    // `/System/Security` exists but is capability-denied; the jump is refused
    // and the browser stays exactly where it was, with no history recorded.
    let target = crate::vfs::components_from_absolute_path("/System/Security").expect("valid");
    let result = browser.navigate_to(target);
    assert_eq!(
        result.err(),
        Some(BrowseError::Source(Errno::PermissionDenied))
    );
    assert_eq!(browser.path(), "/");
    assert!(!browser.can_go_back());
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

// --- FM8b: the drawn permission (mode) control ----------------------------

use crate::render::{
    draw_properties_editable, permission_cell_at, permission_cells, permission_toggle_cells,
    PERMISSION_BITS,
};
use tairix_geometry::Point;

#[test]
fn permission_toggle_cells_are_pairwise_non_overlapping() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 480);
    let cells = permission_toggle_cells(vp, font, &theme).expect("grid fits the default popup");

    // Every checkbox is a legible, non-degenerate target.
    for cell in &cells {
        assert!(
            cell.width >= 4 && cell.height >= 4,
            "checkbox too small to hit"
        );
    }

    // No two checkboxes overlap — the defect this grid replaces crammed nine
    // boxes one glyph apart, so they piled on top of one another. Half-open
    // rectangles are disjoint when one ends at or before the other begins on
    // either axis.
    for (i, a) in cells.iter().enumerate() {
        for b in &cells[i + 1..] {
            let disjoint_x = a.left() + i32::try_from(a.width).unwrap() <= b.left()
                || b.left() + i32::try_from(b.width).unwrap() <= a.left();
            let disjoint_y = a.top() + i32::try_from(a.height).unwrap() <= b.top()
                || b.top() + i32::try_from(b.height).unwrap() <= a.top();
            assert!(disjoint_x || disjoint_y, "permission checkboxes overlap");
        }
    }
}

#[test]
fn permission_bits_are_the_nine_settable_rwx_bits_row_major() {
    // Owner, group, other triads, each read/write/execute — the familiar
    // `rwx` set, and their union is exactly the low nine bits (0o777).
    assert_eq!(
        PERMISSION_BITS,
        [0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001]
    );
    let union = PERMISSION_BITS.iter().fold(0u32, |acc, &b| acc | b);
    assert_eq!(union, 0o777);
    // All nine bits are distinct.
    let set: BTreeSet<u32> = PERMISSION_BITS.iter().copied().collect();
    assert_eq!(set.len(), PERMISSION_BITS.len());
}

#[test]
fn permission_cells_report_exactly_the_set_rwx_bits() {
    // A clear mode shows no cell; a full 0o777 shows all nine.
    assert_eq!(permission_cells(0o000), [false; 9]);
    assert_eq!(permission_cells(0o777), [true; 9]);
    // 0o644 = owner rw-, group r--, other r--.
    assert_eq!(
        permission_cells(0o644),
        [true, true, false, true, false, false, true, false, false]
    );
    // 0o755 = owner rwx, group r-x, other r-x.
    assert_eq!(
        permission_cells(0o755),
        [true, true, true, true, false, true, true, false, true]
    );
    // The setuid/setgid/sticky and file-type bits are not part of the control:
    // 0o4755 reads the same nine cells as 0o755.
    assert_eq!(permission_cells(0o4755), permission_cells(0o755));
    assert_eq!(permission_cells(0o170_755), permission_cells(0o755));
}

#[test]
fn draw_properties_editable_paints_the_toggles_without_panicking() {
    use tairix_raster::Surface;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 480);
    let props = Properties::from_stat(
        "notes.txt",
        crate::entry::EntryKind::File,
        &props_stat(FileKind::Regular, 0o644),
    );
    let mut surface = Surface::new(vp.width, vp.height).expect("surface");
    let before = surface.pixels().to_vec();
    draw_properties_editable(&mut surface, &props, &theme, font, vp);
    assert_ne!(surface.pixels().to_vec(), before);

    // A degenerate viewport draws nothing and does not panic.
    let mut tiny = Surface::new(2, 2).expect("tiny surface");
    draw_properties_editable(&mut tiny, &props, &theme, font, Rect::new(0, 0, 2, 2));
}

#[test]
fn permission_cell_at_mirrors_every_checkbox_and_fails_closed_off_grid() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    // The editable Properties popup is tall enough to hold the metadata fields
    // plus the labelled permissions grid at the default window height.
    let vp = Rect::new(0, 0, 480, 480);

    // Scanning the whole window, every bit the hit-test resolves is one of the
    // nine settable bits, and all nine are reachable — so the drawn grid and
    // the hit-test cover exactly the same nine distinct checkboxes (§2.2).
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut y = 0;
    while y < i32::try_from(vp.height).unwrap() {
        let mut x = 0;
        while x < i32::try_from(vp.width).unwrap() {
            if let Some(bit) = permission_cell_at(vp, font, &theme, Point::new(x, y)) {
                assert!(PERMISSION_BITS.contains(&bit), "resolved a non-grid bit");
                seen.insert(bit);
            }
            x += 1;
        }
        y += 1;
    }
    let expected: BTreeSet<u32> = PERMISSION_BITS.iter().copied().collect();
    assert_eq!(seen, expected);

    // A click well outside the panel resolves nothing (fail closed).
    assert_eq!(
        permission_cell_at(vp, font, &theme, Point::new(-5, -5)),
        None
    );
    // On a window too small for the grid, no cell resolves (fail closed).
    let tiny = Rect::new(0, 0, 20, 16);
    assert_eq!(
        permission_cell_at(tiny, font, &theme, Point::new(5, 5)),
        None
    );
}

// --- FM8b: the drawn ownership control ------------------------------------

use crate::render::{draw_owner_control, owner_field_at, OwnerField};
use tairix_controls::text::TextField;

#[test]
fn owner_field_at_mirrors_the_two_value_cells_and_fails_closed_off_grid() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);
    let props = Properties::from_stat(
        "notes.txt",
        crate::entry::EntryKind::File,
        &props_stat(FileKind::Regular, 0o644),
    );

    // Scanning the whole window, every field the hit-test resolves is one of
    // the two owner values, and both are reachable — so the drawn underlines
    // and the hit-test cover exactly the same two distinct cells (§2.2).
    let mut seen: BTreeSet<OwnerField> = BTreeSet::new();
    let mut y = 0;
    while y < i32::try_from(vp.height).unwrap() {
        let mut x = 0;
        while x < i32::try_from(vp.width).unwrap() {
            if let Some(field) = owner_field_at(&props, vp, font, &theme, Point::new(x, y)) {
                seen.insert(field);
            }
            x += 1;
        }
        y += 1;
    }
    assert_eq!(
        seen,
        [OwnerField::Uid, OwnerField::Gid].into_iter().collect()
    );

    // A click well outside the panel resolves nothing (fail closed).
    assert_eq!(
        owner_field_at(&props, vp, font, &theme, Point::new(-5, -5)),
        None
    );
    // On a window too small for the owner row, no field resolves (fail closed).
    let tiny = Rect::new(0, 0, 20, 16);
    assert_eq!(
        owner_field_at(&props, tiny, font, &theme, Point::new(5, 5)),
        None
    );
}

#[test]
fn draw_owner_control_paints_the_affordances_and_editor_without_panicking() {
    use tairix_raster::Surface;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);
    let props = Properties::from_stat(
        "notes.txt",
        crate::entry::EntryKind::File,
        &props_stat(FileKind::Regular, 0o644),
    );

    // With no active editor the underlines mark both values as editable.
    let mut surface = Surface::new(vp.width, vp.height).expect("surface");
    let before = surface.pixels().to_vec();
    draw_owner_control(&mut surface, &props, &theme, font, vp, None);
    assert_ne!(surface.pixels().to_vec(), before);

    // With the uid field being edited the active field renders over its value.
    let editor = TextField::new().with_text("1000");
    let mut edited = Surface::new(vp.width, vp.height).expect("surface");
    let before_edit = edited.pixels().to_vec();
    draw_owner_control(
        &mut edited,
        &props,
        &theme,
        font,
        vp,
        Some((OwnerField::Uid, &editor)),
    );
    assert_ne!(edited.pixels().to_vec(), before_edit);

    // A degenerate viewport draws nothing and does not panic.
    let mut tiny = Surface::new(2, 2).expect("tiny surface");
    draw_owner_control(&mut tiny, &props, &theme, font, Rect::new(0, 0, 2, 2), None);
}

// --- FM7b: the delete-confirmation dialog ---------------------------------

use crate::render::{
    build_delete_dialog, delete_dialog_action_at, delete_dialog_rect, draw_delete_dialog,
    DELETE_CANCEL_INDEX, DELETE_CONFIRM_INDEX,
};
use crate::trash::DeleteDisposition;

/// A plan removing a single regular file.
fn one_file_plan() -> DeletePlan {
    DeletePlan::new(vec![(comps(&["System", "Kernel"]), false)]).expect("a plan")
}

/// A plan removing two items, one of them a directory.
fn folder_plan() -> DeletePlan {
    DeletePlan::new(vec![
        (comps(&["System", "Fonts"]), true),
        (comps(&["System", "Kernel"]), false),
    ])
    .expect("a plan")
}

#[test]
fn delete_dialog_titles_a_single_target_by_its_name() {
    let dialog = build_delete_dialog(&one_file_plan(), DeleteDisposition::Permanent);
    // A single target is named, so the user sees exactly what they are about
    // to remove.
    assert!(dialog.title().contains("Kernel"));
    // A files-only removal warns only that it cannot be undone.
    let message = dialog.message().expect("a message");
    assert!(message.contains("cannot be undone"));
    assert!(!message.to_ascii_lowercase().contains("folder"));
}

#[test]
fn delete_dialog_reports_the_honest_count_and_folder_warning() {
    let dialog = build_delete_dialog(&folder_plan(), DeleteDisposition::Permanent);
    // More than one target: the honest count, not a single name (§2.24).
    assert!(dialog.title().contains('2'));
    // A plan that includes a directory warns that folders (and their contents)
    // are removed, so the confirmation is not misleading (§2.24).
    let message = dialog.message().expect("a message");
    assert!(message.to_ascii_lowercase().contains("folder"));
}

#[test]
fn delete_dialog_offers_a_destructive_delete_and_a_recommended_cancel() {
    use tairix_controls::state::ControlRole;
    let dialog = build_delete_dialog(&one_file_plan(), DeleteDisposition::Permanent);
    let actions = dialog.actions();
    assert_eq!(actions.len(), 2);
    // The honest warmth is on the safe Cancel, never on the destructive Delete
    // (§2.24): the delete carries the Destructive role, Cancel the Recommended.
    assert_eq!(
        actions[DELETE_CONFIRM_INDEX].role(),
        ControlRole::Destructive
    );
    assert_eq!(
        actions[DELETE_CANCEL_INDEX].role(),
        ControlRole::Recommended
    );
}

#[test]
fn delete_dialog_rect_is_centered_and_clamped_within_the_viewport() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();

    let vp = Rect::new(0, 0, 480, 320);
    let rect = delete_dialog_rect(vp, font, &theme);
    assert!(rect.width > 0 && rect.height > 0);
    assert!(rect.origin.x >= 0 && rect.origin.y >= 0);
    assert!(rect.origin.x + i32::try_from(rect.width).unwrap() <= i32::try_from(vp.width).unwrap());
    assert!(
        rect.origin.y + i32::try_from(rect.height).unwrap() <= i32::try_from(vp.height).unwrap()
    );
    // Centered on each axis (within one pixel of the integer split).
    let right_margin =
        i32::try_from(vp.width).unwrap() - (rect.origin.x + i32::try_from(rect.width).unwrap());
    assert!((rect.origin.x - right_margin).abs() <= 1);

    // A window smaller than the dialog would like still yields a drawable rect
    // clamped to the window, never a zero or over-size rectangle (no panic).
    let tiny = Rect::new(0, 0, 20, 16);
    let small = delete_dialog_rect(tiny, font, &theme);
    assert!(small.width >= 1 && small.width <= tiny.width);
    assert!(small.height >= 1 && small.height <= tiny.height);
}

#[test]
fn draw_delete_dialog_paints_into_the_surface_without_panicking() {
    use tairix_raster::Surface;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);
    let dialog = build_delete_dialog(&folder_plan(), DeleteDisposition::Permanent);

    let mut surface = Surface::new(vp.width, vp.height).expect("surface");
    let before = surface.pixels().to_vec();
    draw_delete_dialog(&mut surface, &dialog, &theme, font, vp);
    assert_ne!(surface.pixels().to_vec(), before);

    // A degenerate viewport draws nothing and does not panic.
    let mut tiny = Surface::new(2, 2).expect("tiny surface");
    draw_delete_dialog(&mut tiny, &dialog, &theme, font, Rect::new(0, 0, 2, 2));
}

#[test]
fn delete_dialog_action_at_mirrors_both_buttons_and_fails_closed_off_grid() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);
    let dialog = build_delete_dialog(&folder_plan(), DeleteDisposition::Permanent);

    // Scanning the whole window, every index the hit-test resolves is one of
    // the two action buttons, and both are reachable — so the drawn buttons
    // and the hit-test cover exactly the same two distinct actions (§2.2).
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut y = 0;
    while y < i32::try_from(vp.height).unwrap() {
        let mut x = 0;
        while x < i32::try_from(vp.width).unwrap() {
            if let Some(index) =
                delete_dialog_action_at(&dialog, vp, font, &theme, Point::new(x, y))
            {
                assert!(
                    index == DELETE_CONFIRM_INDEX || index == DELETE_CANCEL_INDEX,
                    "resolved a non-action index"
                );
                seen.insert(index);
            }
            x += 1;
        }
        y += 1;
    }
    assert_eq!(
        seen,
        [DELETE_CONFIRM_INDEX, DELETE_CANCEL_INDEX]
            .into_iter()
            .collect()
    );

    // A click well outside the dialog resolves nothing (fail closed).
    assert_eq!(
        delete_dialog_action_at(&dialog, vp, font, &theme, Point::new(-5, -5)),
        None
    );
    // On a window too small for the dialog buttons, nothing resolves (fail
    // closed) rather than placing a phantom button.
    let tiny = Rect::new(0, 0, 20, 16);
    assert_eq!(
        delete_dialog_action_at(&dialog, tiny, font, &theme, Point::new(5, 5)),
        None
    );
}

// --- FM10: the move-to-Trash confirmation wording -------------------------

#[test]
fn trash_dialog_is_recoverable_and_worded_honestly() {
    use tairix_controls::state::ControlRole;
    let dialog = build_delete_dialog(&one_file_plan(), DeleteDisposition::Trash);
    // The recoverable move names the item and says "Trash", never "delete" or
    // "cannot be undone" (§2.24 — the wording matches what will happen).
    assert!(dialog.title().contains("Kernel"));
    assert!(dialog.title().contains("Trash"));
    let message = dialog.message().expect("a message");
    assert!(message.contains("restore"));
    assert!(!message.to_ascii_lowercase().contains("cannot be undone"));
    // A recoverable move is not destructive: the confirm action is the
    // recommended (safe) primary, and Cancel carries no honest-warmth role.
    let actions = dialog.actions();
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[DELETE_CONFIRM_INDEX].role(),
        ControlRole::Recommended
    );
    assert_eq!(actions[DELETE_CANCEL_INDEX].role(), ControlRole::Neutral);
}

#[test]
fn permanent_dialog_names_the_irreversible_delete() {
    // The permanent wording is explicit that the removal is forever, so it can
    // never be mistaken for the recoverable Trash move (§2.24).
    let dialog = build_delete_dialog(&one_file_plan(), DeleteDisposition::Permanent);
    assert!(dialog.title().to_ascii_lowercase().contains("permanently"));
}

// --- FM10: the shared Trash-directory location ----------------------------

#[test]
fn trash_dir_is_the_library_trash_subtree_of_home() {
    use crate::trash::{trash_dir, TRASH_LEAF_DIR, TRASH_LIBRARY_DIR};
    let home = comps(&["Users", "root"]);
    assert_eq!(
        trash_dir(&home),
        comps(&["Users", "root", TRASH_LIBRARY_DIR, TRASH_LEAF_DIR])
    );
    // It reads only `home` — nothing is fabricated for an empty home (that
    // parses to no components, the root, upstream), and the leaves are the
    // shared constants, so the location cannot drift from the app's (§2.2).
    assert_eq!(trash_dir(&[]), comps(&["Library", "Trash"]));
}

// --- FM7b: the long-operation progress + cancel surface -------------------

use crate::progress::{ProgressModel, ProgressOp};
use crate::render::{
    build_progress_cancel, draw_progress_dialog, progress_cancel_at, progress_dialog_rect,
};

#[test]
fn progress_model_reports_the_honest_count_and_never_a_percentage() {
    let mut model = ProgressModel::new(ProgressOp::Delete);
    assert_eq!(model.done(), 0);
    // Zero and plural counts read naturally; the verb matches the operation.
    assert_eq!(model.status_line(), "0 items removed");
    model.set_done(1);
    assert_eq!(model.status_line(), "1 item removed");
    model.set_done(42);
    assert_eq!(model.status_line(), "42 items removed");
    // No fabricated percentage anywhere in the caption (§2.24).
    assert!(!model.status_line().contains('%'));

    // A copy model reads with the copy verb.
    let mut copy = ProgressModel::new(ProgressOp::Copy);
    copy.set_done(3);
    assert_eq!(copy.status_line(), "3 items copied");
    assert!(copy.title().starts_with("Copying"));
    assert!(model.title().starts_with("Deleting"));

    // A move-to-Trash model reads with the Trash verb (`plans/NEW-FILEMANAGER.md`
    // FM10): an honest recoverable-move caption, never "removed".
    let mut trash = ProgressModel::new(ProgressOp::Trash);
    trash.set_done(1);
    assert_eq!(trash.status_line(), "1 item moved to Trash");
    assert!(trash.title().starts_with("Moving to Trash"));
}

#[test]
fn progress_cancel_is_latched_and_shown() {
    let mut model = ProgressModel::new(ProgressOp::Delete);
    assert!(!model.is_cancel_requested());
    model.request_cancel();
    assert!(model.is_cancel_requested());
    // The title reflects the pending cancel while the current step finishes.
    assert!(model.title().starts_with("Cancelling"));
    // The latch cannot be reverted by a second request.
    model.request_cancel();
    assert!(model.is_cancel_requested());
    // The Cancel button reads differently once cancel is latched (disabled), so
    // a second press cannot re-request what is already stopping.
    let running = ProgressModel::new(ProgressOp::Delete);
    assert_ne!(
        build_progress_cancel(&model).state(),
        build_progress_cancel(&running).state()
    );
}

#[test]
fn progress_dialog_rect_is_centered_and_clamped_within_the_viewport() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();

    let vp = Rect::new(0, 0, 480, 320);
    let rect = progress_dialog_rect(vp, font, &theme);
    assert!(rect.width > 0 && rect.height > 0);
    assert!(rect.origin.x >= 0 && rect.origin.y >= 0);
    assert!(rect.origin.x + i32::try_from(rect.width).unwrap() <= i32::try_from(vp.width).unwrap());
    assert!(
        rect.origin.y + i32::try_from(rect.height).unwrap() <= i32::try_from(vp.height).unwrap()
    );
    // A window smaller than the panel still yields a drawable clamped rect (no
    // panic).
    let tiny = Rect::new(0, 0, 20, 16);
    let small = progress_dialog_rect(tiny, font, &theme);
    assert!(small.width >= 1 && small.width <= tiny.width);
    assert!(small.height >= 1 && small.height <= tiny.height);
}

#[test]
fn draw_progress_dialog_paints_into_the_surface_without_panicking() {
    use tairix_raster::Surface;

    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);
    let mut model = ProgressModel::new(ProgressOp::Copy);
    model.set_done(7);

    let mut surface = Surface::new(vp.width, vp.height).expect("surface");
    let before = surface.pixels().to_vec();
    draw_progress_dialog(&mut surface, &model, &theme, font, vp);
    assert_ne!(surface.pixels().to_vec(), before);

    // A degenerate viewport draws nothing and does not panic.
    let mut tiny = Surface::new(2, 2).expect("tiny surface");
    draw_progress_dialog(&mut tiny, &model, &theme, font, Rect::new(0, 0, 2, 2));
}

#[test]
fn progress_cancel_at_mirrors_the_cancel_button_and_fails_closed_off_grid() {
    let font = tairix_font::BitmapFont::inconsolata();
    let theme = Theme::dark();
    let vp = Rect::new(0, 0, 480, 320);

    // Scanning the whole window, the Cancel hit-test resolves true for a
    // contiguous, reachable region and false everywhere else — so the drawn
    // button and the hit-test agree on exactly one target (§2.2).
    let mut hits = 0u32;
    let mut y = 0;
    while y < i32::try_from(vp.height).unwrap() {
        let mut x = 0;
        while x < i32::try_from(vp.width).unwrap() {
            if progress_cancel_at(vp, font, &theme, Point::new(x, y)) {
                hits += 1;
            }
            x += 1;
        }
        y += 1;
    }
    assert!(hits > 0, "the Cancel button is reachable");

    // A click well outside the panel resolves nothing (fail closed).
    assert!(!progress_cancel_at(vp, font, &theme, Point::new(-5, -5)));
    // On a window too small to place the button, nothing resolves (fail
    // closed) rather than placing a phantom button.
    let tiny = Rect::new(0, 0, 20, 16);
    assert!(!progress_cancel_at(tiny, font, &theme, Point::new(5, 5)));
}

mod trash {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::execute::VolumeId;
    use crate::trash::{
        empty_trash_plan, trash_dest_path, trash_strategy, TrashError, TrashStrategy,
    };

    fn vol(byte: u8) -> VolumeId {
        VolumeId::new([byte; 16])
    }

    fn owned(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| String::from(*p)).collect()
    }

    fn child(name: &str, is_dir: bool) -> (String, bool) {
        (String::from(name), is_dir)
    }

    #[test]
    fn same_volume_moves_and_cross_volume_unlinks() {
        // An item on the same volume as Trash is a cheap recoverable rename;
        // a different volume falls back to the irreversible unlink.
        assert_eq!(trash_strategy(vol(7), vol(7)), TrashStrategy::Move);
        assert_eq!(trash_strategy(vol(1), vol(2)), TrashStrategy::Unlink);
    }

    #[test]
    fn a_free_name_lands_unchanged_under_the_trash_dir() {
        let trash = owned(&["Users", "root", "Library", "Trash"]);
        let dest = trash_dest_path(&trash, "notes.txt", &[]).expect("free name");
        assert_eq!(
            dest,
            owned(&["Users", "root", "Library", "Trash", "notes.txt"])
        );
    }

    #[test]
    fn a_clashing_name_disambiguates_before_the_extension() {
        let trash = owned(&["Trash"]);
        let taken = owned(&["notes.txt"]);
        let dest = trash_dest_path(&trash, "notes.txt", &taken).expect("disambiguated");
        assert_eq!(dest.last().map(String::as_str), Some("notes (2).txt"));
    }

    #[test]
    fn disambiguation_skips_every_taken_suffix_in_order() {
        let trash = owned(&["Trash"]);
        let taken = owned(&["notes.txt", "notes (2).txt", "notes (3).txt"]);
        let dest = trash_dest_path(&trash, "notes.txt", &taken).expect("disambiguated");
        assert_eq!(dest.last().map(String::as_str), Some("notes (4).txt"));
    }

    #[test]
    fn a_name_with_no_extension_disambiguates_as_a_whole() {
        let trash = owned(&["Trash"]);
        let taken = owned(&["report"]);
        let dest = trash_dest_path(&trash, "report", &taken).expect("disambiguated");
        assert_eq!(dest.last().map(String::as_str), Some("report (2)"));
    }

    #[test]
    fn a_dotfile_disambiguates_after_the_whole_name() {
        // A leading-dot name has no extension to split on, so the suffix lands
        // after the whole name rather than before the leading dot.
        let trash = owned(&["Trash"]);
        let taken = owned(&[".profile"]);
        let dest = trash_dest_path(&trash, ".profile", &taken).expect("disambiguated");
        assert_eq!(dest.last().map(String::as_str), Some(".profile (2)"));
    }

    #[test]
    fn a_root_trash_dir_is_refused() {
        assert_eq!(
            trash_dest_path(&[], "notes.txt", &[]),
            Err(TrashError::RootTrash)
        );
    }

    #[test]
    fn an_invalid_original_name_is_refused() {
        let trash = owned(&["Trash"]);
        for bad in ["", ".", "..", "a/b", "a:b"] {
            assert_eq!(
                trash_dest_path(&trash, bad, &[]),
                Err(TrashError::InvalidName),
                "name {bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_disambiguation_past_the_name_limit_is_refused() {
        // The original leaf is exactly at the 255-byte per-name limit (a valid
        // name), but a forced " (2)" disambiguation would push it over: refused
        // (TooLong), never truncated to a name that could collide.
        let trash = owned(&["Trash"]);
        let leaf = "a".repeat(255); // exactly the per-name limit: a valid leaf.
        let taken = vec![leaf.clone()];
        assert_eq!(
            trash_dest_path(&trash, &leaf, &taken),
            Err(TrashError::TooLong)
        );
    }

    // --- FM11: emptying the Trash --------------------------------------------

    #[test]
    fn emptying_removes_every_child_under_the_trash_dir_not_the_dir_itself() {
        let trash = owned(&["Users", "root", "Library", "Trash"]);
        let children = vec![child("notes.txt", false), child("old_project", true)];
        let plan = empty_trash_plan(&trash, &children)
            .expect("a valid listing")
            .expect("a non-empty Trash yields a plan");

        // One target per child, in listing order — the Trash directory itself is
        // never a target, so emptying leaves the (now-empty) folder in place.
        assert_eq!(plan.len(), 2);
        let targets = plan.targets();
        assert_eq!(
            targets[0].path(),
            owned(&["Users", "root", "Library", "Trash", "notes.txt"]).as_slice()
        );
        assert!(!targets[0].is_directory());
        assert_eq!(
            targets[1].path(),
            owned(&["Users", "root", "Library", "Trash", "old_project"]).as_slice()
        );
        assert!(targets[1].is_directory());
        // A directory-backed child means the recursive DeleteWalk is exercised.
        assert!(plan.has_directories());
    }

    #[test]
    fn emptying_an_already_empty_trash_is_a_no_op_not_an_error() {
        let trash = owned(&["Users", "root", "Library", "Trash"]);
        assert_eq!(empty_trash_plan(&trash, &[]), Ok(None));
    }

    #[test]
    fn emptying_a_root_trash_dir_is_refused() {
        // An empty Trash path would spell each child as a top-level root entry:
        // refused rather than risk removing outside Trash.
        let children = vec![child("notes.txt", false)];
        assert_eq!(empty_trash_plan(&[], &children), Err(TrashError::RootTrash));
    }

    #[test]
    fn an_invalid_child_name_refuses_the_whole_empty() {
        let trash = owned(&["Trash"]);
        for bad in ["", ".", "..", "a/b", "a:b"] {
            let children = vec![child("safe.txt", false), child(bad, false)];
            assert_eq!(
                empty_trash_plan(&trash, &children),
                Err(TrashError::InvalidName),
                "a child named {bad:?} must refuse the whole empty"
            );
        }
    }
}
