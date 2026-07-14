//! RustOS virtio-net link-layer driver.
//!
//! The device engine itself — the bus-agnostic virtio-net bring-up and
//! frame-ring [`Net`](rustos_abi::driver::net::Net) service — lives in
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
//! [`Net`](rustos_abi::driver::net::Net) trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; `service` additionally
//! requires the dispatcher to have verified [`CapabilityId::NET_RAW`]
//! (see `lib/abi/src/driver/net.rs`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};

pub use rustos_virtio_net::VirtioNet;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_4554_0000_0001; // "VNET"

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
            fn kind(&self) -> rustos_abi::driver::DriverKind {
                rustos_abi::driver::DriverKind::UserSpace
            }
        }
        assert_eq!(
            register(&H { grant: false }),
            Err(DriverError::PermissionDenied)
        );
        assert!(register(&H { grant: true }).is_ok());
    }
}
