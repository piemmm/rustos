//! Host seam for virtio drivers.
//!
//! The kernel side of RustOS will, at Stage 4.D wiring time, supply a
//! concrete [`VirtioHost`] backed by a per-process DMA pool
//! (`AGENTS.md` §4). This crate ships the *interface* and a
//! deterministic [`MockHost`] implementation used by the unit tests in
//! every virtio driver crate. Decoupling the driver code from the
//! kernel DMA allocator is what lets the same `virtio_blk` /
//! `virtio_net` source files target x86_64-PCI, aarch64-MMIO,
//! riscv64-MMIO, and the unit-test environment without duplication
//! (`AGENTS.md` §2.2 / §6).

use crate::dma::DmaRegion;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cell::RefCell;
use rustos_abi::DriverError;

/// Trait every virtio driver consumes to allocate DMA-able memory
/// and to wait for queue notifications.
///
/// The two-method shape — `alloc_dma_zeroed` and `notify_wait` —
/// is the minimum surface a Stage-4 split-virtqueue driver needs.
/// Anything larger would be a Stage-5 deliverable per
/// `AGENTS.md` §2.3.
pub trait VirtioHost {
    /// Allocate a contiguous, device-visible DMA region.
    ///
    /// The returned region is zero-initialised so that the driver can
    /// publish it into a queue without first clearing leftover bytes
    /// from another transaction (defence in depth — `AGENTS.md` §7).
    ///
    /// The region is owned by the host; the lifetime tracks the
    /// host's storage.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `size == 0`.
    /// * [`DriverError::LengthOutOfRange`] if the host exhausts its
    ///   DMA pool.
    ///
    /// # Capabilities
    ///
    /// None directly; the host enforces its own DMA-pool quota at
    /// allocation time (`AGENTS.md` §4, "Per-process heaps").
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaRegion<'_>, DriverError>;

    /// Block (or busy-wait) until the device signals a completion on
    /// `queue_index`.
    ///
    /// The mock host returns immediately because completions are
    /// produced inline by the in-process software peer. The
    /// production kernel host parks the calling task on a wait queue
    /// and is resumed by the virtio MSI / MMIO-IRQ ISR.
    ///
    /// # Errors
    ///
    /// Never fails by design; the trait method returns `()` so a
    /// caller cannot accidentally treat "spurious wake-up" as a
    /// retriable failure (which would be the kind of hack
    /// `AGENTS.md` §2.1 forbids).
    fn notify_wait(&self, queue_index: u16);
}

/// In-process [`VirtioHost`] implementation used by the unit tests
/// in this crate and in the consuming `virtio_blk` / `virtio_net`
/// crates.
///
/// Allocates from a `Vec<Box<[u8]>>` (the kernel will replace this
/// with the per-process DMA pool at Stage 4.D wiring time). The
/// `phys` address returned is the CPU-side pointer cast to `u64`,
/// which is the legitimate identity-mapped value for the unit-test
/// process.
#[derive(Default)]
pub struct MockHost {
    notify_log: RefCell<Vec<u16>>,
    bytes_allocated: Cell<usize>,
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

impl VirtioHost for MockHost {
    /// Hand out a zeroed DMA region backed by a `Box<[u8]>` that is
    /// **leaked for the lifetime of the unit-test process** so the
    /// returned slice can carry a `'static`-derived lifetime
    /// without `unsafe`.
    ///
    /// The leak is acceptable in this in-process mock because (a) it
    /// is only compiled into `cargo test`, (b) the pool cap below
    /// bounds total residency, and (c) the kernel host that replaces
    /// this implementation at Stage 4.D wiring time owns and re-uses
    /// pages out of the per-process DMA pool instead of leaking
    /// them.
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaRegion<'_>, DriverError> {
        if size == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        // 64 MiB pool cap is far above the Stage-4 unit-test budget;
        // exceeding it signals a runaway test rather than real
        // allocator pressure. Failing closed (`AGENTS.md` §5.4.5).
        let bytes_now = self.bytes_allocated.get();
        let Some(bytes_after) = bytes_now.checked_add(size) else {
            return Err(DriverError::LengthOutOfRange);
        };
        if bytes_after > 64 * 1024 * 1024 {
            return Err(DriverError::LengthOutOfRange);
        }
        let storage: Box<[u8]> = alloc::vec![0u8; size].into_boxed_slice();
        let phys = storage.as_ptr() as u64;
        // `Box::leak` yields `&'static mut [u8]`, which the
        // `DmaRegion<'_>` borrow-checks against any `'_` we like.
        let bytes: &'static mut [u8] = Box::leak(storage);
        self.bytes_allocated.set(bytes_after);
        Ok(DmaRegion::from_parts(phys, bytes))
    }

    fn notify_wait(&self, queue_index: u16) {
        self.notify_log.borrow_mut().push(queue_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::driver::BufferClass;

    #[test]
    fn mock_host_zero_initialises() {
        let host = MockHost::new();
        let region = host.alloc_dma_zeroed(64).expect("alloc");
        assert_eq!(region.len(), 64);
        assert!(region.as_bytes().iter().all(|b| *b == 0));
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
    fn host_dma_region_supports_bounce_buffer_round_trip() {
        let host = MockHost::new();
        let region = host.alloc_dma_zeroed(32).expect("alloc");
        let mut bb = crate::dma::BounceBuffer::new(region, BufferClass::NonSensitive);
        bb.stage(&[0x42; 16]).unwrap();
        assert_eq!(bb.staged(), &[0x42; 16]);
    }
}
