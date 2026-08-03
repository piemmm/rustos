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
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::NoArtwork;
use tairix_raster::Surface;
use tairix_theme::{TextRole, Theme};
use tairix_wm::{Key, NamedKey};

use crate::desktop::{
    Desktop, DesktopAction, DesktopActivation, DesktopOutcome, RELIST_MIN_INTERVAL_NS,
};

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

fn theme() -> Theme {
    Theme::dark()
}

fn font(theme: &Theme) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, Scale::ONE)
}

/// A work area the size of a modest screen with room for several columns.
fn work_area() -> Rect {
    Rect::new(0, 0, 800, 600)
}

fn layout_of(desktop: &Desktop<FakeDir>) -> GridView {
    let theme = theme();
    desktop.layout(work_area(), Scale::ONE, font(&theme))
}

/// The centre of the icon at `index`, in screen coordinates.
fn centre_of(layout: &GridView, index: usize) -> Point {
    let cell = layout.cell_rect(0, index).expect("a shown icon");
    Point::new(
        cell.left() + i32::try_from(cell.width / 2).unwrap_or(0),
        cell.top() + i32::try_from(cell.height / 2).unwrap_or(0),
    )
}

/// A point clear of every icon: the work area's leading edge, which the
/// trailing-anchored column never reaches while the listing is short.
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
fn the_arrows_move_the_selection_down_the_column_and_across_columns() {
    let entries: Vec<Entry> = (0..12)
        .map(|n| file(&alloc::format!("f{n:02}.txt")))
        .collect();
    let mut desktop = desktop_of(entries);
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

    // Left is one whole column inward (later in the listing); right comes
    // back out again.
    desktop.key(left(), true, &layout, &[]);
    assert_eq!(desktop.selected(), Some(per_column));
    desktop.key(right(), true, &layout, &[]);
    assert_eq!(desktop.selected(), Some(0));
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

    desktop.render(
        &mut surface,
        &layout,
        Scale::ONE,
        &theme,
        font(&theme),
        &mut NoArtwork,
    );

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
    desktop.render(
        &mut surface,
        &layout,
        Scale::ONE,
        &theme,
        font(&theme),
        &mut NoArtwork,
    );
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
