//! Unit tests for the desktop's icon column ([`crate::desktop`]).
//!
//! Kept beside the module in its own file because `desktop.rs` is already
//! past the length at which a `#[cfg(test)]` block belongs in a sibling.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use tairix_abi::window_ipc::AppMenuItemId;
use tairix_abi::{Errno, Time64};
use tairix_browse::{
    AppAssociation, DirectorySource, Entry, EntryKind, GridView, LinkTarget, Listing,
};
use tairix_controls::{ActivityState, MenuMark};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::NoArtwork;
use tairix_proglib::{BundlePath, Catalog, DisplayName, EntryId, LibraryCategory, LibraryEntry};
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_wallpaper::{Backdrop, IconFlow, IconSort, PinboardSettings, Rgb, WallpaperChoice};
use tairix_wm::{Key, NamedKey};

use crate::desktop::{
    Desktop, DesktopAction, DesktopActivation, DesktopOutcome, PinboardChange, DESKTOP_MARGIN,
    RELIST_MIN_INTERVAL_NS,
};
use crate::pinboard::{self, PinboardCommand};
use tairix_controls::{ChainChild, ChainModel};

/// What a [`FakeDir`] answers with, and how often it has been asked.
///
/// `answer` is what the folder holds; `Err` models a folder the source
/// refuses (no permission, not there). Shared with the test so the folder
/// can change under a live desktop without the desktop having to hand its
/// source back out.
#[derive(Default)]
struct Folder {
    answer: Option<Result<Listing, Errno>>,
    listings: usize,
}

/// A directory seam over a shared [`Folder`].
struct FakeDir(Rc<RefCell<Folder>>);

impl DirectorySource for FakeDir {
    fn list(&mut self, _components: &[String]) -> Result<Listing, Errno> {
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
        answer: Some(Ok(Listing::Ready(entries))),
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

/// A shortcut: a link the listing classified as `resolves` and whose stored
/// spelling is `target`.
fn link(name: &str, resolves: LinkTarget, target: &str) -> Entry {
    Entry::new(name, EntryKind::Link(resolves), 0, Time64::UNIX_EPOCH).with_target(target)
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

/// The cell the icon at `index` occupies: the whole of its repaint.
fn cell(layout: &GridView, index: usize) -> Rect {
    layout.cell_rect(0, index).expect("a shown icon")
}

/// The damage `gesture` reports, over a sink of its own so one step's damage
/// can never be read as another's.
fn damage_of(gesture: impl FnOnce(&mut Region)) -> Region {
    let mut damage = Region::new();
    gesture(&mut damage);
    damage
}

/// The centre of the icon at `index`, in screen coordinates.
fn centre_of(layout: &GridView, index: usize) -> Point {
    let bounds = cell(layout, index);
    Point::new(
        bounds.left() + i32::try_from(bounds.width / 2).unwrap_or(0),
        bounds.top() + i32::try_from(bounds.height / 2).unwrap_or(0),
    )
}

/// A point clear of every icon: inside the work area's margin, which no icon
/// reaches under either arrangement.
const EMPTY_DESKTOP: Point = Point::new(2, 2);

/// The catalog identifier `id`, validated.
fn entry_id(id: &str) -> EntryId {
    EntryId::new(id).expect("a valid identifier")
}

/// A catalog holding exactly one entry: `id`, shown as `name`, launching the
/// bundle at `bundle`.
fn catalog_of(id: &str, name: &str, bundle: &str) -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .insert(LibraryEntry::new(
            entry_id(id),
            DisplayName::new(name).expect("a valid display name"),
            BundlePath::new(bundle).expect("a valid bundle path"),
            LibraryCategory::Games,
            None,
        ))
        .expect("the catalog holds one entry");
    catalog
}

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
    desktop.press(centre_of(&layout, 1), &layout, 0, &[], &mut Region::new());
    assert_eq!(desktop.entries()[1].name(), "c.txt");

    // A file appears ahead of it: the selection follows the name, not the
    // index, so the user's selection cannot silently jump to another icon.
    folder.borrow_mut().answer = Some(Ok(Listing::Ready(vec![
        file("a.txt"),
        file("b.txt"),
        file("c.txt"),
    ])));
    assert!(desktop.relist(1));
    assert_eq!(desktop.selected(), Some(2));
    assert_eq!(desktop.entries()[2].name(), "c.txt");
}

#[test]
fn a_relist_that_removes_the_selected_icon_selects_nothing() {
    let folder = holding(vec![file("a.txt"), file("b.txt")]);
    let mut desktop = desktop_over(&folder);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 1), &layout, 0, &[], &mut Region::new());
    folder.borrow_mut().answer = Some(Ok(Listing::Ready(vec![file("a.txt")])));
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
        desktop.pointer_left(&layout, &mut Region::new());
        desktop.pointer_moved(EMPTY_DESKTOP, &layout, step, &mut Region::new());
    }
    assert_eq!(listings(&folder), 1, "a sweep is not a re-list");

    // Once the limit has passed, the next arrival looks again — exactly once.
    desktop.pointer_left(&layout, &mut Region::new());
    desktop.pointer_moved(
        EMPTY_DESKTOP,
        &layout,
        RELIST_MIN_INTERVAL_NS,
        &mut Region::new(),
    );
    assert_eq!(listings(&folder), 2);
    desktop.pointer_moved(
        centre_of(&layout, 0),
        &layout,
        RELIST_MIN_INTERVAL_NS + 1,
        &mut Region::new(),
    );
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
fn hover_follows_the_pointer_and_damages_only_the_cells_it_moves_between() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);
    let mut damage = Region::new();

    desktop.pointer_moved(centre_of(&layout, 0), &layout, 0, &mut damage);
    assert_eq!(damage.rects(), [cell(&layout, 0)]);
    assert_eq!(desktop.hovered(), Some(0));

    damage.clear();
    desktop.pointer_moved(centre_of(&layout, 0), &layout, 1, &mut damage);
    assert!(damage.is_empty(), "the same icon is not a change");

    // Moving between icons costs both cells and nothing between them: the one
    // that lost the highlight and the one that took it.
    damage.clear();
    desktop.pointer_moved(centre_of(&layout, 1), &layout, 2, &mut damage);
    assert_eq!(damage.rects(), [cell(&layout, 0), cell(&layout, 1)]);
    assert_eq!(desktop.hovered(), Some(1));

    damage.clear();
    desktop.pointer_moved(EMPTY_DESKTOP, &layout, 3, &mut damage);
    assert_eq!(damage.rects(), [cell(&layout, 1)]);
    assert_eq!(desktop.hovered(), None);
}

#[test]
fn leaving_the_desktop_clears_the_hover() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.pointer_moved(centre_of(&layout, 0), &layout, 0, &mut Region::new());
    assert_eq!(
        damage_of(|damage| {
            desktop.pointer_left(&layout, damage);
        })
        .rects(),
        [cell(&layout, 0)]
    );
    assert_eq!(desktop.hovered(), None);
    assert!(
        damage_of(|damage| {
            desktop.pointer_left(&layout, damage);
        })
        .is_empty(),
        "leaving twice changes nothing"
    );
}

#[test]
fn a_press_selects_an_icon_and_a_press_on_empty_desktop_clears_it() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);
    let mut damage = Region::new();

    desktop.press(centre_of(&layout, 1), &layout, 0, &[], &mut damage);
    assert_eq!(damage.rects(), [cell(&layout, 1)]);
    assert_eq!(desktop.selected(), Some(1));
    assert!(desktop.is_focused(), "a press moves focus to the desktop");

    // A selection that moves costs the icon it left and the icon it landed on.
    damage.clear();
    desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut damage);
    assert_eq!(damage.rects(), [cell(&layout, 0), cell(&layout, 1)]);
    assert_eq!(desktop.selected(), Some(0));

    damage.clear();
    desktop.press(EMPTY_DESKTOP, &layout, 0, &[], &mut damage);
    assert_eq!(damage.rects(), [cell(&layout, 0)]);
    assert_eq!(desktop.selected(), None);

    damage.clear();
    desktop.press(EMPTY_DESKTOP, &layout, 0, &[], &mut damage);
    assert!(
        damage.is_empty(),
        "clearing an empty selection changes nothing"
    );
}

#[test]
fn focus_moves_the_ring_onto_the_selection_and_costs_nothing_without_one() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    assert!(!desktop.is_focused());

    // Only the selection wears the ring, so with nothing selected the click
    // that moves focus between the desktop and a window moves no pixel.
    assert!(
        damage_of(|damage| desktop.set_focused(true, &layout, damage)).is_empty(),
        "nothing is selected, so no ring appeared"
    );
    assert!(desktop.is_focused());
    assert!(
        damage_of(|damage| desktop.set_focused(false, &layout, damage)).is_empty(),
        "and none disappeared"
    );

    desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut Region::new());
    assert!(desktop.is_focused(), "a press claims the keyboard");
    assert_eq!(
        damage_of(|damage| desktop.set_focused(false, &layout, damage)).rects(),
        [cell(&layout, 0)],
        "the ring left the selected icon"
    );
    assert_eq!(
        damage_of(|damage| desktop.set_focused(true, &layout, damage)).rects(),
        [cell(&layout, 0)],
        "and came back to it"
    );
    assert!(
        damage_of(|damage| desktop.set_focused(true, &layout, damage)).is_empty(),
        "unchanged focus damages nothing"
    );
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
        desktop.set_focused(true, &layout, &mut Region::new());

        // With nothing selected the first arrow starts at the first icon.
        assert_eq!(
            damage_of(|damage| {
                desktop.key(down(), true, &layout, &[], damage);
            })
            .rects(),
            [cell(&layout, 0)]
        );
        assert_eq!(desktop.selected(), Some(0));

        desktop.key(down(), true, &layout, &[], &mut Region::new());
        assert_eq!(desktop.selected(), Some(1));
        desktop.key(up(), true, &layout, &[], &mut Region::new());
        assert_eq!(desktop.selected(), Some(0));

        // One whole column further into the listing, and back out again. The
        // two cells lie in different columns, so their order in the damage is
        // the arrangement's, not the listing's.
        let stepped = damage_of(|damage| {
            desktop.key(later, true, &layout, &[], damage);
        });
        assert_eq!(desktop.selected(), Some(per_column), "{icons:?} onward");
        assert!(
            stepped.intersects(cell(&layout, 0)) && stepped.intersects(cell(&layout, per_column)),
            "{icons:?} repaints the icon left and the icon reached"
        );
        assert_eq!(stepped.rects().len(), 2, "{icons:?} and nothing else");
        desktop.key(earlier, true, &layout, &[], &mut Region::new());
        assert_eq!(desktop.selected(), Some(0), "{icons:?} back");
    }
}

#[test]
fn the_selection_clamps_at_both_ends_of_the_listing() {
    let mut desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);
    desktop.set_focused(true, &layout, &mut Region::new());
    desktop.key(down(), true, &layout, &[], &mut Region::new());
    assert!(damage_of(|damage| {
        desktop.key(up(), true, &layout, &[], damage);
    })
    .is_empty());
    assert_eq!(desktop.selected(), Some(0));

    desktop.key(down(), true, &layout, &[], &mut Region::new());
    assert_eq!(desktop.selected(), Some(1));
    assert!(damage_of(|damage| {
        desktop.key(down(), true, &layout, &[], damage);
    })
    .is_empty());
    assert_eq!(desktop.selected(), Some(1), "clamped at the last icon");
}

#[test]
fn keys_do_nothing_while_the_desktop_does_not_hold_the_keyboard() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    let mut damage = Region::new();
    assert_eq!(
        desktop.key(down(), true, &layout, &[], &mut damage),
        DesktopOutcome::ignored()
    );
    assert!(damage.is_empty(), "and paints nothing either");
    assert_eq!(desktop.selected(), None);
}

#[test]
fn a_key_release_and_an_unknown_key_change_nothing() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    // One sink across all three: between them they must add no cell at all.
    let mut damage = Region::new();
    desktop.set_focused(true, &layout, &mut damage);
    assert_eq!(
        desktop.key(down(), false, &layout, &[], &mut damage),
        DesktopOutcome::ignored()
    );
    assert_eq!(
        desktop.key(Key::Char('x'), true, &layout, &[], &mut damage),
        DesktopOutcome::ignored()
    );
    assert_eq!(
        desktop.key(Key::Named(NamedKey::Tab), true, &layout, &[], &mut damage),
        DesktopOutcome::ignored()
    );
    assert!(damage.is_empty(), "and none of them paints anything");
}

#[test]
fn escape_clears_the_selection() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut Region::new());
    assert_eq!(
        damage_of(|damage| {
            desktop.key(escape(), true, &layout, &[], damage);
        })
        .rects(),
        [cell(&layout, 0)]
    );
    assert_eq!(desktop.selected(), None);
    let mut damage = Region::new();
    assert_eq!(
        desktop.key(escape(), true, &layout, &[], &mut damage),
        DesktopOutcome::ignored(),
        "nothing selected and no offer outstanding"
    );
    assert!(damage.is_empty(), "so there is nothing to repaint");
}

// --- Activation -----------------------------------------------------------

#[test]
fn double_clicking_a_folder_opens_the_file_manager_at_its_path() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    let acted = desktop.press(at, &layout, 1, &[], &mut Region::new());
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
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    assert_eq!(
        desktop
            .press(at, &layout, 1, &[], &mut Region::new())
            .action,
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
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    assert_eq!(
        desktop
            .press(at, &layout, 1, &editor(), &mut Region::new())
            .action,
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
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    let acted = desktop.press(at, &layout, 1, &editor(), &mut Region::new());
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
fn a_shortcut_is_named_in_the_desktop_folder_and_stores_its_target_verbatim() {
    let desktop = desktop_of(vec![]);
    let catalog = catalog_of("os.tairix.chess", "Chess", "/Apps/games/chess.app");
    assert_eq!(
        desktop.shortcut_to(&catalog, &entry_id("os.tairix.chess")),
        DesktopAction::CreateShortcut {
            link: "/Users/ada/Desktop/Chess".to_string(),
            target: "/Apps/games/chess.app".to_string(),
        },
        "the link is the entry's own name inside the desktop folder, and the \
         target is the bundle directory, untouched"
    );
}

#[test]
fn a_shortcut_whose_entry_left_the_catalog_is_refused_rather_than_guessed() {
    let desktop = desktop_of(vec![]);
    assert_eq!(
        desktop.shortcut_to(&Catalog::default(), &entry_id("os.tairix.chess")),
        DesktopAction::Refuse(
            "desktop: library entry os.tairix.chess is no longer catalogued\n".to_string()
        )
    );
}

#[test]
fn a_display_name_the_shared_rule_refuses_is_refused_with_its_reason() {
    // A display name is not automatically a file name: the library permits a
    // `/` and a `:` in one, and the filesystem does not.
    let desktop = desktop_of(vec![]);
    for (name, reason) in [
        (
            "Chess/Deluxe",
            "a file or directory name may not contain `/`",
        ),
        (
            "Chess:1",
            "path component contains a `:` (a reserved delimiter)",
        ),
        ("..", "`.` and `..` are not valid file or directory names"),
    ] {
        let catalog = catalog_of("os.tairix.chess", name, "/Apps/chess.app");
        assert_eq!(
            desktop.shortcut_to(&catalog, &entry_id("os.tairix.chess")),
            DesktopAction::Refuse(alloc::format!(
                "desktop: '{name}' cannot be a shortcut name ({reason})\n"
            )),
            "the one shared name rule's own reason is what the desktop reports"
        );
    }
}

#[test]
fn a_shortcut_is_asked_for_even_where_the_name_is_already_taken() {
    // The desktop never works a collision around: `fs_symlink` replaces no
    // name, so the authoritative answer is the kernel's `AlreadyExists` at
    // create time — not a second, differently-named shortcut chosen here off a
    // listing that may already be stale.
    let desktop = desktop_of(vec![file("Chess")]);
    let catalog = catalog_of("os.tairix.chess", "Chess", "/Apps/chess.app");
    assert_eq!(
        desktop.shortcut_to(&catalog, &entry_id("os.tairix.chess")),
        DesktopAction::CreateShortcut {
            link: "/Users/ada/Desktop/Chess".to_string(),
            target: "/Apps/chess.app".to_string(),
        }
    );
}

#[test]
fn double_clicking_a_shortcut_to_a_bundle_launches_the_resolved_target() {
    // The shortcut is named for the program, not for the bundle: bundle-ness
    // is read off the *target's* leaf, and the launch must name the bundle the
    // link resolves to, because the load gate judges the path it is handed.
    let mut desktop = desktop_of(vec![link("Chess", LinkTarget::Bundle, "/Apps/chess.app")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    assert_eq!(
        desktop
            .press(at, &layout, 1, &[], &mut Region::new())
            .action,
        Some(DesktopAction::Activate(DesktopActivation::Launch {
            run_path: "/Apps/chess.app/Run".to_string(),
            label: "chess".to_string(),
            argument: None,
        }))
    );
}

#[test]
fn double_clicking_a_shortcut_to_a_folder_or_a_file_acts_through_the_link() {
    let mut desktop = desktop_of(vec![
        link("Work", LinkTarget::Directory, "/Users/ada/Documents/Work"),
        link("notes.txt", LinkTarget::File, "/Users/ada/Documents/n.txt"),
    ]);
    let layout = layout_of(&desktop);
    let folder_at = centre_of(&layout, 0);
    desktop.press(folder_at, &layout, 0, &[], &mut Region::new());
    assert_eq!(
        desktop
            .press(folder_at, &layout, 1, &[], &mut Region::new())
            .action,
        Some(DesktopAction::Activate(DesktopActivation::OpenFolder {
            path: "/Users/ada/Desktop/Work".to_string(),
        })),
        "a directory the link names is opened through the link, which the kernel resolves"
    );
    let file_at = centre_of(&layout, 1);
    desktop.press(file_at, &layout, 2, &editor(), &mut Region::new());
    assert_eq!(
        desktop
            .press(file_at, &layout, 3, &editor(), &mut Region::new())
            .action,
        Some(DesktopAction::Activate(DesktopActivation::Launch {
            run_path: "/Apps/Edit.app/Run".to_string(),
            label: "Edit".to_string(),
            argument: Some("/Users/ada/Desktop/notes.txt".to_string()),
        })),
        "the association follows the target's kind and the argument is the link"
    );
}

#[test]
fn double_clicking_a_shortcut_whose_target_has_gone_is_refused_with_its_reason() {
    let mut desktop = desktop_of(vec![link("Chess", LinkTarget::Dangling, "/Apps/chess.app")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    let acted = desktop.press(at, &layout, 1, &editor(), &mut Region::new());
    assert_eq!(
        acted.action,
        Some(DesktopAction::Refuse(
            "desktop: the shortcut 'Chess' points at something that is not there\n".to_string()
        )),
        "a dangling shortcut is never launched on the chance that it works"
    );
    assert_eq!(desktop.selected(), Some(0), "the icon stays selected");
}

#[test]
fn a_shortcut_that_names_nothing_at_all_is_refused_rather_than_opened() {
    // A listing that reported a link but no target describes nothing to act
    // on; the desktop says so instead of resolving an empty spelling.
    let mut desktop = desktop_of(vec![Entry::new(
        "Chess",
        EntryKind::Link(LinkTarget::Bundle),
        0,
        Time64::UNIX_EPOCH,
    )]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    assert_eq!(
        desktop
            .press(at, &layout, 1, &[], &mut Region::new())
            .action,
        Some(DesktopAction::Refuse(
            "desktop: the shortcut 'Chess' names nothing\n".to_string()
        ))
    );
}

#[test]
fn enter_activates_the_selection_and_does_nothing_with_no_selection() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    let mut damage = Region::new();
    desktop.set_focused(true, &layout, &mut damage);
    assert_eq!(
        desktop.key(enter(), true, &layout, &[], &mut damage),
        DesktopOutcome::ignored()
    );
    assert!(
        damage.is_empty(),
        "there was nothing to activate or repaint"
    );
    desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut Region::new());
    assert_eq!(
        desktop
            .key(enter(), true, &layout, &[], &mut Region::new())
            .action,
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
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    let late = desktop.press(
        at,
        &layout,
        tairix_browse::DOUBLE_CLICK_INTERVAL_NS + 1,
        &[],
        &mut Region::new(),
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
    desktop.set_focused(true, &layout, &mut Region::new());
    desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut Region::new());
    desktop.pointer_moved(centre_of(&layout, 1), &layout, 1, &mut Region::new());

    desktop.render(
        &mut surface,
        &layout,
        Scale::ONE,
        &theme,
        &mut NoArtwork,
        work_area(),
    );

    // With no artwork store at all every tile still draws its built-in
    // glyph, so no icon slot can come out blank.
    for index in 0..3 {
        assert!(
            painted(&surface, cell(&layout, index)),
            "icon {index} drew nothing at all"
        );
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
        &mut NoArtwork,
        work_area(),
    );
    assert!(
        !painted(&surface, work_area()),
        "an empty folder leaves the layer fully transparent"
    );
}

#[test]
fn a_render_clipped_to_one_cell_leaves_every_other_icon_untouched() {
    let desktop = desktop_of(vec![file("a.txt"), file("b.txt")]);
    let layout = layout_of(&desktop);
    let theme = theme();
    let mut surface = Surface::new(800, 600).expect("a screen-sized layer");

    desktop.render(
        &mut surface,
        &layout,
        Scale::ONE,
        &theme,
        &mut NoArtwork,
        cell(&layout, 1),
    );

    // A tile draws strictly inside its own cell, which is what lets a moved
    // highlight cost one cell instead of the whole layer.
    assert!(
        painted(&surface, cell(&layout, 1)),
        "the cell asked for drew"
    );
    assert!(
        !painted(&surface, cell(&layout, 0)),
        "a cell outside the repainted area is left exactly as it was"
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
        let bounds = cell(&layout, 0);
        assert_eq!(
            bounds.top(),
            area.top() + margin,
            "{icons:?} starts at the top"
        );
        // The icon the user can see is the icon a press lands on, whichever
        // corner the column grew from.
        desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut Region::new());
        assert_eq!(desktop.selected(), Some(0), "{icons:?} hit-test");
        cells.push(bounds);
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
    let before = cell(&layout_of(&desktop), 0);

    let change = desktop
        .apply_settings(arranged_by(IconFlow::Trailing, IconSort::default()))
        .expect("the arrangement changed");
    assert!(change.relayout);
    assert!(!change.relist && !change.wallpaper);
    assert_eq!(listings(&folder), 1, "an arrangement is not a listing");

    let after = cell(&layout_of(&desktop), 0);
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

    let mut damage = Region::new();
    assert!(
        desktop.context_press(at, &layout, &mut damage),
        "the press landed on an icon, so the menu offers `Open`"
    );
    assert_eq!(damage.rects(), [cell(&layout, 1)]);
    assert_eq!(
        desktop.selected(),
        Some(1),
        "the menu acts on what was pointed at"
    );

    damage.clear();
    assert!(desktop.context_press(at, &layout, &mut damage));
    assert!(damage.is_empty(), "the selection did not move");
}

#[test]
fn a_secondary_press_on_the_backdrop_leaves_the_selection_untouched() {
    let mut desktop = desktop_of(vec![file("a.txt")]);
    let layout = layout_of(&desktop);
    desktop.press(centre_of(&layout, 0), &layout, 0, &[], &mut Region::new());

    let mut damage = Region::new();
    assert!(
        !desktop.context_press(EMPTY_DESKTOP, &layout, &mut damage),
        "a press on empty backdrop has nothing to open"
    );
    assert!(damage.is_empty(), "the backdrop menu moves no highlight");
    assert_eq!(
        desktop.selected(),
        Some(0),
        "asking for the menu is not a way to lose a selection"
    );
}

#[test]
fn the_menus_open_command_resolves_exactly_as_a_double_click_does() {
    let mut desktop = desktop_of(vec![folder("Work")]);
    let layout = layout_of(&desktop);
    let at = centre_of(&layout, 0);
    desktop.press(at, &layout, 0, &[], &mut Region::new());
    let clicked = desktop.press(at, &layout, 1, &[], &mut Region::new());

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
    folder.borrow_mut().answer = Some(Ok(Listing::Ready(vec![file("a.txt"), file("b.txt")])));

    // A re-list reports no cell: the icons themselves moved, so the caller
    // repaints the whole layer instead of any cell of the layout it replaced.
    assert_eq!(
        desktop.command(PinboardCommand::Refresh, &[], 1),
        DesktopOutcome {
            relisted: true,
            action: None,
        }
    );
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

// --- The backdrop menu's row model ----------------------------------------

/// The rows the menu offers, in order, as the labels a user reads.
fn labels(model: &ChainModel) -> Vec<&str> {
    model.rows().iter().map(|row| row.drawn().label()).collect()
}

#[test]
fn the_menu_offers_exactly_the_closed_row_set_with_the_settings_in_force_marked() {
    let model = pinboard::model(true, &arranged_by(IconFlow::Trailing, IconSort::Size));

    assert_eq!(
        labels(&model),
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

    // The sort orders and the arrangements are each a group of alternatives,
    // so the one in force is drawn as that group's chosen member — never as
    // finished work, which is what an activity bead means.
    let marked: Vec<&str> = model
        .rows()
        .iter()
        .filter(|row| row.drawn().mark() == MenuMark::Radio)
        .map(|row| row.drawn().label())
        .collect();
    assert_eq!(marked, vec!["Sort by Size", "Arrange from the Right"]);
    for row in model.rows() {
        let item = row.drawn();
        if item.mark() != MenuMark::Radio {
            assert!(item.state().enabled, "{:?} is offered", item.label());
            assert_eq!(item.state().activity, ActivityState::Idle);
            continue;
        }
        assert!(
            !item.state().enabled,
            "the setting in force is not a command"
        );
        assert!(
            item.reason().is_some(),
            "a marked row says why it is not offered"
        );
        assert_eq!(
            item.state().activity,
            ActivityState::Idle,
            "an appearance row states no progress"
        );
    }
}

#[test]
fn every_row_names_the_command_at_its_own_index() {
    let model = pinboard::model(true, &PinboardSettings::default());
    let commands: Vec<PinboardCommand> = (1..=u16::try_from(model.rows().len()).expect("small"))
        .map(|raw| {
            PinboardCommand::from_item(AppMenuItemId::new(raw).expect("a non-zero row id"))
                .expect("a declared row")
        })
        .collect();
    assert_eq!(commands, PinboardCommand::ALL.to_vec());
    assert_eq!(
        PinboardCommand::from_item(
            AppMenuItemId::new(u16::try_from(PinboardCommand::ALL.len() + 1).expect("small"))
                .expect("a non-zero row id")
        ),
        None,
        "an id the menu does not have names no command"
    );
}

/// The rows a gesture leaves out must not shift what the rows around them
/// mean: an id is a command's own position, never a row's.
#[test]
fn a_menu_opened_on_the_backdrop_offers_no_open_row_and_shifts_no_id() {
    let bare = pinboard::model(false, &PinboardSettings::default());
    assert_eq!(
        bare.rows().first().map(|row| row.drawn().label()),
        Some("New Folder")
    );
    assert!(
        !bare.rows()[0].drawn().is_group_break(),
        "the first row divides nothing"
    );
    assert_eq!(
        bare.rows().len(),
        PinboardCommand::ALL.len() - 1,
        "only `Open` is left out"
    );

    let over_icon = pinboard::model(true, &PinboardSettings::default());
    assert!(
        over_icon.rows()[1].drawn().is_group_break(),
        "with Open above it, New Folder starts its own group"
    );

    // `New Folder` is `ALL` position 1, so its id is 2 whichever gesture built
    // the plate — the row it sits at moved, its command did not.
    let id = AppMenuItemId::new(2).expect("a non-zero row id");
    assert_eq!(
        PinboardCommand::from_item(id),
        Some(PinboardCommand::NewFolder)
    );
}

/// Every group break the plate draws, so a divider cannot drift onto a row
/// that begins nothing.
#[test]
fn the_menu_groups_its_rows_by_what_they_are() {
    let model = pinboard::model(true, &PinboardSettings::default());
    let grouped: Vec<&str> = model
        .rows()
        .iter()
        .filter(|row| row.drawn().is_group_break())
        .map(|row| row.drawn().label())
        .collect();
    assert_eq!(
        grouped,
        vec![
            "New Folder",
            "Sort by Name",
            "Arrange from the Left",
            "Refresh"
        ]
    );
}

/// The chain answers with a row id and nothing else, so every row the model
/// offers must carry one it can read back.
#[test]
fn the_model_declares_no_row_the_chain_cannot_answer_with() {
    for on_icon in [false, true] {
        let model = pinboard::model(on_icon, &PinboardSettings::default());
        assert!(!model.rows().is_empty(), "a plate with nothing to choose");
        for row in model.rows() {
            assert_eq!(
                *row.child(),
                ChainChild::None,
                "the backdrop menu declares no submenu and hangs no window"
            );
        }
    }
}
