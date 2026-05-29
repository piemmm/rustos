//! RustOS cross-arch virtio transport.
//!
//! This crate implements the **virtio 1.x split-virtqueue** protocol
//! one level above an architecture-specific bus seam (`PCI` on
//! `x86_64`; `MMIO` on `AArch64` / RISC-V `virt` machines). It is the
//! shared dependency of `drivers/storage/virtio_blk` and
//! `drivers/network/virtio_net`, satisfying the no-duplication rule
//! in `AGENTS.md` §2.2 ("If two crates need it, it goes there"; the
//! "there" for queue management is this crate).
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 a driver crate's only public function is
//! [`register`]. The other public items in this crate are *types*
//! re-exported through the [`Transport`] / [`VirtioHost`] /
//! [`SplitQueue`] surface so that the two consuming driver crates
//! can use them without duplicating queue code; they are not driver
//! entry points and are not loaded by the userland driver host as
//! independent modules.
//!
//! # Split queues only
//!
//! Stage 4 deliberately implements only the **split** virtqueue
//! variant (virtio 1.1 §2.6). Packed queues (virtio 1.1 §2.7) are
//! documented as a Stage 5 follow-up in `docs/src/drivers/virtio.md`;
//! they are intentionally not started here so that this crate
//! remains within Stage 4 scope (`AGENTS.md` §2.3 — no bloat).
//!
//! # Safety
//!
//! The MMIO and PCI backends each perform `unsafe` volatile loads /
//! stores against device registers; every such block carries a
//! `// SAFETY:` justification, is encapsulated behind a safe
//! trait (`MmioOps` / `PortIo`), and is exercised by a unit test
//! against a deterministic in-memory fake.  No `unsafe` is exported
//! across this crate's public boundary.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod backend;
pub mod dma;
pub mod host;
#[cfg(any(feature = "kernel-host", test))]
pub mod kernel_host;
pub mod queue;
pub mod transport;

#[cfg(test)]
mod tests;

pub use backend::{MmioBackend, MmioOps, PciBackend, PortIo};
pub use dma::{BounceBuffer, DmaSlab, PoolId, SlabFreeFn};
pub use host::{MockHost, VirtioHost};
#[cfg(any(feature = "kernel-host", test))]
pub use kernel_host::KernelVirtioHost;
pub use queue::{ChainSegment, SplitQueue, UsedToken};
pub use transport::{ChainView, Direction, MockTransport, Status, Transport, VirtioError};

use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention of `drivers/bus/pci` and
/// `drivers/bus/mmio`: the driver host re-issues a host-local handle
/// when binding this driver into its load table; this constant is
/// the on-the-wire signal that the load-time gates cleared
/// (`AGENTS.md` §8).
const REGISTER_HANDLE_MARKER: u64 = 0x5654_4E54_0000_0001; // "VTNT" + tag.

/// Driver entry point (`AGENTS.md` §8).
///
/// Verifies the host already granted [`CapabilityId::DRV_LOAD`] and
/// returns the registration marker handle. The cross-arch transport
/// performs no probe work in `register`; concrete instantiation
/// happens when [`Transport`] objects are constructed by the
/// virtio-blk / virtio-net driver crates against an already-bound
/// PCI or MMIO bus handle.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}
