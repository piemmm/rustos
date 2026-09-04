//! Cross-CPU TLB-shootdown surface of the Arch HAL (
//! "TLB shootdown").
//!
//! [`crate::tlb::TlbShootdown`] invalidates a stale cached translation on
//! the **calling** CPU. On an SMP system (SMP from day
//! one) that is not enough: after a leaf is torn down or its permissions
//! tightened, *every other* CPU that may have walked the same page table
//! can still hold the stale translation in its own TLB. Making the edit
//! globally visible means reaching those CPUs and invalidating their
//! cached entry too — the classic "TLB shootdown". The charter makes the
//! architecture surface a closed set of traits on the HAL; this module is
//! the *cross-CPU* member of the "TLB shootdown" set, the sibling of the
//! local [`crate::tlb`] slice.
//!
//! # Why a separate trait from [`crate::tlb::TlbShootdown`]
//!
//! The two are deliberately distinct primitives, not one trait with a
//! flag:
//!
//! * [`crate::tlb::TlbShootdown::flush_page`] is **privilege-neutral and
//!   purely local** — a single instruction (`invlpg` / `tlbi vae1is` /
//!   `sfence.vma`) that touches no other CPU and cannot fail. The
//!   per-process map/unmap path in `kernel/mem` drives it on every edit.
//! * [`CrossCpuTlbShootdown::shootdown_page`] is **a system-wide
//!   operation** that must interrupt the other online CPUs (or issue a
//!   broadcast invalidation) and only return once the invalidation is
//!   architecturally visible everywhere. It needs the calling handle's
//!   knowledge of the CPU topology, so it is implemented on the port's
//!   [`crate::SchedulerArch`] handle (which already owns
//!   [`crate::SchedulerArch::current_cpu`] and the directed-IPI path),
//!   not on a per-process page table.
//!
//! Collapsing them would force every cheap local flush through the
//! expensive cross-CPU path, or smuggle a "is this multi-CPU?" flag into
//! the hot map/unmap loop — the interface creep the charter forbids.
//!
//! # Per-arch shape (the modularity carve-out)
//!
//! Each port implements the *same* trait its own way; these parallel
//! implementations are the deliberate shape of the HAL, never collapsed
//! behind `cfg` (carve-out):
//!
//! * **x86_64** has no broadcast TLB invalidation, so the initiator
//!   raises an inter-processor interrupt to every other online CPU, each
//!   of which runs `invlpg` and acknowledges — from the shootdown ISR, or
//!   from a spin round when its interrupts are masked and the IPI cannot
//!   be taken; the initiator spins until every target has acknowledged.
//!   This is the only port that needs an explicit acknowledge protocol.
//! * **aarch64** issues `tlbi vaae1is` (the *inner-shareable* broadcast
//!   variant): the hardware itself invalidates the page on every PE in
//!   the inner-shareable domain, so the cross-CPU shootdown is the same
//!   instruction the local flush already uses, ordered by a `dsb ish` +
//!   `isb`. No IPI and no software acknowledge are required.
//! * **riscv64** has no broadcast `sfence.vma`, but the SBI firmware
//!   provides the **RFENCE** extension: `remote_sfence_vma` instructs the
//!   listed harts to fence and returns once they have. The initiator
//!   issues the SBI call (plus a local `sfence.vma`) and lets the
//!   firmware perform the remote acknowledge.
//! * **wasm32** is an honest **n/a**: a Web Worker owns an isolated
//!   linear `WebAssembly.Memory` with no shared page table and no TLB to
//!   shoot down, so the slice does not apply (`plans/WIRING.md` §0.4 —
//!   honest absence, never a faked no-op). wasm32 therefore implements no
//!   [`CrossCpuTlbShootdown`].
//!
//! # Why the host conformance vertical proves only the observable half
//!
//! Exactly as for [`crate::tlb`] and [`crate::mmu::AddressSpace::activate`],
//! the *effect* of a shootdown (a remote CPU re-walking the table) is not
//! observable from a single-threaded host test, and the IPI / broadcast /
//! SBI machinery only exists on the freestanding target. The host
//! [`conformance`] vertical therefore proves the contract that *is*
//! observable on the host — the call is object-safe, accepts any address,
//! and never panics or fails (a shootdown can only ever *over*-invalidate,
//! never refuse) — while the real cross-CPU round-trip is exercised
//! end-to-end by the multi-core `cross_cpu_tlb_shootdown_qemu_*` QEMU
//! verticals.

/// System-wide TLB maintenance: invalidate the calling CPU's cached
/// translation for a single virtual page **and** that of every other
/// online CPU, returning only once the invalidation is architecturally
/// visible everywhere.
///
/// The kernel calls [`Self::shootdown_page`] after editing a leaf whose
/// stale translation could be cached on another CPU — for example tearing
/// down or down-grading a mapping that more than one CPU has had active.
/// For a same-CPU-only edit the cheaper local
/// [`crate::tlb::TlbShootdown::flush_page`] is sufficient.
///
/// The trait is implemented on the port's [`crate::SchedulerArch`] handle
/// (the owner of the CPU topology and the directed-IPI path) and is
/// object-safe so the architecture-neutral kernel can hold it behind a
/// `&dyn CrossCpuTlbShootdown`.
///
/// Like [`crate::tlb::TlbShootdown`] a shootdown can only ever *discard*
/// cached state, so it is infallible by construction: there is nothing to
/// fail closed on (is satisfied vacuously — the operation
/// can neither grant authority nor leave a partial mapping). Over-
/// invalidating (reaching a CPU that never cached the page, or flushing on
/// the calling CPU when only a remote one was stale) is always sound;
/// under-invalidating is the only bug, and that is a correctness defect in
/// the port, caught by the QEMU verticals.
///
/// # Required semantics
///
/// * The call must invalidate the 4 KiB page containing `vaddr` on the
///   calling CPU and on every other CPU that is currently online.
/// * The call must not return until that invalidation cannot be undone by
///   a later speculative re-fill of the *old* translation on any CPU —
///   i.e. it carries whatever ordering barrier the architecture requires
///   (the x86 acknowledge spin, the aarch64 `dsb ish`/`isb`, the SBI
///   `remote_sfence_vma` completion).
/// * The call must complete **whatever the callers' interrupt state**, on
///   the initiator and on every target. A port whose targets acknowledge in
///   software owes that itself: a target with its interrupts masked cannot
///   take an interrupt, so the port must give it another way to acknowledge
///   (x86_64 serves the request from any `lib/sync` spin round as well as
///   from its ISR). Pushing the obligation onto callers instead — "only one
///   masked initiator at a time" — is not sound, because a masked CPU that
///   merely *waits* for a lock the initiator holds is enough to wedge both.
/// * Implementations must not panic for any `vaddr`.
pub trait CrossCpuTlbShootdown {
    /// Invalidate every online CPU's cached translation for the 4 KiB
    /// page containing `vaddr`, returning once the invalidation is
    /// globally visible.
    fn shootdown_page(&self, vaddr: u64);

    /// Invalidate every online CPU's cached translations for `page_count`
    /// consecutive 4 KiB pages starting at the page containing
    /// `start_vaddr`, returning once the invalidation is globally visible.
    ///
    /// A zero page count is a no-op. The default is the universally-correct
    /// per-page sequence, mirroring [`crate::tlb::TlbShootdown::flush_range`];
    /// a port whose invalidation carries a per-call cost the range can
    /// amortise overrides it, so tearing a large kernel remapping down pays
    /// one synchronisation boundary rather than one per leaf (a per-page IPI
    /// round-trip over thousands of pages is a real cost, not a theoretical
    /// one).
    fn shootdown_range(&self, start_vaddr: u64, page_count: usize) {
        const PAGE_BYTES: u64 = 4096;

        let mut vaddr = start_vaddr & !(PAGE_BYTES - 1);
        for _ in 0..page_count {
            self.shootdown_page(vaddr);
            vaddr = vaddr.wrapping_add(PAGE_BYTES);
        }
    }

    /// Whether a freshly *installed* leaf must be published to the other
    /// CPUs as well as ordered locally
    /// ([`crate::tlb::TlbShootdown::publish_mappings`]).
    ///
    /// The default is `false`, which is what "a leaf that was absent
    /// cannot be stale in any TLB" buys: a CPU that faults on the new
    /// leaf walks the tables and finds it, so an installation owes
    /// ordering, not invalidation, and costs no cross-CPU work at all.
    ///
    /// A port whose ISA permits caching an **invalid** entry cannot rely
    /// on that — a CPU holding the cached absence keeps faulting until it
    /// is fenced — so it declares `true` and the consumer follows the
    /// local publish with a [`Self::shootdown_range`] over the same run.
    /// Sv39 is such an ISA (RISC-V privileged spec: an implementation may
    /// cache invalid PTEs, and `sfence.vma` is what discards them); the
    /// declaration exists so the ports that do *not* need it keep paying
    /// nothing, rather than every installation broadcasting to be safe.
    ///
    /// This is a per-port constant, not a per-space one: a port cannot
    /// know from an address space alone which CPUs share it, and only a
    /// space active on more than one CPU (the kernel remap window and the
    /// boot root) is ever published from a CPU other than its user's.
    fn publish_needs_remote(&self) -> bool {
        false
    }
}

/// The cross-CPU TLB-shootdown conformance vertical.
///
/// Like [`crate::tlb::conformance`] it names only the trait and runs on
/// the host: there is no privileged IPI/broadcast/SBI machinery on the
/// host build, and a shootdown's cross-CPU *effect* is not observable from
/// a single-threaded test. It proves the observable half of the contract —
/// the call is object-safe and never panics for any address, including a
/// misaligned or zero one (a per-page shootdown always rounds to the
/// containing page, so a non-aligned address is accepted, not rejected).
/// The real multi-core round-trip is proven by the
/// `cross_cpu_tlb_shootdown_qemu_*` verticals.
pub mod conformance {
    use super::CrossCpuTlbShootdown;

    /// Run the [`CrossCpuTlbShootdown`] conformance suite against `xtlb`,
    /// using `vaddr` as a representative mapped page address.
    ///
    /// Shoots down `vaddr`, a misaligned address in the same page, the
    /// zero page, and the top page — proving the port accepts any address
    /// and never panics — then the range form over an empty and a
    /// multi-page span.
    pub fn run_all<T: CrossCpuTlbShootdown + ?Sized>(xtlb: &T, vaddr: u64) {
        xtlb.shootdown_page(vaddr);
        xtlb.shootdown_page(vaddr | 0xFFF);
        xtlb.shootdown_page(0);
        xtlb.shootdown_page(0xFFFF_FFFF_FFFF_F000);
        xtlb.shootdown_range(vaddr, 0);
        xtlb.shootdown_range(vaddr | 0xFFF, 3);
        // A declaration, not an action: what the suite can prove is that
        // the port answers it without panicking and answers it the same
        // way twice (it is a property of the ISA, never of a call site).
        assert_eq!(
            xtlb.publish_needs_remote(),
            xtlb.publish_needs_remote(),
            "publish_needs_remote must be a stable per-port declaration"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::CrossCpuTlbShootdown;
        use super::run_all;
        use core::sync::atomic::{AtomicUsize, Ordering};

        /// A faithful host double: it records how many pages were shot
        /// down so the suite has something observable to assert. The
        /// counter is interior-mutable because [`CrossCpuTlbShootdown`]
        /// takes `&self` — the real handle is shared (`&dyn`) between
        /// CPUs exactly like [`crate::SchedulerArch`].
        #[derive(Default)]
        struct CountingXtlb {
            shootdowns: AtomicUsize,
        }

        impl CrossCpuTlbShootdown for CountingXtlb {
            fn shootdown_page(&self, _vaddr: u64) {
                self.shootdowns.fetch_add(1, Ordering::Relaxed);
            }
        }

        #[test]
        fn suite_drives_every_shootdown_over_a_faithful_xtlb() {
            let xtlb = CountingXtlb::default();
            run_all(&xtlb, 0x10_0000_0000);
            assert_eq!(
                xtlb.shootdowns.load(Ordering::Relaxed),
                7,
                "four single pages plus the default range's three"
            );

            // And over the object-safe erasure the kernel holds it behind.
            let dynamic = CountingXtlb::default();
            let erased: &dyn CrossCpuTlbShootdown = &dynamic;
            run_all(erased, 0x10_0000_0000);
            assert_eq!(dynamic.shootdowns.load(Ordering::Relaxed), 7);
        }

        #[test]
        fn the_default_range_rounds_down_and_walks_consecutive_pages() {
            /// Records the exact addresses the default range issues, so a
            /// port that overrides it can be checked against the same
            /// contract.
            #[derive(Default)]
            struct RecordingXtlb {
                seen: core::cell::RefCell<[u64; 4]>,
                count: core::cell::Cell<usize>,
            }

            impl CrossCpuTlbShootdown for RecordingXtlb {
                fn shootdown_page(&self, vaddr: u64) {
                    let n = self.count.get();
                    self.seen.borrow_mut()[n] = vaddr;
                    self.count.set(n + 1);
                }
            }

            let xtlb = RecordingXtlb::default();
            xtlb.shootdown_range(0x4000_0FFF, 3);
            assert_eq!(xtlb.count.get(), 3);
            assert_eq!(
                xtlb.seen.borrow()[..3],
                [0x4000_0000, 0x4000_1000, 0x4000_2000]
            );
        }
    }
}
