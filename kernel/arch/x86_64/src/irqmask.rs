//! This port's interrupt-masking primitive: the `RFLAGS.IF` implementation of
//! `lib/sync`'s [`InterruptControl`](tairix_sync::irq::InterruptControl), plugged into every lock a handler and a
//! normal path both touch.
//!
//! Masking belongs to the architecture because only the architecture can do
//! it. It lives here — rather than beside one of its users — so the console
//! transmit queue, the receive gate and the kernel heap all mask the same way,
//! and a port that gets the masking wrong gets it wrong in exactly one place.
//!
//! Masking the current CPU is what makes such a lock correct against its own
//! interrupt handlers: a handler that fires on a CPU already inside the
//! section would otherwise spin on a lock its own interrupted mainline holds
//! and never release it. Contention can then only come from another CPU,
//! which releases within its bounded hold.

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
use tairix_sync::irq::NopInterruptControl;
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use tairix_sync::irq::{InterruptControl, IrqState};

/// The interrupt-masking primitive host-testable code on this port names.
///
/// On the target it is [`RflagsIrqControl`]. A host build has no `RFLAGS` to
/// touch and no interrupts to mask, so it is the no-op control, which is what
/// lets the console queue's discipline be unit-tested off-target.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub type PortIrqControl = RflagsIrqControl;

/// See the target variant above.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub type PortIrqControl = NopInterruptControl;

/// Saved `RFLAGS.IF` state for [`RflagsIrqControl`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[derive(Copy, Clone)]
pub struct RflagsState {
    /// Whether maskable interrupts were enabled (`RFLAGS.IF == 1`) before the
    /// matching [`RflagsIrqControl::disable`].
    irqs_were_enabled: bool,
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl IrqState for RflagsState {}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
impl RflagsState {
    /// The saved state as an opaque token, for the `fn`-pointer seams that
    /// cannot name the type (the kernel heap's interrupt-control hooks).
    #[must_use]
    pub const fn as_token(self) -> usize {
        if self.irqs_were_enabled {
            1
        } else {
            0
        }
    }

    /// The inverse of [`Self::as_token`].
    #[must_use]
    pub const fn from_token(token: usize) -> Self {
        Self {
            irqs_were_enabled: token != 0,
        }
    }
}

/// Masks maskable interrupts on the current CPU (`cli`) for a critical section
/// and restores the exact prior `RFLAGS.IF` on release (reentrant — a
/// `disable` while already masked captures a cleared flag, whose restore
/// leaves interrupts masked).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub struct RflagsIrqControl;

// SAFETY: `disable` reads RFLAGS (pushfq/pop) and masks IF (cli), atomically
// masking asynchronous interrupts on this CPU and returning the exact prior IF
// state; `restore` re-enables IF (sti) only when it was set before, leaving an
// already-masked state masked (reentrant), exactly as the trait requires. Both
// instructions are well-defined in ring 0 and touch only the interrupt flag.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe impl InterruptControl for RflagsIrqControl {
    type State = RflagsState;

    fn disable() -> Self::State {
        let flags: u64;
        // SAFETY: pushfq/pop reads RFLAGS into a scratch register; cli then
        // masks IF. Well-defined in ring 0; touches only the flag we read.
        unsafe {
            core::arch::asm!("pushfq", "pop {0}", "cli", out(reg) flags, options(preserves_flags));
        }
        RflagsState {
            irqs_were_enabled: flags & RFLAGS_IF != 0,
        }
    }

    unsafe fn restore(state: Self::State) {
        if state.irqs_were_enabled {
            // SAFETY: re-enable IF exactly as it was before `disable`; `sti`
            // touches only the interrupt flag.
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
        }
    }
}

/// The interrupt-enable flag in `RFLAGS` (bit 9), per the Intel SDM.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const RFLAGS_IF: u64 = 1 << 9;

#[cfg(all(test, not(all(target_arch = "x86_64", target_os = "none"))))]
mod tests {
    use tairix_sync::IrqSafeSpinLock;

    use super::PortIrqControl;

    #[test]
    fn the_port_control_guards_a_lock_the_host_build_can_exercise() {
        // The host stands in for the target only in that it proves the wiring
        // type-checks and the guard releases; the masking itself is target
        // hardware behaviour.
        let lock: IrqSafeSpinLock<u32, PortIrqControl> = IrqSafeSpinLock::new(7);
        assert_eq!(*lock.lock(), 7);
        assert!(lock.try_lock().is_some());
    }
}
