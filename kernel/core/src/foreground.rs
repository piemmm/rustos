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
//! [`ForegroundOwnership::current`] reads the owner through a single atomic
//! load and takes no lock, so the console input filter can call it from the
//! UART RX interrupt handler without risking a self-deadlock against a lock the
//! interrupted task holds. The compound transitions
//! ([`ForegroundOwnership::grant`], [`ForegroundOwnership::release`],
//! [`ForegroundOwnership::clear_dead`]) run under the internal lock, which the
//! interrupt path never takes.

use core::sync::atomic::{AtomicU64, Ordering};

use tairix_abi::Errno;
use tairix_kernel_sec::ProcessId;
use tairix_sync::SpinLock;

/// The [`ForegroundOwnership`] sentinel for "no foreground owner".
///
/// Scheduler task ids are small monotonically increasing values that can never
/// reach `u64::MAX`, so the sentinel is unambiguous.
const FOREGROUND_NONE: u64 = u64::MAX;

/// The controlling (foreground) ownership of one terminal.
///
/// Records the current owner and the task that granted it, guarding the
/// compound transitions with an internal lock while keeping the owner readable
/// lock-free from an interrupt handler (see the module docs).
#[derive(Debug)]
pub struct ForegroundOwnership {
    /// The controlling owner's scheduler task id, or [`FOREGROUND_NONE`] while
    /// unowned. Read lock-free by the interrupt-path input filter; written only
    /// under [`Self::lock`] through the checked transitions below.
    owner: AtomicU64,
    /// The task that granted the current ownership (the parent that handed the
    /// terminal to [`Self::owner`]), or [`FOREGROUND_NONE`] while unowned. Only
    /// this task or the owner itself may release or re-target the ownership, so
    /// a background task can never steal the drain right by clearing the slot.
    granter: AtomicU64,
    /// Serialises the compound transitions (read owner + granter, decide, write
    /// both). The interrupt-path filter never takes this lock — it reads
    /// [`Self::owner`] alone — so the interrupt-safety constraint holds.
    lock: SpinLock<()>,
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
            owner: AtomicU64::new(FOREGROUND_NONE),
            granter: AtomicU64::new(FOREGROUND_NONE),
            lock: SpinLock::new(()),
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
    pub fn grant(&self, caller: ProcessId, owner: ProcessId) -> Result<(), Errno> {
        let _guard = self.lock.lock();
        let current = self.owner.load(Ordering::Acquire);
        let permitted = current == FOREGROUND_NONE
            || current == caller.0
            || self.granter.load(Ordering::Acquire) == caller.0;
        if !permitted {
            return Err(Errno::NotForeground);
        }
        self.granter.store(caller.0, Ordering::Release);
        self.owner.store(owner.0, Ordering::Release);
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
        let _guard = self.lock.lock();
        let current = self.owner.load(Ordering::Acquire);
        if current == FOREGROUND_NONE {
            return Ok(());
        }
        if current != caller.0 && self.granter.load(Ordering::Acquire) != caller.0 {
            return Err(Errno::NotForeground);
        }
        self.owner.store(FOREGROUND_NONE, Ordering::Release);
        self.granter.store(FOREGROUND_NONE, Ordering::Release);
        Ok(())
    }

    /// Clear the slot if `dead` is its recorded owner.
    ///
    /// The exit path calls this for every terminal when a task ends, and the
    /// read gate calls it when it proves a recorded owner dead, so a terminal is
    /// never wedged behind a task that can no longer read it. Task ids are never
    /// reused, so clearing on a proven-dead owner can never displace a live one.
    /// A slot naming any other task is left untouched (idempotent).
    pub fn clear_dead(&self, dead: ProcessId) {
        let _guard = self.lock.lock();
        if self.owner.load(Ordering::Acquire) == dead.0 {
            self.owner.store(FOREGROUND_NONE, Ordering::Release);
            self.granter.store(FOREGROUND_NONE, Ordering::Release);
        }
    }

    /// The terminal's current controlling (foreground) owner, if any.
    ///
    /// A single lock-free atomic load, safe to call from an interrupt handler.
    #[must_use]
    pub fn current(&self) -> Option<ProcessId> {
        match self.owner.load(Ordering::Acquire) {
            FOREGROUND_NONE => None,
            raw => Some(ProcessId(raw)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u64) -> ProcessId {
        ProcessId(n)
    }

    #[test]
    fn a_fresh_ownership_is_unowned() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.current(), None);
    }

    #[test]
    fn granting_an_unowned_terminal_records_owner_and_granter() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
        assert_eq!(fg.current(), Some(pid(2)));
    }

    #[test]
    fn the_granter_can_retarget_between_its_children() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
        // The same granter re-targets to another child.
        assert_eq!(fg.grant(pid(1), pid(3)), Ok(()));
        assert_eq!(fg.current(), Some(pid(3)));
    }

    #[test]
    fn the_owner_can_delegate_onward() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
        // The owner (2) delegates to its own child (4); 2 becomes the granter.
        assert_eq!(fg.grant(pid(2), pid(4)), Ok(()));
        assert_eq!(fg.current(), Some(pid(4)));
        // …and can re-target as the granter now.
        assert_eq!(fg.grant(pid(2), pid(5)), Ok(()));
        assert_eq!(fg.current(), Some(pid(5)));
    }

    #[test]
    fn a_bystander_cannot_take_or_retarget_the_ownership() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
        assert_eq!(fg.grant(pid(9), pid(9)), Err(Errno::NotForeground));
        assert_eq!(fg.current(), Some(pid(2)));
    }

    #[test]
    fn a_bystander_cannot_release_the_ownership() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
        assert_eq!(fg.release(pid(9)), Err(Errno::NotForeground));
        assert_eq!(fg.current(), Some(pid(2)));
    }

    #[test]
    fn the_owner_and_the_granter_can_each_release() {
        let fg = ForegroundOwnership::new();
        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
        assert_eq!(fg.release(pid(2)), Ok(()));
        assert_eq!(fg.current(), None);

        assert_eq!(fg.grant(pid(1), pid(2)), Ok(()));
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
        assert_eq!(fg.grant(pid(1), pid(7)), Ok(()));
        // A different task ending leaves the slot untouched.
        fg.clear_dead(pid(9));
        assert_eq!(fg.current(), Some(pid(7)));
        // The owner ending clears it.
        fg.clear_dead(pid(7));
        assert_eq!(fg.current(), None);
    }
}
