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
use crate::interrupts::SavedRegs;

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
/// `_regs` is the [`SavedRegs`] block the stub pushed; the dispatcher
/// does not currently consult it (the scheduler does not need it for
/// preemption — it only needs the current CPU's id). It is kept in
/// the signature so a future commit (full context-save preemption)
/// can pick it up without changing the ISR ABI.
///
/// Steps (in order):
///
/// 1. Read the LAPIC ID from MMIO and look up the dense `CpuId`.
/// 2. Invoke the installed callback (if any) with that `CpuId`.
/// 3. Write `0` to the LAPIC EOI register, releasing the in-service
///    bit for the timer vector so the next tick can be delivered.
///
/// EOI is performed *after* the callback so the scheduler has driven
/// at least one step before another timer tick can stack. The
/// callback runs with interrupts disabled (the CPU automatically
/// clears `IF` on interrupt-gate delivery), so the dispatcher itself
/// is non-re-entrant by construction.
///
/// # Safety
///
/// Only callable from the ISR stub. Invoking it from arbitrary Rust
/// is undefined behaviour because the EOI write below assumes the
/// LAPIC's in-service bit is set, and writing EOI with no pending
/// IRQ corrupts the LAPIC's TPR-arbitration state.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_arch_x86_64_timer_dispatch(_regs: *mut SavedRegs) {
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
    // sequence (Intel SDM Vol 3A §11.8.5).
    unsafe {
        let eoi = (LAPIC_BASE_PHYS + LAPIC_EOI_OFFSET as u64) as *mut u32;
        core::ptr::write_volatile(eoi, 0);
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
}
