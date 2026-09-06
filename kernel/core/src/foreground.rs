//! Controlling-terminal (foreground) ownership: the one state machine that
//! decides which task may drain a terminal's input and receive its cooked-mode
//! `^C`/`^Z` job-control signals (`plans/DISPLAY.md` D5).
//!
//! A *terminal* here is either the kernel console device
//! ([`crate::console::ConsoleDevice`]) or a pseudo-terminal
//! ([`crate::pty::Pty`]); both carry exactly one [`ForegroundOwnership`] and
//! share **this** definition of the ownership rules rather than each keeping a
//! private copy. The `console_foreground` syscall routes to whichever backing a
//! caller's standard stream resolves to.
//!
//! # The rules
//!
//! Ownership only ever moves *down* a spawn chain — inherited and intersected,
//! never widened — because the `console_foreground` handler authorises the new
//! owner as a live child of the caller before ever reaching here. Within that,
//! a transition is permitted only from a position of authority over the slot:
//! the terminal is unowned, or the caller is the recorded granter (re-targeting
//! between its own children), or the caller is the current owner (delegating
//! onward to its own child). Anyone else is refused, so a background task can
//! never take the drain right, and every path fails closed
//! ([`Errno::NotForeground`]).
//!
//! # Interrupt safety
//!
//! The console input filter reads the owner from the UART receive interrupt
//! handler, so the state sits behind an [`IrqSafeSpinLock`]: the hold masks the
//! holding CPU, and every critical section here is a handful of field accesses
//! that call nothing, so a handler can never find the lock held by the task it
//! interrupted and no lock order can invert through it.

use tairix_abi::{Errno, ProcId};
use tairix_kernel_sec::ProcessId;
use tairix_sync::IrqSafeSpinLock;

/// A terminal's foreground owner: the process, and *which instance of it*.
///
/// A pid alone does not name a process for longer than that process lives —
/// task ids are drawn at random, and an id whose task is gone may be drawn
/// again. The ownership therefore records the process-instance identity the
/// owner had when it was granted, and a `^C`/`^Z` aimed at this owner is
/// refused at delivery unless the process holding the pid is still that same
/// instance.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ForegroundOwner {
    /// The owning process.
    pub process: ProcessId,
    /// The instance [`Self::process`] named when the ownership was granted.
    /// [`ProcId::KERNEL`] for a principal the capability table holds no
    /// distinct process instance for.
    pub instance: ProcId,
}

/// A held ownership: who owns the terminal, and who handed it to them.
#[derive(Debug, Copy, Clone)]
struct Ownership {
    owner: ForegroundOwner,
    /// The task that granted the ownership. Only it or the owner itself may
    /// release or re-target, so a background task can never steal the drain
    /// right by clearing the slot.
    granter: ProcessId,
}

/// The controlling (foreground) ownership of one terminal.
pub struct ForegroundOwnership {
    state: IrqSafeSpinLock<Option<Ownership>>,
}

impl Default for ForegroundOwnership {
    fn default() -> Self {
        Self::new()
    }
}

impl ForegroundOwnership {
    /// A fresh, unowned ownership.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: IrqSafeSpinLock::new(None),
        }
    }

    /// Hand the terminal's controlling ownership to `owner`, recording `caller`
    /// as the granter.
    ///
    /// Permitted only from a position of authority over the slot: the terminal
    /// is unowned, `caller` is the current owner (delegating onward), or
    /// `caller` is the recorded granter (re-targeting between its own children).
    ///
    /// # Errors
    ///
    /// [`Errno::NotForeground`] when another task's ownership is in place and
    /// `caller` is neither its granter nor the owner.
    pub fn grant(&self, caller: ProcessId, owner: ForegroundOwner) -> Result<(), Errno> {
        let mut state = self.state.lock();
        if let Some(held) = *state {
            if held.owner.process != caller && held.granter != caller {
                return Err(Errno::NotForeground);
            }
        }
        *state = Some(Ownership {
            owner,
            granter: caller,
        });
        Ok(())
    }

    /// Release the terminal's foreground ownership, returning it to the open,
    /// unowned state.
    ///
    /// Only the recorded granter or the owner itself may release; anyone else
    /// is refused, so a background task cannot open the terminal by clearing the
    /// slot and then draining it. Releasing an already-unowned terminal is an
    /// idempotent success: the granter legitimately clears after its child
    /// exited (the exit path already cleared the slot), and there is nothing an
    /// unauthorised caller could gain from the no-op.
    ///
    /// # Errors
    ///
    /// [`Errno::NotForeground`] when another task's ownership is in place and
    /// `caller` is neither its granter nor the owner.
    pub fn release(&self, caller: ProcessId) -> Result<(), Errno> {
        let mut state = self.state.lock();
        let Some(held) = *state else {
            return Ok(());
        };
        if held.owner.process != caller && held.granter != caller {
            return Err(Errno::NotForeground);
        }
        *state = None;
        Ok(())
    }

    /// Clear the slot if `dead` is its recorded owner.
    ///
    /// The exit path calls this for every terminal when a task ends, and the
    /// read gate calls it when it proves a recorded owner dead, so a terminal is
    /// never wedged behind a task that can no longer read it. Keyed on the pid
    /// alone, which is enough: the slot is cleared while the dying process is
    /// still reclaiming, before its task id is released back to the draw, so no
    /// live successor can be holding it. A slot naming any other task is left
    /// untouched (idempotent).
    pub fn clear_dead(&self, dead: ProcessId) {
        let mut state = self.state.lock();
        if state.is_some_and(|held| held.owner.process == dead) {
            *state = None;
        }
    }

    /// The terminal's current controlling (foreground) owner, if any.
    #[must_use]
    pub fn current(&self) -> Option<ForegroundOwner> {
        self.state.lock().map(|held| held.owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u64) -> ProcessId {
        ProcessId(n)
    }

    /// An owner at instance `n`, so a test can tell two occupants of one pid
    /// apart the way the delivery gate does.
    fn owner(process: u64, instance: u8) -> ForegroundOwner {
        ForegroundOwner {
            process: ProcessId(process),
            instance: ProcId::from_raw([instance; 16]),
        }
    }

    #[test]
    fn a_fresh_ownership_is_unowned() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.current(), None);
    }

    #[test]
    fn granting_an_unowned_terminal_records_owner_and_granter() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        assert_eq!(fg.current(), Some(owner(2, 0xA1)));
    }

    #[test]
    fn the_granter_can_retarget_between_its_children() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        // The same granter re-targets to another child.
        assert_eq!(fg.grant(pid(1), owner(3, 0xA2)), Ok(()));
        assert_eq!(fg.current(), Some(owner(3, 0xA2)));
    }

    #[test]
    fn the_owner_can_delegate_onward() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        // The owner (2) delegates to its own child (4); 2 becomes the granter.
        assert_eq!(fg.grant(pid(2), owner(4, 0xA3)), Ok(()));
        assert_eq!(fg.current(), Some(owner(4, 0xA3)));
        // …and can re-target as the granter now.
        assert_eq!(fg.grant(pid(2), owner(5, 0xA4)), Ok(()));
        assert_eq!(fg.current(), Some(owner(5, 0xA4)));
    }

    #[test]
    fn a_bystander_cannot_take_or_retarget_the_ownership() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        assert_eq!(fg.grant(pid(9), owner(9, 0xA5)), Err(Errno::NotForeground));
        assert_eq!(fg.current(), Some(owner(2, 0xA1)));
    }

    #[test]
    fn a_bystander_cannot_release_the_ownership() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        assert_eq!(fg.release(pid(9)), Err(Errno::NotForeground));
        assert_eq!(fg.current(), Some(owner(2, 0xA1)));
    }

    #[test]
    fn the_owner_and_the_granter_can_each_release() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        assert_eq!(fg.release(pid(2)), Ok(()));
        assert_eq!(fg.current(), None);

        assert_eq!(fg.grant(pid(1), owner(2, 0xA1)), Ok(()));
        assert_eq!(fg.release(pid(1)), Ok(()));
        assert_eq!(fg.current(), None);
    }

    #[test]
    fn releasing_an_unowned_terminal_is_an_idempotent_success() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.release(pid(9)), Ok(()));
        assert_eq!(fg.current(), None);
    }

    #[test]
    fn clear_dead_clears_only_the_matching_owner() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(7, 0xA6)), Ok(()));
        // A different task ending leaves the slot untouched.
        fg.clear_dead(pid(9));
        assert_eq!(fg.current(), Some(owner(7, 0xA6)));
        // The owner ending clears it.
        fg.clear_dead(pid(7));
        assert_eq!(fg.current(), None);
    }

    /// The recorded owner carries the instance the grant saw, so a later
    /// occupant of the same pid is distinguishable from it.
    #[test]
    fn the_owner_carries_the_instance_it_was_granted_at() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), owner(4, 0xB1)), Ok(()));
        let held = fg.current().expect("an owner is recorded");
        assert_eq!(held.process, pid(4));
        assert_ne!(held.instance, owner(4, 0xB2).instance);
        assert_ne!(held.instance, ProcId::KERNEL);
    }
}
