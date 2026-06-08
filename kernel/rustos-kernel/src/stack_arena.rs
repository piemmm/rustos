//! Guarded kthread kernel-stack arena — `plans/PI.md` G3b-2.
//!
//! The boot path carves a 2 MiB-aligned, [`RegionKind::Reserved`] guard
//! arena out of the usable RAM window ([`crate::mem_map`], stage G2) so the
//! frame allocator never hands its frames to another use. This module hands
//! kthread kernel stacks *out of that arena* instead of off the kernel heap,
//! so a stack's one-page guard can be turned into a genuine hardware fault:
//! the spawn seam re-expresses the coarse identity block covering the guard
//! page at 4 KiB granularity in the task's own page-table root
//! ([`rustos_arch_aarch64::paging::AddressSpace::split_block`]) and unmaps
//! that single page, so an overrun of the kernel stack takes a synchronous
//! data abort under the task's translation regime rather than silently
//! corrupting the lower-addressed neighbour (`AGENTS.md` §4 / §2.17). A
//! heap-backed [`rustos_kernel_core::BoxStack`] (its software poison-canary)
//! remains the fail-closed fallback where no arena is installed.
//!
//! The allocator is a forward-only bump cursor: each region is handed out
//! exactly once and never reclaimed, the same monotonic discipline the
//! spawn page-table pools use (`AGENTS.md` §2.1 — no global mutable heap,
//! no free list to corrupt). The arena holds many stacks; PID 1 `init` and
//! the session it launches are the only consumers this stage.
//!
//! Like [`crate::mem_map`], the bump arithmetic is free of the bare-metal
//! aarch64 port, so it compiles — and its unit tests run — on the CI host
//! as well as on the aarch64 production build that consumes it, and on no
//! other configuration, so it is never dead code (`AGENTS.md` §2.3).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustos_kernel_core::{KernelStack, KTHREAD_STACK_BYTES};

/// Width of a stack's guard region, in bytes: one 4 KiB page.
///
/// Matches [`rustos_kernel_core::BoxStack`]'s guard so the arena form and
/// the software-canary fallback have identical geometry — the guard sits
/// immediately *below* the usable stack, so a downward overrun crosses it
/// first.
const STACK_GUARD_BYTES: u64 = 4096;

/// The widest ABI stack alignment any target requires (`AGENTS.md` §17.2);
/// [`rustos_arch_api::ContextSwitch::prepare`] rejects a misaligned seed
/// `stack_top`.
const STACK_ALIGN: u64 = 16;

/// Bytes one guarded stack occupies in the arena: the guard page plus the
/// usable stack, laid out low-to-high as `[guard | usable]`.
///
/// It is a whole number of 4 KiB pages (the guard is one page and
/// [`KTHREAD_STACK_BYTES`] is page-aligned), so consecutive regions keep
/// every guard page 4 KiB-aligned — the alignment
/// [`rustos_arch_aarch64::paging::AddressSpace::split_block`] requires to
/// re-express the covering block and clear the guard's own leaf.
const STACK_REGION_BYTES: u64 = STACK_GUARD_BYTES + KTHREAD_STACK_BYTES as u64;

/// The region is a whole number of 4 KiB pages so each region's guard page
/// lands on a clean page boundary the block split can clear.
const _STACK_REGION_PAGE_ALIGNED: () = {
    assert!(STACK_REGION_BYTES % 4096 == 0);
};

/// A kthread kernel stack carved from the reserved guard arena.
///
/// The region is `[guard, guard + STACK_REGION_BYTES)` of identity-mapped,
/// allocator-reserved RAM, laid out as a one-page guard region below the
/// usable stack. [`Self::guard_page`] is the page the spawn seam unmaps in
/// the owning task's root; [`KernelStack::top`] is the exclusive upper
/// bound of the usable region above it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArenaStack {
    /// First byte of the region — the low edge of the one-page guard.
    guard: u64,
}

impl ArenaStack {
    /// The guard page's base address (4 KiB-aligned): the page the spawn
    /// seam re-expresses and unmaps in the owning task's page-table root so
    /// an overrun faults (`plans/PI.md` G3b-2).
    pub(crate) const fn guard_page(self) -> u64 {
        self.guard
    }
}

// SAFETY: `top` returns the region's base plus its full length, rounded
// down to `STACK_ALIGN` — the aligned exclusive upper bound of the usable
// stack above the guard. The arena is `RegionKind::Reserved`, so the frame
// allocator never hands its frames elsewhere, and the bump cursor hands
// each region out exactly once, so the region is exclusive to its owner.
// The arena is reserved for the kernel image's lifetime, so the region
// stays valid for as long as the owning task lives. The guard page aside
// (which the spawn seam deliberately unmaps in the task's own root), every
// byte of the usable region stays identity-mapped and writable.
unsafe impl KernelStack for ArenaStack {
    fn top(&self) -> u64 {
        let top = self.guard + STACK_REGION_BYTES;
        // Round down to `STACK_ALIGN`; the region is page-aligned so this
        // wastes nothing, but keeps the contract explicit.
        top & !(STACK_ALIGN - 1)
    }
    // `check_guard` keeps the default `Ok(())`: the guard page is *unmapped*
    // in the owning task's root, so an overrun faults in hardware. There is
    // no poison canary to verify (and reading the page under a different
    // root — e.g. the dispatcher's — would false-positive, since arena RAM
    // is not poison-filled). The hardware fault is the defence here, not a
    // canary scan (contrast `BoxStack`, the fallback).
}

/// A forward-only bump allocator over the reserved guard arena.
///
/// Installed once at boot with the arena the memory-map builder carved
/// ([`crate::mem_map`]); thereafter [`Self::alloc`] hands out one
/// [`ArenaStack`] per call until the arena is exhausted, when it fails
/// closed with `None` (`AGENTS.md` §2.9). Regions are never reclaimed —
/// the monotonic, free-list-free discipline the spawn page-table pools use
/// (`AGENTS.md` §2.1).
pub(crate) struct StackArena {
    /// Set once the arena has been installed; gates [`Self::alloc`] and
    /// makes [`Self::install`] idempotent (a second install is refused).
    installed: AtomicBool,
    /// One past the last arena byte; a region whose end exceeds this does
    /// not fit and `alloc` returns `None`.
    end: AtomicU64,
    /// Base of the next free region; advances by [`STACK_REGION_BYTES`] on
    /// each successful [`Self::alloc`].
    next: AtomicU64,
}

impl StackArena {
    /// Construct an empty, not-yet-installed arena (for the `'static`
    /// boot-installed instance and for unit tests).
    pub(crate) const fn new() -> Self {
        Self {
            installed: AtomicBool::new(false),
            end: AtomicU64::new(0),
            next: AtomicU64::new(0),
        }
    }

    /// Install the reserved arena `[base, base + len)`.
    ///
    /// Called **once**, on the boot CPU, before any task that could
    /// [`Self::alloc`] is spawned, so no allocation races it. `base` is the
    /// 2 MiB-aligned arena base the memory-map builder carved. Returns
    /// `false` if the arena was already installed (a re-entry is refused
    /// rather than silently re-basing a live cursor, `AGENTS.md` §2.9); the
    /// field stores precede the `installed` publish, so any later `alloc`
    /// that observes `installed` also observes a consistent `end`/`next`.
    pub(crate) fn install(&self, base: u64, len: u64) -> bool {
        if self.installed.load(Ordering::Acquire) {
            return false;
        }
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        self.end.store(end, Ordering::Relaxed);
        self.next.store(base, Ordering::Relaxed);
        self.installed.store(true, Ordering::Release);
        true
    }

    /// Hand out the next guarded stack region, or `None` when the arena is
    /// not installed or has no room left for a whole region (fail closed —
    /// the caller falls back to a software-canary [`BoxStack`], never runs
    /// on an unguarded stack, `AGENTS.md` §2.17).
    ///
    /// [`rustos_kernel_core::BoxStack`]: rustos_kernel_core::BoxStack
    pub(crate) fn alloc(&self) -> Option<ArenaStack> {
        if !self.installed.load(Ordering::Acquire) {
            return None;
        }
        let end = self.end.load(Ordering::Relaxed);
        loop {
            let cur = self.next.load(Ordering::Relaxed);
            let region_end = cur.checked_add(STACK_REGION_BYTES)?;
            if region_end > end {
                return None;
            }
            if self
                .next
                .compare_exchange(cur, region_end, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(ArenaStack { guard: cur });
            }
        }
    }
}

/// The single, `'static` guard-stack arena the aarch64 boot path installs
/// (`boot_aarch64`) and the PID 1 spawn seam draws from (`init_spawn`).
///
/// It lives for the kernel image's lifetime; the regions it hands out are
/// inside the boot-reserved arena, so they outlive every task built on
/// them (`AGENTS.md` §2.1 — monotonic, never freed).
///
/// Only the bare-metal aarch64 build instantiates the `'static` arena (the
/// boot path installs it, the spawn seam draws from it); the host-test
/// build exercises the allocator through locally constructed
/// [`StackArena`]s, so the shared instance is gated out there to stay free
/// of an unused-static warning (`AGENTS.md` §2.3).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub(crate) static KTHREAD_STACK_ARENA: StackArena = StackArena::new();

#[cfg(test)]
mod tests {
    use super::*;

    /// A page-aligned, plausibly RAM-resident fake arena base for the host
    /// tests (the real one is the 2 MiB-aligned carved arena).
    const FAKE_BASE: u64 = 0x4000_0000;

    #[test]
    fn uninstalled_arena_allocates_nothing() {
        let arena = StackArena::new();
        assert_eq!(arena.alloc(), None);
    }

    #[test]
    fn install_is_once_and_refuses_re_entry() {
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, 2 * 1024 * 1024));
        // A second install must be refused so a live cursor is never
        // silently re-based.
        assert!(!arena.install(FAKE_BASE + 0x1000, 4 * 1024 * 1024));
    }

    #[test]
    fn install_rejects_a_base_plus_len_overflow() {
        let arena = StackArena::new();
        assert!(!arena.install(u64::MAX - 1, 16));
        // A refused install leaves the arena uninstalled.
        assert_eq!(arena.alloc(), None);
    }

    #[test]
    fn consecutive_regions_step_by_one_region_with_aligned_guard_pages() {
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, 4 * STACK_REGION_BYTES));

        let first = arena.alloc().expect("first region fits");
        let second = arena.alloc().expect("second region fits");

        assert_eq!(first.guard_page(), FAKE_BASE);
        assert_eq!(second.guard_page(), FAKE_BASE + STACK_REGION_BYTES);
        // Each guard page is 4 KiB-aligned, as `split_block` requires.
        assert_eq!(first.guard_page() % 4096, 0);
        assert_eq!(second.guard_page() % 4096, 0);
    }

    #[test]
    fn top_is_above_the_guard_and_aligned() {
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES));
        let stack = arena.alloc().expect("region fits");

        // The usable region sits above the one-page guard; `top` is its
        // exclusive upper bound.
        assert_eq!(
            stack.top(),
            FAKE_BASE + STACK_GUARD_BYTES + KTHREAD_STACK_BYTES as u64
        );
        assert!(stack.top() >= stack.guard_page() + STACK_GUARD_BYTES);
        assert_eq!(stack.top() % STACK_ALIGN, 0);
    }

    #[test]
    fn exhaustion_fails_closed() {
        // Room for exactly two regions.
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, 2 * STACK_REGION_BYTES));
        assert!(arena.alloc().is_some());
        assert!(arena.alloc().is_some());
        // The third does not fit: fail closed, never overrun the arena.
        assert_eq!(arena.alloc(), None);
    }

    #[test]
    fn a_partial_final_region_does_not_fit() {
        // One region plus a page: the second region cannot fit whole.
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES + 4096));
        assert!(arena.alloc().is_some());
        assert_eq!(arena.alloc(), None);
    }
}
