//! TAIRiX virtio bus driver-crate entry point.
//!
//! Neither concrete transport lives here: the MMIO transport
//! ([`MmioTransport`]) and the PCI transport ([`PciTransport`]) depend only
//! on the bounds-checked [`tairix_abi`] register window and the protocol
//! types, so they live in [`tairix_virtio`] with the queue management and the
//! DMA-slab abstraction, where an arch-neutral user-space driver can build
//! them without a `drivers/* → drivers/*` edge. The two transports and the PCI
//! common-configuration table ([`transport_pci`]) are re-exported for the
//! kernel-side consumers that bind a transport through this crate.
//!
//! # Public surface
//!
//! A driver crate's only public function is [`register`]; the re-exports
//! are types, not driver entry points.
//!
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use tairix_virtio::{transport_pci, MmioTransport, PciTransport};

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
/// returns the registration marker handle; it probes nothing, since a
/// transport is built by the class driver over its node's register window.
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

#[cfg(test)]
mod tests {
    use tairix_abi::{CapabilityId, DriverError, DriverHost, DriverKind};

    struct Host {
        granted: bool,
    }

    impl DriverHost for Host {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            self.granted && cap == CapabilityId::DRV_LOAD
        }
        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }
    }

    #[test]
    fn register_requires_drv_load() {
        assert_eq!(
            crate::register(&Host { granted: false }),
            Err(DriverError::PermissionDenied)
        );
        let handle = crate::register(&Host { granted: true }).expect("registers");
        assert_ne!(handle.as_u64(), 0);
    }
}
