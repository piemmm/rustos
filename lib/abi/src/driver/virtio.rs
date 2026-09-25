//! Virtio host-↔-driver ABI seam (`abi-v1`).
//!
//! [`VirtioHost`] is the trait every virtio class driver consumes to
//! allocate DMA-able memory and to wait for queue notifications. It
//! lives in `lib/abi` so the host trait surface
//! ([`super::DriverHost::virtio_host`]) can name it without inverting the
//! dependency direction.
//!
//! Its implementations are `tairix_kernel_virtio::KernelVirtioHost`, the
//! capability-checked in-kernel host backed by a per-driver `DmaPool`; the
//! user-space driver runtime's `RtDriverHost`, over the process's DMA and
//! interrupt grants; and `tairix_virtio::MockHost`, the in-process host every
//! virtio driver's unit tests run on.

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
/// once. The virtio-specific surface is the wait,
/// [`notify_wait`](Self::notify_wait), and the clock its budgets run on,
/// [`now_ns`](Self::now_ns).
pub trait VirtioHost: DmaHost {
    /// Wait until the device signals on `queue_index`, or until
    /// `timeout_ns` of silence has passed, whichever comes first.
    ///
    /// A production host parks the calling task off the run queue until the
    /// device's interrupt or the deadline; the in-process mock plays a
    /// scripted outcome and advances its own clock by what the wait would
    /// have taken.
    ///
    /// A wake is only *advisory*: the device's used-ring write and its
    /// interrupt can be observed in either order and the one shared line
    /// serves every queue, so the caller re-scans its rings on
    /// [`CompletionSignal::Fired`] rather than treating it as proof.
    /// [`CompletionSignal::TimedOut`] is the honest opposite — the device
    /// said nothing within the budget, or the wait could not be made at all
    /// (a revoked or refused interrupt binding, a task being torn down), in
    /// which case it returns at once — and the caller, after one last scan,
    /// fails the affected transfer closed rather than waiting again.
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
    ///   something to say. A [`CompletionSignal::TimedOut`] then can only
    ///   mean the wait could not be made, and waiting again would spin.
    ///
    /// # Errors
    ///
    /// Cannot fail: every outcome is one of the two [`CompletionSignal`]
    /// answers. A signalled wait is still only advisory (above), so a
    /// caller can never mistake a spurious wake for a completion.
    fn notify_wait(&self, queue_index: u16, timeout_ns: u64) -> CompletionSignal;

    /// The monotonic clock [`Self::notify_wait`]'s budgets are measured
    /// against, in nanoseconds from an unspecified epoch.
    ///
    /// A wake restarts no deadline, so a caller holding a request to one
    /// deadline across several wakes passes each wait only what is left of
    /// it.
    fn now_ns(&self) -> u64;
}
