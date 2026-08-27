//! What a pointer gesture on a listed item means.
//!
//! # Why this is its own module
//!
//! The `Run` binary around it is a freestanding program — it only exists when
//! the crate is built for a bare-metal target — so nothing inside it can be
//! reached by a host test, exactly as for [`crate::command`]. Which gesture
//! opens what is worth testing: a modifier changes what activating a bundle
//! means, a second press of the *same* button completes a different gesture
//! from a second press of the other one, and a press that lands on nothing
//! must break the run rather than pair across it. All of that is a pure
//! function of the press and the remembered one, so it lives here and is
//! covered by the tests beside it.
//!
//! # The four gestures
//!
//! | gesture | what it does |
//! |---|---|
//! | double-click | activate: descend, run a bundle, or open a file |
//! | shift-double-click | list a bundle's contents instead of running it |
//! | right-click | open the context menu on the item |
//! | right-double-click | activate, and close this window once handed over |
//!
//! The pairing rule itself is the shared engine's
//! ([`DoubleClickTracker`]) — keyed on the button as well as the item, so a
//! left press and a right press are never mistaken for one gesture.

use tairix_browse::{BundleIntent, ClickKind, DoubleClickTracker, PointerButton};

/// The bundle intent a gesture with (or without) shift held means.
///
/// An application bundle is both a program and a directory, so activating one
/// is genuinely ambiguous and shift is the modifier that asks for the
/// directory. One spelling, shared by the pointer and the keyboard, so
/// `Shift+Enter` and a shift-double-click cannot come to mean different
/// things.
#[must_use]
pub const fn bundle_intent(shift: bool) -> BundleIntent {
    if shift {
        BundleIntent::Browse
    } else {
        BundleIntent::Launch
    }
}

/// Whether a completed activation that handed the entry to another program
/// leaves this window with nothing left to do.
///
/// The gesture decides: a plain activation leaves the manager open on the
/// folder it was showing, while a right-double-click means "open this and I am
/// done here". A *descent* never closes the window whichever is asked for — it
/// is the window's new content, so closing it would leave the user with
/// nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AfterHandoff {
    /// Keep the window on the folder it is showing.
    Keep,
    /// Close the window: the entry has been handed to another program.
    CloseWindow,
}

/// What a lone secondary press does — the half of the gesture that is not yet
/// a double-click.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuOnSingle {
    /// Open the context menu on the item under the pointer: the ordinary
    /// right-click.
    Open,
    /// Leave the already-open menu exactly as it is. The press was made *over*
    /// that menu, so re-anchoring it on whatever item happens to be beneath
    /// would move a surface the user is reading.
    Leave,
}

/// What a primary press on the listing resolved to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PrimaryPress {
    /// The second press of a double-click on the item at `index`: select and
    /// activate it.
    Activate {
        /// The item the pair landed on.
        index: usize,
    },
    /// A lone press on the item at `index`: select it and nothing more.
    Select {
        /// The item the press landed on.
        index: usize,
    },
    /// The press landed on no item, so it belongs to the chrome behind the
    /// listing.
    Chrome,
}

/// Decide what a primary press at monotonic time `now_ns` means, given the item
/// `index` the hit-test resolved (`None` for the chrome).
///
/// A press that resolves to no item cannot begin a pair, so it resets
/// `tracker`: a click *through* the chrome and back onto the same item is never
/// mistaken for a double-click of that item.
pub fn primary_press(
    tracker: &mut DoubleClickTracker,
    now_ns: u64,
    index: Option<usize>,
) -> PrimaryPress {
    let Some(index) = index else {
        tracker.reset();
        return PrimaryPress::Chrome;
    };
    match tracker.register(now_ns, index, PointerButton::Primary) {
        ClickKind::Double => PrimaryPress::Activate { index },
        ClickKind::Single => PrimaryPress::Select { index },
    }
}

/// What a secondary press resolved to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SecondaryPress {
    /// The second press of a right-double-click on the item at `index`:
    /// activate it, and close the window once it is handed over.
    OpenAndLeave {
        /// The item the pair landed on.
        index: usize,
    },
    /// A lone right-click: open the context menu, acting on `index` (`None`
    /// when the press landed on empty space or the chrome, where the menu
    /// offers only the directory-scoped commands).
    OpenMenu {
        /// The item the menu acts on.
        index: Option<usize>,
    },
    /// Nothing: the press began no pair and there is no menu to open.
    Ignore,
}

/// Decide what a secondary press at monotonic time `now_ns` means, given the
/// item `index` the hit-test resolved (`None` for empty space or the chrome)
/// and what a lone press should do.
///
/// The press is registered against `tracker` so a quick second press on the
/// same item completes the pair; a press that resolves to no item cannot begin
/// one, so it resets the tracker rather than leaving a stale press to pair
/// with something the pointer has since left.
pub fn secondary_press(
    tracker: &mut DoubleClickTracker,
    now_ns: u64,
    index: Option<usize>,
    single: MenuOnSingle,
) -> SecondaryPress {
    let lone = match single {
        MenuOnSingle::Open => SecondaryPress::OpenMenu { index },
        MenuOnSingle::Leave => SecondaryPress::Ignore,
    };
    let Some(index) = index else {
        tracker.reset();
        return lone;
    };
    match tracker.register(now_ns, index, PointerButton::Secondary) {
        ClickKind::Double => SecondaryPress::OpenAndLeave { index },
        ClickKind::Single => lone,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bundle_intent, primary_press, secondary_press, MenuOnSingle, PrimaryPress, SecondaryPress,
    };
    use tairix_browse::{BundleIntent, DoubleClickTracker, DOUBLE_CLICK_INTERVAL_NS};

    #[test]
    fn a_lone_left_click_selects_and_a_quick_second_activates() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            primary_press(&mut tracker, 0, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
        assert_eq!(
            primary_press(&mut tracker, 1_000, Some(2)),
            PrimaryPress::Activate { index: 2 }
        );
    }

    #[test]
    fn a_left_click_on_the_chrome_breaks_the_run() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            primary_press(&mut tracker, 0, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
        assert_eq!(primary_press(&mut tracker, 1, None), PrimaryPress::Chrome);
        // Back on the same item, the run has been broken: a fresh single.
        assert_eq!(
            primary_press(&mut tracker, 2, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
    }

    #[test]
    fn the_two_buttons_never_complete_each_others_gesture() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            primary_press(&mut tracker, 0, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
        // A right press on the same item is a new gesture, not the second half
        // of the left one.
        assert_eq!(
            secondary_press(&mut tracker, 1, Some(2), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(2) }
        );
        assert_eq!(
            primary_press(&mut tracker, 2, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
    }

    #[test]
    fn shift_asks_for_the_bundles_contents_and_nothing_else_does() {
        assert_eq!(bundle_intent(true), BundleIntent::Browse);
        assert_eq!(bundle_intent(false), BundleIntent::Launch);
    }

    #[test]
    fn a_lone_right_click_on_an_item_opens_the_menu_on_it() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            secondary_press(&mut tracker, 0, Some(4), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(4) }
        );
    }

    #[test]
    fn a_quick_second_right_click_on_the_same_item_opens_and_leaves() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            secondary_press(&mut tracker, 0, Some(4), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(4) }
        );
        // The menu the first press opened is over the item, so the second
        // press is told to leave it rather than re-anchor it — and completes
        // the pair.
        assert_eq!(
            secondary_press(&mut tracker, 1_000, Some(4), MenuOnSingle::Leave),
            SecondaryPress::OpenAndLeave { index: 4 }
        );
    }

    #[test]
    fn a_slow_second_right_click_leaves_the_open_menu_alone() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            secondary_press(&mut tracker, 0, Some(4), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(4) }
        );
        assert_eq!(
            secondary_press(
                &mut tracker,
                DOUBLE_CLICK_INTERVAL_NS + 1,
                Some(4),
                MenuOnSingle::Leave
            ),
            SecondaryPress::Ignore
        );
    }

    #[test]
    fn a_second_right_click_on_a_different_item_completes_nothing() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            secondary_press(&mut tracker, 0, Some(4), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(4) }
        );
        assert_eq!(
            secondary_press(&mut tracker, 1, Some(5), MenuOnSingle::Leave),
            SecondaryPress::Ignore
        );
    }

    #[test]
    fn a_right_click_on_empty_space_opens_the_directory_menu_and_breaks_the_run() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            secondary_press(&mut tracker, 0, Some(4), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(4) }
        );
        // Empty space is not an item: it opens the directory-scoped menu and
        // clears the remembered press, so returning to the item is a fresh
        // single rather than a pair across the gap.
        assert_eq!(
            secondary_press(&mut tracker, 1, None, MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: None }
        );
        assert_eq!(
            secondary_press(&mut tracker, 2, Some(4), MenuOnSingle::Open),
            SecondaryPress::OpenMenu { index: Some(4) }
        );
    }

    #[test]
    fn a_press_over_a_menu_on_empty_space_is_ignored() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            secondary_press(&mut tracker, 0, None, MenuOnSingle::Leave),
            SecondaryPress::Ignore
        );
    }
}
