//! Unit tests for the desktop's icon column ([`crate::desktop`]).
//!
//! Kept beside the module in its own file because `desktop.rs` is already
//! past the length at which a `#[cfg(test)]` block belongs in a sibling.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::{Errno, Time64};
use tairix_browse::{AppAssociation, DirectorySource, Entry, EntryKind, GridView};
use tairix_controls::{ActivityState, MenuItem};
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::NoArtwork;
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_wallpaper::{Backdrop, IconFlow, IconSort, PinboardSettings, Rgb, WallpaperChoice};
use tairix_wm::{InputEvent, Key, NamedKey, PointerButton};

use crate::desktop::{
    Desktop, DesktopAction, DesktopActivation, DesktopOutcome, PinboardChange, DESKTOP_MARGIN,
    RELIST_MIN_INTERVAL_NS,
};
use crate::pinboard::{PinboardCommand, PinboardMenu, PinboardMenuOutcome};

/// What a [`FakeDir`] answers with, and how often it has been asked.
///
/// `answer` is what the folder holds; `Err` models a folder the source
/// refuses (no permission, not there). Shared with the test so the folder
/// can change under a live desktop without the desktop having to hand its
/// source back out.
#[derive(Default)]
struct Folder {
    answer: Option<Result<Vec<Entry>, Errno>>,
    listings: usize,
}

/// A directory seam over a shared [`Folder`].
struct FakeDir(Rc<RefCell<Folder>>);

impl DirectorySource for FakeDir {
    fn list(&mut self, _components: &[String]) -> Result<Vec<Entry>, Errno> {
        let mut folder = self.0.borrow_mut();
        folder.listings = folder.listings.saturating_add(1);
        folder
            .answer
            .clone()
            .unwrap_or(Err(Errno::PermissionDenied))
    }
}

/// A shared folder holding `entries`.
fn holding(entries: Vec<Entry>) -> Rc<RefCell<Folder>> {
    Rc::new(RefCell::new(Folder {
        answer: Some(Ok(entries)),
        listings: 0,
    }))
}

/// How many times the folder has been listed.
fn listings(folder: &Rc<RefCell<Folder>>) -> usize {
    folder.borrow().listings
}

fn file(name: &str) -> Entry {
    Entry::new(name, EntryKind::File, 1, Time64::UNIX_EPOCH)
}

fn folder(name: &str) -> Entry {
    Entry::new(name, EntryKind::Directory, 0, Time64::UNIX_EPOCH)
}

fn bundle(name: &str) -> Entry {
    Entry::new(name, EntryKind::Bundle, 0, Time64::UNIX_EPOCH)
}

/// The user's desktop folder, root-first.
fn home() -> Vec<String> {
    vec![
        "Users".to_string(),
        "ada".to_string(),
        "Desktop".to_string(),
    ]
}

/// A desktop over `folder`, already listed once.
fn desktop_over(folder: &Rc<RefCell<Folder>>) -> Desktop<FakeDir> {
    let mut desktop = Desktop::new(FakeDir(Rc::clone(folder)), home());
    desktop.relist(0);
    desktop
}

/// A desktop over `entries`, already listed once.
fn desktop_of(entries: Vec<Entry>) -> Desktop<FakeDir> {
    desktop_over(&holding(entries))
}

/// The default settings with the icon arrangement and sort order replaced.
fn arranged_by(icons: IconFlow, sort: IconSort) -> PinboardSettings {
    PinboardSettings {
        icons,
        sort,
        ..PinboardSettings::default()
    }
}

/// A desktop over `entries` under `settings`, already listed once.
fn desktop_with(entries: Vec<Entry>, settings: PinboardSettings) -> Desktop<FakeDir> {
    let mut desktop = Desktop::new(FakeDir(holding(entries)), home());
    let _ = desktop.apply_settings(settings);
    desktop.relist(0);
    desktop
}

fn theme() -> Theme {
    Theme::dark()
}

/// A work area the size of a modest screen with room for several columns.
fn work_area() -> Rect {
    Rect::new(0, 0, 800, 600)
}

fn layout_of(desktop: &Desktop<FakeDir>) -> GridView {
    desktop.layout(work_area(), Scale::ONE, &theme())
}

/// The centre of the icon at `index`, in screen coordinates.
fn centre_of(layout: &GridView, index: usize) -> Point {
    let cell = layout.cell_rect(0, index).expect("a shown icon");
    Point::new(
        cell.left() + i32::try_from(cell.width / 2).unwrap_or(0),
        cell.top() + i32::try_from(cell.height / 2).unwrap_or(0),
    )
}

/// A point clear of every icon: inside the work area's margin, which no icon
/// reaches under either arrangement.
const EMPTY_DESKTOP: Point = Point::new(2, 2);

/// One "text/plain opens in the editor" association.
fn editor() -> Vec<AppAssociation> {
    vec![AppAssociation::new(
        "Edit",
        "/Apps/Edit.app",
        vec!["text/plain".to_string()],
    )]
}

// --- Listing --------------------------------------------------------------

#[test]
fn the_listing_is_the_shared_sort_order_of_the_folder() {
    let desktop = desktop_of(vec![file("zeta.txt"), folder("Work"), file("alpha.txt")]);
    let names: Vec<&str> = desktop.entries().iter().map(Entry::name).collect();
    // Directories first, then names in the shared order — the desktop adds
    // no ordering of its own.
    assert_eq!(names, vec!["Work", "alpha.txt", "zeta.txt"]);
}

#[test]
fn a_folder_that_will_not_list_shows_nothing_and_never_panics() {
    let mut desktop = Desktop::new(FakeDir(Rc::new(RefCell::new(Folder::default()))), home());
    assert!(
        !desktop.relist(0),
        "an empty listing is no change from empty"
    );
    assert!(desktop.entries().is_empty());
    assert_eq!(desktop.selected(), None);
}

#[test]
fn a_relist_keeps_the_selection_on_the_same_named_icon() {
    let folder = holding(vec![file("b.txt"), file("c.txt")]);
    let mut desktop = desktop_over(&folder);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 1), &layout, 0, &[]);
    assert_eq!(desktop.entries()[1].name(), "c.txt");

    // A file appears ahead of it: the selection follows the name, not the
    // index, so the user's selection cannot silently jump to another icon.
    folder.borrow_mut().answer = Some(Ok(vec![file("a.txt"), file("b.txt"), file("c.txt")]));
    assert!(desktop.relist(1));
    assert_eq!(desktop.selected(), Some(2));
    assert_eq!(desktop.entries()[2].name(), "c.txt");
}

#[test]
fn a_relist_that_removes_the_selected_icon_selects_nothing() {
    let folder = holding(vec![file("a.txt"), file("b.txt")]);
    let mut desktop = desktop_over(&folder);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 1), &layout, 0, &[]);
    folder.borrow_mut().answer = Some(Ok(vec![file("a.txt")]));
    assert!(desktop.relist(1));
    assert_eq!(desktop.selected(), None);
}

// --- The gesture-driven, rate-limited re-list -----------------------------

#[test]
fn arriving_on_the_desktop_relists_but_no_more_often_than_the_rate_limit() {
    let folder = holding(vec![file("a.txt")]);
    let mut desktop = desktop_over(&folder);
    let layout = layout_of(&desktop);
    assert_eq!(listings(&folder), 1, "the bring-up listing");

    // Sweeping on and off inside the limit costs no further listing at all.
    for step in 0..5 {
        desktop.pointer_left();
        desktop.pointer_moved(EMPTY_DESKTOP, &layout, step);
    }
    assert_eq!(listings(&folder), 1, "a sweep is not a re-list");

    // Once the limit has passed, the next arrival looks again — exactly once.
    desktop.pointer_left();
    desktop.pointer_moved(EMPTY_DESKTOP, &layout, RELIST_MIN_INTERVAL_NS);
    assert_eq!(listings(&folder), 2);
    desktop.pointer_moved(centre_of(&layout, 0), &layout, RELIST_MIN_INTERVAL_NS + 1);
    assert_eq!(
        listings(&folder),
        2,
        "motion that never left is not an arrival"
    );
}

#[test]
fn a_forced_relist_ignores_the_rate_limit() {
    // The rate limit exists to stop a pointer sweep becoming a stream of
    // directory reads; it never delays a re-list the session asked for
    // because it knows something changed.
    let folder = holding(vec![file("a.txt")]);
    let mut desktop = desktop_over(&folder);
    desktop.relist(1);
    desktop.relist(2);
    assert_eq!(listings(&folder), 3);
}

// --- Hover, selection, focus ---------------------------------------------

#[test]
fn hover_follows_the_pointer_and_redraws_only_when_it_moves_icon() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);

    let first = desktop.pointer_moved(centre_of(&layout, 0), &layout, 0);
    assert!(first.redraw);
    assert_eq!(desktop.hovered(), Some(0));

    let again = desktop.pointer_moved(centre_of(&layout, 0), &layout, 1);
    assert!(!again.redraw, "the same icon is not a change");

    let empty = desktop.pointer_moved(EMPTY_DESKTOP, &layout, 2);
    assert!(empty.redraw);
    assert_eq!(desktop.hovered(), None);
}

#[test]
fn leaving_the_desktop_clears_the_hover() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.pointer_moved(centre_of(&layout, 0), &layout, 0);
    assert!(desktop.pointer_left().redraw);
    assert_eq!(desktop.hovered(), None);
    assert!(
        !desktop.pointer_left().redraw,
        "leaving twice changes nothing"
    );
}

#[test]
fn a_press_selects_an_icon_and_a_press_on_empty_desktop_clears_it() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);

    let picked = desktop.press(centre_of(&layout, 1), &layout, 0, &[]);
    assert!(picked.redraw);
    assert_eq!(desktop.selected(), Some(1));
    assert!(desktop.is_focused(), "a press moves focus to the desktop");

    let cleared = desktop.press(EMPTY_DESKTOP, &layout, 0, &[]);
    assert!(cleared.redraw);
    assert_eq!(desktop.selected(), None);
    assert!(
        !desktop.press(EMPTY_DESKTOP, &layout, 0, &[]).redraw,
        "clearing an empty selection changes nothing"
    );
}

#[test]
fn focus_is_the_embedders_to_set_and_reports_whether_it_moved() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    assert!(!desktop.is_focused());
    assert!(desktop.set_focused(true));
    assert!(!desktop.set_focused(true), "unchanged focus is no redraw");
    assert!(desktop.set_focused(false));
}

// --- Keyboard -------------------------------------------------------------

#[test]
fn the_arrows_move_the_selection_the_way_the_icons_run_under_both_arrangements() {
    // Columns grow rightward from the leading edge and leftward from the
    // trailing one, so which horizontal arrow runs later into the listing is a
    // property of the arrangement, never a constant.
    for (icons, later, earlier) in [
        (IconFlow::Leading, right(), left()),
        (IconFlow::Trailing, left(), right()),
    ] {
        let entries: Vec<Entry> = (0..12)
            .map(|n| file(&alloc::format!("f{n:02}.txt")))
            .collect();
        let mut desktop = desktop_with(entries, arranged_by(icons, IconSort::default()));
        let layout = layout_of(&desktop);
        let per_column = layout.cells_per_line();
        assert!(per_column >= 2, "the fixture needs a multi-icon column");
        desktop.set_focused(true);

        // With nothing selected the first arrow starts at the first icon.
        assert!(desktop.key(down(), true, &layout, &[]).redraw);
        assert_eq!(desktop.selected(), Some(0));

        desktop.key(down(), true, &layout, &[]);
        assert_eq!(desktop.selected(), Some(1));
        desktop.key(up(), true, &layout, &[]);
        assert_eq!(desktop.selected(), Some(0));

        // One whole column further into the listing, and back out again.
        desktop.key(later, true, &layout, &[]);
        assert_eq!(desktop.selected(), Some(per_column), "{icons:?} onward");
        desktop.key(earlier, true, &layout, &[]);
        assert_eq!(desktop.selected(), Some(0), "{icons:?} back");
    }
}

#[test]
fn the_selection_clamps_at_both_ends_of_the_listing() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);
    desktop.set_focused(true);
    desktop.key(down(), true, &layout, &[]);
    assert!(!desktop.key(up(), true, &layout, &[]).redraw);
    assert_eq!(desktop.selected(), Some(0));

    desktop.key(down(), true, &layout, &[]);
    assert_eq!(desktop.selected(), Some(1));
    assert!(!desktop.key(down(), true, &layout, &[]).redraw);
    assert_eq!(desktop.selected(), Some(1), "clamped at the last icon");
}

#[test]
fn keys_do_nothing_while_the_desktop_does_not_hold_the_keyboard() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    assert_eq!(
        desktop.key(down(), true, &layout, &[]),
        DesktopOutcome::ignored()
    );
    assert_eq!(desktop.selected(), None);
}

#[test]
fn a_key_release_and_an_unknown_key_change_nothing() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.set_focused(true);
    assert_eq!(
        desktop.key(down(), false, &layout, &[]),
        DesktopOutcome::ignored()
    );
    assert_eq!(
        desktop.key(Key::Char('x'), true, &layout, &[]),
        DesktopOutcome::ignored()
    );
    assert_eq!(
        desktop.key(Key::Named(NamedKey::Tab), true, &layout, &[]),
        DesktopOutcome::ignored()
    );
}

#[test]
fn escape_clears_the_selection() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 0), &layout, 0, &[]);
    assert!(desktop.key(escape(), true, &layout, &[]).redraw);
    assert_eq!(desktop.selected(), None);
    assert_eq!(
        desktop.key(escape(), true, &layout, &[]),
        DesktopOutcome::ignored(),
        "nothing selected and no offer outstanding"
    );
}

// --- Activation -----------------------------------------------------------

#[test]
fn double_clicking_a_folder_opens_the_file_manager_at_its_path() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[]);
    let acted = desktop.press(at, &layout, 1, &[]);
    assert_eq!(
        acted.action,
        Some(DesktopAction::Activate(DesktopActivation::OpenFolder {
            path: "/Users/ada/Desktop/Work".to_string(),
        }))
    );
}

#[test]
fn double_clicking_an_application_bundle_launches_its_run_binary() {
    let mut desktop = desktop_of(vec![bundle("Chess.app")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[]);
    assert_eq!(
        desktop.press(at, &layout, 1, &[]).action,
        Some(DesktopAction::Activate(DesktopActivation::Launch {
            run_path: "/Users/ada/Desktop/Chess.app/Run".to_string(),
            label: "Chess".to_string(),
            argument: None,
        }))
    );
}

#[test]
fn double_clicking_a_file_launches_its_associated_application_with_the_file() {
    let mut desktop = desktop_of(vec![file("notes.txt")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[]);
    assert_eq!(
        desktop.press(at, &layout, 1, &editor()).action,
        Some(DesktopAction::Activate(DesktopActivation::Launch {
            run_path: "/Apps/Edit.app/Run".to_string(),
            label: "Edit".to_string(),
            argument: Some("/Users/ada/Desktop/notes.txt".to_string()),
        }))
    );
}

#[test]
fn a_file_no_application_opens_is_refused_with_its_reason_and_does_nothing_else() {
    let mut desktop = desktop_of(vec![file("mystery.qqq")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[]);
    let acted = desktop.press(at, &layout, 1, &editor());
    assert_eq!(
        acted.action,
        Some(DesktopAction::Refuse(
            "desktop: no installed application opens 'mystery.qqq'\n".to_string()
        )),
        "the refusal names the file and is ready for the error stream"
    );
    assert_eq!(desktop.selected(), Some(0), "the icon stays selected");
}

#[test]
fn enter_activates_the_selection_and_does_nothing_with_no_selection() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    desktop.set_focused(true);
    assert_eq!(
        desktop.key(enter(), true, &layout, &[]),
        DesktopOutcome::ignored()
    );
    desktop.press(centre_of(&layout, 0), &layout, 0, &[]);
    assert_eq!(
        desktop.key(enter(), true, &layout, &[]).action,
        Some(DesktopAction::Activate(DesktopActivation::OpenFolder {
            path: "/Users/ada/Desktop/Work".to_string(),
        }))
    );
}

#[test]
fn two_slow_clicks_are_two_clicks_not_an_activation() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[]);
    let late = desktop.press(
        at,
        &layout,
        tairix_browse::DOUBLE_CLICK_INTERVAL_NS + 1,
        &[],
    );
    assert_eq!(late.action, None);
}

// --- Painting -------------------------------------------------------------

#[test]
fn every_icon_the_column_shows_is_painted_even_with_no_artwork_at_all() {
    let mut desktop = desktop_of(vec![folder("Work"), file("notes.txt"), bundle("Chess.app")]);
    let layout = layout_of(&desktop);
    let theme = theme();
    let mut surface = Surface::new(800, 600).expect("a screen-sized layer");
    desktop.set_focused(true);
    desktop.press(centre_of(&layout, 0), &layout, 0, &[]);
    desktop.pointer_moved(centre_of(&layout, 1), &layout, 1);

    desktop.render(&mut surface, &layout, Scale::ONE, &theme, &mut NoArtwork);

    // With no artwork store at all every tile still draws its built-in
    // glyph, so no icon slot can come out blank.
    for index in 0..3 {
        let cell = layout.cell_rect(0, index).expect("a shown icon");
        assert!(painted(&surface, cell), "icon {index} drew nothing at all");
    }
}

#[test]
fn an_empty_desktop_paints_nothing_and_leaves_the_wallpaper_showing() {
    let desktop = desktop_of(Vec::new());
    let layout = layout_of(&desktop);
    let theme = theme();
    let mut surface = Surface::new(800, 600).expect("a screen-sized layer");
    desktop.render(&mut surface, &layout, Scale::ONE, &theme, &mut NoArtwork);
    assert!(
        !painted(&surface, work_area()),
        "an empty folder leaves the layer fully transparent"
    );
}

/// Whether anything at all was drawn inside `area` (any non-transparent
/// pixel).
fn painted(surface: &Surface, area: Rect) -> bool {
    let right = u32::try_from(area.right())
        .unwrap_or(0)
        .min(surface.width());
    let bottom = u32::try_from(area.bottom())
        .unwrap_or(0)
        .min(surface.height());
    let left = u32::try_from(area.left()).unwrap_or(0);
    let top = u32::try_from(area.top()).unwrap_or(0);
    for y in top..bottom {
        for x in left..right {
            if surface.get(x, y).is_some_and(|pixel| pixel.a != 0) {
                return true;
            }
        }
    }
    false
}

const fn down() -> Key {
    Key::Named(NamedKey::Down)
}

const fn up() -> Key {
    Key::Named(NamedKey::Up)
}

const fn left() -> Key {
    Key::Named(NamedKey::Left)
}

const fn right() -> Key {
    Key::Named(NamedKey::Right)
}

const fn enter() -> Key {
    Key::Named(NamedKey::Enter)
}

const fn escape() -> Key {
    Key::Named(NamedKey::Escape)
}

// --- The pinboard settings ------------------------------------------------

#[test]
fn each_arrangement_lays_the_column_out_at_its_own_corner_and_hit_tests_there() {
    let area = work_area();
    let margin = i32::try_from(Scale::ONE.scale_length(DESKTOP_MARGIN)).unwrap_or(0);
    let mut cells = Vec::new();
    for icons in [IconFlow::Leading, IconFlow::Trailing] {
        let mut desktop =
            desktop_with(vec![file("a.txt")], arranged_by(icons, IconSort::default()));
        let layout = layout_of(&desktop);
        let cell = layout.cell_rect(0, 0).expect("a shown icon");
        assert_eq!(
            cell.top(),
            area.top() + margin,
            "{icons:?} starts at the top"
        );
        // The icon the user can see is the icon a press lands on, whichever
        // corner the column grew from.
        desktop.press(centre_of(&layout, 0), &layout, 0, &[]);
        assert_eq!(desktop.selected(), Some(0), "{icons:?} hit-test");
        cells.push(cell);
    }
    let (leading, trailing) = (cells[0], cells[1]);
    assert_eq!(
        leading.left(),
        area.left() + margin,
        "the leading column starts at the margin"
    );
    assert!(
        trailing.right() <= area.right() - margin,
        "the trailing column stays inside the margin"
    );
    assert!(
        trailing.left() > leading.left() + i32::try_from(area.width / 2).unwrap_or(0),
        "the two arrangements hug opposite edges"
    );
}

#[test]
fn changing_the_arrangement_relays_the_icons_out_without_relisting() {
    let folder = holding(vec![file("a.txt")]);
    let mut desktop = desktop_over(&folder);
    let before = layout_of(&desktop).cell_rect(0, 0).expect("a shown icon");

    let change = desktop
        .apply_settings(arranged_by(IconFlow::Trailing, IconSort::default()))
        .expect("the arrangement changed");
    assert!(change.relayout);
    assert!(!change.relist && !change.wallpaper);
    assert_eq!(listings(&folder), 1, "an arrangement is not a listing");

    let after = layout_of(&desktop).cell_rect(0, 0).expect("a shown icon");
    assert_ne!(before.left(), after.left(), "the column moved edge");
}

#[test]
fn each_sort_order_lists_the_folder_in_the_shared_order_it_names() {
    let entries = vec![
        Entry::new("b.txt", EntryKind::File, 300, Time64::from_secs(30)),
        Entry::new("a.txt", EntryKind::File, 900, Time64::from_secs(20)),
        Entry::new("c.txt", EntryKind::File, 100, Time64::from_secs(10)),
    ];
    for (sort, expected) in [
        (IconSort::Name, ["a.txt", "b.txt", "c.txt"]),
        (IconSort::Size, ["c.txt", "b.txt", "a.txt"]),
        (IconSort::Date, ["c.txt", "a.txt", "b.txt"]),
    ] {
        let desktop = desktop_with(entries.clone(), arranged_by(IconFlow::default(), sort));
        let names: Vec<&str> = desktop.entries().iter().map(Entry::name).collect();
        assert_eq!(names, expected.to_vec(), "{sort:?} order");
    }
}

#[test]
fn the_kind_order_groups_folders_then_bundles_then_files() {
    let mixed = vec![file("b.txt"), bundle("Chess.app"), folder("Work")];
    let by_name = desktop_of(mixed.clone());
    let names: Vec<&str> = by_name.entries().iter().map(Entry::name).collect();
    assert_eq!(
        names,
        vec!["Work", "b.txt", "Chess.app"],
        "the default order"
    );

    let by_kind = desktop_with(mixed, arranged_by(IconFlow::default(), IconSort::Kind));
    let names: Vec<&str> = by_kind.entries().iter().map(Entry::name).collect();
    assert_eq!(names, vec!["Work", "Chess.app", "b.txt"]);
}

#[test]
fn adopting_settings_reports_only_the_work_the_edit_implies() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    assert_eq!(
        desktop.apply_settings(PinboardSettings::default()),
        None,
        "the settings already in force cost nothing at all"
    );

    let sorted = desktop
        .apply_settings(arranged_by(IconFlow::default(), IconSort::Size))
        .expect("the order changed");
    assert!(sorted.relist);
    assert!(!sorted.relayout && !sorted.wallpaper);

    let base = desktop.settings().clone();
    let recoloured = desktop.apply_settings(PinboardSettings {
        backdrop: Backdrop::Colour(Rgb::new(1, 2, 3)),
        ..base
    });
    assert_eq!(
        recoloured,
        Some(PinboardChange::default()),
        "a new backdrop colour is shown by the repaint alone"
    );

    let base = desktop.settings().clone();
    let papered = desktop
        .apply_settings(PinboardSettings {
            wallpaper: WallpaperChoice::None,
            ..base
        })
        .expect("the wallpaper changed");
    assert!(papered.wallpaper);
    assert!(!papered.relist && !papered.relayout);
}

// --- The context-menu gesture and its commands ----------------------------

#[test]
fn a_secondary_press_on_an_icon_selects_it_and_asks_for_the_menu() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 1);

    let opened = desktop.context_press(at, &layout);
    assert!(opened.redraw);
    assert_eq!(
        desktop.selected(),
        Some(1),
        "the menu acts on what was pointed at"
    );
    assert_eq!(
        opened.action,
        Some(DesktopAction::OpenMenu { at, on_icon: true })
    );

    let again = desktop.context_press(at, &layout);
    assert!(!again.redraw, "the selection did not move");
    assert_eq!(
        again.action,
        Some(DesktopAction::OpenMenu { at, on_icon: true })
    );
}

#[test]
fn a_secondary_press_on_the_backdrop_leaves_the_selection_untouched() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 0), &layout, 0, &[]);

    let opened = desktop.context_press(EMPTY_DESKTOP, &layout);
    assert!(!opened.redraw);
    assert_eq!(
        desktop.selected(),
        Some(0),
        "asking for the menu is not a way to lose a selection"
    );
    assert_eq!(
        opened.action,
        Some(DesktopAction::OpenMenu {
            at: EMPTY_DESKTOP,
            on_icon: false,
        })
    );
}

#[test]
fn the_menus_open_command_resolves_exactly_as_a_double_click_does() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[]);
    let clicked = desktop.press(at, &layout, 1, &[]);

    assert_eq!(
        desktop.command(PinboardCommand::Open, &[], 2).action,
        clicked.action,
        "one definition of what opening an icon means"
    );
    assert_eq!(
        clicked.action,
        Some(DesktopAction::Activate(DesktopActivation::OpenFolder {
            path: "/Users/ada/Desktop/Work".to_string(),
        }))
    );
}

#[test]
fn open_with_nothing_selected_asks_for_nothing() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    assert_eq!(
        desktop.command(PinboardCommand::Open, &[], 0),
        DesktopOutcome::ignored()
    );
}

#[test]
fn a_new_folder_is_named_through_the_shared_naming_over_the_listing() {
    let mut desktop = desktop_of(vec![folder("New Folder")]);
    assert_eq!(
        desktop.command(PinboardCommand::NewFolder, &[], 0).action,
        Some(DesktopAction::CreateFolder {
            path: "/Users/ada/Desktop/New Folder 2".to_string(),
        })
    );
}

#[test]
fn a_sort_or_arrangement_row_asks_the_embedder_to_adopt_the_edit() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    assert_eq!(
        desktop
            .command(PinboardCommand::SortBy(IconSort::Size), &[], 0)
            .action,
        Some(DesktopAction::AdoptSettings(arranged_by(
            IconFlow::default(),
            IconSort::Size
        )))
    );
    assert_eq!(
        desktop.settings().sort,
        IconSort::default(),
        "the model does not adopt settings behind the embedder's back"
    );
    assert_eq!(
        desktop
            .command(PinboardCommand::ArrangeFrom(IconFlow::Trailing), &[], 0)
            .action,
        Some(DesktopAction::AdoptSettings(arranged_by(
            IconFlow::Trailing,
            IconSort::default()
        )))
    );
    assert_eq!(
        desktop.command(PinboardCommand::SortBy(IconSort::Name), &[], 0),
        DesktopOutcome::ignored(),
        "the order already in force is no edit at all"
    );
}

#[test]
fn refresh_relists_now_and_the_remaining_rows_name_their_own_action() {
    let folder = holding(vec![file("a.txt")]);
    let mut desktop = desktop_over(&folder);
    folder.borrow_mut().answer = Some(Ok(vec![file("a.txt"), file("b.txt")]));

    let refreshed = desktop.command(PinboardCommand::Refresh, &[], 1);
    assert!(refreshed.relisted && refreshed.redraw);
    assert_eq!(listings(&folder), 2);
    assert_eq!(desktop.entries().len(), 2);
    assert_eq!(
        desktop.command(PinboardCommand::Refresh, &[], 2),
        DesktopOutcome::ignored(),
        "a refresh that finds nothing changed costs nothing"
    );

    assert_eq!(
        desktop
            .command(PinboardCommand::OpenDesktopFolder, &[], 3)
            .action,
        Some(DesktopAction::Activate(DesktopActivation::OpenFolder {
            path: "/Users/ada/Desktop".to_string(),
        }))
    );
    assert_eq!(
        desktop
            .command(PinboardCommand::ChangeBackground, &[], 4)
            .action,
        Some(DesktopAction::ChangeBackground)
    );
    assert_eq!(desktop.folder_path(), "/Users/ada/Desktop");
}

// --- The context menu itself ----------------------------------------------

/// The screen the menu is clamped onto.
fn screen() -> Rect {
    Rect::new(0, 0, 800, 600)
}

/// The menu's plate on [`screen`], which must exist for an open menu.
fn plate_of(menu: &PinboardMenu, theme: &Theme) -> Rect {
    menu.layout(screen(), Scale::ONE, theme)
        .expect("an open menu has a plate")
}

/// A primary press.
const fn press_event() -> InputEvent {
    InputEvent::PointerPressed {
        button: PointerButton::Primary,
    }
}

/// A primary release.
const fn release_event() -> InputEvent {
    InputEvent::PointerReleased {
        button: PointerButton::Primary,
    }
}

#[test]
fn the_menu_offers_exactly_the_closed_row_set_with_the_settings_in_force_marked() {
    let mut menu = PinboardMenu::new();
    assert!(!menu.is_open());
    menu.open(
        Point::new(10, 10),
        true,
        &arranged_by(IconFlow::Trailing, IconSort::Size),
    );

    assert!(menu.is_open());
    assert_eq!(menu.anchor(), Some(Point::new(10, 10)));
    let labels: Vec<&str> = menu.menu().items().iter().map(MenuItem::label).collect();
    assert_eq!(
        labels,
        vec![
            "Open",
            "New Folder",
            "Sort by Name",
            "Sort by Kind",
            "Sort by Size",
            "Sort by Date",
            "Arrange from the Left",
            "Arrange from the Right",
            "Refresh",
            "Open Desktop Folder",
            "Change Background…",
        ]
    );

    let marked: Vec<&str> = menu
        .menu()
        .items()
        .iter()
        .filter(|item| item.state().activity == ActivityState::Complete)
        .map(MenuItem::label)
        .collect();
    assert_eq!(marked, vec!["Sort by Size", "Arrange from the Right"]);
    for item in menu.menu().items() {
        if item.state().activity == ActivityState::Complete {
            assert!(
                item.reason().is_some(),
                "a marked row says why it is not offered"
            );
        }
    }
}

#[test]
fn every_row_names_the_command_at_its_own_index() {
    let mut menu = PinboardMenu::new();
    menu.open(Point::ORIGIN, true, &PinboardSettings::default());
    let commands: Vec<PinboardCommand> = (0..menu.menu().len())
        .filter_map(|index| menu.command_at(index))
        .collect();
    assert_eq!(
        commands,
        vec![
            PinboardCommand::Open,
            PinboardCommand::NewFolder,
            PinboardCommand::SortBy(IconSort::Name),
            PinboardCommand::SortBy(IconSort::Kind),
            PinboardCommand::SortBy(IconSort::Size),
            PinboardCommand::SortBy(IconSort::Date),
            PinboardCommand::ArrangeFrom(IconFlow::Leading),
            PinboardCommand::ArrangeFrom(IconFlow::Trailing),
            PinboardCommand::Refresh,
            PinboardCommand::OpenDesktopFolder,
            PinboardCommand::ChangeBackground,
        ]
    );
    assert_eq!(
        menu.command_at(menu.menu().len()),
        None,
        "an index the menu does not have names no command"
    );
}

#[test]
fn a_menu_opened_on_the_backdrop_offers_no_open_row() {
    let mut menu = PinboardMenu::new();
    menu.open(Point::ORIGIN, false, &PinboardSettings::default());
    assert_eq!(
        menu.menu().items().first().map(MenuItem::label),
        Some("New Folder")
    );
    assert_eq!(menu.command_at(0), Some(PinboardCommand::NewFolder));
    assert!(
        !menu.menu().items()[0].is_group_break(),
        "the first row divides nothing"
    );

    menu.open(Point::ORIGIN, true, &PinboardSettings::default());
    assert!(
        menu.menu().items()[1].is_group_break(),
        "with Open above it, New Folder starts its own group"
    );
}

#[test]
fn the_menu_is_clamped_wholly_onto_the_screen_from_every_corner() {
    let theme = theme();
    let screen = screen();
    let mut menu = PinboardMenu::new();
    assert_eq!(
        menu.layout(screen, Scale::ONE, &theme),
        None,
        "a closed menu has no plate"
    );

    for corner in [
        Point::new(screen.left(), screen.top()),
        Point::new(screen.right() - 1, screen.top()),
        Point::new(screen.left(), screen.bottom() - 1),
        Point::new(screen.right() - 1, screen.bottom() - 1),
    ] {
        menu.open(corner, true, &PinboardSettings::default());
        let plate = plate_of(&menu, &theme);
        assert!(
            plate.left() >= screen.left() && plate.top() >= screen.top(),
            "{corner:?} ran off the near edge"
        );
        assert!(
            plate.right() <= screen.right() && plate.bottom() <= screen.bottom(),
            "{corner:?} ran off the far edge"
        );
    }

    menu.open(Point::new(100, 100), true, &PinboardSettings::default());
    assert_eq!(
        plate_of(&menu, &theme).origin,
        Point::new(100, 100),
        "a menu with room opens exactly at the pointer"
    );
}

#[test]
fn a_click_on_a_row_chooses_its_command_and_the_keyboard_reaches_it_too() {
    let theme = theme();
    let mut menu = PinboardMenu::new();
    menu.open(Point::new(100, 100), true, &PinboardSettings::default());
    let plate = plate_of(&menu, &theme);
    let row = menu
        .menu()
        .row_rect(0, plate, Scale::ONE, &theme)
        .expect("the Open row");
    let at = Point::new(row.left() + 1, row.top() + 1);

    assert_eq!(
        menu.on_pointer(
            &InputEvent::PointerMoved { to: at },
            at,
            plate,
            Scale::ONE,
            &theme
        ),
        PinboardMenuOutcome::Changed
    );
    assert_eq!(
        menu.on_pointer(&press_event(), at, plate, Scale::ONE, &theme),
        PinboardMenuOutcome::Ignored
    );
    assert_eq!(
        menu.on_pointer(&release_event(), at, plate, Scale::ONE, &theme),
        PinboardMenuOutcome::Chose(PinboardCommand::Open)
    );
    assert!(!menu.is_open(), "choosing closes the menu");

    menu.open(Point::new(100, 100), true, &PinboardSettings::default());
    assert_eq!(menu.on_key(down()), PinboardMenuOutcome::Changed);
    assert_eq!(
        menu.on_key(enter()),
        PinboardMenuOutcome::Chose(PinboardCommand::Open)
    );
}

#[test]
fn escape_dismisses_the_menu_and_so_does_a_press_away_from_it() {
    let theme = theme();
    let mut menu = PinboardMenu::new();
    menu.open(Point::new(100, 100), false, &PinboardSettings::default());
    assert_eq!(menu.on_key(escape()), PinboardMenuOutcome::Dismissed);
    assert!(!menu.is_open());
    assert_eq!(
        menu.on_key(escape()),
        PinboardMenuOutcome::Ignored,
        "a closed menu claims nothing"
    );

    menu.open(Point::new(100, 100), false, &PinboardSettings::default());
    let plate = plate_of(&menu, &theme);
    let away = Point::new(plate.left() - 5, plate.top() - 5);
    assert_eq!(
        menu.on_pointer(&press_event(), away, plate, Scale::ONE, &theme),
        PinboardMenuOutcome::Dismissed
    );
    assert!(!menu.is_open());
    assert_eq!(
        menu.on_pointer(&press_event(), away, plate, Scale::ONE, &theme),
        PinboardMenuOutcome::Ignored
    );
}

#[test]
fn the_row_for_a_setting_already_in_force_cannot_be_chosen() {
    let theme = theme();
    let mut menu = PinboardMenu::new();
    menu.open(Point::new(100, 100), true, &PinboardSettings::default());
    let plate = plate_of(&menu, &theme);
    // "Sort by Name" is the default order, so its row is marked and inert.
    let row = menu
        .menu()
        .row_rect(2, plate, Scale::ONE, &theme)
        .expect("the marked sort row");
    let at = Point::new(row.left() + 1, row.top() + 1);
    assert_eq!(
        menu.command_at(2),
        Some(PinboardCommand::SortBy(IconSort::Name))
    );

    menu.on_pointer(
        &InputEvent::PointerMoved { to: at },
        at,
        plate,
        Scale::ONE,
        &theme,
    );
    menu.on_pointer(&press_event(), at, plate, Scale::ONE, &theme);
    assert_eq!(
        menu.on_pointer(&release_event(), at, plate, Scale::ONE, &theme),
        PinboardMenuOutcome::Ignored
    );
    assert!(menu.is_open(), "an inert row neither acts nor dismisses");
}
