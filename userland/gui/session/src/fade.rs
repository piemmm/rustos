//! The session's screen fade: up from black when it starts, back down to
//! black when it hands the screen on.
//!
//! The login screen fades to black and exits, so the screen a session
//! inherits is already dark. [`ScreenFade`] drives the compositor's screen
//! reveal ([`Compositor::set_reveal`]) up out of that black over the theme's
//! own [`MotionInteraction::SessionFade`] span, and the same fade the other
//! way when the session logs out or steps aside — so the desktop dissolves
//! into the black the login screen then appears out of, and the two ends of
//! a switch meet on the same colour.
//!
//! It is one [`Fade`] and nothing else: the shared motion state machine
//! decides how a duration becomes frames, when the next frame is worth
//! drawing, and that a reduced-motion theme's zero duration is complete
//! immediately — so a desktop that is not fading arms no timer and presents
//! no extra frame. Turning a fade around picks up the strength that is
//! actually on screen, so a log-out chosen while the desktop is still
//! revealing dims from where it had got to rather than flashing bright.
//!
//! Reaching full strength is announced once, as [`DESKTOP_REVEALED`]: until
//! then the desktop is on screen but dark, so "a frame was presented" is not
//! yet "a user can see the desktop". A departure never announces it; the
//! witness says the desktop became visible, not that it stopped being.

use tairix_log::{log, Event, EventId, Level, Sink};
use tairix_theme::{Fade, MotionInteraction};
use tairix_wm::Compositor;

use crate::switchuser::park_within;

/// Range start (inclusive) reserved for the desktop session's event
/// identifiers. Per `lib/log` convention every subsystem owns a 1 000-wide
/// reserved range; the desktop session occupies `20000..21000` (adjacent to
/// the graphical login's `19000..20000`). Once shipped the numeric values
/// must never be re-used or re-numbered.
pub const DESKTOP_SESSION_RANGE_START: u32 = 20_000;
/// Range end (exclusive) reserved for desktop-session event identifiers.
pub const DESKTOP_SESSION_RANGE_END: u32 = 21_000;

/// One-shot: a composited desktop frame at full reveal strength has reached
/// the display — the witness that the desktop is not merely presenting but
/// visible. Emitted at most once per session, after the present rather than
/// on the way to it.
pub const DESKTOP_REVEALED: EventId = EventId(20_001);

/// The exact message [`DESKTOP_REVEALED`] is emitted with. A log consumer
/// (the desktop QEMU verticals' host runner gates its scan-out readback on
/// it) keys on this rendered text, so it is defined once beside the id and
/// imported by both sides.
pub const DESKTOP_REVEALED_MESSAGE: &str = "desktop fully revealed on screen";

/// The session's screen fade, in whichever direction it is currently
/// running.
pub struct ScreenFade {
    fade: Fade,
    announced: bool,
}

impl ScreenFade {
    /// Begin revealing `compositor`'s screen at `now_ns`, applying the first
    /// frame's strength before the caller presents it.
    ///
    /// Begin this where the session first has something to show: the span is
    /// wall-clock, so a reveal begun over bring-up would spend itself on a
    /// screen with nothing on it yet.
    #[must_use]
    pub fn begin(now_ns: u64, compositor: &mut Compositor) -> Self {
        let mut screen = Self {
            // The screen the login authority hands over is already black,
            // and this fade has not touched it yet.
            fade: Fade::start(now_ns, 0, 0, 0),
            announced: false,
        };
        screen.arrive(now_ns, compositor);
        screen
    }

    /// Fade the screen up to the composed desktop from wherever it is now,
    /// applying the first frame's strength before the caller presents it.
    ///
    /// What a session resumed from the background does: it was dimmed to
    /// black on its way out and the login screen has handed the seat back
    /// cleared, so it appears exactly as a fresh session does.
    pub fn arrive(&mut self, now_ns: u64, compositor: &mut Compositor) {
        self.turn(now_ns, compositor, u8::MAX);
    }

    /// Fade the screen down to black from wherever it is now, applying the
    /// first frame's strength before the caller presents it.
    ///
    /// What logging out and stepping aside do, before the seat is handed on
    /// cleared.
    pub fn depart(&mut self, now_ns: u64, compositor: &mut Compositor) {
        self.turn(now_ns, compositor, 0);
    }

    /// Whether the screen has reached this fade's end, so nothing more is
    /// owed and no timer is armed for it.
    #[must_use]
    pub const fn settled(&self) -> bool {
        !self.fade.running()
    }

    /// Step the fade to `now_ns`, returning whether the screen changed (and
    /// so owes a present). A settled fade is no work at all.
    ///
    /// Time drives this alone: reaching the end of the span puts the screen
    /// at that end and settles, whatever became of any frame presented on
    /// the way, so a refused present cannot leave the desktop stuck part-lit.
    /// A clock that jumped backwards settles it too, rather than stalling.
    pub fn advance(&mut self, now_ns: u64, compositor: &mut Compositor) -> bool {
        self.fade.running() && self.apply(now_ns, compositor)
    }

    /// `park_ns` shortened to this fade's next frame, or left exactly as it
    /// is when nothing is fading — an idle desktop arms no timer of its own.
    ///
    /// A fade whose span ran out while the loop was presenting shortens it to
    /// nothing rather than leaving it alone: the frame that completes the
    /// fade is still owed, and only [`advance`](Self::advance) drawing it
    /// settles the fade.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(park_ns, self.fade.next_frame_in(now_ns))
    }

    /// Announce, once, that the frame just handed to the display carried the
    /// fully-revealed screen.
    ///
    /// Call this after a present that actually reached the display: a session
    /// holding no screen has shown nothing and may not claim it. Anything
    /// still fading is dark to a degree, so nothing is said until the fade
    /// has settled; a reduced-motion theme is settled from its first frame,
    /// which is where its witness lands. A fade heading for black says
    /// nothing at all, whichever end it reaches.
    pub fn presented<S: Sink + ?Sized>(&mut self, sink: &S) {
        if self.announced || self.fade.running() || self.fade.target() != u8::MAX {
            return;
        }
        self.announced = true;
        log(
            sink,
            &Event {
                level: Level::Info,
                id: DESKTOP_REVEALED,
                message: DESKTOP_REVEALED_MESSAGE,
                fields: &[],
            },
        );
    }

    /// Point the fade at `to`, starting from the strength on screen now, and
    /// put that first frame up.
    ///
    /// The span is the compositor's own theme's session-fade duration, so
    /// the desktop cannot fade at a rate the screen it is drawn on disagrees
    /// with. Under a reduced-motion theme that duration is zero, which is
    /// already complete: the screen is at `to` here and no frame, and no
    /// timer, is ever owed.
    fn turn(&mut self, now_ns: u64, compositor: &mut Compositor, to: u8) {
        let span = compositor
            .theme()
            .motion()
            .duration(MotionInteraction::SessionFade);
        self.fade = Fade::start(now_ns, span, self.fade.strength(now_ns), to);
        self.apply(now_ns, compositor);
    }

    /// Put this fade's strength at `now_ns` on the screen, settling once it
    /// has arrived, and report whether the screen changed.
    fn apply(&mut self, now_ns: u64, compositor: &mut Compositor) -> bool {
        let strength = self.fade.strength(now_ns);
        if strength == self.fade.target() {
            self.fade.settle();
        }
        compositor.set_reveal(strength)
    }
}
