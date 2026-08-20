//! The app-ward event hold-back: what the session owes a window whose
//! owner's mailbox was full.
//!
//! An app's event mailbox is a bounded kernel resource
//! ([`tairix_window::EVENT_MAILBOX_CAPACITY`]), so a send into it can be
//! refused with [`Errno::WouldBlock`] whenever the owner is merely slow.
//! Dropping the refused event is right for a *delta* — the next pointer
//! sample supersedes the one before it — and wrong for everything else: a
//! `Resized` the app never sees leaves it laying out at a size the
//! compositor no longer uses, a lost `Focus`/`Minimized`/`CloseRequested`
//! /`DesktopChanged` is a state edge with no second telling, and a lost
//! `FilePicked`/`PickCancelled` strands the window's picker for good
//! (the engine clears its pending pick only once the conclusion is
//! accepted). This module is what the session owes instead.
//!
//! # Shape
//!
//! One ordered queue per `(destination mailbox, window)`. Order within a
//! window is exactly the order the events happened; across an owner's
//! windows nothing is ordered, because nothing in the protocol relates two
//! windows' events. A flush therefore serves an owner's windows
//! round-robin — one event each per pass — so a window with a long backlog
//! cannot starve its sibling's `Resized`.
//!
//! An application-scoped event — an icon-bar click or menu outcome — names
//! no window, so it queues under the destination's `None` slot and takes
//! its turn beside the windows in the same round-robin. It is a *discrete*
//! occurrence: a *New window* row chosen twice must open two windows, so
//! neither folds into the other.
//!
//! # Folding
//!
//! Each kind folds by the rule its own quantity obeys, so a destination
//! that stays behind accumulates work in proportion to what the app still
//! needs to know rather than to how long it was away:
//!
//! * A **state edge** (`Focus`, `Resized`, `RedrawRequested`,
//!   `CloseRequested`, `Minimized`, `DesktopChanged`) is a value the app
//!   converges on, not an occurrence it must witness. A later one
//!   overwrites the held one where it stands, so one window owes at most
//!   one of each.
//! * A **position sample** (`Pointer`/`Moved`) is level-triggered: the
//!   newest supersedes an unbroken run of its predecessors.
//! * A **wheel delta** (`Scrolled`) is additive: a run in one direction
//!   sums. A reversal is a distinct gesture — a tick that clamped at a
//!   range end is not undone by the tick back — and ends the run, decided
//!   by the same `shell::continues` predicate the live drain uses.
//! * Everything else is **discrete** — a key, a button press or release, a
//!   pick conclusion — and every one is owed.
//!
//! # Bound
//!
//! [`HOLD_BACK_CAPACITY`] caps one window's queue. It is a defence, not a
//! capacity to scale: it is what stops an app that never drains from making
//! the desktop hold memory on its behalf, so it stays fixed. The rest of
//! the footprint is already bounded by the client's live windows, each of
//! which costs a mapped frame region orders of magnitude larger than its
//! queue.
//!
//! Overflow evicts the oldest *input* event, never a state edge and never a
//! pick conclusion — and that is total, not a preference: folding leaves a
//! window owing at most six state edges and at most one pick conclusion (a
//! second is refused before it reaches the sink), so a queue at capacity
//! always holds an input event to shed. Oldest-first is also the safe
//! direction for a button: a press is shed before its release, so the app
//! can be left with an unmatched release, never an unmatched press it would
//! hold as a latched grab.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use tairix_abi::window_ipc::{PointerAction, WindowEvent};
use tairix_abi::Errno;

use crate::shell::continues;

/// The most events one window may owe its owner before the oldest input
/// event is shed to make room.
///
/// A security bound rather than a scalable capacity: it is the ceiling on
/// what a client that stops draining can make the session hold. Comfortably
/// above the seven slots folding can leave un-shed, and above the input a
/// user produces in the seconds before the owner is declared unresponsive.
pub const HOLD_BACK_CAPACITY: usize = 64;

/// What became of one event offered to the hold-back.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// It went straight to the destination.
    Sent,
    /// The destination is behind, so the event is owed. Its caller still
    /// has an undeliverable event on its hands, which is the evidence a
    /// responsiveness verdict rests on.
    Owed {
        /// `true` when this is the first thing owed to the destination, so
        /// the caller must arm its room wake; a destination already owing
        /// something is already watched.
        watch: bool,
    },
}

/// What one destination's flush concluded.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Flush {
    /// Everything owed went out; the destination's room wake is no longer
    /// needed.
    Settled,
    /// The mailbox filled again; the rest stays owed and the wake stands.
    Blocked,
    /// The send proved the destination gone; everything owed to it was
    /// discarded.
    Gone,
}

/// The destinations a flush finished with, so the caller can drop the wakes
/// it armed and tear down the clients it lost.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Flushed {
    /// Destinations that owe nothing more.
    pub settled: Vec<u64>,
    /// Destinations whose owner is gone. Their windows go with them; their
    /// wake is dropped like a settled one's.
    pub gone: Vec<u64>,
}

/// What the session owes each `(destination mailbox, window)` pair, the
/// `None` window naming the destination's application-scoped queue.
#[derive(Debug, Default)]
pub struct HoldBack {
    owed: BTreeMap<(u64, Option<u64>), VecDeque<WindowEvent>>,
}

impl HoldBack {
    /// A hold-back owing nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            owed: BTreeMap::new(),
        }
    }

    /// Offer `event` to `send`, and take responsibility for it when the
    /// mailbox refuses it as back-pressure.
    ///
    /// A destination already owed something takes this event unsent:
    /// sending it now would put it ahead of what is queued, and the app
    /// must see its events in the order they happened.
    ///
    /// # Errors
    ///
    /// Any refusal that is not [`Errno::WouldBlock`]: the destination is
    /// gone, or the send is one waiting cannot fix, so holding the event
    /// would strand it.
    pub fn deliver<F>(
        &mut self,
        endpoint: u64,
        event: &WindowEvent,
        send: F,
    ) -> Result<Delivery, Errno>
    where
        F: FnOnce(&WindowEvent) -> Result<(), Errno>,
    {
        if !self.owes(endpoint) {
            match send(event) {
                Ok(()) => return Ok(Delivery::Sent),
                Err(Errno::WouldBlock) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Delivery::Owed {
            watch: self.hold(endpoint, *event),
        })
    }

    /// Take responsibility for `event`, folding it into what `endpoint`
    /// already owes that window where the kind allows, and report whether
    /// this is the first thing owed to `endpoint`.
    fn hold(&mut self, endpoint: u64, event: WindowEvent) -> bool {
        let first = !self.owes(endpoint);
        let queue = self.owed.entry((endpoint, event.window_id())).or_default();
        if !fold(queue, &event) {
            if queue.len() >= HOLD_BACK_CAPACITY {
                shed_oldest_input(queue);
            }
            queue.push_back(event);
        }
        first
    }

    /// Whether `endpoint` is owed anything.
    #[must_use]
    pub fn owes(&self, endpoint: u64) -> bool {
        self.windows_of(endpoint).next().is_some()
    }

    /// How much `endpoint` owes `window_id` — the queue depth the fold and
    /// the bound act on. `None` asks after its application-scoped queue.
    #[must_use]
    pub fn depth(&self, endpoint: u64, window_id: Option<u64>) -> usize {
        self.owed
            .get(&(endpoint, window_id))
            .map_or(0, VecDeque::len)
    }

    /// Discard everything owed to `endpoint` — its owner is gone, so the
    /// events have nowhere to land. Returns `true` when something was
    /// discarded, so the caller knows a wake is armed and must be dropped.
    pub fn forget(&mut self, endpoint: u64) -> bool {
        let windows: Vec<Option<u64>> = self.windows_of(endpoint).collect();
        for window_id in &windows {
            self.owed.remove(&(endpoint, *window_id));
        }
        !windows.is_empty()
    }

    /// Offer everything owed to `send`, destination by destination, and
    /// report which destinations finished.
    ///
    /// Each pass over a destination offers one event per window, so no
    /// window's backlog starves another's. A destination stops at its first
    /// [`Errno::WouldBlock`] — the mailbox filled again — and keeps the
    /// rest owed. An [`Errno::NotFound`] is the owner proving itself gone
    /// and discards the lot. Any other refusal is one the destination
    /// cannot recover from by waiting, so that single event is dropped and
    /// the rest go on; nothing is ever re-offered in a loop.
    pub fn flush<F>(&mut self, mut send: F) -> Flushed
    where
        F: FnMut(u64, &WindowEvent) -> Result<(), Errno>,
    {
        let mut report = Flushed::default();
        for endpoint in self.endpoints() {
            match self.flush_one(endpoint, &mut send) {
                Flush::Settled => report.settled.push(endpoint),
                Flush::Gone => {
                    self.forget(endpoint);
                    report.gone.push(endpoint);
                }
                Flush::Blocked => {}
            }
        }
        report
    }

    /// Drain one destination, round-robin across its windows.
    fn flush_one<F>(&mut self, endpoint: u64, send: &mut F) -> Flush
    where
        F: FnMut(u64, &WindowEvent) -> Result<(), Errno>,
    {
        // Fixed for the whole drain: `send` cannot add to the hold-back, so
        // the only change under it is a queue emptying, which `get_mut`
        // then simply misses.
        let windows: Vec<Option<u64>> = self.windows_of(endpoint).collect();
        loop {
            let mut served = false;
            for window_id in &windows {
                let Some(queue) = self.owed.get_mut(&(endpoint, *window_id)) else {
                    continue;
                };
                let Some(event) = queue.front() else {
                    continue;
                };
                match send(endpoint, event) {
                    Err(Errno::WouldBlock) => return Flush::Blocked,
                    Err(Errno::NotFound) => return Flush::Gone,
                    // Sent, or refused for something waiting cannot fix:
                    // either way this event is answered for and the drain
                    // moves on, so a flush never re-offers one for ever.
                    Ok(()) | Err(_) => {}
                }
                queue.pop_front();
                served = true;
                if queue.is_empty() {
                    self.owed.remove(&(endpoint, *window_id));
                }
            }
            if !served {
                return Flush::Settled;
            }
        }
    }

    /// The destinations currently owed something, each once.
    fn endpoints(&self) -> Vec<u64> {
        let mut seen: Vec<u64> = Vec::new();
        for (endpoint, _) in self.owed.keys() {
            if seen.last() != Some(endpoint) {
                seen.push(*endpoint);
            }
        }
        seen
    }

    /// The windows `endpoint` owes something to, in id order, the `None`
    /// slot (its application-scoped queue) first.
    fn windows_of(&self, endpoint: u64) -> impl Iterator<Item = Option<u64>> + '_ {
        self.owed
            .range((endpoint, None)..=(endpoint, Some(u64::MAX)))
            .map(|((_, window_id), _)| *window_id)
    }
}

/// Fold `next` into what `queue` already owes, reporting `true` when it left
/// nothing new to queue.
fn fold(queue: &mut VecDeque<WindowEvent>, next: &WindowEvent) -> bool {
    if let Some(held) = state_edge_slot(queue, next) {
        *held = *next;
        return true;
    }
    // A sample or a delta folds only into an unbroken run at the tail:
    // anything queued behind it is an occurrence the app must see in order.
    match (queue.back_mut(), *next) {
        (
            Some(WindowEvent::Pointer {
                action: PointerAction::Moved,
                x,
                y,
                ..
            }),
            WindowEvent::Pointer {
                action: PointerAction::Moved,
                x: newest_x,
                y: newest_y,
                ..
            },
        ) => {
            *x = newest_x;
            *y = newest_y;
            true
        }
        (
            Some(WindowEvent::Scrolled {
                dx: sideways,
                dy: downward,
                ..
            }),
            WindowEvent::Scrolled {
                dx: across,
                dy: down,
                ..
            },
        ) if continues(*sideways, across) && continues(*downward, down) => {
            *sideways = sideways.saturating_add(across);
            *downward = downward.saturating_add(down);
            true
        }
        _ => false,
    }
}

/// The held event `next` replaces outright, if `next` is a state edge and
/// one of its kind is already owed.
///
/// A state edge names what *is*, so a window owes at most one of each kind
/// and the newest wins. Replacing where it stands rather than re-queueing at
/// the back keeps the queue bounded without disturbing the order of the
/// occurrences around it.
fn state_edge_slot<'q>(
    queue: &'q mut VecDeque<WindowEvent>,
    next: &WindowEvent,
) -> Option<&'q mut WindowEvent> {
    if !is_state_edge(next) {
        return None;
    }
    queue
        .iter_mut()
        .find(|held| core::mem::discriminant(*held) == core::mem::discriminant(next))
}

/// Whether `event` carries a state the app converges on rather than an
/// occurrence it must witness.
const fn is_state_edge(event: &WindowEvent) -> bool {
    matches!(
        event,
        WindowEvent::Focus { .. }
            | WindowEvent::Resized { .. }
            | WindowEvent::RedrawRequested { .. }
            | WindowEvent::CloseRequested { .. }
            | WindowEvent::Minimized { .. }
            | WindowEvent::DesktopChanged { .. }
    )
}

/// Whether `event` is an input occurrence — the only class overflow sheds.
///
/// Losing one costs the app a keystroke or a pointer action it can act
/// without; losing a state edge or a pick conclusion costs it something it
/// cannot re-derive.
const fn is_input(event: &WindowEvent) -> bool {
    matches!(
        event,
        WindowEvent::Key { .. } | WindowEvent::Pointer { .. } | WindowEvent::Scrolled { .. }
    )
}

/// Make room in a queue at capacity by shedding its oldest input event.
///
/// Folding leaves at most six state edges and one pick conclusion owed per
/// window, so a full queue always holds one; the `None` arm cannot be
/// reached from a queue this module built, and shedding nothing is the
/// fail-safe answer if it ever were.
fn shed_oldest_input(queue: &mut VecDeque<WindowEvent>) {
    if let Some(oldest) = queue.iter().position(is_input) {
        queue.remove(oldest);
    }
}
