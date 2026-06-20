//! EL1 exception handling: vector-table install, IRQ dispatch, and the
//! synchronous-fault path.
//!
//! The assembly vector table (`vectors.s`, included via `global_asm!` in
//! `lib.rs`) tags every exception with a numeric *kind* and calls
//! `rustos_aarch64_trap_handler`. This module:
//!
//! * `init_vectors` points `VBAR_EL1` at the table.
//! * `enable_irq` unmasks IRQs at the PE (`DAIF.I`), the aarch64
//!   analogue of riscv64's `sstatus.SIE` enable — kept separate from
//!   arming the timer so the caller controls exactly when ticks begin.
//! * `rustos_aarch64_trap_handler` dispatches an IRQ to the GIC
//!   acknowledge → timer/SGI → end-of-interrupt handshake, routes an EL0
//!   `svc` (lower-EL synchronous exception) to the installed
//!   [`crate::syscall_entry`] dispatch callback, and routes any other
//!   synchronous exception to the installed [`crate::fault`] handler (or
//!   fails closed by parking the CPU).
//!
//! # EL0 `svc` syscall dispatch
//!
//! The trampoline (`vectors.s`) passes the saved register frame to the
//! handler; on a lower-EL synchronous `svc` the handler marshals the
//! saved `x0`–`x5`/`x8` into the architecture-neutral
//! `[u64; SYSCALL_MAX_ARGS]` layout (via
//! [`crate::syscall_entry::syscall_frame_from_saved`]), forwards them to
//! the installed dispatch callback, and writes the result back into the
//! saved `x0` slot so the `eret` returns it to EL0. This is the aarch64
//! analogue of riscv64's `ecall` dispatch; the architecture-neutral
//! validation / capability / audit dispatcher lives in `kernel/syscall`,
//! never re-implemented here.
//!
//! The handler and the CSR writes are freestanding-only; the exception
//! *kind* constants and their classification build on the host so their
//! unit tests run under `cargo test`.

/// Exception kinds the vector table tags each entry with, matching the
/// `mov x0, #N` immediates in `vectors.s` (entry index `0..16`).
pub mod kind {
    /// Current EL with SP0 — IRQ. Not used (the kernel runs on `SP_EL1`)
    /// but classified so a stray entry is still dispatched correctly.
    pub const CUR_SP0_IRQ: u64 = 1;
    /// Current EL with SPx — Synchronous (a kernel-mode fault).
    pub const CUR_SPX_SYNC: u64 = 4;
    /// Current EL with SPx — IRQ (the timer / SGI path).
    pub const CUR_SPX_IRQ: u64 = 5;
    /// Lower EL (AArch64) — Synchronous (an EL0 `svc` or user fault).
    pub const LOWER_SYNC: u64 = 8;
    /// Lower EL (AArch64) — IRQ.
    pub const LOWER_IRQ: u64 = 9;
}

/// `true` iff `kind` denotes an IRQ entry (from any EL the kernel may be
/// interrupted in).
#[must_use]
pub const fn is_irq(kind: u64) -> bool {
    matches!(
        kind,
        kind::CUR_SP0_IRQ | kind::CUR_SPX_IRQ | kind::LOWER_IRQ
    )
}

/// `true` iff `kind` denotes a synchronous-exception entry the fault path
/// handles (current-EL or lower-EL synchronous).
#[must_use]
pub const fn is_sync(kind: u64) -> bool {
    matches!(kind, kind::CUR_SPX_SYNC | kind::LOWER_SYNC)
}

// --- Device-IRQ dispatch hook -------------------------------------
//
// The timer PPI ([`crate::preempt::TIMER_PPI`]) has its own dedicated
// path; every *other* acknowledged INTID (a device's shared-peripheral
// interrupt routed through the GIC by [`crate::gic::route_spi`]) is
// forwarded to a set-once dispatch callback the binary installs. This
// mirrors riscv64's `trap::set_trap_dispatch` external-interrupt seam:
// the callback claims/services the source and forwards it to
// `rustos_kernel_irq::IrqTable::fire` (which masks the GIC line before
// the waiter observes the wake — `docs/src/security/irq.md`), while the
// GIC end-of-interrupt handshake stays in [`handle_irq`]. The slot is
// set-once, backed by an atomic so the IRQ path reads it without a lock
// (`AGENTS.md` §2.1 — no global mutable state; this is an immutable,
// publish-once pointer).

use core::sync::atomic::{AtomicUsize, Ordering};

/// Signature of the installed device-IRQ dispatcher, invoked from the
/// IRQ path with the acknowledged GIC INTID. Like the timer callback it
/// is a bare `extern "C" fn` (no captured environment) so it is safe to
/// call from interrupt context.
pub type DeviceIrqDispatchFn = extern "C" fn(u32);

/// Slot holding the installed dispatcher as a raw function pointer
/// (`0` = none).
static DEVICE_IRQ_DISPATCH_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_device_irq_dispatch`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetDispatchError {
    /// A dispatcher was already published; the slot is set-once per boot
    /// (`AGENTS.md` §2.1).
    AlreadyInstalled,
}

/// Install the device-IRQ dispatcher.
///
/// # Errors
///
/// [`SetDispatchError::AlreadyInstalled`] on the second publish.
pub fn set_device_irq_dispatch(cb: DeviceIrqDispatchFn) -> Result<(), SetDispatchError> {
    let raw = cb as usize;
    DEVICE_IRQ_DISPATCH_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetDispatchError::AlreadyInstalled)
}

/// Address of the installed device-IRQ dispatcher (`0` if none).
/// Test/diagnostic observer.
#[must_use]
pub fn device_irq_dispatch_addr() -> usize {
    DEVICE_IRQ_DISPATCH_FN.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_device_irq_dispatch_for_tests() {
    // Test-only: lets back-to-back host tests reinstall a dispatcher.
    // Production code never clears the slot (`AGENTS.md` §2.1).
    DEVICE_IRQ_DISPATCH_FN.store(0, Ordering::Release);
}

/// Invoke the installed device-IRQ dispatcher with `intid`, if any.
///
/// A device interrupt that arrives before the binary installed a
/// dispatcher is left unserviced here (the GIC line stays active until
/// [`handle_irq`]'s end-of-interrupt); the boot path installs the
/// dispatcher before routing any device SPI, so this is not reached in
/// practice (`AGENTS.md` §5.4.5 — fail closed rather than guess).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn dispatch_device_irq(intid: u32) {
    let raw = DEVICE_IRQ_DISPATCH_FN.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: every value stored into the slot round-trips a valid
        // `DeviceIrqDispatchFn` through `set_device_irq_dispatch`;
        // function pointers are `usize`-sized so the transmute is
        // lossless, and the callback carries no captured environment.
        let cb: DeviceIrqDispatchFn =
            unsafe { core::mem::transmute::<usize, DeviceIrqDispatchFn>(raw) };
        cb(intid);
    }
}

// --- Freestanding vector install + dispatch -----------------------

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
extern "C" {
    /// EL1 vector table published by `vectors.s`. Installed into
    /// `VBAR_EL1` by [`init_vectors`]; never called from Rust.
    fn rustos_aarch64_vectors();
}

/// Point `VBAR_EL1` at the exception vector table.
///
/// # Safety
///
/// Must be called once, on the boot CPU, after a stack is established
/// and before interrupts are unmasked. The table is 2 KiB aligned by
/// `vectors.s`, satisfying the `VBAR_EL1` alignment requirement.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init_vectors() {
    let base = rustos_aarch64_vectors as *const () as u64;
    // SAFETY: `base` is the 2 KiB-aligned address of the asm vector
    // table; writing it to `VBAR_EL1` has no side effect beyond the
    // system register.
    unsafe {
        core::arch::asm!("msr VBAR_EL1, {}", in(reg) base, options(nomem, nostack));
    }
}

/// Unmask IRQs at the PE (`DAIF.I`), allowing the CPU to take interrupts.
///
/// Like riscv64's `sstatus.SIE` enable, this is deliberately separate
/// from arming a source ([`crate::preempt::init_local_preempt`]): the
/// caller unmasks only once the vector table and the source are in
/// place.
///
/// # Safety
///
/// The caller must have installed the vector table ([`init_vectors`])
/// and any IRQ handler state (the timer callback) first; otherwise an
/// in-flight interrupt would dispatch through an unset slot.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn enable_irq() {
    // SAFETY: clearing `DAIF.I` (the IRQ mask, immediate bit 1) unmasks
    // IRQs; it has no other side effect.
    unsafe {
        core::arch::asm!("msr DAIFClr, #2", options(nomem, nostack));
    }
}

/// Mask IRQ *taking* at the PE (`DAIF.I`), without disturbing a pending
/// interrupt's latch.
///
/// This is the first half of the canonical race-free park (mask → check
/// ready → [`wait_for_interrupt`] → [`enable_irq`]): masking only stops the
/// CPU from *taking* an interrupt, so an enabled source that asserts
/// between the readiness check and the `wfi` stays pending and still wakes
/// the `wfi` — no edge is lost (`AGENTS.md` §2.1 — no unbounded sleep
/// loop). An in-kernel service kthread blocking on a device line uses it to
/// close the check-then-park window.
///
/// # Safety
///
/// Setting `DAIF.I` only changes the interrupt mask; it has no other side
/// effect. The caller must pair it with [`enable_irq`] (or
/// [`wait_for_interrupt`] then [`enable_irq`]) so IRQ taking is restored.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn mask_irq() {
    // SAFETY: setting `DAIF.I` (immediate bit 1) masks IRQ taking; it has
    // no other side effect and leaves any pending interrupt latched.
    unsafe {
        core::arch::asm!("msr DAIFSet, #2", options(nomem, nostack));
    }
}

/// Park the calling CPU on `wfi` until an enabled interrupt is pending.
///
/// `wfi` wakes on a pending *enabled* interrupt even while IRQ taking is
/// masked ([`mask_irq`]), which is exactly what makes the race-free park
/// correct: the caller masks taking, re-checks the readiness condition, and
/// only parks here if it is still unmet — a completion that lands in that
/// window leaves the line pending and wakes the `wfi`. It is a hint with no
/// architectural side effects, so a spurious wake merely returns to the
/// caller's poll loop.
///
/// # Safety
///
/// `wfi` is a hint with no architectural side effects. The caller must hold
/// IRQ taking masked ([`mask_irq`]) across the readiness check and this
/// park, then restore it with [`enable_irq`], so the woken interrupt is
/// actually dispatched.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn wait_for_interrupt() {
    // SAFETY: `wfi` is a hint instruction; it suspends the CPU until an
    // enabled interrupt is pending and has no other architectural effect.
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// Read the `ESR_EL1` exception syndrome.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn read_esr() -> u64 {
    let esr: u64;
    // SAFETY: reading `ESR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ESR_EL1", out(reg) esr, options(nomem, nostack, preserves_flags));
    }
    esr
}

/// Read the `FAR_EL1` faulting virtual address.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn read_far() -> u64 {
    let far: u64;
    // SAFETY: reading `FAR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, FAR_EL1", out(reg) far, options(nomem, nostack, preserves_flags));
    }
    far
}

/// Read the `ELR_EL1` faulting / return PC.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn read_elr() -> u64 {
    let elr: u64;
    // SAFETY: reading `ELR_EL1` has no side effects.
    unsafe {
        core::arch::asm!("mrs {}, ELR_EL1", out(reg) elr, options(nomem, nostack, preserves_flags));
    }
    elr
}

/// Handle an IRQ: acknowledge the GIC, dispatch the timer PPI to the
/// scheduler-tick path, complete the interrupt, then — for a timer tick
/// taken from EL0 — drive the preemption point.
///
/// `from_el0` is `true` when the interrupted context was EL0 user mode
/// (the `LOWER_IRQ` vector). It gates **preemption only**: a timer tick
/// taken in EL1 (`CUR_SPX_IRQ`) still runs the scheduler-tick accounting
/// and re-arms the timer, but it never switches the current task away —
/// the kernel is non-preemptible, so a half-completed kernel critical
/// section (a held `lib/sync` lock, an in-flight syscall) is never
/// abandoned mid-flight (`AGENTS.md` §4 SMP watch-out / §2.1 no hacks).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn handle_irq(from_el0: bool) {
    let intid = crate::gic::acknowledge();
    if intid == crate::gic::SPURIOUS_INTID {
        // Spurious read: nothing pending, and the GIC requires no EOI.
        return;
    }
    // The running CPU's dense id, recovered from `MPIDR_EL1`, drives both
    // the per-CPU timer slot and the IPI callback (`AGENTS.md` §2.2 — one
    // identity source).
    let cpu = crate::smp::current_cpu_index();
    if intid == crate::preempt::TIMER_PPI {
        crate::preempt::on_timer_interrupt(cpu);
    } else if intid < crate::gic::MIN_SPI_INTID {
        // INTIDs 0..32 are SGIs/PPIs; INTID 0..16 are the inter-processor
        // SGIs. A delivered directed IPI (`crate::kernel_arch` `send_ipi`
        // → `gic::send_sgi`) surfaces here — run the reschedule callback.
        crate::preempt::on_ipi_interrupt(cpu);
    } else {
        // Any other acknowledged INTID is a device interrupt (a GIC SPI
        // routed by `crate::gic::route_spi`); forward it to the installed
        // device-IRQ dispatcher (which services the source and runs the
        // `kernel/irq` mask-before-wake path).
        dispatch_device_irq(intid);
    }
    // Complete every acknowledged interrupt (timer, SGI/IPI, or device)
    // so the CPU interface does not wedge with an active priority.
    crate::gic::end_of_interrupt(intid);

    // Preempt **after** the end-of-interrupt handshake: the installed
    // callback may context-switch away to another task and not return to
    // this frame for a long time, so the timer line must already be
    // deactivated (otherwise the GIC would hold an active priority across
    // the switch and block every further interrupt on this CPU). Only a
    // timer tick taken from EL0 preempts; the callback suspends the
    // running user task back to the scheduler (the involuntary analogue
    // of a `yield` syscall) and returns here when the task is next
    // dispatched, after which the trampoline `eret`s to the interrupted
    // EL0 context. A build that armed the timer without installing the
    // callback keeps cooperative scheduling (`AGENTS.md` §2.9 fail-safe).
    if from_el0 && intid == crate::preempt::TIMER_PPI {
        crate::preempt::on_el0_preempt_point(cpu);
    }
}

/// Rust entry invoked by the asm vector trampoline with the exception
/// `kind` and the saved-register-`frame` base.
///
/// `frame` points at the `[u64; SAVED_GPRS]` register frame the
/// trampoline built (`x0`–`x30` at indices `0..=30`). The syscall path
/// reads the EL0 `svc` registers from it and writes the result back into
/// the `x0` slot; the trampoline then restores from the same frame, so
/// `eret` returns the result to the EL0 caller.
///
/// # Safety
///
/// Only callable from `rustos_aarch64_trap_common`, which has saved the
/// interrupted GP registers (so `frame` is a valid `[u64; SAVED_GPRS]`
/// for the duration of this call) and tagged the exception kind. An IRQ
/// or a serviced `svc` returns (the trampoline `eret`s); a fault diverges
/// (the installed handler or the park never returns).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_aarch64_trap_handler(kind: u64, frame: *mut u64) {
    if is_irq(kind) {
        // `LOWER_IRQ` is the only IRQ entry whose interrupted context was
        // EL0 user mode; `CUR_SP0_IRQ`/`CUR_SPX_IRQ` interrupted EL1
        // kernel code, which is never preempted (see [`handle_irq`]).
        handle_irq(kind == kind::LOWER_IRQ);
        return;
    }

    if is_sync(kind) {
        let esr = read_esr();

        // An `svc` from a lower EL (AArch64) is the EL0 syscall path:
        // marshal the saved registers into the canonical
        // `[u64; SYSCALL_MAX_ARGS]` layout and forward to the installed
        // dispatch callback (the architecture-neutral validation /
        // capability / audit dispatcher lives in `kernel/syscall`). The
        // result is written back into the saved `x0` slot so the
        // trampoline's `eret` returns it to EL0; the PE already advanced
        // `ELR_EL1` past the `svc`. A syscall that arrives before the
        // binary installed a dispatcher fails closed (`AGENTS.md`
        // §5.4.5) rather than returning an unspecified value to EL0.
        if kind == kind::LOWER_SYNC && crate::syscall_entry::is_svc(esr) {
            // SAFETY: `frame` is the live `[u64; SAVED_GPRS]` register
            // frame the trampoline built; reading it for the duration of
            // this call is sound.
            let saved = unsafe { &*frame.cast::<[u64; crate::syscall_entry::SAVED_GPRS]>() };
            let mut syscall_frame = crate::syscall_entry::syscall_frame_from_saved(saved);
            if !crate::syscall_entry::dispatch_svc(&mut syscall_frame) {
                crate::kernel_arch::halt_current_cpu();
            }
            // SAFETY: index 0 is the saved `x0` slot; writing the result
            // there makes the trampoline restore the new `x0` before
            // `eret`.
            unsafe {
                *frame = syscall_frame.args[0];
            }
            return;
        }

        // Any other synchronous exception (an abort, or a non-`svc`
        // lower-EL fault). Forward to the installed fault handler if
        // present (the memory-isolation vertical installs one);
        // otherwise fail closed by parking.
        if let Some(handler) = crate::fault::fault_handler() {
            handler(esr, read_far(), read_elr());
        }
        crate::kernel_arch::halt_current_cpu();
    }

    // FIQ / SError / AArch32 entries are not expected in this slice.
    // Park rather than `eret`-looping on an unhandled condition
    // (`AGENTS.md` §2 — never silently reset).
    crate::kernel_arch::halt_current_cpu();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq_kinds_are_classified() {
        assert!(is_irq(kind::CUR_SP0_IRQ));
        assert!(is_irq(kind::CUR_SPX_IRQ));
        assert!(is_irq(kind::LOWER_IRQ));
        assert!(!is_irq(kind::CUR_SPX_SYNC));
    }

    #[test]
    fn sync_kinds_are_classified() {
        assert!(is_sync(kind::CUR_SPX_SYNC));
        assert!(is_sync(kind::LOWER_SYNC));
        assert!(!is_sync(kind::CUR_SPX_IRQ));
    }

    #[test]
    fn kind_values_match_vector_table_indices() {
        // The `mov x0, #N` immediates in `vectors.s` use these indices.
        assert_eq!(kind::CUR_SP0_IRQ, 1);
        assert_eq!(kind::CUR_SPX_SYNC, 4);
        assert_eq!(kind::CUR_SPX_IRQ, 5);
        assert_eq!(kind::LOWER_SYNC, 8);
        assert_eq!(kind::LOWER_IRQ, 9);
    }

    extern "C" fn host_device_dispatch(_intid: u32) {}

    #[test]
    fn set_device_irq_dispatch_fails_closed_on_second_install() {
        clear_device_irq_dispatch_for_tests();
        set_device_irq_dispatch(host_device_dispatch).expect("first install");
        assert_eq!(
            set_device_irq_dispatch(host_device_dispatch),
            Err(SetDispatchError::AlreadyInstalled)
        );
        clear_device_irq_dispatch_for_tests();
    }

    #[test]
    fn device_irq_dispatch_addr_round_trips_installed_fn() {
        clear_device_irq_dispatch_for_tests();
        set_device_irq_dispatch(host_device_dispatch).expect("install");
        assert_eq!(
            device_irq_dispatch_addr(),
            host_device_dispatch as *const () as usize
        );
        clear_device_irq_dispatch_for_tests();
    }
}
