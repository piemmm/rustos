//! Carrying the pointer across a model refresh.
//!
//! A refresh re-derives a section's controls from a fresh sample, but the
//! pointer has not moved and neither have the slots the controls sit in, so
//! the reader is still over whatever they were over. Controls left resting
//! would state the opposite, and would state it again on every sample — at
//! compositor-frame rate that is a highlight flickering under a moving
//! pointer. The two rules that put it right live here, once, for every
//! section: keep the live control where the refresh derived the same one, and
//! carry only the hover where it did not.

use alloc::vec::Vec;
use core::mem;

use tairix_controls::{ActionRail, Button, Card, ControlState, ListRow, PointerState, TableRow};

/// A control carrying a pointer highlight a refresh can restate.
pub(super) trait Pointed {
    /// Where this control currently states the pointer is.
    fn pointer(&self) -> PointerState;

    /// Restate where the pointer is, leaving the rest of the state alone.
    fn set_pointer(&mut self, pointer: PointerState);
}

/// Delegate [`Pointed`] to the composed state each control already exposes,
/// rather than restating the same two lines per control.
macro_rules! pointed {
    ($($control:ty),+ $(,)?) => {
        $(impl Pointed for $control {
            fn pointer(&self) -> PointerState {
                <$control>::state(self).pointer
            }

            fn set_pointer(&mut self, pointer: PointerState) {
                let state = <$control>::state(self).with_pointer(pointer);
                <$control>::set_state(self, state);
            }
        })+
    };
}

pointed!(Button, ListRow, TableRow);

/// Carry a hover from the control a slot held onto the one that replaced it.
///
/// Hover states where the *pointer* is, and the pointer has not moved, so it
/// is genuinely over the fresh control at the same slot. A press is not
/// carried: a press latch names the object it began on, and the slot may now
/// hold a different one.
pub(super) fn carry_hover_one<T: Pointed>(was: &T, now: &mut T) {
    if was.pointer() == PointerState::Hover {
        now.set_pointer(PointerState::Hover);
    }
}

/// Carry every slot's hover onto the control that replaced it.
///
/// A slot the refresh dropped carries nothing — the pointer is over no
/// control there.
pub(super) fn carry_hover<'a, T: Pointed + 'a>(
    was: impl IntoIterator<Item = &'a T>,
    now: impl IntoIterator<Item = &'a mut T>,
) {
    for (was, now) in was.into_iter().zip(now) {
        carry_hover_one(was, now);
    }
}

/// Re-settle a refreshed card onto the slot a live one holds.
///
/// A card draws no pointer highlight of its own; the commands in its footer do,
/// and the card keeps its own record of which one the pointer is on. That
/// record cannot be restated from outside, so forcing a hover onto a *fresh*
/// card's footer command would leave a highlight the card can never clear.
/// Where the refresh derived the same card again the live one is therefore kept
/// whole, records and all — the common case, since a frame report arrives far
/// more often than the readings move.
///
/// A card whose footer holds a press is never kept: a footer action names the
/// slot it was pressed on and the refresh may have filed a different object
/// there, so the press must not complete against whatever took the slot. A card
/// the refresh genuinely changed rests its footer and waits for the pointer's
/// next motion, which the reader's own movement supplies.
pub(super) fn resettle_card(mut live: Card, fresh: &mut Card) {
    if latched(&live) {
        return;
    }
    wear_screen_marks(&live, fresh);
    if *fresh == live {
        mem::swap(fresh, &mut live);
        return;
    }
    strip_screen_marks(fresh);
}

/// Re-settle each refreshed card against the live one whose slot it took.
pub(super) fn resettle_cards(live: Vec<Card>, fresh: &mut [Card]) {
    for (live, fresh) in live.into_iter().zip(fresh.iter_mut()) {
        resettle_card(live, fresh);
    }
}

/// Whether one of `card`'s footer commands holds a press or a drag — a latch
/// that names the object it began on rather than where the pointer is.
fn latched(card: &Card) -> bool {
    card.footer()
        .iter()
        .any(|button| !matches!(button.pointer(), PointerState::None | PointerState::Hover))
}

/// `derived` wearing the marks the *screen* puts on a control rather than the
/// refresh: where the pointer is, and the keyboard marks the screen re-asserts
/// over whatever controls the sections hold once every section has adopted.
/// Neither makes a control a different control.
fn with_screen_marks(derived: ControlState, screen: ControlState) -> ControlState {
    ControlState {
        pointer: screen.pointer,
        focus: screen.focus,
        ..derived
    }
}

/// Dress a freshly derived card and its footer in the live one's screen marks,
/// so what is left to compare is what the refresh actually derived.
fn wear_screen_marks(live: &Card, fresh: &mut Card) {
    fresh.set_state(with_screen_marks(fresh.state(), live.state()));
    for (live, fresh) in live.footer().iter().zip(fresh.footer_mut()) {
        fresh.set_state(with_screen_marks(fresh.state(), live.state()));
    }
}

/// Undo [`wear_screen_marks`], leaving the unmarked card the refresh derived.
fn strip_screen_marks(fresh: &mut Card) {
    fresh.set_state(with_screen_marks(fresh.state(), ControlState::idle()));
    for button in fresh.footer_mut() {
        button.set_state(with_screen_marks(button.state(), ControlState::idle()));
    }
}

/// Restate a rail's commands in place, replacing the rail only when the
/// refresh derived different commands.
///
/// A rail keeps its own record of which command the pointer is on and which
/// one is holding a press, and neither can be restated from outside — so a
/// replaced rail drops the reader's highlight and swallows a press begun
/// before the refresh landed. The commands' *states* are the refresh's to
/// say; where the commands themselves changed there is nothing to keep and
/// the rail takes the fresh ones.
///
/// A press does survive here, unlike on a card: a rail commands whichever
/// object is *selected*, and the selection is re-resolved by identity across a
/// refresh, so the command completes against the object it was pressed for
/// however far that object has moved in the list.
pub(super) fn restate_rail(rail: &mut ActionRail, fresh: Vec<Button>) {
    let focus = rail.focus();
    if same_commands(rail.items(), &fresh) {
        for (live, derived) in rail.items_mut().iter_mut().zip(fresh) {
            let pointer = live.state().pointer;
            live.set_state(derived.state().with_pointer(pointer));
        }
    } else {
        *rail = ActionRail::new(fresh);
    }
    rail.adopt_focus(focus);
}

/// Whether two runs name the same commands in the same order — the identity
/// a rail's own records are about.
fn same_commands(live: &[Button], fresh: &[Button]) -> bool {
    live.len() == fresh.len()
        && live
            .iter()
            .zip(fresh)
            .all(|(live, fresh)| live.content() == fresh.content() && live.role() == fresh.role())
}

#[cfg(test)]
#[path = "refresh_tests.rs"]
mod tests;
