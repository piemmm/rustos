//! One seat wake's input, drained into whoever holds the seat.
//!
//! Routing one event can hand the seat to another holder — the menu chain,
//! the lock, another account's session — so a drain re-reads the holder
//! between events and never applies one against the holder it is about to
//! replace. What only the session program can act on (the window channel,
//! launches, the session authority) leaves through [`SeatRouter`], so the
//! drain itself is host-tested.

use alloc::vec::Vec;

use tairix_abi::input::KeyInput;
use tairix_abi::Errno;
use tairix_greeter::Verifier;
use tairix_wm::{Compositor, InputEvent};

use crate::keyboard::{KeyInputChannel, KeyboardInputSource};
use crate::lock::{LockedDrain, ScreenLock};
use crate::menu::MenuChain;
use crate::shell::{DesktopShell, InputSource, ShellOutcome, Stopped};

/// What routing one outcome decided about the session.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Routed {
    /// Keep serving.
    Continue,
    /// The user asked to log out.
    EndSession,
    /// The user asked to switch to another account.
    SwitchUser,
}

/// How one seat wake ended.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SeatWake {
    /// The wake's input was served and the session still has the screen.
    Served,
    /// The session gave the screen to another account; nothing after that
    /// point was applied.
    SteppedAside,
    /// The user logged out.
    EndSession,
}

/// The session state a seat drain re-reads between events.
pub struct Seat<'a> {
    /// The desktop shell.
    pub shell: &'a mut DesktopShell,
    /// The compositor.
    pub compositor: &'a mut Compositor,
    /// The seat's one menu chain, which takes the seat while it is open.
    pub menu: &'a mut MenuChain,
    /// The screen lock, which takes the seat while it is engaged.
    pub lock: &'a mut ScreenLock,
}

/// Where a seat drain hands what only the session program can act on.
pub trait SeatRouter {
    /// Carry `outcome` onward. `key` is the record of the keystroke it came
    /// from.
    fn route(
        &mut self,
        seat: &mut Seat<'_>,
        outcome: ShellOutcome,
        key: Option<KeyInput>,
        now_ns: u64,
    ) -> Routed;

    /// Bring the screen into line with the chain and deliver every answer it
    /// owes, appending the icon bar's own answers to `answered`.
    ///
    /// Presenting can itself close the chain (a plate that cannot be drawn is
    /// refused), so the answers are delivered after the present.
    fn settle_chain(&mut self, seat: &mut Seat<'_>, answered: &mut Vec<ShellOutcome>, now_ns: u64);

    /// Ask to give the screen to another account. Answers whether it was
    /// given up.
    fn step_aside(&mut self, seat: &mut Seat<'_>) -> bool;
}

/// A seat drain, keeping the outcome buffer its batches reuse.
#[derive(Debug, Default)]
pub struct SeatDrain {
    outcomes: Vec<ShellOutcome>,
}

impl SeatDrain {
    /// A drain holding no outcomes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            outcomes: Vec::new(),
        }
    }

    /// Drain one wake of `pointer` and `keyboard` into whoever holds the
    /// seat, every event against the one instant `now_ns`.
    ///
    /// The pointer drains batch by batch, each routed before the next is
    /// taken, so what follows an edge goes to whoever that edge left holding
    /// the seat. Keys follow, each only while the shell holds the seat. What
    /// the lock is handed stays queued for the lock's own drain.
    ///
    /// # Errors
    ///
    /// A channel's fault. What was applied before it stays applied.
    pub fn wake<P, C, R>(
        &mut self,
        seat: &mut Seat<'_>,
        pointer: &mut P,
        keyboard: &mut KeyboardInputSource<C>,
        router: &mut R,
        now_ns: u64,
    ) -> Result<SeatWake, Errno>
    where
        P: InputSource + ?Sized,
        C: KeyInputChannel,
        R: SeatRouter,
    {
        loop {
            let stopped = if seat.menu.is_open() {
                drain_chain(seat, pointer, keyboard, router, &mut self.outcomes, now_ns)?
            } else {
                seat.shell
                    .pump(pointer, seat.compositor, now_ns, &mut self.outcomes)?
            };
            for outcome in self.outcomes.drain(..) {
                if let Some(end) = route(router, seat, outcome, None, now_ns) {
                    return Ok(end);
                }
            }
            if stopped == Stopped::Empty || seat.lock.is_locked() {
                break;
            }
        }
        let mut typed = false;
        while !seat.menu.is_open() && !seat.lock.is_locked() {
            let Some((event, record)) = seat.shell.poll_key(keyboard, seat.compositor, now_ns)?
            else {
                break;
            };
            let outcome = seat.shell.apply(event, seat.compositor, now_ns);
            typed = true;
            if let Some(end) = route(router, seat, outcome, Some(record), now_ns) {
                return Ok(end);
            }
        }
        // A held key repeating costs one settle per wake, not one per repeat.
        if typed {
            seat.shell.settle(seat.compositor);
        }
        Ok(SeatWake::Served)
    }
}

/// Route one outcome; `Some` is how the wake ends.
fn route<R: SeatRouter>(
    router: &mut R,
    seat: &mut Seat<'_>,
    outcome: ShellOutcome,
    key: Option<KeyInput>,
    now_ns: u64,
) -> Option<SeatWake> {
    match router.route(seat, outcome, key, now_ns) {
        Routed::Continue => None,
        Routed::EndSession => Some(SeatWake::EndSession),
        Routed::SwitchUser => router.step_aside(seat).then_some(SeatWake::SteppedAside),
    }
}

/// Drain the pointer, then the keys, into the open chain until it closes,
/// appending the icon bar's own answers to `answered`.
///
/// Nothing behind the chain is reachable while it is up. It is settled once
/// per phase that routed it an event, before anything is handed on, so the
/// next holder never hit-tests a plate that has gone. Settling also retires a
/// chain the display mode has moved under, which routing an event does not
/// notice. [`Stopped::AtEdge`] when it closed with pointer input still
/// queued, which is the next holder's.
fn drain_chain<P, C, R>(
    seat: &mut Seat<'_>,
    pointer: &mut P,
    keyboard: &mut KeyboardInputSource<C>,
    router: &mut R,
    answered: &mut Vec<ShellOutcome>,
    now_ns: u64,
) -> Result<Stopped, Errno>
where
    P: InputSource + ?Sized,
    C: KeyInputChannel,
    R: SeatRouter,
{
    answered.clear();
    // No gesture of the shell's can complete while the chain takes the stream.
    seat.shell.yield_pointer(seat.compositor);
    let mut moved = false;
    let mut handled = false;
    let pointer_empty = loop {
        if !seat.menu.is_open() {
            break false;
        }
        let Some(event) = pointer.poll()? else {
            break true;
        };
        moved |= matches!(event, InputEvent::PointerMoved { .. });
        seat.shell
            .route_to_chain(seat.compositor, seat.menu, &event, now_ns);
        handled = true;
    };
    if moved {
        seat.shell.settle(seat.compositor);
    }
    if handled {
        router.settle_chain(seat, answered, now_ns);
    }
    if !pointer_empty {
        return Ok(Stopped::AtEdge);
    }
    handled = false;
    while seat.menu.is_open() {
        let Some((event, _)) = seat.shell.poll_key(keyboard, seat.compositor, now_ns)? else {
            break;
        };
        if matches!(event, InputEvent::KeyPressed { .. }) {
            seat.shell
                .route_to_chain(seat.compositor, seat.menu, &event, now_ns);
            handled = true;
        }
    }
    if handled {
        router.settle_chain(seat, answered, now_ns);
    }
    Ok(Stopped::Empty)
}

/// Drain the seat straight into the engaged lock: no motion, click or key
/// reaches the window manager, the taskbar or an application.
///
/// Both channels drain to empty even once a password is verified part-way;
/// [`LockedDrain`] discards what follows it. The seat's modifier state is
/// still kept, so the first click after unlocking is not stamped with a
/// modifier released while locked.
///
/// # Errors
///
/// A channel's fault.
pub fn drain_locked<P, C>(
    seat: &mut Seat<'_>,
    pointer: &mut P,
    keyboard: &mut KeyboardInputSource<C>,
    verifier: &mut dyn Verifier,
    now_ns: u64,
) -> Result<(), Errno>
where
    P: InputSource + ?Sized,
    C: KeyInputChannel,
{
    // The shell will not see the release that ends any gesture in flight.
    seat.shell.yield_pointer(seat.compositor);
    let mut drain = LockedDrain::new();
    while let Some(event) = pointer.poll()? {
        drain.feed(
            seat.lock,
            &event,
            now_ns,
            verifier,
            seat.shell,
            seat.compositor,
        );
    }
    while let Some((event, _)) = seat.shell.poll_key(keyboard, seat.compositor, now_ns)? {
        drain.feed(
            seat.lock,
            &event,
            now_ns,
            verifier,
            seat.shell,
            seat.compositor,
        );
    }
    Ok(())
}

/// Drain the seat into nothing but the cursor's position and the seat's
/// modifier state: the wake that takes the screensaver down reaches nothing
/// behind it.
///
/// # Errors
///
/// A channel's fault.
pub fn drain_away<P, C>(
    seat: &mut Seat<'_>,
    pointer: &mut P,
    keyboard: &mut KeyboardInputSource<C>,
    now_ns: u64,
) -> Result<(), Errno>
where
    P: InputSource + ?Sized,
    C: KeyInputChannel,
{
    while let Some(event) = pointer.poll()? {
        if let InputEvent::PointerMoved { to } = event {
            let _ = seat.compositor.move_cursor(to);
        }
    }
    while seat
        .shell
        .poll_key(keyboard, seat.compositor, now_ns)?
        .is_some()
    {}
    keyboard.cancel_repeat();
    Ok(())
}
