//! TLB-shootdown surface of the Arch HAL (
//! "TLB shootdown").
//!
//! After a page table is edited — a leaf installed, torn down, or its
//! permissions changed — the CPU may still hold a *stale* cached
//! translation for that virtual page in its translation-lookaside
//! buffer. Invalidating that entry is privilege-neutral but
//! deeply architecture-specific: x86_64 issues `invlpg`, aarch64 a
//! `tlbi vae1is` + barrier, riscv64 an `sfence.vma vaddr`. The charter makes
//! the architecture surface a closed set of traits on the HAL; this
//! module is the "TLB shootdown" member of that set, so the per-page
//! invalidation lives behind one vocabulary instead of being re-spelled
//! at every call site. The parallel per-arch
//! implementations of this one trait are the deliberate shape of
//! modularity, never collapsed behind `cfg` (carve-out).
//!
//! # Scope (the burn-down)
//!
//! This is the `plans/WIRING.md` **Stage W5b-2** slice: the *local*,
//! single-CPU per-page invalidation the per-process map/unmap path in
//! `kernel/mem` consumes through [`TlbShootdown::flush_page`]. The
//! *cross-CPU* shootdown — interrupting the other CPUs that may cache a
//! translation and waiting for them to acknowledge the invalidation —
//! depends on the aarch64 directed IPI landing in Stage W6, so it is a
//! tracked follow-up, not silently stubbed here.
//!
//! # Why there is no host `activate`-style asymmetry here
//!
//! Unlike [`crate::mmu::AddressSpace::activate`], a TLB flush has no
//! observable architectural *effect* a host test could assert (it only
//! affects a later translation), but it also touches no privileged state
//! a host cannot model: the operation is "ask the CPU to forget one
//! cached translation". The host [`conformance`] vertical therefore
//! proves the contract that *is* observable — the call is object-safe,
//! accepts any address, and never panics or fails (a fail-closed
//! invalidation can only ever *over*-invalidate, never refuse) — while
//! the real instruction is exercised end-to-end by each port's
//! `memory_isolation` / spawn QEMU verticals, where a freshly mapped
//! leaf must be reachable immediately after the flush.

/// Per-CPU TLB maintenance: invalidate the calling CPU's cached
/// translation for a single virtual page.
///
/// The kernel calls [`Self::flush_page`] after editing a leaf so the
/// next access to that page re-walks the (updated) table rather than
/// hitting a stale cached translation. The trait is object-safe so the
/// per-process address-space façade can hold it behind a generic bound
/// alongside [`crate::mmu::AddressSpace`].
///
/// A flush can only ever *discard* cached state, so it is infallible by
/// construction: there is nothing to fail closed on (is
/// satisfied vacuously — the operation cannot grant authority or leave a
/// partial mapping). Over-invalidating (flushing more than the one page)
/// is always sound; under-invalidating is the only bug, and that is a
/// correctness defect in the port, caught by the QEMU verticals.
pub trait TlbShootdown {
    /// Invalidate the calling CPU's cached translation for the 4 KiB page
    /// containing `vaddr`.
    ///
    /// On the bare-metal target this is the port's single-page TLB
    /// invalidation instruction (`invlpg` / `tlbi vae1is` / `sfence.vma`).
    /// Implementations must not panic for any `vaddr`.
    fn flush_page(&mut self, vaddr: u64);
}

/// The TLB-shootdown conformance vertical.
///
/// Like [`crate::mmu::conformance`] it names only the trait and runs on
/// the host. There is no privileged register write to gate (a flush is
/// "forget a cached translation"), so — unlike
/// [`crate::mmu::AddressSpace::activate`] — every port can run this on
/// the host. It proves the observable half of the contract: the call is
/// object-safe and never panics for any address, including a misaligned
/// or zero one (a per-page flush always rounds to the containing page,
/// so a non-aligned address is accepted, not rejected).
pub mod conformance {
    use super::TlbShootdown;

    /// Run the [`TlbShootdown`] conformance suite against `tlb`, using
    /// `vaddr` as a representative mapped page address.
    ///
    /// Flushes `vaddr`, a misaligned address in the same page, the zero
    /// page, and the top page — proving the port accepts any address and
    /// never panics.
    pub fn run_all<T: TlbShootdown + ?Sized>(tlb: &mut T, vaddr: u64) {
        tlb.flush_page(vaddr);
        tlb.flush_page(vaddr | 0xFFF);
        tlb.flush_page(0);
        tlb.flush_page(0xFFFF_FFFF_FFFF_F000);
    }

    #[cfg(test)]
    mod tests {
        use super::super::TlbShootdown;
        use super::run_all;

        /// A faithful host double: it records how many pages were flushed
        /// so the suite has something observable to assert, exactly as
        /// the `kernel/mem` `HostPageTable` does for its TLB-flush
        /// discipline test.
        #[derive(Default)]
        struct CountingTlb {
            flushes: usize,
        }

        impl TlbShootdown for CountingTlb {
            fn flush_page(&mut self, _vaddr: u64) {
                self.flushes += 1;
            }
        }

        #[test]
        fn suite_drives_every_flush_over_a_faithful_tlb() {
            let mut tlb = CountingTlb::default();
            run_all(&mut tlb, 0x10_0000_0000);
            assert_eq!(tlb.flushes, 4, "the suite issues four flushes");

            // And over the object-safe erasure the per-process façade and
            // the kernel registry both rely on.
            let mut dynamic = CountingTlb::default();
            let erased: &mut dyn TlbShootdown = &mut dynamic;
            run_all(erased, 0x10_0000_0000);
            assert_eq!(dynamic.flushes, 4);
        }
    }
}
