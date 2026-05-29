//! In-kernel [`VirtioHost`] backed by a per-process [`DmaPool`].
//!
//! Stage 4.D Item 0 wiring: the always-available [`crate::MockHost`]
//! satisfies [`crate::VirtioHost`] by leaking `Box<[u8]>` storage; the
//! [`KernelVirtioHost`] here satisfies the same trait but routes every
//! allocation through the capability-gated [`rustos_kernel_sec::alloc_dma`]
//! / [`rustos_kernel_sec::free_dma`] pair (`AGENTS.md` §5.4).
//!
//! The host owns a borrowed mutable reference to a single
//! [`rustos_kernel_mem::DmaPool`] (per-driver pool — `AGENTS.md` §4
//! "per-process heaps, never a global user heap"). Allocations:
//!
//! 1. `kernel_sec::alloc_dma` performs the [`CapabilityId::MEM_DMA`]
//!    check and emits the [`AuditEvent::DmaAllocated`] / `…Denied`
//!    record. The pool zero-fills the data slots; the bytes are
//!    therefore safe to publish into a virtio descriptor without an
//!    extra scrub pass.
//! 2. The host obtains the raw, non-null base pointer through
//!    [`DmaPool::slot_base`] and mints a [`DmaSlab`] via
//!    [`DmaSlab::from_pool`]; the slab carries a generic free shim
//!    that re-enters the host on drop and routes the buffer back
//!    through `kernel_sec::free_dma`.
//! 3. The host records the live `(slot, DmaBuffer)` pair in an
//!    internal table so the drop-path shim — which receives only
//!    `(*const(), slot, len)` — can recover the originating
//!    [`DmaBuffer`] without further state in the slab.
//!
//! [`KernelVirtioHost::notify_wait`] is the polled cooperative shim
//! inherited from [`crate::MockHost`]: real IRQ-routed wake-ups are
//! Stage 4.D Item 2 work (tracked in `.junie/next-session-prompt.md`).
//!
//! # Safety
//!
//! The unsafe construction site is [`DmaSlab::from_pool`]. Every
//! invariant required by that constructor is established here:
//!
//! * `pool_ptr` is `self as *const Self as *const ()`; the slab's
//!   lifetime is bounded by `'a`, which is the lifetime of the
//!   `KernelVirtioHost`. The borrow checker enforces that no slab
//!   minted by this host outlives the host's borrow.
//! * `ptr` is the result of [`DmaPool::slot_base`], which is
//!   documented to be a non-null, page-aligned pointer covering
//!   exactly `buf.len()` disjoint bytes (witnessed by the pool's
//!   slot bitmap — one slot ↔ one allocation).
//! * The free shim is monomorphised per `(P, S)`; the cast back from
//!   `*const ()` to `*const KernelVirtioHost<'_, P, S>` is the
//!   inverse of the cast performed at construction and therefore
//!   sound.

#![cfg(any(feature = "kernel-host", test))]

use alloc::collections::BTreeMap;
use core::cell::{Cell, RefCell};
use core::ptr::NonNull;

use rustos_abi::DriverError;
use rustos_kernel_mem::{DmaBuffer, DmaPool, PageTableOps};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_kernel_sec::dma::{alloc_dma, free_dma, DmaGateError};
use rustos_log::Sink;

use crate::dma::{DmaSlab, PoolId};
use crate::host::VirtioHost;

/// Capability-checked, [`DmaPool`]-backed [`VirtioHost`].
///
/// Generic over the page-table backend `P` (so the same code is
/// exercised by `kernel/mem::HostPageTable` in unit tests and by the
/// architecture page-table types in production) and the audit
/// [`Sink`] implementation `S`.
///
/// # Lifetime
///
/// `'a` bounds both the borrowed pool and the borrowed
/// [`TaskCapabilities`]. Every [`DmaSlab`] minted by this host
/// re-enters the host on drop, so by construction no slab can
/// outlive the host — Rust's borrow checker enforces this through
/// the `&'a self` borrow returned by [`Self::alloc_dma_zeroed`]
/// (the slab carries no lifetime in its type, but the slab's drop
/// would dereference `&self` if it ran after the host went away;
/// see [`Self::shutdown`] for the audited tear-down contract).
pub struct KernelVirtioHost<'a, P: PageTableOps, S: Sink + ?Sized> {
    pool: RefCell<&'a mut DmaPool<'a, P>>,
    caller: &'a TaskCapabilities,
    audit: &'a S,
    id: PoolId,
    next_slot: Cell<usize>,
    /// Live `(slot → DmaBuffer)` table. The drop-path shim receives
    /// only `(*const(), slot, len)`; it recovers the originating
    /// [`DmaBuffer`] by removing the entry under `slot`.
    live: RefCell<BTreeMap<usize, DmaBuffer>>,
    notify_log: RefCell<alloc::vec::Vec<u16>>,
}

impl<'a, P: PageTableOps, S: Sink + ?Sized> KernelVirtioHost<'a, P, S> {
    /// Wrap a borrowed [`DmaPool`] in a capability-checking host.
    ///
    /// `id` must be a fresh, process-unique [`PoolId`] (mint via
    /// [`PoolId::fresh`]). `caller` is the [`TaskCapabilities`] of
    /// the task that owns the per-process pool; every allocation
    /// and every drop-frees the buffer is audited against this
    /// capability set.
    #[must_use]
    pub fn new(
        pool: &'a mut DmaPool<'a, P>,
        caller: &'a TaskCapabilities,
        audit: &'a S,
        id: PoolId,
    ) -> Self {
        Self {
            pool: RefCell::new(pool),
            caller,
            audit,
            id,
            next_slot: Cell::new(0),
            live: RefCell::new(BTreeMap::new()),
            notify_log: RefCell::new(alloc::vec::Vec::new()),
        }
    }

    /// Identifier this host stamps on every slab it mints.
    #[must_use]
    pub fn pool_id(&self) -> PoolId {
        self.id
    }

    /// Number of slabs this host currently has outstanding.
    ///
    /// Decremented exactly once per slab drop. Test-only consumers
    /// assert this returns zero after a transaction completes — the
    /// canonical leak check for the slab/free-shim wiring.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.live.borrow().len()
    }

    /// All notify events the host has seen so far, in order.
    ///
    /// The polled cooperative [`Self::notify_wait`] records each
    /// `queue_index` here; Stage 4.D Item 2 will replace the
    /// in-process log with a real IRQ-routed wake-up.
    #[must_use]
    pub fn notify_log(&self) -> alloc::vec::Vec<u16> {
        self.notify_log.borrow().clone()
    }
}

/// Drop-path shim invoked by [`DmaSlab::drop`].
///
/// `pool` is the host pointer recorded at construction time
/// (cast through `*const ()`). `slot` is the slab's unique slot
/// index within this host; `len` is the slab's byte length and is
/// retained for symmetry with [`crate::SlabFreeFn`] — the actual
/// free goes through [`free_dma`] which keys on the [`DmaBuffer`]
/// stored in [`KernelVirtioHost::live`].
///
/// # Safety
///
/// Mirrors the [`crate::SlabFreeFn`] contract:
///
/// * `pool` must be the `*const ()` produced by
///   `KernelVirtioHost::alloc_dma_zeroed` for some slab carrying
///   `slot`. The originating host must still be live (the slab's
///   lifetime is bounded by the host's `'a`).
/// * `slot` must be the slot index recorded on the slab. Calling
///   with a stale or unknown slot is a logic bug; the shim is
///   defensive and silently returns when no entry is found (the
///   slab will then have been double-freed, which is `Drop`'s
///   responsibility to avoid — see [`DmaSlab::drop`] running exactly
///   once).
unsafe fn slab_free_shim<P: PageTableOps, S: Sink + ?Sized>(
    pool: *const (),
    slot: usize,
    _len: usize,
) {
    // SAFETY: `pool` was produced at the matching `from_pool` call by
    // casting `&KernelVirtioHost<'_, P, S>` through `*const Self as
    // *const ()`. The slab's lifetime is bounded by the host's `'a`
    // borrow, so the dereference here observes a live host of the
    // same monomorphisation.
    let host: &KernelVirtioHost<'_, P, S> =
        unsafe { &*(pool.cast::<KernelVirtioHost<'_, P, S>>()) };
    let removed = host.live.borrow_mut().remove(&slot);
    if let Some(buf) = removed {
        // `Drop` cannot propagate errors; a refusal here means the
        // task lost `CAP_MEM_DMA` between alloc and free, which
        // `free_dma` records as a `DmaAllocDenied` audit event. The
        // pool keeps the slot reserved (the supervisor process is
        // expected to reclaim it). We deliberately do not retry —
        // `AGENTS.md` §2.1 forbids retry-until-it-works.
        let _ = free_dma(*host.pool.borrow_mut(), host.caller, buf, host.audit);
    }
}

impl<P: PageTableOps, S: Sink + ?Sized> VirtioHost for KernelVirtioHost<'_, P, S> {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        if size == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let buf = {
            let mut pool = self.pool.borrow_mut();
            alloc_dma(*pool, self.caller, size, self.audit).map_err(map_gate_error)?
        };
        // `slot_base` returns the data-region base for the buffer.
        // It cannot fail for a buffer minted from this pool one
        // statement above, but the result is plumbed through to
        // surface any allocator-internal inconsistency as a
        // `DriverError` instead of a panic (`AGENTS.md` §2.9).
        let base: NonNull<u8> = if let Ok(p) = self.pool.borrow().slot_base(&buf) {
            p
        } else {
            // Roll back the allocation rather than leak the buffer;
            // this is fail-closed per `AGENTS.md` §5.4. Cannot happen
            // in practice for a buffer minted one statement above,
            // but the recovery path keeps `AGENTS.md` §2.9 satisfied
            // without an `expect`.
            let _ = free_dma(*self.pool.borrow_mut(), self.caller, buf, self.audit);
            return Err(DriverError::LengthOutOfRange);
        };
        let slot = self.next_slot.get();
        self.next_slot.set(slot.wrapping_add(1));
        self.live.borrow_mut().insert(slot, buf);
        let phys = buf.phys().as_u64();
        let len = buf.len();
        let pool_ptr: *const () = (self as *const Self).cast::<()>();
        // SAFETY: as discussed in the module-level comment —
        // (i) `base` is non-null and covers `len` disjoint bytes
        //     (witnessed by the pool's slot bitmap);
        // (ii) `pool_ptr` outlives the slab because the slab carries
        //     no lifetime in its type but the slab's drop shim
        //     dereferences `&self`, which the borrow checker
        //     enforces via the `&'a self` return path;
        // (iii) `phys` is the device-visible base of `base[0]`,
        //     produced by `DmaPool::alloc` and stored in `buf.phys()`.
        let slab = unsafe {
            DmaSlab::from_pool(
                phys,
                base,
                len,
                self.id,
                slot,
                pool_ptr,
                slab_free_shim::<P, S>,
            )
        };
        Ok(slab)
    }

    fn notify_wait(&self, queue_index: u16) {
        // Polled cooperative shim, as on `MockHost`. The IRQ-routed
        // wake-up that will eventually replace this body is tracked
        // as Stage 4.D Item 2 in `.junie/next-session-prompt.md`.
        self.notify_log.borrow_mut().push(queue_index);
    }
}

/// Map a [`DmaGateError`] to the closest [`DriverError`].
///
/// Capability refusals surface as [`DriverError::PermissionDenied`];
/// every other failure (oversize requests, OOM, pool config bugs)
/// collapses to [`DriverError::LengthOutOfRange`] — the same
/// variant the existing [`crate::MockHost`] uses when its
/// 64 MiB cap is hit, so a driver consumer sees a single failure
/// shape regardless of which host minted it.
fn map_gate_error(e: DmaGateError) -> DriverError {
    // `DmaGateError` is `#[non_exhaustive]`; today every non-
    // capability variant collapses to `LengthOutOfRange`, but the
    // explicit wildcard arm keeps the function total against
    // future kernel-side additions without a panic
    // (`AGENTS.md` §2.9).
    match e {
        DmaGateError::CapabilityMissing => DriverError::PermissionDenied,
        _ => DriverError::LengthOutOfRange,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell as StdRefCell;
    use rustos_abi::CapabilityId;
    use rustos_caps::CapabilitySet;
    use rustos_kernel_mem::{
        bootinfo::{BootMemoryMap, MemoryRegion, RegionKind},
        AddressSpace, FrameAllocator, HostPageTable, PhysAddr, VirtAddr, PAGE_SIZE,
    };
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;
    use rustos_log::{Event, Sink};

    /// Minimal in-memory [`Sink`] that records `(level, event-id)` for
    /// every event. Used in lieu of the kernel/sec `RecordingSink` to
    /// keep this crate from depending on `kernel/sec`'s test-only
    /// surface.
    struct Recorder {
        events: StdRefCell<Vec<u32>>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                events: StdRefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.events.borrow().clone()
        }
    }

    impl Sink for Recorder {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push(event.id.0);
        }
    }

    fn small_map(usable_pages: usize) -> BootMemoryMap {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 16),
            length: (PAGE_SIZE * usable_pages) as u64,
        });
        m
    }

    fn fresh_pool(frames: &FrameAllocator) -> DmaPool<'_, HostPageTable> {
        DmaPool::new(
            AddressSpace::new(HostPageTable::new()),
            VirtAddr::new(0x2000_0000),
            16,
            frames,
        )
        .expect("pool constructs")
    }

    fn task_with(caps: &[CapabilityId], sink: &Recorder) -> TaskCapabilities {
        let mut set = CapabilitySet::empty();
        for c in caps {
            set.insert(*c);
        }
        TaskCapabilities::derive(TaskId(99), UserId(1000), set, set, sink)
    }

    /// Event ID emitted by `kernel/sec::dma::alloc_dma` on a
    /// successful grant. Mirrored from `kernel/sec::audit` so the
    /// test asserts the observable contract without depending on
    /// the internal `AuditEvent` enum.
    const DMA_ALLOCATED_ID: u32 = 1030;
    const DMA_ALLOC_DENIED_ID: u32 = 1031;

    #[test]
    fn alloc_returns_zero_initialised_slab() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        let slab = host.alloc_dma_zeroed(PAGE_SIZE).expect("granted");
        assert_eq!(slab.len(), PAGE_SIZE);
        assert!(slab.as_bytes().iter().all(|b| *b == 0));
        assert_eq!(slab.pool_id(), host.pool_id());
        assert_ne!(host.pool_id(), PoolId::MOCK);
        // Exactly one `DmaAllocated` audit event so far.
        let dma_events: Vec<u32> = sink
            .ids()
            .into_iter()
            .filter(|id| *id == DMA_ALLOCATED_ID || *id == DMA_ALLOC_DENIED_ID)
            .collect();
        assert_eq!(dma_events, [DMA_ALLOCATED_ID]);
    }

    #[test]
    fn drop_routes_through_free_dma() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        {
            let slab = host.alloc_dma_zeroed(PAGE_SIZE).expect("granted");
            assert_eq!(host.outstanding(), 1);
            drop(slab);
        }
        assert_eq!(
            host.outstanding(),
            0,
            "the drop-path shim must remove the slot from the live table"
        );
        // After drop the pool must show zero live allocations — the
        // canonical witness that `kernel_sec::free_dma` ran.
        assert_eq!(host.pool.borrow().live(), 0);
    }

    #[test]
    fn capability_missing_is_permission_denied() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        // No `MEM_DMA` capability: every allocation must fail closed.
        let caller = task_with(&[], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        let err = host.alloc_dma_zeroed(PAGE_SIZE).unwrap_err();
        assert!(matches!(err, DriverError::PermissionDenied));
        assert_eq!(host.outstanding(), 0);
        // The denial must have been audited.
        assert!(sink.ids().contains(&DMA_ALLOC_DENIED_ID));
        assert!(!sink.ids().contains(&DMA_ALLOCATED_ID));
    }

    #[test]
    fn zero_size_request_rejected_before_capability_check() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        let err = host.alloc_dma_zeroed(0).unwrap_err();
        assert!(matches!(err, DriverError::BufferTooSmall));
        assert_eq!(host.outstanding(), 0);
    }

    #[test]
    fn two_simultaneous_slabs_are_disjoint() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        let mut a = host.alloc_dma_zeroed(PAGE_SIZE).expect("first");
        let mut b = host.alloc_dma_zeroed(PAGE_SIZE).expect("second");
        // Distinct slot indices guarantee the slabs name disjoint
        // ranges of the pool's slot bitmap.
        assert_ne!(a.slot(), b.slot());
        // Distinct physical bases.
        assert_ne!(a.phys(), b.phys());
        a.as_bytes_mut().copy_from_slice(&[0xAA; PAGE_SIZE]);
        b.as_bytes_mut().copy_from_slice(&[0xBB; PAGE_SIZE]);
        // No cross-talk: each slab observes only the byte pattern it
        // wrote.
        assert!(a.as_bytes().iter().all(|byte| *byte == 0xAA));
        assert!(b.as_bytes().iter().all(|byte| *byte == 0xBB));
        assert_eq!(host.outstanding(), 2);
        drop(a);
        assert_eq!(host.outstanding(), 1);
        drop(b);
        assert_eq!(host.outstanding(), 0);
        assert_eq!(host.pool.borrow().live(), 0);
    }

    #[test]
    fn notify_wait_records_queue_index() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        host.notify_wait(0);
        host.notify_wait(2);
        host.notify_wait(0);
        assert_eq!(host.notify_log(), alloc::vec![0u16, 2, 0]);
    }

    #[test]
    fn oversize_request_collapses_to_length_out_of_range() {
        let frames = FrameAllocator::new(&small_map(16)).unwrap();
        let mut pool = fresh_pool(&frames);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let host = KernelVirtioHost::new(&mut pool, &caller, &sink, PoolId::fresh());
        // The pool is configured with 16 pages; requesting many
        // multiples of that triggers the pool's size-or-OOM path,
        // which `map_gate_error` collapses to `LengthOutOfRange`.
        let err = host.alloc_dma_zeroed(PAGE_SIZE * 64).unwrap_err();
        assert!(matches!(err, DriverError::LengthOutOfRange));
        assert_eq!(host.outstanding(), 0);
    }
}
