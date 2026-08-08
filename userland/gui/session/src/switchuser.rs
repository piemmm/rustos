//! The desktop session's half of **fast user switching**
//! (`plans/NEW-DESKTOP-LOGIN.md` G5): stepping aside so somebody else can
//! log in, parking while another account holds the screen, and being woken
//! back to exactly the session that was left.
//!
//! Switching away is not logging out. Everything in the session keeps
//! running — an editor keeps its unsaved buffer, a build finishes, every
//! served window stays open — but the session presents nothing and drains
//! no seat input, because it no longer owns the seat.
//!
//! # The order is the safety property
//!
//! [`SwitchUser::step_aside`] asks the authority *first* and gives up the
//! screen only on [`SessionVerdict::Accepted`]. Releasing the seat before
//! the authority has agreed to put the login screen back up would leave the
//! display owned by nobody and drawn by nobody. A refusal, a reply that
//! does not decode, and a transport failure are one answer: stay exactly as
//! we are (fail closed).
//!
//! # Who may wake us
//!
//! The wake mailbox is unforgeable by construction — the kernel attests
//! every sender's [`Origin`] — but it is an ordinary, unreserved id, so
//! anything could *send* to it. [`SwitchUser::classify`] therefore honours
//! a message only from a principal on this session's own console holding
//! `CAP_IPC_BIND_PRIVILEGED`, which is the authority that alone may serve
//! the reserved session rendezvous. The authority's account is a
//! configured one, so the check is the capability and the console rather
//! than a uid guessed here. A message that does not decode, or whose
//! sender does not clear that bar, is dropped and stated — never acted on.
//!
//! The wake carries no authority of its own: it says "you are foreground",
//! and the kernel's seat exclusivity is what decides who may actually
//! present. A session that finds the seat taken reports and ends rather
//! than sitting invisible.
//!
//! # Parking
//!
//! A background session parks with no deadline at all
//! ([`SwitchUser::park_deadline_ns`]): it has nothing to redraw and nothing
//! to poll, so a timer would only wake a core to do nothing. A cache-report
//! change the runtime's rate limiter is holding back at that moment goes
//! out on the next wake instead of arming one.

use tairix_abi::driver::display::DisplayMode;
use tairix_abi::session_ipc::{SessionVerdict, SessionWake};
use tairix_abi::{CapabilityId, Errno, Origin};

/// The `waitset_wait` timeout meaning "no deadline: wake only on a member".
pub const NO_DEADLINE_NS: u64 = u64::MAX;

/// `park_ns` shortened to `due`, or left exactly as it is when nothing is
/// due.
///
/// The one fold every animated surface's next frame goes through, so two
/// animations cannot round or clamp the session's park differently, and a
/// desktop with nothing in flight parks on precisely the deadline it would
/// have had — an idle screen arms no timer.
#[must_use]
pub fn park_within(park_ns: u64, due: Option<u64>) -> u64 {
    due.map_or(park_ns, |due| park_ns.min(due))
}

/// One `Background` request to the session authority.
///
/// A synchronous `ipc_call` on the reserved session rendezvous in
/// production, a scripted answer under test. The request is bodyless and
/// carries no identity: the authority attests the caller from the kernel.
pub trait SessionAuthority {
    /// Ask to be recorded as a background session.
    ///
    /// # Errors
    ///
    /// The transport's own [`Errno`] when the authority could not be
    /// reached, or when its reply did not decode as a verdict. Both are
    /// treated as a refusal by the caller.
    fn request_background(&mut self) -> Result<SessionVerdict, Errno>;
}

/// The session's ownership of the screen, as the switch drives it.
///
/// Every method is a step the switch performs in a fixed order and nothing
/// else calls: [`suspend`](Self::suspend) then
/// [`release_seat`](Self::release_seat) on the way out, and
/// [`acquire_seat`](Self::acquire_seat), [`query_mode`](Self::query_mode),
/// [`reconfigure`](Self::reconfigure), [`repaint_all`](Self::repaint_all)
/// on the way back.
pub trait SeatPresentation {
    /// Stop compositing and hand back the shared frame region.
    ///
    /// Called while the seat is still held, so the last thing on screen is
    /// this session's own frame rather than a torn one.
    fn suspend(&mut self);

    /// Release the seat lease.
    ///
    /// The kernel purges the seat's undrained input on the lease change, so
    /// nothing typed at this desktop reaches the next owner.
    fn release_seat(&mut self);

    /// Re-take the seat lease.
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal when the seat is held by somebody else or
    /// this session may no longer take it.
    fn acquire_seat(&mut self) -> Result<(), Errno>;

    /// Re-read the display mode now in force.
    ///
    /// # Errors
    ///
    /// The display service's typed refusal or a transport failure. The mode
    /// is never guessed from the one this session came up with.
    fn query_mode(&mut self) -> Result<DisplayMode, Errno>;

    /// Re-establish the shared frame region for `mode` and re-lay every
    /// surface whose geometry comes from the mode.
    ///
    /// Everything mode-sized is rebuilt here rather than assumed: the frame
    /// region, the compositor's own buffers, the bar, the desktop icons,
    /// and the wallpaper's fit.
    ///
    /// # Errors
    ///
    /// The refusal that stopped the frame being re-established, or
    /// [`Errno::NotSupported`] when `mode` is not one this session can
    /// present into.
    fn reconfigure(&mut self, mode: DisplayMode) -> Result<(), Errno>;

    /// Repaint every pixel of `mode` and present it.
    ///
    /// The whole screen, not a damage rectangle: another account has been
    /// drawing here, so nothing about the previous contents may be assumed.
    ///
    /// # Errors
    ///
    /// The refusal that stopped the frame reaching the screen.
    fn repaint_all(&mut self, mode: DisplayMode) -> Result<(), Errno>;
}

/// Why a step-aside did not happen. The session is untouched in every case.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchRefusal {
    /// This session holds no wake mailbox, so nothing could ever bring it
    /// back. The menu entry is absent for exactly this reason; reaching
    /// here means the state changed under an already-open menu.
    Unavailable,
    /// The authority refused. It alone knows why, and does not say.
    Refused,
    /// The authority could not be reached, or answered something that is
    /// not a verdict.
    Unreachable(Errno),
}

impl SwitchRefusal {
    /// The `stderr` line, without the caller's own `desktop: ` prefix or
    /// trailing newline.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unavailable => "cannot switch user: this session has no way to be resumed",
            Self::Refused => "the login service refused to switch user; nothing has changed",
            Self::Unreachable(_) => {
                "could not reach the login service to switch user; nothing has changed"
            }
        }
    }
}

/// Why a wake was dropped instead of acted on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WakeRefusal {
    /// The kernel-attested sender is not the session authority.
    Unattested,
    /// The message did not decode.
    Malformed(Errno),
}

impl WakeRefusal {
    /// The `stderr` line, without the caller's own `desktop: ` prefix or
    /// trailing newline.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Unattested => "dropped a session wake from an unattested sender",
            Self::Malformed(_) => "dropped a malformed session wake",
        }
    }
}

/// Which step of coming back to the foreground failed, and how.
///
/// Every one of them leaves the session unable to show itself, so each ends
/// it with its reason stated rather than leaving the user looking at
/// somebody else's screen with an invisible desktop behind it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResumeFailure {
    /// The seat could not be re-acquired.
    SeatRefused(Errno),
    /// The display mode could not be re-read.
    ModeUnavailable(Errno),
    /// The shared frame could not be re-established for that mode.
    FrameRefused(Errno),
    /// The full repaint did not reach the screen.
    PaintRefused(Errno),
}

impl ResumeFailure {
    /// The `stderr` line, without the caller's own `desktop: ` prefix or
    /// trailing newline.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::SeatRefused(_) => "could not take the screen back; ending the session",
            Self::ModeUnavailable(_) => "could not re-read the display mode; ending the session",
            Self::FrameRefused(_) => "could not re-establish the display frame; ending the session",
            Self::PaintRefused(_) => "could not repaint the screen; ending the session",
        }
    }

    /// The underlying refusal, for the diagnosis line.
    #[must_use]
    pub const fn errno(self) -> Errno {
        match self {
            Self::SeatRefused(err)
            | Self::ModeUnavailable(err)
            | Self::FrameRefused(err)
            | Self::PaintRefused(err) => err,
        }
    }
}

/// This session's place in fast user switching: whether it can be switched
/// away from at all, and whether it currently is.
///
/// Built once at bring-up and consulted by the serve loop on every turn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SwitchUser {
    wake: Option<u64>,
    console: u64,
    background: bool,
}

impl SwitchUser {
    /// A foreground session holding `wake` — the bound wake-mailbox
    /// endpoint, or `None` when the bind failed — on `console`.
    ///
    /// Without a mailbox the session simply cannot be switched away from:
    /// it says so through [`is_available`](Self::is_available) and the
    /// desktop leaves the menu entry out, rather than offering a switch
    /// that would strand the user at a login screen with no way back.
    #[must_use]
    pub const fn new(wake: Option<u64>, console: u64) -> Self {
        Self {
            wake,
            console,
            background: false,
        }
    }

    /// Whether this session can be switched away from.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.wake.is_some()
    }

    /// The bound wake-mailbox endpoint this session is drained on, if the
    /// bind succeeded.
    #[must_use]
    pub const fn wake_endpoint(&self) -> Option<u64> {
        self.wake
    }

    /// Whether this session is currently background: presenting nothing,
    /// draining no seat input, and parked on its wake mailbox.
    #[must_use]
    pub const fn is_background(&self) -> bool {
        self.background
    }

    /// The deadline the serve loop parks on this turn, given the
    /// `foreground_ns` deadline it would otherwise use.
    ///
    /// A background session parks indefinitely: it presents nothing and
    /// polls nothing, so any deadline would only wake a core with no work
    /// to do for it.
    #[must_use]
    pub const fn park_deadline_ns(&self, foreground_ns: u64) -> u64 {
        if self.background {
            NO_DEADLINE_NS
        } else {
            foreground_ns
        }
    }

    /// Ask `authority` to record this session as background and, only once
    /// it has, give up the screen through `presentation`.
    ///
    /// The seat is released strictly after an [`SessionVerdict::Accepted`]
    /// reply, and never on any other answer: the login screen must be ready
    /// to come up before this desktop stops drawing.
    ///
    /// # Errors
    ///
    /// The [`SwitchRefusal`] naming why the session did not step aside.
    /// Nothing has been suspended or released on any of them.
    pub fn step_aside(
        &mut self,
        authority: &mut dyn SessionAuthority,
        presentation: &mut dyn SeatPresentation,
    ) -> Result<(), SwitchRefusal> {
        if !self.is_available() || self.background {
            return Err(SwitchRefusal::Unavailable);
        }
        match authority.request_background() {
            Ok(SessionVerdict::Accepted) => {}
            Ok(SessionVerdict::Refused { .. }) => return Err(SwitchRefusal::Refused),
            Err(err) => return Err(SwitchRefusal::Unreachable(err)),
        }
        presentation.suspend();
        presentation.release_seat();
        self.background = true;
        Ok(())
    }

    /// Decode one wake-mailbox message and attest its sender.
    ///
    /// `sender` is the kernel's own account of who sent it, never a claim
    /// on the wire. Attestation comes first, so a message from anybody but
    /// the authority is refused before it is even decoded.
    ///
    /// # Errors
    ///
    /// The [`WakeRefusal`] naming why the message was dropped.
    pub fn classify(&self, message: &[u8], sender: &Origin) -> Result<SessionWake, WakeRefusal> {
        if sender.console() != self.console
            || !sender
                .capabilities()
                .holds_cap(CapabilityId::IPC_BIND_PRIVILEGED)
        {
            return Err(WakeRefusal::Unattested);
        }
        SessionWake::decode(message).map_err(WakeRefusal::Malformed)
    }

    /// Come back to the foreground: re-take the seat, re-read the mode —
    /// which may have changed while another account held the screen —
    /// re-establish the shared frame for it, and repaint every pixel.
    ///
    /// Answers the mode now in force, which the caller uses to rebuild what
    /// it owns outside the presentation (the pointer's screen rectangle).
    ///
    /// # Errors
    ///
    /// The [`ResumeFailure`] naming the step that refused. The session
    /// stays background, because it still has no screen to draw on.
    pub fn resume(
        &mut self,
        presentation: &mut dyn SeatPresentation,
    ) -> Result<DisplayMode, ResumeFailure> {
        presentation
            .acquire_seat()
            .map_err(ResumeFailure::SeatRefused)?;
        let mode = presentation
            .query_mode()
            .map_err(ResumeFailure::ModeUnavailable)?;
        presentation
            .reconfigure(mode)
            .map_err(ResumeFailure::FrameRefused)?;
        presentation
            .repaint_all(mode)
            .map_err(ResumeFailure::PaintRefused)?;
        self.background = false;
        Ok(mode)
    }
}
