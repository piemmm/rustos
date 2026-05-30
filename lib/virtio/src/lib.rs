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
//!   The concrete PCI / MMIO implementations live in
//!   `drivers/bus/virtio`; [`MockTransport`] is the in-process peer the
//!   driver unit tests drive.
//! * [`SplitQueue`] — split-virtqueue descriptor/avail/used management.
//! * [`VirtioHost`] — the DMA-allocation seam (re-exported from
//!   [`rustos_abi`]); [`MockHost`] is the in-process implementation, and
//!   `rustos_kernel_virtio::KernelVirtioHost` is the capability-checked
//!   production host.
//! * [`DmaSlab`] / [`BounceBuffer`] — owned device-visible memory and
//!   the zero-on-free staging wrapper (`AGENTS.md` §4).
//!
//! # Split queues only
//!
//! Stage 4 deliberately implements only the **split** virtqueue variant
//! (virtio 1.1 §2.6). Packed queues (virtio 1.1 §2.7) are documented as
//! a Stage 5 follow-up in `docs/src/drivers/virtio.md`; they are
//! intentionally not started here (`AGENTS.md` §2.3 — no bloat).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod dma;
pub mod host;
pub mod queue;
pub mod transport;

#[cfg(test)]
mod tests;

pub use dma::{BounceBuffer, DmaSlab, PoolId, SlabFreeFn};
pub use host::{MockHost, VirtioHost};
pub use queue::{ChainSegment, SplitQueue, UsedToken};
pub use transport::{ChainView, Direction, MockTransport, Status, Transport, VirtioError};
