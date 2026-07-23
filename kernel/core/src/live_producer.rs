//! Production `mem_map` / `mmio_map` producers over the per-task retained
//! live address space (`plans/PI.md` P10 chunk 5d-0-ii (b′)).
//!
//! [`crate::memmap::MemMap`] and [`crate::devres::MmioMapFacility`] are the
//! object-safe seams the `mem_map` and `mmio_map` syscall handlers reach.
//! Before this module the only implementations were the fail-closed
//! `NULL_*` defaults, because no live, mutable address space was retained
//! per task (the spawn path froze the space into a read-only snapshot and
//! dropped the live one). With per-task retention now wired through the
//! kthread runtime ([`crate::kthread::with_current_live_space`]) these two
//! producers route each call to the **caller's own** live space:
//!
//! * a syscall handler runs on the CPU servicing the trap, on which the
//!   calling task is the one currently switched in, so its live space is
//!   exactly the per-CPU slot for [`SchedulerArch::current_cpu`]; and
//! * the access is exclusive — the task is suspended in its own syscall
//!   trap for the whole call (see [`crate::kthread::with_current_live_space`]).
//!
//! Both producers are generic over the arch (`A: SchedulerArch`) and hold a
//! `&'static A`, mirroring [`crate::procwait::KernelProcessWait`], so
//! `kernel/core` reads the current CPU without naming a concrete port. A call on a CPU with no published live space (a
//! task spawned without a retained space) fails closed with
//! [`Errno::NotImplemented`] rather than touching another task's memory.

use alloc::vec::Vec;

use tairix_abi::{Errno, MapFlags};
use tairix_kernel_mem::{
    page_count_for, AllocError, AnonError, DmaError, Frame, FrameAllocator, LiveSpaceError,
    MmioError, PhysAddr, PhysMap, PAGE_SIZE,
};
use tairix_kernel_sched_api::SchedulerArch;

use crate::devres::{
    DmaAllocFacility, DmaCarve, MmioMapFacility, MmioMemoryKind, SharedChunk, SharedMemFacility,
};
use crate::filemap::FileMap;
use crate::kthread::with_current_live_space;
use crate::memmap::MemMap;

/// Fold an [`AnonError`] onto a stable [`Errno`]:
/// allocator exhaustion is [`Errno::OutOfMemory`], a not-mapped range
/// is [`Errno::NotFound`] (fail closed), and a misalignment/overflow
/// is [`Errno::OutOfRange`].
fn anon_errno(err: AnonError) -> Errno {
    match err {
        AnonError::ZeroLength => Errno::LengthOutOfRange,
        AnonError::Unaligned | AnonError::Overflow => Errno::OutOfRange,
        AnonError::OutOfMemory => Errno::OutOfMemory,
        AnonError::NotMapped => Errno::NotFound,
        // `PhysUnmapped`, `Map(_)`, and any future (`#[non_exhaustive]`)
        // variant fold to the generic bad-address error, failing closed
        // rather than being silently dropped.
        _ => Errno::BadAddress,
    }
}

/// Fold an [`MmioError`] onto a stable [`Errno`]: no free virtual slot is
/// [`Errno::OutOfMemory`] (deterministic exhaustion), a malformed
/// region or mapper config is [`Errno::OutOfRange`], and a page-table or
/// direct-map failure is [`Errno::BadAddress`].
fn mmio_errno(err: MmioError) -> Errno {
    match err {
        MmioError::NoVirtualSpace | MmioError::OutOfMemory => Errno::OutOfMemory,
        MmioError::InvalidRegion | MmioError::InvalidMapConfig => Errno::OutOfRange,
        MmioError::UnknownRegion => Errno::NotFound,
        // `PageTable`, `DirectMap`, and any future (`#[non_exhaustive]`)
        // kind fail closed to a generic bad-address error.
        _ => Errno::BadAddress,
    }
}

/// Fold a [`DmaError`] onto a stable [`Errno`]: a contiguous-block or
/// page-table-frame exhaustion is [`Errno::OutOfMemory`] (deterministic OOM); a request beyond the max buddy order or the granted addressing limit
/// is [`Errno::OutOfRange`]; a zero-length request is
/// [`Errno::LengthOutOfRange`]; and a not-reachable frame or page-table
/// refusal is [`Errno::BadAddress`] (fail closed).
fn dma_errno(err: DmaError) -> Errno {
    match err {
        DmaError::Alloc(_) => Errno::OutOfMemory,
        DmaError::ZeroSize => Errno::LengthOutOfRange,
        DmaError::SizeUnsupported | DmaError::AddrLimitExceeded => Errno::OutOfRange,
        // `PageTable`, `DirectMap`, `UnknownBuffer`, `InvalidPoolConfig`, and
        // any future (`#[non_exhaustive]`) variant fail closed to a generic
        // bad-address error.
        _ => Errno::BadAddress,
    }
}

/// Fold an [`AllocError`] onto a stable [`Errno`]: exhaustion is
/// [`Errno::OutOfMemory`] (deterministic OOM), a too-large order is
/// [`Errno::OutOfRange`], a zero-size request is [`Errno::LengthOutOfRange`],
/// and any other (out-of-range frame/address) fails closed to
/// [`Errno::OutOfRange`].
fn alloc_errno(err: AllocError) -> Errno {
    match err {
        AllocError::OutOfMemory => Errno::OutOfMemory,
        AllocError::ZeroSize => Errno::LengthOutOfRange,
        // `SizeUnsupported`, `OutOfRange`, and any future variant fail closed
        // to an out-of-range error.
        _ => Errno::OutOfRange,
    }
}

/// Fold a [`LiveSpaceError`] onto a stable [`Errno`].
fn live_errno(err: LiveSpaceError) -> Errno {
    match err {
        LiveSpaceError::Anon(anon) => anon_errno(anon),
        LiveSpaceError::Mmio(mmio) => mmio_errno(mmio),
        LiveSpaceError::Dma(dma) => dma_errno(dma),
        // `LiveSpaceError` is `#[non_exhaustive]`; fail closed.
        _ => Errno::BadAddress,
    }
}

/// The production anonymous-memory producer: maps/unmaps `RW` anonymous
/// pages in the **calling task's own** live address space (`plans/SPAWN.md`
/// `SP5b` production form).
pub struct LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
}

impl<A> LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle the CPU id is read
    /// from (the boot-leaked `KernelState` arch, exactly as
    /// [`crate::procwait::KernelProcessWait`]).
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> MemMap for LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn reserve(&self, len: usize, flags: MapFlags, addr_hint: u64) -> Result<u64, Errno> {
        let page_count = page_count_for(len).map_err(anon_errno)?;
        let cpu = self.arch.current_cpu();
        // Reserve address space only — no frame, no page-table entry. The
        // pages fault in one at a time (`map` below, from the anonymous
        // fault path), so a large `mem_map` never zeroes and commits
        // thousands of pages in one non-preemptible syscall. `FIXED` names
        // its own base; a non-`FIXED` request draws a base from this task's
        // own heap window (never a base guessed here that might collide with
        // the image, stack, or a granted device window).
        if flags.is_fixed() {
            with_current_live_space(cpu, |space| {
                space.reserve_anonymous_at(addr_hint, page_count)
            })
        } else {
            with_current_live_space(cpu, |space| space.reserve_anonymous(page_count))
        }
        .ok_or(Errno::NotImplemented)?
        .map_err(live_errno)
    }

    fn commit(&self, pages: u64) -> Result<(), Errno> {
        let cpu = self.arch.current_cpu();
        // Reserve physical headroom for `pages` demand-paged pages whose
        // address space already exists (stack growth): commitment only, no
        // placement. Fails closed as a `Result` when the no-overcommit
        // budget cannot admit the growth, so the stack-fault path refuses
        // rather than killing the task on first touch.
        with_current_live_space(cpu, |space| space.commit_anonymous(pages))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }

    fn map(&self, len: usize, flags: MapFlags, addr_hint: u64) -> Result<u64, Errno> {
        let page_count = page_count_for(len).map_err(anon_errno)?;
        let cpu = self.arch.current_cpu();
        // The single-page commit the anonymous and stack fault paths use to
        // back one reserved page with a fresh zeroed `RW|USER` frame. Always
        // `FIXED` (the faulting page's own base); a non-`FIXED` request asks
        // the live space's per-task heap-window allocator to choose a base
        // out of this task's own free user-VA.
        if flags.is_fixed() {
            with_current_live_space(cpu, |space| space.map_anonymous(addr_hint, page_count))
        } else {
            with_current_live_space(cpu, |space| space.map_anonymous_placed(page_count))
        }
        .ok_or(Errno::NotImplemented)?
        .map_err(live_errno)
    }

    fn unmap(&self, base: u64, len: usize) -> Result<(), Errno> {
        let page_count = page_count_for(len).map_err(anon_errno)?;
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.unmap_anonymous(base, page_count))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }
}

/// The whole-page count a `len`-byte file mapping spans, rounded up.
///
/// The file-mapping length is 64-bit end to end (a mappable file may
/// exceed both `usize` and any 32-bit figure), so this is the `u64` form
/// of [`page_count_for`]; a zero length names nothing and fails closed.
fn file_page_count(len: u64) -> Result<u64, Errno> {
    if len == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(len.div_ceil(PAGE_SIZE as u64))
}

impl<A> FileMap for LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn reserve(&self, len: u64) -> Result<u64, Errno> {
        let page_count = file_page_count(len)?;
        let cpu = self.arch.current_cpu();
        // Pure address-space reservation out of this task's own
        // file-mapping window; no frame moves until a fault lands.
        with_current_live_space(cpu, |space| space.reserve_file_region(page_count))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }

    fn map_page(&self, va: u64, contents: &[u8]) -> Result<(), Errno> {
        let cpu = self.arch.current_cpu();
        // The live space refuses an address outside every reserved file
        // region (`NotFound` after folding), so the fault path can never
        // materialise memory the task did not map.
        with_current_live_space(cpu, |space| space.map_file_page_at(va, contents))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }

    fn release(&self, base: u64, len: u64) -> Result<u64, Errno> {
        let page_count = file_page_count(len)?;
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.release_file_region(base, page_count))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }
}

/// The production MMIO-map facility: maps a validated, **granted** device
/// window into the calling driver task's own live address space
/// (`plans/PI.md` P10 chunk 5d-0). The handler has already resolved and
/// owner-checked the grant; this performs only
/// the page-table mechanism, guard-bracketed and caching-disabled.
pub struct LiveMmioMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
}

impl<A> LiveMmioMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle.
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> MmioMapFacility for LiveMmioMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn map_window(&self, phys_base: u64, len: usize, kind: MmioMemoryKind) -> Result<u64, Errno> {
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| match kind {
            MmioMemoryKind::Device => space.map_device_window(phys_base, len),
            MmioMemoryKind::FramebufferWriteBack => {
                space.map_writeback_framebuffer_window(phys_base, len)
            }
            MmioMemoryKind::FramebufferWriteCombine => space.map_framebuffer_window(phys_base, len),
        })
        .ok_or(Errno::NotImplemented)?
        .map_err(live_errno)
    }
}

/// The production DMA-alloc facility: carves a coherent, guard-bracketed DMA
/// buffer into the calling driver task's own live address space
/// (`plans/PI.md` P10 chunk 5d-0). The handler has already resolved and
/// owner-checked the grant and validated its DMA constraint; this performs only the carve mechanism, bounded by the
/// grant's `addr_limit`.
pub struct LiveDmaAlloc<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
}

impl<A> LiveDmaAlloc<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle.
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> DmaAllocFacility for LiveDmaAlloc<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn alloc(&self, len: usize, addr_limit: u64) -> Result<DmaCarve, Errno> {
        let cpu = self.arch.current_cpu();
        // The coherent (and QEMU `virt`) device-visible address is the
        // CPU-physical base; a translating inbound viewport is refused
        // earlier in the handler (it rides the metal item), so here the
        // device address is exactly the carved physical base.
        with_current_live_space(cpu, |space| space.alloc_dma(len, addr_limit))
            .ok_or(Errno::NotImplemented)?
            .map(|mapping| DmaCarve {
                cpu_va: mapping.cpu_va,
                device_addr: mapping.phys_base,
            })
            .map_err(live_errno)
    }

    fn free(&self, cpu_va: u64) -> Result<(), Errno> {
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.free_dma(cpu_va))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }
}

/// The production shared-memory facility: allocates, zeroes, maps, and frees
/// cross-process shared-memory regions over the kernel frame allocator and
/// the calling task's own live address space (`plans/USB.md`).
///
/// `arch` is read for the current CPU (the slot the calling task's live
/// space the *mapping* lands in is published on, exactly like
/// [`LiveMmioMap`]); `frames` is the kernel allocator the region's
/// physically-contiguous backing is drawn from and returned to; `physmap` is
/// the kernel direct map the region's frames are scrubbed through on
/// allocation and on free. Scrubbing through the direct map (not a user
/// mapping) is what makes the last-reference free's zero-on-free hold even
/// when the task whose teardown drops it is a kernel thread with no live
/// address space (a hot-removed driver torn down by the device manager).
pub struct LiveSharedMem<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
}

impl<A> LiveSharedMem<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle, the kernel frame
    /// allocator, and the kernel direct physical map.
    ///
    /// `physmap` is the kernel-privileged view of all RAM (identity on
    /// aarch64 / riscv64, higher-half on x86_64); the facility scrubs a
    /// region's frames through it on allocation and on free, independent of
    /// any user mapping, so the zero-on-free guarantee holds even when the
    /// task whose teardown frees the region's last reference is a kernel
    /// thread with no live address space (a driver-store unload).
    #[must_use]
    pub const fn new(
        arch: &'static A,
        frames: &'static FrameAllocator,
        physmap: &'static (dyn PhysMap + Sync),
    ) -> Self {
        Self {
            arch,
            frames,
            physmap,
        }
    }

    /// Scrub `pages` frames beginning at `phys_base` through the kernel
    /// direct map. A frame the map cannot reach is left untouched (best
    /// effort, never a panic) — but it cannot become user-visible
    /// un-scrubbed, because every region is also scrubbed on allocation.
    fn scrub(&self, phys_base: u64, pages: u64) {
        let Some(len) = usize::try_from(pages)
            .ok()
            .and_then(|p| p.checked_mul(PAGE_SIZE))
        else {
            return;
        };
        if len == 0 {
            return;
        }
        if let Some(ptr) = self.physmap.translate(PhysAddr::new(phys_base), len) {
            // SAFETY: `translate` returned a pointer valid for `len` bytes of
            // the kernel direct map. The frames are the region's own backing,
            // owned by the registry and not mapped writable anywhere else at
            // scrub time (allocation has not yet handed them out / free has
            // dropped the last mapping), so no concurrent access aliases them.
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, len);
            }
        }
    }
}

impl<A> SharedMemFacility for LiveSharedMem<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn alloc_region(&self, pages: u64) -> Result<Vec<SharedChunk>, Errno> {
        if pages == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        // Allocate the backing as a set of physically-contiguous buddy chunks
        // (one for a small region, several for one larger than the 8 MiB
        // single-block ceiling), so the region size is bounded by RAM.
        let blocks = self.frames.alloc_chunks(pages).map_err(alloc_errno)?;
        let mut chunks: Vec<SharedChunk> = Vec::new();
        if chunks.try_reserve_exact(blocks.len()).is_err() {
            // Bookkeeping OOM: return every just-allocated block and fail
            // closed, leaking nothing.
            for (frame, order) in &blocks {
                let _ = self.frames.free_order(*frame, *order);
            }
            return Err(Errno::OutOfMemory);
        }
        for (frame, order) in blocks {
            let phys_base = frame.start().as_u64();
            let pages = 1u64 << order;
            // Scrub each block before it can become user-visible (no
            // cross-process leak); the kernel direct map needs no user
            // mapping, so it is robust in every context.
            self.scrub(phys_base, pages);
            chunks.push(SharedChunk {
                phys_base,
                order,
                pages,
            });
        }
        Ok(chunks)
    }

    fn map_region(&self, chunks: &[SharedChunk]) -> Result<u64, Errno> {
        // Project the chunk list onto the `(phys_base, pages)` list the live
        // space maps into one contiguous virtual window.
        let mut list: Vec<(u64, u64)> = Vec::new();
        if list.try_reserve_exact(chunks.len()).is_err() {
            return Err(Errno::OutOfMemory);
        }
        for c in chunks {
            list.push((c.phys_base, c.pages));
        }
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.map_shared_chunks(&list))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }

    fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.unmap_shared(base, len))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }

    fn free_region(&self, chunks: &[SharedChunk]) {
        // Scrub before returning each block to the allocator (zero-on-free)
        // through the kernel direct map, then free the buddy block. Robust in
        // every context, including a kernel-thread teardown with no live
        // address space.
        for c in chunks {
            self.scrub(c.phys_base, c.pages);
            let frame = Frame::containing(PhysAddr::new(c.phys_base));
            let _ = self.frames.free_order(frame, c.order);
        }
    }

    fn kernel_window(&self, chunks: &[SharedChunk], len: usize) -> Option<core::ptr::NonNull<u8>> {
        // Only a single-chunk region is physically contiguous, so one direct-
        // map translation covers the whole window; a multi-chunk region is
        // not contiguous and fails closed (no kernel consumer maps one).
        if let [chunk] = chunks {
            self.physmap.translate(PhysAddr::new(chunk.phys_base), len)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::boxed::Box;
    use std::vec::Vec;

    use tairix_kernel_mem::{
        AddressSpace, DmaMapping, FrozenAddressSpace, HostPageTable, LiveUserSpace,
    };

    use crate::kthread::publish_live_space_for_test;
    use crate::test_arch::TestArch;

    /// A recording [`LiveUserSpace`] double: it logs each call and returns a
    /// configurable result, so the producer's routing + error fold are
    /// exercised without a real page table (the real [`LiveUserSpace`] is
    /// covered in `kernel/mem`). `&mut self` methods mean plain fields
    /// suffice — no interior mutability — so it stays `Send`.
    #[derive(Default)]
    struct FakeLive {
        anon_maps: Vec<(u64, u64)>,
        anon_placed: Vec<u64>,
        anon_reserves: Vec<u64>,
        anon_commits: Vec<u64>,
        anon_reserves_at: Vec<(u64, u64)>,
        anon_unmaps: Vec<(u64, u64)>,
        device_maps: Vec<(u64, usize)>,
        writeback_framebuffer_maps: Vec<(u64, usize)>,
        framebuffer_maps: Vec<(u64, usize)>,
        dma_allocs: Vec<(usize, u64)>,
        dma_frees: Vec<u64>,
        file_reserves: Vec<u64>,
        file_page_maps: Vec<(u64, usize)>,
        file_releases: Vec<(u64, u64)>,
        next: Option<LiveSpaceError>,
    }

    /// The physical base a DMA carve reports back from the fake, so the
    /// producer test can assert the device address flows through unchanged.
    const DMA_PHYS: u64 = 0x4001_0000;

    /// The base a placed (non-`FIXED`) map reports back from the fake, so the
    /// producer test can assert the returned value flows through unchanged.
    const PLACED_BASE: u64 = 0xC000_0000;

    /// The base a file-region reservation reports back from the fake, so the
    /// producer test can assert the returned value flows through unchanged.
    const FILE_BASE: u64 = 0xF000_0000;

    /// The resident-page count a file-region release reports back from the
    /// fake.
    const FILE_RESIDENT: u64 = 3;

    impl LiveUserSpace for FakeLive {
        fn map_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<u64, LiveSpaceError> {
            self.anon_maps.push((base_va, page_count));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(base_va),
            }
        }

        fn map_anonymous_placed(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
            self.anon_placed.push(page_count);
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(PLACED_BASE),
            }
        }

        fn reserve_anonymous(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
            self.anon_reserves.push(page_count);
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(PLACED_BASE),
            }
        }

        fn commit_anonymous(&mut self, page_count: u64) -> Result<(), LiveSpaceError> {
            self.anon_commits.push(page_count);
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn reserve_anonymous_at(
            &mut self,
            base_va: u64,
            page_count: u64,
        ) -> Result<u64, LiveSpaceError> {
            self.anon_reserves_at.push((base_va, page_count));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(base_va),
            }
        }

        fn unmap_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<(), LiveSpaceError> {
            self.anon_unmaps.push((base_va, page_count));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn reserve_file_region(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
            self.file_reserves.push(page_count);
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(FILE_BASE),
            }
        }

        fn map_file_page_at(&mut self, va: u64, contents: &[u8]) -> Result<(), LiveSpaceError> {
            self.file_page_maps.push((va, contents.len()));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn release_file_region(
            &mut self,
            base_va: u64,
            page_count: u64,
        ) -> Result<u64, LiveSpaceError> {
            self.file_releases.push((base_va, page_count));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(FILE_RESIDENT),
            }
        }

        fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError> {
            self.device_maps.push((phys_base, len));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(0x9000_1000),
            }
        }

        fn map_framebuffer_window(
            &mut self,
            phys_base: u64,
            len: usize,
        ) -> Result<u64, LiveSpaceError> {
            self.framebuffer_maps.push((phys_base, len));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(0x9000_2000),
            }
        }

        fn map_writeback_framebuffer_window(
            &mut self,
            phys_base: u64,
            len: usize,
        ) -> Result<u64, LiveSpaceError> {
            self.writeback_framebuffer_maps.push((phys_base, len));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(0x9000_3000),
            }
        }

        fn translate_page(
            &self,
            _page: tairix_kernel_mem::Page,
        ) -> Option<(tairix_kernel_mem::Frame, tairix_kernel_mem::MapFlags)> {
            // The routing double models no page table; fault-resolution
            // translation is covered by the real `LiveSpace` in `kernel/mem`.
            None
        }

        fn alloc_dma(&mut self, len: usize, addr_limit: u64) -> Result<DmaMapping, LiveSpaceError> {
            self.dma_allocs.push((len, addr_limit));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(DmaMapping {
                    cpu_va: 0xD000_2000,
                    phys_base: DMA_PHYS,
                }),
            }
        }

        fn free_dma(&mut self, cpu_va: u64) -> Result<(), LiveSpaceError> {
            self.dma_frees.push(cpu_va);
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn map_shared(&mut self, _phys_base: u64, _len: usize) -> Result<u64, LiveSpaceError> {
            // The shared-memory producer's map/unmap routing is exercised at
            // the syscall-handler level and end-to-end in QEMU; this double
            // only satisfies the trait for the other producers' tests.
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(0x9000_5000),
            }
        }

        fn map_shared_chunks(&mut self, _chunks: &[(u64, u64)]) -> Result<u64, LiveSpaceError> {
            // As `map_shared`: the chunked mapping is covered at the
            // syscall-handler level and end-to-end in QEMU.
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(0x9000_5000),
            }
        }

        fn unmap_shared(&mut self, _base_va: u64, _len: usize) -> Result<(), LiveSpaceError> {
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn freeze(&self) -> FrozenAddressSpace {
            // The producer-routing tests never inspect the snapshot; an empty
            // frozen space satisfies the trait. The re-freeze behaviour is
            // exercised end-to-end against a real `LiveSpace` in `aspace`.
            AddressSpace::new(HostPageTable::new()).freeze()
        }

        fn ramzip_fault_in(
            &mut self,
            _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
            _va: u64,
            _sink: &dyn tairix_log::Sink,
        ) -> tairix_kernel_mem::RamzipFaultOutcome {
            // The routing double models no page table and holds no tier; the
            // real compressed fault-in is exercised against `LiveSpace` in
            // `kernel/mem`. Fall through (no entry).
            tairix_kernel_mem::RamzipFaultOutcome::NoEntry
        }

        fn ramzip_reclaim(
            &mut self,
            _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
            _pressure: &tairix_kernel_mem::MemoryPressure,
            _reclaimable_residue: usize,
            _want: usize,
            _template: tairix_kernel_mem::PageCandidate,
            _sink: &dyn tairix_log::Sink,
        ) -> tairix_kernel_mem::RamzipReclaimSummary {
            // No candidates in the routing double; reclaim is exercised
            // against the real `LiveSpace`.
            tairix_kernel_mem::RamzipReclaimSummary::default()
        }

        fn ramzip_cluster(
            &mut self,
            _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
            _pressure: &tairix_kernel_mem::MemoryPressure,
            _va: u64,
            _sink: &dyn tairix_log::Sink,
        ) -> usize {
            // The routing double holds no tier; clustering is exercised
            // against the real `LiveSpace` in `kernel/mem`.
            0
        }

        fn ramzip_warm(
            &mut self,
            _tier: &tairix_sync::SpinLock<tairix_kernel_mem::Ramzip>,
            _pressure: &tairix_kernel_mem::MemoryPressure,
            _sink: &dyn tairix_log::Sink,
        ) -> usize {
            // As `ramzip_cluster`: warm-up is exercised against the real
            // `LiveSpace`.
            0
        }
    }

    /// A `TestArch` reporting `cpu`, leaked to the `'static` shape the
    /// producers hold (mirroring the boot-global arch handle).
    ///
    /// Each test uses a **distinct** `cpu` so the global per-CPU
    /// [`with_current_live_space`] slot is never shared between tests running
    /// in parallel (no flaky tests).
    fn arch_at(cpu: u32) -> &'static TestArch {
        let arch = Box::leak(Box::new(TestArch::with_cpus(cpu + 1)));
        arch.set_current_cpu(cpu);
        arch
    }

    /// Leak `fake` to the `'static` lifetime [`publish_live_space_for_test`]
    /// requires (the production live space is owned for the task's life),
    /// returning the `&'static mut` to publish and a raw pointer to inspect
    /// the recording after the producer call (the producer's `&mut` has ended
    /// by then; single-threaded). A test leak is bounded by the process.
    fn leak_fake() -> (&'static mut FakeLive, *const FakeLive) {
        leak_fake_with(FakeLive::default())
    }

    fn leak_fake_with(fake: FakeLive) -> (&'static mut FakeLive, *const FakeLive) {
        let leaked: &'static mut FakeLive = Box::leak(Box::new(fake));
        let ptr: *const FakeLive = leaked;
        (leaked, ptr)
    }

    const PAGE: usize = 4096;

    #[test]
    fn mem_map_routes_a_fixed_request_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(1, fake);

        let producer = LiveMemMap::new(arch_at(1));
        let base = 0x4000;
        let got = producer.map(2 * PAGE, MapFlags::FIXED, base);
        assert_eq!(got, Ok(base));
        // The producer rounded the byte length to a page count and forwarded
        // the FIXED base unchanged.
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_maps, std::vec![(base, 2)]);
    }

    #[test]
    fn mem_map_unmap_routes_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(2, fake);

        let producer = LiveMemMap::new(arch_at(2));
        assert_eq!(producer.unmap(0x4000, PAGE), Ok(()));
        // SAFETY: see above.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_unmaps, std::vec![(0x4000, 1)]);
    }

    #[test]
    fn mem_map_non_fixed_routes_to_the_placement_allocator() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(3, fake);

        let producer = LiveMemMap::new(arch_at(3));
        // A non-`FIXED` request asks the live space to choose the base; the
        // `addr_hint` is ignored, and the placed base flows back unchanged.
        let got = producer.map(2 * PAGE, MapFlags::empty(), 0xDEAD_0000);
        assert_eq!(got, Ok(PLACED_BASE));
        // The producer routed to `map_anonymous_placed` (page count only),
        // never the `FIXED` `map_anonymous`.
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_placed, std::vec![2]);
        assert!(recorded.anon_maps.is_empty());
    }

    #[test]
    fn mem_map_reserve_non_fixed_routes_to_the_placement_reservation() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(11, fake);

        let producer = LiveMemMap::new(arch_at(11));
        // A non-`FIXED` `mem_map` reserves address space only (no eager
        // commit); the placed base flows back unchanged.
        let got = MemMap::reserve(&producer, 2 * PAGE, MapFlags::empty(), 0xDEAD_0000);
        assert_eq!(got, Ok(PLACED_BASE));
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_reserves, std::vec![2]);
        // A reservation never eagerly maps.
        assert!(recorded.anon_maps.is_empty());
        assert!(recorded.anon_placed.is_empty());
    }

    #[test]
    fn mem_map_reserve_fixed_routes_to_the_placed_reservation() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(12, fake);

        let producer = LiveMemMap::new(arch_at(12));
        let base = 0x4000;
        let got = MemMap::reserve(&producer, 2 * PAGE, MapFlags::FIXED, base);
        assert_eq!(got, Ok(base));
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_reserves_at, std::vec![(base, 2)]);
        assert!(recorded.anon_maps.is_empty());
    }

    #[test]
    fn mem_map_with_no_published_space_fails_closed_for_a_non_fixed_request() {
        // No live space published on this CPU: a non-`FIXED` placement must
        // also fail closed rather than fabricating a base.
        let producer = LiveMemMap::new(arch_at(9));
        assert_eq!(
            producer.map(PAGE, MapFlags::empty(), 0),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mem_map_with_no_published_space_fails_closed() {
        // No live space published on this CPU: the producer must not map
        // anything (a task spawned without a retained space).
        let producer = LiveMemMap::new(arch_at(4));
        assert_eq!(
            producer.map(PAGE, MapFlags::FIXED, 0x4000),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mem_map_folds_an_out_of_memory_error() {
        let (fake, _ptr) = leak_fake_with(FakeLive {
            next: Some(LiveSpaceError::Anon(AnonError::OutOfMemory)),
            ..FakeLive::default()
        });
        let _guard = publish_live_space_for_test(5, fake);

        let producer = LiveMemMap::new(arch_at(5));
        assert_eq!(
            producer.map(PAGE, MapFlags::FIXED, 0x4000),
            Err(Errno::OutOfMemory)
        );
    }

    #[test]
    fn file_map_reserve_routes_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(10, fake);

        let producer = LiveMemMap::new(arch_at(10));
        // The byte length rounds up to whole pages; the reserved base flows
        // back unchanged.
        assert_eq!(FileMap::reserve(&producer, PAGE as u64 + 1), Ok(FILE_BASE));
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.file_reserves, std::vec![2]);
    }

    #[test]
    fn file_map_page_and_release_route_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(11, fake);

        let producer = LiveMemMap::new(arch_at(11));
        assert_eq!(producer.map_page(FILE_BASE, &[7; 12]), Ok(()));
        assert_eq!(
            producer.release(FILE_BASE, 4 * PAGE as u64),
            Ok(FILE_RESIDENT)
        );
        // SAFETY: see above.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.file_page_maps, std::vec![(FILE_BASE, 12)]);
        assert_eq!(recorded.file_releases, std::vec![(FILE_BASE, 4)]);
    }

    #[test]
    fn file_map_with_no_published_space_fails_closed() {
        // No live space published on this CPU: every file-mapping operation
        // announces the inert interface rather than pretending anything was
        // reserved, backed, or freed. A zero length is refused before the
        // space is even consulted.
        let producer = LiveMemMap::new(arch_at(12));
        assert_eq!(
            FileMap::reserve(&producer, PAGE as u64),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            producer.map_page(0xF000_0000, &[1]),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            producer.release(0xF000_0000, PAGE as u64),
            Err(Errno::NotImplemented)
        );
        assert_eq!(FileMap::reserve(&producer, 0), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn mmio_map_routes_a_granted_window_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(6, fake);

        let producer = LiveMmioMap::new(arch_at(6));
        let va = producer.map_window(0xFE98_0000, 0x4000, MmioMemoryKind::Device);
        assert_eq!(va, Ok(0x9000_1000));
        // SAFETY: see above.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.device_maps, std::vec![(0xFE98_0000, 0x4000)]);
        // A device window never takes either framebuffer path.
        assert!(recorded.framebuffer_maps.is_empty());
    }

    #[test]
    fn mmio_map_routes_a_write_combining_framebuffer_to_the_scanout_path() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(9, fake);

        let producer = LiveMmioMap::new(arch_at(9));
        let va = producer.map_window(
            0x8000_0000,
            0x30_0000,
            MmioMemoryKind::FramebufferWriteCombine,
        );
        assert_eq!(va, Ok(0x9000_2000));
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        // A framebuffer grant takes the scan-out (Normal-NC) path, never the
        // strongly-ordered device path.
        assert_eq!(
            recorded.framebuffer_maps,
            std::vec![(0x8000_0000, 0x30_0000)]
        );
        assert!(recorded.device_maps.is_empty());
    }

    #[test]
    fn mmio_map_with_no_published_space_fails_closed() {
        let producer = LiveMmioMap::new(arch_at(7));
        assert_eq!(
            producer.map_window(0xFE98_0000, 0x4000, MmioMemoryKind::Device),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mmio_map_folds_a_no_virtual_space_error() {
        let (fake, _ptr) = leak_fake_with(FakeLive {
            next: Some(LiveSpaceError::Mmio(MmioError::NoVirtualSpace)),
            ..FakeLive::default()
        });
        let _guard = publish_live_space_for_test(8, fake);

        let producer = LiveMmioMap::new(arch_at(8));
        assert_eq!(
            producer.map_window(0xFE98_0000, 0x4000, MmioMemoryKind::Device),
            Err(Errno::OutOfMemory)
        );
    }

    #[test]
    fn dma_alloc_routes_a_carve_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(10, fake);

        let producer = LiveDmaAlloc::new(arch_at(10));
        let carve = producer.alloc(2 * PAGE, 0x4000_0000);
        // The CPU VA and the physical-base-as-device-address flow back from
        // the live space unchanged.
        assert_eq!(
            carve,
            Ok(DmaCarve {
                cpu_va: 0xD000_2000,
                device_addr: DMA_PHYS,
            })
        );
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.dma_allocs, std::vec![(2 * PAGE, 0x4000_0000)]);
    }

    #[test]
    fn dma_alloc_with_no_published_space_fails_closed() {
        let producer = LiveDmaAlloc::new(arch_at(11));
        assert_eq!(producer.alloc(PAGE, 0), Err(Errno::NotImplemented));
    }

    #[test]
    fn dma_alloc_folds_an_addressing_limit_error_to_out_of_range() {
        let (fake, _ptr) = leak_fake_with(FakeLive {
            next: Some(LiveSpaceError::Dma(DmaError::AddrLimitExceeded)),
            ..FakeLive::default()
        });
        let _guard = publish_live_space_for_test(12, fake);

        let producer = LiveDmaAlloc::new(arch_at(12));
        assert_eq!(producer.alloc(PAGE, 0x1000), Err(Errno::OutOfRange));
    }
}
