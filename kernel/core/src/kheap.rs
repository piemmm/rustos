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
//! The bin's allocator reaches the kernel core as a *handover value*, not
//! through a registry: every bin hands its `#[global_allocator]` to `boot`,
//! which carries it in [`crate::BootInfo`], and
//! [`install_frame_heap_source`] wires the frame-backed source into that
//! heap once the frame allocator, the arch direct physical map, and the
//! port's kernel remap window all exist and are `'static`. The compiler
//! therefore enforces the wiring: a bin cannot boot without naming its
//! heap, so no binary can silently run with an ungrowable one.
//!
//! The lock's interrupt-safety travels differently, and deliberately so: it
//! is installed straight into `tairix_kalloc` by the port's boot entry, so
//! it covers every heap in the binary rather than only the one the handover
//! names.
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
//! A port that reserves no remap window simply leaves the heap capped at
//! its bootstrap region (fail closed, never a panic).

use alloc::boxed::Box;
use core::ptr::NonNull;

use tairix_kalloc::{FreeListAllocator, HeapSource};
use tairix_kernel_mem::{
    back_run, release_run, FrameAllocator, FramePages, KernelVirtMap, PhysMap, SlotWindow,
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

impl HeapSource for FrameHeapSource {
    fn grow(&self, min_len: usize) -> Option<(*mut u8, usize)> {
        // The exact page count, floored at the amortised growth granule —
        // never rounded up to a power of two.
        let pages = min_len.div_ceil(PAGE_SIZE).max(MIN_GROW_PAGES);
        let len = pages.checked_mul(PAGE_SIZE)?;
        let window = self.kvmap.window();

        let mut slots = self.slots.lock();
        let slot = slots.allocate(pages).ok()?;
        let Some(region) = window.page_ptr(slot) else {
            let _ = slots.release(slot, pages);
            return None;
        };
        // The address the page tables take is read back off the pointer the
        // region is handed out as, so the mapped run and the run the heap
        // writes through cannot drift apart.
        let base = region.addr().get() as u64;
        if !back_run(self.kvmap, self.frames, base, pages) {
            // Fail closed leaking nothing: hand back every chunk that did
            // land and release the address space.
            release_run(self.kvmap, self.frames, base, pages);
            let _ = slots.release(slot, pages);
            return None;
        }
        Some((region.as_ptr(), len))
    }

    fn alloc_page(&self) -> Option<NonNull<u8>> {
        self.pages.alloc()
    }

    fn free_page(&self, page: NonNull<u8>) {
        self.pages.free(page);
    }

    fn shrink(&self, base: *mut u8, len: usize) {
        let base_addr = base.addr() as u64;
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
        release_run(self.kvmap, self.frames, base_addr, pages);
    }
}

/// Wire the frame-backed source into `heap` — the binary's
/// `#[global_allocator]`, carried here from the bin through
/// [`crate::BootInfo`] — so its byte-granular tier can grow past the
/// bootstrap region and its slab tier can draw pages.
///
/// Called once from the boot path after the frame allocator, the arch direct
/// physical map, and the port's kernel remap window all exist and are
/// `'static`. `physmap` backs the address-space bookkeeping's own
/// heap-independent record storage; `kvmap` is where the regions are
/// assembled. `window_pages` is the heap's share of the window — its low
/// pages, the kthread stack tier holding the rest
/// (`crate::kstack::install_kernel_stacks`) — so the two can never hand out
/// the same address. A window whose bookkeeping cannot be sized leaves the
/// heap on its bootstrap region, fail closed.
pub fn install_frame_heap_source(
    heap: &'static FreeListAllocator,
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
    kvmap: &'static dyn KernelVirtMap,
    window_pages: usize,
) {
    let Ok(slots) = SlotWindow::new(window_pages, frames, physmap) else {
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
        AccessTracking, AddressSpace as HalAddressSpace, KernelWindow, MapError, PageFlags,
    };
    use tairix_arch_api::tlb::TlbShootdown;
    use tairix_arch_api::CrossCpuTlbShootdown;
    use tairix_kernel_mem::MAX_ORDER;
    use tairix_kernel_mem::{
        BootMemoryMap, KernelRemap, MemoryClass, MemoryRegion, PhysAddr, RegionKind, SimPhysMap,
    };
    use tairix_sync::Once;

    /// Pages in the largest physically contiguous block the frame
    /// allocator can draw, which the assembly tests deliberately exceed.
    ///
    /// Interpreted, the same assembly path is driven at a fraction of the
    /// extent: a 32 MiB pool and a 64 MiB window per test is far beyond the
    /// interpreter's budget, and neither the chunk loop nor the pointer a
    /// region is handed out as varies with the chunk count. The native run
    /// keeps the full `MAX_ORDER` extent, so the "larger than one
    /// contiguous block" property is asserted where it is affordable.
    const LARGE_RUN_PAGES: usize = if cfg!(miri) { 64 } else { 1 << MAX_ORDER };

    /// Window pages a test asks for when it only needs room above its
    /// request. The window is real memory here, so its slack costs the
    /// interpreter a page each; what these tests assert is the behaviour at
    /// the low end of the window, never its extent.
    const ROOMY_WINDOW_PAGES: usize = if cfg!(miri) { 128 } else { 4096 };

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

        fn access_tracking(&self) -> AccessTracking {
            AccessTracking::Unsupported("the double models no referenced bit")
        }

        unsafe fn activate(&self) {}
    }

    impl TlbShootdown for WindowPageTable {
        fn flush_page(&mut self, _vaddr: u64) {}
    }

    /// A frame-backed growth source and the pieces it draws from.
    struct Harness {
        frames: &'static FrameAllocator,
        source: &'static FrameHeapSource,
        window: KernelWindow,
    }

    /// The `'static` pieces one harness borrows.
    ///
    /// `FrameAllocator`, `PhysMap` and `SlotWindow` are all reached through
    /// `&'static` in production, so a harness cannot own them on its stack.
    /// A leaked `Box` is indistinguishable from a real leak to the
    /// interpreter, so each piece lives in a cell instead, reachable for
    /// the whole run and accountable. One cell per [`harness!`] expansion,
    /// so no two concurrently-running tests share a frame pool.
    struct HarnessCell {
        /// Backs the remap window with memory the test owns, so the
        /// window's root is a real pointer rather than an address no
        /// interpreter could follow. One page of slack absorbs the
        /// alignment offset.
        arena: Once<Vec<u8>>,
        sim: Once<SimPhysMap>,
        frames: Once<FrameAllocator>,
        xtlb: Once<NoTlb>,
        kvmap: Once<KernelRemap<WindowPageTable>>,
        source: Once<FrameHeapSource>,
    }

    impl HarnessCell {
        const fn new() -> Self {
            Self {
                arena: Once::new(),
                sim: Once::new(),
                frames: Once::new(),
                xtlb: Once::new(),
                kvmap: Once::new(),
                source: Once::new(),
            }
        }

        /// Build the harness: `ram_pages` frames of usable RAM based at
        /// `ram_base`, a `window_pages` remap window, and an empty
        /// bootstrap heap so every allocation must grow.
        ///
        /// The window is real memory here, so a grown region is genuinely
        /// writable and the source's whole contract — extents, frame
        /// accounting, fail-closed behaviour, and the pointer it hands
        /// back — is checkable on the host. On the metal the same run is
        /// page-table-backed, which the QEMU verticals prove by booting.
        fn build(&'static self, ram_base: u64, ram_pages: usize, window_pages: usize) -> Harness {
            let arena = self
                .arena
                .call_once_infallible(|| alloc::vec![0u8; (window_pages + 1) * PAGE_SIZE])
                .expect("a fresh cell");
            let offset = arena.as_ptr().align_offset(PAGE_SIZE);
            let root = NonNull::new(arena.as_ptr().wrapping_add(offset).cast_mut())
                .expect("a live allocation is non-null");
            // SAFETY: `root` is the page-aligned base of `window_pages`
            // whole pages of `arena`, which this cell holds for the rest of
            // the run.
            let window = unsafe { KernelWindow::from_root(root, window_pages) }
                .expect("the arena backs a representable window");

            let sim = self
                .sim
                .call_once_infallible(|| {
                    SimPhysMap::new(PhysAddr::new(ram_base), ram_pages * PAGE_SIZE)
                })
                .expect("a fresh cell");
            let mut map = BootMemoryMap::new();
            map.push(MemoryRegion {
                start: PhysAddr::new(ram_base),
                length: (ram_pages * PAGE_SIZE) as u64,
                kind: RegionKind::Usable,
            });
            let frames = self
                .frames
                .call_once_infallible(|| FrameAllocator::new(&map).expect("allocator"))
                .expect("a fresh cell");
            let xtlb = self
                .xtlb
                .call_once_infallible(|| NoTlb)
                .expect("a fresh cell");
            let kvmap = self
                .kvmap
                .call_once_infallible(|| {
                    let table = WindowPageTable::new(window.base(), window_pages);
                    KernelRemap::new(window, table, xtlb)
                })
                .expect("a fresh cell");

            let slots = SlotWindow::new(window_pages, frames, sim).expect("non-empty window");
            let source = self
                .source
                .call_once_infallible(|| FrameHeapSource {
                    frames,
                    kvmap,
                    pages: FramePages::new(frames, sim),
                    slots: SpinLock::new(slots),
                })
                .expect("a fresh cell");

            let harness = Harness {
                frames,
                source,
                window: kvmap.window(),
            };
            harness.warm();
            harness
        }
    }

    /// Build a harness over a cell of this expansion's own, so each test
    /// gets an independent frame pool and window.
    macro_rules! harness {
        ($ram_base:expr, $ram_pages:expr, $window_pages:expr) => {{
            static CELL: HarnessCell = HarnessCell::new();
            CELL.build($ram_base, $ram_pages, $window_pages)
        }};
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
        while let Ok(frame) = frames.alloc(MemoryClass::Kernel) {
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
        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        let free_before = h.frames.free_frames();

        // 128 KiB — the empty bootstrap cannot satisfy it — forcing a grow.
        let (base, len) = h
            .source
            .grow(128 * 1024)
            .expect("grow satisfied the large request");
        assert!(len >= 128 * 1024);
        assert!(
            h.window.page_index(base.addr() as u64).is_some(),
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

    /// The pointer `grow` hands back addresses the run it mapped: the heap
    /// receives this region as its own memory and writes allocation headers
    /// straight into it, so a pointer that named anything else — or that
    /// carried no provenance for these bytes — would corrupt the free-list
    /// algebra silently.
    #[test]
    fn a_grown_region_is_writable_through_the_pointer_it_was_handed() {
        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        let (base, len) = h.source.grow(4 * PAGE_SIZE).expect("grows");

        // SAFETY: the source mapped `len` bytes at `base` and the region is
        // this test's until the `shrink` below returns it.
        let region = unsafe { core::slice::from_raw_parts_mut(base, len) };
        region.fill(0xA5);
        region[0] = 0x5A;
        region[len - 1] = 0x5A;
        assert_eq!(region[0], 0x5A);
        assert_eq!(region[len - 1], 0x5A);
        assert!(
            region[1..len - 1].iter().all(|byte| *byte == 0xA5),
            "the region's interior held the pattern written through it"
        );

        h.source.shrink(base, len);
    }

    #[test]
    fn grows_across_a_fragmented_pool_with_no_large_contiguous_block() {
        // Enough RAM that the request also exceeds one `MAX_ORDER` block,
        // so this covers the headline regression: a fragmented pool *and* a
        // region larger than the largest contiguous draw.
        let block_pages = LARGE_RUN_PAGES;
        let h = harness!(0x100_0000, block_pages + block_pages / 2, 2 * block_pages);
        fragment_pool(h.frames);
        let free_before = h.frames.free_frames();
        assert!(
            h.frames.alloc_order(MemoryClass::Kernel, 2).is_err(),
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
            let vaddr = base.addr() as u64 + (index * PAGE_SIZE) as u64;
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
        let block_pages = LARGE_RUN_PAGES;
        let pages = 2 * block_pages + 3;
        let h = harness!(0x100_0000, pages + 512, 4 * block_pages);
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
        let h = harness!(0x10_0000, 2048, ROOMY_WINDOW_PAGES);
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
        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        let (base, len) = h.source.grow(1).expect("grows");
        assert_eq!(len, MIN_GROW_PAGES * PAGE_SIZE);
        h.source.shrink(base, len);
    }

    #[test]
    fn true_exhaustion_fails_closed_without_leaking() {
        let h = harness!(0x10_0000, 64, ROOMY_WINDOW_PAGES);
        let free_before = h.frames.free_frames();
        // Far more pages than the pool holds: the partial fill must be
        // handed back whole.
        assert!(h.source.grow(ROOMY_WINDOW_PAGES * PAGE_SIZE).is_none());
        assert_eq!(
            h.frames.free_frames(),
            free_before,
            "a refused grow returns every frame it drew"
        );
        // And the address space is available again from the start.
        let (base, len) = h.source.grow(PAGE_SIZE).expect("a small grow still fits");
        assert_eq!(
            h.window.page_index(base.addr() as u64),
            Some(0),
            "the refused reservation was released"
        );
        h.source.shrink(base, len);
    }

    #[test]
    fn a_window_too_small_for_the_granule_fails_closed() {
        let h = harness!(0x10_0000, 512, MIN_GROW_PAGES - 1);
        let free_before = h.frames.free_frames();
        assert!(h.source.grow(1).is_none());
        assert_eq!(h.frames.free_frames(), free_before);
    }

    #[test]
    fn shrink_refuses_an_address_the_source_never_handed_out() {
        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        let (base, len) = h.source.grow(PAGE_SIZE).expect("grows");
        let free_after_grow = h.frames.free_frames();

        // Outside the window entirely.
        h.source
            .shrink(core::ptr::without_provenance_mut(PAGE_SIZE), len);
        // Inside the window but not a run this source reserved.
        h.source.shrink(base.wrapping_add(len), len);
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
        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        let (first, len) = h.source.grow(PAGE_SIZE).expect("grows");
        h.source.shrink(first, len);
        let (second, len2) = h.source.grow(PAGE_SIZE).expect("grows again");
        assert_eq!(first, second, "the released run was handed out again");
        h.source.shrink(second, len2);
    }

    #[test]
    fn a_slab_page_costs_one_frame_and_no_window_space() {
        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        let free_before = h.frames.free_frames();

        let page = h.source.alloc_page().expect("a page");
        assert_eq!(
            h.frames.free_frames(),
            free_before - 1,
            "a slab page costs exactly one frame"
        );
        assert_eq!(
            page.addr().get() % PAGE_SIZE,
            0,
            "a slab page is granule-aligned"
        );
        assert!(
            h.window.page_index(page.addr().get() as u64).is_none(),
            "a slab page is direct-mapped, never carved out of the remap window"
        );
        // The direct map is real storage in this harness, so the page is
        // genuinely writable.
        // SAFETY: the source owns the frame until the page is returned.
        unsafe { core::ptr::write_bytes(page.as_ptr(), 0x5A, PAGE_SIZE) };

        // The window is untouched, so a region still starts at its first slot.
        let (base, len) = h.source.grow(PAGE_SIZE).expect("grows");
        assert_eq!(
            h.window.page_index(base.addr() as u64),
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
        let h = harness!(0x10_0000, 8, ROOMY_WINDOW_PAGES);
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
        // Counts this thread's allocations only, so the rest of the test
        // binary cannot perturb the measurement.
        static COUNTER: LiveBytes = LiveBytes::new();

        let block_pages = LARGE_RUN_PAGES;
        // Large enough to force several chunks, a hole record, and more than
        // one teardown batch.
        let h = harness!(0x100_0000, block_pages + 1024, 2 * block_pages);

        // The harness has already warmed the record arena, so the measured
        // window covers steady-state growth.
        opt_in_current_thread(&COUNTER);
        let grown = h.source.grow((block_pages + 1) * PAGE_SIZE);
        let after_grow = COUNTER.allocations();
        if let Some((base, len)) = grown {
            h.source.shrink(base, len);
        }
        let after_shrink = COUNTER.allocations();
        opt_out_current_thread();

        assert!(grown.is_some(), "the measured grow succeeded");
        assert_eq!(after_grow, 0, "grow allocated from the global heap");
        assert_eq!(after_shrink, 0, "shrink allocated from the global heap");
    }

    /// The same proof for the slab tier's page supply, which the heap drives
    /// from inside that very lock every time a size class needs a page.
    #[test]
    fn neither_page_draw_nor_page_release_allocates_from_the_global_heap() {
        static COUNTER: LiveBytes = LiveBytes::new();

        let h = harness!(0x10_0000, 512, ROOMY_WINDOW_PAGES);
        opt_in_current_thread(&COUNTER);
        let page = h.source.alloc_page();
        let after_draw = COUNTER.allocations();
        if let Some(page) = page {
            h.source.free_page(page);
        }
        let after_release = COUNTER.allocations();
        opt_out_current_thread();

        assert!(page.is_some(), "the measured draw succeeded");
        assert_eq!(after_draw, 0, "a page draw allocated from the global heap");
        assert_eq!(
            after_release, 0,
            "a page release allocated from the global heap"
        );
    }
}
