//! Guarded kthread kernel-stack arena — `plans/PI.md` G3b-2 + (resource limits and scalability).
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
//! corrupting the lower-addressed neighbour. A
//! heap-backed [`rustos_kernel_core::BoxStack`] (its software poison-canary)
//! remains the fail-closed fallback where no arena is installed.
//!
//! # Growing *and* shrinking
//!
//! The arena is a list of blocks: the boot-carved first block plus, on
//! demand, fresh 2 MiB blocks chained out of the live [`FrameAllocator`]
//! ([`FrameArenaGrow`]) when every existing block is full. Each block hands
//! out fixed-size guarded regions through a forward bump cursor, and tracks
//! the count of regions currently *live* (handed out and not yet freed).
//!
//! Reclamation is symmetric: when a task exits, the scheduler drops its
//! `Box<dyn KernelStack>`, which drops the [`ArenaStack`], whose [`Drop`]
//! returns the region to its owning block ([`StackArena::free`]). A block
//! whose live count reaches zero is *idle*; the capacity therefore falls as
//! well as rises rather than ratcheting up forever. To avoid thrashing a
//! block across a boundary, exactly one idle chained block is kept resident
//! (a one-free-block grace); a *second* idle chained block is zeroed
//! (a kthread stack can hold spilled capability tokens) and
//! returned to the allocator through [`FrameArenaShrink`]. The boot-carved
//! block (kernel-image-owned `Reserved` frames, not allocator frames) is
//! never returned. A block that cannot be safely scrubbed/returned is
//! retained rather than released (fail closed).
//!
//! # Block list is itself a capacity (no fixed ceiling)
//!
//! Each block's `{ next, live, cursor, … }` record lives in an
//! intrusive header in the block's own base page — outside the guarded
//! regions — so block tracking needs no second growable allocation and no
//! hand-picked block cap. Header access is abstracted behind [`BlockStore`]:
//! production reads/writes the identity-mapped header in place
//! ([`IdentityBlockStore`]); the host unit tests use an in-memory map, so
//! the allocator arithmetic is exercised on CI without real RAM.
//!
//! Like [`crate::mem_map`], the bump/list arithmetic is free of the
//! bare-metal ports, so it compiles — and its unit tests run — on the
//! CI host as well as on the bare-metal production builds that consume it,
//! and on no other configuration, so it is never dead code.

use rustos_kernel_core::{KernelStack, KTHREAD_STACK_BYTES};
use rustos_kernel_mem::{Frame, FrameAllocator, PhysAddr};
use rustos_sync::SpinLock;

/// Width of a stack's guard region, in bytes: one 4 KiB page.
///
/// Matches [`rustos_kernel_core::BoxStack`]'s guard so the arena form and
/// the software-canary fallback have identical geometry — the guard sits
/// immediately *below* the usable stack, so a downward overrun crosses it
/// first.
const STACK_GUARD_BYTES: u64 = 4096;

/// The widest ABI stack alignment any target requires;
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

/// Bytes reserved at each block's base for its intrusive [`BlockHeader`]:
/// one 4 KiB page. Regions begin at `base + BLOCK_HEADER_BYTES`, so the
/// header is outside every guarded region and the first region's guard
/// page is still 4 KiB-aligned (the block base is ≥ 4 KiB-aligned).
const BLOCK_HEADER_BYTES: u64 = 4096;

/// The header page must be large enough for a [`BlockHeader`].
const _BLOCK_HEADER_FITS: () = {
    assert!(core::mem::size_of::<BlockHeader>() as u64 <= BLOCK_HEADER_BYTES);
};

/// Size **and** alignment of a freshly chained arena block: one 2 MiB
/// region, matching the boot-carved arena's [`crate::mem_map`] block
/// granularity.
///
/// A 2 MiB-aligned block means every guard page inside it still lands on
/// its own L3 leaf when the spawn seam re-expresses the covering block at
/// 4 KiB granularity in the owning task's root, so a chained block hosts
/// hardware-guarded stacks exactly as the boot-carved arena does
/// (the capacity grows on demand without weakening the
/// guard-page invariant).
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

/// A chained block must hold its header page **and** at least one whole
/// guarded region. It does (2 MiB ≫ a header page plus a few-page region),
/// which is what makes [`StackArena::alloc`]'s grow loop provably bounded: a
/// single chained block always satisfies the pending request, so the loop
/// chains at most once per call (no retry-until-it-works).
const _ARENA_GROW_BLOCK_FITS_A_REGION: () = {
    assert!(ARENA_GROW_BLOCK_BYTES >= BLOCK_HEADER_BYTES + STACK_REGION_BYTES);
};

/// VA offset from a stack region's physical/identity base to the virtual
/// address the kernel runs the stack at (its `SP`/`RSP`) and unmaps the
/// guard page at.
///
/// aarch64 runs the kernel on the identity map (and sets `SP_EL1` from the
/// stack top directly), so the offset is **zero** — the identity address is
/// the stack address. x86_64 runs the kernel in the -2 GiB higher-half
/// window, and its `set_kernel_rsp0`/`validate_kernel_rsp0` *requires* a
/// canonical **kernel-half** RSP0 (a CVE-2019-1125 / Meltdown-class defence): a low-identity RSP0 is rejected fail-closed,
/// which would leave a ring-3 task's syscall stack pointing at a stale,
/// shared stack. So on x86_64 the stack is addressed through its **per-task**
/// higher-half alias `KERNEL_VMA_BASE + phys` (the window `new_identity`
/// builds with fresh, per-root tables), and the guard page is unmapped at
/// that same higher-half VA — so an overrun via the kernel-half RSP faults in
/// the task's own root, exactly as on aarch64. The offset is sourced from the
/// arch port (`KERNEL_VMA_BASE`), never re-hardcoded.
#[cfg(all(freestanding, kernel_isa = "x86_64"))]
const STACK_VA_OFFSET: u64 = rustos_arch_x86_64::paging::KERNEL_VMA_BASE;
#[cfg(not(all(freestanding, kernel_isa = "x86_64")))]
const STACK_VA_OFFSET: u64 = 0;

/// A kthread kernel stack carved from a guard-arena block.
///
/// The region is `[guard, guard + STACK_REGION_BYTES)` of
/// allocator-reserved RAM, laid out as a one-page guard region below the
/// usable stack. [`Self::guard_page`] is the page the spawn seam unmaps in
/// the owning task's root; [`KernelStack::top`] is the exclusive upper
/// bound of the usable region above it. Both are expressed at the virtual
/// address the kernel runs the stack at — the identity base on aarch64, the
/// per-task higher-half alias on x86_64 (see [`STACK_VA_OFFSET`]) — while the
/// stored [`Self::guard`] field stays the **physical/identity** base the
/// arena's block bookkeeping ([`StackArena::free`]) locates the region by.
///
/// It is **not** `Copy`: it owns its region for as long as it lives, and its
/// [`Drop`] returns that region to the arena ([`StackArena::free`]) so the
/// capacity shrinks when a task exits. On the host build
/// the `Drop` is inert (the unit tests call `free` explicitly through a test
/// [`BlockStore`]); only the freestanding `aarch64`/`x86_64`/`riscv64`
/// builds wire the production reclaim path.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ArenaStack {
    /// First byte of the region's **physical/identity** base — the low edge
    /// of the one-page guard. The arena's [`BlockStore`] bookkeeping and
    /// [`StackArena::free`] locate the owning block by this physical address,
    /// so it is stored unmodified; the kernel-visible VA (which adds
    /// [`STACK_VA_OFFSET`]) is computed in [`Self::guard_page`]/
    /// [`KernelStack::top`].
    guard: u64,
}

impl ArenaStack {
    /// The guard page's **virtual** address (4 KiB-aligned): the page the
    /// spawn seam re-expresses and unmaps in the owning task's page-table
    /// root so an overrun faults (`plans/PI.md` G3b-2). This is the address
    /// the kernel runs the stack at — the identity base on aarch64, the
    /// per-task higher-half alias on x86_64 ([`STACK_VA_OFFSET`]) — so the
    /// page unmapped is exactly the one a stack overrun reaches.
    pub(crate) fn guard_page(&self) -> u64 {
        STACK_VA_OFFSET + self.guard
    }
}

impl Drop for ArenaStack {
    fn drop(&mut self) {
        // The host build exercises reclamation through `StackArena::free`
        // directly (with a test `BlockStore`); only the freestanding
        // bare-metal builds own the single `'static` arena + frame
        // allocator the production reclaim path needs, so the `Drop` is inert
        // elsewhere and never references state that build lacks.
        #[cfg(all(
            freestanding,
            any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
        ))]
        reclaim_arena_stack(self.guard);
    }
}

// SAFETY: `top` returns the region's base plus its full length (at the
// kernel-visible VA — see `STACK_VA_OFFSET`), rounded down to `STACK_ALIGN`:
// the aligned exclusive upper bound of the usable stack above the guard. The
// arena is `RegionKind::Reserved` (boot block) or a `FrameAllocator`-reserved
// chained block, so the frames are not handed elsewhere, and the bump cursor
// hands each region out exactly once, so the region is exclusive to its owner
// for as long as the `ArenaStack` lives. The guard page aside (which the
// spawn seam deliberately unmaps in the task's own root, at this same VA),
// every byte of the usable region stays mapped and writable — via the
// identity map on aarch64, via the per-task higher-half window on x86_64.
unsafe impl KernelStack for ArenaStack {
    fn top(&self) -> u64 {
        let top = STACK_VA_OFFSET + self.guard + STACK_REGION_BYTES;
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

/// The intrusive per-block bookkeeping record, stored in the block's own
/// base page (the block list is itself a capacity, so
/// it needs no second growable allocation and no hand-picked block cap).
///
/// A plain `#[repr(C)]` of `u64`s so [`IdentityBlockStore`] can read/write
/// it in place at the block base (2 MiB-aligned, so well-aligned for `u64`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct BlockHeader {
    /// Base of the next block in the list, or `0` for the tail.
    next: u64,
    /// One past the block's last byte (`base + block_len`); used to locate
    /// the owning block of a freed region by address range and to bound the
    /// bump cursor.
    block_end: u64,
    /// Bump cursor: base of the next free region in this block.
    alloc_next: u64,
    /// Count of regions currently handed out from this block and not yet
    /// freed. A block with `live == 0` is *idle*.
    live: u64,
    /// `1` for the boot-carved block (kernel-image-owned `Reserved` frames,
    /// never returned to the allocator), `0` for a chained block.
    is_boot: u64,
}

impl BlockHeader {
    /// Base of the first region in a block based at `base` (immediately
    /// above the header page).
    const fn first_region(base: u64) -> u64 {
        base + BLOCK_HEADER_BYTES
    }
}

/// Read/write access to the intrusive [`BlockHeader`] stored at a block's
/// base.
///
/// Production ([`IdentityBlockStore`]) accesses the identity-mapped header
/// in place; the host unit tests use an in-memory map so the arena
/// arithmetic runs on CI without real RAM. All access happens under the
/// arena's [`SpinLock`], so the store implementations need no internal
/// synchronisation.
pub(crate) trait BlockStore {
    /// Read the header stored at block base `base`.
    fn read(&self, base: u64) -> BlockHeader;
    /// Write `header` to the header slot at block base `base`.
    fn write(&self, base: u64, header: BlockHeader);
}

/// A source of fresh, 2 MiB-aligned, identity-mapped arena blocks the
/// [`StackArena`] chains onto when every existing block is full
/// (a capacity that grows on demand).
pub(crate) trait ArenaGrow {
    /// Hand out a fresh `[base, base + ARENA_GROW_BLOCK_BYTES)` block, or
    /// `None` on genuine exhaustion. `base` is the block's identity-mapped
    /// address (`virtual == physical`).
    fn grow_block(&self) -> Option<u64>;
}

/// The symmetric counterpart of [`ArenaGrow`]: returns an *idle* chained
/// block's RAM to its backing (grow *and* shrink).
///
/// The implementation must scrub the block before releasing it (
/// zero-on-free — a kthread stack can hold spilled capability tokens) and
/// return `false` if the block cannot be safely scrubbed/returned, so the
/// arena retains it rather than releasing unsafely (fail closed).
/// It is only ever called for [`ARENA_GROW_BLOCK_BYTES`] chained blocks —
/// never the boot-carved block.
pub(crate) trait ArenaShrink {
    /// Scrub and return the chained block `[base, base + len)`; `true` if it
    /// was released, `false` to retain it (fail closed).
    fn release_block(&self, base: u64, len: u64) -> bool;
}

/// Outcome of [`StackArena::free`], so callers (and the host tests) can
/// observe the fail-closed paths without a side channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FreeOutcome {
    /// The region was returned to its block; the block is still in use.
    Freed,
    /// The region was returned and its (chained) block went idle but was
    /// kept resident by the one-free-block grace.
    FreedKeptIdle,
    /// The region was returned and its (chained) block was released to the
    /// allocator.
    FreedReleasedBlock,
    /// The region was returned, its block went idle and was eligible for
    /// release, but [`ArenaShrink::release_block`] retained it (fail closed).
    FreedRetainedBlock,
    /// The arena holds no blocks (not installed): nothing to free.
    NotInstalled,
    /// `guard` did not name a region start in any block — a foreign or
    /// misaligned address. Rejected without touching any block (fail closed,
    /// never an underflow).
    ForeignAddress,
    /// `guard` named a region in a block whose live count is already zero — a
    /// double free. Rejected without decrementing (fail closed, no underflow).
    DoubleFree,
}

/// The arena's persistent state behind its lock.
struct Inner {
    /// Set once installed; gates [`StackArena::alloc`]/[`StackArena::free`].
    installed: bool,
    /// Base of the first block in the list (the boot-carved block), or `0`.
    head: u64,
    /// Number of idle (`live == 0`) **chained** blocks currently resident.
    /// Capped at one by the one-free-block grace, so this is `0` or `1` in
    /// the steady state; a release that fails closed may transiently leave a
    /// second idle block resident (retained, still reusable).
    idle_chained: u32,
}

/// A guard-arena kthread-stack allocator that **grows and shrinks** on
/// demand.
///
/// Installed once at boot with the 2 MiB-aligned block the memory-map
/// builder carved ([`crate::mem_map`]); thereafter [`Self::alloc`] hands out
/// one [`ArenaStack`] per call and [`Self::free`] returns one. When every
/// block is full it chains a fresh 2 MiB block from the supplied
/// [`ArenaGrow`] source; when a chained block goes idle (and another idle
/// chained block is already resident) it returns the block through the
/// supplied [`ArenaShrink`] source. Only genuine physical exhaustion fails
/// `alloc` closed with `None`, and the caller then falls back to a
/// software-canary [`rustos_kernel_core::BoxStack`] — never an unguarded
/// stack.
///
/// The whole allocation/free is serialised by a [`SpinLock`]: both are
/// per-spawn / per-exit operations, not a hot path, so a lock is the
/// simplest correct way to walk the block list and chain/return a block
/// atomically (locking only off the hot path).
pub(crate) struct StackArena {
    inner: SpinLock<Inner>,
}

impl StackArena {
    /// Construct an empty, not-yet-installed arena (for the `'static`
    /// boot-installed instance and for unit tests).
    pub(crate) const fn new() -> Self {
        Self {
            inner: SpinLock::new(Inner {
                installed: false,
                head: 0,
                idle_chained: 0,
            }),
        }
    }

    /// Install the reserved first arena block `[base, base + len)`.
    ///
    /// Called **once**, on the boot CPU, before any task that could
    /// [`Self::alloc`] is spawned. `base` is the 2 MiB-aligned arena base
    /// the memory-map builder carved. Returns `false` if the arena was
    /// already installed (a re-entry is refused rather than silently
    /// re-basing a live list), if `base + len` overflows,
    /// or if the block is too small to hold its header page plus one region.
    pub(crate) fn install(&self, base: u64, len: u64, store: &dyn BlockStore) -> bool {
        let mut inner = self.inner.lock();
        if inner.installed {
            return false;
        }
        let Some(block_end) = base.checked_add(len) else {
            return false;
        };
        let first = BlockHeader::first_region(base);
        // The boot block must hold its header page and at least one region.
        match first.checked_add(STACK_REGION_BYTES) {
            Some(region_end) if region_end <= block_end => {}
            _ => return false,
        }
        store.write(
            base,
            BlockHeader {
                next: 0,
                block_end,
                alloc_next: first,
                live: 0,
                is_boot: 1,
            },
        );
        inner.head = base;
        inner.installed = true;
        true
    }

    /// Hand out the next guarded stack region, reusing an idle block or
    /// chaining a fresh one from `grow` when every block is full.
    ///
    /// Returns `None` only when the arena is not installed or the `grow`
    /// source is itself exhausted (fail closed — the caller falls back to a
    /// software-canary [`rustos_kernel_core::BoxStack`]).
    ///
    /// The grow path chains **at most once** per call: a chained block holds
    /// a header page and many regions (`_ARENA_GROW_BLOCK_FITS_A_REGION`), so
    /// the fresh block necessarily satisfies the request (no retry-until-it-works).
    pub(crate) fn alloc(&self, grow: &dyn ArenaGrow, store: &dyn BlockStore) -> Option<ArenaStack> {
        let mut inner = self.inner.lock();
        if !inner.installed {
            return None;
        }

        // Reuse: walk the existing blocks; an idle block is reset to its
        // first region so its freed space is reusable, and the first block
        // with room serves the request.
        let mut cur = inner.head;
        while cur != 0 {
            let mut header = store.read(cur);
            let was_idle = header.live == 0;
            if was_idle {
                header.alloc_next = BlockHeader::first_region(cur);
            }
            if let Some(region_end) = header.alloc_next.checked_add(STACK_REGION_BYTES) {
                if region_end <= header.block_end {
                    let guard = header.alloc_next;
                    header.alloc_next = region_end;
                    header.live += 1;
                    store.write(cur, header);
                    if was_idle && header.is_boot == 0 {
                        inner.idle_chained = inner.idle_chained.saturating_sub(1);
                    }
                    return Some(ArenaStack { guard });
                }
            }
            cur = header.next;
        }

        // Grow: chain one fresh block and serve from it.
        let base = grow.grow_block()?;
        let block_end = base.checked_add(ARENA_GROW_BLOCK_BYTES)?;
        let first = BlockHeader::first_region(base);
        let region_end = first.checked_add(STACK_REGION_BYTES)?;
        if region_end > block_end {
            return None;
        }
        store.write(
            base,
            BlockHeader {
                next: inner.head,
                block_end,
                alloc_next: region_end,
                live: 1,
                is_boot: 0,
            },
        );
        inner.head = base;
        Some(ArenaStack { guard: first })
    }

    /// Return the region whose guard page is `guard` to its owning block.
    ///
    /// Locates the owning block by address range, checked-decrements its
    /// live count, and — when a chained block goes idle and another idle
    /// chained block is already resident — releases this one through
    /// `shrink` (the one-free-block grace). A foreign or
    /// misaligned address, or an already-zero block, is rejected without
    /// underflowing (fail closed). The boot block is never
    /// released. See [`FreeOutcome`].
    pub(crate) fn free(
        &self,
        guard: u64,
        shrink: &dyn ArenaShrink,
        store: &dyn BlockStore,
    ) -> FreeOutcome {
        let mut inner = self.inner.lock();
        if !inner.installed {
            return FreeOutcome::NotInstalled;
        }

        let mut prev = 0u64;
        let mut cur = inner.head;
        while cur != 0 {
            let header = store.read(cur);
            let first = BlockHeader::first_region(cur);
            if guard >= first && guard < header.block_end {
                if (guard - first) % STACK_REGION_BYTES != 0 {
                    return FreeOutcome::ForeignAddress;
                }
                if header.live == 0 {
                    return FreeOutcome::DoubleFree;
                }
                let mut updated = header;
                updated.live -= 1;
                store.write(cur, updated);

                if updated.live != 0 || updated.is_boot != 0 {
                    return FreeOutcome::Freed;
                }
                // A chained block just went idle: apply the grace.
                if inner.idle_chained >= 1 {
                    if shrink.release_block(cur, ARENA_GROW_BLOCK_BYTES) {
                        // Unlink the released block from the list.
                        if prev == 0 {
                            inner.head = updated.next;
                        } else {
                            let mut prev_header = store.read(prev);
                            prev_header.next = updated.next;
                            store.write(prev, prev_header);
                        }
                        return FreeOutcome::FreedReleasedBlock;
                    }
                    return FreeOutcome::FreedRetainedBlock;
                }
                inner.idle_chained += 1;
                return FreeOutcome::FreedKeptIdle;
            }
            prev = cur;
            cur = header.next;
        }
        FreeOutcome::ForeignAddress
    }
}

/// An [`ArenaGrow`] that chains fresh blocks out of the kernel's live
/// [`FrameAllocator`], bounded to the per-space identity window so a
/// chained kthread stack stays identity-mapped in every address space a
/// task runs under.
///
/// Each grow draws a 2 MiB-aligned [`ARENA_GROW_BLOCK_ORDER`] block from the
/// buddy allocator. A block whose end exceeds [`Self::identity_limit`] would
/// be unmapped in some space the task executes under — its guard page could
/// not fault there, and a stack body byte would be unreachable — so it is
/// returned to the allocator and the grow fails closed,
/// dropping the caller to the software-canary [`rustos_kernel_core::BoxStack`]
/// fallback rather than handing out an unreachable stack.
pub(crate) struct FrameArenaGrow<'a> {
    frames: &'a FrameAllocator,
    /// Exclusive upper bound: a chained block must lie wholly below this so
    /// it is covered by every address space's identity map.
    identity_limit: u64,
}

impl<'a> FrameArenaGrow<'a> {
    /// Wrap the live frame allocator, bounding chained blocks to
    /// `[0, identity_limit)` (the identity window each spawned space
    /// identity-maps — on aarch64 derived from the configured Device/RAM
    /// gigapage masks, `paging::configured_identity_gigapages`).
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

/// The symmetric [`ArenaShrink`] over the live [`FrameAllocator`]: it scrubs
/// an idle chained block and returns it through [`FrameAllocator::free_order`]
/// (the capacity shrinks; — zero-on-free).
///
/// `release_block` is only ever called for a [`ARENA_GROW_BLOCK_BYTES`]
/// chained block whose live count is zero (the arena guarantees both before
/// calling), so the whole block is exclusively owned and safe to scrub.
pub(crate) struct FrameArenaShrink<'a> {
    frames: &'a FrameAllocator,
}

impl<'a> FrameArenaShrink<'a> {
    /// Wrap the live frame allocator.
    pub(crate) fn new(frames: &'a FrameAllocator) -> Self {
        Self { frames }
    }
}

impl ArenaShrink for FrameArenaShrink<'_> {
    fn release_block(&self, base: u64, len: u64) -> bool {
        // Only a whole, correctly-sized chained block is ever released; a
        // mismatch means a caller bug, so retain rather than free the wrong
        // thing (fail closed).
        if len != ARENA_GROW_BLOCK_BYTES {
            return false;
        }
        let Ok(len_usize) = usize::try_from(len) else {
            return false;
        };
        // SAFETY: the arena calls this only for an idle (`live == 0`)
        // chained block it handed out from `FrameArenaGrow`, i.e. a
        // 2 MiB-aligned, identity-mapped (`virtual == physical`) RAM block
        // exclusively owned by the arena with no live region inside it.
        // Scrubbing its whole extent is therefore a write to owned, mapped,
        // exclusive RAM (clear spilled capability tokens
        // before the frames are reused).
        unsafe {
            scrub_block(base, len_usize);
        }
        let frame = Frame::containing(PhysAddr::new(base));
        self.frames
            .free_order(frame, ARENA_GROW_BLOCK_ORDER)
            .is_ok()
    }
}

/// Zero `[base, base + len)` in place (zero-on-free).
///
/// Split out from [`FrameArenaShrink::release_block`] so the scrub itself is
/// unit-tested over a real host buffer, while the surrounding
/// `free_order` return-to-allocator step is proven on the aarch64 QEMU
/// guard verticals (the host `FrameAllocator` is index-addressed and cannot
/// dereference a fabricated low physical base).
///
/// # Safety
///
/// `[base, base + len)` must be an owned, mapped, writable, exclusively-held
/// RAM region with no live reference into it.
unsafe fn scrub_block(base: u64, len: usize) {
    let Ok(base_usize) = usize::try_from(base) else {
        return;
    };
    // SAFETY: the caller guarantees `[base, base + len)` is owned, mapped,
    // writable, and exclusive (see the function contract).
    unsafe {
        core::ptr::write_bytes(base_usize as *mut u8, 0, len);
    }
}

/// The production [`BlockStore`]: read/write the intrusive [`BlockHeader`]
/// in the identity-mapped header page at the block's own base.
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
pub(crate) struct IdentityBlockStore;

#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
impl BlockStore for IdentityBlockStore {
    fn read(&self, base: u64) -> BlockHeader {
        // SAFETY: `base` is a block base the arena installed/chained — a
        // 2 MiB-aligned (so `BlockHeader`-aligned), identity-mapped RAM page
        // reserved for this header, accessed under the arena lock, and
        // initialised by a prior `write` before any `read`.
        unsafe { core::ptr::read(base as *const BlockHeader) }
    }
    fn write(&self, base: u64, header: BlockHeader) {
        // SAFETY: as for `read` — `base`'s header page is owned, aligned,
        // identity-mapped, writable RAM, accessed under the arena lock.
        unsafe { core::ptr::write(base as *mut BlockHeader, header) }
    }
}

/// An [`ArenaShrink`] that always retains (releases nothing): the fail-safe
/// used when no live [`FrameAllocator`] has been published for reclamation,
/// so a freed region's bookkeeping is still updated but no block is returned
/// (fail closed).
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
struct RetainShrink;

#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
impl ArenaShrink for RetainShrink {
    fn release_block(&self, _base: u64, _len: u64) -> bool {
        false
    }
}

/// The single, `'static` guard-stack arena the boot path installs
/// (`aarch64::boot` / `x86_64::boot` / `riscv64::boot`) and the spawn seams
/// draw from (`aarch64::init_spawn` / `x86_64::init_spawn` /
/// `riscv64::init_spawn`, `aarch64::spawn_producer` / `x86_64::spawn_producer`
/// / `riscv64::spawn_producer`).
///
/// Only the bare-metal builds instantiate it; the host-test
/// build exercises the allocator through locally constructed [`StackArena`]s,
/// so the shared instance is gated out there to stay free of an unused-static
/// warning.
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
pub(crate) static KTHREAD_STACK_ARENA: StackArena = StackArena::new();

/// The live `'static` [`FrameAllocator`] reclamation returns idle chained
/// blocks to, published once on the first runtime spawn
/// ([`publish_reclaim_frames`]). A region freed before any allocator is
/// published is still accounted (its block's live count decremented) but no
/// block is released — fail safe.
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
static SHRINK_FRAMES: rustos_sync::Once<&'static FrameAllocator> = rustos_sync::Once::new();

/// Publish the live `'static` frame allocator the [`ArenaStack`] `Drop`
/// reclaim path returns idle chained blocks to. Idempotent (set-once); a
/// later call with a different allocator is ignored, matching the one
/// boot-threaded allocator the spawn path already uses.
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
pub(crate) fn publish_reclaim_frames(frames: &'static FrameAllocator) {
    let _ = SHRINK_FRAMES.call_once_infallible(|| frames);
}

/// Reclaim an [`ArenaStack`]'s region on drop: return it to
/// [`KTHREAD_STACK_ARENA`], releasing its (chained) block to the published
/// `'static` allocator when the one-free-block grace allows, or retaining it
/// when none is published (fail safe).
#[cfg(all(
    freestanding,
    any(kernel_isa = "aarch64", kernel_isa = "x86_64", kernel_isa = "riscv64")
))]
fn reclaim_arena_stack(guard: u64) {
    match SHRINK_FRAMES.get() {
        Ok(Some(frames)) => {
            KTHREAD_STACK_ARENA.free(guard, &FrameArenaShrink::new(frames), &IdentityBlockStore);
        }
        _ => {
            KTHREAD_STACK_ARENA.free(guard, &RetainShrink, &IdentityBlockStore);
        }
    }
}

#[cfg(test)]
#[path = "stack_arena_tests.rs"]
mod tests;
