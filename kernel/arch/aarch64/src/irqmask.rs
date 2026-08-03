//! This port's interrupt-masking primitive: the `DAIF` implementation of
//! `lib/sync`'s [`InterruptControl`](tairix_sync::irq::InterruptControl), plugged into every lock a handler and a
//! normal path both touch.
//!
//! Masking belongs to the architecture because only the architecture can do
//! it. It lives here — rather than beside one of its users — so the console
//! transmit queue, the receive gate, the boot audit ring and the kernel heap
//! all mask the same way, and a port that gets the masking wrong gets it
//! wrong in exactly one place.
//!
//! Masking the current CPU is what makes such a lock correct against its own
//! interrupt handlers: a handler that fires on a CPU already inside the
//! section would otherwise spin on a lock its own interrupted mainline holds
//! and never release it. Contention can then only come from another CPU,
//! which releases within its bounded hold.

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
use tairix_sync::irq::NopInterruptControl;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use tairix_sync::irq::{InterruptControl, IrqState};

/// The interrupt-masking primitive host-testable code on this port names.
///
/// On the target it is [`DaifIrqControl`]. A host build has no `DAIF` and no
/// interrupts to mask, so it is the no-op control, which is what lets the
/// console queue's discipline be unit-tested off-target.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub type PortIrqControl = DaifIrqControl;

/// See the target variant above.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub type PortIrqControl = NopInterruptControl;

/// Saved `DAIF` state for [`DaifIrqControl`].
///
/// Held as a `usize` because that is exactly a general-purpose register on
/// this architecture, so it round-trips through the `fn`-pointer seams below
/// without a width conversion anywhere.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[derive(Copy, Clone)]
pub struct DaifState(usize);

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl IrqState for DaifState {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl DaifState {
    /// The saved state as an opaque token, for the `fn`-pointer seams that
    /// cannot name the type (the kernel heap's interrupt-control hooks).
    #[must_use]
    pub const fn as_token(self) -> usize {
        self.0
    }

    /// The inverse of [`Self::as_token`].
    #[must_use]
    pub const fn from_token(token: usize) -> Self {
        Self(token)
    }
}

/// Masks asynchronous interrupts through `DAIF` for a critical section and
/// restores the exact prior state on release (reentrant — an already-masked
/// state round-trips unchanged).
///
/// The section masks IRQ+FIQ, the classic discipline. The debug watchdog build
/// additionally re-clears `DAIF.F` — so its non-maskable Group-0/FIQ
/// self-sample can observe a core wedged inside a section
/// (`plans/WATCHDOG.md`) — but **only** where the boot probe proved a
/// non-maskable FIQ is deliverable to this kernel
/// ([`crate::watchdog::fiq_cadence_enabled`]). Where the probe found FIQ
/// undeliverable (a two-Security-state GIC-400, a Raspberry Pi 4, whose Group
/// 0 belongs to the secure world) FIQ stays masked exactly as in a shippable
/// build.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub struct DaifIrqControl;

// SAFETY: `disable` reads DAIF and sets its IRQ+FIQ mask bits — always
// permitted at EL1, touches no memory — atomically masking asynchronous
// interrupts on this CPU and returning the exact prior state; `restore`
// writes that state back verbatim. A `disable` while already masked returns
// the masked state, whose restore leaves interrupts masked (reentrant),
// exactly as the trait requires. The debug watchdog build re-clears FIQ
// (`DAIF.F`) only when the boot probe proved FIQ is genuinely deliverable to
// this kernel; that is sound because on such a GIC the only Group-0/FIQ
// source is the watchdog self-sample, which reads the interrupted context and
// never takes a lock, so it cannot deadlock against a held critical section.
// Where the probe found FIQ undeliverable (a secure Group 0 on a
// two-Security-state GIC-400) FIQ stays masked, so a held section is never
// exposed to a secure-world Group-0 FIQ the kernel cannot service.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe impl InterruptControl for DaifIrqControl {
    type State = DaifState;

    fn disable() -> Self::State {
        let daif: usize;
        // SAFETY: reading DAIF and setting its mask bits is always permitted
        // at EL1 and touches no memory.
        unsafe {
            core::arch::asm!(
                "mrs {0}, daif",
                "msr daifset, #{mask}",
                out(reg) daif,
                mask = const (crate::exceptions::daif::I | crate::exceptions::daif::F),
                options(nomem, nostack, preserves_flags)
            );
        }
        // The decision is a run-time property of the hardware, not of the
        // build, so it is read from the boot probe rather than a compile-time
        // constant.
        #[cfg(feature = "watchdog-diagnostics")]
        if crate::watchdog::fiq_cadence_enabled() {
            // SAFETY: clearing `DAIF.F` only unmasks FIQ and touches no
            // memory; the probe has confirmed a taken FIQ reaches this kernel,
            // and the self-sample reads the interrupted context and never
            // takes a lock, so it cannot deadlock against the hold.
            unsafe {
                core::arch::asm!(
                    "msr daifclr, #{f}",
                    f = const crate::exceptions::daif::F,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
        DaifState(daif)
    }

    unsafe fn restore(state: Self::State) {
        // SAFETY: writing back the DAIF value captured by `disable` on this
        // CPU restores exactly the prior mask state.
        unsafe {
            core::arch::asm!(
                "msr daif, {0}",
                in(reg) state.0,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

#[cfg(all(test, not(all(target_arch = "aarch64", target_os = "none"))))]
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
