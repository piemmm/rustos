//! Virtio host-↔-driver ABI seam (`abi-v1`).
//!
//! [`VirtioHost`] is the trait every virtio class driver consumes to
//! allocate DMA-able memory and to wait for queue notifications. It
//! lives in `lib/abi` rather than in `drivers/bus/virtio` so the
//! host trait surface ([`super::DriverHost::virtio_host`]) can name
//! it without inverting the dependency direction.
//!
//! The virtio bus crate provides:
//!
//! * `tairix_drv_bus_virtio::MockHost` — an always-available
//!   in-process implementation used by every virtio driver's unit
//!   tests.
//! * `tairix_drv_bus_virtio::KernelVirtioHost` (gated behind the
//!   `kernel-host` feature) — the real, capability-checked
//!   implementation backed by a per-process `DmaPool`. The userland
//!   driver host (`userland/system/drvhost`) mints one per loaded
//!   driver module and exposes it through [`super::DriverHost::virtio_host`].

use super::dma::DmaHost;

/// Trait every virtio driver consumes to wait for queue
/// notifications, on top of the bus-neutral DMA-allocation surface it
/// inherits from [`DmaHost`].
///
/// DMA allocation lives in the [`DmaHost`] supertrait
/// ([`alloc_dma_zeroed`](DmaHost::alloc_dma_zeroed)) rather than here so a
/// non-virtio bus driver can allocate DMA without depending on a
/// virtio-shaped trait, and so the allocation contract is defined exactly
/// once. The virtio-specific surface is therefore the
/// single [`notify_wait`](Self::notify_wait) method; anything larger would
/// be a Stage-5 deliverable.
pub trait VirtioHost: DmaHost {
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
    /// the charter forbids).
    fn notify_wait(&self, queue_index: u16);
}
