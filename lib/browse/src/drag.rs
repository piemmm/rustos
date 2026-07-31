//! Bundle-drag detection: the one pure rule that turns the file manager's
//! primary presses, pointer motion, and release/cancel events into the
//! window-channel drag verbs — offer exactly once, withdraw on cancel
//! (`plans/NEW-TASKBAR.md` T7).
//!
//! Dragging an application bundle out of the browser is how a user pins it
//! by hand: the app *offers* the bundle's path over its window channel when
//! a pressed pointer has travelled beyond [`DRAG_THRESHOLD_PX`], and the
//! desktop session resolves what a later drop over the taskbar means. The
//! decision of *when* a press becomes a drag lives here, once, so the offer
//! can never fire twice for one gesture and a plain click (press, negligible
//! motion, release) never becomes a drag at all.
//!
//! The detector holds no authority and does no I/O. It decides only whether
//! this motion crosses the threshold of an armed press; the caller supplies
//! the row classification (only a bundle row arms — pinning names an
//! installed application) and window-local pointer positions, and performs
//! the offer/withdraw calls itself over its own window channel. A refused
//! offer is reported back ([`offer_failed`](BundleDrag::offer_failed)) so a
//! later cancel never withdraws an offer the session refused.

use tairix_geometry::Point;

/// How far, in physical pixels, a pressed pointer must travel before the
/// press becomes a drag offer.
///
/// A deliberate, fixed UX convenience bound, not a hardware-scaled capacity:
/// large enough that the natural jitter of a click never turns it into a
/// drag, small enough that a real drag offers before the pointer leaves the
/// row. Physical pixels because the window channel's pointer events carry
/// them; crossing the bound never fails anything — motion within it is
/// simply still a click.
pub const DRAG_THRESHOLD_PX: u64 = 6;

/// Where one drag gesture currently stands.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
enum DragState {
    /// No gesture in progress: presses are watched, nothing is offered.
    #[default]
    Idle,
    /// A primary press landed on a bundle row and has not travelled beyond
    /// the threshold yet: motion is measured against the press point.
    Armed {
        /// The listing index of the pressed bundle row.
        index: usize,
        /// The window-local press point motion is measured from.
        press: Point,
    },
    /// The offer for this gesture has been sent: no further offer may fire
    /// until the gesture ends (release, cancel, or a refused offer).
    Offered,
}

/// The pure bundle-drag detector: it remembers the armed press and reports
/// when its motion crosses the threshold — exactly once per gesture.
///
/// The caller drives it from its pointer and key events: a primary press
/// [`press`](Self::press)es (arming only a bundle row), pointer motion asks
/// [`motion`](Self::motion) whether to send the offer now, a primary release
/// [`release`](Self::release)s (the drop itself is the desktop session's to
/// resolve), and `Escape` asks [`cancel`](Self::cancel) whether an offer is
/// outstanding to withdraw. A refused offer is handed back through
/// [`offer_failed`](Self::offer_failed) so the gesture dies silently without
/// a stray withdraw.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BundleDrag {
    state: DragState,
}

impl BundleDrag {
    /// A fresh detector with no gesture in progress.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: DragState::Idle,
        }
    }

    /// Register a primary press on the row at `index` at window-local `at`.
    ///
    /// Only a bundle row arms — pinning names an installed application, so a
    /// press on anything else (or off the listing) ends any gesture instead:
    /// a fresh press always supersedes whatever came before it, so a stale
    /// armed press can never carry over into a new gesture.
    pub fn press(&mut self, index: usize, is_bundle: bool, at: Point) {
        self.state = if is_bundle {
            DragState::Armed { index, press: at }
        } else {
            DragState::Idle
        };
    }

    /// Register pointer motion to window-local `to`, returning the armed
    /// row's index exactly when the offer should be sent now — the first
    /// motion beyond [`DRAG_THRESHOLD_PX`] from the armed press point —
    /// and `None` otherwise.
    ///
    /// Returning the index consumes the arming: the gesture moves to its
    /// offered state, so no second offer can fire until it ends. Motion with
    /// nothing armed (or after the offer) is `None`.
    #[must_use]
    pub fn motion(&mut self, to: Point) -> Option<usize> {
        let DragState::Armed { index, press } = self.state else {
            return None;
        };
        if !beyond_threshold(press, to) {
            return None;
        }
        self.state = DragState::Offered;
        Some(index)
    }

    /// Register a primary release: the gesture ends locally whatever its
    /// state. What a release *means* for an offered drag (a drop, or a
    /// no-op) is the desktop session's decision — the release event reaches
    /// it as well, so the app only stops tracking.
    pub fn release(&mut self) {
        self.state = DragState::Idle;
    }

    /// Cancel the gesture (`Escape`), returning `true` exactly when an offer
    /// is outstanding and the caller must withdraw it — an armed-but-unsent
    /// press just disarms, and cancelling with no gesture is a no-op.
    #[must_use]
    pub fn cancel(&mut self) -> bool {
        let withdraw = self.state == DragState::Offered;
        self.state = DragState::Idle;
        withdraw
    }

    /// Record that the offer this gesture sent was refused (or could not be
    /// sent): the gesture ends silently, so a later [`cancel`](Self::cancel)
    /// never withdraws an offer the session does not hold.
    pub fn offer_failed(&mut self) {
        self.state = DragState::Idle;
    }
}

/// Whether `to` lies strictly beyond [`DRAG_THRESHOLD_PX`] of `press`, by
/// Euclidean distance.
///
/// Either axis alone exceeding the threshold is already beyond it, which
/// keeps the remaining squared comparison within `u64` — no coordinate pair
/// the wire can carry overflows it.
fn beyond_threshold(press: Point, to: Point) -> bool {
    let dx = u64::from(to.x.abs_diff(press.x));
    let dy = u64::from(to.y.abs_diff(press.y));
    dx > DRAG_THRESHOLD_PX
        || dy > DRAG_THRESHOLD_PX
        || dx * dx + dy * dy > DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX
}

#[cfg(test)]
mod tests {
    use super::{BundleDrag, DRAG_THRESHOLD_PX};
    use tairix_geometry::Point;

    fn threshold() -> i32 {
        // The bound is tiny; the cast cannot lose value.
        i32::try_from(DRAG_THRESHOLD_PX).unwrap_or(i32::MAX)
    }

    #[test]
    fn a_press_on_a_bundle_row_arms_and_far_motion_offers_that_row() {
        let mut drag = BundleDrag::new();
        drag.press(4, true, Point::new(100, 100));
        assert_eq!(drag.motion(Point::new(100 + threshold() + 1, 100)), Some(4));
    }

    #[test]
    fn a_press_on_a_non_bundle_row_never_arms() {
        let mut drag = BundleDrag::new();
        drag.press(2, false, Point::new(100, 100));
        assert_eq!(drag.motion(Point::new(300, 300)), None);
    }

    #[test]
    fn a_non_bundle_press_supersedes_an_armed_press() {
        let mut drag = BundleDrag::new();
        drag.press(4, true, Point::new(100, 100));
        // The next press lands on a plain file: the earlier arming must not
        // survive into this new gesture.
        drag.press(5, false, Point::new(100, 100));
        assert_eq!(drag.motion(Point::new(300, 300)), None);
    }

    #[test]
    fn motion_within_the_threshold_does_not_offer_but_crossing_later_does() {
        let mut drag = BundleDrag::new();
        drag.press(1, true, Point::new(50, 50));
        // Exactly at the threshold is not beyond it: still a click.
        assert_eq!(drag.motion(Point::new(50 + threshold(), 50)), None);
        assert_eq!(drag.motion(Point::new(50, 50 - threshold())), None);
        // One pixel further is a drag.
        assert_eq!(drag.motion(Point::new(50 + threshold() + 1, 50)), Some(1));
    }

    #[test]
    fn the_threshold_is_euclidean_not_per_axis() {
        let mut drag = BundleDrag::new();
        drag.press(3, true, Point::new(0, 0));
        // 5² + 5² = 50 > 36: beyond the bound although each axis is within it.
        assert_eq!(drag.motion(Point::new(5, 5)), Some(3));
        // 4² + 4² = 32 ≤ 36: still a click.
        let mut close = BundleDrag::new();
        close.press(3, true, Point::new(0, 0));
        assert_eq!(close.motion(Point::new(4, -4)), None);
    }

    #[test]
    fn exactly_one_offer_fires_per_gesture() {
        let mut drag = BundleDrag::new();
        drag.press(7, true, Point::new(10, 10));
        assert_eq!(drag.motion(Point::new(30, 30)), Some(7));
        // Further motion in the same gesture must not offer again.
        assert_eq!(drag.motion(Point::new(60, 60)), None);
        assert_eq!(drag.motion(Point::new(10, 10)), None);
    }

    #[test]
    fn a_release_ends_the_gesture_locally() {
        let mut drag = BundleDrag::new();
        drag.press(2, true, Point::new(10, 10));
        drag.release();
        // The armed press died with the release: later motion offers nothing.
        assert_eq!(drag.motion(Point::new(200, 200)), None);
        // A fresh press starts a fresh gesture.
        drag.press(2, true, Point::new(10, 10));
        assert_eq!(drag.motion(Point::new(200, 200)), Some(2));
    }

    #[test]
    fn cancel_withdraws_only_an_outstanding_offer() {
        let mut drag = BundleDrag::new();
        // Nothing in progress: no withdraw.
        assert!(!drag.cancel());
        // Armed but not offered: the press just disarms.
        drag.press(1, true, Point::new(0, 0));
        assert!(!drag.cancel());
        assert_eq!(drag.motion(Point::new(100, 100)), None);
        // Offered: the caller must withdraw, and only once.
        drag.press(1, true, Point::new(0, 0));
        assert_eq!(drag.motion(Point::new(100, 100)), Some(1));
        assert!(drag.cancel());
        assert!(!drag.cancel());
    }

    #[test]
    fn a_refused_offer_dies_silently_without_a_withdraw() {
        let mut drag = BundleDrag::new();
        drag.press(6, true, Point::new(0, 0));
        assert_eq!(drag.motion(Point::new(100, 100)), Some(6));
        drag.offer_failed();
        // The session holds no offer, so a cancel must not withdraw one.
        assert!(!drag.cancel());
        assert_eq!(drag.motion(Point::new(200, 200)), None);
    }
}
