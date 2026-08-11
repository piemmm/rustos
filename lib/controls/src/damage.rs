//! The damage sink a control reports its own repainted pixels into.
//!
//! A control is host-composed: it owns no tree, and the host's only signal
//! that anything changed is the render-equivalence comparison every family
//! derives, which fails for the whole surface as soon as one control's hover
//! flips. The sink is the missing seam. An input or update call takes it, the
//! control pushes *its own* bounds when a drawn state field changes, and the
//! host renders and presents only what came back.
//!
//! Reporting goes through one guarded write inside the crate, so no family
//! grows its own idea of when a change is worth a repaint. Hit-testing
//! bookkeeping — the last pointer position, a press latch — is not a drawn
//! field and reports nothing, the same rule that keeps it out of the render
//! comparison.

use tairix_geometry::{Rect, Region};

/// The rectangle budget of a control damage sink.
///
/// A host pays for a reported rectangle twice: once to re-render clipped to
/// it, once to present it. The compositor already refuses more than eight
/// present round trips for one frame and collapses anything past that to a
/// single bounding box, so a ninth rectangle can never buy its own present —
/// it would only add another clip-and-render walk for pixels the present is
/// going to carry anyway.
///
/// Eight is also comfortably above what one routed pointer event can produce:
/// it reaches at most the child the pointer left, the child it entered, a
/// child holding a press, and the container's own chrome — four. So an
/// interactive frame stays exact to the control that changed, while a
/// whole-model refresh degrades to the one box it may as well have been.
const BUDGET: usize = 8;

/// A fresh, empty sink for one round of input or updates.
#[must_use]
pub fn sink() -> Region {
    Region::with_budget(BUDGET)
}

/// Write `value` into `field`, reporting `bounds` when that changed it, and
/// return whether it did.
///
/// The comparison is the whole rule: a write that lands the value already
/// there changes no pixel and must report nothing, or a host would repaint on
/// every idle motion sample. The answer is returned because a family whose
/// action is "the value moved" — a slider step, a scroll request — must not
/// compare a second time to learn what this write already decided.
pub(crate) fn set<T: PartialEq>(
    field: &mut T,
    value: T,
    bounds: Rect,
    damage: &mut Region,
) -> bool {
    if *field == value {
        return false;
    }
    *field = value;
    damage.add(bounds);
    true
}

/// Report the two children an index-valued mark moves between — the highlighted
/// menu row, the hovered tab, the focused crumb — and answer whether it moved.
///
/// A container draws such a mark on one child at a time, so the two rectangles
/// that change are the child it leaves and the child it arrives on, never the
/// strip or popup they sit in. `rect_of` names a child's rectangle in the
/// container's current layout; a child that lays out nowhere reports nothing.
///
/// The caller performs the write itself, because `rect_of` resolves the
/// rectangles from the container while the mark is a field of that same
/// container: taking the field by mutable reference here would conflict with the
/// layout the rectangles come from.
pub(crate) fn move_mark(
    mark: Option<usize>,
    next: Option<usize>,
    rect_of: impl Fn(usize) -> Option<Rect>,
    damage: &mut Region,
) -> bool {
    if mark == next {
        return false;
    }
    for index in [mark, next].into_iter().flatten() {
        if let Some(rect) = rect_of(index) {
            damage.add(rect);
        }
    }
    true
}
