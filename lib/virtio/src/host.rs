//! Host seam for virtio drivers.
//!
//! The production hosts are `tairix_kernel_virtio::KernelVirtioHost` and the
//! user-space driver runtime's `RtDriverHost`; this crate ships the seam and,
//! for tests only, `MockHost`, the deterministic host every virtio driver's
//! unit tests run on, so one driver source serves every bus and the tests
//! alike.

use alloc::boxed::Box;
use tairix_abi::CapabilityQuery;

// `VirtioHost` moved into `lib/abi` at Stage 4.D Item 0-tail; the
// trait is re-exported here so existing `use crate::VirtioHost`
// import sites (in this crate and in every consuming virtio driver
// crate) keep working unchanged.
pub use tairix_abi::driver::{CompletionSignal, DmaHost, VirtioHost};

#[cfg(any(test, feature = "mock"))]
mod mock;
#[cfg(any(test, feature = "mock"))]
pub use mock::{MockHost, MockWait};

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
/// depend on the other — a userland service and a kernel subsystem are
/// sibling strata — so the shared contract lives
/// here in the bus-agnostic virtio host seam, alongside [`VirtioHost`].
/// Both sides depend only on `lib/*`, so the edge that
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
