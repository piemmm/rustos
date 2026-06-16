//! Retained live user address space — the production target the
//! `mem_map` / `mmio_map` syscall producers mutate (`plans/PI.md` P10
//! chunk 5d-0-ii (b′); the `plans/SPAWN.md` `SP5b` production follow-on).
//!
//! Post-spawn an address space was previously captured only as an immutable
//! [`FrozenAddressSpace`](crate::vmm::FrozenAddressSpace) snapshot — enough
//! for the read-only user-memory copy path, but the live arch
//! [`AddressSpace<P>`] was *dropped*. `mem_map` / `mmio_map` need the
//! *running* space to stay **mutable** so a process can grow its own heap or
//! a driver can map a granted device window into its own address space.
//!
//! This module is that retained, mutable space, behind one object-safe
//! boundary so `kernel/core` can hold it without naming a concrete
//! page-table backend `P` (`AGENTS.md` §17.4):
//!
//! * [`LiveUserSpace`] — the object-safe, mutating operations the producers
//!   reach (anonymous map/unmap; device-window map). `Send` so the boxed
//!   space can be **owned by the kernel thread that runs the task**; it is
//!   deliberately **not** `Sync` and is reached only by the single CPU
//!   currently running the task, from that task's own synchronous syscall
//!   path, so the `&mut` its methods take can never alias — the same
//!   exclusivity the task's kernel stack already relies on (`AGENTS.md`
//!   §4 — the access is genuinely exclusive).
//! * [`LiveSpace`] — the generic concrete implementation over a port's
//!   [`PageTable`] backend `P`, the kernel direct map `M`, the kernel
//!   [`FrameAllocator`] (anonymous frames), and an [`MmioWindowMap`]
//!   (device windows). It composes the already-audited
//!   [`map_anonymous`] / [`unmap_anonymous`] and
//!   [`MmioWindowMap`] mechanisms — there is no second mapping path
//!   (`AGENTS.md` §2.2 — one definition each).
//!
//! The capability posture (`mem_map` is unprivileged, the `mmio_map` grant
//! is owner-checked, `AGENTS.md` §5.4 / §18.3) and the placement of a
//! non-`FIXED` anonymous region belong to the higher-level `kernel/core`
//! producer that calls these — this layer knows only the page table, the
//! direct map, the frame allocator, and the device-window allocator
//! (`AGENTS.md` §17.4).

use crate::anon::{map_anonymous, unmap_anonymous, AnonError};
use crate::anon_window::AnonWindowMap;
use crate::frame::FrameAllocator;
use crate::mmio::{MmioError, MmioWindowMap};
use crate::phys::PhysMap;
use crate::vmm::{AddressSpace, PageTable, VirtAddr};

/// Why a [`LiveUserSpace`] operation failed.
///
/// A faithful union of the two underlying mechanisms' errors so the
/// `kernel/core` producer can fold each onto a stable `Errno` without this
/// layer knowing the ABI error type (`AGENTS.md` §17.4 — `kernel/mem` names
/// no `lib/abi` error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LiveSpaceError {
    /// An anonymous map/unmap failed (OOM, not-mapped, misaligned, …).
    Anon(AnonError),
    /// A device-window map failed (no virtual slot, page-table refusal, …).
    Mmio(MmioError),
}

impl From<AnonError> for LiveSpaceError {
    fn from(err: AnonError) -> Self {
        Self::Anon(err)
    }
}

impl From<MmioError> for LiveSpaceError {
    fn from(err: MmioError) -> Self {
        Self::Mmio(err)
    }
}

/// The object-safe, mutating view of a task's retained live address space.
///
/// Held by `kernel/core` as a `Box<dyn LiveUserSpace + Send>` owned by the
/// task's kernel thread, reached only by the CPU currently running the task
/// from that task's syscall path (see the module docs for the exclusivity
/// argument). Every method takes `&mut self`; the producer that calls them
/// guarantees the access is exclusive.
pub trait LiveUserSpace: Send {
    /// Map `page_count` fresh, zeroed `RW|USER` pages at the page-aligned
    /// `base_va` into this space, returning `base_va` on success.
    ///
    /// The placement (the value of `base_va`) is the producer's decision;
    /// this is the `FIXED` map mechanism. The pages are zeroed before they
    /// are user-visible and are never executable (`AGENTS.md` §4 / §19.2);
    /// a part-way failure unwinds every page already mapped, leaving the
    /// space unchanged (`AGENTS.md` §2.9).
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] carrying the precise
    /// [`AnonError`] (zero length, misalignment,
    /// overflow, OOM, …).
    fn map_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<u64, LiveSpaceError>;

    /// Map `page_count` fresh, zeroed `RW|USER` pages at a **kernel-chosen**
    /// base — the non-`FIXED` `mem_map` placement (`plans/PI.md` 5d-0-ii (c))
    /// — returning the base virtual address the space placed them at.
    ///
    /// The placement is drawn from this task's anonymous heap window so a
    /// second non-`FIXED` request never overlaps the first, the program
    /// image, its stack, or a granted device window. The pages obey the same
    /// W^X / zero-on-map / all-or-nothing contract as [`Self::map_anonymous`];
    /// the reserved range is released back to the window on a mapping failure
    /// so a failed call consumes no address space (`AGENTS.md` §2.9).
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::OutOfMemory`] when the heap
    /// window or the frame allocator is exhausted (deterministic OOM, §4),
    /// or the precise placement/map error otherwise.
    fn map_anonymous_placed(&mut self, page_count: u64) -> Result<u64, LiveSpaceError>;

    /// Release the `page_count`-page region based at `base_va`, zeroing
    /// every frame before it is returned to the allocator (`AGENTS.md` §4 —
    /// zero on free). The whole range is validated mapped before any page is
    /// torn down (`AGENTS.md` §5.4 — fail closed on a bad range).
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] (e.g. [`AnonError::NotMapped`] when the
    /// range is not one this space mapped).
    fn unmap_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<(), LiveSpaceError>;

    /// Map `len` bytes of device physical memory beginning at `phys_base`
    /// into this space, returning the kernel-chosen base user virtual
    /// address of the new, guard-bracketed, caching-disabled,
    /// non-executable window (`AGENTS.md` §4 / §19.2).
    ///
    /// The producer has already resolved and validated the grant the window
    /// comes from (owner-checked, kind, length — `AGENTS.md` §5.4 / §18.3);
    /// this only performs the page-table mechanism.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] carrying the precise
    /// [`MmioError`] (no free virtual slot,
    /// page-table refusal, …).
    fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError>;
}

/// The generic concrete live address space retained per task.
///
/// Generic over the port's [`PageTable`] backend `P` and the kernel direct
/// map `M`, so it is constructed by the architecture spawn producer (which
/// names both) and stored behind [`LiveUserSpace`] by `kernel/core` (which
/// names neither, `AGENTS.md` §17.4). It owns:
///
/// * `space` — the live arch [`AddressSpace<P>`] (its page-table frames come
///   from the backend's own [`PageTableFrames`](rustos_arch_api::frames::PageTableFrames)
///   source, wired at spawn);
/// * `physmap` — the kernel direct map used to zero anonymous frames on map
///   and on free;
/// * `frames` — the kernel [`FrameAllocator`] anonymous pages are drawn from
///   and returned to;
/// * `mmio` — the per-task guarded device-window virtual-address allocator;
/// * `anon` — the per-task placement allocator that chooses the base for a
///   non-`FIXED` anonymous mapping out of this task's heap window.
pub struct LiveSpace<P: PageTable, M: PhysMap> {
    space: AddressSpace<P>,
    physmap: M,
    frames: &'static FrameAllocator,
    mmio: MmioWindowMap,
    anon: AnonWindowMap,
}

impl<P: PageTable, M: PhysMap> LiveSpace<P, M> {
    /// Retain `space` as a live, mutable user address space.
    ///
    /// `physmap` is the kernel direct map (used to zero anonymous frames),
    /// `frames` the allocator anonymous pages come from,
    /// `[mmio_window_base, mmio_window_base + mmio_window_pages * PAGE_SIZE)`
    /// the virtual range device windows are mapped into (guard-bracketed by
    /// [`MmioWindowMap`]), and
    /// `[anon_window_base, anon_window_base + anon_window_pages * PAGE_SIZE)`
    /// the range non-`FIXED` anonymous mappings are placed into
    /// ([`AnonWindowMap`]). Both windows must lie in the task's own free user
    /// virtual space, clear of each other, its image, and its stack.
    ///
    /// # Errors
    ///
    /// [`MmioError::InvalidMapConfig`] if either window is misconfigured
    /// (zero pages, a base that is not page-aligned, or a size that
    /// overflows the address space).
    pub fn new(
        space: AddressSpace<P>,
        physmap: M,
        frames: &'static FrameAllocator,
        mmio_window_base: VirtAddr,
        mmio_window_pages: usize,
        anon_window_base: VirtAddr,
        anon_window_pages: usize,
    ) -> Result<Self, MmioError> {
        let mmio = MmioWindowMap::new(mmio_window_base, mmio_window_pages)?;
        // An anonymous-heap-window config error is the same class of fault as
        // an MMIO-window one; fold it onto `InvalidMapConfig` so the spawn
        // producer has one constructor error to handle (`AGENTS.md` §2.9).
        let anon = AnonWindowMap::new(anon_window_base, anon_window_pages)
            .map_err(|_| MmioError::InvalidMapConfig)?;
        Ok(Self {
            space,
            physmap,
            frames,
            mmio,
            anon,
        })
    }

    /// Borrow the underlying live address space (e.g. to freeze a fresh
    /// snapshot for the read-only copy path after a mutation).
    #[must_use]
    pub fn space(&self) -> &AddressSpace<P> {
        &self.space
    }
}

impl<P, M> LiveUserSpace for LiveSpace<P, M>
where
    P: PageTable + Send,
    M: PhysMap + Send,
{
    fn map_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<u64, LiveSpaceError> {
        // Copy the `'static` allocator handle out so the alloc/free closures
        // do not borrow `self` while `map_anonymous` borrows `self.space`
        // and `self.physmap`.
        let frames = self.frames;
        map_anonymous(
            &mut self.space,
            &self.physmap,
            base_va,
            page_count,
            || frames.alloc().ok(),
            |frame| {
                // The frame was just unwound from this space; the matching
                // free of a frame the allocator handed out cannot fail, and
                // there is no better recovery than dropping it (`AGENTS.md`
                // §2.9 — best-effort, never a panic).
                let _ = frames.free(frame);
            },
        )?;
        Ok(base_va)
    }

    fn map_anonymous_placed(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        // Choose a base out of this task's heap window first; no page table is
        // touched until the placement succeeds.
        let base_va = self.anon.allocate(page_count)?;
        let frames = self.frames;
        match map_anonymous(
            &mut self.space,
            &self.physmap,
            base_va,
            page_count,
            || frames.alloc().ok(),
            |frame| {
                let _ = frames.free(frame);
            },
        ) {
            Ok(()) => Ok(base_va),
            Err(err) => {
                // The mapping failed (frame exhaustion, …): give the reserved
                // range back so a failed call consumes no address space
                // (`AGENTS.md` §2.9). The range was just minted, so the
                // release matches and cannot fail; ignore its result rather
                // than panic on a path that already failed closed.
                let _ = self.anon.release(base_va, page_count);
                Err(err.into())
            }
        }
    }

    fn unmap_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<(), LiveSpaceError> {
        // A base inside the heap window is a non-`FIXED` placement: it must
        // match a live record exactly before any teardown, so a bad
        // (base, len) for an in-window address fails closed without unmapping
        // a neighbour's pages (`AGENTS.md` §5.4). A `FIXED` base (outside the
        // window) skips this and is torn down by extent as before.
        let placed = self.anon.owns(base_va);
        if placed {
            self.anon.validate(base_va, page_count)?;
        }
        let frames = self.frames;
        unmap_anonymous(
            &mut self.space,
            &self.physmap,
            base_va,
            page_count,
            |frame| {
                let _ = frames.free(frame);
            },
        )?;
        if placed {
            // Validated above, so the release matches; ignore its result.
            let _ = self.anon.release(base_va, page_count);
        }
        Ok(())
    }

    fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError> {
        let region = self.mmio.map_into(&mut self.space, phys_base, len)?;
        Ok(region.virt().as_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveSpace, LiveSpaceError, LiveUserSpace};
    use crate::anon::AnonError;
    use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
    use crate::frame::{FrameAllocator, PhysAddr, PAGE_SIZE};
    use crate::phys::SimPhysMap;
    use crate::uaccess::copy_in;
    use crate::vmm::{AddressSpace, HostPageTable, VirtAddr};

    extern crate std;
    use std::boxed::Box;
    use std::vec;

    /// A simulated physical window the frame allocator draws from and the
    /// direct map translates — frame 16 up, 256 KiB. Anonymous frames must be
    /// reachable through this map to be zeroed (`AGENTS.md` §4).
    const SIM_BASE: u64 = 16 * PAGE_SIZE as u64;
    const SIM_BYTES: usize = 64 * PAGE_SIZE;

    /// A `'static` frame allocator over the simulated usable window. Leaked so
    /// the live space can hold the production `&'static FrameAllocator` shape
    /// (the kernel allocator is a boot global, `AGENTS.md` §2.1); a test leak
    /// is bounded by the process lifetime.
    fn leaked_frames() -> &'static FrameAllocator {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(SIM_BASE),
            length: SIM_BYTES as u64,
        });
        let alloc = FrameAllocator::new(&map).expect("usable window builds an allocator");
        Box::leak(Box::new(alloc))
    }

    fn sim() -> SimPhysMap {
        SimPhysMap::new(PhysAddr::new(SIM_BASE), SIM_BYTES)
    }

    /// The user virtual window device mappings land in — far above the
    /// anonymous regions the tests map, on freshly walked tables.
    const MMIO_WINDOW_BASE: u64 = 0x8000_0000;
    const MMIO_WINDOW_PAGES: usize = 64;

    /// The user virtual window non-`FIXED` anonymous mappings are placed into
    /// — distinct from both the `FIXED` bases the tests choose (low, 0x4000)
    /// and the device window above.
    const ANON_WINDOW_BASE: u64 = 0xC000_0000;
    const ANON_WINDOW_PAGES: usize = 64;

    fn live() -> LiveSpace<HostPageTable, SimPhysMap> {
        LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            sim(),
            leaked_frames(),
            VirtAddr::new(MMIO_WINDOW_BASE),
            MMIO_WINDOW_PAGES,
            VirtAddr::new(ANON_WINDOW_BASE),
            ANON_WINDOW_PAGES,
        )
        .expect("a page-aligned, non-zero window is valid")
    }

    #[test]
    fn map_anonymous_returns_the_fixed_base_and_maps_zeroed_user_pages() {
        let mut live = live();
        let base = 0x4000;
        assert_eq!(live.map_anonymous(base, 3), Ok(base));
        assert_eq!(live.space().mapped_pages(), 3);

        // The pages are readable user memory and read back as zero (no stale
        // bytes are ever user-visible, `AGENTS.md` §4).
        let sim = sim();
        let mut buf = vec![0xAAu8; 3 * PAGE_SIZE];
        copy_in(live.space(), &sim, VirtAddr::new(base), &mut buf).expect("readable user range");
        assert!(buf.iter().all(|&b| b == 0), "anonymous pages are zeroed");
    }

    #[test]
    fn unmap_anonymous_releases_the_whole_region() {
        let mut live = live();
        let base = 0x4000;
        live.map_anonymous(base, 4).expect("map");
        assert_eq!(live.space().mapped_pages(), 4);
        live.unmap_anonymous(base, 4).expect("unmap");
        assert_eq!(live.space().mapped_pages(), 0);
    }

    #[test]
    fn unmap_of_an_unmapped_range_fails_closed_without_partial_teardown() {
        let mut live = live();
        let base = 0x4000;
        live.map_anonymous(base, 2).expect("map");
        // The second page of a 3-page unmap is mapped but the third is not:
        // the whole range is rejected and nothing is torn down (§5.4).
        assert_eq!(
            live.unmap_anonymous(base, 3),
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        );
        assert_eq!(live.space().mapped_pages(), 2, "no partial teardown");
    }

    #[test]
    fn map_anonymous_rejects_a_misaligned_base() {
        let mut live = live();
        assert_eq!(
            live.map_anonymous(0x4001, 1),
            Err(LiveSpaceError::Anon(AnonError::Unaligned))
        );
    }

    #[test]
    fn map_device_window_returns_a_base_in_the_guarded_window() {
        let mut live = live();
        // A device window at an arbitrary physical base; the returned VA is
        // chosen by the guarded allocator inside the configured window.
        let va = live
            .map_device_window(0xFE98_0000, 0x4000)
            .expect("a free slot exists");
        assert!(
            va >= MMIO_WINDOW_BASE
                && va < MMIO_WINDOW_BASE + (MMIO_WINDOW_PAGES as u64) * PAGE_SIZE as u64,
            "device VA lies inside the configured window"
        );
        // A guard page precedes the data, so the first data page is never the
        // window base itself.
        assert!(
            va > MMIO_WINDOW_BASE,
            "a leading guard page precedes the data"
        );
    }

    #[test]
    fn map_device_window_rejects_zero_length() {
        let mut live = live();
        assert!(matches!(
            live.map_device_window(0xFE98_0000, 0),
            Err(LiveSpaceError::Mmio(_))
        ));
    }

    #[test]
    fn map_anonymous_placed_chooses_a_base_in_the_heap_window_and_zeroes_it() {
        let mut live = live();
        let base = live
            .map_anonymous_placed(2)
            .expect("the heap window has room");
        assert!(
            base >= ANON_WINDOW_BASE
                && base < ANON_WINDOW_BASE + (ANON_WINDOW_PAGES as u64) * PAGE_SIZE as u64,
            "a placed base lies inside the heap window"
        );
        assert_eq!(live.space().mapped_pages(), 2);

        // The placed pages are readable user memory and read back as zero.
        let sim = sim();
        let mut buf = vec![0xAAu8; 2 * PAGE_SIZE];
        copy_in(live.space(), &sim, VirtAddr::new(base), &mut buf).expect("readable user range");
        assert!(buf.iter().all(|&b| b == 0), "placed pages are zeroed");
    }

    #[test]
    fn map_anonymous_placed_does_not_overlap_a_prior_placement() {
        let mut live = live();
        let a = live.map_anonymous_placed(3).expect("room");
        let b = live.map_anonymous_placed(2).expect("room");
        assert_ne!(a, b);
        assert!(
            b >= a + 3 * PAGE_SIZE as u64,
            "the second placement bumps past the first"
        );
        assert_eq!(live.space().mapped_pages(), 5);
    }

    #[test]
    fn unmap_releases_a_placement_for_reuse() {
        let mut live = live();
        let a = live.map_anonymous_placed(4).expect("room");
        live.unmap_anonymous(a, 4).expect("placed region unmaps");
        assert_eq!(live.space().mapped_pages(), 0);
        // The freed heap range is reused by the next placement (the bump
        // cursor did not simply advance past it).
        let b = live.map_anonymous_placed(4).expect("room");
        assert_eq!(b, a, "the freed placement base is reused");
    }

    #[test]
    fn unmap_of_a_placed_base_with_a_wrong_extent_fails_closed() {
        let mut live = live();
        let a = live.map_anonymous_placed(3).expect("room");
        // A wrong page count for an in-window (placed) base is refused before
        // any teardown (`AGENTS.md` §5.4) — no partial unmap, region intact.
        assert_eq!(
            live.unmap_anonymous(a, 2),
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        );
        assert_eq!(live.space().mapped_pages(), 3, "no partial teardown");
        // The matching unmap still succeeds afterwards.
        live.unmap_anonymous(a, 3)
            .expect("the matching unmap succeeds");
        assert_eq!(live.space().mapped_pages(), 0);
    }

    /// The retained live space must be `Send` (it is owned by the kernel
    /// thread that runs the task) but is intentionally **not** stored behind
    /// a shared lock — its `Send`-ness is what lets it move to its running
    /// CPU (`AGENTS.md` §4, the module exclusivity argument).
    #[test]
    fn live_space_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<LiveSpace<HostPageTable, SimPhysMap>>();
        assert_send::<Box<dyn LiveUserSpace + Send>>();
    }
}
