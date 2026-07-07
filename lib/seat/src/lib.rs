//! RustOS arch-neutral seat model (`lib/seat`, `plans/DISPLAY.md` D1).
//!
//! A **seat** is one physical display plus the keyboard and pointer attached
//! to it. This crate owns the pure, host-testable state machine behind seat
//! ownership: who holds the seat, how a hold is granted, released, and
//! forcibly revoked, and where a seat's input is routed while it is (or is
//! not) held. It is the one definition of that state machine — the in-kernel
//! seat registry and the user-space seat manager both build on it and never
//! re-derive it.
//!
//! # The model
//!
//! - A seat is either **unowned** or **held** under a [`Lease`] by exactly
//!   one task ([`SeatOwner`], the kernel-attested task identity — never a
//!   caller-supplied claim).
//! - [`SeatState::acquire`] grants the lease only when no other task holds
//!   it; a second holder is refused with [`SeatError::SeatBusy`], never
//!   displaced. This is what makes "an ordinary task cannot steal focus"
//!   an enforced invariant rather than a documentation claim.
//! - [`SeatState::release`] and every owner-gated access check the caller
//!   against the recorded owner and refuse a non-owner with
//!   [`SeatError::NotOwner`] — a release is not a global "anyone may flip
//!   it back" switch.
//! - An administrator (the seat manager, once it exists) can
//!   [`SeatState::revoke`] a lease out from under a wedged or switched-away
//!   owner. Revocation is **observable**: the evicted owner's next
//!   owner-gated call is refused with the distinct
//!   [`SeatError::SeatRevoked`], so a well-behaved compositor learns it
//!   lost the seat instead of scribbling over the new foreground.
//! - Every successful acquire mints a fresh [`Lease`] carrying a
//!   monotonically increasing generation, so a lease held across a
//!   revoke/reacquire cycle can never be confused with the live one.
//! - While a seat is held, its key edges route to the owner's desktop
//!   channel; while it is unowned (including after a revoke), they route to
//!   the seat's foreground **text console** — never to a stale desktop
//!   channel. [`SeatState::route`] is that decision.
//!
//! Every transition is total: each illegal request maps to a typed
//! [`SeatError`], and no path panics — a denied caller receives an error
//! value and the seat state is unchanged (fail closed).
//!
//! # Where it sits
//!
//! `lib/seat` has no dependencies and is `no_std`: it holds policy-free
//! mechanism only. Identity arrives as the opaque [`SeatOwner`] the kernel
//! derives from its own task table; capability checks (`CAP_DISPLAY`,
//! `CAP_SEAT_ADMIN`) happen in the kernel *before* these methods are
//! reached. The kernel's seat registry (`kernel/core`) hosts one
//! [`SeatState`] per discovered display; the syscall layer maps
//! [`SeatError`] onto ABI error codes.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Identifies one seat: one physical display with its keyboard and pointer.
///
/// Seat identifiers are minted by the kernel's seat registry, one per
/// discovered display node (a text-only build has a single seat with no
/// display). They are stable for the life of the seat object.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SeatId(pub u64);

/// The kernel-attested identity of the task a seat records as its owner.
///
/// This is the seat model's view of the kernel's task id: the caller of an
/// ownership-changing operation is identified by the kernel's per-CPU
/// current-task slot, never by a value the caller supplies. The kernel
/// converts its own task-id type into this newtype at the boundary.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SeatOwner(pub u64);

/// Index of a text console within a seat's console set.
///
/// The seat's *foreground* console is the one an unowned seat's key edges
/// drain to.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ConsoleIndex(pub u32);

/// A granted seat hold: the recorded owner plus the generation that makes
/// this grant distinguishable from every earlier one.
///
/// The generation increases on every successful [`SeatState::acquire`], so
/// a holder that was revoked and later reacquired holds a *different*
/// lease; a stale copy of the old one can never be mistaken for the live
/// grant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Lease {
    /// The task the seat is held by.
    pub owner: SeatOwner,
    /// Monotonic grant counter for this seat; starts at 1 and never
    /// repeats.
    pub generation: u64,
}

/// Typed refusal of an illegal seat transition or access.
///
/// Every operation on [`SeatState`] is total: an illegal request returns
/// one of these values and leaves the seat unchanged. The syscall layer
/// maps them onto ABI error codes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SeatError {
    /// Another task currently holds the seat; the acquire is refused
    /// rather than displacing the holder.
    SeatBusy,
    /// The requested acquire came from the task that already holds the
    /// seat; a double acquire is a caller bug, surfaced rather than
    /// silently succeeding.
    AlreadyOwner,
    /// The caller is not the recorded owner of the seat.
    NotOwner,
    /// The seat has no owner, so there is nothing to revoke.
    SeatUnowned,
    /// The caller's lease was forcibly revoked; this distinct refusal is
    /// how the evicted owner learns it lost the seat.
    SeatRevoked,
}

/// Where a seat's key edges are delivered right now.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// The seat is unowned: input drains to its foreground text console.
    Text(ConsoleIndex),
    /// The seat is held: input drains to the owner's desktop channel.
    Desktop(SeatOwner),
}

/// The lease half of a seat's state.
///
/// `Revoked` is deliberately distinct from `Unowned`: the seat is equally
/// acquirable in both, but the *evicted* task's next owner-gated call must
/// be refused with [`SeatError::SeatRevoked`] so the loss is observable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LeaseState {
    /// Nobody holds the seat and no revocation is pending acknowledgement.
    Unowned,
    /// Exactly one task holds the seat.
    Held(Lease),
    /// An administrator revoked the lease; the seat is unowned, and
    /// `evicted`'s next owner-gated call sees the distinct refusal.
    Revoked {
        /// The task whose lease was revoked.
        evicted: SeatOwner,
    },
}

/// The complete state of one seat: its lease and its foreground text
/// console.
///
/// This is a pure value type: it performs no capability checks (the kernel
/// gates every entry point before reaching it) and takes no locks (the
/// registry hosting it owns the synchronisation).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeatState {
    lease: LeaseState,
    foreground: ConsoleIndex,
    /// Generation of the most recently minted lease; the next acquire
    /// mints `generations + 1`.
    generations: u64,
}

impl SeatState {
    /// A fresh, unowned seat whose input drains to `foreground`.
    #[must_use]
    pub const fn new(foreground: ConsoleIndex) -> Self {
        Self {
            lease: LeaseState::Unowned,
            foreground,
            generations: 0,
        }
    }

    /// Grant the seat to `task`, minting a fresh [`Lease`].
    ///
    /// Succeeds only when no other task holds the seat. A pending
    /// revocation does not block a new acquire — an acquire is an
    /// explicit, capability-checked new claim, so it clears the
    /// revocation marker even when the claimant is the evicted task
    /// itself (reacquiring *is* acknowledging the loss).
    ///
    /// # Errors
    ///
    /// - [`SeatError::SeatBusy`] — another task holds the seat.
    /// - [`SeatError::AlreadyOwner`] — `task` already holds it.
    pub fn acquire(&mut self, task: SeatOwner) -> Result<Lease, SeatError> {
        match self.lease {
            LeaseState::Held(lease) if lease.owner == task => Err(SeatError::AlreadyOwner),
            LeaseState::Held(_) => Err(SeatError::SeatBusy),
            LeaseState::Unowned | LeaseState::Revoked { .. } => {
                self.generations += 1;
                let lease = Lease {
                    owner: task,
                    generation: self.generations,
                };
                self.lease = LeaseState::Held(lease);
                Ok(lease)
            }
        }
    }

    /// Release the seat held by `task`, returning it to the text
    /// foreground.
    ///
    /// # Errors
    ///
    /// - [`SeatError::NotOwner`] — `task` does not hold the seat (it is
    ///   unowned, or held by another task).
    /// - [`SeatError::SeatRevoked`] — `task`'s lease was revoked; the
    ///   refusal acknowledges the pending revocation, so later calls by
    ///   `task` see plain [`SeatError::NotOwner`].
    pub fn release(&mut self, task: SeatOwner) -> Result<(), SeatError> {
        match self.lease {
            LeaseState::Held(lease) if lease.owner == task => {
                self.lease = LeaseState::Unowned;
                Ok(())
            }
            LeaseState::Revoked { evicted } if evicted == task => {
                self.lease = LeaseState::Unowned;
                Err(SeatError::SeatRevoked)
            }
            LeaseState::Held(_) | LeaseState::Unowned | LeaseState::Revoked { .. } => {
                Err(SeatError::NotOwner)
            }
        }
    }

    /// Forcibly revoke the current lease (administrator path).
    ///
    /// Returns the evicted owner so the caller can audit-log the
    /// decision. The seat becomes acquirable immediately; the evicted
    /// task's next owner-gated call is refused with
    /// [`SeatError::SeatRevoked`].
    ///
    /// # Errors
    ///
    /// - [`SeatError::SeatUnowned`] — no lease is held, so there is
    ///   nothing to revoke.
    pub fn revoke(&mut self) -> Result<SeatOwner, SeatError> {
        match self.lease {
            LeaseState::Held(lease) => {
                self.lease = LeaseState::Revoked {
                    evicted: lease.owner,
                };
                Ok(lease.owner)
            }
            LeaseState::Unowned | LeaseState::Revoked { .. } => Err(SeatError::SeatUnowned),
        }
    }

    /// Check an owner-gated access (present a frame, drain the desktop
    /// keyboard channel) by `task` against the live lease.
    ///
    /// # Errors
    ///
    /// - [`SeatError::SeatRevoked`] — `task`'s lease was revoked.
    /// - [`SeatError::NotOwner`] — `task` does not hold the seat.
    pub fn access(&self, task: SeatOwner) -> Result<Lease, SeatError> {
        match self.lease {
            LeaseState::Held(lease) if lease.owner == task => Ok(lease),
            LeaseState::Revoked { evicted } if evicted == task => Err(SeatError::SeatRevoked),
            LeaseState::Held(_) | LeaseState::Unowned | LeaseState::Revoked { .. } => {
                Err(SeatError::NotOwner)
            }
        }
    }

    /// The task currently holding the seat, if any.
    #[must_use]
    pub fn owner(&self) -> Option<SeatOwner> {
        match self.lease {
            LeaseState::Held(lease) => Some(lease.owner),
            LeaseState::Unowned | LeaseState::Revoked { .. } => None,
        }
    }

    /// Where this seat's key edges are delivered right now.
    ///
    /// A held seat routes to the owner's desktop channel; an unowned seat
    /// — including one whose lease was just revoked — routes to the
    /// foreground text console, never to a stale desktop channel.
    #[must_use]
    pub fn route(&self) -> Route {
        match self.lease {
            LeaseState::Held(lease) => Route::Desktop(lease.owner),
            LeaseState::Unowned | LeaseState::Revoked { .. } => Route::Text(self.foreground),
        }
    }

    /// The text console an unowned seat's input drains to.
    #[must_use]
    pub const fn foreground_console(&self) -> ConsoleIndex {
        self.foreground
    }

    /// Retarget the seat's foreground text console (the console-switch
    /// half of a foreground handoff). Takes effect immediately for an
    /// unowned seat; a held seat keeps routing to its owner until the
    /// lease ends.
    pub fn set_foreground_console(&mut self, console: ConsoleIndex) {
        self.foreground = console;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WM: SeatOwner = SeatOwner(7);
    const INTRUDER: SeatOwner = SeatOwner(9);
    const CONSOLE: ConsoleIndex = ConsoleIndex(0);

    fn held_seat() -> (SeatState, Lease) {
        let mut seat = SeatState::new(CONSOLE);
        let lease = seat.acquire(WM).expect("fresh seat is acquirable");
        (seat, lease)
    }

    #[test]
    fn a_fresh_seat_is_unowned_and_routes_to_the_text_foreground() {
        let seat = SeatState::new(CONSOLE);
        assert_eq!(seat.owner(), None);
        assert_eq!(seat.route(), Route::Text(CONSOLE));
    }

    #[test]
    fn acquire_records_the_owner_and_routes_to_the_desktop() {
        let (seat, lease) = held_seat();
        assert_eq!(lease.owner, WM);
        assert_eq!(seat.owner(), Some(WM));
        assert_eq!(seat.route(), Route::Desktop(WM));
    }

    #[test]
    fn a_second_task_cannot_steal_a_held_seat() {
        let (mut seat, _) = held_seat();
        assert_eq!(seat.acquire(INTRUDER), Err(SeatError::SeatBusy));
        assert_eq!(seat.owner(), Some(WM));
    }

    #[test]
    fn a_double_acquire_by_the_owner_is_refused() {
        let (mut seat, _) = held_seat();
        assert_eq!(seat.acquire(WM), Err(SeatError::AlreadyOwner));
        assert_eq!(seat.owner(), Some(WM));
    }

    #[test]
    fn release_returns_the_seat_to_the_text_foreground() {
        let (mut seat, _) = held_seat();
        assert_eq!(seat.release(WM), Ok(()));
        assert_eq!(seat.owner(), None);
        assert_eq!(seat.route(), Route::Text(CONSOLE));
    }

    #[test]
    fn a_non_owner_cannot_release_a_held_seat() {
        let (mut seat, _) = held_seat();
        assert_eq!(seat.release(INTRUDER), Err(SeatError::NotOwner));
        assert_eq!(seat.owner(), Some(WM));
        assert_eq!(seat.route(), Route::Desktop(WM));
    }

    #[test]
    fn releasing_an_unowned_seat_is_refused() {
        let mut seat = SeatState::new(CONSOLE);
        assert_eq!(seat.release(WM), Err(SeatError::NotOwner));
    }

    #[test]
    fn revoke_evicts_the_owner_and_returns_input_to_text() {
        let (mut seat, _) = held_seat();
        assert_eq!(seat.revoke(), Ok(WM));
        assert_eq!(seat.owner(), None);
        assert_eq!(seat.route(), Route::Text(CONSOLE));
    }

    #[test]
    fn revoking_an_unowned_seat_is_refused() {
        let mut seat = SeatState::new(CONSOLE);
        assert_eq!(seat.revoke(), Err(SeatError::SeatUnowned));
        let (mut seat, _) = held_seat();
        seat.revoke().expect("held seat revokes");
        assert_eq!(seat.revoke(), Err(SeatError::SeatUnowned));
    }

    #[test]
    fn the_evicted_owner_observes_the_revocation_on_access() {
        let (mut seat, _) = held_seat();
        seat.revoke().expect("held seat revokes");
        assert_eq!(seat.access(WM), Err(SeatError::SeatRevoked));
        assert_eq!(seat.access(INTRUDER), Err(SeatError::NotOwner));
    }

    #[test]
    fn the_evicted_owner_observes_the_revocation_once_on_release() {
        let (mut seat, _) = held_seat();
        seat.revoke().expect("held seat revokes");
        assert_eq!(seat.release(WM), Err(SeatError::SeatRevoked));
        assert_eq!(seat.release(WM), Err(SeatError::NotOwner));
        assert_eq!(seat.access(WM), Err(SeatError::NotOwner));
    }

    #[test]
    fn a_revoked_seat_is_acquirable_and_the_marker_clears() {
        let (mut seat, _) = held_seat();
        seat.revoke().expect("held seat revokes");
        let lease = seat.acquire(INTRUDER).expect("revoked seat is acquirable");
        assert_eq!(lease.owner, INTRUDER);
        assert_eq!(seat.route(), Route::Desktop(INTRUDER));
        assert_eq!(seat.access(WM), Err(SeatError::NotOwner));
    }

    #[test]
    fn the_evicted_owner_may_explicitly_reacquire() {
        let (mut seat, first) = held_seat();
        seat.revoke().expect("held seat revokes");
        let second = seat.acquire(WM).expect("an acquire is a new claim");
        assert_eq!(second.owner, WM);
        assert!(second.generation > first.generation);
        assert_eq!(seat.access(WM), Ok(second));
    }

    #[test]
    fn generations_increase_across_every_grant() {
        let mut seat = SeatState::new(CONSOLE);
        let a = seat.acquire(WM).expect("acquire");
        seat.release(WM).expect("release");
        let b = seat.acquire(INTRUDER).expect("acquire");
        seat.revoke().expect("revoke");
        let c = seat.acquire(WM).expect("acquire");
        assert_eq!((a.generation, b.generation, c.generation), (1, 2, 3));
    }

    #[test]
    fn access_by_the_owner_returns_the_live_lease() {
        let (seat, lease) = held_seat();
        assert_eq!(seat.access(WM), Ok(lease));
        assert_eq!(seat.access(INTRUDER), Err(SeatError::NotOwner));
    }

    #[test]
    fn access_to_an_unowned_seat_is_refused() {
        let seat = SeatState::new(CONSOLE);
        assert_eq!(seat.access(WM), Err(SeatError::NotOwner));
    }

    #[test]
    fn the_foreground_console_is_retargetable() {
        let mut seat = SeatState::new(CONSOLE);
        let other = ConsoleIndex(2);
        seat.set_foreground_console(other);
        assert_eq!(seat.foreground_console(), other);
        assert_eq!(seat.route(), Route::Text(other));
        seat.acquire(WM).expect("acquire");
        assert_eq!(seat.route(), Route::Desktop(WM));
        seat.release(WM).expect("release");
        assert_eq!(seat.route(), Route::Text(other));
    }
}
