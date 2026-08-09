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
use tairix_theme::{MotionInteraction, Theme};
use tairix_window::pointer_input_events;

use crate::accounts::SessionTransport;
use crate::chrome::chrome;
use crate::cursor::Cursor;
use crate::frame::{Present, Scanout};
use crate::verify::{Answer, SessionVerifier};
use crate::wait::{frame_budget, park_timeout, Cooldown};

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
    /// The surface as last rendered, with no cursor drawn into it.
    ///
    /// Everything the render reads — the account tiles, the field, the
    /// chrome, the lockout, the backdrop — changes only through the surface
    /// reporting it or a wallpaper arriving, and both drop this. A pointer
    /// sliding across an unchanged screen therefore re-composes a
    /// cursor-sized patch of pixels that already exist, instead of building
    /// a whole screen for every motion report the seat delivers.
    painted: Option<Surface>,
}

/// A decoded, screen-fitted wallpaper and the scrim sized for it.
struct Wallpaper {
    image: Surface,
    scrim: u8,
}

/// A repaint request: what changed, and which pixels it changed.
///
/// Two updates can land in one round — a keystroke and the lockout that
/// answered it, or a move and the tile it moved onto — and each is reported
/// separately, so they are combined here. A whole-screen surface change from
/// either side stays whole: one part of a paint changing everything is not
/// narrowed by another changing a rectangle.
///
/// A pointer that only moved is kept apart from a surface that changed: the
/// pixels it moves over are already rendered, so only the frame composed
/// from them is redone.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Repaint {
    /// Nothing changed, so nothing is painted.
    Nothing,
    /// Only the pointer moved, over these pixels.
    Cursor(Rect),
    /// The surface's own content changed, within this rectangle — `None` for
    /// all of it.
    Painted(Option<Rect>),
}

impl Repaint {
    fn of(outcome: Outcome) -> Self {
        if outcome.redraw() {
            Self::Painted(outcome.damage())
        } else {
            Self::Nothing
        }
    }

    fn merged(self, other: Self) -> Self {
        match (self, other) {
            (Self::Nothing, repaint) | (repaint, Self::Nothing) => repaint,
            (Self::Cursor(mine), Self::Cursor(theirs)) => Self::Cursor(mine.union(&theirs)),
            (Self::Painted(None), _) | (_, Self::Painted(None)) => Self::Painted(None),
            (Self::Cursor(moved), Self::Painted(Some(damage)))
            | (Self::Painted(Some(damage)), Self::Cursor(moved)) => {
                Self::Painted(Some(damage.union(&moved)))
            }
            (Self::Painted(Some(mine)), Self::Painted(Some(theirs))) => {
                Self::Painted(Some(mine.union(&theirs)))
            }
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
            painted: None,
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
        self.painted = None;
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

    /// Compose the whole screen and present all of it.
    ///
    /// Used for the first frame and after anything that changes more than one
    /// part of the surface.
    pub fn repaint(&mut self) -> Present {
        self.compose(None)
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
        let mut repaint = moved.map_or(Repaint::Nothing, Repaint::Cursor);
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
        let cooldown = Repaint::of(self.surface.set_cooldown(remaining));
        let motion = Repaint::of(self.surface.advance(now_ns));
        let repaint = clock.merged(cooldown).merged(motion);
        Step {
            present: self.present_for(repaint),
            ..Step::quiet()
        }
    }

    /// The relative nanosecond timeout for the next park.
    ///
    /// The nearer of the existing clock/lockout deadline and whatever the
    /// surface is animating — a selection mark crossing, a stage giving way,
    /// a refused attempt shaking. When nothing is animating the timeout is
    /// exactly what it was before motion existed: an idle screen still arms
    /// no timer.
    #[must_use]
    pub fn park_timeout(&self, now_ns: u64, wall: Option<Time64>) -> u64 {
        let base = park_timeout(wall, self.cooldown.remaining(now_ns));
        match self.surface.motion_due(now_ns) {
            Some(motion) => base.min(motion),
            None => base,
        }
    }

    /// Begin the fade the screen arrives out of, and present its first frame.
    ///
    /// Called before the opening present, so the first frame the display is
    /// handed is full black and the login screen appears out of it. That
    /// black is what the seat was handed over cleared to — and at first boot
    /// it covers the text console's pixels in one step instead of replacing
    /// them with a chooser. The screen answers input and draws its pointer
    /// throughout: it is arriving, not leaving.
    ///
    /// [`Present::Nothing`] when the theme fades instantly: there is nothing
    /// to cover, so the opening frame is the screen itself.
    pub fn begin_entry_fade(&mut self, now_ns: u64) -> Present {
        let outcome = self.surface.begin_entry_fade(now_ns, &self.theme);
        self.present_for(Repaint::of(outcome))
    }

    /// Begin the fade to black the screen leaves through, and present its
    /// first frame.
    ///
    /// Called once a secret has been accepted. The desktop cannot appear
    /// until this process exits, so the screen goes black *before* it does
    /// and the desktop comes up out of the same black — that is what makes
    /// the handover read as one movement rather than two screens swapping.
    /// The surface stops answering input from here on.
    ///
    /// [`Present::Nothing`] when the theme fades instantly: there is no frame
    /// worth showing, so the caller leaves at once.
    pub fn begin_session_fade(&mut self, now_ns: u64) -> Present {
        let outcome = self.surface.begin_session_fade(now_ns, &self.theme);
        if self.surface.session_fade_finished() {
            return Present::Nothing;
        }
        self.present_for(Repaint::of(outcome))
    }

    /// Whether the screen has finished going black, so its owner may leave.
    #[must_use]
    pub fn session_fade_finished(&self) -> bool {
        self.surface.session_fade_finished()
    }

    /// Nanoseconds until the fade's next frame, or `None` once it is over.
    #[must_use]
    pub fn session_fade_due(&self, now_ns: u64) -> Option<u64> {
        if self.surface.session_fade_finished() {
            return None;
        }
        self.surface.motion_due(now_ns)
    }

    /// Darken the fade to `now_ns` and present what changed.
    pub fn session_fade_step(&mut self, now_ns: u64) -> Present {
        let darkened = Repaint::of(self.surface.advance(now_ns));
        self.present_for(darkened)
    }

    /// The most frames the fade can ever ask for.
    ///
    /// What bounds the loop that presents it: a stopped clock or a seat that
    /// reads ready forever must not be able to strand a successful login on
    /// a screen that never finishes leaving.
    #[must_use]
    pub fn session_fade_budget(&self) -> u32 {
        frame_budget(self.theme.motion().duration(MotionInteraction::SessionFade))
    }

    /// Move the pointer by `(dx, dy)` and report the pixels that owe a
    /// repaint: where the cursor was, unioned with where it now is, clipped
    /// to the screen. `None` when the pointer did not move, when there is no
    /// cursor drawn to move, or once the screen is leaving and nothing is
    /// drawn for it — the position is still tracked either way.
    fn move_pointer(&mut self, dx: i32, dy: i32) -> Option<Rect> {
        let screen = self.scanout.screen();
        let drawn = self.draws_pointer();
        let was = self.cursor.at();
        let at = self.cursor.moved_by(dx, dy);
        if at == was {
            return None;
        }
        let pointer = self.pointer.as_mut()?;
        let vacated = pointer.bounds();
        pointer.set_pointer(at);
        if !drawn {
            return None;
        }
        let damage = vacated.union(&pointer.bounds()).intersection(&screen);
        (!damage.is_empty()).then_some(damage)
    }

    /// Whether the pointer is drawn over the frame at all.
    ///
    /// It stops the moment the screen begins leaving: a pointer is something
    /// to point *with*, and the verdict is given, input is no longer
    /// answered, and there is nothing left under the black to point at. It
    /// goes with the screen it belonged to rather than staying bright over
    /// it.
    fn draws_pointer(&self) -> bool {
        !self.surface.session_fade_begun()
    }

    /// One event through the surface, with any verdict it produced applied.
    fn apply(&mut self, event: &InputEvent, now_ns: u64) -> Round {
        let outcome = {
            let mut ctx = EventContext {
                screen: self.scanout.screen(),
                scale: self.scale,
                theme: &self.theme,
                verifier: &mut self.verifier,
                now_ns,
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

    /// Present what `repaint` changed, or nothing when it changed nothing.
    fn present_for(&mut self, repaint: Repaint) -> Present {
        match repaint {
            Repaint::Nothing => Present::Nothing,
            Repaint::Cursor(damage) => self.compose(Some(damage)),
            Repaint::Painted(damage) => {
                self.painted = None;
                self.compose(damage)
            }
        }
    }

    /// Copy `damage` of the painted surface into the frame with the pointer
    /// over it, rendering the surface first when nothing holds it.
    ///
    /// A screen that is leaving hands the composer no pointer, so the first
    /// veiled frame — which covers the whole screen — is also the one that
    /// paints the arrow out.
    fn compose(&mut self, damage: Option<Rect>) -> Present {
        if self.painted.is_none() {
            self.painted = self.render();
        }
        let drawn = self.draws_pointer();
        let Self {
            scanout,
            pointer,
            painted,
            ..
        } = self;
        let Some(painted) = painted.as_ref() else {
            return Present::Nothing;
        };
        let cursor = if drawn { pointer.as_ref() } else { None };
        scanout.compose(painted, cursor, damage)
    }

    /// The surface as it now stands, with no cursor drawn into it, or `None`
    /// when it will not render at all.
    fn render(&self) -> Option<Surface> {
        let backdrop = match self.wallpaper.as_ref() {
            Some(paper) => Backdrop::Wallpaper {
                image: &paper.image,
                scrim: paper.scrim,
            },
            None => Backdrop::Desktop,
        };
        self.surface
            .render(self.scanout.screen(), self.scale, &self.theme, backdrop)
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
