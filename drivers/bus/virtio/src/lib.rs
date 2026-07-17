//! TAIRiX concrete virtio PCI transport + register-window backends.
//!
//! This crate provides the **architecture-specific bus bindings** for
//! the bus-agnostic virtio split-virtqueue protocol that lives in
//! [`tairix_virtio`] (`lib/virtio`): the PCI ([`PciTransport`])
//! implementation of the [`tairix_virtio::Transport`] trait, plus the
//! [`PciBackend`] / [`MmioBackend`] register-window adapters.
//!
//! The concrete **MMIO** transport ([`MmioTransport`]) does *not* live
//! here: it depends only on the bounds-checked [`tairix_abi`] register
//! window and the protocol types, so it lives in [`tairix_virtio`] where
//! both this crate's kernel-side consumers and an arch-neutral
//! user-space virtio driver process can construct it without a
//! `drivers/* → drivers/*` edge (the
//! `lib/usb` ↔ `drivers/bus/usb` precedent). It is re-exported below so
//! existing `tairix_drv_bus_virtio::MmioTransport` import sites keep
//! resolving.
//!
//! The queue management, owned DMA-slab abstraction, and the
//! in-process [`MockHost`] / [`MockTransport`] doubles do **not** live
//! here — they live in [`tairix_virtio`] so that the device-class
//! drivers (`drivers/storage/virtio_blk`, `drivers/network/virtio_net`)
//! consume them through `lib/*` rather than depending on this bus
//! driver crate, which the charter forbids (`drivers/* → lib/*`
//! only). The protocol surface is re-exported below so the kernel-side
//! consumers that legitimately bind both a concrete transport and the
//! protocol types (`kernel/virtio`, the production binary, the QEMU
//! integration tests) can name it through this crate.
//!
//! # Public surface
//!
//! Per a driver crate's only public function is
//! [`register`]. The other public items are *types* re-exported through
//! the [`Transport`] surface so consumers can construct a concrete
//! transport; they are not driver entry points.
//!
//! # Safety
//!
//! The MMIO and PCI backends reach device registers exclusively through
//! the bounds-checked accessors on [`tairix_abi::RegisterWindow`], the
//! capability-checked window the kernel's MMIO-map facility mints for
//! the bus driver. The backends themselves perform no raw pointer
//! arithmetic and export no `unsafe` across this crate's public
//! boundary; the single `unsafe` construction site for a window lives
//! in `lib/abi` and is reached only by the kernel mapper.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod backend;
pub mod transport_pci;

pub use backend::{MmioBackend, PciBackend};
pub use transport_pci::PciTransport;

// The bus-agnostic protocol now lives in `lib/virtio`. Re-export it so
// existing `tairix_drv_bus_virtio::{...}` import sites in the kernel-side
// consumers keep resolving without each one having to also name the
// `tairix-virtio` crate directly (one canonical
// definition, re-exported, never duplicated).
pub use tairix_virtio::{
    BounceBuffer, ChainSegment, ChainView, Direction, DmaSlab, MmioTransport, MockHost,
    MockTransport, PciTransportWindows, PoolId, SlabFreeFn, SplitQueue, Status, Transport,
    UsedToken, VirtioError, VirtioHost,
};

use tairix_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// Mirrors the convention of `lib/pci` and
/// `drivers/bus/mmio`: the driver host re-issues a host-local handle
/// when binding this driver into its load table; this constant is
/// the on-the-wire signal that the load-time gates cleared.
const REGISTER_HANDLE_MARKER: u64 = 0x5654_4E54_0000_0001; // "VTNT" + tag.

/// Driver entry point.
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
