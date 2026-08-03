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
use super::CompletionSignal;

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
    /// Wait until the device signals on `queue_index`, or until
    /// `timeout_ns` of silence has passed, whichever comes first.
    ///
    /// The mock host returns immediately because completions are produced
    /// inline by the in-process software peer. The production kernel host
    /// parks the calling task off the run queue and is resumed by the virtio
    /// MSI / MMIO-IRQ ISR, or by the timed sweep at the deadline.
    ///
    /// A wake is only *advisory*: the device's used-ring write and its
    /// interrupt can be observed in either order and the one shared line
    /// serves every queue, so the caller re-scans its rings on
    /// [`CompletionSignal::Fired`] rather than treating it as proof.
    /// [`CompletionSignal::TimedOut`] is the honest opposite — the device
    /// said nothing at all within the budget — and the caller fails the
    /// affected transfer closed rather than waiting again indefinitely.
    ///
    /// `timeout_ns` is the caller's budget, and choosing it is the caller's
    /// responsibility because only the caller knows what it is waiting for:
    ///
    /// * A driver with a **request outstanding** passes that device's
    ///   per-request deadline ([`crate::blkio::IoBudget::deadline_ns`] for a
    ///   block device). The request's own completion is the only other event
    ///   that can end the wait, so an unbounded wait here turns a single
    ///   lost or coalesced interrupt into a task parked forever — and a task
    ///   parked forever inside a device operation holds that device's lock
    ///   forever, wedging every other user of the same hardware.
    /// * A driver waiting for an **unsolicited event** (an idle keyboard's
    ///   next keystroke, a NIC's next inbound frame) legitimately has
    ///   nothing outstanding and no deadline to apply: it passes
    ///   [`u64::MAX`], the "no timeout" spelling the `irq_wait` and
    ///   `waitset_wait` seams already use, and parks until the device has
    ///   something to say.
    ///
    /// # Errors
    ///
    /// Cannot fail: every outcome is one of the two [`CompletionSignal`]
    /// answers. A signalled wait is still only advisory (above), so a
    /// caller can never mistake a spurious wake for a completion.
    fn notify_wait(&self, queue_index: u16, timeout_ns: u64) -> CompletionSignal;
}
