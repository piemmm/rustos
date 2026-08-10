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

/// Write `value` into `field`, reporting `bounds` when that changed it.
///
/// The comparison is the whole rule: a write that lands the value already
/// there changes no pixel and must report nothing, or a host would repaint on
/// every idle motion sample.
pub(crate) fn set<T: PartialEq>(field: &mut T, value: T, bounds: Rect, damage: &mut Region) {
    if *field == value {
        return;
    }
    *field = value;
    damage.add(bounds);
}
