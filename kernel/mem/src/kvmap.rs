//! Kernel virtual remapping — assembling a contiguous kernel window out of
//! scattered physical chunks.
//!
//! The kernel heap grows by adding whole regions, and a single allocation
//! must fit inside one region, so growth needs a *virtually* contiguous
//! run. Drawing it as one physically contiguous block welds the largest
//! serviceable allocation to the buddy allocator's maximum contiguity order
//! and makes growth fail on a fragmented pool while gigabytes are still
//! free. Mapping several `<= MAX_ORDER` chunks into one virtual window
//! removes both limits: growth then succeeds whenever the *total* free
//! frame count suffices, in any physical layout.
//!
//! # Why this needs the port's help
//!
//! Kernel code runs with the *current task's* translation root active, so a
//! kernel address must resolve identically under every root. A port
//! therefore reserves a [`KernelWindow`] by pointing the covering
//! top-level entry of every root it builds at one shared sub-hierarchy;
//! installing a leaf in that sub-hierarchy — which is what this module does
//! — makes it visible everywhere at once.
//!
//! # The re-entrancy rule
//!
//! Every path here runs inside the kernel heap's own growth source (the
//! `tairix_kalloc` `HeapSource` `grow`/`shrink` pair), under that heap's
//! non-reentrant lock, and must therefore allocate nothing from it. The only allocation it
//! makes is a page-table frame, drawn from the physical
//! [`crate::FrameAllocator`] through
//! [`crate::pagetables::FrameTableSource`] — heap-independent by
//! construction. Nothing is boxed, no side table records what was mapped:
//! the page tables *are* the record, and teardown recovers each frame by
//! walking them.

use tairix_arch_api::mmu::{KernelWindow, MapError, PageFlags};
use tairix_arch_api::CrossCpuTlbShootdown;
use tairix_sync::SpinLock;

use crate::frame::{Frame, PhysAddr, PAGE_SIZE};
use crate::vmm::PageTable;

/// Leaf permissions every remapped page carries: readable and writable,
/// kernel-only, never executable. The window backs kernel *data*, so W^X
/// holds by construction and no caller can ask for anything else.
const WINDOW_FLAGS: PageFlags = PageFlags::READ.union(PageFlags::WRITE);

/// Pages torn down between consecutive TLB synchronisation boundaries.
///
/// Teardown must invalidate every CPU's cached translation *before* a
/// recovered frame is reallocated, or a stale entry would alias freed
/// memory. Doing that per page costs one system-wide synchronisation per
/// page — on a port whose shootdown is an inter-processor round-trip that
/// is thousands of round-trips for one large region. Batching amortises the
/// boundary while keeping the recovered frames in a small fixed stack
/// buffer: this is a batching granule, not a limit on how much may be torn
/// down, so an arbitrarily long run is simply processed in more batches.
const TEARDOWN_BATCH: usize = 64;

/// Why a remap operation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemapError {
    /// The request was for zero pages.
    ZeroLength,
    /// The virtual or physical address was not page-aligned.
    Misaligned,
    /// The run does not lie wholly inside the window, or its extent
    /// overflows the address space.
    OutsideWindow,
    /// The port refused a leaf: the page-table frame source is exhausted,
    /// the address is already mapped, or the flags are unrepresentable.
    Map(MapError),
}

/// Installing and tearing down leaves in the kernel remap window.
///
/// Object-safe and [`Sync`] so the kernel heap can hold one behind a
/// `&'static dyn KernelVirtMap` and drive it from any CPU.
pub trait KernelVirtMap: Sync {
    /// The window this map owns.
    fn window(&self) -> KernelWindow;

    /// Map `pages` consecutive window pages starting at `vaddr` onto the
    /// `pages` consecutive frames starting at `frame`, readable and
    /// writable to the kernel only.
    ///
    /// The whole chunk lands or none of it does: a refused leaf tears down
    /// the leaves this call already installed before returning.
    ///
    /// # Errors
    ///
    /// [`RemapError`] for a zero, misaligned, or out-of-window request, or
    /// for the port's refusal of a leaf.
    fn map_chunk(&self, vaddr: u64, frame: Frame, pages: usize) -> Result<(), RemapError>;

    /// Tear down every mapped page of the run `[vaddr, vaddr + pages)`,
    /// handing each recovered frame to `recovered`, and return how many
    /// were recovered.
    ///
    /// A frame is handed over only after the invalidation covering it is
    /// globally visible, so the caller may free it immediately without
    /// leaving a stale translation aliasing reallocated memory. An
    /// unmapped page inside the run is skipped, not an error: the run's
    /// mapped extent is whatever the page tables say it is.
    fn unmap_run(&self, vaddr: u64, pages: usize, recovered: &mut dyn FnMut(Frame)) -> usize;

    /// The frame backing the window page at `vaddr`, or `None` when it has
    /// no live leaf.
    fn translate(&self, vaddr: u64) -> Option<Frame>;
}

/// The one implementation of [`KernelVirtMap`], generic over the port's
/// page-table backend.
///
/// A port supplies a handle onto the window's shared sub-hierarchy — an
/// address space whose only mapped region *is* the window — plus its
/// cross-CPU invalidation. Everything above that is architecture-neutral
/// and lives here once, so no port re-derives the map/teardown discipline
/// (`plans/FIX-KHEAP.md`).
pub struct KernelRemap<P: PageTable + Send> {
    window: KernelWindow,
    /// The window's sub-hierarchy. Locked rather than borrowed because the
    /// heap drives it from any CPU through a shared reference; the only
    /// caller already holds the global heap lock, so the critical section
    /// is uncontended by construction.
    space: SpinLock<P>,
    xtlb: &'static (dyn CrossCpuTlbShootdown + Sync),
}

impl<P: PageTable + Send> KernelRemap<P> {
    /// Wrap `space` — a handle onto `window`'s shared sub-hierarchy — with
    /// the port's cross-CPU invalidation.
    pub fn new(
        window: KernelWindow,
        space: P,
        xtlb: &'static (dyn CrossCpuTlbShootdown + Sync),
    ) -> Self {
        Self {
            window,
            space: SpinLock::new(space),
            xtlb,
        }
    }

    /// Validate that `[vaddr, vaddr + pages)` is a non-empty, page-aligned
    /// run inside the window.
    fn check_run(&self, vaddr: u64, pages: usize) -> Result<(), RemapError> {
        if pages == 0 {
            return Err(RemapError::ZeroLength);
        }
        if vaddr & (PAGE_SIZE as u64 - 1) != 0 {
            return Err(RemapError::Misaligned);
        }
        let span = (pages as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(RemapError::OutsideWindow)?;
        let last = vaddr
            .checked_add(span - 1)
            .ok_or(RemapError::OutsideWindow)?;
        if !self.window.contains(vaddr) || !self.window.contains(last) {
            return Err(RemapError::OutsideWindow);
        }
        Ok(())
    }
}

impl<P: PageTable + Send> KernelVirtMap for KernelRemap<P> {
    fn window(&self) -> KernelWindow {
        self.window
    }

    fn map_chunk(&self, vaddr: u64, frame: Frame, pages: usize) -> Result<(), RemapError> {
        self.check_run(vaddr, pages)?;
        // The frames must exist as one block; refuse an index range that
        // would wrap rather than mapping a wrapped-around frame.
        frame
            .0
            .checked_add(pages - 1)
            .ok_or(RemapError::OutsideWindow)?;

        let mut space = self.space.lock();
        for index in 0..pages {
            let offset = index as u64 * PAGE_SIZE as u64;
            let paddr = Frame(frame.0 + index).start().as_u64();
            if let Err(err) = space.map_page(vaddr + offset, paddr, WINDOW_FLAGS) {
                // Undo this call's own leaves so a refused chunk leaves the
                // window exactly as it was found.
                for undone in 0..index {
                    let _ = space.unmap(vaddr + undone as u64 * PAGE_SIZE as u64);
                }
                if index != 0 {
                    self.xtlb.shootdown_range(vaddr, index);
                }
                return Err(RemapError::Map(err));
            }
        }
        // An invalid-to-valid leaf cannot be stale in any CPU's TLB, so what
        // is owed is ordering, not invalidation — and, on a port whose ISA
        // never caches an absent entry, no cross-CPU work at all: a CPU
        // that faults on one of the new leaves walks the tables and finds
        // it. Invalidating here instead cost a whole-domain TLB broadcast
        // per chunk on aarch64, which is what made a fragmented pool's
        // many-chunk growth ruinous.
        space.publish_mappings(vaddr, pages);
        // The window is one shared sub-hierarchy every root installs, so a
        // leaf added here is reachable from every CPU at once — and a port
        // that may have cached the absence needs each of them fenced, not
        // just the publisher. Ports that do not declare it pay nothing.
        if self.xtlb.publish_needs_remote() {
            self.xtlb.shootdown_range(vaddr, pages);
        }
        Ok(())
    }

    fn unmap_run(&self, vaddr: u64, pages: usize, recovered: &mut dyn FnMut(Frame)) -> usize {
        if self.check_run(vaddr, pages).is_err() {
            return 0;
        }
        let mut space = self.space.lock();
        let mut total = 0;
        let mut done = 0;
        while done < pages {
            let batch = core::cmp::min(TEARDOWN_BATCH, pages - done);
            let batch_base = vaddr + done as u64 * PAGE_SIZE as u64;
            let mut frames = [Frame(0); TEARDOWN_BATCH];
            let mut found = 0;
            for index in 0..batch {
                let page = batch_base + index as u64 * PAGE_SIZE as u64;
                if let Ok(paddr) = space.unmap(page) {
                    frames[found] = Frame::containing(PhysAddr::new(paddr));
                    found += 1;
                }
            }
            // Every CPU must have forgotten these translations before a
            // recovered frame can be handed back for reuse — and only then;
            // a batch that tore down no leaf left nothing stale to discard.
            if found != 0 {
                self.xtlb.shootdown_range(batch_base, batch);
            }
            for frame in &frames[..found] {
                recovered(*frame);
            }
            total += found;
            done += batch;
        }
        total
    }

    fn translate(&self, vaddr: u64) -> Option<Frame> {
        if !self.window.contains(vaddr) {
            return None;
        }
        let (paddr, _) = self.space.lock().translate(vaddr)?;
        Some(Frame::containing(PhysAddr::new(paddr)))
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::vmm::HostPageTable;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tairix_arch_api::mmu::AddressSpace as HalAddressSpace;

    const WINDOW_BASE: u64 = 0x80_0000_0000;
    const WINDOW_PAGES: usize = 64;
    const PAGE: u64 = PAGE_SIZE as u64;

    /// Records the ranges the port was asked to invalidate, so the tests can
    /// prove teardown synchronises before a frame is handed back — and
    /// declares whether an installation needs the cross-CPU publish, so both
    /// port postures are exercised on the host.
    #[derive(Default)]
    struct CountingXtlb {
        pages: AtomicUsize,
        calls: AtomicUsize,
        publish_remote: bool,
    }

    impl CrossCpuTlbShootdown for CountingXtlb {
        fn shootdown_page(&self, _vaddr: u64) {
            self.pages.fetch_add(1, Ordering::Relaxed);
            self.calls.fetch_add(1, Ordering::Relaxed);
        }

        fn shootdown_range(&self, _start_vaddr: u64, page_count: usize) {
            self.pages.fetch_add(page_count, Ordering::Relaxed);
            self.calls.fetch_add(1, Ordering::Relaxed);
        }

        fn publish_needs_remote(&self) -> bool {
            self.publish_remote
        }
    }

    fn remap(pages: usize) -> (KernelRemap<HostPageTable>, &'static CountingXtlb) {
        remap_with_publish(pages, false)
    }

    /// [`remap`] over a port that declares whether an installation needs
    /// the cross-CPU publish.
    fn remap_with_publish(
        pages: usize,
        publish_remote: bool,
    ) -> (KernelRemap<HostPageTable>, &'static CountingXtlb) {
        let xtlb: &'static CountingXtlb =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(CountingXtlb {
                publish_remote,
                ..CountingXtlb::default()
            }));
        let window = KernelWindow::new(WINDOW_BASE, pages).expect("valid window");
        (KernelRemap::new(window, HostPageTable::new(), xtlb), xtlb)
    }

    #[test]
    fn an_installation_reaches_every_cpu_where_the_port_declares_it_must() {
        // A port whose ISA may cache the *absence* leaves a peer faulting
        // forever on a leaf the tables plainly hold, so the window's shared
        // sub-hierarchy owes it a fence over exactly the installed run.
        let (map, xtlb) = remap_with_publish(WINDOW_PAGES, true);
        map.map_chunk(WINDOW_BASE, Frame(0x200), 4).expect("maps");
        assert_eq!(xtlb.calls.load(Ordering::Relaxed), 1);
        assert_eq!(xtlb.pages.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn a_refused_installation_publishes_nothing_remotely() {
        // The undo path already synchronises the leaves it withdrew; a
        // failed chunk must not additionally publish a run it did not
        // install.
        let (map, xtlb) = remap_with_publish(WINDOW_PAGES, true);
        map.map_chunk(WINDOW_BASE, Frame(1), 1).expect("maps");
        let before = xtlb.calls.load(Ordering::Relaxed);
        map.map_chunk(WINDOW_BASE, Frame(2), 1)
            .expect_err("already mapped");
        assert_eq!(xtlb.calls.load(Ordering::Relaxed), before);
    }

    #[test]
    fn a_chunk_maps_consecutive_pages_onto_consecutive_frames() {
        let (map, _xtlb) = remap(WINDOW_PAGES);
        map.map_chunk(WINDOW_BASE, Frame(0x200), 4).expect("maps");
        for index in 0..4usize {
            assert_eq!(
                map.translate(WINDOW_BASE + index as u64 * PAGE),
                Some(Frame(0x200 + index))
            );
        }
        assert_eq!(map.translate(WINDOW_BASE + 4 * PAGE), None);
    }

    #[test]
    fn window_pages_are_writable_and_never_executable() {
        let (map, _xtlb) = remap(WINDOW_PAGES);
        map.map_chunk(WINDOW_BASE, Frame(1), 1).expect("maps");
        let (_, flags) = map
            .space
            .lock()
            .translate(WINDOW_BASE)
            .expect("a live leaf");
        assert!(flags.contains(PageFlags::READ));
        assert!(flags.contains(PageFlags::WRITE));
        assert!(!flags.contains(PageFlags::EXEC), "W^X");
        assert!(!flags.contains(PageFlags::USER), "kernel-only");
    }

    #[test]
    fn a_request_outside_the_window_is_refused() {
        let (map, _xtlb) = remap(4);
        let top = WINDOW_BASE + 4 * PAGE;
        assert_eq!(
            map.map_chunk(top, Frame(1), 1),
            Err(RemapError::OutsideWindow)
        );
        assert_eq!(
            map.map_chunk(WINDOW_BASE, Frame(1), 5),
            Err(RemapError::OutsideWindow),
            "a run overhanging the top is refused whole"
        );
        assert_eq!(
            map.map_chunk(WINDOW_BASE - PAGE, Frame(1), 1),
            Err(RemapError::OutsideWindow)
        );
        assert_eq!(
            map.map_chunk(WINDOW_BASE + 1, Frame(1), 1),
            Err(RemapError::Misaligned)
        );
        assert_eq!(
            map.map_chunk(WINDOW_BASE, Frame(1), 0),
            Err(RemapError::ZeroLength)
        );
        assert_eq!(map.translate(WINDOW_BASE), None, "nothing was mapped");
    }

    #[test]
    fn a_refused_leaf_rolls_the_whole_chunk_back() {
        let (map, _xtlb) = remap(WINDOW_PAGES);
        // Occupy the third page so the chunk's third leaf is refused.
        map.map_chunk(WINDOW_BASE + 2 * PAGE, Frame(0x900), 1)
            .expect("maps");

        assert_eq!(
            map.map_chunk(WINDOW_BASE, Frame(0x100), 4),
            Err(RemapError::Map(MapError::AlreadyMapped))
        );
        assert_eq!(map.translate(WINDOW_BASE), None);
        assert_eq!(map.translate(WINDOW_BASE + PAGE), None);
        assert_eq!(
            map.translate(WINDOW_BASE + 2 * PAGE),
            Some(Frame(0x900)),
            "the pre-existing leaf survived the rollback"
        );
        assert_eq!(map.translate(WINDOW_BASE + 3 * PAGE), None);
    }

    #[test]
    fn teardown_recovers_every_frame_of_a_multi_chunk_run() {
        let (map, _xtlb) = remap(WINDOW_PAGES);
        // Three chunks of different sizes, as growth from a fragmented pool
        // produces.
        map.map_chunk(WINDOW_BASE, Frame(0x40), 8).expect("maps");
        map.map_chunk(WINDOW_BASE + 8 * PAGE, Frame(0x300), 4)
            .expect("maps");
        map.map_chunk(WINDOW_BASE + 12 * PAGE, Frame(0x11), 2)
            .expect("maps");

        let mut recovered = Vec::new();
        let count = map.unmap_run(WINDOW_BASE, 14, &mut |frame| recovered.push(frame));
        assert_eq!(count, 14);
        let expected: Vec<Frame> = (0x40..0x48)
            .chain(0x300..0x304)
            .chain(0x11..0x13)
            .map(Frame)
            .collect();
        assert_eq!(recovered, expected);
        for index in 0..14 {
            assert_eq!(map.translate(WINDOW_BASE + index * PAGE), None);
        }
    }

    #[test]
    fn teardown_synchronises_every_page_it_recovers() {
        let (map, xtlb) = remap(WINDOW_PAGES);
        map.map_chunk(WINDOW_BASE, Frame(0x40), WINDOW_PAGES)
            .expect("maps");
        let before = xtlb.pages.load(Ordering::Relaxed);

        let mut recovered = 0;
        map.unmap_run(WINDOW_BASE, WINDOW_PAGES, &mut |_| recovered += 1);
        assert_eq!(recovered, WINDOW_PAGES);
        assert_eq!(
            xtlb.pages.load(Ordering::Relaxed) - before,
            WINDOW_PAGES,
            "every torn-down page was invalidated system-wide"
        );
    }

    #[test]
    fn mapping_publishes_without_invalidating_anything() {
        // A not-present-to-present leaf is never stale, so installing one
        // must cost no cross-CPU invalidation on a port that declares none:
        // doing it anyway made a many-chunk growth issue one whole-domain
        // TLB broadcast per chunk.
        let (map, xtlb) = remap(WINDOW_PAGES);
        map.map_chunk(WINDOW_BASE, Frame(0x40), 8).expect("maps");
        map.map_chunk(WINDOW_BASE + 8 * PAGE, Frame(0x300), 8)
            .expect("maps");
        assert_eq!(
            xtlb.calls.load(Ordering::Relaxed),
            0,
            "installing leaves issued a system-wide invalidation"
        );
    }

    #[test]
    fn tearing_down_an_unmapped_run_invalidates_nothing() {
        let (map, xtlb) = remap(WINDOW_PAGES);
        let calls_before = xtlb.calls.load(Ordering::Relaxed);
        assert_eq!(map.unmap_run(WINDOW_BASE, 8, &mut |_| {}), 0);
        assert_eq!(
            xtlb.calls.load(Ordering::Relaxed),
            calls_before,
            "a batch that tore down no leaf owes no invalidation"
        );
    }

    #[test]
    fn teardown_batches_the_synchronisation_boundary() {
        // A run longer than one batch must still pay far fewer boundaries
        // than it has pages.
        let pages = TEARDOWN_BATCH * 3;
        let (map, xtlb) = remap(pages);
        map.map_chunk(WINDOW_BASE, Frame(0x1000), pages)
            .expect("maps");
        let calls_before = xtlb.calls.load(Ordering::Relaxed);

        map.unmap_run(WINDOW_BASE, pages, &mut |_| {});
        assert_eq!(
            xtlb.calls.load(Ordering::Relaxed) - calls_before,
            3,
            "one boundary per batch, not one per page"
        );
    }

    #[test]
    fn teardown_skips_unmapped_pages_without_fabricating_a_frame() {
        let (map, _xtlb) = remap(WINDOW_PAGES);
        map.map_chunk(WINDOW_BASE, Frame(7), 1).expect("maps");
        map.map_chunk(WINDOW_BASE + 3 * PAGE, Frame(9), 1)
            .expect("maps");

        let mut recovered = Vec::new();
        let count = map.unmap_run(WINDOW_BASE, 4, &mut |frame| recovered.push(frame));
        assert_eq!(count, 2);
        assert_eq!(recovered, alloc::vec![Frame(7), Frame(9)]);
    }

    #[test]
    fn teardown_outside_the_window_recovers_nothing() {
        let (map, xtlb) = remap(4);
        let calls_before = xtlb.calls.load(Ordering::Relaxed);
        let mut recovered = 0;
        assert_eq!(
            map.unmap_run(WINDOW_BASE + 4 * PAGE, 1, &mut |_| recovered += 1),
            0
        );
        assert_eq!(map.unmap_run(WINDOW_BASE, 0, &mut |_| recovered += 1), 0);
        assert_eq!(recovered, 0);
        assert_eq!(
            xtlb.calls.load(Ordering::Relaxed),
            calls_before,
            "a refused teardown touches nothing"
        );
    }

    #[test]
    fn translate_names_only_window_addresses() {
        let (map, _xtlb) = remap(4);
        map.map_chunk(WINDOW_BASE, Frame(5), 1).expect("maps");
        assert_eq!(map.translate(WINDOW_BASE), Some(Frame(5)));
        assert_eq!(map.translate(WINDOW_BASE - PAGE), None);
        assert_eq!(map.translate(WINDOW_BASE + 4 * PAGE), None);
        assert_eq!(map.window().pages(), 4);
    }

    #[test]
    fn the_map_is_shareable_across_cpus() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<KernelRemap<HostPageTable>>();
        let (map, _xtlb) = remap(4);
        let erased: &dyn KernelVirtMap = &map;
        assert_eq!(erased.window().base(), WINDOW_BASE);
    }
}
