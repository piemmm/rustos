//! Cross-CPU TLB shootdown on x86_64 (the `plans/WIRING.md` W6 slice of
//! the Arch HAL "TLB shootdown" surface).
//!
//! x86_64 has no broadcast TLB-invalidation instruction: `invlpg` only
//! affects the CPU that executes it. To make a page-table edit globally
//! visible the initiating CPU must therefore *interrupt* every other
//! online CPU and have each one run `invlpg` for the affected page —
//! the classic inter-processor "TLB shootdown". This module owns that
//! protocol; `crate::kernel_arch::X86_64Arch` implements
//! [`tairix_arch_api::CrossCpuTlbShootdown`] over it.
//!
//! # Protocol
//!
//! A single global descriptor (`SHOOTDOWN`) serialises shootdowns behind a
//! spin-acquired flag, so the (rare) page-table-teardown path never races
//! itself across CPUs. The initiator:
//!
//! 1. builds the set of CPUs to interrupt as a bitmap of LAPIC ids, minus
//!    its own bit (`target_map`),
//! 2. spin-acquires the descriptor lock,
//! 3. stores the target range and the outstanding-acknowledge count, then
//!    publishes the bitmap **last** — the bitmap is the "go" signal,
//! 4. raises a [`TLB_SHOOTDOWN_VECTOR`] IPI at each CPU in the bitmap,
//! 5. invalidates the range on *itself* with `invlpg`,
//! 6. spins until every target has acknowledged (the count reaches zero),
//!    then releases the lock.
//!
//! With no target at all — the single-CPU case — there is nothing to publish
//! and nobody to wait for, so the call is just the local `invlpg` sweep and
//! never touches the descriptor.
//!
//! The spin in step 6 is a genuine, bounded synchronisation, not a "retry
//! until it works" bring-up hack: under-invalidating (returning before a CPU
//! has flushed) is the only failure mode, so the initiator *must* wait for
//! the acknowledge.
//!
//! # A target acknowledges from a spin as readily as from its ISR
//!
//! The acknowledge is `serve_pending`, reached from two places: the
//! shootdown ISR, and any spin round in `lib/sync` (the boot path installs it
//! as that crate's spin service). Both matter, because a CPU whose own
//! interrupts are masked cannot take the IPI at all — and masking is exactly
//! what `tairix_sync::IrqSafeSpinLock` does for the whole of its acquire
//! spin, the kernel heap's lock included. An initiator holding that heap lock
//! and a second CPU spinning to acquire it would otherwise wait on each other
//! for ever: the initiator for an acknowledge the masked CPU cannot send, the
//! masked CPU for a lock the initiator will not release until it has one.
//!
//! Serving from a spin means a target could otherwise acknowledge twice —
//! once from the spin, once when the deferred IPI is finally delivered —
//! double-decrementing the count and returning the *next* initiator early,
//! i.e. under-invalidating. The bitmap forecloses it: a target claims by
//! clearing its own bit, so the prior value that `fetch_and` returns is both
//! the claim and the "am I a target?" test, and exactly one caller can win
//! it. A CPU whose bit is already clear — a stale delivery, a CPU that was
//! never asked, the initiator itself — invalidates nothing and decrements
//! nothing. The ISR still writes its LAPIC EOI unconditionally: the
//! in-service bit is set whether or not there was work to do.
//!
//! # Host build
//!
//! The descriptor, the ISR, and the install helper are gated to
//! `target_os = "none"`: they reach LAPIC MMIO and the per-CPU IDT. The
//! bitmap bookkeeping is not, so the decisions the protocol rests on — who
//! is asked, who is excluded, how many acknowledges are owed — are
//! host-tested below. The host `X86_64Arch` shootdown impl is a vacuous
//! no-op (there is no second CPU and no TLB) and the conformance vertical
//! asserts only that the call is total and panic-free; the real cross-CPU
//! round-trip, including the masked-spin acknowledge, is proven by the
//! `cross_cpu_tlb_shootdown_qemu_x86_64` QEMU vertical.
//!

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
use crate::interrupts::SavedRegs;

/// IDT vector the cross-CPU TLB-shootdown IPI is delivered on.
///
/// One past [`crate::preempt::TIMER_VECTOR`] (`0x20`); the first
/// user-defined vectors are `0x20..` (Intel SDM Vol 3A §6.3.1). The
/// constant is `pub` so the integration test can cross-check the IDT
/// slot it installs.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0x21;

/// Bytes in the target bitmap: one bit per xAPIC id.
///
/// An xAPIC id is 8 bits wide (Intel SDM Vol 3A §11.4.6), the same width
/// every LAPIC-id-taking path on this port carries, so 32 bytes covers the id
/// space exactly and no id can fall outside the map. That is an architectural
/// width, not a CPU-count ceiling: it neither shrinks on a small machine nor
/// needs raising on a large one.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
const TARGET_BYTES: usize = 32;

/// The bitmap byte and bit mask a LAPIC id occupies.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
const fn target_slot(lapic_id: u8) -> (usize, u8) {
    ((lapic_id >> 3) as usize, 1u8 << (lapic_id & 7))
}

/// The target bitmap for `targets`, excluding `own`, and the number of
/// acknowledges it owes.
///
/// Excluding the caller here rather than trusting it to exclude itself is
/// what makes the acknowledge wait unable to wait on the initiator, and
/// collapsing duplicates is what stops a repeated id inflating the count into
/// a wait that never ends.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
fn target_map(targets: impl Iterator<Item = u8>, own: u8) -> ([u8; TARGET_BYTES], usize) {
    let mut map = [0u8; TARGET_BYTES];
    for id in targets {
        let (byte, bit) = target_slot(id);
        map[byte] |= bit;
    }
    let (own_byte, own_bit) = target_slot(own);
    map[own_byte] &= !own_bit;
    let owed = map.iter().map(|bits| bits.count_ones() as usize).sum();
    (map, owed)
}

/// Call `f` with every LAPIC id set in `map`.
#[cfg(any(test, all(target_arch = "x86_64", target_os = "none")))]
fn for_each_target(map: &[u8; TARGET_BYTES], mut f: impl FnMut(u8)) {
    let mut base: u8 = 0;
    for bits in map {
        if *bits != 0 {
            for shift in 0..8u8 {
                if bits & (1u8 << shift) != 0 {
                    f(base | shift);
                }
            }
        }
        // The map's 32 bytes cover the 8-bit id space exactly, so the last
        // step is the one that wraps and no id is built from it.
        base = base.wrapping_add(8);
    }
}

/// Global, lock-serialised shootdown descriptor.
///
/// There is at most one in-flight cross-CPU shootdown system-wide; the
/// `lock` flag enforces that.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
struct ShootdownMailbox {
    /// Spin flag: `true` while an initiator owns the descriptor.
    lock: AtomicBool,
    /// First page of the range every target must `invlpg`.
    vaddr: AtomicU64,
    /// How many consecutive 4 KiB pages from `vaddr` to invalidate.
    pages: AtomicUsize,
    /// Outstanding acknowledges; the initiator waits for this to reach 0.
    ///
    /// Never below the number of bits still set in `targets`, because a
    /// target clears its bit before it decrements. So `pending == 0` proves
    /// no bit is set, which is what lets [`serve_pending`] gate on one load.
    pending: AtomicUsize,
    /// One bit per LAPIC id still owing an acknowledge: published last, and
    /// cleared by the owning CPU as its claim.
    targets: [AtomicU8; TARGET_BYTES],
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
static SHOOTDOWN: ShootdownMailbox = ShootdownMailbox {
    lock: AtomicBool::new(false),
    vaddr: AtomicU64::new(0),
    pages: AtomicUsize::new(0),
    pending: AtomicUsize::new(0),
    targets: [const { AtomicU8::new(0) }; TARGET_BYTES],
};

/// Invalidate `pages` consecutive 4 KiB pages from the page containing
/// `vaddr` on the calling CPU and on every CPU whose LAPIC ID `targets`
/// yields, returning once all of them have acknowledged.
///
/// `targets` may yield the calling CPU's own id and may repeat one: the
/// bitmap excludes the caller and collapses duplicates (`target_map`). A
/// zero page count is a no-op, and an empty target set degrades to a purely
/// local `invlpg` sweep that never touches the descriptor.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn shootdown<I>(vaddr: u64, pages: usize, targets: I)
where
    I: Iterator<Item = u8>,
{
    if pages == 0 {
        return;
    }

    // Built before the descriptor is touched, so the lock is held for the
    // round-trip alone.
    let (map, owed) = target_map(targets, crate::preempt::local_lapic_id());
    if owed == 0 {
        invlpg_range(vaddr, pages);
        return;
    }

    // Acquire the descriptor, serving any request already in flight: this
    // spin is reached with interrupts masked (the kernel-heap teardown is
    // such a caller), so waiting without serving would leave the CPU holding
    // the descriptor waiting on an acknowledge this CPU cannot send.
    while SHOOTDOWN
        .lock
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        serve_pending();
        core::hint::spin_loop();
    }

    // The range and the count go out *before* the bitmap: a target that wins
    // its bit decrements `pending` at once, so storing `pending` after the
    // bitmap could overwrite that decrement.
    SHOOTDOWN.vaddr.store(vaddr, Ordering::Relaxed);
    SHOOTDOWN.pages.store(pages, Ordering::Relaxed);
    SHOOTDOWN.pending.store(owed, Ordering::Relaxed);
    for (slot, bits) in SHOOTDOWN.targets.iter().zip(map) {
        // `Release` on every byte, not just the last: a target reads only
        // the byte its own id lies in, so each must carry the range with it.
        slot.store(bits, Ordering::Release);
    }

    // SAFETY: `LAPIC_BASE_PHYS` is identity-mapped (boot.s
    // SAFETY-INVARIANT 4). Each CPU accesses its own per-CPU LAPIC at
    // that physical address, so concurrent senders touch independent
    // registers; the global lock above already serialises shootdowns on
    // this CPU.
    let mmio =
        unsafe { crate::apic::VolatileLapicMmio::new(crate::preempt::LAPIC_BASE_PHYS as *mut u32) };
    let mut lapic = crate::apic::Lapic::new(mmio);
    // Raised from the local copy, never from the published bitmap the targets
    // are concurrently clearing, so the set asked is exactly the set counted.
    for_each_target(&map, |id| {
        lapic.send_ipi(id, crate::apic::DeliveryMode::Fixed, TLB_SHOOTDOWN_VECTOR);
    });

    // Invalidate locally while the targets are flushing in parallel.
    invlpg_range(vaddr, pages);

    // Wait for every interrupted CPU to acknowledge. `Acquire` pairs with the
    // acknowledge's `Release` decrement so the remote `invlpg`s are ordered
    // before this call returns. Serving here would be dead work: this CPU's
    // own bit was masked out above, so it can never be a target of its own
    // request.
    while SHOOTDOWN.pending.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
    }

    SHOOTDOWN.lock.store(false, Ordering::Release);
}

/// Claim and discharge this CPU's outstanding acknowledge, if it has one.
///
/// Total and idempotent: a CPU that was never asked, or that has already
/// acknowledged, invalidates nothing and decrements nothing. The boot path
/// installs it as `lib/sync`'s spin service so a CPU spinning with its
/// interrupts masked still acknowledges; the shootdown ISR calls it for the
/// ordinary interrupt-delivered case.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn serve_pending() {
    // Nothing in flight is the overwhelmingly common case and must cost one
    // load rather than a LAPIC MMIO read. Sound because `pending` is never
    // below the number of bits still set.
    if SHOOTDOWN.pending.load(Ordering::Acquire) == 0 {
        return;
    }

    let (byte, bit) = target_slot(crate::preempt::local_lapic_id());
    // The claim. `AcqRel` so the range read below cannot be hoisted above it,
    // and so it synchronises-with the initiator's `Release` publish of this
    // byte.
    if SHOOTDOWN.targets[byte].fetch_and(!bit, Ordering::AcqRel) & bit == 0 {
        return;
    }

    // Winning the claim proves this CPU is a target that has not yet
    // acknowledged, so the initiator is still inside `shootdown` holding the
    // descriptor and the range read here is still its range.
    let pages = SHOOTDOWN.pages.load(Ordering::Relaxed);
    let vaddr = SHOOTDOWN.vaddr.load(Ordering::Relaxed);
    invlpg_range(vaddr, pages);

    // Acknowledge last, with `Release`, so the `invlpg`s above are ordered
    // before the initiator's `Acquire` load observes the decrement.
    SHOOTDOWN.pending.fetch_sub(1, Ordering::Release);
}

/// Invalidate the calling CPU's TLB entries for `pages` consecutive 4 KiB
/// pages from the page containing `vaddr`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn invlpg_range(vaddr: u64, pages: usize) {
    const PAGE_BYTES: u64 = 4096;
    let mut page = vaddr & !(PAGE_BYTES - 1);
    for _ in 0..pages {
        invlpg(page);
        page = page.wrapping_add(PAGE_BYTES);
    }
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
/// # Safety
///
/// Only callable from the ISR stub. Invoking it from arbitrary Rust is
/// undefined behaviour because the EOI write assumes the LAPIC's
/// in-service bit is set for [`TLB_SHOOTDOWN_VECTOR`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[no_mangle]
unsafe extern "C" fn tairix_arch_x86_64_tlb_shootdown_dispatch(_regs: *mut SavedRegs) {
    serve_pending();

    // Unconditional, unlike the acknowledge above: the in-service bit is set
    // for this vector whether or not this CPU still owed one.
    // SAFETY: `LAPIC_EOI_OFFSET` is the architecturally-fixed EOI
    // register; writing `0` is the documented end-of-interrupt sequence
    // (Intel SDM Vol 3A §11.8.5). LAPIC MMIO is identity-mapped.
    unsafe {
        let eoi =
            (crate::preempt::LAPIC_BASE_PHYS + crate::preempt::LAPIC_EOI_OFFSET as u64) as *mut u32;
        core::ptr::write_volatile(eoi, 0);
    }
}

// Emit the ISR stub the IDT vector points at (gated to the freestanding
// target by the macro, exactly like the timer ISR).
crate::define_isr!(tairix_arch_x86_64_isr_tlb_shootdown => tairix_arch_x86_64_tlb_shootdown_dispatch);

/// Linear address of the shootdown ISR stub, for IDT installation.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub fn tlb_shootdown_isr_addr() -> u64 {
    tairix_arch_x86_64_isr_tlb_shootdown as *const () as usize as u64
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
    use super::{for_each_target, target_map, target_slot, TARGET_BYTES, TLB_SHOOTDOWN_VECTOR};

    /// Every LAPIC id lands in the map, and no two share a bit.
    #[test]
    fn the_bitmap_covers_the_whole_xapic_id_space_one_bit_each() {
        let mut seen = [false; 256];
        for id in 0..=u8::MAX {
            let (byte, bit) = target_slot(id);
            assert!(byte < TARGET_BYTES, "id {id} outside the map");
            let index = byte * 8 + bit.trailing_zeros() as usize;
            assert!(!seen[index], "id {id} shares a bit");
            seen[index] = true;
        }
        assert!(seen.iter().all(|hit| *hit));
    }

    #[test]
    fn the_map_excludes_the_caller_so_it_cannot_wait_on_itself() {
        // The caller is listed anyway, exactly as a careless caller would.
        let (map, owed) = target_map([0u8, 1, 2].into_iter(), 1);
        assert_eq!(owed, 2, "the caller's own id owes no acknowledge");
        let (byte, bit) = target_slot(1);
        assert_eq!(map[byte] & bit, 0);
        let mut asked = [false; 256];
        for_each_target(&map, |id| asked[id as usize] = true);
        assert!(asked[0] && asked[2] && !asked[1]);
    }

    #[test]
    fn a_repeated_id_owes_one_acknowledge_not_two() {
        // A duplicate that inflated the count would leave the initiator
        // waiting for an acknowledge no CPU owes it.
        let (map, owed) = target_map([7u8, 7, 7, 200, 200].into_iter(), 0);
        assert_eq!(owed, 2);
        let mut asked = 0;
        for_each_target(&map, |_| asked += 1);
        assert_eq!(asked, 2, "one IPI per distinct target");
    }

    #[test]
    fn an_empty_or_self_only_target_set_owes_nothing() {
        assert_eq!(target_map(core::iter::empty(), 3).1, 0);
        assert_eq!(target_map(core::iter::once(3), 3).1, 0);
    }

    /// The map and the ids it yields are inverses across the whole id space,
    /// including the top byte whose walk ends on the wrapping step.
    #[test]
    fn every_asked_id_is_yielded_back_exactly_once() {
        let ids: [u8; 6] = [0, 8, 63, 64, 254, 255];
        let (map, owed) = target_map(ids.into_iter(), 1);
        assert_eq!(owed, ids.len());
        let mut yielded = [0usize; 256];
        for_each_target(&map, |id| yielded[id as usize] += 1);
        for id in ids {
            assert_eq!(yielded[id as usize], 1, "id {id} not yielded once");
        }
        assert_eq!(yielded.iter().sum::<usize>(), owed);
    }

    #[test]
    fn shootdown_vector_is_one_past_the_timer_vector() {
        // The timer owns 0x20 (the first user vector); the shootdown IPI
        // takes the next slot. If this changes, the QEMU vertical that
        // installs the vector must change in lock-step.
        assert_eq!(TLB_SHOOTDOWN_VECTOR, 0x21);
        assert_eq!(TLB_SHOOTDOWN_VECTOR, crate::preempt::TIMER_VECTOR + 1);
    }
}
