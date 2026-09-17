//! The playpen: where Cinder lives when he is not out on the desktop.
//!
//! The pen owns whether Cinder is *in* or *out*, the furniture he interacts
//! with, and the petting and dragging the user does with the pointer. It does
//! not own the desktop — that is `world` — and it does not decide what he
//! wants — that is `mind`.

use alloc::string::String;

use tairix_controls::{Button, ButtonAction, ButtonContent, ControlRole};
use tairix_geometry::{Point, Rect, Region};

use crate::layout::PenLayout;
use crate::project::Ground;

/// Where Cinder is.
///
/// A closed set with no "unknown": the pen always knows, because *it* is what
/// lets him out and takes him back.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Whereabouts {
    /// In the pen.
    Inside,
    /// Out on the desktop.
    Loose,
}

/// Why letting Cinder out was refused.
///
/// A refusal is an answer the pen states and carries on from, never a reason
/// to end the application.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The account's ceiling does not carry the desktop-layer capability.
    NotPermitted,
    /// There is no graphical session to be loose on.
    NoDesktop,
    /// The seat already has as many companions as it allows.
    SeatFull,
}

impl Refusal {
    /// The one-line reason, for `stderr` and for the pen's own readout.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotPermitted => "not allowed out: this account may not place desktop companions",
            Self::NoDesktop => "not allowed out: no desktop session to be loose on",
            Self::SeatFull => "not allowed out: the desktop already has its companions",
        }
    }
}

/// What a pointer press in the pen did.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PenAction {
    /// Nothing under the press.
    Nothing,
    /// Cinder was petted.
    Petted,
    /// The toy was batted from the press point.
    ToyBatted,
    /// A drag of Cinder began.
    DragBegan,
    /// The strip's button was activated: let him out, or bring him home.
    Toggled,
}

/// The playpen's own state.
pub struct Pen {
    whereabouts: Whereabouts,
    /// Where Cinder stands inside the pen, in client pixels.
    at: Ground,
    /// Where the toy has rolled to, in client pixels.
    toy: Point,
    /// The drag in flight, if any: the offset from the press to his feet, so
    /// he does not jump to the cursor when the drag starts.
    ///
    /// Held in the same fractional pixels his position is, because rounding
    /// it would nudge him by up to half a pixel the instant a drag began —
    /// a press that has not moved must move nothing at all.
    dragging: Option<(f64, f64)>,
    /// The last refusal, shown in the pen until something else happens.
    refusal: Option<Refusal>,
    /// The strip's one control, which is how a user actually lets him out:
    /// the icon-bar menu is not where anyone looks for an application's main
    /// action.
    button: Button,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            whereabouts: Whereabouts::Inside,
            at: Ground::new(0.0, 0.0),
            toy: Point::new(0, 0),
            dragging: None,
            refusal: None,
            button: Button::new(
                ButtonContent::Label(String::from(LET_OUT_LABEL)),
                ControlRole::Primary,
            ),
        }
    }
}

/// The button's label while Cinder is in the pen.
pub const LET_OUT_LABEL: &str = "Let Cinder out";

/// The button's label while he is out on the desktop.
pub const BRING_HOME_LABEL: &str = "Bring Cinder home";

/// How close a press must be to Cinder's feet, in client pixels, to reach
/// him.
///
/// Generous, because a companion is a thing you reach for rather than a
/// control you aim at.
pub const PET_RADIUS: f64 = 34.0;

impl Pen {
    /// A fresh pen with Cinder inside.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Where Cinder is.
    #[must_use]
    pub const fn whereabouts(&self) -> Whereabouts {
        self.whereabouts
    }

    /// Where he stands inside the pen.
    #[must_use]
    pub const fn at(&self) -> Ground {
        self.at
    }

    /// Where the toy has rolled to.
    #[must_use]
    pub const fn toy(&self) -> Point {
        self.toy
    }

    /// The refusal to show, if the last attempt to let him out failed.
    #[must_use]
    pub const fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }

    /// The strip's button, for the painter.
    #[must_use]
    pub const fn button(&self) -> &Button {
        &self.button
    }

    /// Whether a drag is in flight.
    #[must_use]
    pub const fn is_dragging(&self) -> bool {
        self.dragging.is_some()
    }

    /// Put Cinder back where the layout says he starts, and the toy with him.
    ///
    /// Called when the pen is first laid out and whenever it is resized, so a
    /// resize never leaves him standing outside his own floor.
    pub fn settle(&mut self, layout: &PenLayout) {
        self.at = floor_centre(layout);
        self.toy = Point::new(layout.toy.left(), layout.toy.top());
    }

    /// Let Cinder out, recording `refusal` if the desktop would not have him.
    ///
    /// Answers whether he actually got out. A refusal is stated and the pen
    /// carries on: closing over it, or ending the application over it, would
    /// be a program dying of a denied optional action.
    pub fn let_out(&mut self, refusal: Option<Refusal>) -> bool {
        self.refusal = refusal;
        if refusal.is_some() {
            return false;
        }
        self.whereabouts = Whereabouts::Loose;
        self.dragging = None;
        self.relabel();
        true
    }

    /// Put the button's label back in step with where Cinder is.
    fn relabel(&mut self) {
        let label = match self.whereabouts {
            Whereabouts::Inside => LET_OUT_LABEL,
            Whereabouts::Loose => BRING_HOME_LABEL,
        };
        self.button = Button::new(
            ButtonContent::Label(String::from(label)),
            ControlRole::Primary,
        );
    }

    /// Bring Cinder back into the pen.
    pub fn put_away(&mut self, layout: &PenLayout) {
        self.whereabouts = Whereabouts::Inside;
        self.refusal = None;
        self.at = floor_centre(layout);
        self.relabel();
    }

    /// Feed the strip's button a pointer event, answering whether it fired.
    ///
    /// Taken before the pen's own handling so a press on the control is never
    /// also a press on the floor behind it.
    pub fn button_pointer(
        &mut self,
        event: &tairix_input::InputEvent,
        layout: &PenLayout,
        damage: &mut Region,
    ) -> Option<PenAction> {
        let fired = self.button.on_pointer(event, layout.button, damage);
        matches!(fired, Some(ButtonAction::Activated)).then_some(PenAction::Toggled)
    }

    /// Handle a press at window-local `point`.
    pub fn press(&mut self, point: Point, layout: &PenLayout) -> PenAction {
        self.refusal = None;
        // The strip belongs to the control, never to the floor.
        if layout.strip.contains(point) {
            return PenAction::Nothing;
        }
        if self.whereabouts == Whereabouts::Inside && self.reaches(point) {
            self.dragging = Some((
                f64::from(point.x) - self.at.x,
                f64::from(point.y) - self.at.y,
            ));
            return PenAction::DragBegan;
        }
        if layout.toy.contains(point) {
            self.bat_toy(point, layout);
            return PenAction::ToyBatted;
        }
        PenAction::Nothing
    }

    /// Handle a motion to window-local `point`, answering whether anything
    /// moved.
    pub fn motion(&mut self, point: Point, layout: &PenLayout) -> bool {
        let Some((dx, dy)) = self.dragging else {
            return false;
        };
        let wanted = Ground::new(f64::from(point.x) - dx, f64::from(point.y) - dy);
        let settled = clamp_to_floor(wanted, layout);
        let moved = settled != self.at;
        self.at = settled;
        moved
    }

    /// Handle a release at window-local `point`, answering what it did.
    ///
    /// A release that never moved is a pet rather than a drag: picking a
    /// creature up and putting it straight back down is how one strokes it.
    pub fn release(&mut self, point: Point, layout: &PenLayout) -> PenAction {
        let Some((dx, dy)) = self.dragging.take() else {
            return PenAction::Nothing;
        };
        let wanted = Ground::new(f64::from(point.x) - dx, f64::from(point.y) - dy);
        self.at = clamp_to_floor(wanted, layout);
        if self.reaches(point) {
            PenAction::Petted
        } else {
            PenAction::Nothing
        }
    }

    /// Whether `point` is close enough to Cinder's feet to reach him.
    fn reaches(&self, point: Point) -> bool {
        let dx = f64::from(point.x) - self.at.x;
        let dy = f64::from(point.y) - self.at.y;
        dx * dx + dy * dy <= PET_RADIUS * PET_RADIUS
    }

    /// Send the toy rolling away from `from`.
    fn bat_toy(&mut self, from: Point, layout: &PenLayout) {
        // Away from the press and along the floor, so a bat looks like a bat
        // rather than a teleport.
        let dx = layout.toy.left() - from.x;
        let step = if dx >= 0 { TOY_BAT } else { -TOY_BAT };
        let x = (self.toy.x + step).clamp(
            layout.floor.left(),
            layout
                .floor
                .right()
                .saturating_sub(i32::try_from(layout.toy.width).unwrap_or(0)),
        );
        self.toy = Point::new(x, self.toy.y);
    }
}

/// How far one bat sends the toy, in client pixels.
const TOY_BAT: i32 = 36;

/// The middle of the pen's floor.
fn floor_centre(layout: &PenLayout) -> Ground {
    Ground::new(
        f64::from(layout.floor.left()) + f64::from(layout.floor.width) / 2.0,
        f64::from(layout.floor.top()) + f64::from(layout.floor.height) * 0.7,
    )
}

/// `at` pulled back onto the floor band.
fn clamp_to_floor(at: Ground, layout: &PenLayout) -> Ground {
    let floor: Rect = layout.floor;
    Ground::new(
        at.x.clamp(
            f64::from(floor.left()),
            f64::from(floor.right().saturating_sub(1)),
        ),
        at.y.clamp(
            f64::from(floor.top()),
            f64::from(floor.bottom().saturating_sub(1)),
        ),
    )
}

#[cfg(test)]
#[path = "pen_tests.rs"]
mod tests;
