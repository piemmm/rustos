//! Host seam for virtio drivers.
//!
//! The kernel side of TAIRiX will, at Stage 4.D Item 0 wiring time,
//! supply a concrete [`VirtioHost`] backed by a per-process DMA pool. This crate ships the *interface* and a
//! deterministic [`MockHost`] implementation used by the unit tests
//! in every virtio driver crate. Decoupling the driver code from the
//! kernel DMA allocator is what lets the same `virtio_blk` /
//! `virtio_net` source files target x86_64-PCI, aarch64-MMIO,
//! riscv64-MMIO, and the unit-test environment without duplication.

use crate::dma::{DmaSlab, PoolId};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cell::RefCell;
use core::ptr::NonNull;
use tairix_abi::{CapabilityQuery, DriverError};

// `VirtioHost` moved into `lib/abi` at Stage 4.D Item 0-tail; the
// trait is re-exported here so existing `use crate::VirtioHost`
// import sites (in this crate and in every consuming virtio driver
// crate) keep working unchanged.
pub use tairix_abi::driver::{DmaHost, VirtioHost};

/// Factory that mints a per-driver [`VirtioHost`] for the duration of a
/// single driver `register()` call.
///
/// The driver host (`userland/system/drvhost`) calls [`Self::mint`]
/// just before invoking a driver's `register` entry point; the returned
/// host lives only for that call and is dropped immediately afterwards,
/// reclaiming any per-driver DMA bookkeeping.
///
/// # Why this lives in `lib/virtio`
///
/// The factory is the seam between the userland driver host
/// (`userland/system/drvhost`) and the concrete, kernel-linking
/// implementation (`kernel/virtio`'s `KernelVirtioFactory`). Neither may
/// depend on the other (: a userland service and a
/// kernel subsystem are sibling strata), so the shared contract lives
/// here in the bus-agnostic virtio host seam, alongside [`VirtioHost`]
/// and [`MockHost`]. Both sides depend only on `lib/*`, so the edge that
/// used to run `kernel/virtio -> drvhost` disappears.
///
/// # Capabilities
///
/// `mint` receives the already-intersected set granted to the driver as
/// a [`CapabilityQuery`] (so this crate need not depend on `lib/caps`;
/// see that trait's documentation). A capability-aware factory uses it
/// to short-circuit the allocation path when the driver was not granted
/// `CAP_MEM_DMA`. The host's own per-task DMA gate remains authoritative
/// (fail closed).
pub trait VirtioHostFactory {
    /// Construct a fresh virtio host for the upcoming `register()` call.
    ///
    /// Returns `None` if the factory chooses not to expose a virtio host
    /// to this driver — for example because `granted` does not include
    /// `CAP_MEM_DMA`, or because the platform has no virtio transport at
    /// all.
    ///
    /// The returned box borrows from the factory's lifetime; the factory
    /// must outlive the host, which is at most the duration of
    /// `register()`.
    fn mint<'r>(&'r self, granted: &dyn CapabilityQuery) -> Option<Box<dyn VirtioHost + 'r>>;
}

/// In-process [`VirtioHost`] implementation used by the unit tests
/// in this crate and in the consuming `virtio_blk` / `virtio_net`
/// crates.
///
/// Allocates from a `Vec<Box<[u8]>>` (the kernel will replace this
/// with the per-process DMA pool at Stage 4.D Item 0 wiring time).
/// The `phys` address returned is the CPU-side pointer cast to
/// `u64`, which is the legitimate identity-mapped value for the
/// unit-test process. The leaked `Box::leak` storage strategy is
/// retained: slabs are minted with [`PoolId::MOCK`] and a
/// monotonically increasing `slot`, and freeing through the slab's
/// drop is a no-op (the slab carries no `free_fn`).
#[derive(Default)]
pub struct MockHost {
    notify_log: RefCell<Vec<u16>>,
    bytes_allocated: Cell<usize>,
    next_slot: Cell<usize>,
}

impl MockHost {
    /// Construct an empty mock host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All notify events the host has seen so far, in order.
    #[must_use]
    pub fn notify_log(&self) -> Vec<u16> {
        self.notify_log.borrow().clone()
    }

    /// Total number of bytes ever handed out by this host.
    ///
    /// Because the test mock leaks its backing allocations for the
    /// lifetime of the unit-test process (see
    /// [`Self::alloc_dma_zeroed`]) this counter is monotonic.
    #[must_use]
    pub fn bytes_allocated(&self) -> usize {
        self.bytes_allocated.get()
    }
}

impl DmaHost for MockHost {
    /// Hand out a zeroed [`DmaSlab`] backed by a `Box<[u8]>` that
    /// is **leaked for the lifetime of the unit-test process** so
    /// the returned slab can carry its raw pointer without
    /// `unsafe` aliasing concerns.
    ///
    /// The leak is acceptable in this in-process mock because (a)
    /// it is only compiled into `cargo test`, (b) the pool cap
    /// below bounds total residency, and (c) the kernel host that
    /// replaces this implementation at Stage 4.D Item 0 wiring
    /// time owns and re-uses pages out of the per-process DMA
    /// pool instead of leaking them.
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        if size == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        // 64 MiB pool cap is far above the Stage-4 unit-test budget;
        // exceeding it signals a runaway test rather than real
        // allocator pressure. Failing closed.
        let bytes_now = self.bytes_allocated.get();
        let Some(bytes_after) = bytes_now.checked_add(size) else {
            return Err(DriverError::LengthOutOfRange);
        };
        if bytes_after > 64 * 1024 * 1024 {
            return Err(DriverError::LengthOutOfRange);
        }
        let storage: Box<[u8]> = alloc::vec![0u8; size].into_boxed_slice();
        let phys = storage.as_ptr() as u64;
        // `Box::leak` yields `&'static mut [u8]`; we record only its
        // raw pointer into the slab so the slab carries no borrow
        // and can be stored in `SplitQueue` without a lifetime.
        let bytes: &'static mut [u8] = Box::leak(storage);
        let ptr = NonNull::new(bytes.as_mut_ptr()).expect("box leak is non-null");
        let slot = self.next_slot.get();
        self.next_slot.set(slot.wrapping_add(1));
        self.bytes_allocated.set(bytes_after);
        // SAFETY: `bytes` is a `'static`-lifetime exclusive slice
        // of exactly `size` bytes; nothing else holds a reference
        // to it. We discard the `&'static mut [u8]` value above
        // and treat the slab as the sole owner via `ptr`.
        Ok(unsafe { DmaSlab::from_leaked(phys, ptr, size, PoolId::MOCK, slot) })
    }
}

impl VirtioHost for MockHost {
    fn notify_wait(&self, queue_index: u16) {
        self.notify_log.borrow_mut().push(queue_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::driver::BufferClass;

    #[test]
    fn mock_host_zero_initialises() {
        let host = MockHost::new();
        let slab = host.alloc_dma_zeroed(64).expect("alloc");
        assert_eq!(slab.len(), 64);
        assert!(slab.as_bytes().iter().all(|b| *b == 0));
        assert_eq!(slab.pool_id(), PoolId::MOCK);
    }

    #[test]
    fn mock_host_rejects_zero_size() {
        let host = MockHost::new();
        assert!(matches!(
            host.alloc_dma_zeroed(0),
            Err(DriverError::BufferTooSmall)
        ));
    }

    #[test]
    fn mock_host_records_notifies() {
        let host = MockHost::new();
        host.notify_wait(0);
        host.notify_wait(1);
        host.notify_wait(0);
        assert_eq!(host.notify_log(), alloc::vec![0u16, 1, 0]);
    }

    #[test]
    fn mock_host_assigns_distinct_slots() {
        let host = MockHost::new();
        let a = host.alloc_dma_zeroed(4).unwrap();
        let b = host.alloc_dma_zeroed(4).unwrap();
        let c = host.alloc_dma_zeroed(4).unwrap();
        assert_ne!(a.slot(), b.slot());
        assert_ne!(b.slot(), c.slot());
        assert_ne!(a.slot(), c.slot());
    }

    #[test]
    fn host_dma_slab_supports_bounce_buffer_round_trip() {
        let host = MockHost::new();
        let slab = host.alloc_dma_zeroed(32).expect("alloc");
        let mut bb = crate::dma::BounceBuffer::new(slab, BufferClass::NonSensitive);
        bb.stage(&[0x42; 16]).unwrap();
        assert_eq!(bb.staged(), &[0x42; 16]);
    }
}
