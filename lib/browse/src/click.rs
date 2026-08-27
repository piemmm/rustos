//! Double-click detection: the one pure rule that turns a stream of primary
//! pointer presses into single-click and double-click gestures the file
//! manager and the trusted picker share (`plans/NEW-FILEMANAGER.md` `FM12`).
//!
//! A pointer double-click is what *activating* an item means — the keyboard
//! spelling is `Enter` on the selection
//! ([`activate_selected`](crate::Browser::activate_selected)). It is two
//! presses of the *same button* on the *same* item close enough together in
//! time. The decision lives here, once, so a double-click can never open
//! something a keyboard `Enter` would not: the app resolves the press to an
//! item, asks this detector whether it completes a double-click, and — if it
//! does — runs the very same [`Activation`](crate::Activation) dispatch.
//!
//! The button is part of the pairing because the two buttons mean different
//! things: a primary double-click activates in place, a secondary one activates
//! and leaves. One press of each is therefore two gestures begun, never one
//! completed.
//!
//! The detector holds no authority and does no I/O. It decides only *whether* a
//! press is the second of a pair; the caller supplies the item index (from the
//! shared pixel→index hit-test), the button, and a monotonic timestamp (the
//! kernel monotonic clock, which needs no capability), and performs the
//! activation itself under the user's own identity.

use tairix_input::PointerButton;

/// The default maximum interval between the two presses of a double-click, in
/// nanoseconds (half a second).
///
/// A deliberate, fixed UX convenience bound, not a hardware-scaled capacity: it
/// is the human-perception window for "one gesture, two clicks", the same order
/// of magnitude every desktop uses, and reaching it never fails anything — a
/// slower second press is simply a fresh single click.
pub const DOUBLE_CLICK_INTERVAL_NS: u64 = 500_000_000;

/// What a press resolved to once the double-click rule was applied.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClickKind {
    /// A lone press so far: it selects the item under the pointer. A matching
    /// press soon after on the same item will complete a [`Double`](Self::Double).
    Single,
    /// The second press of a pair, same button and same item, within the
    /// interval: the caller activates the item (descend / launch a bundle /
    /// open a file).
    Double,
}

/// One remembered press: the button, the item it landed on, and when.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct LastClick {
    /// The monotonic timestamp of the press, in nanoseconds.
    at_ns: u64,
    /// The item index the press resolved to (the shared hit-test's result).
    index: usize,
    /// The button that was pressed.
    button: PointerButton,
}

/// The pure double-click detector: it remembers the previous qualifying press
/// and reports whether the next one completes a double-click.
///
/// A completed double-click *consumes* both presses — the state is cleared — so
/// a third quick press on the same item begins a fresh single click rather than
/// registering a second double from one rapid run (standard triple-click
/// semantics). Any press that is not the second of a pair becomes the new
/// remembered press, so only *consecutive* presses of the *same button* on the
/// *same* item can pair — a press of the other button in between breaks the run
/// rather than being invisible to it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct DoubleClickTracker {
    last: Option<LastClick>,
}

impl DoubleClickTracker {
    /// A fresh tracker with nothing remembered.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Register a `button` press on the item at `index` at monotonic time
    /// `now_ns`, using the default [`DOUBLE_CLICK_INTERVAL_NS`] window, and
    /// report whether it completes a double-click.
    #[must_use]
    pub fn register(&mut self, now_ns: u64, index: usize, button: PointerButton) -> ClickKind {
        self.register_within(now_ns, index, button, DOUBLE_CLICK_INTERVAL_NS)
    }

    /// Register a press against an explicit `interval_ns` window — the one
    /// definition [`register`](Self::register) is a default-interval spelling
    /// of, so the pairing rule has a single home. Exposed so a caller (or a
    /// test) may choose a different window without a second detector.
    #[must_use]
    pub fn register_within(
        &mut self,
        now_ns: u64,
        index: usize,
        button: PointerButton,
        interval_ns: u64,
    ) -> ClickKind {
        if let Some(prev) = self.last {
            // Pair only a press of the same button on the same item that
            // follows the previous one within the window. `now_ns >=
            // prev.at_ns` guards a non-monotonic reading (a clock that
            // appeared to step back): such a press fails closed to a fresh
            // single rather than a spurious double.
            if prev.index == index
                && prev.button == button
                && now_ns >= prev.at_ns
                && now_ns - prev.at_ns <= interval_ns
            {
                self.last = None;
                return ClickKind::Double;
            }
        }
        self.last = Some(LastClick {
            at_ns: now_ns,
            index,
            button,
        });
        ClickKind::Single
    }

    /// Forget any remembered press, so the next press starts a fresh single.
    ///
    /// The caller resets when an intervening interaction breaks the pair — a
    /// press that lands on chrome (a toolbar tool, the places rail) rather
    /// than an item — so a click *through* the chrome and back onto the same
    /// item is never mistaken for a double-click of that item.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{ClickKind, DoubleClickTracker, PointerButton, DOUBLE_CLICK_INTERVAL_NS};

    /// The primary button, which every pre-existing case below presses.
    const LEFT: PointerButton = PointerButton::Primary;
    const RIGHT: PointerButton = PointerButton::Secondary;

    #[test]
    fn a_lone_press_is_a_single_click() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 3, LEFT), ClickKind::Single);
    }

    #[test]
    fn two_quick_presses_on_the_same_item_are_a_double_click() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(1_000, 3, LEFT), ClickKind::Single);
        assert_eq!(
            tracker.register(1_000 + DOUBLE_CLICK_INTERVAL_NS / 2, 3, LEFT),
            ClickKind::Double
        );
    }

    #[test]
    fn a_press_exactly_at_the_interval_still_pairs() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 0, LEFT), ClickKind::Single);
        assert_eq!(
            tracker.register(DOUBLE_CLICK_INTERVAL_NS, 0, LEFT),
            ClickKind::Double
        );
    }

    #[test]
    fn a_slow_second_press_is_a_fresh_single_not_a_double() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 3, LEFT), ClickKind::Single);
        assert_eq!(
            tracker.register(DOUBLE_CLICK_INTERVAL_NS + 1, 3, LEFT),
            ClickKind::Single
        );
    }

    #[test]
    fn a_quick_press_on_a_different_item_is_a_single() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 3, LEFT), ClickKind::Single);
        assert_eq!(tracker.register(1, 4, LEFT), ClickKind::Single);
    }

    #[test]
    fn a_double_click_consumes_both_presses() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 3, LEFT), ClickKind::Single);
        assert_eq!(tracker.register(1, 3, LEFT), ClickKind::Double);
        // A third quick press begins a fresh single, never a second double.
        assert_eq!(tracker.register(2, 3, LEFT), ClickKind::Single);
        assert_eq!(tracker.register(3, 3, LEFT), ClickKind::Double);
    }

    #[test]
    fn a_reset_breaks_the_pair() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 3, LEFT), ClickKind::Single);
        tracker.reset();
        // Without the remembered first press, the next is a lone single even
        // on the same item within the window.
        assert_eq!(tracker.register(1, 3, LEFT), ClickKind::Single);
    }

    #[test]
    fn a_backwards_clock_reading_fails_closed_to_a_single() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(10_000, 3, LEFT), ClickKind::Single);
        // A reading before the remembered press must not pair (it would
        // otherwise underflow the interval test); it is a fresh single.
        assert_eq!(tracker.register(9_000, 3, LEFT), ClickKind::Single);
        // And that fresh press is now the remembered one: a proper follow-up
        // pairs against it.
        assert_eq!(tracker.register(9_500, 3, LEFT), ClickKind::Double);
    }

    #[test]
    fn a_custom_interval_is_honoured() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register_within(0, 1, LEFT, 100), ClickKind::Single);
        assert_eq!(tracker.register_within(50, 1, LEFT, 100), ClickKind::Double);
        assert_eq!(
            tracker.register_within(200, 1, LEFT, 100),
            ClickKind::Single
        );
        assert_eq!(
            tracker.register_within(400, 1, LEFT, 100),
            ClickKind::Single
        );
    }

    #[test]
    fn the_two_buttons_pair_independently_and_never_with_each_other() {
        let mut tracker = DoubleClickTracker::new();
        // A left press then a right press on the same item is two gestures
        // begun, not one completed.
        assert_eq!(tracker.register(0, 3, LEFT), ClickKind::Single);
        assert_eq!(tracker.register(1, 3, RIGHT), ClickKind::Single);
        // The right press is now the remembered one, so the right pair
        // completes...
        assert_eq!(tracker.register(2, 3, RIGHT), ClickKind::Double);
        // ...and the left run was broken by it rather than left pending.
        assert_eq!(tracker.register(3, 3, LEFT), ClickKind::Single);
        assert_eq!(tracker.register(4, 3, LEFT), ClickKind::Double);
    }

    #[test]
    fn a_right_double_click_on_a_different_item_is_a_single() {
        let mut tracker = DoubleClickTracker::new();
        assert_eq!(tracker.register(0, 3, RIGHT), ClickKind::Single);
        assert_eq!(tracker.register(1, 4, RIGHT), ClickKind::Single);
    }
}
