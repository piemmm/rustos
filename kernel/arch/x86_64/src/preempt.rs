//! LAPIC-timer-driven preemption (Stage 3a (c5)).
//!
//! This module owns the per-CPU preemption surface on x86_64:
//!
//! * The IDT vector the timer LVT fires on ([`TIMER_VECTOR`]).
//! * The ISR stub emitted by [`crate::define_isr`] that captures the
//!   GPRs and trampolines into a Rust dispatcher.
//! * The Rust dispatcher itself (`rustos_arch_x86_64_timer_dispatch`)
//!   which forwards into the user-installed callback and then issues
//!   the LAPIC end-of-interrupt write.
//! * A per-CPU init helper (`init_local_preempt`) that installs the
//!   timer ISR into the per-CPU IDT, programs the LAPIC timer in
//!   periodic mode from the BSP-supplied `Calibration`, and returns.
//!
//! The dispatcher, the ISR stub emitted by [`crate::define_isr`], and
//! `init_local_preempt` are gated to `target_os = "none"` because
//! they reach for LAPIC MMIO and naked-asm; rustdoc on the host
//! target therefore does not see them. The bare-metal documentation
//! lives next to those items in the source.
//!
//! The kernel-side preemption logic lives in
//! `kernel/sched::Scheduler::on_timer_tick`; this module merely wires
//! the x86_64 hardware into the architecture-neutral surface. The
//! split keeps `AGENTS.md` §2.4 (no interface creep) honest — the
//! arch port owns timer state, the scheduler owns the run-queue
//! mutation that follows from a tick.
//!
//! # LAPIC-timer-calibration policy
//!
//! Calibration on x86 derives from busy-waiting against the i8254 PIT
//! (see [`crate::apic_timer::calibrate`]), which is a single global
//! device. Doing it concurrently on every CPU would corrupt PIT
//! channel 2. The QEMU integration test
//! (`tests/integration/scheduler_stress_qemu`) therefore calibrates
//! exactly once on the BSP and reuses the resulting `Calibration`
//! on every AP. This is correct on QEMU (one bus clock) and on
//! homogeneous single-package SMP Intel systems where the LAPIC
//! timer's source frequency is shared across logical CPUs. Multi-
//! socket asymmetric hardware would need per-package re-calibration;
//! the scheduler-side `Scheduler::preemption_count` assertion in the
//! integration test trips loudly if any CPU's LAPIC fails to
//! advance, so a future port that violates the assumption fails
//! closed.

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::sync::atomic::{AtomicUsize, Ordering};

// Bare-metal-only imports — host builds carry neither
// `init_local_preempt` nor the timer dispatcher (the static callback
// storage and ISR stub are gated to the freestanding target).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::apic::{Lapic, LapicMmio};
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::apic_timer::{self, Calibration};
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::interrupts::{InterruptStackFrame, SavedRegs};

/// IDT vector the LAPIC timer fires on.
///
/// `0x20` is the first user-defined vector — vectors `0x00..=0x1F`
/// are reserved for architectural exceptions (Intel SDM Vol 3A
/// §6.3.1). The constant is `pub` so the integration test can
/// cross-check the IDT slot it observes.
pub const TIMER_VECTOR: u8 = 0x20;

// --- Callback storage ----------------------------------------------

/// The Rust callback the timer ISR forwards each tick to.
///
/// Stored as an `Option<extern "C" fn(u32)>` packed into a `usize`
/// (the architectural size of a function pointer) so the ISR can
/// swap it in/out with `Relaxed` atomics — the callback table is set
/// up *before* any timer fires and never mutated again in normal
/// operation.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static TIMER_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// The preemption callback the timer ISR forwards each tick **taken from
/// ring 3** to, packed into a `usize`. Installed by the binary before the
/// timer is armed; absent (`0`) the tick is pure accounting and nothing is
/// preempted, so an image that arms the timer without wiring preemption
/// simply keeps cooperative scheduling (fail-safe, `AGENTS.md` §2.9).
///
/// This is the x86_64 sibling of the aarch64/riscv64 `PREEMPT_CALLBACK_FN`
/// (`AGENTS.md` §2.21 — the same shape over the Arch HAL): the involuntary
/// analogue of the cooperative reschedule the `syscall` path drives. A
/// timer interrupt taken while ring 3 was running is delivered through the
/// IDT interrupt gate onto the interrupted task's own kernel stack (the
/// `TSS.RSP0` the resume hook repoints per task), so the installed callback
/// can suspend that task back to the scheduler exactly as
/// `reschedule_current` does for a `yield` syscall.
///
/// The callback runs **after** the LAPIC EOI (so the in-service bit is
/// released before the context switch strands it) and **only** for a tick
/// taken from ring 3 — a tick taken in ring 0 never preempts (the kernel is
/// non-preemptible, `AGENTS.md` §4 watch-out: a half-completed kernel
/// critical section must never be switched away from). In production the
/// kernel runs with `RFLAGS.IF == 0`, so a maskable timer IRQ is *taken*
/// only while ring 3 runs (which `crate::userentry` enters with `IF` set);
/// the explicit ring gate is defence-in-depth so a future in-kernel `sti`
/// can never accidentally preempt the kernel (`AGENTS.md` §2.9).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static PREEMPT_CALLBACK_FN: AtomicUsize = AtomicUsize::new(0);

/// LAPIC EOI register MMIO offset (Intel SDM Vol 3A §11.4.1 Table 11-1).
/// Re-declared here so the dispatcher can write through a bare-metal
/// raw-pointer write without going through the `Lapic<M>` driver
/// (the driver needs `&mut`, which the ISR cannot hold).
pub const LAPIC_EOI_OFFSET: usize = 0xB0;

/// LAPIC base MMIO address (the architecturally-fixed value Intel CPUs
/// expose after reset; OVMF and QEMU agree on the same default).
///
/// Identity-mapped by the boot trampoline (`boot.s` SAFETY-INVARIANT 4
/// — 0..4 GiB identity map). Re-declared here rather than imported
/// from `apic.rs` to avoid a dependency cycle in the ISR-fast path.
pub const LAPIC_BASE_PHYS: u64 = 0xFEE0_0000;

/// Install the per-CPU timer callback.
///
/// The callback is invoked from the timer ISR on every tick with the
/// calling CPU's `rustos_arch_api::CpuId` (the LAPIC ID as
/// determined by the `id` register read at install time of the BSP's
/// scheduler, mapped to a dense `CpuId` by the binary; the callback
/// receives the `CpuId` directly because the ISR cannot afford to
/// re-derive it on every tick).
///
/// Called exactly once during BSP boot; subsequent calls overwrite
/// the slot atomically and are documented as "test-helper only" — the
/// production binary installs its scheduler-tick callback before any
/// AP comes up.
///
/// Storing a `fn` (not a closure) keeps the callback safe to invoke
/// from interrupt context: there is no captured environment that
/// could be `Drop`-ped while the ISR is mid-flight.
pub fn set_timer_callback(cb: extern "C" fn(u32)) {
    // `fn` pointers are `usize`-sized, so `as usize` is lossless.
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    TIMER_CALLBACK_FN.store(cb as usize, Ordering::Relaxed);
    // On host builds the static is omitted — callers gated this themselves.
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    let _ = cb;
}

/// Read the currently-installed timer callback, if any. Test-only.
///
/// Host builds always return `None` because the callback storage is
/// gated to the freestanding target.
#[must_use]
pub fn timer_callback() -> Option<extern "C" fn(u32)> {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        let raw = TIMER_CALLBACK_FN.load(Ordering::Relaxed);
        if raw == 0 {
            None
        } else {
            // SAFETY: every store into `TIMER_CALLBACK_FN` originates
            // from `set_timer_callback`, which always rounds-trips a
            // valid `extern "C" fn(u32)` pointer.
            Some(unsafe { core::mem::transmute::<usize, extern "C" fn(u32)>(raw) })
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        None
    }
}

/// Install the per-CPU ring-3-preemption callback the timer ISR forwards
/// each tick taken from ring 3 to (the private `PREEMPT_CALLBACK_FN`
/// slot).
///
/// The binary installs the callback (which suspends the running user task
/// back to the scheduler via `reschedule_current`) before arming the
/// timer. Storing a `fn` (not a closure) keeps it safe to invoke from
/// interrupt context: there is no captured environment that could be
/// `Drop`-ped while the ISR is mid-flight. Mirrors
/// [`set_timer_callback`]'s host-inert gating.
pub fn set_preempt_callback(cb: extern "C" fn(u32)) {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    PREEMPT_CALLBACK_FN.store(cb as usize, Ordering::Release);
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    let _ = cb;
}

/// Read the currently-installed ring-3-preemption callback, if any.
/// Test-only; always `None` on the host (the storage is gated to the
/// freestanding target, like [`timer_callback`]).
#[must_use]
pub fn preempt_callback() -> Option<extern "C" fn(u32)> {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        let raw = PREEMPT_CALLBACK_FN.load(Ordering::Acquire);
        if raw == 0 {
            None
        } else {
            // SAFETY: every store into `PREEMPT_CALLBACK_FN` originates
            // from `set_preempt_callback`, which always round-trips a
            // valid `extern "C" fn(u32)` pointer.
            Some(unsafe { core::mem::transmute::<usize, extern "C" fn(u32)>(raw) })
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        None
    }
}

/// `true` iff the code-segment selector `cs` of an interrupted context
/// has requestor privilege level (RPL, the low two bits) 3 — i.e. the
/// interrupt was taken from ring 3 (user mode).
///
/// The CPU pushes the full ring-3 `CS` (RPL 3) on a privilege-raising
/// interrupt and the kernel `CS` (RPL 0) on a ring-0 interrupt, so the RPL
/// is the authoritative origin (Intel SDM Vol 3A §6.12.1). Pure and
/// host-testable; the freestanding dispatcher reads `cs` from the saved
/// [`crate::interrupts::InterruptStackFrame`] and consults this.
#[must_use]
pub const fn cs_is_ring3(cs: u64) -> bool {
    (cs & 0b11) == 3
}

// --- Per-CPU ID hook ------------------------------------------------

/// Per-CPU mapping from LAPIC ID to dense `rustos_arch_api::CpuId`.
///
/// The scheduler addresses CPUs with a dense `0..config.cpus` range;
/// the LAPIC ID on QEMU is sparse (`0`, `1`, `2`, …) but on real
/// hardware can be any 8-bit value. The binary populates this table
/// at AP bring-up time via [`set_cpu_id_for_lapic`]; the ISR consults
/// it with one MMIO read of the LAPIC ID register plus one indexed
/// load.
///
/// `u32::MAX` is the sentinel for "no mapping installed yet"; the ISR
/// silently EOI's and returns in that case so a stray timer that
/// fires before the mapping table is populated is *not* a panic.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static LAPIC_TO_CPU_ID: [core::sync::atomic::AtomicU32; 256] = {
    // SAFETY-INVARIANT: the `const` here is used **only** as the
    // initializer for a static array of atomics — the
    // `declare_interior_mutable_const` lint flags this idiom even
    // though there is no way to observe the interior mutability
    // through the const itself (it is consumed at array-literal
    // expansion time and never named again). This is the canonical
    // pattern for building a static `[Atomic_; N]` in `no_std` Rust;
    // see Rust RFC 1440 and the `core` source for `AtomicUsize`'s
    // own array constructors. Allow with rationale per AGENTS.md
    // §15.10.
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);
    [ZERO; 256]
};

/// Record the dense `rustos_arch_api::CpuId` this `lapic_id` maps to.
///
/// Called from each CPU's bring-up path *before* it enables interrupts.
/// `u32::MAX` is reserved as the "unmapped" sentinel; passing it is
/// equivalent to clearing the slot.
pub fn set_cpu_id_for_lapic(lapic_id: u8, cpu_id: u32) {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    LAPIC_TO_CPU_ID[lapic_id as usize].store(cpu_id, Ordering::Relaxed);
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        let _ = (lapic_id, cpu_id);
    }
}

/// Test-only accessor for the LAPIC→CpuId mapping table.
#[must_use]
pub fn cpu_id_for_lapic(lapic_id: u8) -> u32 {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        LAPIC_TO_CPU_ID[lapic_id as usize].load(Ordering::Relaxed)
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        let _ = lapic_id;
        u32::MAX
    }
}

// --- Timer dispatcher (called from the ISR stub) -------------------

/// Rust trampoline called by the timer ISR stub emitted via
/// `define_isr!`.
///
/// `regs` is the [`SavedRegs`] block the stub pushed; the dispatcher reads
/// the CPU-pushed [`InterruptStackFrame`] that sits immediately above it to
/// recover the interrupted context's `CS`, which decides whether the tick
/// preempts (ring 3) or merely accounts (ring 0).
///
/// Steps (in order):
///
/// 1. Read the LAPIC ID from MMIO and look up the dense `CpuId`.
/// 2. Invoke the installed scheduler-tick callback (if any) with that
///    `CpuId` (EEVDF is tickless in production, so there usually is none).
/// 3. Write `0` to the LAPIC EOI register, releasing the in-service bit
///    for the timer vector so the next tick can be delivered — done
///    **before** any preemptive context switch so the switch cannot strand
///    the in-service bit while another task runs.
/// 4. If the tick was taken from **ring 3** and a ring-3-preemption
///    callback is installed, invoke it (it suspends the running user task
///    back to the scheduler), bracketed by the `swapgs` pair that
///    establishes the in-handler GS convention for the kthread
///    cooperative-park balance and restores the user GS before `iretq`.
///
/// A tick taken in ring 0 never preempts: the kernel is non-preemptible
/// (`AGENTS.md` §4). The callback runs with interrupts disabled (the CPU
/// clears `IF` on interrupt-gate delivery), so the dispatcher is
/// non-re-entrant by construction.
///
/// # Safety
///
/// Only callable from the ISR stub, with `regs` the live saved-regs block
/// at the current `%rsp`. Invoking it from arbitrary Rust is undefined
/// behaviour: the EOI write assumes the LAPIC's in-service bit is set, and
/// the `InterruptStackFrame` read assumes the CPU-pushed frame sits above
/// `regs`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_arch_x86_64_timer_dispatch(regs: *mut SavedRegs) {
    // SAFETY: LAPIC MMIO is identity-mapped (boot.s SAFETY-INVARIANT 4).
    // The ID register is read-only and accessing it has no side
    // effects. The EOI register accepts any 32-bit write.
    let lapic_id = unsafe {
        let id_reg = (LAPIC_BASE_PHYS + 0x20) as *const u32;
        core::ptr::read_volatile(id_reg) >> 24
    };

    let cpu_id = LAPIC_TO_CPU_ID[(lapic_id & 0xFF) as usize].load(Ordering::Relaxed);

    let raw = TIMER_CALLBACK_FN.load(Ordering::Relaxed);
    if raw != 0 && cpu_id != u32::MAX {
        // SAFETY: every store into `TIMER_CALLBACK_FN` is the
        // round-trip of a valid `extern "C" fn(u32)` pointer through
        // `set_timer_callback`. The callback is `fn` (not a closure),
        // so it has no captured environment and is safe to invoke
        // from interrupt context with interrupts disabled.
        let cb: extern "C" fn(u32) =
            unsafe { core::mem::transmute::<usize, extern "C" fn(u32)>(raw) };
        cb(cpu_id);
    }

    // SAFETY: LAPIC_EOI_OFFSET is the architecturally-fixed EOI
    // register; writing `0` is the documented "end-of-interrupt"
    // sequence (Intel SDM Vol 3A §11.8.5). EOI is written *before* the
    // preemptive switch below so the in-service bit is released and a
    // later resumed task can be preempted again.
    unsafe {
        let eoi = (LAPIC_BASE_PHYS + LAPIC_EOI_OFFSET as u64) as *mut u32;
        core::ptr::write_volatile(eoi, 0);
    }

    // Involuntary preemption (`plans/PI.md` D2b-2b-A P-1c): a tick taken
    // from ring 3 suspends the running user task back to the scheduler,
    // exactly as a cooperative `yield` does. A tick taken in ring 0 never
    // preempts — the kernel is non-preemptible (`AGENTS.md` §4) — and an
    // absent callback keeps the system cooperative (fail-safe, §2.9).
    let preempt_raw = PREEMPT_CALLBACK_FN.load(Ordering::Relaxed);
    if preempt_raw != 0 && cpu_id != u32::MAX {
        // The CPU-pushed interrupt frame sits immediately above the saved
        // GPR block the `define_isr!` stub pushed; its `cs` is the selector
        // of the interrupted context.
        // SAFETY: `regs` is the live saved-regs block the stub passed at the
        // current `%rsp`; the `InterruptStackFrame` the CPU pushed lies
        // exactly `size_of::<SavedRegs>()` bytes above it (the stub inserts
        // no other words between them, per `define_isr!`), so the read is in
        // bounds and reads an initialised qword.
        let from_ring3 = unsafe {
            let frame =
                (regs as usize + core::mem::size_of::<SavedRegs>()) as *const InterruptStackFrame;
            cs_is_ring3((*frame).cs)
        };
        if from_ring3 {
            // SAFETY: every store into `PREEMPT_CALLBACK_FN` round-trips a
            // valid `extern "C" fn(u32)` through `set_preempt_callback`; the
            // callback is a `fn` with no captured environment, safe to call
            // from interrupt context.
            let cb: extern "C" fn(u32) =
                unsafe { core::mem::transmute::<usize, extern "C" fn(u32)>(preempt_raw) };
            // Establish the in-handler GS convention (current GS = kernel
            // TLS) the kthread cooperative-park balance expects, exactly as
            // the `syscall` entry stub's `swapgs` does (`plans/PI.md` X2):
            // an interrupt gate taken from ring 3 does *not* swap GS, so on
            // entry the current GS is still the user value. The callback's
            // `reschedule_current` flips GS to the between-handler
            // convention for the dispatcher (`enter_cooperative_park`) and
            // back on resume (`leave_cooperative_park`); the closing
            // `swapgs` then restores the user GS before the stub's `iretq`
            // returns to ring 3.
            // SAFETY: `swapgs` is privileged and runs in ring 0 here; it
            // touches only the GS-base/`KERNEL_GS_BASE` swap, no memory or
            // flags. The two swaps bracket exactly one preempt callback on
            // this task's own ISR control flow, so they pair.
            unsafe {
                core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
            }
            cb(cpu_id);
            // SAFETY: as above — the matching swap restoring the user GS the
            // `iretq` returns into.
            unsafe {
                core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
            }
        }
    }
}

// Emit the actual ISR stub the IDT vector points at. The macro's
// `unsafe(naked)` attribute is gated to the freestanding target, so
// the symbol only exists when `interrupts.s` does — host builds carry
// neither.
crate::define_isr!(rustos_arch_x86_64_isr_timer => rustos_arch_x86_64_timer_dispatch);

/// Return the linear address of the timer ISR stub for IDT
/// installation. Only meaningful on the freestanding target.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn timer_isr_addr() -> u64 {
    rustos_arch_x86_64_isr_timer as *const () as usize as u64
}

// --- Per-CPU init --------------------------------------------------

/// Initialise LAPIC-timer-driven preemption on the calling CPU.
///
/// Steps performed:
///
/// 1. Install the timer ISR stub in the calling CPU's per-CPU IDT at
///    [`TIMER_VECTOR`].
/// 2. Program the LAPIC timer in periodic mode from `calibration`
///    (the [`Calibration`] the BSP produced via
///    [`crate::apic_timer::calibrate`]).
///
/// The function does *not* enable interrupts — the caller is
/// responsible for `sti` once it is ready to accept ticks. This split
/// matches the AP-bring-up sequence in `scheduler_stress_qemu`:
/// `percpu::init` → `init_local_preempt` → `sti` → step loop.
///
/// # Errors
///
/// * [`crate::percpu::InitError::CpuIndexOutOfRange`] if `cpu_index`
///   is outside the registered [`crate::percpu::PerCpuStorage`].
/// * [`crate::percpu::InitError::NotInitialised`] if
///   [`crate::percpu::init`] has not yet run for `cpu_index`.
///
/// # Safety
///
/// * `cpu_index` must be the index passed to
///   [`crate::percpu::init`] on *this* CPU. Passing another CPU's
///   index would install the timer vector into the wrong IDT.
/// * Interrupts on the calling CPU must be disabled.
/// * `lapic` must be the calling CPU's `Lapic` — the function
///   programs the LVT and initial-count registers of whatever LAPIC
///   the driver wraps.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn init_local_preempt<M: LapicMmio>(
    cpu_index: usize,
    lapic: &mut Lapic<M>,
    calibration: Calibration,
) -> Result<(), crate::percpu::InitError> {
    // 1. Install the timer ISR in this CPU's IDT.
    // SAFETY: caller's contract guarantees this is the CPU whose
    // index was passed to `percpu::init`, and interrupts are
    // disabled.
    unsafe {
        crate::percpu::install_vector(cpu_index, TIMER_VECTOR, timer_isr_addr())?;
    }

    // 2. Program the LAPIC timer in periodic mode.
    apic_timer::program_periodic(lapic, calibration, TIMER_VECTOR);

    Ok(())
}

// --- Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_vector_is_first_user_vector() {
        // 0x00..=0x1F are reserved architectural exceptions. The
        // timer is the first user-installable vector. If this number
        // ever needs to change, the scheduler_stress_qemu binary
        // must be updated in lock-step — there is no other consumer.
        assert_eq!(TIMER_VECTOR, 0x20);
    }

    #[test]
    fn lapic_constants_match_intel_sdm() {
        // EOI = 0xB0 per Intel SDM Vol 3A §11.4.1 Table 11-1.
        assert_eq!(LAPIC_EOI_OFFSET, 0xB0);
        // LAPIC default base = 0xFEE0_0000 per SDM §11.4.5.
        assert_eq!(LAPIC_BASE_PHYS, 0xFEE0_0000);
    }

    #[test]
    fn timer_callback_round_trip_on_host_is_none() {
        // The freestanding-target storage is `cfg`-gated out on the
        // host. `set_timer_callback` is callable on the host (it's a
        // no-op stub); the getter must consistently report "none".
        extern "C" fn cb(_cpu: u32) {}
        set_timer_callback(cb);
        assert!(timer_callback().is_none());
    }

    #[test]
    fn set_cpu_id_for_lapic_on_host_is_inert() {
        // Same gating as above. The host build cannot observe a real
        // mapping; we cross-check that the getter returns the
        // documented sentinel.
        set_cpu_id_for_lapic(0, 7);
        assert_eq!(cpu_id_for_lapic(0), u32::MAX);
    }

    #[test]
    fn preempt_callback_on_host_is_none() {
        // The ring-3-preemption callback storage is `cfg`-gated out on the
        // host, exactly like `TIMER_CALLBACK_FN`. `set_preempt_callback` is
        // a no-op stub on the host; the getter must consistently report
        // "none" so a regression that quietly enables host-side storage is
        // caught.
        extern "C" fn cb(_cpu: u32) {}
        set_preempt_callback(cb);
        assert!(preempt_callback().is_none());
    }

    #[test]
    fn cs_is_ring3_reads_the_selector_rpl() {
        // Kernel CS (RPL 0) is not ring 3; a ring-3 selector (RPL 3) is.
        assert!(!cs_is_ring3(0x08)); // kernel CS, RPL 0
        assert!(!cs_is_ring3(0x00));
        // User 64-bit CS at GDT index 5 with RPL 3 (`(5 << 3) | 3 = 0x2B`).
        assert!(cs_is_ring3(0x2B));
        // Only the low two bits matter — RPL 1/2 are not ring 3.
        assert!(!cs_is_ring3(0x29)); // ...01
        assert!(!cs_is_ring3(0x2A)); // ...10
        assert!(cs_is_ring3(0x2B)); // ...11
    }
}
