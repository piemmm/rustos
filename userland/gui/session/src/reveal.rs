//! The desktop's reveal from black when a session starts.
//!
//! The login screen fades to black and exits, so the screen this session
//! inherits is already dark. [`SessionReveal`] drives the compositor's screen
//! reveal ([`Compositor::set_reveal`]) from that black up to the composed
//! desktop over the theme's own [`MotionInteraction::SessionFade`] span, so
//! the desktop appears rather than snapping on.
//!
//! It is one [`Timeline`] and nothing else: the shared motion state machine
//! decides how a duration becomes frames, when the next frame is worth
//! drawing, and that a reduced-motion theme's zero duration is complete
//! immediately — so a desktop that is not fading arms no timer and presents
//! no extra frame.
//!
//! Reaching full strength is announced once, as [`DESKTOP_REVEALED`]: until
//! then the desktop is on screen but dark, so "a frame was presented" is not
//! yet "a user can see the desktop".

use tairix_log::{log, Event, EventId, Level, Sink};
use tairix_theme::{MotionInteraction, Timeline};
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

/// The session's fade in: black to the composed desktop, once, at the start
/// of a session.
pub struct SessionReveal {
    timeline: Timeline,
    announced: bool,
}

impl SessionReveal {
    /// Begin revealing `compositor`'s screen at `now_ns`, applying the first
    /// frame's strength before the caller presents it.
    ///
    /// The span is the compositor's own theme's session-fade duration, so the
    /// desktop cannot fade at a rate the screen it is drawn on disagrees
    /// with. Begin this where the session first has something to show: the
    /// span is wall-clock, so a reveal begun over bring-up would spend itself
    /// on a screen with nothing on it yet.
    ///
    /// Under a reduced-motion theme the duration is zero, which is already
    /// complete: the screen is fully revealed here and no frame, and no
    /// timer, is ever owed.
    #[must_use]
    pub fn begin(now_ns: u64, compositor: &mut Compositor) -> Self {
        let span = compositor
            .theme()
            .motion()
            .duration(MotionInteraction::SessionFade);
        let mut reveal = Self {
            timeline: Timeline::start(now_ns, span),
            announced: false,
        };
        reveal.apply(now_ns, compositor);
        reveal
    }

    /// Step the reveal to `now_ns`, returning whether the screen changed (and
    /// so owes a present). A settled reveal is no work at all.
    ///
    /// Time drives this alone: reaching the end of the span reveals the
    /// screen fully and settles, whatever became of any frame presented on
    /// the way, so a refused present cannot leave the desktop dark. A clock
    /// that jumped backwards settles it too, rather than stalling black.
    pub fn advance(&mut self, now_ns: u64, compositor: &mut Compositor) -> bool {
        self.timeline.running() && self.apply(now_ns, compositor)
    }

    /// `park_ns` shortened to this reveal's next frame, or left exactly as it
    /// is when nothing is fading — an idle desktop arms no timer of its own.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(park_ns, self.timeline.next_frame_in(now_ns))
    }

    /// Announce, once, that the frame just handed to the display carried the
    /// fully-revealed screen.
    ///
    /// Call this after a present that actually reached the display: a session
    /// holding no screen has shown nothing and may not claim it. Anything
    /// still fading is dark to a degree, so nothing is said until the reveal
    /// has settled; a reduced-motion theme is settled from its first frame,
    /// which is where its witness lands.
    pub fn presented<S: Sink + ?Sized>(&mut self, sink: &S) {
        if self.announced || self.timeline.running() {
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

    /// Put this reveal's strength at `now_ns` on the screen, settling once
    /// there is nothing left to reveal, and report whether the screen
    /// changed.
    fn apply(&mut self, now_ns: u64, compositor: &mut Compositor) -> bool {
        let strength = self.timeline.progress(now_ns);
        if strength == u8::MAX {
            self.timeline.settle();
        }
        compositor.set_reveal(strength)
    }
}
