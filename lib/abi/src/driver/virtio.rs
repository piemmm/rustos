//! Virtio host-↔-driver ABI seam (`abi-v1`).
//!
//! [`VirtioHost`] is the trait every virtio class driver consumes to
//! allocate DMA-able memory and to wait for queue notifications. It
//! lives in `lib/abi` rather than in `drivers/bus/virtio` so the
//! host trait surface ([`super::DriverHost::virtio_host`]) can name
//! it without inverting the dependency direction (`AGENTS.md` §3).
//!
//! The virtio bus crate provides:
//!
//! * `rustos_drv_bus_virtio::MockHost` — an always-available
//!   in-process implementation used by every virtio driver's unit
//!   tests.
//! * `rustos_drv_bus_virtio::KernelVirtioHost` (gated behind the
//!   `kernel-host` feature) — the real, capability-checked
//!   implementation backed by a per-process `DmaPool`. The userland
//!   driver host (`userland/system/drvhost`) mints one per loaded
//!   driver module and exposes it through [`super::DriverHost::virtio_host`].

use super::dma::DmaSlab;
use crate::DriverError;

/// Trait every virtio driver consumes to allocate DMA-able memory
/// and to wait for queue notifications.
///
/// The two-method shape — [`alloc_dma_zeroed`](Self::alloc_dma_zeroed)
/// and [`notify_wait`](Self::notify_wait) — is the minimum surface a
/// Stage-4 split-virtqueue driver needs. Anything larger would be a
/// Stage-5 deliverable per `AGENTS.md` §2.3.
pub trait VirtioHost {
    /// Allocate a contiguous, device-visible DMA region.
    ///
    /// The returned [`DmaSlab`] is owned by the caller; it carries
    /// the pool id and slot it was minted from so the host's drop
    /// path can reclaim the slot. The bytes are zero-initialised so
    /// that the driver can publish the slab into a queue without
    /// first clearing leftover bytes from another transaction
    /// (defence in depth — `AGENTS.md` §7).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `size == 0`.
    /// * [`DriverError::LengthOutOfRange`] if the host exhausts its
    ///   DMA pool.
    /// * [`DriverError::PermissionDenied`] if the calling task is
    ///   missing the capability the host enforces at allocation
    ///   time (the kernel host gates on `CAP_MEM_DMA`).
    ///
    /// # Capabilities
    ///
    /// None directly at the trait level; the host enforces its own
    /// DMA-pool quota and per-task capability check at allocation
    /// time (`AGENTS.md` §4 "per-process heaps" + §5.4 "fail
    /// closed").
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError>;

    /// Block (or busy-wait) until the device signals a completion
    /// on `queue_index`.
    ///
    /// The mock host returns immediately because completions are
    /// produced inline by the in-process software peer. The
    /// production kernel host parks the calling task on a wait
    /// queue and is resumed by the virtio MSI / MMIO-IRQ ISR (the
    /// IRQ plumbing lands in Stage 4.D Item 2).
    ///
    /// # Errors
    ///
    /// Never fails by design; the trait method returns `()` so a
    /// caller cannot accidentally treat "spurious wake-up" as a
    /// retriable failure (which would be the kind of hack
    /// `AGENTS.md` §2.1 forbids).
    fn notify_wait(&self, queue_index: u16);
}
