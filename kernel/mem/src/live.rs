//! Retained live user address space — the production target the
//! `mem_map` / `mmio_map` syscall producers mutate (`plans/PI.md` P10
//! chunk 5d-0-ii (b′); the `plans/SPAWN.md` `SP5b` production follow-on).
//!
//! Post-spawn an address space was previously captured only as an immutable
//! [`FrozenAddressSpace`] snapshot — enough
//! for the read-only user-memory copy path, but the live arch
//! [`AddressSpace<P>`] was *dropped*. `mem_map` / `mmio_map` need the
//! *running* space to stay **mutable** so a process can grow its own heap or
//! a driver can map a granted device window into its own address space.
//!
//! This module is that retained, mutable space, behind one object-safe
//! boundary so `kernel/core` can hold it without naming a concrete
//! page-table backend `P`:
//!
//! * [`LiveUserSpace`] — the object-safe, mutating operations the producers
//!   reach (anonymous map/unmap; device-window map). `Send` so the boxed
//!   space can be **owned by the kernel thread that runs the task**; it is
//!   deliberately **not** `Sync` and is reached only by the single CPU
//!   currently running the task, from that task's own synchronous syscall
//!   path, so the `&mut` its methods take can never alias — the same
//!   exclusivity the task's kernel stack already relies on (the access is genuinely exclusive).
//! * [`LiveSpace`] — the generic concrete implementation over a port's
//!   [`PageTable`] backend `P`, the kernel direct map `M`, the kernel
//!   [`FrameAllocator`] (anonymous frames), and an [`MmioWindowMap`]
//!   (device windows). It composes the already-audited
//!   [`map_anonymous`] / [`unmap_anonymous`] and
//!   [`MmioWindowMap`] mechanisms — there is no second mapping path
//!   (one definition each).
//!
//! The capability posture (`mem_map` is unprivileged, the `mmio_map` grant
//! is owner-checked) and the placement of a
//! non-`FIXED` anonymous region belong to the higher-level `kernel/core`
//! producer that calls these — this layer knows only the page table, the
//! direct map, the frame allocator, and the device-window allocator.

use crate::anon::{map_anonymous, unmap_anonymous, AnonError};
use crate::anon_window::AnonWindowMap;
use crate::dma::{DmaError, DmaWindowMap};
use crate::frame::FrameAllocator;
use crate::mmio::{MmioError, MmioWindowMap};
use crate::phys::PhysMap;
use crate::vmm::{AddressSpace, FrozenAddressSpace, PageTable, VirtAddr};

/// Why a [`LiveUserSpace`] operation failed.
///
/// A faithful union of the two underlying mechanisms' errors so the
/// `kernel/core` producer can fold each onto a stable `Errno` without this
/// layer knowing the ABI error type (`kernel/mem` names
/// no `lib/abi` error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LiveSpaceError {
    /// An anonymous map/unmap failed (OOM, not-mapped, misaligned, …).
    Anon(AnonError),
    /// A device-window map failed (no virtual slot, page-table refusal, …).
    Mmio(MmioError),
    /// A DMA carve failed (no contiguous block, addressing-limit exceeded,
    /// no virtual slot, …).
    Dma(DmaError),
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

impl From<DmaError> for LiveSpaceError {
    fn from(err: DmaError) -> Self {
        Self::Dma(err)
    }
}

/// A live DMA buffer the [`LiveUserSpace::alloc_dma`] carve returns.
///
/// `cpu_va` is the base **user virtual address** the driver's CPU accesses
/// go through; `phys_base` is the physically-contiguous base of the backing
/// frames — the value the `kernel/core` producer turns into the
/// device-visible address (CPU-physical for a coherent bus, or translated
/// through an inbound viewport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaMapping {
    /// Base user virtual address of the mapped, guard-bracketed buffer.
    pub cpu_va: u64,
    /// Physically-contiguous base address of the backing frames.
    pub phys_base: u64,
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
    /// are user-visible and are never executable;
    /// a part-way failure unwinds every page already mapped, leaving the
    /// space unchanged.
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
    /// so a failed call consumes no address space.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::OutOfMemory`] when the heap
    /// window or the frame allocator is exhausted (deterministic OOM),
    /// or the precise placement/map error otherwise.
    fn map_anonymous_placed(&mut self, page_count: u64) -> Result<u64, LiveSpaceError>;

    /// Release the `page_count`-page region based at `base_va`, zeroing
    /// every frame before it is returned to the allocator (zero on free). The whole range is validated mapped before any page is
    /// torn down (fail closed on a bad range).
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] (e.g. [`AnonError::NotMapped`] when the
    /// range is not one this space mapped).
    fn unmap_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<(), LiveSpaceError>;

    /// Map `len` bytes of device physical memory beginning at `phys_base`
    /// into this space, returning the kernel-chosen base user virtual
    /// address of the new, guard-bracketed, caching-disabled,
    /// non-executable window.
    ///
    /// The producer has already resolved and validated the grant the window
    /// comes from (owner-checked, kind, length);
    /// this only performs the page-table mechanism.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] carrying the precise
    /// [`MmioError`] (no free virtual slot,
    /// page-table refusal, …).
    fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError>;

    /// Carve a physically-contiguous, zeroed, coherent DMA buffer of `len`
    /// bytes into this space, returning its CPU virtual base and its
    /// physically-contiguous base ([`DmaMapping`]).
    ///
    /// The block is mapped `RW|USER`, never executable,
    /// guard-bracketed, and zeroed before it is user-visible. When `addr_limit` is non-zero the contiguous block
    /// is bounded to lie wholly below it (the granted device addressing
    /// constraint); a block that would exceed the limit is returned to
    /// the allocator and the request refused fail-closed. `addr_limit == 0` declares no constraint.
    ///
    /// The producer has already resolved and validated the grant the buffer
    /// is bounded by (owner-checked, kind, length); this only performs the carve + page-table mechanism, and the
    /// buffer is reclaimed (frames zeroed and freed) when the live space is
    /// dropped on task teardown.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Dma`] carrying the precise [`DmaError`]
    /// (zero length, exceeds the max buddy order, no contiguous block,
    /// addressing-limit exceeded, no virtual slot, …).
    fn alloc_dma(&mut self, len: usize, addr_limit: u64) -> Result<DmaMapping, LiveSpaceError>;

    /// Release the DMA buffer whose CPU virtual base is `cpu_va`, zeroing
    /// every backing byte (zero-on-free) before its frames return to the
    /// allocator — the symmetric free for [`Self::alloc_dma`].
    ///
    /// A long-running driver that issues many transfers reclaims each
    /// request's buffers through this rather than leaking frames until it
    /// exits. Only `cpu_va` is taken from the caller; the buffer's extent is
    /// the allocator's own authoritative record, so a `cpu_va` that is not the
    /// base of a live carve in this space fails closed (covering a forged,
    /// stale, or double free) without releasing anything.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Dma`] — [`DmaError::UnknownBuffer`] when `cpu_va` is
    /// not the base of a live DMA carve of this space.
    fn free_dma(&mut self, cpu_va: u64) -> Result<(), LiveSpaceError>;

    /// Map `len` bytes of an existing, kernel-owned, physically-contiguous
    /// **shared-memory region** beginning at `phys_base` into this space as
    /// cacheable `RW|USER` (never executable), guard-bracketed, returning the
    /// kernel-chosen base user virtual address.
    ///
    /// Unlike [`Self::map_device_window`] the frames are ordinary RAM (mapped
    /// cacheable, not device-ordered); unlike [`Self::alloc_dma`] the frames
    /// are **not** allocated or owned by this space \u2014 they belong to the
    /// shared-region registry, which zeroed them on allocation and frees them
    /// only when the owner and every grantee have released the region. This
    /// installs page-table entries only, so a space drop or
    /// [`Self::unmap_shared`] releases the *mapping* without touching the
    /// frames (a second process may still map them).
    ///
    /// The producer has already resolved and owner-checked the per-region
    /// grant the region comes from; this only performs the page-table
    /// mechanism.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] carrying the precise [`MmioError`] (no free
    /// virtual slot, page-table refusal, \u2026) \u2014 the shared mapping reuses the
    /// guarded-window mechanism.
    fn map_shared(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError>;

    /// Release the shared-region mapping based at `base_va` from this space,
    /// tearing down only its page-table entries (the registry owns the
    /// frames). `len` is advisory \u2014 the allocator releases exactly the pages
    /// it recorded for `base_va`.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] \u2014 [`MmioError::UnknownRegion`] if `base_va`
    /// does not name a live shared mapping of this space (fail closed), or a
    /// page-table error.
    fn unmap_shared(&mut self, base_va: u64, len: usize) -> Result<(), LiveSpaceError>;

    /// Snapshot this space's current live mappings into a `Send + Sync`
    /// [`FrozenAddressSpace`], the form the kernel-wide address-space
    /// registry holds for the user-memory copy path.
    ///
    /// A live space grows and shrinks as a task maps its own heap
    /// ([`Self::map_anonymous`] / [`Self::map_anonymous_placed`]), unmaps it
    /// ([`Self::unmap_anonymous`]), or a driver maps a granted device window
    /// or DMA buffer. The registry's snapshot must be re-frozen after every
    /// such mutation, or the copy path (`copy_in` / `copy_out`) would walk a
    /// stale snapshot that cannot see memory the task mapped after spawn —
    /// the exact defect [`FrozenAddressSpace`]'s own docs warn against. This
    /// is the object-safe seam `kernel/core` calls to produce that fresh
    /// snapshot without naming the concrete page-table backend `P`
    /// (one freeze definition).
    fn freeze(&self) -> FrozenAddressSpace;
}

/// The generic concrete live address space retained per task.
///
/// Generic over the port's [`PageTable`] backend `P` and the kernel direct
/// map `M`, so it is constructed by the architecture spawn producer (which
/// names both) and stored behind [`LiveUserSpace`] by `kernel/core` (which
/// names neither). It owns:
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
///   non-`FIXED` anonymous mapping out of this task's heap window;
/// * `dma` — the per-task guarded DMA-buffer allocator that carves a
///   physically-contiguous coherent buffer out of this task's DMA window;
/// * `shared` — the per-task guarded allocator that maps a kernel-owned
///   cross-process shared-memory region (cacheable RAM) into this task's
///   shared-memory window. It reuses the [`MmioWindowMap`] guarded-window
///   mechanism (one slot/guard definition) but maps cacheable, not
///   device-ordered, and owns no frames (the region's frames belong to the
///   shared-region registry).
pub struct LiveSpace<P: PageTable, M: PhysMap> {
    space: AddressSpace<P>,
    physmap: M,
    frames: &'static FrameAllocator,
    mmio: MmioWindowMap,
    anon: AnonWindowMap,
    dma: DmaWindowMap,
    shared: MmioWindowMap,
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
    /// ([`AnonWindowMap`]), and
    /// `[dma_window_base, dma_window_base + dma_window_pages * PAGE_SIZE)` the
    /// range guarded DMA buffers are carved into ([`DmaWindowMap`]). All
    /// three windows must lie in the task's own free user virtual space,
    /// clear of each other, its image, and its stack.
    ///
    /// # Errors
    ///
    /// [`MmioError::InvalidMapConfig`] if any window is misconfigured
    /// (zero pages, a base that is not page-aligned, or a size that
    /// overflows the address space).
    // Each argument is a *distinct* piece of the retained space the
    // architecture spawn producer threads explicitly — the live arch space,
    // the direct map, the frame allocator, and the three guarded windows'
    // `(base, pages)` pairs. Bundling the windows behind a one-use config
    // wrapper purely to satisfy the arg-count lint would be the wrapper
    // type the charter forbids; the explicit list is the clearer shape,
    // mirroring `KernelSyscallHandlers::new`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        space: AddressSpace<P>,
        physmap: M,
        frames: &'static FrameAllocator,
        mmio_window_base: VirtAddr,
        mmio_window_pages: usize,
        anon_window_base: VirtAddr,
        anon_window_pages: usize,
        dma_window_base: VirtAddr,
        dma_window_pages: usize,
        shared_window_base: VirtAddr,
        shared_window_pages: usize,
    ) -> Result<Self, MmioError> {
        let mmio = MmioWindowMap::new(mmio_window_base, mmio_window_pages)?;
        // An anonymous-heap-window config error is the same class of fault as
        // an MMIO-window one; fold it onto `InvalidMapConfig` so the spawn
        // producer has one constructor error to handle.
        let anon = AnonWindowMap::new(anon_window_base, anon_window_pages)
            .map_err(|_| MmioError::InvalidMapConfig)?;
        // A DMA-window config error is likewise folded onto `InvalidMapConfig`
        // (the [`DmaWindowMap`] constructor returns its own `DmaError`).
        let dma = DmaWindowMap::new(dma_window_base, dma_window_pages)
            .map_err(|_| MmioError::InvalidMapConfig)?;
        // The shared-memory window reuses the guarded-window mechanism, in
        // its own virtual range distinct from the device-window range so the
        // two never collide.
        let shared = MmioWindowMap::new(shared_window_base, shared_window_pages)?;
        Ok(Self {
            space,
            physmap,
            frames,
            mmio,
            anon,
            dma,
            shared,
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
                // there is no better recovery than dropping it (best-effort, never a panic).
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
                // range back so a failed call consumes no address space. The range was just minted, so the
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
        // a neighbour's pages. A `FIXED` base (outside the
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

    fn alloc_dma(&mut self, len: usize, addr_limit: u64) -> Result<DmaMapping, LiveSpaceError> {
        let buf =
            self.dma
                .alloc_into(&mut self.space, self.frames, &self.physmap, len, addr_limit)?;
        Ok(DmaMapping {
            cpu_va: buf.virt().as_u64(),
            phys_base: buf.phys().as_u64(),
        })
    }

    fn free_dma(&mut self, cpu_va: u64) -> Result<(), LiveSpaceError> {
        self.dma.free_at(
            &mut self.space,
            self.frames,
            &self.physmap,
            VirtAddr::new(cpu_va),
        )?;
        Ok(())
    }

    fn map_shared(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError> {
        // Reuse the guarded-window mechanism, mapping cacheable RAM rather
        // than device registers. The frames are owned by the shared-region
        // registry, so the space installs page-table entries only.
        let region = self
            .shared
            .map_cacheable_into(&mut self.space, phys_base, len)?;
        Ok(region.virt().as_u64())
    }

    fn unmap_shared(&mut self, base_va: u64, _len: usize) -> Result<(), LiveSpaceError> {
        // Release exactly the pages the shared-window allocator recorded for
        // this base; `len` is advisory (the record is authoritative). The
        // frames are not freed here - they belong to the registry, which
        // frees them when the owner and every grantee have released the
        // region.
        self.shared
            .unmap_at(&mut self.space, VirtAddr::new(base_va))
            .map_err(LiveSpaceError::from)
    }

    fn freeze(&self) -> FrozenAddressSpace {
        // One freeze definition lives on `AddressSpace`; this only erases the
        // backend `P` for the registry.
        self.space.freeze()
    }
}

impl<P: PageTable, M: PhysMap> Drop for LiveSpace<P, M> {
    fn drop(&mut self) {
        // Reclaim every live DMA buffer when the task's live space is torn
        // down: each backing block is zeroed (zero-on-free)
        // and returned to the frame allocator, so a driver task's exit never
        // leaks the physical frames its DMA buffers held or leaves their
        // (possibly secret-bearing) contents recoverable. Anonymous and
        // device-window mappings live and die with the page-table frames of
        // the `space` being dropped; the DMA frames come from the shared
        // global allocator and so must be returned explicitly here.
        self.dma
            .drain_into(&mut self.space, self.frames, &self.physmap);
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveSpace, LiveSpaceError, LiveUserSpace};
    use crate::anon::AnonError;
    use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
    use crate::dma::DmaError;
    use crate::frame::{FrameAllocator, PhysAddr, PAGE_SIZE};
    use crate::phys::SimPhysMap;
    use crate::uaccess::{copy_in, copy_out};
    use crate::vmm::{AddressSpace, HostPageTable, VirtAddr};

    extern crate std;
    use std::boxed::Box;
    use std::vec;

    /// A simulated physical window the frame allocator draws from and the
    /// direct map translates — frame 16 up, 256 KiB. Anonymous frames must be
    /// reachable through this map to be zeroed.
    const SIM_BASE: u64 = 16 * PAGE_SIZE as u64;
    const SIM_BYTES: usize = 64 * PAGE_SIZE;

    /// A `'static` frame allocator over the simulated usable window. Leaked so
    /// the live space can hold the production `&'static FrameAllocator` shape
    /// (the kernel allocator is a boot global); a test leak
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

    /// The user virtual window guarded DMA buffers are carved into — distinct
    /// from the device, anon, and `FIXED` regions above.
    const DMA_WINDOW_BASE: u64 = 0x1_0000_0000;
    const DMA_WINDOW_PAGES: usize = 64;

    /// The user virtual window shared-memory regions are mapped into —
    /// distinct from the device, anon, DMA, and `FIXED` regions above.
    const SHARED_WINDOW_BASE: u64 = 0x2_0000_0000;
    const SHARED_WINDOW_PAGES: usize = 64;

    fn live() -> LiveSpace<HostPageTable, SimPhysMap> {
        LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            sim(),
            leaked_frames(),
            VirtAddr::new(MMIO_WINDOW_BASE),
            MMIO_WINDOW_PAGES,
            VirtAddr::new(ANON_WINDOW_BASE),
            ANON_WINDOW_PAGES,
            VirtAddr::new(DMA_WINDOW_BASE),
            DMA_WINDOW_PAGES,
            VirtAddr::new(SHARED_WINDOW_BASE),
            SHARED_WINDOW_PAGES,
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
        // bytes are ever user-visible).
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
        // the whole range is rejected and nothing is torn down.
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
        // any teardown — no partial unmap, region intact.
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

    #[test]
    fn alloc_dma_maps_a_zeroed_coherent_buffer_in_the_dma_window() {
        let mut live = live();
        let mapping = live
            .alloc_dma(2 * PAGE_SIZE, 0)
            .expect("a free block exists");
        // The CPU VA lies inside the configured DMA window, past the leading
        // guard page.
        assert!(
            mapping.cpu_va > DMA_WINDOW_BASE
                && mapping.cpu_va < DMA_WINDOW_BASE + (DMA_WINDOW_PAGES as u64) * PAGE_SIZE as u64,
            "DMA VA lies inside the configured window, past the leading guard"
        );
        // The backing block is physically contiguous RAM drawn from the
        // allocator's window.
        assert!(mapping.phys_base >= SIM_BASE, "phys base is real RAM");
        // The buffer reads back as zero through the CPU mapping (no stale
        // bytes are ever user-visible).
        let sim = sim();
        let mut buf = vec![0xAAu8; 2 * PAGE_SIZE];
        copy_in(live.space(), &sim, VirtAddr::new(mapping.cpu_va), &mut buf)
            .expect("readable user range");
        assert!(buf.iter().all(|&b| b == 0), "DMA buffer is zeroed");
    }

    #[test]
    fn alloc_dma_rejects_a_block_above_the_addressing_limit() {
        let mut live = live();
        // An addressing limit below the allocator's RAM window cannot be
        // satisfied by any block, so the carve is refused fail-closed and no
        // pages are mapped.
        assert_eq!(
            live.alloc_dma(PAGE_SIZE, SIM_BASE),
            Err(LiveSpaceError::Dma(DmaError::AddrLimitExceeded))
        );
        assert_eq!(
            live.space().mapped_pages(),
            0,
            "no buffer mapped on refusal"
        );
    }

    #[test]
    fn alloc_dma_rejects_zero_length() {
        let mut live = live();
        assert_eq!(
            live.alloc_dma(0, 0),
            Err(LiveSpaceError::Dma(DmaError::ZeroSize))
        );
    }

    #[test]
    fn dropping_the_live_space_reclaims_every_dma_block() {
        // Build the live space over a `'static` allocator we keep a handle to,
        // so we can observe the frame count before, during, and after the
        // space (and its DMA buffers) are torn down.
        let frames = leaked_frames();
        let before = frames.free_frames();
        {
            let mut live = LiveSpace::new(
                AddressSpace::new(HostPageTable::new()),
                sim(),
                frames,
                VirtAddr::new(MMIO_WINDOW_BASE),
                MMIO_WINDOW_PAGES,
                VirtAddr::new(ANON_WINDOW_BASE),
                ANON_WINDOW_PAGES,
                VirtAddr::new(DMA_WINDOW_BASE),
                DMA_WINDOW_PAGES,
                VirtAddr::new(SHARED_WINDOW_BASE),
                SHARED_WINDOW_PAGES,
            )
            .expect("windows are valid");
            live.alloc_dma(2 * PAGE_SIZE, 0)
                .expect("a free block exists");
            assert!(
                frames.free_frames() < before,
                "the DMA carve consumed frames"
            );
        }
        // Dropping the live space reclaimed the DMA block's frames.
        assert_eq!(
            frames.free_frames(),
            before,
            "every DMA frame is returned to the allocator on teardown"
        );
    }

    #[test]
    fn free_dma_releases_a_carve_and_repeated_cycles_reclaim_fully() {
        // `free_dma` is the per-buffer release the `dma_free` syscall drives:
        // a long-running driver allocates and frees a DMA buffer every
        // transfer, so many alloc/free cycles must leave the frame allocator
        // exactly as full as it started — never marching upward (the leak the
        // syscall exists to close).
        let frames = leaked_frames();
        let before = frames.free_frames();
        let mut live = LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            sim(),
            frames,
            VirtAddr::new(MMIO_WINDOW_BASE),
            MMIO_WINDOW_PAGES,
            VirtAddr::new(ANON_WINDOW_BASE),
            ANON_WINDOW_PAGES,
            VirtAddr::new(DMA_WINDOW_BASE),
            DMA_WINDOW_PAGES,
            VirtAddr::new(SHARED_WINDOW_BASE),
            SHARED_WINDOW_PAGES,
        )
        .expect("windows are valid");

        for _ in 0..50 {
            let mapping = live.alloc_dma(2 * PAGE_SIZE, 0).expect("a free block");
            assert!(frames.free_frames() < before, "the carve consumed frames");
            live.free_dma(mapping.cpu_va).expect("free by cpu base");
            assert_eq!(
                frames.free_frames(),
                before,
                "each free returns every frame — no leak across cycles"
            );
            assert_eq!(live.space().mapped_pages(), 0, "no data page left mapped");
        }
        // A free of an address that names no live carve fails closed.
        assert_eq!(
            live.free_dma(DMA_WINDOW_BASE + PAGE_SIZE as u64),
            Err(LiveSpaceError::Dma(DmaError::UnknownBuffer))
        );
    }

    #[test]
    fn map_shared_maps_a_cacheable_region_in_its_window_and_unmaps_by_base() {
        let mut live = live();
        // A real frame reachable through the sim direct map; map_shared only
        // installs page-table entries (the registry owns/zeroes the frames).
        let phys = SIM_BASE;
        let len = 2 * PAGE_SIZE;
        let base = live.map_shared(phys, len).expect("maps the region");
        // The mapping lands inside the configured shared window, past the
        // leading guard page, and clear of the DMA window below it.
        assert!(
            base > SHARED_WINDOW_BASE
                && base < SHARED_WINDOW_BASE + (SHARED_WINDOW_PAGES as u64) * PAGE_SIZE as u64,
            "shared VA lies inside the configured window, past the leading guard"
        );
        // The region is read/write through the CPU mapping: a write is
        // visible on read-back through the same backing frames.
        let sim = sim();
        let payload = [0x5Au8; 16];
        copy_out(live.space(), &sim, VirtAddr::new(base), &payload).expect("writable user range");
        let mut back = [0u8; 16];
        copy_in(live.space(), &sim, VirtAddr::new(base), &mut back).expect("readable user range");
        assert_eq!(back, payload, "shared region round-trips reads and writes");
        // Unmapping by base tears down the page-table entries; the frames are
        // not freed here (the registry owns them).
        live.unmap_shared(base, len).expect("unmaps by base");
        assert_eq!(
            live.space().mapped_pages(),
            0,
            "the shared mapping's pages are gone after unmap"
        );
        // A double-unmap of the same base fails closed.
        assert!(
            live.unmap_shared(base, len).is_err(),
            "double-unmap of a shared region fails closed"
        );
    }

    /// The retained live space must be `Send` (it is owned by the kernel
    /// thread that runs the task) but is intentionally **not** stored behind
    /// a shared lock — its `Send`-ness is what lets it move to its running
    /// CPU (the module exclusivity argument).
    #[test]
    fn live_space_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<LiveSpace<HostPageTable, SimPhysMap>>();
        assert_send::<Box<dyn LiveUserSpace + Send>>();
    }
}
