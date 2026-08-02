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

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_log::Sink;
use tairix_reclaim::MemoryPressure;
use tairix_sync::SpinLock;

use crate::anon::{map_anonymous, unmap_anonymous, zero_frame, AnonError};
use crate::anon_window::AnonWindowMap;
use crate::coldscan::{ColdPageScanner, ColdScanError};
use crate::dma::{DmaError, DmaWindowMap};
use crate::filemap::{map_file_page, unmap_file_region};
use crate::frame::{Frame, FrameAllocator, PAGE_SIZE};
use crate::mmio::{MmioError, MmioWindowMap};
use crate::phys::PhysMap;
use crate::ramzip::{
    FaultError, PageCandidate, Ramzip, RamzipFaultOutcome, RamzipReclaimSummary, VmContext,
    WarmOutcome,
};
use crate::vmm::{AddressSpace, FrozenAddressSpace, MapFlags, Page, PageTable, VirtAddr};

/// Monotonic allocator of the stable per-address-space id the global
/// [`Ramzip`] tier keys a space's compressed entries on and the audit
/// log records.
///
/// A live space is minted with the next id here, exactly as a process
/// id is allocated: ids are never reused within a boot, so a space's
/// entries can never be confused with a later space's. `u64` never
/// wraps in any realistic boot (2^64 spawns).
static NEXT_SPACE_ID: AtomicU64 = AtomicU64::new(1);

/// Mint the next unique live-address-space id.
fn next_space_id() -> u64 {
    NEXT_SPACE_ID.fetch_add(1, Ordering::Relaxed)
}

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

    /// Reserve `page_count` pages of *address space* for a demand-paged
    /// **anonymous** mapping at a **kernel-chosen** base (the non-`FIXED`
    /// `mem_map`), returning that base. No page-table entry is written and
    /// no frame is drawn: the region is backed one zeroed `RW|USER` page at
    /// a time by [`Self::map_anonymous`] from the kernel's anonymous fault
    /// path, so reserving a large region costs nothing until it is touched
    /// — a huge `mem_map` never zeroes and commits thousands of pages in
    /// one non-preemptible syscall.
    ///
    /// The placement is drawn from this task's anonymous heap window so a
    /// second reservation never overlaps the first, the program image, its
    /// stack, or a granted device window. Anonymous pages must all be
    /// backable, so the heap window's span is clamped to physical RAM.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::OutOfMemory`] when the heap
    /// window is exhausted, or the precise placement error otherwise.
    fn reserve_anonymous(&mut self, page_count: u64) -> Result<u64, LiveSpaceError>;

    /// Reserve exactly the `page_count`-page anonymous region based at the
    /// page-aligned `base_va` (the `FIXED` `mem_map`), returning `base_va`.
    /// Like [`Self::reserve_anonymous`] this commits no frame and writes no
    /// page-table entry; the pages fault in one at a time. The caller owns
    /// the placement, so a `base_va` that later collides with resident
    /// memory fails closed when its first page faults in.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::ZeroLength`] for a zero page
    /// count, [`AnonError::Unaligned`] for a misaligned base.
    fn reserve_anonymous_at(
        &mut self,
        base_va: u64,
        page_count: u64,
    ) -> Result<u64, LiveSpaceError>;

    /// Reserve physical headroom for `page_count` demand-paged anonymous
    /// pages whose *address space is already reserved* — the stack-growth
    /// case, where the growth room was carved at spawn and only the
    /// no-overcommit commitment is taken here, one growth step at a time,
    /// immediately before the pages are faulted in by [`Self::map_anonymous`].
    ///
    /// This is the commitment half of [`Self::reserve_anonymous`] without the
    /// address-space placement (the stack span owns the address space). The
    /// committed pages join this space's outstanding tally and are consumed
    /// as they fault in or released on teardown, exactly like a `mem_map`
    /// reservation, so stack growth is bounded by the same no-overcommit
    /// budget and fails closed rather than killing the task on first touch.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] ([`AnonError::OutOfMemory`]) when the
    /// commitment cannot be admitted without overcommitting.
    fn commit_anonymous(&mut self, page_count: u64) -> Result<(), LiveSpaceError>;

    /// Reserve `page_count` pages of *address space* for a demand-paged,
    /// read-only file mapping at a **kernel-chosen** base, returning that
    /// base. No page table entry is written and no frame is drawn: the
    /// region is backed one page at a time by [`Self::map_file_page_at`]
    /// from the kernel's fault path, so reserving a huge region costs
    /// nothing until it is touched.
    ///
    /// The placement is drawn from this task's file-mapping window —
    /// deliberately separate from the anonymous heap window, whose span is
    /// clamped to physical RAM (anonymous pages must all be backable); a
    /// file mapping is bounded only by address space.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::OutOfMemory`] when the file
    /// window is exhausted (or absent on a degenerate configuration with no
    /// address space left above the heap window), or the precise
    /// placement error otherwise.
    fn reserve_file_region(&mut self, page_count: u64) -> Result<u64, LiveSpaceError>;

    /// Make the single page at the page-aligned `va` resident inside a
    /// region previously returned by [`Self::reserve_file_region`],
    /// carrying `contents` (at most one page; a short slice is the page
    /// straddling end-of-file, zero-filled past it). The page is mapped
    /// read-only and never executable.
    ///
    /// `va` must lie inside a *live* reserved file region: an address in
    /// the window but outside every reservation is refused (fail closed) —
    /// the fault path never backs address space the task did not map.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::NotMapped`] when `va` is not
    /// covered by a live file region, [`AnonError::Map`] when the page is
    /// already resident (a benign fault race; the caller retries the
    /// access), or the precise frame/fill error otherwise.
    fn map_file_page_at(&mut self, va: u64, contents: &[u8]) -> Result<(), LiveSpaceError>;

    /// Release the whole file region based at `base_va` (`page_count`
    /// pages, exactly as reserved), sparsely unmapping the pages fault
    /// history made resident — zeroing each frame on free — and returning
    /// the reservation to the file window. Returns the number of pages
    /// that were resident.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Anon`] — [`AnonError::NotMapped`] when
    /// `(base_va, page_count)` is not a live file region of this space
    /// (fail closed: nothing is torn down).
    fn release_file_region(&mut self, base_va: u64, page_count: u64)
        -> Result<u64, LiveSpaceError>;

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

    /// Map `len` bytes of a **linear framebuffer** beginning at `phys_base`
    /// into this space, returning the kernel-chosen base user virtual address
    /// of the new, guard-bracketed, non-executable window.
    ///
    /// Identical guard-bracketed slot mechanism as [`Self::map_device_window`],
    /// but the scan-out pages are mapped non-cacheable **Normal** memory
    /// instead of Device-strongly-ordered: a framebuffer is bulk pixel memory
    /// the CPU fills a whole frame at a time and the display engine reads back
    /// as a DMA master, so the slow per-access Device ordering registers need
    /// is exactly wrong for it (a full-frame blit of Device memory costs tens
    /// of seconds under emulation). The producer has already resolved and
    /// validated the framebuffer grant the window comes from (owner-checked,
    /// kind, length); this only performs the page-table mechanism.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] carrying the precise [`MmioError`] (no free
    /// virtual slot, page-table refusal, …).
    fn map_framebuffer_window(&mut self, phys_base: u64, len: usize)
        -> Result<u64, LiveSpaceError>;

    /// Map a linear framebuffer backed by ordinary coherent RAM as
    /// guard-bracketed, non-executable, write-back memory.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] carrying the precise [`MmioError`].
    fn map_writeback_framebuffer_window(
        &mut self,
        phys_base: u64,
        len: usize,
    ) -> Result<u64, LiveSpaceError>;

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

    /// Map an existing, kernel-owned **shared-memory region** whose backing
    /// is a *list* of physically-contiguous chunks (`(phys_base, pages)`)
    /// into this space as one contiguous, cacheable `RW|USER` (never
    /// executable), guard-bracketed window, returning the kernel-chosen base
    /// user virtual address.
    ///
    /// The chunked form of [`Self::map_shared`]: a region larger than the
    /// frame allocator's single-block ceiling is backed by several
    /// blocks the shared-region registry allocated, and this maps them
    /// back-to-back in virtual space so the process sees one flat buffer (the
    /// display frame ring). As with [`Self::map_shared`] the frames belong to
    /// the registry; this installs page-table entries only, released by
    /// [`Self::unmap_shared`] or a space drop without freeing the frames.
    ///
    /// # Errors
    ///
    /// [`LiveSpaceError::Mmio`] carrying the precise [`MmioError`] (an empty
    /// or malformed chunk list, no free virtual run, a page-table refusal) —
    /// the shared mapping reuses the guarded-window mechanism.
    fn map_shared_chunks(&mut self, chunks: &[(u64, u64)]) -> Result<u64, LiveSpaceError>;

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

    /// Translate a single `page` to the `(frame, flags)` the backend
    /// resolves it to, or `None` when it is not mapped.
    ///
    /// The O(1) counterpart to [`Self::freeze`]: after the demand-fault
    /// resolver backs one page it reads that page's mapping through here and
    /// applies it to the registry snapshot as a single-page delta, instead of
    /// re-freezing the whole space per fault (which is O(N²) over a large
    /// mapping). Object-safe so `kernel/core` needs no concrete backend `P`.
    fn translate_page(&self, page: Page) -> Option<(Frame, MapFlags)>;

    /// Restore a compressed anonymous page on a demand fault at user
    /// virtual address `va`, through the process-global [`Ramzip`]
    /// tier.
    ///
    /// The fault resolver calls this **before** the plain anonymous
    /// handler: a compressed page is *reserved* anonymous memory, so
    /// the anonymous handler would otherwise re-zero it and destroy the
    /// compressed contents. [`RamzipFaultOutcome::NoEntry`] means no
    /// compressed entry existed (fall through to the anonymous
    /// handler); [`RamzipFaultOutcome::Handled`] means the page is now
    /// resident (retry the instruction);
    /// [`RamzipFaultOutcome::Fatal`] means the entry was unrecoverable
    /// (fail closed, no plaintext, escalate the fault). `sink` records
    /// authentication/corruption failures.
    fn ramzip_fault_in(
        &mut self,
        tier: &SpinLock<Ramzip>,
        va: u64,
        sink: &dyn Sink,
    ) -> RamzipFaultOutcome;

    /// Compress up to `want` of this space's cold anonymous pages out
    /// into the global [`Ramzip`] tier under memory pressure, and
    /// report what the sweep did.
    ///
    /// The candidate set is this space's resident *placed anonymous*
    /// pages (its heap window); the owned second-chance
    /// [`ColdPageScanner`] decides which are cold (fails closed,
    /// reclaiming nothing, on a backend with no referenced bit), and
    /// the tier's own gates (pressure handoff, caps, per-task share,
    /// decompression floor, eligibility) decide which cold pages are
    /// admitted. `template` carries the task-level attributes the
    /// caller knows (pinned / sensitive / latency-critical); a pinned
    /// task yields nothing. `reclaimable_residue` is the cheaper-cache
    /// residue the tier waits to drain first.
    fn ramzip_reclaim(
        &mut self,
        tier: &SpinLock<Ramzip>,
        pressure: &MemoryPressure,
        reclaimable_residue: usize,
        want: usize,
        template: PageCandidate,
        sink: &dyn Sink,
    ) -> RamzipReclaimSummary;

    /// Opportunistically restore compressed entries adjacent to the
    /// page just faulted in at `va`, through the global [`Ramzip`]
    /// tier, returning how many pages were brought back.
    ///
    /// The fault resolver calls this **after** a successful
    /// [`Self::ramzip_fault_in`]: that demand fault recorded the
    /// locality evidence this consumes. The tier restores only
    /// same-space neighbours within its cluster radius that were
    /// sealed close in time to the faulted entry, and only while free
    /// memory is comfortably above the warm-up watermark with the
    /// decompression floor protected — so clustering never runs under
    /// pressure and never draws down a reserve. It is best-effort: a
    /// restore failure never propagates (the original fault already
    /// succeeded), and `sink` records any authentication/corruption
    /// loss. The caller republishes the restored pages to the registry
    /// snapshot.
    fn ramzip_cluster(
        &mut self,
        tier: &SpinLock<Ramzip>,
        pressure: &MemoryPressure,
        va: u64,
        sink: &dyn Sink,
    ) -> usize;

    /// Run one bounded, opportunistic warm-up step over this space's
    /// compressed entries, returning how many pages were restored.
    ///
    /// The move-only background-warm optimisation: it restores up to a
    /// fixed batch of entries near recent demand faults, only while
    /// free memory is comfortably above the warm-up watermark
    /// (re-checked before every page, so it stops the instant pressure
    /// returns) and never touching the decompression reserve. A tier
    /// with no fault-locality evidence restores nothing — keeping cold
    /// pages compressed is the design. There is no timer here: the
    /// resolver drives one step when a compressed page is faulted back
    /// and memory is comfortable, so the cost is charged to the
    /// resuming task rather than a background daemon. The caller
    /// republishes the restored pages.
    fn ramzip_warm(
        &mut self,
        tier: &SpinLock<Ramzip>,
        pressure: &MemoryPressure,
        sink: &dyn Sink,
    ) -> usize;
}

/// The generic concrete live address space retained per task.
///
/// Generic over the port's [`PageTable`] backend `P` and the kernel direct
/// map `M`, so it is constructed by the architecture spawn producer (which
/// names both) and stored behind [`LiveUserSpace`] by `kernel/core` (which
/// names neither). It owns:
///
/// * `space` — the live arch [`AddressSpace<P>`] (its page-table frames come
///   from the backend's own [`PageTableFrames`](tairix_arch_api::frames::PageTableFrames)
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
///   shared-region registry);
/// * `file` — the per-task placement allocator for demand-paged file
///   mappings, over its own window above the heap window: reservations are
///   pure address space, backed one page at a time by the fault path
///   ([`crate::filemap`]). `None` on a degenerate configuration whose user
///   address space has no room above the heap window (file mapping then
///   fails closed as a deterministic OOM).
/// * `space_id` — the stable, monotonically minted id the process-global
///   [`Ramzip`] tier keys this space's compressed entries on;
/// * `scanner` — this space's own second-chance [`ColdPageScanner`], so
///   the reclaim clock hand rotates independently per space.
pub struct LiveSpace<P: PageTable, M: PhysMap> {
    space: AddressSpace<P>,
    physmap: M,
    frames: &'static FrameAllocator,
    mmio: MmioWindowMap,
    anon: AnonWindowMap,
    dma: DmaWindowMap,
    shared: MmioWindowMap,
    file: Option<AnonWindowMap>,
    space_id: u64,
    scanner: ColdPageScanner,
    /// Frames this space has committed for demand-paged anonymous memory
    /// (`mem_map` reservations and per-page stack growth) but which have
    /// not yet faulted in. Charged against the global no-overcommit budget
    /// ([`FrameAllocator::commit`]) at reservation, decremented as a page
    /// becomes resident or its reservation is released, and released in full
    /// on teardown ([`Drop`]) so a task that dies with untouched
    /// reservations returns exactly its outstanding commitment to the pool.
    committed_unbacked: u64,
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
        file_window_base: VirtAddr,
        file_window_pages: usize,
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
        // The file-mapping window is the one window that may legitimately be
        // empty: a degenerate configuration with no address space left above
        // the heap window still spawns, and file mapping fails closed at
        // reservation time. A non-empty window is validated like the others.
        let file = if file_window_pages == 0 {
            None
        } else {
            Some(
                AnonWindowMap::new(file_window_base, file_window_pages)
                    .map_err(|_| MmioError::InvalidMapConfig)?,
            )
        };
        Ok(Self {
            space,
            physmap,
            frames,
            mmio,
            anon,
            dma,
            shared,
            file,
            space_id: next_space_id(),
            scanner: ColdPageScanner::new(),
            committed_unbacked: 0,
        })
    }

    /// This space's stable id in the global [`Ramzip`] tier
    /// (diagnostic / test observer).
    #[must_use]
    pub fn space_id(&self) -> u64 {
        self.space_id
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
        // This is the demand-fault path: every page here was reserved ahead
        // of time (a `mem_map` reservation, or a stack-growth commit) against
        // the no-overcommit budget, so it draws through the *committed* path
        // — guaranteed a frame, and not re-gated by the kernel reserve the
        // reservation already cleared. An unwound page returns to its
        // reservation (`free_committed`) so the commitment survives a
        // page-table-build failure without drifting the counter.
        map_anonymous(
            &mut self.space,
            &self.physmap,
            base_va,
            page_count,
            || frames.alloc_user_committed().ok(),
            |frame| {
                let _ = frames.free_committed(frame);
            },
        )?;
        // The reserved pages are now resident: convert them from
        // committed-unbacked headroom to real residency (the per-page
        // `alloc_user_committed` already lowered the global counter; keep the
        // per-space tally in step so teardown releases exactly the pages that
        // never faulted in).
        self.committed_unbacked = self.committed_unbacked.saturating_sub(page_count);
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
            // Reserve-gated user commit (see `map_anonymous`).
            || frames.alloc_user().ok(),
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
        // A file-mapped region is released only through
        // `release_file_region` (its residency is sparse and its
        // bookkeeping lives in the file window): an anonymous unmap naming
        // an address in the file window is the wrong call and fails closed
        // before any teardown.
        if self.file.as_ref().is_some_and(|file| file.owns(base_va)) {
            return Err(LiveSpaceError::Anon(AnonError::NotMapped));
        }
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
        // Count the frames actually reclaimed: a demand-paged region is
        // sparsely resident, so the resident count tells us how many of its
        // pages ever faulted in. The remainder are still committed-unbacked
        // reservations whose physical headroom must be returned to the pool.
        let mut resident = 0u64;
        unmap_anonymous(
            &mut self.space,
            &self.physmap,
            base_va,
            page_count,
            |frame| {
                resident = resident.saturating_add(1);
                let _ = frames.free(frame);
            },
        )?;
        // Release the reservation for every page that never became resident
        // (an eager, fully-resident region leaves nothing to release). The
        // resident pages already left the committed tally when they faulted
        // in; their frames were just freed above.
        let unbacked = page_count.saturating_sub(resident);
        if unbacked > 0 {
            self.frames.uncommit(unbacked);
            self.committed_unbacked = self.committed_unbacked.saturating_sub(unbacked);
        }
        if placed {
            // Validated above, so the release matches; ignore its result.
            let _ = self.anon.release(base_va, page_count);
        }
        Ok(())
    }

    fn reserve_anonymous(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        // No-overcommit admission first: reserve physical headroom for every
        // page before handing back any address space, so a `mem_map` that
        // could not later be backed fails *here* as a deterministic
        // `OutOfMemory` rather than as a fault-time kill on first touch. Only
        // then place the region out of this task's heap window; no page-table
        // entry and no frame move until a fault lands, and the pages fault in
        // one at a time through `map_anonymous`.
        self.frames
            .commit(page_count)
            .map_err(|_| LiveSpaceError::Anon(AnonError::OutOfMemory))?;
        match self.anon.allocate(page_count) {
            Ok(base_va) => {
                self.committed_unbacked = self.committed_unbacked.saturating_add(page_count);
                Ok(base_va)
            }
            Err(err) => {
                // The address-space placement failed: return the commitment
                // we just made so a failed reservation consumes no headroom.
                self.frames.uncommit(page_count);
                Err(err.into())
            }
        }
    }

    fn reserve_anonymous_at(
        &mut self,
        base_va: u64,
        page_count: u64,
    ) -> Result<u64, LiveSpaceError> {
        // A caller-placed (`FIXED`) reservation records no window entry: it
        // is torn down by extent, and the fault path fails closed if a page
        // it backs collides with resident memory. Validate the shape first,
        // *before* charging the commitment, so a malformed request reserves
        // nothing.
        if page_count == 0 {
            return Err(LiveSpaceError::Anon(AnonError::ZeroLength));
        }
        if !base_va.is_multiple_of(crate::frame::PAGE_SIZE as u64) {
            return Err(LiveSpaceError::Anon(AnonError::Unaligned));
        }
        // No-overcommit admission (see `reserve_anonymous`): reserve physical
        // headroom for the whole extent up front, so a first touch is
        // guaranteed a frame.
        self.frames
            .commit(page_count)
            .map_err(|_| LiveSpaceError::Anon(AnonError::OutOfMemory))?;
        self.committed_unbacked = self.committed_unbacked.saturating_add(page_count);
        Ok(base_va)
    }

    fn commit_anonymous(&mut self, page_count: u64) -> Result<(), LiveSpaceError> {
        // Commitment only — the address space (the stack span's growth room)
        // is already reserved. Charge the no-overcommit budget and join the
        // pages to this space's outstanding tally; `map_anonymous` consumes
        // them as each faults in, and teardown releases any that never did.
        self.frames
            .commit(page_count)
            .map_err(|_| LiveSpaceError::Anon(AnonError::OutOfMemory))?;
        self.committed_unbacked = self.committed_unbacked.saturating_add(page_count);
        Ok(())
    }

    fn reserve_file_region(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
        // Pure placement: no page table entry and no frame until a fault
        // lands in the region. An absent window (degenerate configuration)
        // is the same deterministic refusal as an exhausted one.
        let file = self
            .file
            .as_mut()
            .ok_or(LiveSpaceError::Anon(AnonError::OutOfMemory))?;
        Ok(file.allocate(page_count)?)
    }

    fn map_file_page_at(&mut self, va: u64, contents: &[u8]) -> Result<(), LiveSpaceError> {
        // Only an address inside a live reserved file region is ever
        // backed: the fault path must not be able to materialise memory
        // the task never mapped (fail closed before any frame is drawn).
        if !self.file.as_ref().is_some_and(|file| file.covers(va)) {
            return Err(LiveSpaceError::Anon(AnonError::NotMapped));
        }
        let frames = self.frames;
        map_file_page(
            &mut self.space,
            &self.physmap,
            va,
            contents,
            || frames.alloc().ok(),
            |frame| {
                // The frame never became user-visible; returning it to the
                // allocator cannot fail meaningfully (best-effort, never a
                // panic).
                let _ = frames.free(frame);
            },
        )?;
        Ok(())
    }

    fn release_file_region(
        &mut self,
        base_va: u64,
        page_count: u64,
    ) -> Result<u64, LiveSpaceError> {
        // The (base, extent) must name a live reservation exactly before
        // any teardown, so a bad pair fails closed without touching a
        // neighbour's pages; residency inside the region is fault history
        // and legitimately sparse.
        let file = self
            .file
            .as_mut()
            .ok_or(LiveSpaceError::Anon(AnonError::NotMapped))?;
        file.validate(base_va, page_count)?;
        let frames = self.frames;
        let resident = unmap_file_region(
            &mut self.space,
            &self.physmap,
            base_va,
            page_count,
            |frame| {
                let _ = frames.free(frame);
            },
        )?;
        // Validated above, so the release matches; ignore its result.
        let _ = file.release(base_va, page_count);
        Ok(resident)
    }

    fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError> {
        let region = self.mmio.map_into(&mut self.space, phys_base, len)?;
        Ok(region.virt().as_u64())
    }

    fn map_framebuffer_window(
        &mut self,
        phys_base: u64,
        len: usize,
    ) -> Result<u64, LiveSpaceError> {
        let region = self
            .mmio
            .map_framebuffer_into(&mut self.space, phys_base, len)?;
        Ok(region.virt().as_u64())
    }

    fn map_writeback_framebuffer_window(
        &mut self,
        phys_base: u64,
        len: usize,
    ) -> Result<u64, LiveSpaceError> {
        let region = self
            .mmio
            .map_cacheable_into(&mut self.space, phys_base, len)?;
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

    fn map_shared_chunks(&mut self, chunks: &[(u64, u64)]) -> Result<u64, LiveSpaceError> {
        // Reuse the guarded-window mechanism, mapping the chunk list into one
        // contiguous virtual window (cacheable RAM). The frames belong to the
        // shared-region registry, so the space installs page-table entries
        // only — released without freeing the frames (a second process may
        // still map them).
        let region = self
            .shared
            .map_cacheable_chunks_into(&mut self.space, chunks)?;
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

    fn translate_page(&self, page: Page) -> Option<(Frame, MapFlags)> {
        self.space.translate(page)
    }

    fn ramzip_fault_in(
        &mut self,
        tier: &SpinLock<Ramzip>,
        va: u64,
        sink: &dyn Sink,
    ) -> RamzipFaultOutcome {
        // Round down to the faulting page; a misaligned or out-of-range
        // address has no compressed entry (fall through).
        let Ok(page) = Page::from_addr(VirtAddr::new(va & !((PAGE_SIZE as u64) - 1))) else {
            return RamzipFaultOutcome::NoEntry;
        };
        let space_id = self.space_id;
        let mut tier = tier.lock();
        let mut ctx = VmContext {
            space_id,
            space: &mut self.space,
            physmap: &self.physmap,
            frames: self.frames,
            sink,
        };
        match tier.fault_in(&mut ctx, page) {
            Ok(()) => RamzipFaultOutcome::Handled,
            // No entry, or the page is already resident (a spurious or
            // raced fault): the anonymous handler takes it from here.
            Err(FaultError::NoEntry | FaultError::AlreadyMapped) => RamzipFaultOutcome::NoEntry,
            Err(other) => RamzipFaultOutcome::Fatal(other),
        }
    }

    fn ramzip_reclaim(
        &mut self,
        tier: &SpinLock<Ramzip>,
        pressure: &MemoryPressure,
        reclaimable_residue: usize,
        want: usize,
        template: PageCandidate,
        sink: &dyn Sink,
    ) -> RamzipReclaimSummary {
        if want == 0 {
            return RamzipReclaimSummary::default();
        }
        // Cheap early-out before the O(n) candidate collection: on a
        // backend with no referenced bit the scanner can never show a page
        // cold, so reclaim nothing (fail closed) without walking the whole
        // live mapping on every triggering fault. The scanner re-checks
        // this too, but only after the caller has paid for the candidate
        // list — this keeps direct reclaim near-free on a port that has
        // not yet enabled access tracking.
        if !self.space.access_tracking().is_supported() {
            return RamzipReclaimSummary {
                access_tracking_unsupported: true,
                ..RamzipReclaimSummary::default()
            };
        }
        // Candidate set: this space's resident *placed anonymous* pages
        // (its heap window), ascending and unique — the order the
        // second-chance scanner requires. Image, stack, file, device,
        // DMA, and shared pages are all excluded here: only pages the
        // window allocator proved anonymous are ever offered.
        let mut candidates: Vec<Page> = self
            .space
            .live_pages()
            .filter(|page| self.anon.covers(page.start().as_u64()))
            .collect();
        candidates.sort_unstable_by_key(|page| page.number());
        candidates.dedup_by_key(|page| page.number());

        let mut tier = tier.lock();
        let cold = match self.scanner.scan(&mut self.space, &candidates, want) {
            Ok(cold) => cold,
            // No referenced bit on this backend: reclaim nothing rather
            // than guess a page is cold (fail closed).
            Err(ColdScanError::Unsupported) => {
                return RamzipReclaimSummary {
                    access_tracking_unsupported: true,
                    ..RamzipReclaimSummary::default()
                };
            }
        };
        let mut summary = RamzipReclaimSummary {
            scanned: candidates.len(),
            ..RamzipReclaimSummary::default()
        };
        let space_id = self.space_id;
        for page in cold {
            let mut ctx = VmContext {
                space_id,
                space: &mut self.space,
                physmap: &self.physmap,
                frames: self.frames,
                sink,
            };
            // The owning task is the space itself (one task per space),
            // so its tier task id is the space id.
            match tier.compress_out(
                pressure,
                reclaimable_residue,
                &mut ctx,
                page,
                space_id,
                &template,
            ) {
                Ok(()) => summary.compressed += 1,
                Err(_) => summary.refused += 1,
            }
        }
        summary
    }

    fn ramzip_cluster(
        &mut self,
        tier: &SpinLock<Ramzip>,
        pressure: &MemoryPressure,
        va: u64,
        sink: &dyn Sink,
    ) -> usize {
        // A misaligned or out-of-range address names no faulted page to
        // cluster around: restore nothing (fail closed).
        let Ok(page) = Page::from_addr(VirtAddr::new(va & !((PAGE_SIZE as u64) - 1))) else {
            return 0;
        };
        let space_id = self.space_id;
        let mut tier = tier.lock();
        let mut ctx = VmContext {
            space_id,
            space: &mut self.space,
            physmap: &self.physmap,
            frames: self.frames,
            sink,
        };
        tier.cluster_after_fault(pressure, &mut ctx, page)
    }

    fn ramzip_warm(
        &mut self,
        tier: &SpinLock<Ramzip>,
        pressure: &MemoryPressure,
        sink: &dyn Sink,
    ) -> usize {
        let space_id = self.space_id;
        let mut tier = tier.lock();
        let mut ctx = VmContext {
            space_id,
            space: &mut self.space,
            physmap: &self.physmap,
            frames: self.frames,
            sink,
        };
        match tier.warm_step(pressure, &mut ctx) {
            WarmOutcome::Restored(restored) => restored,
            // No candidate had locality evidence, or a gate closed
            // mid-step: nothing became resident either way.
            WarmOutcome::NothingToDo | WarmOutcome::Stopped => 0,
        }
    }
}

impl<P: PageTable, M: PhysMap> Drop for LiveSpace<P, M> {
    /// Reclaim the dead task's **entire** memory footprint — the fix for
    /// the login/logout RAM leak (`plans/APPS.md` I2): before this, exit
    /// reclaimed the kernel stack, grants, endpoints, and shared regions,
    /// but every user frame (image, stack, startup block, anonymous heap)
    /// and every page-table frame leaked.
    fn drop(&mut self) {
        // 0. Purge this space's compressed entries from the global tier
        //    (if one is installed): a global pool keyed by space id must
        //    not keep a dead space's entries — nothing would ever fault
        //    them back in, so their RAM (sealed blobs) and ledger charge
        //    would leak. Freeing them here restores the frame/ledger
        //    balance the exit-cycle leak tests assert.
        if let Some(tier) = crate::ramzip::global() {
            tier.lock().purge_space(self.space_id);
        }

        // 1. Reclaim every live DMA buffer: each physically-contiguous
        //    backing block is zeroed (zero-on-free) and returned to the
        //    frame allocator, and its pages leave the space's bookkeeping.
        self.dma
            .drain_into(&mut self.space, self.frames, &self.physmap);

        // 2. Release every remaining tracked mapping. A page inside the
        //    device-window or shared-memory window is only *unmapped* —
        //    its frame belongs to a device (MMIO registers) or to the
        //    shared-region registry, which zeroes and frees region frames
        //    itself once the owner and every grantee have released them.
        //    Every other page (image segments, user stack, startup block,
        //    anonymous heap — `FIXED` and placed alike) is backed by a
        //    frame this task drew from the kernel allocator: it is zeroed
        //    (no dead process's bytes are ever recycled readable) and
        //    freed. A frame the direct map cannot reach is leaked rather
        //    than freed unscrubbed (fail closed; unreachable in practice
        //    — every allocator frame lies in the direct map).
        let pages: Vec<_> = self.space.live_pages().collect();
        for page in pages {
            let window_only =
                self.mmio.contains(page.start()) || self.shared.contains(page.start());
            // The page is recorded live, so the unmap can only fail on a
            // backend defect; declining to touch the frame is the only
            // safe recovery (never a panic).
            let Ok(frame) = self.space.unmap(page) else {
                continue;
            };
            if window_only {
                continue;
            }
            if zero_frame(&self.physmap, frame).is_ok() {
                let _ = self.frames.free(frame);
            }
        }

        // 3. Return the page-table frames themselves — the root and every
        //    intermediate table — to the source they were drawn from.
        // SAFETY: the owning task has exited and the dispatcher parked
        // every CPU that ran it off its root at switch-back (the port's
        // reclaim additionally re-parks defensively if the calling CPU
        // still holds this root); `self` is being dropped, so no other
        // reference into the tables is live.
        unsafe { self.space.reclaim_table_frames() };

        // 4. Return this space's outstanding no-overcommit reservations —
        //    the pages a `mem_map`/stack commit reserved physical headroom
        //    for but which never faulted in — to the global budget. Their
        //    resident siblings already left the tally as they faulted in and
        //    had their frames freed in step 2, so a task that dies holding
        //    untouched reservations leaks no committed headroom.
        if self.committed_unbacked > 0 {
            self.frames.uncommit(self.committed_unbacked);
            self.committed_unbacked = 0;
        }
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

    use crate::phys::PhysMap;

    /// A `PhysMap` view over one leaked, shared [`SimPhysMap`], so a test
    /// observes the same simulated physical memory the live space writes
    /// and scrubs through (each `sim()` owns disjoint storage).
    struct SharedSim(&'static SimPhysMap);
    impl PhysMap for SharedSim {
        fn translate(
            &self,
            phys: crate::frame::PhysAddr,
            len: usize,
        ) -> Option<core::ptr::NonNull<u8>> {
            self.0.translate(phys, len)
        }

        fn clean_invalidate(&self, phys: crate::frame::PhysAddr, len: usize) {
            self.0.clean_invalidate(phys, len);
        }

        fn sync_instruction_cache(&self, phys: crate::frame::PhysAddr, len: usize) {
            self.0.sync_instruction_cache(phys, len);
        }
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

    /// The user virtual window demand-paged file mappings are reserved in —
    /// distinct from every window and `FIXED` region above.
    const FILE_WINDOW_BASE: u64 = 0x3_0000_0000;
    const FILE_WINDOW_PAGES: usize = 64;

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
            VirtAddr::new(FILE_WINDOW_BASE),
            FILE_WINDOW_PAGES,
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
    fn unmap_of_a_sparsely_resident_range_reclaims_only_the_resident_pages() {
        let mut live = live();
        let base = 0x4000;
        // A demand-paged region: two of the three pages ever faulted in (the
        // fault path backs one page at a time via `map_anonymous`), so the
        // third is a never-touched reservation page.
        live.map_anonymous(base, 2).expect("map");
        // Releasing the whole three-page range reclaims the two resident
        // pages and skips the unbacked one — sparse residency is not an
        // error (the caller validates the reservation before it gets here).
        live.unmap_anonymous(base, 3).expect("sparse release");
        assert_eq!(live.space().mapped_pages(), 0);
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
    fn reserve_anonymous_places_without_committing_and_faults_in_on_map() {
        let mut live = live();
        let base = live.reserve_anonymous(4).expect("the heap window has room");
        assert!(
            base >= ANON_WINDOW_BASE
                && base < ANON_WINDOW_BASE + (ANON_WINDOW_PAGES as u64) * PAGE_SIZE as u64,
            "a reserved base lies inside the heap window"
        );
        // A reservation commits nothing: no page is resident yet.
        assert_eq!(live.space().mapped_pages(), 0, "reserve maps no frame");
        // The fault path backs one covering page at a time.
        live.map_anonymous(base + PAGE_SIZE as u64, 1)
            .expect("fault in the second page");
        assert_eq!(live.space().mapped_pages(), 1, "one page faulted in");
        // Releasing the whole reservation reclaims the one resident page and
        // skips the three never-touched ones, and frees the placement.
        live.unmap_anonymous(base, 4).expect("sparse release");
        assert_eq!(live.space().mapped_pages(), 0);
        // The freed placement base is reusable.
        assert_eq!(live.reserve_anonymous(4), Ok(base));
    }

    #[test]
    fn reserve_anonymous_at_validates_shape_and_commits_nothing() {
        let mut live = live();
        let base = 0x8000;
        assert_eq!(live.reserve_anonymous_at(base, 2), Ok(base));
        assert_eq!(live.space().mapped_pages(), 0, "FIXED reserve maps nothing");
        assert_eq!(
            live.reserve_anonymous_at(base, 0),
            Err(LiveSpaceError::Anon(AnonError::ZeroLength))
        );
        assert_eq!(
            live.reserve_anonymous_at(base + 1, 1),
            Err(LiveSpaceError::Anon(AnonError::Unaligned))
        );
    }

    #[test]
    fn reserve_charges_the_commit_budget_and_fault_in_converts_it() {
        // A `mem_map` reservation reserves physical headroom up front (the
        // no-overcommit guarantee), a fault-in converts one reserved page to
        // residency, and unmap returns the still-unbacked reservations — so
        // the committed tally is exactly balanced across the region's life.
        let frames = leaked_frames();
        let (mut live, _sim) = live_over(frames);
        assert_eq!(frames.committed_frames(), 0);

        let base = live.reserve_anonymous(4).expect("heap window has room");
        assert_eq!(
            frames.committed_frames(),
            4,
            "reserve charges the whole extent up front"
        );
        assert_eq!(live.space().mapped_pages(), 0, "but commits no frame yet");

        // Fault one page in: it converts from reserved-unbacked to resident.
        live.map_anonymous(base + PAGE_SIZE as u64, 1)
            .expect("fault in one page");
        assert_eq!(
            frames.committed_frames(),
            3,
            "one reservation became resident"
        );
        assert_eq!(live.space().mapped_pages(), 1);

        // Unmap: the three never-touched reservations are released and the
        // one resident page is freed, returning the tally to zero.
        live.unmap_anonymous(base, 4).expect("sparse release");
        assert_eq!(frames.committed_frames(), 0, "every commitment released");
        assert_eq!(live.space().mapped_pages(), 0);
    }

    #[test]
    fn dropping_a_space_releases_its_untouched_reservations() {
        // A task that dies holding untouched `mem_map` reservations must
        // return that reserved headroom to the global budget, or a
        // long-running system would leak commitment on every short-lived
        // process that reserved but never touched memory.
        let frames = leaked_frames();
        {
            let (mut live, _sim) = live_over(frames);
            live.reserve_anonymous(5).expect("heap window has room");
            assert_eq!(frames.committed_frames(), 5);
        }
        assert_eq!(
            frames.committed_frames(),
            0,
            "teardown released the untouched reservations"
        );
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

    /// The live space's DMA path must route the post-zero cache maintenance
    /// through *its own* `PhysMap` on both allocation and free: the carve is
    /// zeroed through the cacheable direct-map alias while the owning task
    /// reaches the same frames through a non-cacheable mapping, so a live
    /// space built over a physmap whose `clean_invalidate` does nothing
    /// leaves dirty zero lines that are later written back over the task's
    /// device-shared rings (the Pi 4 xHCI regression). This pins the
    /// delegation the spawn producers rely on when they wire the
    /// cache-maintaining physmap into each task's live space.
    #[test]
    fn dma_alloc_and_free_clean_the_direct_map_alias_through_the_spaces_physmap() {
        use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        struct Recorded {
            calls: AtomicUsize,
            last_phys: AtomicU64,
            last_len: AtomicUsize,
        }
        struct RecordingSim {
            inner: SimPhysMap,
            recorded: &'static Recorded,
        }
        impl PhysMap for RecordingSim {
            fn translate(
                &self,
                phys: crate::frame::PhysAddr,
                len: usize,
            ) -> Option<core::ptr::NonNull<u8>> {
                self.inner.translate(phys, len)
            }

            fn clean_invalidate(&self, phys: crate::frame::PhysAddr, len: usize) {
                self.recorded.calls.fetch_add(1, Ordering::Relaxed);
                self.recorded
                    .last_phys
                    .store(phys.as_u64(), Ordering::Relaxed);
                self.recorded.last_len.store(len, Ordering::Relaxed);
            }

            fn sync_instruction_cache(&self, phys: crate::frame::PhysAddr, len: usize) {
                self.inner.sync_instruction_cache(phys, len);
            }
        }

        let recorded: &'static Recorded = Box::leak(Box::new(Recorded {
            calls: AtomicUsize::new(0),
            last_phys: AtomicU64::new(0),
            last_len: AtomicUsize::new(0),
        }));
        let mut live = LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            RecordingSim {
                inner: sim(),
                recorded,
            },
            leaked_frames(),
            VirtAddr::new(MMIO_WINDOW_BASE),
            MMIO_WINDOW_PAGES,
            VirtAddr::new(ANON_WINDOW_BASE),
            ANON_WINDOW_PAGES,
            VirtAddr::new(DMA_WINDOW_BASE),
            DMA_WINDOW_PAGES,
            VirtAddr::new(SHARED_WINDOW_BASE),
            SHARED_WINDOW_PAGES,
            VirtAddr::new(FILE_WINDOW_BASE),
            FILE_WINDOW_PAGES,
        )
        .expect("windows are valid");

        let mapping = live
            .alloc_dma(2 * PAGE_SIZE, 0)
            .expect("a free block exists");
        assert_eq!(
            recorded.calls.load(Ordering::Relaxed),
            1,
            "alloc cleans the alias"
        );
        assert_eq!(
            recorded.last_phys.load(Ordering::Relaxed),
            mapping.phys_base
        );
        assert_eq!(recorded.last_len.load(Ordering::Relaxed), 2 * PAGE_SIZE);

        live.free_dma(mapping.cpu_va).expect("live carve frees");
        assert_eq!(
            recorded.calls.load(Ordering::Relaxed),
            2,
            "free cleans the alias"
        );
        assert_eq!(
            recorded.last_phys.load(Ordering::Relaxed),
            mapping.phys_base
        );
        assert_eq!(recorded.last_len.load(Ordering::Relaxed), 2 * PAGE_SIZE);
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
                VirtAddr::new(FILE_WINDOW_BASE),
                FILE_WINDOW_PAGES,
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
    fn dropping_the_live_space_reclaims_the_whole_footprint() {
        // The I2 regression test (`plans/APPS.md`): a task's exit must
        // return *every* frame it owned — `FIXED` anonymous, placed
        // anonymous, and DMA alike — while leaving registry-owned shared
        // frames and device windows untouched, and scrub the freed bytes.
        let frames = leaked_frames();
        let before = frames.free_frames();
        let simmap: &'static SimPhysMap = Box::leak(Box::new(sim()));

        // A stand-in shared-region frame owned by "the registry", mapped
        // into the space but never owned by it.
        let region_frame = frames.alloc().expect("region frame");
        let after_region = before - 1;

        let fixed_base: u64 = 0x4000;
        let fixed_phys;
        {
            let mut live = LiveSpace::new(
                AddressSpace::new(HostPageTable::new()),
                SharedSim(simmap),
                frames,
                VirtAddr::new(MMIO_WINDOW_BASE),
                MMIO_WINDOW_PAGES,
                VirtAddr::new(ANON_WINDOW_BASE),
                ANON_WINDOW_PAGES,
                VirtAddr::new(DMA_WINDOW_BASE),
                DMA_WINDOW_PAGES,
                VirtAddr::new(SHARED_WINDOW_BASE),
                SHARED_WINDOW_PAGES,
                VirtAddr::new(FILE_WINDOW_BASE),
                FILE_WINDOW_PAGES,
            )
            .expect("windows are valid");

            live.map_anonymous(fixed_base, 2).expect("fixed anon");
            live.map_anonymous_placed(3).expect("placed anon");
            live.alloc_dma(2 * PAGE_SIZE, 0).expect("dma carve");
            live.map_device_window(0xFE98_0000, 0x2000)
                .expect("device window");
            live.map_shared(region_frame.start().as_u64(), PAGE_SIZE)
                .expect("shared region");

            // A recognisable secret in a fixed anonymous page, so the
            // zero-on-free scrub below is observable.
            copy_out(
                live.space(),
                simmap,
                VirtAddr::new(fixed_base),
                &[0xA5u8; 32],
            )
            .expect("writable user range");
            fixed_phys = live
                .space()
                .translate(crate::vmm::Page::from_addr(VirtAddr::new(fixed_base)).expect("aligned"))
                .expect("mapped")
                .0;

            assert!(
                frames.free_frames() < after_region,
                "the mappings consumed frames"
            );
        }

        // Every frame the task owned returned to the allocator; the
        // registry-owned region frame did not (its lifecycle belongs to
        // the shared-region registry).
        assert_eq!(
            frames.free_frames(),
            after_region,
            "exit reclaims the whole owned footprint and nothing else"
        );

        // The freed anonymous frame was scrubbed before it returned: the
        // dead task's bytes are unrecoverable through the direct map.
        let ptr = simmap
            .translate(fixed_phys.start(), PAGE_SIZE)
            .expect("frame in the sim window");
        // SAFETY: the sim map proved the pointer valid for a page; the
        // frame is free, so nothing else references it in this test.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.as_ptr(), 32) };
        assert!(
            bytes.iter().all(|&b| b == 0),
            "freed frames are zeroed on teardown"
        );

        // Hygiene: return the registry frame so the allocator is whole.
        let _ = frames.free(region_frame);
        assert_eq!(frames.free_frames(), before);
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
            VirtAddr::new(FILE_WINDOW_BASE),
            FILE_WINDOW_PAGES,
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

    #[test]
    fn map_shared_chunks_maps_a_multi_block_region_into_one_window() {
        let mut live = live();
        // Two physically-disjoint blocks in the sim window become one flat
        // three-page shared window — the display frame ring a single buddy
        // block could not hold.
        let chunk_a = SIM_BASE; // 2 pages
        let chunk_b = SIM_BASE + 4 * PAGE_SIZE as u64; // 1 page, past chunk_a
        let base = live
            .map_shared_chunks(&[(chunk_a, 2), (chunk_b, 1)])
            .expect("chunk list maps into one window");
        assert!(
            base > SHARED_WINDOW_BASE
                && base < SHARED_WINDOW_BASE + (SHARED_WINDOW_PAGES as u64) * PAGE_SIZE as u64,
            "shared VA lies inside the configured window, past the leading guard"
        );
        assert_eq!(live.space().mapped_pages(), 3);

        // The window is one flat buffer: a write into the page backed by the
        // *second* block round-trips, proving the blocks are contiguous in
        // virtual space and both reachable.
        let sim = sim();
        let payload = [0xA5u8; 16];
        let second_block_va = VirtAddr::new(base + 2 * PAGE_SIZE as u64);
        copy_out(live.space(), &sim, second_block_va, &payload).expect("writable user range");
        let mut back = [0u8; 16];
        copy_in(live.space(), &sim, second_block_va, &mut back).expect("readable user range");
        assert_eq!(
            back, payload,
            "second-block page round-trips through the window"
        );

        // Unmapping by base tears down all three pages; the frames belong to
        // the registry and are not freed here.
        live.unmap_shared(base, 3 * PAGE_SIZE)
            .expect("unmaps the whole window by base");
        assert_eq!(live.space().mapped_pages(), 0);
    }

    #[test]
    fn a_file_region_reserves_faults_reads_back_and_releases_sparsely() {
        let frames = leaked_frames();
        let before = frames.free_frames();
        let (mut live, simmap) = live_over(frames);
        // Reserving draws pure address space: no frame moves.
        let base = live.reserve_file_region(4).expect("window has room");
        assert!(base >= FILE_WINDOW_BASE);
        assert_eq!(frames.free_frames(), before, "reservation costs no RAM");

        // Fault two of the four pages resident and read the bytes back
        // through the uaccess boundary.
        live.map_file_page_at(base + PAGE_SIZE as u64, &[0x11; 8])
            .expect("fault page 1");
        live.map_file_page_at(base + 3 * PAGE_SIZE as u64, &[0x33; 8])
            .expect("fault page 3");
        let mut buf = [0u8; 8];
        copy_in(
            live.space(),
            simmap,
            VirtAddr::new(base + PAGE_SIZE as u64),
            &mut buf,
        )
        .expect("resident page reads");
        assert_eq!(buf, [0x11; 8]);

        // Releasing reclaims exactly the two resident pages and returns
        // the reservation for reuse.
        assert_eq!(live.release_file_region(base, 4), Ok(2));
        assert_eq!(frames.free_frames(), before, "all frames returned");
        assert_eq!(
            live.release_file_region(base, 4),
            Err(LiveSpaceError::Anon(AnonError::NotMapped)),
            "a released region is gone"
        );
        let again = live.reserve_file_region(4).expect("slots were returned");
        assert_eq!(again, base, "first-fit reuses the released range");
    }

    #[test]
    fn a_fault_outside_any_reserved_file_region_is_refused() {
        let mut live = live();
        // Nothing reserved: any window address is refused.
        assert_eq!(
            live.map_file_page_at(FILE_WINDOW_BASE, &[1]),
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        );
        // With a reservation, an address past its top is still refused.
        let base = live.reserve_file_region(2).expect("room");
        assert_eq!(
            live.map_file_page_at(base + 2 * PAGE_SIZE as u64, &[1]),
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        );
    }

    #[test]
    fn unmap_anonymous_refuses_a_file_region_base() {
        let mut live = live();
        let base = live.reserve_file_region(2).expect("room");
        live.map_file_page_at(base, &[7]).expect("fault");
        // The anonymous release path must not tear down (or even inspect)
        // a file region — wrong syscall, fail closed.
        assert_eq!(
            live.unmap_anonymous(base, 2),
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        );
        assert_eq!(live.release_file_region(base, 2), Ok(1));
    }

    #[test]
    fn dropping_the_live_space_reclaims_resident_file_pages() {
        let frames = leaked_frames();
        let before = frames.free_frames();
        {
            let (mut live, _sim) = live_over(frames);
            let base = live.reserve_file_region(8).expect("room");
            live.map_file_page_at(base, &[1]).expect("fault");
            live.map_file_page_at(base + 5 * PAGE_SIZE as u64, &[2])
                .expect("fault");
            assert!(frames.free_frames() < before);
        }
        // Teardown walked the live pages: both resident file pages (and the
        // page-table frames) came back.
        assert_eq!(frames.free_frames(), before);
    }

    /// A live space over the caller's own `'static` allocator handle and a
    /// shared simulated physical map (returned alongside), so a test can
    /// watch frame counts across the space's life and read the bytes the
    /// space wrote through the same storage.
    fn live_over(
        frames: &'static FrameAllocator,
    ) -> (LiveSpace<HostPageTable, SharedSim>, &'static SimPhysMap) {
        let simmap: &'static SimPhysMap = Box::leak(Box::new(sim()));
        let live = LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            SharedSim(simmap),
            frames,
            VirtAddr::new(MMIO_WINDOW_BASE),
            MMIO_WINDOW_PAGES,
            VirtAddr::new(ANON_WINDOW_BASE),
            ANON_WINDOW_PAGES,
            VirtAddr::new(DMA_WINDOW_BASE),
            DMA_WINDOW_PAGES,
            VirtAddr::new(SHARED_WINDOW_BASE),
            SHARED_WINDOW_PAGES,
            VirtAddr::new(FILE_WINDOW_BASE),
            FILE_WINDOW_PAGES,
        )
        .expect("windows are valid");
        (live, simmap)
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

    mod ramzip {
        use super::{SharedSim, ANON_WINDOW_BASE};
        use crate::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
        use crate::frame::{FrameAllocator, PhysAddr, PAGE_SIZE};
        use crate::live::{LiveSpace, LiveUserSpace};
        use crate::phys::SimPhysMap;
        use crate::ramzip::{PageCandidate, Ramzip, RamzipCaps, RamzipFaultOutcome};
        use crate::seal::{EntropySource, SealError};
        use crate::vmm::{AddressSpace, HostPageTable, VirtAddr};
        use alloc::boxed::Box;
        use tairix_log::{Event, Sink};
        use tairix_reclaim::{MemoryPressure, PressureBand};
        use tairix_sync::SpinLock;

        /// A larger machine than the shared 64-frame harness, so a test can
        /// press to a band *and* leave the tier room above the reserve.
        const FRAMES: usize = 512;

        struct NullSink;
        impl Sink for NullSink {
            fn write_event(&self, _event: &Event<'_>) {}
        }

        /// Deterministic counting entropy for the tier key/salt.
        struct CountingEntropy(u8);
        impl EntropySource for CountingEntropy {
            fn fill(&mut self, out: &mut [u8]) -> Result<(), SealError> {
                for byte in out.iter_mut() {
                    *byte = self.0;
                    self.0 = self.0.wrapping_add(1);
                }
                Ok(())
            }
        }

        /// A test tier keyed to a large machine (so caps never bind before
        /// the deliberately-exercised gate does).
        fn tier() -> SpinLock<Ramzip> {
            let mut entropy = CountingEntropy(1);
            SpinLock::new(
                Ramzip::new(RamzipCaps::from_physical(8 << 30), &mut entropy).expect("tier"),
            )
        }

        /// A live space over a fresh 512-frame allocator and a shared,
        /// leaked physical map (so `copy_in` observes the same storage).
        fn env() -> (
            LiveSpace<HostPageTable, SharedSim>,
            &'static FrameAllocator,
            MemoryPressure,
            &'static SimPhysMap,
        ) {
            let mut map = BootMemoryMap::new();
            map.push(MemoryRegion {
                kind: RegionKind::Usable,
                start: PhysAddr::new(0),
                length: (FRAMES * PAGE_SIZE) as u64,
            });
            let frames: &'static FrameAllocator =
                Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));
            let simmap: &'static SimPhysMap = Box::leak(Box::new(SimPhysMap::new(
                PhysAddr::new(0),
                FRAMES * PAGE_SIZE,
            )));
            let pressure = MemoryPressure::over(frames);
            let live = LiveSpace::new(
                AddressSpace::new(HostPageTable::new()),
                SharedSim(simmap),
                frames,
                VirtAddr::new(0x8000_0000),
                64,
                VirtAddr::new(ANON_WINDOW_BASE),
                256,
                VirtAddr::new(0x1_0000_0000),
                64,
                VirtAddr::new(0x2_0000_0000),
                64,
                VirtAddr::new(0x3_0000_0000),
                64,
            )
            .expect("windows valid");
            (live, frames, pressure, simmap)
        }

        /// Hold frames until the sampled band reaches `band`.
        fn press_to(frames: &FrameAllocator, pressure: &MemoryPressure, band: PressureBand) {
            let mut held = alloc::vec::Vec::new();
            let mut guard = 0;
            while pressure.sample() != band {
                held.push(frames.alloc().expect("pressure frame"));
                guard += 1;
                assert!(guard <= FRAMES, "band {band:?} never reached");
            }
            // Leak the held frames for the test's lifetime so the band stays.
            core::mem::forget(held);
        }

        /// The tier entry count, read through the shared stats projection.
        fn entries(tier: &SpinLock<Ramzip>) -> u64 {
            crate::ramzip::stats_of(&tier.lock()).entries
        }

        #[test]
        fn reclaim_compresses_cold_placed_anonymous_pages_and_faults_them_back() {
            let (mut live, frames, pressure, simmap) = env();
            let tier = tier();
            let sink = NullSink;

            // Six placed anonymous pages (zeroed, so highly compressible),
            // never accessed since mapping -> cold on the first scan.
            let base = live.map_anonymous_placed(6).expect("place anon region");
            assert_eq!(live.space().mapped_pages(), 6);

            press_to(frames, &pressure, PressureBand::Moderate);
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                6,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            assert_eq!(summary.scanned, 6);
            assert_eq!(summary.compressed, 6, "every cold page compressed");
            assert_eq!(summary.refused, 0);
            assert!(!summary.access_tracking_unsupported);
            assert_eq!(entries(&tier), 6, "six entries parked in the tier");
            assert_eq!(live.space().mapped_pages(), 0, "pages left resident RAM");

            // Fault every page back and confirm the restored bytes are the
            // original zeros (move-only: the entry is gone afterwards).
            for i in 0..6u64 {
                let va = base + i * PAGE_SIZE as u64;
                assert_eq!(
                    live.ramzip_fault_in(&tier, va, &sink),
                    RamzipFaultOutcome::Handled,
                    "page {i} restored"
                );
                let mut buf = [0xAAu8; PAGE_SIZE];
                crate::uaccess::copy_in(
                    live.space(),
                    &SharedSim(simmap),
                    VirtAddr::new(va),
                    &mut buf,
                )
                .expect("restored page is readable");
                assert!(buf.iter().all(|&b| b == 0), "page {i} restored to zeros");
            }
            assert_eq!(entries(&tier), 0, "move-only: tier emptied by fault-ins");
            assert_eq!(
                live.space().mapped_pages(),
                6,
                "all six pages resident again"
            );
        }

        #[test]
        fn reclaim_under_normal_pressure_compresses_nothing() {
            let (mut live, _frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            live.map_anonymous_placed(4).expect("place anon region");
            // No pressure: the handoff gate is closed, so every cold page is
            // refused by policy — nothing is compressed, but the sweep still
            // examined them.
            assert_eq!(pressure.sample(), PressureBand::Normal);
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                4,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            assert_eq!(summary.scanned, 4);
            assert_eq!(summary.compressed, 0);
            assert_eq!(summary.refused, 4);
            assert_eq!(entries(&tier), 0);
            assert_eq!(live.space().mapped_pages(), 4, "pages untouched");
        }

        #[test]
        fn only_placed_anonymous_pages_are_candidates() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            // A FIXED anonymous mapping outside every window is *not* a
            // reclaim candidate; only the placed-window region is.
            live.map_anonymous(0x4000, 2).expect("fixed map");
            live.map_anonymous_placed(3).expect("placed region");

            press_to(frames, &pressure, PressureBand::Moderate);
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                16,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            // Only the three placed pages were ever offered.
            assert_eq!(summary.scanned, 3);
            assert_eq!(summary.compressed, 3);
            // The two FIXED pages are still resident and untouched.
            assert!(live
                .space()
                .translate(crate::vmm::Page::from_addr(VirtAddr::new(0x4000)).unwrap())
                .is_some());
            assert!(live
                .space()
                .translate(crate::vmm::Page::from_addr(VirtAddr::new(0x5000)).unwrap())
                .is_some());
        }

        #[test]
        fn fault_in_with_no_entry_falls_through() {
            let (mut live, _frames, _pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            // An address the tier has no entry for: the resolver falls
            // through to the anonymous handler.
            assert_eq!(
                live.ramzip_fault_in(&tier, ANON_WINDOW_BASE, &sink),
                RamzipFaultOutcome::NoEntry
            );
        }

        #[test]
        fn reclaim_respects_the_want_budget() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            live.map_anonymous_placed(10).expect("place anon region");
            press_to(frames, &pressure, PressureBand::Moderate);
            // Ask for only 3 of the 10 cold pages.
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                3,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            assert_eq!(summary.compressed, 3, "budget honoured");
            assert_eq!(entries(&tier), 3);
            assert_eq!(live.space().mapped_pages(), 7, "seven pages stay resident");
        }

        #[test]
        fn a_reclaim_and_fault_cycle_leaks_no_frames() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            let base = live.map_anonymous_placed(5).expect("place anon region");

            press_to(frames, &pressure, PressureBand::Moderate);
            let held_free = frames.free_frames();
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                5,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            assert_eq!(summary.compressed, 5);
            // Compression returned five frames to the allocator.
            assert_eq!(frames.free_frames(), held_free + 5);

            for i in 0..5u64 {
                assert_eq!(
                    live.ramzip_fault_in(&tier, base + i * PAGE_SIZE as u64, &sink),
                    RamzipFaultOutcome::Handled
                );
            }
            // Faulting all five back consumed exactly the five frames again:
            // free returns to the held level, and re-mapping restored the
            // pre-pressure resident set (the compressed round trip is frame
            // neutral).
            assert_eq!(frames.free_frames(), held_free);
            assert_eq!(entries(&tier), 0);
        }

        /// Hold frames until the sampled band reaches `band`, returning
        /// them so a test can free them again to relax back to `Normal`.
        /// The shared `press_to` leaks its frames; the clustering and
        /// warm-up paths must compress under pressure and then observe
        /// *comfortable* memory, so their tests need releasable pressure.
        fn press_holding(
            frames: &FrameAllocator,
            pressure: &MemoryPressure,
            band: PressureBand,
        ) -> alloc::vec::Vec<crate::frame::Frame> {
            let mut held = alloc::vec::Vec::new();
            let mut guard = 0;
            while pressure.sample() != band {
                held.push(frames.alloc().expect("pressure frame"));
                guard += 1;
                assert!(guard <= FRAMES, "band {band:?} never reached");
            }
            held
        }

        /// Return every held pressure frame to the allocator, relaxing
        /// back toward `Normal`.
        fn release(frames: &FrameAllocator, held: alloc::vec::Vec<crate::frame::Frame>) {
            for frame in held {
                frames.free(frame).expect("return held frame");
            }
        }

        #[test]
        fn cluster_restores_contemporaneous_neighbours_after_a_comfortable_fault() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            // Six contiguous placed anonymous pages, all cold: compressed
            // together, so their seal times are contemporaneous.
            let base = live.map_anonymous_placed(6).expect("place anon region");
            let held = press_holding(frames, &pressure, PressureBand::Moderate);
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                6,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            assert_eq!(summary.compressed, 6);
            assert_eq!(entries(&tier), 6);

            // Relax so memory is comfortably free again — the warm gate.
            release(frames, held);
            assert_eq!(pressure.sample(), PressureBand::Normal);

            // Demand-fault the first page (records locality), then cluster
            // its neighbours: the exact production sequence the resolver
            // drives after a restore.
            assert_eq!(
                live.ramzip_fault_in(&tier, base, &sink),
                RamzipFaultOutcome::Handled
            );
            let restored = live.ramzip_cluster(&tier, &pressure, base, &sink);
            assert_eq!(restored, 5, "the five contemporaneous neighbours came back");
            assert_eq!(entries(&tier), 0, "fault + cluster emptied the tier");
            for i in 0..6u64 {
                let page = crate::vmm::Page::from_addr(VirtAddr::new(base + i * PAGE_SIZE as u64))
                    .unwrap();
                assert!(live.space().translate(page).is_some(), "page {i} resident");
            }
        }

        #[test]
        fn cluster_does_nothing_while_under_pressure() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            let base = live.map_anonymous_placed(4).expect("place anon region");
            // Stay under pressure: the held frames are never released.
            let _held = press_holding(frames, &pressure, PressureBand::Moderate);
            let summary = live.ramzip_reclaim(
                &tier,
                &pressure,
                0,
                4,
                PageCandidate::cold_anonymous(),
                &sink,
            );
            assert_eq!(summary.compressed, 4);
            assert_eq!(
                live.ramzip_fault_in(&tier, base, &sink),
                RamzipFaultOutcome::Handled
            );
            // The warm gate is closed under pressure: clustering restores
            // nothing, and the other three pages stay compressed.
            assert_eq!(live.ramzip_cluster(&tier, &pressure, base, &sink), 0);
            assert_eq!(entries(&tier), 3);
        }

        #[test]
        fn warm_restores_near_recent_faults_only_with_evidence_and_comfort() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            let base = live.map_anonymous_placed(6).expect("place anon region");
            let held = press_holding(frames, &pressure, PressureBand::Moderate);
            assert_eq!(
                live.ramzip_reclaim(
                    &tier,
                    &pressure,
                    0,
                    6,
                    PageCandidate::cold_anonymous(),
                    &sink
                )
                .compressed,
                6
            );
            release(frames, held);
            assert_eq!(pressure.sample(), PressureBand::Normal);

            // No demand fault yet: warm-up has no locality evidence, so it
            // keeps every cold page compressed by design.
            assert_eq!(live.ramzip_warm(&tier, &pressure, &sink), 0);
            assert_eq!(entries(&tier), 6, "nothing warmed without evidence");

            // A demand fault provides evidence; the warm step brings the
            // neighbours back (batch budget covers all five).
            assert_eq!(
                live.ramzip_fault_in(&tier, base, &sink),
                RamzipFaultOutcome::Handled
            );
            let restored = live.ramzip_warm(&tier, &pressure, &sink);
            assert_eq!(restored, 5, "the five neighbours warmed back");
            assert_eq!(entries(&tier), 0);
        }

        #[test]
        fn warm_stops_immediately_under_pressure() {
            let (mut live, frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            let base = live.map_anonymous_placed(4).expect("place anon region");
            let _held = press_holding(frames, &pressure, PressureBand::Moderate);
            assert_eq!(
                live.ramzip_reclaim(
                    &tier,
                    &pressure,
                    0,
                    4,
                    PageCandidate::cold_anonymous(),
                    &sink
                )
                .compressed,
                4
            );
            assert_eq!(
                live.ramzip_fault_in(&tier, base, &sink),
                RamzipFaultOutcome::Handled
            );
            // Still under pressure: the warm gate is closed, so the step
            // stops immediately and the three others stay compressed.
            assert_eq!(live.ramzip_warm(&tier, &pressure, &sink), 0);
            assert_eq!(entries(&tier), 3);
        }

        #[test]
        fn cluster_and_warm_are_no_ops_on_an_empty_tier() {
            let (mut live, _frames, pressure, _simmap) = env();
            let tier = tier();
            let sink = NullSink;
            // No entries and no faults: both restore nothing (fail closed)
            // and never panic — the resolver may call them after any fault.
            assert_eq!(
                live.ramzip_cluster(&tier, &pressure, ANON_WINDOW_BASE, &sink),
                0
            );
            assert_eq!(live.ramzip_warm(&tier, &pressure, &sink), 0);
            assert_eq!(entries(&tier), 0);
        }
    }
}
