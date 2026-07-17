//! TAIRiX virtio-net link-layer driver.
//!
//! The device engine itself — the bus-agnostic virtio-net bring-up and
//! frame-ring [`Net`](tairix_abi::driver::net::Net) service — lives in
//! the `lib/virtio_net` crate and is re-exported here as [`VirtioNet`].
//! It is hoisted into `lib/*` (rather than kept in this `drivers/*`
//! crate) so a user-space driver *process* can link the engine directly:
//! a process crate may depend on `lib/*` but never on another `drivers/*`
//! crate (`AGENTS.md` §17.4). This crate is the driver-host registration
//! shell over that shared engine.
//!
//! # Public surface
//!
//! [`register`] is the only public *function* — the driver-host entry
//! point. [`VirtioNet`] is re-exported so a host can instantiate the
//! engine; the host never reaches the type beyond the
//! [`Net`](tairix_abi::driver::net::Net) trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; `service` additionally
//! requires the dispatcher to have verified [`CapabilityId::NET_RAW`]
//! (see `lib/abi/src/driver/net.rs`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};

pub use tairix_virtio_net::VirtioNet;

/// The virtio device id of a network device (virtio 1.1 §5.1 — `virtio-net`
/// is device type 1), re-exported from `lib/virtio_net` (its single
/// definition, shared with the kernel's bootstrap-floor discovery, §2.2).
/// This driver's [`BIND_KEYS`] match key is built from it, so a discovered
/// virtio node whose probed device id is 1 binds this driver and nothing
/// else.
pub use tairix_virtio_net::VIRTIO_NET_DEVICE_ID;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_4554_0000_0001; // "VNET"

/// The bind priority [`BIND_KEYS`] carries.
///
/// A virtio device-id match is *exact* (the discovered node's probed device
/// id either is `virtio-net` or it is not — there is no wildcard, see
/// [`HwMatchKey::matches`]), so it ranks at the exact-match tier alongside
/// the other concrete-identity drivers (higher matched priority binds; an
/// unbroken tie is a packaging defect).
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: a virtio network device, matched by
/// its virtio device id ([`VIRTIO_NET_DEVICE_ID`]).
///
/// The single source of truth the signed-manifest bind table is authored
/// from and the device manager (or the in-kernel bootstrap-floor catalogue)
/// resolves a discovered node against. The match key carries no transport
/// (PCI vs MMIO) detail: the same driver binds a virtio-net device however
/// it is attached, because the bus-agnostic `tairix_virtio::Transport`
/// abstracts the transport.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::virtio(VIRTIO_NET_DEVICE_ID),
)];

/// Driver entry point.
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
    use super::*;

    #[test]
    fn register_requires_drv_load() {
        struct H {
            grant: bool,
        }
        impl DriverHost for H {
            fn has_capability(&self, cap: CapabilityId) -> bool {
                cap == CapabilityId::DRV_LOAD && self.grant
            }
            fn kind(&self) -> tairix_abi::driver::DriverKind {
                tairix_abi::driver::DriverKind::UserSpace
            }
        }
        assert_eq!(
            register(&H { grant: false }),
            Err(DriverError::PermissionDenied)
        );
        assert!(register(&H { grant: true }).is_ok());
    }

    #[test]
    fn bind_table_matches_a_virtio_net_node() {
        // One entry at the declared exact-match priority, matching a
        // discovered virtio node whose probed device id is `virtio-net`.
        assert_eq!(BIND_KEYS.len(), 1);
        assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
        let net = HwMatchKey::virtio(VIRTIO_NET_DEVICE_ID);
        assert!(BIND_KEYS[0].key.matches(&net));

        // A different virtio device (e.g. virtio-blk, device id 2) and a
        // non-virtio node both fail the match — the caller leaves them
        // unbound rather than guessing.
        let blk = HwMatchKey::virtio(2);
        assert!(!BIND_KEYS[0].key.matches(&blk));
        let pci_net = HwMatchKey::pci(0x1234, 0x5678, 0x02_00_00);
        assert!(!BIND_KEYS[0].key.matches(&pci_net));
    }
}
