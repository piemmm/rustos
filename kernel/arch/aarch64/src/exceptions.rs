//! EL1 exception handling: vector-table install, IRQ dispatch, and the
//! synchronous-fault path.
//!
//! The assembly vector table (`vectors.s`, included via `global_asm!` in
//! `lib.rs`) tags every exception with a numeric *kind* and calls
//! `tairix_aarch64_trap_handler`. This module:
//!
//! * `init_vectors` points `VBAR_EL1` at the table.
//! * `enable_irq` unmasks IRQs at the PE (`DAIF.I`), the aarch64
//!   analogue of riscv64's `sstatus.SIE` enable — kept separate from
//!   arming the timer so the caller controls exactly when ticks begin.
//! * `tairix_aarch64_trap_handler` dispatches an IRQ to the GIC
//!   acknowledge → timer/SGI → end-of-interrupt handshake, routes an EL0
//!   `svc` (lower-EL synchronous exception) to the installed
//!   [`crate::syscall_entry`] dispatch callback, redirects a same-EL
//!   data abort taken inside the guarded user-copy fault window
//!   ([`crate::uaccess`]) to the copy's fix-up (the frame's ELR slot is
//!   rewritten, so the copy returns an error instead of the CPU
//!   halting), and routes any other synchronous exception to the
//!   installed [`crate::fault`] handler (or fails closed by parking the
//!   CPU).
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

/// Index of the saved `ELR_EL1` slot in the trampoline's register frame:
/// the word straight after the 31 saved GP registers (`vectors.s` stores
/// it at byte offset 248). The guarded user-copy fix-up rewrites this
/// slot, so the epilogue's `msr ELR_EL1` + `eret` resume at the fix-up.
pub const ELR_FRAME_INDEX: usize = crate::syscall_entry::SAVED_GPRS;

/// Index of the saved `SP_EL0` (the interrupted EL0 stack pointer) in the
/// trampoline's register frame: `vectors.s` stores it two words past
/// `ELR_EL1` (ELR at 248, `SPSR_EL1` at 256, `SP_EL0` at byte offset 264).
/// The user-fault crash path reads it as the faulting stack pointer.
pub const SP_EL0_FRAME_INDEX: usize = ELR_FRAME_INDEX + 2;

/// Index of the saved frame pointer (`x29`) in the register frame — a GP
/// register, so at its own number.
pub const FP_FRAME_INDEX: usize = 29;

/// Stable names of the 31 saved general-purpose registers (`x0`..`x30`),
/// in frame-index order, for the user-fault crash record's register
/// snapshot. `x30` is the link register; `x29` the frame pointer.
pub const GP_REG_NAMES: [&str; crate::syscall_entry::SAVED_GPRS] = [
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14",
    "x15", "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27",
    "x28", "x29", "x30",
];

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
    /// Current EL with SP0 — FIQ (unused SP0 group; classified for the
    /// debug watchdog's non-maskable self-sample).
    pub const CUR_SP0_FIQ: u64 = 2;
    /// Current EL with SPx — FIQ (the debug watchdog's Group-0 cadence
    /// while the kernel runs on `SP_EL1`).
    pub const CUR_SPX_FIQ: u64 = 6;
    /// Lower EL (AArch64) — FIQ (the Group-0 cadence taken while EL0 runs).
    pub const LOWER_FIQ: u64 = 10;
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

/// `true` iff `kind` denotes an FIQ entry (from any EL). The debug
/// watchdog's non-maskable Group-0 cadence self-sample is the only FIQ
/// source this port routes (`plans/WATCHDOG.md`); on a shippable image no
/// FIQ is ever routed, so an FIQ entry there is a spurious park.
#[must_use]
pub const fn is_fiq(kind: u64) -> bool {
    matches!(
        kind,
        kind::CUR_SP0_FIQ | kind::CUR_SPX_FIQ | kind::LOWER_FIQ
    )
}

// --- Device-IRQ dispatch hook -------------------------------------
//
// The timer PPI ([`crate::preempt::TIMER_PPI`]) has its own dedicated
// path; every *other* acknowledged INTID (a device's shared-peripheral
// interrupt routed through the GIC by [`crate::gic::route_spi`]) is
// forwarded to a set-once dispatch callback the binary installs. This
// mirrors riscv64's `trap::set_trap_dispatch` external-interrupt seam:
// the callback claims/services the source and forwards it to
// `tairix_kernel_irq::IrqTable::fire` (which masks the GIC line before
// the waiter observes the wake — `docs/src/security/irq.md`), while the
// GIC end-of-interrupt handshake stays in [`handle_irq`]. The slot is
// set-once, backed by an atomic so the IRQ path reads it without a lock
// (no global mutable state; this is an immutable,
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
    /// A dispatcher was already published; the slot is set-once per boot.
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
    // Production code never clears the slot.
    DEVICE_IRQ_DISPATCH_FN.store(0, Ordering::Release);
}

/// Invoke the installed device-IRQ dispatcher with `intid`, if any.
///
/// A device interrupt that arrives before the binary installed a
/// dispatcher is left unserviced here (the GIC line stays active until
/// [`handle_irq`]'s end-of-interrupt); the boot path installs the
/// dispatcher before routing any device SPI, so this is not reached in
/// practice (fail closed rather than guess).
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
    fn tairix_aarch64_vectors();
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
    // The vector entry/exit path (`vectors.s`) unconditionally saves and
    // restores the full FP/SIMD register file (q0..q31, FPCR, FPSR), so
    // FP/SIMD must not trap at EL1 before the first exception is taken:
    // otherwise the handler's first `stp q0, q1` would itself trap back
    // into this same synchronous vector and recurse forever, hanging the
    // core. Enabling it here — the one chokepoint that arms the FP-using
    // table — makes installing the vectors and enabling the FP their
    // handler needs one indivisible step, so no consumer can arm the
    // table without it. Idempotent, so a caller that already enabled FP
    // for its own early NEON code loses nothing.
    // SAFETY: `enable_fp_el1` only clears this CPU's `CPACR_EL1` FP/SIMD
    // trap; it confers no cross-privilege authority and this routine runs
    // before the CPU takes any exception.
    unsafe {
        crate::kernel_arch::enable_fp_el1();
    }
    // Arm the fault-windowed user copy alongside the vector table: the
    // two are one mechanism (the handler below redirects an in-window
    // same-EL data abort to the copy's fix-up), so no consumer can
    // install the vectors without the recovery. The install is
    // idempotent for this routine; a conflicting occupant is a
    // boot-order defect the CPU must not run past (fail closed).
    if crate::uaccess::install().is_err() {
        crate::kernel_arch::halt_current_cpu();
    }
    let base = tairix_aarch64_vectors as *const () as u64;
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
/// the `wfi` — no edge is lost (no unbounded sleep
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

/// `DAIF` asynchronous-exception mask immediates and the debug
/// watchdog's `DAIF.F` (FIQ) execution discipline.
///
/// `DAIFSet`/`DAIFClr` take a 4-bit immediate selecting the
/// Debug/SError/IRQ/FIQ mask bits `D A I F` (bits 3..0). The port masks
/// **IRQ** (`I`, bit 1) around an interrupt-safe critical section. The
/// debug watchdog's non-maskable FIQ self-sample (`plans/WATCHDOG.md`,
/// staged) additionally needs **FIQ** (`F`, bit 0) left *unmasked*
/// wherever a wedge can occur, so a Group-0/FIQ cadence can fire inside a
/// `DAIF.I`-masked section that the maskable IRQ cadence cannot observe.
///
/// The immediate an interrupt-safe critical section masks with therefore
/// depends on the build, and [`daif::critical_section_mask`] is the single
/// definition of it that the lock primitive consumes, so the value can
/// never be transcribed inconsistently.
pub mod daif {
    /// `DAIF` FIQ-mask immediate bit (`F`).
    pub const F: u64 = 1 << 0;
    /// `DAIF` IRQ-mask immediate bit (`I`).
    pub const I: u64 = 1 << 1;

    /// The `DAIFSet` immediate an interrupt-safe critical section masks
    /// with.
    ///
    /// When the debug watchdog's FIQ self-sample is compiled in
    /// (`diagnostics = true`) it masks **IRQ only**, leaving `DAIF.F`
    /// clear so a Group-0/FIQ cadence can still fire inside the section
    /// and observe a core wedged there; otherwise it masks **IRQ + FIQ**,
    /// the classic shippable discipline.
    #[must_use]
    pub const fn critical_section_mask(diagnostics: bool) -> u64 {
        if diagnostics {
            I
        } else {
            I | F
        }
    }
}

/// Unmask **FIQ** taking at the PE (`DAIF.F`) so the debug watchdog's
/// non-maskable Group-0/FIQ self-sample can fire on this CPU.
///
/// The sibling of [`enable_irq`] the staged FIQ masked-section sampler
/// (`plans/WATCHDOG.md`) requires. AArch64 exception entry masks `DAIF.F`
/// in hardware and [`enable_irq`] clears only `DAIF.I`, so without this
/// the `svc`/fault handler paths — and, once cleared per-CPU at boot,
/// thread-mode kernel code — would keep FIQ masked and a Group-0 cadence
/// could never reach a core wedged in a `DAIF.I`-masked section.
///
/// Compiled only into the debug (`watchdog-diagnostics`) image, and inert
/// until a Group-0/FIQ source is actually routed (staged separately): with
/// no Group-0 interrupt enabled, clearing `DAIF.F` raises nothing, so on
/// its own this changes no observable behaviour. It is deliberately never
/// called from the FIQ handler's own entry or from
/// [`crate::kernel_arch::halt_current_cpu`] (`msr DAIFSet, #0xf`), where a
/// nested FIQ would be unsafe.
///
/// # Safety
///
/// Clearing `DAIF.F` only changes the FIQ mask; it has no other side
/// effect. The caller must have installed the vector table
/// ([`init_vectors`]) so a taken FIQ dispatches through a real slot.
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
pub unsafe fn enable_fiq_delivery() {
    // SAFETY: clearing `DAIF.F` (immediate bit 0) unmasks FIQ taking; it
    // has no other side effect.
    unsafe {
        core::arch::asm!("msr DAIFClr, #{f}", f = const daif::F, options(nomem, nostack));
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

/// Build the faulting-thread [`UserRegisterFrame`] from the saved EL0
/// trampoline `frame`.
///
/// The saved frame carries `x0`..`x30` (indices `0..=30`), the faulting
/// `ELR_EL1` (pc) at [`ELR_FRAME_INDEX`], and `SP_EL0` (the interrupted
/// user stack pointer) at [`SP_EL0_FRAME_INDEX`]; the frame pointer is
/// `x29` at [`FP_FRAME_INDEX`]. AArch64 always saves the fp on trap entry,
/// so the frame is marked `fp_valid` and the crash path can follow the
/// AAPCS64 frame-pointer chain using [`crate::backtrace::Backtracer::LAYOUT`].
///
/// # Safety
///
/// `frame` must be the live `[u64; …]` register frame `vectors.s` built for
/// a lower-EL entry, which saves the full GP set plus `ELR/SPSR/SP_EL0`;
/// every index read here lies within it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe fn user_register_frame(frame: *const u64) -> tairix_arch_api::backtrace::UserRegisterFrame {
    use tairix_arch_api::backtrace::{RegisterSnapshot, UserRegisterFrame};
    // SAFETY: the caller guarantees `frame` addresses the live saved frame.
    let read = |i: usize| unsafe { *frame.add(i) };
    let pc = read(ELR_FRAME_INDEX);
    let sp = read(SP_EL0_FRAME_INDEX);
    let fp = read(FP_FRAME_INDEX);
    let mut snapshot = RegisterSnapshot::new(pc, sp, fp);
    for (index, name) in GP_REG_NAMES.iter().enumerate() {
        snapshot = snapshot.with(name, read(index));
    }
    UserRegisterFrame::new(snapshot, crate::backtrace::Backtracer::LAYOUT, true)
}

/// Handle a **Group-0 FIQ** — the debug watchdog's non-maskable
/// masked-section self-sample (`plans/WATCHDOG.md`), the only FIQ this port
/// ever routes.
///
/// FIQ is masked by `DAIF.F`, a bit *separate* from the `DAIF.I` that an
/// `IrqSafeSpinLock` / syscall body / fault resolver masks, so this fires
/// inside exactly the IRQ-masked wedge the maskable cadence cannot observe.
/// It acknowledges the Group-0 interrupt through the same `GICC_IAR`/
/// `GICC_EOIR` handshake as the IRQ path (carrying the full IAR cookie end
/// to end), records that an FIQ was actually taken (the deliverability
/// probe reads this, `crate::watchdog::note_fiq_taken`), and for the
/// cadence PPI runs the self-sample `on_watchdog_interrupt` — which
/// re-arms the one-shot and, on a kernel-context sample, unwinds the *live*
/// wedged PC. It is purely observational: it **never** clears `DAIF.F`
/// (a nested FIQ inside the FIQ handler is unsafe) and **never** preempts
/// (it may have interrupted an IRQ-masked critical section the kernel must
/// not abandon).
#[cfg(all(
    target_arch = "aarch64",
    target_os = "none",
    feature = "watchdog-diagnostics"
))]
fn handle_fiq(frame: *const u64) {
    let iar = crate::gic::acknowledge();
    let intid = iar & crate::gic::IAR_INTID_MASK;
    if intid == crate::gic::SPURIOUS_INTID {
        // Nothing pending (an FIQ that raced its own deactivation): the
        // GIC requires no EOI for a spurious read.
        return;
    }
    let cpu = crate::smp::current_cpu_index();
    // Record the delivery for the boot probe before any other work, so a
    // probe FIQ that fires on a bare cadence PPI is still observed.
    crate::watchdog::note_fiq_taken();
    if intid == crate::watchdog::WATCHDOG_PPI {
        crate::watchdog::on_watchdog_interrupt(cpu, frame);
    }
    // Complete the interrupt with the full IAR cookie so the CPU interface
    // does not wedge with an active Group-0 priority.
    crate::gic::end_of_interrupt(iar);
}

/// Handle an IRQ: acknowledge the GIC, dispatch the timer PPI to the
/// scheduler-tick path (or an IPI / device interrupt to its handler),
/// complete the interrupt, then — for **any** interrupt taken from EL0 —
/// drive the preemption point.
///
/// `from_el0` is `true` when the interrupted context was EL0 user mode
/// (the `LOWER_IRQ` vector). It gates **preemption only**: an interrupt
/// taken in EL1 (`CUR_SPX_IRQ`) still runs its handler (scheduler-tick
/// accounting, the timed-wake sweep, a device wake) but never switches the
/// current task away — the kernel is non-preemptible, so a half-completed
/// kernel critical section (a held `lib/sync` lock, an in-flight syscall)
/// is never abandoned mid-flight (SMP watch-out / no hacks); its pending
/// reschedule is instead latched and honoured at the interrupted syscall's
/// completion. Only a tick taken from EL0 preempts immediately, and only
/// when the need-resched latch the preempt callback consults is set.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn handle_irq(from_el0: bool, frame: *const u64) {
    // Keep the *full* IAR value: for a software-generated interrupt it
    // carries the source CPU in bits [12:10], and the matching EOIR write
    // must carry those bits back or the GICv2 never deactivates the SGI
    // (the CPU-interface running priority stays raised and every further
    // interrupt on this CPU — timer, watchdog, device — is blocked, hanging
    // the core under IPI-heavy load). Dispatch decisions use only the INTID
    // field; the end-of-interrupt handshake writes the whole value.
    let iar = crate::gic::acknowledge();
    let intid = iar & crate::gic::IAR_INTID_MASK;
    if intid == crate::gic::SPURIOUS_INTID {
        // Spurious read: nothing pending, and the GIC requires no EOI.
        return;
    }
    // The running CPU's dense id, recovered from `MPIDR_EL1`, drives both
    // the per-CPU timer slot and the IPI callback (one
    // identity source).
    let cpu = crate::smp::current_cpu_index();
    if intid == crate::preempt::TIMER_PPI {
        crate::preempt::on_timer_interrupt(cpu);
    } else if intid == crate::watchdog::WATCHDOG_PPI {
        // The virtual-timer lockup-watchdog cadence sample: re-arm the next
        // one-shot and run the installed detector callback (which stamps
        // this CPU's liveness heartbeat and scans the other CPUs). Taken
        // from EL0 or EL1 alike — it never preempts, so it is not gated on
        // `from_el0`. The saved `frame` is forwarded so the sample can
        // unwind the interrupted context for the pre-silence backtrace.
        crate::watchdog::on_watchdog_interrupt(cpu, frame);
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
    // so the CPU interface does not wedge with an active priority. The
    // **full** IAR value is written back (source-CPU field included) so an
    // SGI from any CPU is actually deactivated.
    crate::gic::end_of_interrupt(iar);

    // Check for a pending reschedule on the way back to user mode, for
    // **any** interrupt — a timer quantum expiry, a cross-CPU reschedule
    // IPI, or a device IRQ that woke a higher-priority task. This is the
    // aarch64 analogue of an OS honouring `need_resched` on
    // interrupt-return-to-user: without it, a CPU-bound EL0 task that
    // never issues a syscall (and, being the sole runnable task, has no
    // tickless quantum armed) could never be forced back into the
    // scheduler when a device interrupt made new work runnable, so
    // input/ctrl-C would never run while it spun.
    //
    // Preempting runs **after** the end-of-interrupt handshake: the
    // installed callback may context-switch away to another task and not
    // return to this frame for a long time, so the interrupt line must
    // already be deactivated (otherwise the GIC would hold an active
    // priority across the switch and block every further interrupt on
    // this CPU). The callback consults the per-CPU need-resched latch and
    // only suspends the running user task back to the scheduler (the
    // involuntary analogue of a `yield` syscall) when a reschedule is
    // actually owed, so an interrupt that woke nothing returns straight to
    // EL0 with no gratuitous context switch. Only a tick taken from EL0
    // reaches here: the kernel is non-preemptible, so an interrupt taken
    // in EL1 latches its reschedule (honoured at the interrupted syscall's
    // completion) rather than switching away mid-critical-section. A build
    // that installed no callback keeps cooperative scheduling (fail-safe).
    if from_el0 {
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
/// Only callable from `tairix_aarch64_trap_common`, which has saved the
/// interrupted GP registers (so `frame` is a valid `[u64; SAVED_GPRS]`
/// for the duration of this call) and tagged the exception kind. An IRQ
/// or a serviced `svc` returns (the trampoline `eret`s); a fault diverges
/// (the installed handler or the park never returns).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn tairix_aarch64_trap_handler(kind: u64, frame: *mut u64) {
    if is_irq(kind) {
        // `LOWER_IRQ` is the only IRQ entry whose interrupted context was
        // EL0 user mode; `CUR_SP0_IRQ`/`CUR_SPX_IRQ` interrupted EL1
        // kernel code, which is never preempted (see [`handle_irq`]).
        handle_irq(kind == kind::LOWER_IRQ, frame);
        return;
    }

    // The debug watchdog's non-maskable Group-0 cadence self-sample: only
    // ever routed in a `watchdog-diagnostics` build, and observational (it
    // never preempts). A shippable image routes no FIQ, so an FIQ entry
    // there falls through to the park below (never silently `eret`-looped).
    #[cfg(feature = "watchdog-diagnostics")]
    if is_fiq(kind) {
        handle_fiq(frame);
        return;
    }

    if is_sync(kind) {
        let esr = read_esr();

        // Debug watchdog FIQ self-sample discipline: run the syscall body
        // and the user-fault resolver with `DAIF.F` clear so a Group-0/FIQ
        // cadence can observe a core wedged in this (IRQ-masked) section
        // (`plans/WATCHDOG.md`). Exception entry masked `DAIF.F` in
        // hardware; re-clear it for the handler. Inert until a Group-0
        // source is routed, and compiled out of shippable images.
        #[cfg(feature = "watchdog-diagnostics")]
        // SAFETY: the vector table is installed before any exception can be
        // taken, so a taken FIQ dispatches through a real slot; clearing
        // `DAIF.F` only changes the FIQ mask.
        unsafe {
            enable_fiq_delivery();
        }

        // An `svc` from a lower EL (AArch64) is the EL0 syscall path:
        // marshal the saved registers into the canonical
        // `[u64; SYSCALL_MAX_ARGS]` layout and forward to the installed
        // dispatch callback (the architecture-neutral validation /
        // capability / audit dispatcher lives in `kernel/syscall`). The
        // result is written back into the saved `x0` slot so the
        // trampoline's `eret` returns it to EL0; the PE already advanced
        // `ELR_EL1` past the `svc`. A syscall that arrives before the
        // binary installed a dispatcher fails closed rather than returning an unspecified value to EL0.
        if kind == kind::LOWER_SYNC && crate::syscall_entry::is_svc(esr) {
            // SAFETY: `frame` is the live `[u64; SAVED_GPRS]` register
            // frame the trampoline built; reading it for the duration of
            // this call is sound.
            let saved = unsafe { &*frame.cast::<[u64; crate::syscall_entry::SAVED_GPRS]>() };
            let mut syscall_frame = crate::syscall_entry::syscall_frame_from_saved(saved);
            // Run the syscall body with device IRQs deliverable. The PE
            // masked `DAIF.I` on exception entry; the trampoline has now
            // saved the full frame (GPRs + ELR/SPSR/SP_EL0 + FP) and we run
            // on `SP_EL1`, so a device IRQ or the preemption tick taken here
            // is a nested EL1 exception that saves its own frame, services
            // its source, and `eret`s back to this handler — the outer svc's
            // return state is frame-resident and restored before the final
            // `eret`. This is what stops a long, non-blocking syscall body
            // from monopolising the CPU with interrupts masked. The kernel
            // stays non-preemptible: an IRQ taken in EL1 latches its
            // reschedule (honoured at return-to-user in `completion_outcome`)
            // rather than switching away mid-critical-section, enforced by
            // the `from_el0` gate in `handle_irq`. Re-mask before returning
            // so the trampoline restores the user frame with IRQ-taking off.
            // SAFETY: the vector table, GIC, and IRQ callbacks are installed
            // before any EL0 code can `svc`, so a taken IRQ dispatches
            // correctly; `enable_irq`/`mask_irq` only toggle `DAIF.I`.
            unsafe {
                enable_irq();
            }
            let dispatched = crate::syscall_entry::dispatch_svc(&mut syscall_frame);
            // SAFETY: as above — restoring the IRQ mask before the epilogue
            // restores the interrupted user frame.
            unsafe {
                mask_irq();
            }
            if !dispatched {
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

        // Software-managed Access Flag (the cold-page referenced bit for
        // `plans/SWAPSWAPSWAP.md`): cortex-a57/a72 lack HAFDBS, so an
        // access to a valid leaf whose AF the scanner cleared
        // (`crate::paging::AddressSpace::test_and_clear_accessed`) raises
        // an Access-Flag fault rather than updating AF in the walk. Set AF
        // back on the faulting leaf and return so the trampoline `eret`
        // retries the access (`ELR_EL1` still points at the faulting
        // instruction — the PE does not advance it for an abort). Both
        // data and instruction aborts, from either EL, are handled. When
        // the leaf is not a cleared-AF leaf (`set_accessed_flag_in_active`
        // returns `false`), the fault was something else and falls through
        // to the resolver / fatal path unchanged (fail closed).
        if crate::fault::is_abort(esr) && crate::fault::is_access_flag_fault(esr) {
            if crate::paging::set_accessed_flag_in_active(read_far()) {
                return;
            }
        }

        // Every data abort from EL0 is offered to the installed resolver
        // before the fatal path, with the `ESR.WnR` verdict. A *read* may
        // be a demand-paged file-mapping fault: a `true` return means the
        // faulting page is now resident, so returning here lets the
        // trampoline `eret` re-run the faulting instruction (`ELR_EL1`
        // still points at it — the PE does not advance it for an abort).
        // A *write* is never resolved (file mappings are read-only;
        // resolving a store would retry it forever) — the resolver kills
        // the faulting task instead, so a store to a read-only mapping or
        // any wild write costs the task, never the CPU. A fault that is
        // fatal to the task alone never returns from the resolver (the
        // callback suspends the task into the scheduler with an exit
        // action); `false` falls through to the fatal path below, exactly
        // as with no resolver installed (fail closed).
        if kind == kind::LOWER_SYNC && crate::fault::is_lower_el_data_abort(esr) {
            if let Some(resolver) = crate::fault::user_fault_resolver() {
                // Capture the faulting EL0 register frame from the saved
                // trampoline frame so the resolver can record a post-mortem
                // crash record with a backtrace. The frame lives on this
                // kernel stack for the duration of the resolver call.
                // SAFETY: `frame` is the live `[u64; …]` register frame the
                // trampoline built for this lower-EL entry, which saved the
                // full GP set plus ELR/SPSR/SP_EL0 (`vectors.s`); every index
                // read is within it.
                let user_frame = unsafe { user_register_frame(frame) };
                if resolver(
                    read_far(),
                    crate::fault::is_write_data_abort(esr),
                    &user_frame,
                ) {
                    return;
                }
            }
        }

        // A data abort taken from EL1 itself whose saved `ELR_EL1` lies
        // inside the guarded user-copy window: the validated copy's
        // software proof was violated underneath it. Rewrite the frame's
        // ELR slot to the copy's fix-up so the trampoline's `eret`
        // resumes there and the copy returns an error to its caller
        // instead of taking the CPU down. Every other same-EL abort
        // stays on the fatal path below.
        if kind == kind::CUR_SPX_SYNC && crate::fault::is_current_el_data_abort(esr) {
            if let Some(fixup) = crate::uaccess::kernel_fixup_for(read_elr()) {
                // SAFETY: `frame` is the live trampoline register frame;
                // `ELR_FRAME_INDEX` addresses its saved-`ELR_EL1` word
                // (`vectors.s` byte offset 248), which the epilogue
                // restores before `eret`. The fix-up address is a real
                // instruction in this image.
                unsafe {
                    *frame.add(ELR_FRAME_INDEX) = fixup;
                }
                return;
            }
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
    // (never silently reset).
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
    fn fiq_kinds_are_classified_and_disjoint_from_irq_and_sync() {
        // The three FIQ vector kinds (SP0/SPx/lower-EL) match; the vector
        // immediates must stay disjoint from the IRQ and sync kinds so the
        // Group-0 self-sample never shadows a real IRQ or fault.
        assert!(is_fiq(kind::CUR_SP0_FIQ));
        assert!(is_fiq(kind::CUR_SPX_FIQ));
        assert!(is_fiq(kind::LOWER_FIQ));
        assert!(!is_fiq(kind::CUR_SPX_IRQ));
        assert!(!is_fiq(kind::LOWER_IRQ));
        assert!(!is_fiq(kind::LOWER_SYNC));
        for k in [kind::CUR_SP0_FIQ, kind::CUR_SPX_FIQ, kind::LOWER_FIQ] {
            assert!(!is_irq(k));
            assert!(!is_sync(k));
        }
    }

    #[test]
    fn daif_immediate_bits_match_the_arm_encoding() {
        // `DAIFSet`/`DAIFClr` immediate: bit 0 = FIQ (F), bit 1 = IRQ (I).
        assert_eq!(daif::F, 0b01);
        assert_eq!(daif::I, 0b10);
    }

    #[test]
    fn critical_section_masks_irq_only_in_the_diagnostics_build() {
        // The debug watchdog build masks IRQ only, leaving FIQ deliverable
        // for the non-maskable self-sample; a shippable build masks
        // IRQ+FIQ, the classic discipline.
        assert_eq!(daif::critical_section_mask(true), daif::I);
        assert_eq!(daif::critical_section_mask(false), daif::I | daif::F);
    }

    #[test]
    fn elr_frame_index_matches_the_trampoline_layout() {
        // `vectors.s` stores ELR_EL1 at byte offset 248, straight after
        // x0..x30; a desync here would make the fix-up rewrite corrupt a
        // GP register instead of the return address.
        assert_eq!(ELR_FRAME_INDEX * 8, 248);
    }

    #[test]
    fn user_fault_frame_indices_match_the_trampoline_layout() {
        // `vectors.s` saves SPSR_EL1 at byte 256 and SP_EL0 at byte 264,
        // straight after ELR_EL1 (248); x29 (the frame pointer) is a GP
        // register at its own index. A desync here would make the crash
        // record read a wrong sp/fp.
        assert_eq!(SP_EL0_FRAME_INDEX * 8, 264);
        assert_eq!(FP_FRAME_INDEX, 29);
        // One name per saved GP register, in frame-index order.
        assert_eq!(GP_REG_NAMES.len(), crate::syscall_entry::SAVED_GPRS);
        assert_eq!(GP_REG_NAMES[0], "x0");
        assert_eq!(GP_REG_NAMES[29], "x29");
        assert_eq!(GP_REG_NAMES[30], "x30");
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
