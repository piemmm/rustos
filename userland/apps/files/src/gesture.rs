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
//! # The three gestures
//!
//! | gesture | what it does |
//! |---|---|
//! | double-click | activate: descend, run a bundle, or open a file |
//! | shift-double-click | list a bundle's contents instead of running it |
//! | right-click | ask the desktop for the context menu on the item |
//!
//! There is no right-*double*-click: the menu the first press opens is the
//! desktop's chain and holds the seat's grab, so the second press is consumed
//! there and never reaches this window. Its "open this and I am done here" verb
//! is a row of the menu instead ([`AfterHandoff::CloseWindow`], reached from
//! `ContextCommand::OpenAndClose`) — discoverable, and reachable from the
//! keyboard, which the gesture never was (`plans/NEW-MENUS.md` D20).
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

#[cfg(test)]
mod tests {
    use super::{bundle_intent, primary_press, PrimaryPress};
    use tairix_browse::{BundleIntent, DoubleClickTracker};

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
    fn a_right_click_breaks_a_half_finished_left_pair() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(
            primary_press(&mut tracker, 0, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
        // A right press asks the desktop for the menu and resets the tracker,
        // exactly as the app's own secondary-press path does, so the click after it
        // is a fresh single rather than the second half of the left pair.
        tracker.reset();
        assert_eq!(
            primary_press(&mut tracker, 1, Some(2)),
            PrimaryPress::Select { index: 2 }
        );
    }

    #[test]
    fn shift_asks_for_the_bundles_contents_and_nothing_else_does() {
        assert_eq!(bundle_intent(true), BundleIntent::Browse);
        assert_eq!(bundle_intent(false), BundleIntent::Launch);
    }
}
