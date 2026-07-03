//! Cross-CPU TLB shootdown on x86_64 (the `plans/WIRING.md` W6 slice of
//! the Arch HAL "TLB shootdown" surface).
//!
//! x86_64 has no broadcast TLB-invalidation instruction: `invlpg` only
//! affects the CPU that executes it. To make a page-table edit globally
//! visible the initiating CPU must therefore *interrupt* every other
//! online CPU and have each one run `invlpg` for the affected page —
//! the classic inter-processor "TLB shootdown". This module owns that
//! protocol; `crate::kernel_arch::X86_64Arch` implements
//! [`rustos_arch_api::CrossCpuTlbShootdown`] over it.
//!
//! # Protocol
//!
//! A single global descriptor (`SHOOTDOWN`) serialises shootdowns
//! behind a spin-acquired flag, so the (rare) page-table-teardown path
//! never races itself across CPUs. The initiator:
//!
//! 1. spin-acquires the descriptor lock,
//! 2. publishes the target virtual address and the number of CPUs it is
//!    about to interrupt (the outstanding-acknowledge count),
//! 3. raises a [`TLB_SHOOTDOWN_VECTOR`] IPI at each of those CPUs,
//! 4. invalidates the page on *itself* with `invlpg`,
//! 5. spins until every interrupted CPU has acknowledged (the count
//!    reaches zero), then releases the lock.
//!
//! Each interrupted CPU runs `rustos_arch_x86_64_tlb_shootdown_dispatch`
//! from the ISR: it reads the published address, runs `invlpg`, writes
//! the LAPIC end-of-interrupt, and decrements the acknowledge count.
//!
//! The spin in step 5 is a genuine, bounded synchronisation — every
//! targeted CPU has interrupts enabled in the kernel idle/work loop and
//! will service the IPI — not a "retry until it works" bring-up hack: under-invalidating (returning before a CPU has
//! flushed) is the only failure mode, so the initiator *must* wait for
//! the acknowledge. Deadlock is impossible because only the lock holder
//! sends IPIs, and a CPU spinning to *acquire* the lock still services
//! incoming shootdown IPIs (its interrupts are not masked).
//!
//! # Host build
//!
//! The mailbox, the ISR, and the install helper are gated to
//! `target_os = "none"`: they reach LAPIC MMIO and the per-CPU IDT.
//! The host build carries none of them; the `X86_64Arch` shootdown
//! impl is a vacuous no-op there (there is no second CPU and no TLB),
//! and the conformance vertical asserts only that the call is
//! total and panic-free. The real cross-CPU round-trip is proven by the
//! `cross_cpu_tlb_shootdown_qemu_x86_64` QEMU vertical.
//!

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::interrupts::SavedRegs;

/// IDT vector the cross-CPU TLB-shootdown IPI is delivered on.
///
/// One past [`crate::preempt::TIMER_VECTOR`] (`0x20`); the first
/// user-defined vectors are `0x20..` (Intel SDM Vol 3A §6.3.1). The
/// constant is `pub` so the integration test can cross-check the IDT
/// slot it installs.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0x21;

/// Global, lock-serialised shootdown descriptor.
///
/// There is at most one in-flight cross-CPU shootdown system-wide; the
/// `lock` flag enforces that. `vaddr` is the page to invalidate and
/// `pending` is the number of interrupted CPUs that have not yet
/// acknowledged.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
struct ShootdownMailbox {
    /// Spin flag: `true` while an initiator owns the descriptor.
    lock: AtomicBool,
    /// The virtual address whose page every target must `invlpg`.
    vaddr: AtomicU64,
    /// Outstanding acknowledges; the initiator waits for this to reach 0.
    pending: AtomicUsize,
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static SHOOTDOWN: ShootdownMailbox = ShootdownMailbox {
    lock: AtomicBool::new(false),
    vaddr: AtomicU64::new(0),
    pending: AtomicUsize::new(0),
};

/// Invalidate the 4 KiB page containing `vaddr` on the calling CPU and on
/// every CPU whose LAPIC ID `targets` yields, returning once all of them
/// have acknowledged.
///
/// `targets` must yield the *other* online CPUs' LAPIC ids (never the
/// caller). An empty iterator degrades to a purely local `invlpg`. The
/// iterator is taken `Clone` rather than as a `&[u8]` so the caller can
/// stream the ids straight out of its caller-sized per-CPU map without a
/// fixed `MAX_CPUS` scratch buffer; `shootdown` walks
/// it twice — once to publish the acknowledge count, once to raise the
/// IPIs — so it must be cheap to re-walk.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn shootdown<I>(vaddr: u64, targets: I)
where
    I: Iterator<Item = u8> + Clone,
{
    // Acquire the global descriptor. `Acquire` pairs with the `Release`
    // store in the unlock below so a previous shootdown's writes are
    // visible before this one reuses the mailbox.
    while SHOOTDOWN
        .lock
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }

    // Publish the page and the acknowledge count *before* any IPI is
    // raised. `vaddr` is stored with `Release` so the ISR's `Acquire`
    // load synchronises-with it; the IPI send below is additionally an
    // MMIO write (a strong ordering point), so a target never observes a
    // stale address.
    SHOOTDOWN.vaddr.store(vaddr, Ordering::Release);
    SHOOTDOWN
        .pending
        .store(targets.clone().count(), Ordering::Release);

    // SAFETY: `LAPIC_BASE_PHYS` is identity-mapped (boot.s
    // SAFETY-INVARIANT 4). Each CPU accesses its own per-CPU LAPIC at
    // that physical address, so concurrent senders touch independent
    // registers; the global lock above already serialises shootdowns on
    // this CPU.
    let mmio =
        unsafe { crate::apic::VolatileLapicMmio::new(crate::preempt::LAPIC_BASE_PHYS as *mut u32) };
    let mut lapic = crate::apic::Lapic::new(mmio);
    for target in targets {
        lapic.send_ipi(
            target,
            crate::apic::DeliveryMode::Fixed,
            TLB_SHOOTDOWN_VECTOR,
        );
    }

    // Invalidate locally while the targets are flushing in parallel.
    invlpg(vaddr);

    // Wait for every interrupted CPU to acknowledge. `Acquire` pairs with
    // the dispatcher's `Release` decrement so the remote `invlpg`s are
    // ordered before this call returns.
    while SHOOTDOWN.pending.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }

    SHOOTDOWN.lock.store(false, Ordering::Release);
}

/// Invalidate the calling CPU's TLB entry for the page containing
/// `vaddr`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn invlpg(vaddr: u64) {
    // SAFETY: `invlpg` invalidates the calling CPU's TLB entry for the
    // page containing the operand address; it touches no memory and only
    // discards a cached translation. No Rust spelling exists. This is the
    // same instruction `crate::paging`'s local `TlbShootdown::flush_page`
    // issues (the local-invalidation primitive); the cross-CPU initiator
    // and the per-target ISR below both reuse it.
    unsafe {
        core::arch::asm!(
            "invlpg [{addr}]",
            addr = in(reg) vaddr,
            options(nostack, preserves_flags),
        );
    }
}

/// Rust trampoline called by the shootdown ISR stub.
///
/// Reads the published page, invalidates it on this CPU, writes the LAPIC
/// end-of-interrupt, and decrements the outstanding-acknowledge count so
/// the initiator can observe completion.
///
/// # Safety
///
/// Only callable from the ISR stub. Invoking it from arbitrary Rust is
/// undefined behaviour because the EOI write assumes the LAPIC's
/// in-service bit is set for [`TLB_SHOOTDOWN_VECTOR`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn rustos_arch_x86_64_tlb_shootdown_dispatch(_regs: *mut SavedRegs) {
    // `Acquire` pairs with the initiator's `Release` store of `pending`,
    // which is published after `vaddr`, so the address read here is the
    // current one.
    let vaddr = SHOOTDOWN.vaddr.load(Ordering::Acquire);
    invlpg(vaddr);

    // SAFETY: `LAPIC_EOI_OFFSET` is the architecturally-fixed EOI
    // register; writing `0` is the documented end-of-interrupt sequence
    // (Intel SDM Vol 3A §11.8.5). LAPIC MMIO is identity-mapped.
    unsafe {
        let eoi =
            (crate::preempt::LAPIC_BASE_PHYS + crate::preempt::LAPIC_EOI_OFFSET as u64) as *mut u32;
        core::ptr::write_volatile(eoi, 0);
    }

    // Acknowledge last, with `Release`, so the remote `invlpg` above is
    // ordered before the initiator's `Acquire` load observes the
    // decrement. `saturating_sub`-style guard: a spurious delivery with
    // `pending == 0` must not wrap the counter.
    let prev = SHOOTDOWN.pending.load(Ordering::Relaxed);
    if prev != 0 {
        SHOOTDOWN.pending.fetch_sub(1, Ordering::Release);
    }
}

// Emit the ISR stub the IDT vector points at (gated to the freestanding
// target by the macro, exactly like the timer ISR).
crate::define_isr!(rustos_arch_x86_64_isr_tlb_shootdown => rustos_arch_x86_64_tlb_shootdown_dispatch);

/// Linear address of the shootdown ISR stub, for IDT installation.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn tlb_shootdown_isr_addr() -> u64 {
    rustos_arch_x86_64_isr_tlb_shootdown as *const () as usize as u64
}

/// Install the cross-CPU TLB-shootdown ISR in the calling CPU's per-CPU
/// IDT at [`TLB_SHOOTDOWN_VECTOR`].
///
/// Called once on every CPU as it comes online, alongside
/// [`crate::preempt::init_local_preempt`], so the CPU can service a
/// shootdown IPI. Unlike the timer it programs no device — the vector is
/// raised on demand by `shootdown`.
///
/// # Errors
///
/// * [`crate::percpu::InitError::CpuIndexOutOfRange`] if `cpu_index` is
///   outside the registered [`crate::percpu::PerCpuStorage`].
/// * [`crate::percpu::InitError::NotInitialised`] if
///   [`crate::percpu::init`] has not yet run for `cpu_index`.
///
/// # Safety
///
/// * `cpu_index` must be the index passed to [`crate::percpu::init`] on
///   *this* CPU.
/// * Interrupts on the calling CPU must be disabled during install.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub unsafe fn init_local_tlb_shootdown(cpu_index: usize) -> Result<(), crate::percpu::InitError> {
    // SAFETY: caller's contract guarantees this is the CPU whose index
    // was passed to `percpu::init`, and interrupts are disabled.
    unsafe {
        crate::percpu::install_vector(cpu_index, TLB_SHOOTDOWN_VECTOR, tlb_shootdown_isr_addr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shootdown_vector_is_one_past_the_timer_vector() {
        // The timer owns 0x20 (the first user vector); the shootdown IPI
        // takes the next slot. If this changes, the QEMU vertical that
        // installs the vector must change in lock-step.
        assert_eq!(TLB_SHOOTDOWN_VECTOR, 0x21);
        assert_eq!(TLB_SHOOTDOWN_VECTOR, crate::preempt::TIMER_VECTOR + 1);
    }
}
