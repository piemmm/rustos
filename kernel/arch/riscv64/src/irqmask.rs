//! This port's interrupt-masking primitive: the `sstatus.SIE` implementation
//! of `lib/sync`'s [`InterruptControl`](tairix_sync::irq::InterruptControl), plugged into every lock a handler and
//! a normal path both touch.
//!
//! Masking belongs to the architecture because only the architecture can do
//! it. It lives here — rather than beside one of its users — so the console
//! transmit queue, the boot audit ring and the kernel heap all mask the same
//! way, and a port that gets the masking wrong gets it wrong in exactly one
//! place.
//!
//! Masking the current hart is what makes such a lock correct against its own
//! interrupt handlers: a handler that fires on a hart already inside the
//! section would otherwise spin on a lock its own interrupted mainline holds
//! and never release it. Contention can then only come from another hart,
//! which releases within its bounded hold.

#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
use tairix_sync::irq::NopInterruptControl;
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
use tairix_sync::irq::{InterruptControl, IrqState};

/// The interrupt-masking primitive host-testable code on this port names.
///
/// On the target it is [`SstatusIrqControl`]. A host build has no supervisor
/// status register and no interrupts to mask, so it is the no-op control, which
/// is what lets the console queue's discipline be unit-tested off-target.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub type PortIrqControl = SstatusIrqControl;

/// See the target variant above.
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
pub type PortIrqControl = NopInterruptControl;

/// Saved `sstatus.SIE` state for [`SstatusIrqControl`].
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[derive(Copy, Clone)]
pub struct SstatusState {
    /// Whether supervisor interrupts were enabled (`sstatus.SIE == 1`) before
    /// the matching [`SstatusIrqControl::disable`].
    irqs_were_enabled: bool,
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
impl IrqState for SstatusState {}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
impl SstatusState {
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

/// Clears `sstatus.SIE` — masking supervisor interrupt *taking* on this hart —
/// for a critical section and restores the exact prior state on release
/// (reentrant: a `disable` while already masked captures a cleared bit whose
/// restore leaves interrupts masked).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub struct SstatusIrqControl;

// SAFETY: `disable` clears `sstatus.SIE` and reads the prior `sstatus`,
// atomically masking S-mode interrupts on this hart and returning the exact
// prior interrupt-enable state; `restore` re-sets `sstatus.SIE` only when it
// was set before, leaving an already-masked state masked (reentrant), exactly
// as the trait requires. Both CSR operations are well-defined in S-mode and
// touch only the interrupt-enable bit.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
unsafe impl InterruptControl for SstatusIrqControl {
    type State = SstatusState;

    fn disable() -> Self::State {
        let prev: u64;
        // SAFETY: `csrrci sstatus, 2` atomically clears `sstatus.SIE` (bit 1)
        // and reads the prior `sstatus` into `prev`; it touches only the
        // interrupt-enable bit.
        unsafe {
            core::arch::asm!("csrrci {0}, sstatus, 2", out(reg) prev, options(nomem, nostack));
        }
        SstatusState {
            irqs_were_enabled: prev & crate::trap::SSTATUS_SIE != 0,
        }
    }

    unsafe fn restore(state: Self::State) {
        if state.irqs_were_enabled {
            // SAFETY: `csrs sstatus` sets only `sstatus.SIE`, re-enabling
            // S-mode interrupts exactly as they were before the paired
            // disable.
            unsafe {
                core::arch::asm!("csrs sstatus, {0}", in(reg) crate::trap::SSTATUS_SIE, options(nomem, nostack));
            }
        }
    }
}

#[cfg(all(test, not(all(target_arch = "riscv64", target_os = "none"))))]
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
