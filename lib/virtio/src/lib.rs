//! RustOS bus-agnostic virtio split-virtqueue protocol.
//!
//! This crate implements the **virtio 1.x split-virtqueue** protocol
//! one level above any architecture-specific bus seam. It is the
//! shared dependency of `drivers/storage/virtio_blk`,
//! `drivers/network/virtio_net`, the concrete PCI / MMIO transports in
//! `drivers/bus/virtio`, and the kernel-side host in `kernel/virtio`.
//! Hosting the protocol here — in `lib/` rather than in one of the
//! driver crates — is what keeps every consumer on the layering of
//! `AGENTS.md` §17.4 (`drivers/* → lib/*` only, never another driver)
//! while satisfying the no-duplication rule in §2.2 / §6.
//!
//! # Surface
//!
//! * [`Transport`] — the bus seam every virtio device speaks through.
//!   The concrete MMIO implementation ([`MmioTransport`]) lives here so
//!   both kernel-side consumers and an arch-neutral user-space driver
//!   process can build it without a `drivers/*` dependency (§17.4); the
//!   concrete PCI implementation (which needs the bus driver's PCI
//!   capability-window wiring) lives in `drivers/bus/virtio`.
//!   [`MockTransport`] is the in-process peer the driver unit tests
//!   drive.
//! * [`SplitQueue`] — split-virtqueue descriptor/avail/used management
//!   (virtio 1.1 §2.6).
//! * [`PackedQueue`] — packed-virtqueue single-ring management
//!   (virtio 1.1 §2.7).
//! * [`VirtioHost`] — the DMA-allocation seam (re-exported from
//!   [`rustos_abi`]); [`MockHost`] is the in-process implementation, and
//!   `rustos_kernel_virtio::KernelVirtioHost` is the capability-checked
//!   production host.
//! * [`DmaSlab`] / [`BounceBuffer`] — owned device-visible memory and
//!   the zero-on-free staging wrapper (`AGENTS.md` §4).
//!
//! # Ring formats
//!
//! Both virtqueue wire formats are implemented as parallel siblings
//! (the `AGENTS.md` §2.2 carve-out): [`SplitQueue`] for the split ring
//! (virtio 1.1 §2.6) and [`PackedQueue`] for the packed ring
//! (virtio 1.1 §2.7). A device advertises the packed format through the
//! `VIRTIO_F_RING_PACKED` feature bit; the two queues share the
//! [`ChainSegment`] / [`UsedToken`] vocabulary and the [`Transport`]
//! seam (whose `queue_set` programs the descriptor / driver-area /
//! device-area addresses for either layout). See
//! `docs/src/drivers/virtio.md`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod dma;
pub mod host;
pub mod packed;
pub mod queue;
pub mod transport;
pub mod transport_mmio;

#[cfg(test)]
mod tests;

pub use dma::{BounceBuffer, DmaSlab, PoolId, SlabFreeFn};
pub use host::{DmaHost, MockHost, VirtioHost, VirtioHostFactory};
pub use packed::PackedQueue;
pub use queue::{ChainSegment, SplitQueue, UsedToken};
pub use transport::{
    ChainView, Direction, MockTransport, PciTransportWindows, Status, Transport, VirtioError,
};
pub use transport_mmio::MmioTransport;
