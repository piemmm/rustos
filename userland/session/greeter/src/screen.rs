//! The login screen: the surface, the frame it goes into, the lockout it
//! presents, and the authority behind it.
//!
//! Everything about *what the screen does* lives here, so the whole flow —
//! a keystroke reaching a verdict, a refusal becoming a countdown, an idle
//! screen arming no timer — is exercised on the host. What the `Run` binary
//! adds is only where the events and the pixels come from.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::input::PointerInput;
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::window_ipc::PointerAction;
use tairix_cursor::{CursorImage, PlacedCursor};
use tairix_geometry::{Rect, Scale};
use tairix_greeter::{
    panel_rect, scrim_alpha, AccountTile, AuthSurface, Backdrop, EventContext, Outcome,
};
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;
use tairix_window::pointer_input_events;

use crate::accounts::SessionTransport;
use crate::chrome::chrome;
use crate::cursor::Cursor;
use crate::frame::{Present, Scanout};
use crate::verify::{Answer, SessionVerifier};
use crate::wait::{park_timeout, Cooldown};

/// What one round of the screen did.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Step {
    /// What to hand the display, if anything.
    pub present: Present,
    /// Whether a secret was verified. The screen is finished: the authority
    /// is watching for the exit and starts the session itself.
    pub verified: bool,
    /// The authority's answer, when one came back this round.
    pub answer: Option<Answer>,
}

impl Step {
    /// Nothing happened.
    const fn quiet() -> Self {
        Self {
            present: Present::Nothing,
            verified: false,
            answer: None,
        }
    }
}

/// The graphical login screen.
///
/// It draws and types; it never decides. Every question about a secret goes
/// out over `session-v1` and comes back as one of three answers, and only a
/// verified one finishes the screen. A refusal, an unreachable authority, an
/// empty account list, and a lockout running out are all "still asking" —
/// none of them exits, so a transient fault cannot spend the authority's
/// restart budget.
pub struct LoginScreen<T: SessionTransport> {
    surface: AuthSurface,
    scanout: Scanout,
    cooldown: Cooldown,
    verifier: SessionVerifier<T>,
    theme: Theme,
    scale: Scale,
    host: String,
    wallpaper: Option<Wallpaper>,
    cursor: Cursor,
    /// The pointer artwork, once one has been installed. A screen whose
    /// cursor would not rasterise keeps hit-testing and typing with nothing
    /// drawn, which is a missing pointer rather than a broken login.
    pointer: Option<PlacedCursor>,
}

/// A decoded, screen-fitted wallpaper and the scrim sized for it.
struct Wallpaper {
    image: Surface,
    scrim: u8,
}

/// A repaint request: whether to paint, and which pixels change.
///
/// Two updates can land in one round — a keystroke and the lockout that
/// answered it — and the surface reports each separately, so they are
/// combined here. A whole-screen damage from either side stays whole: one
/// part of a paint changing everything is not narrowed by another changing a
/// rectangle.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Repaint {
    redraw: bool,
    damage: Option<Rect>,
}

impl Repaint {
    /// Nothing to paint.
    const QUIET: Self = Self {
        redraw: false,
        damage: None,
    };

    fn of(outcome: Outcome) -> Self {
        Self {
            redraw: outcome.redraw(),
            damage: outcome.damage(),
        }
    }

    /// Paint exactly `rect`.
    const fn region(rect: Rect) -> Self {
        Self {
            redraw: true,
            damage: Some(rect),
        }
    }

    fn merged(self, other: Self) -> Self {
        if !other.redraw {
            return self;
        }
        if !self.redraw {
            return other;
        }
        let damage = match (self.damage, other.damage) {
            (Some(mine), Some(theirs)) => Some(mine.union(&theirs)),
            _ => None,
        };
        Self {
            redraw: true,
            damage,
        }
    }
}

impl<T: SessionTransport> LoginScreen<T> {
    /// A screen offering `accounts`, painted into `scanout`.
    ///
    /// An empty list is not an error: the chooser always carries its
    /// typed-name tile, so a machine whose account directory could not be
    /// read is still one a user can log into.
    pub fn new(
        scanout: Scanout,
        theme: Theme,
        scale: Scale,
        host: String,
        accounts: Vec<AccountTile>,
        transport: T,
    ) -> Self {
        let cursor = Cursor::centred(scanout.mode());
        Self {
            surface: AuthSurface::with_accounts(accounts),
            scanout,
            cooldown: Cooldown::default(),
            verifier: SessionVerifier::new(transport),
            theme,
            scale,
            host,
            wallpaper: None,
            cursor,
            pointer: None,
        }
    }

    /// Draw `image` behind the panel under a scrim sized for it.
    ///
    /// The picture is already decoded and already fitted by the caller — in
    /// its own sandbox, never in the address space that owns the seat.
    pub fn set_wallpaper(&mut self, image: Surface) {
        let panel = panel_rect(self.screen(), self.scale);
        let scrim = scrim_alpha(&image, panel, &self.theme);
        self.wallpaper = Some(Wallpaper { image, scrim });
    }

    /// Draw `image` as the pointer, from where the pointer already is.
    ///
    /// Called once at start-up with the arrow rasterised for the active
    /// scale. Until it is, the pointer moves and hit-tests but nothing is
    /// drawn for it.
    pub fn set_pointer(&mut self, image: CursorImage) {
        self.pointer = Some(PlacedCursor::new(image, self.cursor.at()));
    }

    /// The rectangle the screen covers.
    #[must_use]
    pub const fn screen(&self) -> Rect {
        self.scanout.screen()
    }

    /// The line currently shown under the field.
    #[must_use]
    pub fn notice(&self) -> &str {
        self.surface.notice()
    }

    /// The frame's bytes, for the present call.
    #[must_use]
    pub fn frame(&self) -> &[u8] {
        self.scanout.frame()
    }

    /// Paint the whole screen and present all of it.
    ///
    /// Used for the first frame and after anything that changes more than one
    /// part of the surface.
    pub fn repaint(&mut self) -> Present {
        self.paint(None)
    }

    /// Apply one input event.
    ///
    /// The verdict a submitted secret produced is applied before this
    /// returns: a refusal's lockout starts counting from `now_ns` and is
    /// already on screen in the frame this round presents.
    pub fn on_input(&mut self, event: &InputEvent, now_ns: u64) -> Step {
        let round = self.apply(event, now_ns);
        Step {
            present: self.present_for(round.repaint),
            verified: round.verified,
            answer: round.answer,
        }
    }

    /// Apply one pointer report from the seat.
    ///
    /// The report is relative motion or a button, so the running position is
    /// kept here and the surface is given the absolute events it hit-tests.
    /// One report expands to as many as two of them, and they present
    /// together: a press is a move and a press, not two frames.
    ///
    /// A move also repaints the pointer itself — the union of where it was
    /// and where it now is — so no cursor is left painted behind. Motion
    /// that lands on the same pixel moves nothing and paints nothing.
    pub fn on_pointer(&mut self, input: &PointerInput, now_ns: u64) -> Step {
        let (action, moved) = match *input {
            PointerInput::MovedBy { dx, dy } => (PointerAction::Moved, self.move_pointer(dx, dy)),
            PointerInput::Pressed(button) => (PointerAction::Pressed(button), None),
            PointerInput::Released(button) => (PointerAction::Released(button), None),
            // The authentication surface has nothing scrollable.
            PointerInput::Scrolled { .. } => return Step::quiet(),
        };
        let mut repaint = moved.map_or(Repaint::QUIET, Repaint::region);
        let mut verified = false;
        let mut answer = None;
        for event in pointer_input_events(action, self.cursor.at()) {
            let round = self.apply(&event, now_ns);
            repaint = repaint.merged(round.repaint);
            answer = round.answer.or(answer);
            if round.verified {
                verified = true;
                break;
            }
        }
        Step {
            present: self.present_for(repaint),
            verified,
            answer,
        }
    }

    /// Bring the clock and the lockout up to date.
    ///
    /// Called when the park deadline elapses. Nothing repaints unless one of
    /// them actually changed, so a wake that finds nothing to do presents
    /// nothing.
    pub fn refresh(&mut self, now_ns: u64, wall: Option<Time64>) -> Step {
        let clock = Repaint::of(self.surface.set_chrome(chrome(wall, &self.host)));
        let remaining = self.cooldown.remaining(now_ns);
        let repaint = clock.merged(Repaint::of(self.surface.set_cooldown(remaining)));
        Step {
            present: self.present_for(repaint),
            ..Step::quiet()
        }
    }

    /// The relative nanosecond timeout for the next park.
    #[must_use]
    pub fn park_timeout(&self, now_ns: u64, wall: Option<Time64>) -> u64 {
        park_timeout(wall, self.cooldown.remaining(now_ns))
    }

    /// Move the pointer by `(dx, dy)` and report the pixels that owe a
    /// repaint: where the cursor was, unioned with where it now is, clipped
    /// to the screen. `None` when the pointer did not move, or when there is
    /// no cursor drawn to move.
    fn move_pointer(&mut self, dx: i32, dy: i32) -> Option<Rect> {
        let screen = self.scanout.screen();
        let was = self.cursor.at();
        let at = self.cursor.moved_by(dx, dy);
        if at == was {
            return None;
        }
        let pointer = self.pointer.as_mut()?;
        let vacated = pointer.bounds();
        pointer.set_pointer(at);
        let damage = vacated.union(&pointer.bounds()).intersection(&screen);
        (!damage.is_empty()).then_some(damage)
    }

    /// One event through the surface, with any verdict it produced applied.
    fn apply(&mut self, event: &InputEvent, now_ns: u64) -> Round {
        let outcome = {
            let mut ctx = EventContext {
                screen: self.scanout.screen(),
                scale: self.scale,
                theme: &self.theme,
                verifier: &mut self.verifier,
            };
            self.surface.on_event(event, &mut ctx)
        };
        let answer = self.verifier.take_answer();
        let mut repaint = Repaint::of(outcome);
        if let Some(answer) = answer {
            self.cooldown.start(now_ns, answer.retry_after);
            if answer.retry_after > Duration64::ZERO {
                repaint =
                    repaint.merged(Repaint::of(self.surface.set_cooldown(answer.retry_after)));
            }
        }
        Round {
            repaint,
            verified: outcome.verified(),
            answer,
        }
    }

    /// Repaint for `repaint`, or present nothing when it changed nothing.
    fn present_for(&mut self, repaint: Repaint) -> Present {
        if !repaint.redraw {
            return Present::Nothing;
        }
        self.paint(repaint.damage)
    }

    /// Render the surface and copy `damage` of it into the frame.
    fn paint(&mut self, damage: Option<Rect>) -> Present {
        let screen = self.scanout.screen();
        let backdrop = match self.wallpaper.as_ref() {
            Some(paper) => Backdrop::Wallpaper {
                image: &paper.image,
                scrim: paper.scrim,
            },
            None => Backdrop::Desktop,
        };
        let Some(mut painted) = self
            .surface
            .render(screen, self.scale, &self.theme, backdrop)
        else {
            return Present::Nothing;
        };
        // Over everything the surface drew: the pointer is the top-most
        // thing on the screen.
        if let Some(pointer) = self.pointer.as_ref() {
            pointer.draw(&mut painted);
        }
        self.scanout.compose(&painted, damage)
    }
}

/// What one event through the surface did.
struct Round {
    repaint: Repaint,
    verified: bool,
    answer: Option<Answer>,
}

#[cfg(test)]
#[path = "screen_tests.rs"]
mod tests;
