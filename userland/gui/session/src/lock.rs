//! The desktop session's **screen lock**: the shared authentication surface
//! (`lib/greeter`) held over the whole display, taking every input event
//! until the signed-in user proves they are still the one at the keyboard.
//!
//! Locking is the one way out of a session that keeps the session. Everything
//! carries on running behind the lock — a build finishes, a download
//! completes, an editor keeps its unsaved buffer — but nothing on screen is
//! legible and no keystroke or click reaches any of it.
//!
//! # What makes it a lock rather than a window
//!
//! The surface asks for a password and concludes only on a verified one;
//! that much is `tairix_greeter`'s. What this module adds is everything that
//! makes asking *unavoidable*, and none of it can be done from inside a
//! surface:
//!
//! * **It covers the screen.** The window is the compositor's full extent
//!   and fully opaque, so no pixel of the session shows through. Someone
//!   walking past a locked machine learns nothing about what is on it.
//! * **It takes every event.** While locked, the embedder drains the pointer
//!   and keyboard into [`handle`](ScreenLock::handle) and routes *nothing*
//!   onward — not to the window manager, not to the taskbar, not to a served
//!   application. That is the embedder's contract, stated on
//!   [`is_locked`](ScreenLock::is_locked); a window cannot enforce it from
//!   the inside.
//! * **It stays on top.** [`keep_topmost`](ScreenLock::keep_topmost) raises
//!   it before every composite, so an application that opens or raises a
//!   window behind the lock cannot surface above it.
//!
//! # Who decides it may open again
//!
//! The lock holds no authority to authenticate anybody. The desktop's
//! [`Verifier`] is the per-console elevation broker — the login supervisor
//! that started this session — which attests the caller's identity from the
//! kernel, checks the password against *that* uid, and audits the decision.
//! A refusal, a transport failure, a broker that is not there, and a reply it
//! cannot parse are all the same answer: still locked.

use tairix_greeter::{AuthSurface, Backdrop, EventContext, Verifier};
use tairix_wm::{Compositor, InputEvent, Point, Rect, Scale, Surface, WindowId};

use crate::shell::DesktopShell;

/// What one input event did to the lock.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LockOutcome {
    /// Nothing for the embedder to act on. The screen is still locked.
    Pending,
    /// The user proved their identity. The lock is already down — its window
    /// closed and the password erased — and the desktop is reachable again.
    Unlocked,
}

/// One engaged lock: its full-screen window and the surface drawn into it.
struct Engaged {
    wm: WindowId,
    surface: AuthSurface,
}

/// The session's screen lock.
///
/// Idle until [`engage`](ScreenLock::engage) puts it up, then holding the
/// screen until the user is verified or the session is torn down.
#[derive(Default)]
pub struct ScreenLock {
    engaged: Option<Engaged>,
}

impl ScreenLock {
    /// An idle lock.
    #[must_use]
    pub const fn new() -> Self {
        Self { engaged: None }
    }

    /// Whether the screen is locked right now.
    ///
    /// While this is `true` the embedder MUST drain every pointer and
    /// keyboard event into [`handle`](Self::handle) and route none of them to
    /// the window manager, the taskbar, or any served window. That routing is
    /// what makes this a lock; the surface only hides the session.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.engaged.is_some()
    }

    /// Lock the screen, naming `account` so the user knows whose password is
    /// wanted. An empty name heads the prompt with the surface's placeholder.
    ///
    /// Returns whether the screen is now locked. A lock already up answers
    /// `true` and changes nothing. A surface the compositor cannot give
    /// answers `false` and locks nothing: a lock that cannot cover the screen
    /// is worse than no lock, because the user would walk away believing they
    /// were protected.
    pub fn engage(
        &mut self,
        account: &str,
        shell: &DesktopShell,
        compositor: &mut Compositor,
    ) -> bool {
        if self.engaged.is_some() {
            return true;
        }
        let screen = compositor.screen_rect();
        let surface = AuthSurface::new(account);
        let Some(frame) = render_frame(&surface, screen, compositor.scale(), shell) else {
            return false;
        };
        let wm = compositor.add_window(screen.origin, frame);
        compositor.raise(wm);
        self.engaged = Some(Engaged { wm, surface });
        true
    }

    /// Raise the lock above everything, so a window opened or raised behind
    /// it cannot surface over it.
    ///
    /// The embedder calls this immediately before each composite while
    /// locked. It is cheap and idempotent, and does nothing at all when the
    /// screen is not locked.
    pub fn keep_topmost(&self, compositor: &mut Compositor) {
        if let Some(engaged) = self.engaged.as_ref() {
            let _ = compositor.raise(engaged.wm);
        }
    }

    /// Apply one input event — pointer or key — to the lock.
    ///
    /// Keys edit the password; `Enter` offers it to `verifier`. The pointer
    /// places the caret and selects within the field, and reaches nothing
    /// else on screen. There is no key and no click that dismisses the lock
    /// without a verified password.
    ///
    /// Returns [`LockOutcome::Unlocked`] once the user has been verified, by
    /// which time the lock is already down.
    pub fn handle(
        &mut self,
        event: &InputEvent,
        verifier: &mut dyn Verifier,
        shell: &DesktopShell,
        compositor: &mut Compositor,
    ) -> LockOutcome {
        if let InputEvent::PointerMoved { to } = event {
            compositor.move_cursor(*to);
        }
        let screen = compositor.screen_rect();
        let scale = compositor.scale();
        let theme = shell.session().active_theme();
        let Some(engaged) = self.engaged.as_mut() else {
            return LockOutcome::Pending;
        };
        let outcome = engaged.surface.on_event(
            &to_window_space(event, screen),
            &mut EventContext {
                screen: window_bounds(screen),
                scale,
                theme,
                verifier,
            },
        );
        if outcome.verified() {
            self.release(compositor);
            return LockOutcome::Unlocked;
        }
        if outcome.redraw() {
            self.repaint(shell, compositor);
        }
        LockOutcome::Pending
    }

    /// Take the lock down without verifying anybody.
    ///
    /// Used when the session is tearing the desktop down. It unlocks nothing:
    /// the session is ending, and the login prompt that replaces it asks for
    /// a password of its own. The password goes with the surface — the masked
    /// field zeroises its buffer as it is dropped — so an abandoned prompt
    /// leaves no plaintext behind either.
    pub fn abandon(&mut self, compositor: &mut Compositor) {
        if let Some(engaged) = self.engaged.take() {
            let _ = compositor.remove(engaged.wm);
        }
    }

    /// Repaint the lock, so a theme switch or a resolution change behind it
    /// redraws it at the right size in the appearance now in use.
    ///
    /// A surface that cannot be built leaves the previous frame up rather
    /// than uncovering the session.
    pub fn repaint(&self, shell: &DesktopShell, compositor: &mut Compositor) {
        let screen = compositor.screen_rect();
        let scale = compositor.scale();
        let Some(engaged) = self.engaged.as_ref() else {
            return;
        };
        if let Some(frame) = render_frame(&engaged.surface, screen, scale, shell) {
            let _ = compositor.set_surface(engaged.wm, frame);
        }
        let _ = compositor.raise(engaged.wm);
    }

    /// Take the lock down after a successful verification.
    fn release(&mut self, compositor: &mut Compositor) {
        if let Some(engaged) = self.engaged.take() {
            let _ = compositor.remove(engaged.wm);
        }
    }
}

/// One pass of the embedder draining the seat into a locked screen.
///
/// The embedder empties the pointer and keyboard channels on every wake, and
/// keeps emptying them even once a password has been verified part-way
/// through the batch. What is still queued at that instant is the tail of the
/// gesture that typed the password — the release of the `Enter` that
/// submitted it, a stray keystroke landing behind it — and routing that into
/// the session the moment it becomes visible would spill part of a password
/// entry into whatever holds focus.
///
/// So the first unlock latches here and every event after it in the same
/// drain is **dropped**: the channels still reach empty, so the seat cannot
/// wake the loop over events nobody will read, and nothing typed at a locked
/// screen is ever delivered onward. The next wake starts a fresh drain, by
/// which time the desktop is genuinely visible and the user is typing at what
/// they can see.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct LockedDrain {
    unlocked: bool,
}

impl LockedDrain {
    /// A drain that has not yet seen the lock come down.
    #[must_use]
    pub const fn new() -> Self {
        Self { unlocked: false }
    }

    /// Whether the lock came down part-way through this drain.
    #[must_use]
    pub const fn unlocked(&self) -> bool {
        self.unlocked
    }

    /// Apply one drained event to `lock`, or discard it because the lock has
    /// already come down earlier in this same drain.
    pub fn feed(
        &mut self,
        lock: &mut ScreenLock,
        event: &InputEvent,
        verifier: &mut dyn Verifier,
        shell: &DesktopShell,
        compositor: &mut Compositor,
    ) {
        if self.unlocked {
            return;
        }
        if lock.handle(event, verifier, shell, compositor) == LockOutcome::Unlocked {
            self.unlocked = true;
        }
    }
}

/// The lock window's own rectangle: the screen's extent at the window's
/// origin, which is where its pixels start.
fn window_bounds(screen: Rect) -> Rect {
    Rect::new(0, 0, screen.width, screen.height)
}

/// Rebase a pointer event from screen space into the lock window's space.
///
/// The lock window covers the screen from its top-left, so the two spaces
/// differ only by the screen rectangle's own origin — which is zero on every
/// single-output desktop, and correct rather than assumed on any other.
fn to_window_space(event: &InputEvent, screen: Rect) -> InputEvent {
    match event {
        InputEvent::PointerMoved { to } => InputEvent::PointerMoved {
            to: Point::new(to.x - screen.origin.x, to.y - screen.origin.y),
        },
        other => *other,
    }
}

/// Paint one frame of the lock over the whole screen, in the appearance the
/// session is currently wearing.
fn render_frame(
    surface: &AuthSurface,
    screen: Rect,
    scale: Scale,
    shell: &DesktopShell,
) -> Option<Surface> {
    surface.render(
        window_bounds(screen),
        scale,
        shell.session().active_theme(),
        Backdrop::Desktop,
    )
}
