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

use rustos_kernel_core::{KernelStack, KTHREAD_STACK_BYTES};
use rustos_kernel_mem::FrameAllocator;
use rustos_sync::SpinLock;

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

/// Size **and** alignment of a freshly chained arena block: one 2 MiB
/// region, matching the boot-carved arena's [`crate::mem_map`] block
/// granularity.
///
/// A 2 MiB-aligned block means every guard page inside it still lands on
/// its own L3 leaf when the spawn seam re-expresses the covering block at
/// 4 KiB granularity in the owning task's root
/// ([`rustos_arch_aarch64::paging::AddressSpace::split_block`]), so a
/// chained block hosts hardware-guarded stacks exactly as the boot-carved
/// arena does (`AGENTS.md` §24.1 — the capacity grows on demand without
/// weakening the §4 guard-page invariant).
const ARENA_GROW_BLOCK_BYTES: u64 = 2 * 1024 * 1024;

/// Buddy-allocator order whose contiguous block is exactly
/// [`ARENA_GROW_BLOCK_BYTES`] (`2^9` × 4 KiB = 2 MiB), so
/// [`FrameAllocator::alloc_order`] returns a 2 MiB-aligned block.
const ARENA_GROW_BLOCK_ORDER: u32 = 9;

/// The grow order must name exactly the 2 MiB block size above, or a
/// chained block would be mis-sized or mis-aligned.
const _ARENA_GROW_BLOCK_ORDER_MATCHES: () = {
    assert!((1u64 << ARENA_GROW_BLOCK_ORDER) * 4096 == ARENA_GROW_BLOCK_BYTES);
};

/// A chained block must hold at least one whole guarded region. It does
/// (2 MiB ≫ a few-page region), which is what makes [`StackArena::alloc`]'s
/// grow loop provably bounded: a single chained block always satisfies the
/// pending request, so the loop chains at most once per call.
const _ARENA_GROW_BLOCK_FITS_A_REGION: () = {
    assert!(ARENA_GROW_BLOCK_BYTES >= STACK_REGION_BYTES);
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

/// A source of fresh, 2 MiB-aligned, identity-mapped arena blocks the
/// [`StackArena`] chains onto when its current block is exhausted
/// (`AGENTS.md` §24.1 — a capacity that grows on demand, never a frozen
/// ceiling).
///
/// A block must be exactly [`ARENA_GROW_BLOCK_BYTES`], 2 MiB-aligned, and
/// identity-mapped (`virtual == physical`) in *every* address space a
/// kthread can run under — the spawn seam re-expresses the block in the
/// task's own root and an overrun must fault there, not read a different
/// task's memory. Returning `None` (genuine physical exhaustion, or a
/// block outside the identity window) makes [`StackArena::alloc`] fail
/// closed to the software-canary [`BoxStack`] fallback rather than ever
/// hand out an unreachable or unguarded stack (`AGENTS.md` §2.9 / §2.17).
pub(crate) trait ArenaGrow {
    /// Hand out a fresh `[base, base + ARENA_GROW_BLOCK_BYTES)` block, or
    /// `None` on genuine exhaustion. `base` is the block's identity-mapped
    /// address (`virtual == physical`).
    fn grow_block(&self) -> Option<u64>;
}

/// The live cursor over the arena's *current* block, behind the arena's
/// lock. Chaining a fresh block re-bases `next`/`end` onto it; the prior
/// block's regions were already handed out and are never reclaimed, so
/// only the current block's cursor need be tracked.
struct Cursor {
    /// Set once the arena has been installed; gates [`StackArena::alloc`]
    /// and makes [`StackArena::install`] idempotent.
    installed: bool,
    /// Base of the next free region in the current block.
    next: u64,
    /// One past the current block's last byte; a region whose end exceeds
    /// this does not fit and triggers a grow (or fails closed).
    end: u64,
}

/// A forward-only bump allocator over the kthread-stack guard arena that
/// **grows on demand** (`AGENTS.md` §24.1).
///
/// Installed once at boot with the 2 MiB-aligned block the memory-map
/// builder carved ([`crate::mem_map`]); thereafter [`Self::alloc`] hands
/// out one [`ArenaStack`] per call. When the current block has no room for
/// a whole region it **chains** a fresh 2 MiB block from the supplied
/// [`ArenaGrow`] source (the live frame allocator in production) and
/// continues, so the kthread-stack capacity scales with discovered RAM
/// instead of capping at the boot-carved block. Only genuine physical
/// exhaustion fails closed with `None`, and the caller then falls back to
/// a software-canary [`BoxStack`] (`AGENTS.md` §2.9 / §2.17) — never an
/// unguarded stack.
///
/// Regions are never reclaimed — the monotonic, free-list-free discipline
/// the spawn page-table pools use (`AGENTS.md` §2.1); a chained block is
/// likewise leaked for the kernel image's lifetime. The whole allocation
/// is serialised by a [`SpinLock`]: `alloc` is a per-spawn operation, not
/// a hot path, so a lock is the simplest correct way to chain a fresh
/// block atomically (`AGENTS.md` §2.16 — locking only off the hot path).
pub(crate) struct StackArena {
    cursor: SpinLock<Cursor>,
}

impl StackArena {
    /// Construct an empty, not-yet-installed arena (for the `'static`
    /// boot-installed instance and for unit tests).
    pub(crate) const fn new() -> Self {
        Self {
            cursor: SpinLock::new(Cursor {
                installed: false,
                next: 0,
                end: 0,
            }),
        }
    }

    /// Install the reserved first arena block `[base, base + len)`.
    ///
    /// Called **once**, on the boot CPU, before any task that could
    /// [`Self::alloc`] is spawned. `base` is the 2 MiB-aligned arena base
    /// the memory-map builder carved. Returns `false` if the arena was
    /// already installed (a re-entry is refused rather than silently
    /// re-basing a live cursor, `AGENTS.md` §2.9) or if `base + len`
    /// overflows.
    pub(crate) fn install(&self, base: u64, len: u64) -> bool {
        let mut cursor = self.cursor.lock();
        if cursor.installed {
            return false;
        }
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        cursor.next = base;
        cursor.end = end;
        cursor.installed = true;
        true
    }

    /// Hand out the next guarded stack region, chaining a fresh block from
    /// `grow` when the current block is exhausted.
    ///
    /// Returns `None` only when the arena is not installed or the `grow`
    /// source is itself exhausted (fail closed — the caller falls back to
    /// a software-canary [`BoxStack`], never runs on an unguarded stack,
    /// `AGENTS.md` §2.17).
    ///
    /// The loop chains **at most once** per pending region: a chained
    /// block is [`ARENA_GROW_BLOCK_BYTES`] and a region is far smaller
    /// (`_ARENA_GROW_BLOCK_FITS_A_REGION`), so the freshly chained block
    /// always satisfies the request, making the loop provably bounded
    /// (`AGENTS.md` §2.1 — no retry-until-it-works).
    ///
    /// [`rustos_kernel_core::BoxStack`]: rustos_kernel_core::BoxStack
    pub(crate) fn alloc(&self, grow: &dyn ArenaGrow) -> Option<ArenaStack> {
        let mut cursor = self.cursor.lock();
        if !cursor.installed {
            return None;
        }
        loop {
            // Fits in the current block?
            if let Some(region_end) = cursor.next.checked_add(STACK_REGION_BYTES) {
                if region_end <= cursor.end {
                    let guard = cursor.next;
                    cursor.next = region_end;
                    return Some(ArenaStack { guard });
                }
            }
            // Exhausted: chain a fresh block, or fail closed.
            let base = grow.grow_block()?;
            let block_end = base.checked_add(ARENA_GROW_BLOCK_BYTES)?;
            cursor.next = base;
            cursor.end = block_end;
            // The fresh block is whole-2 MiB and a region is far smaller,
            // so the next iteration's fit check necessarily succeeds.
        }
    }
}

/// An [`ArenaGrow`] that chains fresh blocks out of the kernel's live
/// [`FrameAllocator`], bounded to the per-space identity window so a
/// chained kthread stack stays identity-mapped in every address space a
/// task runs under (`AGENTS.md` §4 / §24.1).
///
/// Each grow draws a 2 MiB-aligned [`ARENA_GROW_BLOCK_ORDER`] block from
/// the buddy allocator. A block whose end exceeds [`Self::identity_limit`]
/// would be unmapped in some space the task executes under — its guard
/// page could not fault there, and a stack body byte would be unreachable
/// — so it is returned to the allocator and the grow fails closed
/// (`AGENTS.md` §2.9), dropping the caller to the software-canary
/// [`BoxStack`] fallback rather than handing out an unreachable stack.
pub(crate) struct FrameArenaGrow<'a> {
    frames: &'a FrameAllocator,
    /// Exclusive upper bound: a chained block must lie wholly below this so
    /// it is covered by every address space's identity map.
    identity_limit: u64,
}

impl<'a> FrameArenaGrow<'a> {
    /// Wrap the live frame allocator, bounding chained blocks to
    /// `[0, identity_limit)` (the `IDENTITY_GIB`-gigapage window each
    /// spawned space identity-maps).
    pub(crate) fn new(frames: &'a FrameAllocator, identity_limit: u64) -> Self {
        Self {
            frames,
            identity_limit,
        }
    }
}

impl ArenaGrow for FrameArenaGrow<'_> {
    fn grow_block(&self) -> Option<u64> {
        let frame = self.frames.alloc_order(ARENA_GROW_BLOCK_ORDER).ok()?;
        let base = frame.start().as_u64();
        match base.checked_add(ARENA_GROW_BLOCK_BYTES) {
            Some(block_end) if block_end <= self.identity_limit => Some(base),
            _ => {
                // Outside the identity window (or an address overflow):
                // return the block and fail closed rather than host a
                // stack the task's translation regime cannot reach.
                let _ = self.frames.free_order(frame, ARENA_GROW_BLOCK_ORDER);
                None
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

    use core::cell::Cell;

    use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind};

    /// A page-aligned, plausibly RAM-resident fake arena base for the host
    /// tests (the real one is the 2 MiB-aligned carved arena).
    const FAKE_BASE: u64 = 0x4000_0000;

    /// An [`ArenaGrow`] that never grows: the arena is bounded to its
    /// installed block. Mirrors a build with no allocator-backed grow
    /// source, and proves the within-block / exhaustion paths in isolation.
    struct NoGrow;
    impl ArenaGrow for NoGrow {
        fn grow_block(&self) -> Option<u64> {
            None
        }
    }

    /// An [`ArenaGrow`] that hands out a fixed sequence of block bases, then
    /// fails closed — a deterministic stand-in for the frame-allocator
    /// source so the chaining arithmetic is tested without real memory.
    struct FakeGrow<'a> {
        bases: &'a [u64],
        next: Cell<usize>,
    }
    impl<'a> FakeGrow<'a> {
        fn new(bases: &'a [u64]) -> Self {
            Self {
                bases,
                next: Cell::new(0),
            }
        }
    }
    impl ArenaGrow for FakeGrow<'_> {
        fn grow_block(&self) -> Option<u64> {
            let i = self.next.get();
            let base = *self.bases.get(i)?;
            self.next.set(i + 1);
            Some(base)
        }
    }

    /// A usable-only [`BootMemoryMap`] of `[base, base + len)`.
    fn usable_map(base: u64, len: u64) -> BootMemoryMap {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(base),
            length: len,
        });
        map
    }

    #[test]
    fn uninstalled_arena_allocates_nothing() {
        let arena = StackArena::new();
        assert_eq!(arena.alloc(&NoGrow), None);
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
        assert_eq!(arena.alloc(&NoGrow), None);
    }

    #[test]
    fn consecutive_regions_step_by_one_region_with_aligned_guard_pages() {
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, 4 * STACK_REGION_BYTES));

        let first = arena.alloc(&NoGrow).expect("first region fits");
        let second = arena.alloc(&NoGrow).expect("second region fits");

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
        let stack = arena.alloc(&NoGrow).expect("region fits");

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
    fn exhaustion_without_a_grow_source_fails_closed() {
        // Room for exactly two regions and a `NoGrow` source.
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, 2 * STACK_REGION_BYTES));
        assert!(arena.alloc(&NoGrow).is_some());
        assert!(arena.alloc(&NoGrow).is_some());
        // The third does not fit and cannot grow: fail closed, never
        // overrun the arena.
        assert_eq!(arena.alloc(&NoGrow), None);
    }

    #[test]
    fn a_partial_final_region_does_not_fit() {
        // One region plus a page: the second region cannot fit whole, and a
        // `NoGrow` source cannot supply another block.
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES + 4096));
        assert!(arena.alloc(&NoGrow).is_some());
        assert_eq!(arena.alloc(&NoGrow), None);
    }

    #[test]
    fn grows_onto_a_fresh_chained_block_when_the_first_is_exhausted() {
        // A first block with room for exactly one region, and a fake grow
        // source offering a single fresh 2 MiB block far away.
        const SECOND_BLOCK: u64 = FAKE_BASE + 0x1000_0000;
        let bases = [SECOND_BLOCK];
        let grow = FakeGrow::new(&bases);

        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES));

        // First region comes from the installed block.
        let first = arena.alloc(&grow).expect("first region fits");
        assert_eq!(first.guard_page(), FAKE_BASE);

        // The installed block is now exhausted; the next region is served
        // from the freshly chained block.
        let second = arena.alloc(&grow).expect("chained region fits");
        assert_eq!(second.guard_page(), SECOND_BLOCK);
        // And a third still comes from the chained block (it is whole-2 MiB,
        // so it holds many regions).
        let third = arena.alloc(&grow).expect("chained block holds more");
        assert_eq!(third.guard_page(), SECOND_BLOCK + STACK_REGION_BYTES);

        // Exactly one block was chained for the run above.
        assert_eq!(grow.next.get(), 1);
    }

    #[test]
    fn fails_closed_when_the_grow_source_is_exhausted() {
        // First block holds one region; the grow source is empty.
        let grow = FakeGrow::new(&[]);
        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES));
        assert!(arena.alloc(&grow).is_some());
        // No fresh block to chain: fail closed, never an unguarded stack.
        assert_eq!(arena.alloc(&grow), None);
    }

    #[test]
    fn frame_allocator_grow_chains_a_real_block_past_the_first() {
        // The installed block is placed above the allocator's window so its
        // addresses never collide with a chained block in this arithmetic
        // test.
        const FIRST_BLOCK: u64 = 0x8000_0000;

        // An 8 MiB usable window the buddy allocator can carve 2 MiB blocks
        // from, fully inside a generous identity window.
        let map = usable_map(0, 8 * 1024 * 1024);
        let frames = FrameAllocator::new(&map).expect("allocator builds");
        let grow = FrameArenaGrow::new(&frames, 8 * 1024 * 1024);

        // A first block with room for exactly one region forces a grow on
        // the second allocation.
        let arena = StackArena::new();
        assert!(arena.install(FIRST_BLOCK, STACK_REGION_BYTES));

        let first = arena.alloc(&grow).expect("first region fits");
        assert_eq!(first.guard_page(), FIRST_BLOCK);

        // The second allocation grows by drawing a real 2 MiB block from
        // the frame allocator; the region lands inside the allocator's
        // window, distinct from the first block.
        let second = arena.alloc(&grow).expect("grows onto a real block");
        assert!(second.guard_page() < 8 * 1024 * 1024);
        assert_eq!(second.guard_page() % ARENA_GROW_BLOCK_BYTES, 0);
        assert_ne!(second.guard_page(), first.guard_page());
    }

    #[test]
    fn frame_allocator_grow_fails_closed_on_physical_exhaustion() {
        // A window too small to carve even one 2 MiB block: the grow source
        // is exhausted, so the arena fails closed once the first block is.
        let map = usable_map(0, 1024 * 1024); // 1 MiB < a 2 MiB block
        let frames = FrameAllocator::new(&map).expect("allocator builds");
        let grow = FrameArenaGrow::new(&frames, 1 << 31);

        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES));
        assert!(arena.alloc(&grow).is_some());
        assert_eq!(
            arena.alloc(&grow),
            None,
            "no 2 MiB block to chain: fail closed (§2.9)"
        );
    }

    #[test]
    fn frame_allocator_grow_rejects_a_block_outside_the_identity_window() {
        // The allocator can carve a 2 MiB block, but the identity window is
        // below it, so a chained stack would be unmapped in the task's own
        // root: the grow must fail closed rather than hand out an
        // unreachable stack (§4 / §2.9).
        let map = usable_map(0, 8 * 1024 * 1024);
        let frames = FrameAllocator::new(&map).expect("allocator builds");
        // Identity limit of 1 MiB: every 2 MiB block ends above it.
        let grow = FrameArenaGrow::new(&frames, 1024 * 1024);

        let arena = StackArena::new();
        assert!(arena.install(FAKE_BASE, STACK_REGION_BYTES));
        assert!(arena.alloc(&grow).is_some());
        assert_eq!(arena.alloc(&grow), None);

        // The rejected block was returned to the allocator, not leaked: a
        // grow with a sufficient window still succeeds afterwards.
        let grow_ok = FrameArenaGrow::new(&frames, 8 * 1024 * 1024);
        let arena2 = StackArena::new();
        assert!(arena2.install(FAKE_BASE, STACK_REGION_BYTES));
        assert!(arena2.alloc(&grow_ok).is_some());
        assert!(
            arena2.alloc(&grow_ok).is_some(),
            "the earlier rejected block was freed, so RAM is still available"
        );
    }
}
