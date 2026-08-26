//! Growable kernel heap wiring.
//!
//! The kernel `#[global_allocator]` is a [`tairix_kalloc::FreeListAllocator`]
//! living in the bin crate over a small `.bss` bootstrap region. That
//! bootstrap covers early boot, before a physical frame allocator exists;
//! once one does, the boot path installs a source here so the heap draws
//! fresh memory on demand and hands it back — the growable-capacity
//! discipline the charter requires of every resource ceiling, replacing the
//! former fixed slab that a busy kernel could exhaust into an
//! allocation-failure panic.
//!
//! One source feeds both of the allocator's tiers: whole regions for the
//! byte-granular tier (below), and single direct-mapped frames for the slab
//! tier that serves everything up to a page.
//!
//! Three seams connect the bin's allocator to the kernel core:
//!
//! * [`register_global_heap`] — each arch bin publishes its
//!   `#[global_allocator]` here with one line before it calls `boot`, so
//!   the core can reach the same allocator instance without naming the
//!   bin.
//! * [`install_kheap_irq_control`] — the bin installs its per-CPU
//!   interrupt mask/restore so the allocator's lock is interrupt-safe.
//! * [`install_frame_heap_source`] — the boot path calls this once the
//!   frame allocator, the arch direct physical map, and the port's kernel
//!   remap window all exist and are `'static`, wiring the frame-backed
//!   source into the registered heap.
//!
//! # How a region is grown
//!
//! Each region is drawn as the **exact** page count the request needs and
//! assembled from as many `<= MAX_ORDER` physical chunks as the pool can
//! offer, mapped into one virtually-contiguous run of the port's kernel
//! remap window ([`tairix_kernel_mem::KernelVirtMap`]). Two properties
//! follow, and both are the point:
//!
//! * Growth succeeds whenever the *total* free frame count suffices, in any
//!   physical layout. It no longer needs one large physically-contiguous
//!   block, so it cannot fail on a fragmented pool while gigabytes are
//!   free, and the largest serviceable single allocation is bounded by RAM
//!   rather than by the buddy allocator's contiguity order.
//! * Internal waste is under one page, where a power-of-two growth granule
//!   cost up to twice the request.
//!
//! Growth draws through the frame allocator's **kernel** commit path, so it
//! may use the kernel reserve and keeps making progress under user memory
//! pressure. Window pages are mapped read/write and never executable.
//!
//! # How a slab page is supplied
//!
//! The slab tier wants one whole page addressed by an ordinary pointer, which
//! is a frame plus the direct map ([`tairix_kernel_mem::FramePages`]) — no
//! window slot, no page-table work, and no invalidation on either side. That
//! is what lets a page-sized allocation cost exactly one frame: a window slot
//! per single-page slab would put a first-fit walk back one layer down.
//!
//! # No re-entry into the heap being grown
//!
//! Every path here runs under the global heap's own non-reentrant lock, so
//! it must allocate nothing from that heap: no `Vec` of chunks, no boxed
//! side table of what was mapped. The page tables are the record —
//! [`tairix_kalloc::HeapSource::shrink`] recovers each frame by walking
//! them — and the address-space bookkeeping keeps its own state in frames
//! drawn from the frame allocator ([`tairix_kernel_mem::SlotWindow`]),
//! which is heap-independent by construction.
//!
//! A bin that registers no heap, or a port that reserves no remap window,
//! simply leaves the heap capped at its bootstrap region (fail closed,
//! never a panic).

use alloc::boxed::Box;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use tairix_kalloc::{FreeListAllocator, HeapSource};
use tairix_kernel_mem::{
    AllocError, Frame, FrameAllocator, FramePages, KernelVirtMap, PhysMap, SlotWindow, MAX_ORDER,
    PAGE_SIZE,
};
use tairix_sync::SpinLock;

/// Minimum growth granule, in pages (64 KiB).
///
/// A miss draws at least this much even for a small allocation, so a burst
/// of small allocations does not force a fresh frame draw each time
/// (amortised growth); the remainder stays as free holes the next
/// allocation reuses, and a wholly-drained region is returned intact.
const MIN_GROW_PAGES: usize = 16;

/// The registered bin `#[global_allocator]`, or null before a bin
/// publishes one. Set once per boot by [`register_global_heap`].
static GLOBAL_HEAP: AtomicPtr<FreeListAllocator> = AtomicPtr::new(core::ptr::null_mut());

/// Publish the bin's `#[global_allocator]` so [`install_frame_heap_source`]
/// can later wire the growth source into it.
///
/// Each arch bin calls this with its `&'static FreeListAllocator` before
/// entering `boot`. Idempotent-by-policy: the boot path registers exactly
/// one heap for the life of the binary; a second call simply retargets the
/// slot.
pub fn register_global_heap(heap: &'static FreeListAllocator) {
    GLOBAL_HEAP.store(
        core::ptr::from_ref::<FreeListAllocator>(heap).cast_mut(),
        Ordering::Release,
    );
}

/// Make the registered kernel heap's lock **interrupt-safe** by installing
/// the arch's per-CPU interrupt mask/restore primitives into it.
///
/// TAIRiX takes interrupts while in-kernel code runs, so an interrupt
/// service routine can fire on a CPU that is mid-allocation holding the
/// allocator lock; without masking, an ISR that allocates would spin
/// forever on the lock its own interrupted mainline holds — a single-CPU
/// self-deadlock. Each arch bin calls this once from `boot`, **before**
/// interrupts are first enabled and before any secondary CPU is started,
/// passing its `InterruptControl` primitives (`msr daifset` on AArch64,
/// `cli`/`pushf` on x86_64, `csrrci sstatus` on RISC-V) adapted to the
/// opaque-token `fn` shape [`FreeListAllocator::install_irq_control`] takes.
/// A no-op when no bin registered a heap (a host harness); the
/// interrupt-free `wasm32` port installs nothing (fail-safe: that window is
/// single-CPU with interrupts already masked).
pub fn install_kheap_irq_control(disable: fn() -> usize, restore: fn(usize)) {
    let Some(heap) = global_heap() else {
        return;
    };
    heap.install_irq_control(disable, restore);
}

/// Borrow the registered heap, or `None` when no bin published one (a host
/// harness, or an early call before registration).
fn global_heap() -> Option<&'static FreeListAllocator> {
    // SAFETY: the pointer is only ever set by `register_global_heap` from a
    // `&'static FreeListAllocator`, so a non-null value is a valid `'static`
    // reference for the life of the binary; null means none was registered.
    unsafe { GLOBAL_HEAP.load(Ordering::Acquire).as_ref() }
}

/// The production kernel-heap source: physical chunks from the frame
/// allocator assembled into one virtually-contiguous region in the port's
/// kernel remap window for the byte-granular tier, and plain direct-mapped
/// frames for the slab tier.
struct FrameHeapSource {
    frames: &'static FrameAllocator,
    kvmap: &'static dyn KernelVirtMap,
    /// The slab tier's page supply. A slab page is one frame addressed
    /// through the direct map: no window slot, no page-table work, and no
    /// invalidation, which is what makes a page-sized allocation cost exactly
    /// one frame.
    pages: FramePages,
    /// Which runs of the window are handed out. Locked rather than borrowed
    /// because the heap drives the source through a shared reference; the
    /// only caller already holds the global heap lock, whose hold masks this
    /// CPU's interrupts, so no interrupt service routine can reenter and the
    /// critical section is uncontended by construction. It is always taken
    /// *before* the remap map's own lock, on both the grow and the shrink
    /// path, so the two can never be acquired in opposing order.
    slots: SpinLock<SlotWindow>,
}

impl FrameHeapSource {
    /// Back `[base, base + pages)` with physical chunks, preferring the
    /// largest block that fits the remainder and stepping the order down
    /// when the pool cannot offer one.
    ///
    /// Returns `false` having mapped only part of the run when the frame
    /// pool is genuinely exhausted or the port refuses a leaf; the caller
    /// then tears the partial run down.
    fn fill(&self, base: u64, pages: usize) -> bool {
        let mut done = 0;
        while done < pages {
            let remaining = pages - done;
            // Largest order whose block fits the remainder, capped at the
            // allocator's contiguity ceiling. `remaining >= 1` (the loop
            // guard), so `ilog2` is well defined.
            let mut order = core::cmp::min(remaining.ilog2(), MAX_ORDER);
            let frame = loop {
                // The kernel commit path, so growth may draw the reserve and
                // keeps making progress under user memory pressure.
                match self.frames.alloc_order(order) {
                    Ok(frame) => break frame,
                    // No block of this order is free; the pool may be
                    // fragmented, so step down one size and retry before
                    // declaring the request out of memory.
                    Err(AllocError::OutOfMemory) if order > 0 => order -= 1,
                    Err(_) => return false,
                }
            };
            let chunk = 1usize << order;
            let vaddr = base + (done * PAGE_SIZE) as u64;
            if self.kvmap.map_chunk(vaddr, frame, chunk).is_err() {
                let _ = self.frames.free_order(frame, order);
                return false;
            }
            done += chunk;
        }
        true
    }

    /// Tear `[base, base + pages)` down, returning every frame it held.
    fn drain(&self, base: u64, pages: usize) {
        self.kvmap.unmap_run(base, pages, &mut |frame: Frame| {
            // A frame the window held is always one this allocator handed
            // out, so the free cannot legitimately fail; there is no
            // recovery beyond declining, so the result is dropped rather
            // than panicking.
            let _ = self.frames.free(frame);
        });
    }
}

impl HeapSource for FrameHeapSource {
    fn grow(&self, min_len: usize) -> Option<(*mut u8, usize)> {
        // The exact page count, floored at the amortised growth granule —
        // never rounded up to a power of two.
        let pages = min_len.div_ceil(PAGE_SIZE).max(MIN_GROW_PAGES);
        let len = pages.checked_mul(PAGE_SIZE)?;
        let window = self.kvmap.window();

        let mut slots = self.slots.lock();
        let slot = slots.allocate(pages).ok()?;
        // The window's extent was validated when it was built, so a slot
        // inside it always has a representable address.
        let base = window.base() + (slot as u64) * PAGE_SIZE as u64;
        let Ok(addr) = usize::try_from(base) else {
            let _ = slots.release(slot, pages);
            return None;
        };
        if !self.fill(base, pages) {
            // Fail closed leaking nothing: hand back every chunk that did
            // land and release the address space.
            self.drain(base, pages);
            let _ = slots.release(slot, pages);
            return None;
        }
        Some((addr as *mut u8, len))
    }

    fn alloc_page(&self) -> Option<NonNull<u8>> {
        self.pages.alloc()
    }

    fn free_page(&self, page: NonNull<u8>) {
        self.pages.free(page);
    }

    fn shrink(&self, base: *mut u8, len: usize) {
        let base_addr = base as u64;
        let pages = len / PAGE_SIZE;
        let Some(slot) = self.kvmap.window().page_index(base_addr) else {
            // Not an address this source ever handed out: fail closed
            // rather than unmap a region belonging to something else.
            return;
        };
        let mut slots = self.slots.lock();
        // Release first: it accepts only an exact live run, so a mismatched
        // `(base, len)` is refused *before* anything is unmapped or freed.
        // The lock is held across the teardown, so the released address
        // space cannot be re-handed out while its pages are still mapped.
        if slots.release(slot, pages).is_err() {
            return;
        }
        self.drain(base_addr, pages);
    }
}

/// Wire the frame-backed source into the registered kernel heap, so its
/// byte-granular tier can grow past the bootstrap region and its slab tier
/// can draw pages.
///
/// Called once from the boot path after the frame allocator, the arch direct
/// physical map, and the port's kernel remap window all exist and are
/// `'static`. `physmap` backs the address-space bookkeeping's own
/// heap-independent record storage; `kvmap` is where the regions are
/// assembled. A no-op when no bin registered a heap (a host harness): the
/// heap then stays capped at its bootstrap region, fail closed.
pub fn install_frame_heap_source(
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
    kvmap: &'static dyn KernelVirtMap,
) {
    let Some(heap) = global_heap() else {
        return;
    };
    let Ok(slots) = SlotWindow::new(kvmap.window().pages(), frames, physmap) else {
        return;
    };
    let source: &'static FrameHeapSource = Box::leak(Box::new(FrameHeapSource {
        frames,
        kvmap,
        pages: FramePages::new(frames, physmap),
        slots: SpinLock::new(slots),
    }));
    heap.install_source(source);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::{opt_in_current_thread, opt_out_current_thread, LiveBytes};
    use alloc::vec::Vec;
    use tairix_arch_api::mmu::{
        AccessTracking, AddressSpace as HalAddressSpace, BlockSplit, KernelWindow, MapError,
        PageFlags,
    };
    use tairix_arch_api::tlb::TlbShootdown;
    use tairix_arch_api::CrossCpuTlbShootdown;
    use tairix_kernel_mem::{
        BootMemoryMap, KernelRemap, MemoryRegion, PhysAddr, RegionKind, SimPhysMap,
    };

    /// Base of the window the host tests remap into. Far from every
    /// simulated physical window, so a confused address cannot accidentally
    /// resolve.
    const WINDOW_BASE: u64 = 0x4000_0000_0000;

    /// The host has no TLB, so a shootdown is vacuous; the discipline the
    /// remap layer applies around it is asserted in `kernel/mem`.
    struct NoTlb;

    impl CrossCpuTlbShootdown for NoTlb {
        fn shootdown_page(&self, _vaddr: u64) {}
    }

    /// A page-table double over one window, backed by a slot per page.
    ///
    /// The `kernel/mem` `HostPageTable` keeps its leaves in a `BTreeMap`,
    /// which allocates — fine for that crate's tests, fatal for the
    /// no-re-entry proof here. This double reserves its storage once at
    /// construction and never allocates again, exactly as a real port's
    /// table draws frames rather than heap.
    struct WindowPageTable {
        base: u64,
        leaves: Vec<Option<u64>>,
    }

    impl WindowPageTable {
        fn new(base: u64, pages: usize) -> Self {
            Self {
                base,
                leaves: alloc::vec![None; pages],
            }
        }

        fn slot(&self, vaddr: u64) -> Option<usize> {
            if vaddr < self.base {
                return None;
            }
            let index = usize::try_from((vaddr - self.base) / PAGE_SIZE as u64).ok()?;
            (index < self.leaves.len()).then_some(index)
        }
    }

    impl HalAddressSpace for WindowPageTable {
        fn map_page(&mut self, vaddr: u64, paddr: u64, flags: PageFlags) -> Result<(), MapError> {
            if !vaddr.is_multiple_of(PAGE_SIZE as u64) || !paddr.is_multiple_of(PAGE_SIZE as u64) {
                return Err(MapError::Misaligned);
            }
            if flags.is_write_exec() {
                return Err(MapError::InvalidFlags);
            }
            let slot = self.slot(vaddr).ok_or(MapError::PoolExhausted)?;
            if self.leaves[slot].is_some() {
                return Err(MapError::AlreadyMapped);
            }
            self.leaves[slot] = Some(paddr);
            Ok(())
        }

        fn translate(&self, vaddr: u64) -> Option<(u64, PageFlags)> {
            let slot = self.slot(vaddr)?;
            self.leaves[slot].map(|paddr| (paddr, PageFlags::READ.union(PageFlags::WRITE)))
        }

        fn unmap(&mut self, vaddr: u64) -> Result<u64, MapError> {
            if !vaddr.is_multiple_of(PAGE_SIZE as u64) {
                return Err(MapError::Misaligned);
            }
            let slot = self.slot(vaddr).ok_or(MapError::NotMapped)?;
            self.leaves[slot].take().ok_or(MapError::NotMapped)
        }

        fn root_phys(&self) -> u64 {
            PAGE_SIZE as u64
        }

        fn block_split_support(&self) -> BlockSplit {
            BlockSplit::Unsupported("the double tracks single 4 KiB leaves")
        }

        fn access_tracking(&self) -> AccessTracking {
            AccessTracking::Unsupported("the double models no referenced bit")
        }

        unsafe fn activate(&self) {}
    }

    impl TlbShootdown for WindowPageTable {
        fn flush_page(&mut self, _vaddr: u64) {}
    }

    /// A frame-backed growth source and the pieces it draws from, all
    /// leaked `'static` as they are in production.
    struct Harness {
        frames: &'static FrameAllocator,
        source: &'static FrameHeapSource,
        window: KernelWindow,
    }

    /// Build a harness whose only usable RAM is `ram_pages` frames based at
    /// `ram_base`, with an empty bootstrap heap so every allocation must
    /// grow.
    ///
    /// The window's addresses are *not* dereferenceable on the host (no
    /// hardware maps them), so these tests drive the growth source's
    /// contract — extents, frame accounting, fail-closed behaviour — and the
    /// end-to-end dereference is proven on the metal by the QEMU verticals,
    /// which cannot boot at all unless the window works.
    fn harness(ram_base: u64, ram_pages: usize, window_pages: usize) -> Harness {
        let sim: &'static SimPhysMap = Box::leak(Box::new(SimPhysMap::new(
            PhysAddr::new(ram_base),
            ram_pages * PAGE_SIZE,
        )));
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(ram_base),
            length: (ram_pages * PAGE_SIZE) as u64,
            kind: RegionKind::Usable,
        });
        let frames: &'static FrameAllocator =
            Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));

        let xtlb: &'static NoTlb = Box::leak(Box::new(NoTlb));
        let window = KernelWindow::new(WINDOW_BASE, window_pages).expect("valid window");
        let table = WindowPageTable::new(WINDOW_BASE, window_pages);
        let kvmap: &'static KernelRemap<WindowPageTable> =
            Box::leak(Box::new(KernelRemap::new(window, table, xtlb)));

        let slots = SlotWindow::new(window_pages, frames, sim).expect("non-empty window");
        let source: &'static FrameHeapSource = Box::leak(Box::new(FrameHeapSource {
            frames,
            kvmap,
            pages: FramePages::new(frames, sim),
            slots: SpinLock::new(slots),
        }));
        let harness = Harness {
            frames,
            source,
            window,
        };
        harness.warm();
        harness
    }

    impl Harness {
        /// Run one small grow/shrink cycle so the address-space
        /// bookkeeping's record arena has drawn the frame it keeps for
        /// reuse. Every frame-accounting assertion is taken against the
        /// steady state that follows, so the arena's retained frame is not
        /// mistaken for a leak.
        fn warm(&self) {
            if let Some((base, len)) = self.source.grow(1) {
                self.source.shrink(base, len);
            }
        }
    }

    /// Fragment the pool so no aligned block of more than two frames
    /// survives: draw every frame, then return only those whose index is not
    /// a multiple of four. The pinned frames stay allocated for the rest of
    /// the test.
    fn fragment_pool(frames: &'static FrameAllocator) {
        let mut drawn = Vec::new();
        while let Ok(frame) = frames.alloc() {
            drawn.push(frame);
        }
        for frame in drawn {
            if frame.0 % 4 != 0 {
                frames.free(frame).expect("a just-drawn frame frees");
            }
        }
    }

    #[test]
    fn grows_from_frames_and_shrinks_back() {
        let h = harness(0x10_0000, 512, 4096);
        let free_before = h.frames.free_frames();

        // 128 KiB — the empty bootstrap cannot satisfy it — forcing a grow.
        let (base, len) = h
            .source
            .grow(128 * 1024)
            .expect("grow satisfied the large request");
        assert!(len >= 128 * 1024);
        assert!(
            h.window.page_index(base as u64).is_some(),
            "the region lives in the remap window"
        );
        assert!(
            h.frames.free_frames() < free_before,
            "growth drew frames from the allocator"
        );

        h.source.shrink(base, len);
        assert_eq!(
            h.frames.free_frames(),
            free_before,
            "shrink returned every drawn frame"
        );
    }

    #[test]
    fn grows_across_a_fragmented_pool_with_no_large_contiguous_block() {
        // Enough RAM that the request also exceeds one `MAX_ORDER` block,
        // so this covers the headline regression: a fragmented pool *and* a
        // region larger than the largest contiguous draw.
        let block_pages = 1usize << MAX_ORDER;
        let h = harness(0x100_0000, block_pages + 4096, 2 * block_pages);
        fragment_pool(h.frames);
        let free_before = h.frames.free_frames();
        assert!(
            h.frames.alloc_order(2).is_err(),
            "no four-frame contiguous block survives the fragmentation"
        );

        let pages = block_pages + 1;
        assert!(pages <= free_before, "the pool has the frames in total");
        let (base, len) = h
            .source
            .grow(pages * PAGE_SIZE)
            .expect("growth served the request from a fragmented pool");
        assert_eq!(len, pages * PAGE_SIZE);

        // Every page of the region is backed by a distinct frame.
        let mut seen = Vec::with_capacity(pages);
        for index in 0..pages {
            let vaddr = base as u64 + (index * PAGE_SIZE) as u64;
            seen.push(
                h.source
                    .kvmap
                    .translate(vaddr)
                    .expect("every page of the region is mapped"),
            );
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pages, "no frame backs two pages");

        h.source.shrink(base, len);
        assert_eq!(
            h.frames.free_frames(),
            free_before,
            "shrink returned every frame the assembled region held"
        );
    }

    #[test]
    fn grows_for_an_allocation_spanning_several_max_order_blocks() {
        let block_pages = 1usize << MAX_ORDER;
        let pages = 2 * block_pages + 3;
        let h = harness(0x100_0000, pages + 512, 4 * block_pages);
        let free_before = h.frames.free_frames();

        let (base, len) = h
            .source
            .grow(pages * PAGE_SIZE)
            .expect("growth is not capped at one contiguous block");
        assert_eq!(len, pages * PAGE_SIZE);
        assert!(h.frames.free_frames() < free_before, "growth drew frames");

        h.source.shrink(base, len);
        assert_eq!(
            h.frames.free_frames(),
            free_before,
            "shrink returned every drawn frame"
        );
    }

    #[test]
    fn a_grown_region_wastes_less_than_one_page() {
        let h = harness(0x10_0000, 2048, 4096);
        // A request a page-exact draw serves with under a page of slack but
        // a power-of-two granule would nearly double.
        let min_len = 33 * PAGE_SIZE + 1;
        let (base, len) = h.source.grow(min_len).expect("grows");
        assert!(len >= min_len);
        assert!(
            len - min_len < PAGE_SIZE,
            "waste of {} bytes must be under one page",
            len - min_len
        );
        h.source.shrink(base, len);
    }

    #[test]
    fn a_small_request_still_draws_the_amortised_granule() {
        let h = harness(0x10_0000, 512, 4096);
        let (base, len) = h.source.grow(1).expect("grows");
        assert_eq!(len, MIN_GROW_PAGES * PAGE_SIZE);
        h.source.shrink(base, len);
    }

    #[test]
    fn true_exhaustion_fails_closed_without_leaking() {
        let h = harness(0x10_0000, 64, 4096);
        let free_before = h.frames.free_frames();
        // Far more pages than the pool holds: the partial fill must be
        // handed back whole.
        assert!(h.source.grow(4096 * PAGE_SIZE).is_none());
        assert_eq!(
            h.frames.free_frames(),
            free_before,
            "a refused grow returns every frame it drew"
        );
        // And the address space is available again from the start.
        let (base, len) = h.source.grow(PAGE_SIZE).expect("a small grow still fits");
        assert_eq!(
            h.window.page_index(base as u64),
            Some(0),
            "the refused reservation was released"
        );
        h.source.shrink(base, len);
    }

    #[test]
    fn a_window_too_small_for_the_granule_fails_closed() {
        let h = harness(0x10_0000, 512, MIN_GROW_PAGES - 1);
        let free_before = h.frames.free_frames();
        assert!(h.source.grow(1).is_none());
        assert_eq!(h.frames.free_frames(), free_before);
    }

    #[test]
    fn shrink_refuses_an_address_the_source_never_handed_out() {
        let h = harness(0x10_0000, 512, 4096);
        let (base, len) = h.source.grow(PAGE_SIZE).expect("grows");
        let free_after_grow = h.frames.free_frames();

        // Outside the window entirely.
        h.source.shrink(0x1000_usize as *mut u8, len);
        // Inside the window but not a run this source reserved.
        let above = base as usize + len;
        h.source.shrink(above as *mut u8, len);
        // The right base with the wrong extent.
        h.source.shrink(base, len + PAGE_SIZE);
        assert_eq!(
            h.frames.free_frames(),
            free_after_grow,
            "no refused shrink freed a frame"
        );

        // The matching shrink still works.
        h.source.shrink(base, len);
        assert!(h.frames.free_frames() > free_after_grow);
    }

    #[test]
    fn a_drained_window_is_reused_rather_than_marched_through() {
        let h = harness(0x10_0000, 512, 4096);
        let (first, len) = h.source.grow(PAGE_SIZE).expect("grows");
        h.source.shrink(first, len);
        let (second, len2) = h.source.grow(PAGE_SIZE).expect("grows again");
        assert_eq!(first, second, "the released run was handed out again");
        h.source.shrink(second, len2);
    }

    #[test]
    fn a_slab_page_costs_one_frame_and_no_window_space() {
        let h = harness(0x10_0000, 512, 4096);
        let free_before = h.frames.free_frames();

        let page = h.source.alloc_page().expect("a page");
        assert_eq!(
            h.frames.free_frames(),
            free_before - 1,
            "a slab page costs exactly one frame"
        );
        assert_eq!(
            page.as_ptr() as usize % PAGE_SIZE,
            0,
            "a slab page is granule-aligned"
        );
        assert!(
            h.window.page_index(page.as_ptr() as u64).is_none(),
            "a slab page is direct-mapped, never carved out of the remap window"
        );
        // The direct map is real storage in this harness, so the page is
        // genuinely writable.
        // SAFETY: the source owns the frame until the page is returned.
        unsafe { core::ptr::write_bytes(page.as_ptr(), 0x5A, PAGE_SIZE) };

        // The window is untouched, so a region still starts at its first slot.
        let (base, len) = h.source.grow(PAGE_SIZE).expect("grows");
        assert_eq!(
            h.window.page_index(base as u64),
            Some(0),
            "the page draw consumed no window slot"
        );
        h.source.shrink(base, len);

        h.source.free_page(page);
        assert_eq!(
            h.frames.free_frames(),
            free_before,
            "the page's frame came back"
        );
    }

    #[test]
    fn page_exhaustion_fails_closed_without_leaking() {
        let h = harness(0x10_0000, 8, 4096);
        let mut pages = Vec::new();
        while let Some(page) = h.source.alloc_page() {
            pages.push(page);
        }
        assert!(!pages.is_empty(), "the pool serves at least one page");
        assert_eq!(h.frames.free_frames(), 0, "the pool is drained");
        // Exhausted: `None`, never a panic.
        assert!(h.source.alloc_page().is_none());
        let drawn = pages.len();
        for page in pages {
            h.source.free_page(page);
        }
        assert_eq!(
            h.frames.free_frames(),
            drawn,
            "every page came back to the allocator"
        );
    }

    /// The invariant the whole design turns on: in production the heap being
    /// grown is the global heap, and its lock is not reentrant, so a single
    /// allocation from either path deadlocks. It binds the slab tier's page
    /// supply exactly as it binds region growth: both run under that lock.
    #[test]
    fn neither_grow_nor_shrink_allocates_from_the_global_heap() {
        let block_pages = 1usize << MAX_ORDER;
        // Large enough to force several chunks, a hole record, and more than
        // one teardown batch.
        let h = harness(0x100_0000, block_pages + 1024, 2 * block_pages);

        // Count this thread's allocations only, so the rest of the test
        // binary cannot perturb the measurement. `harness` has already
        // warmed the record arena, so the measured window covers
        // steady-state growth.
        let counter: &'static LiveBytes = Box::leak(Box::new(LiveBytes::new()));
        opt_in_current_thread(counter);
        let grown = h.source.grow((block_pages + 1) * PAGE_SIZE);
        let after_grow = counter.allocations();
        let region = grown.map(|(base, len)| (base as usize, len));
        if let Some((base, len)) = region {
            h.source.shrink(base as *mut u8, len);
        }
        let after_shrink = counter.allocations();
        opt_out_current_thread();

        assert!(region.is_some(), "the measured grow succeeded");
        assert_eq!(after_grow, 0, "grow allocated from the global heap");
        assert_eq!(after_shrink, 0, "shrink allocated from the global heap");
    }

    /// The same proof for the slab tier's page supply, which the heap drives
    /// from inside that very lock every time a size class needs a page.
    #[test]
    fn neither_page_draw_nor_page_release_allocates_from_the_global_heap() {
        let h = harness(0x10_0000, 512, 4096);

        let counter: &'static LiveBytes = Box::leak(Box::new(LiveBytes::new()));
        opt_in_current_thread(counter);
        let page = h.source.alloc_page();
        let after_draw = counter.allocations();
        if let Some(page) = page {
            h.source.free_page(page);
        }
        let after_release = counter.allocations();
        opt_out_current_thread();

        assert!(page.is_some(), "the measured draw succeeded");
        assert_eq!(after_draw, 0, "a page draw allocated from the global heap");
        assert_eq!(
            after_release, 0,
            "a page release allocated from the global heap"
        );
    }
}
