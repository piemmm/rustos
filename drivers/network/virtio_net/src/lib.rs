//! TAIRiX virtio-net link-layer driver identity.
//!
//! The device engine itself — the bus-agnostic virtio-net bring-up and
//! frame-ring [`Net`](tairix_abi::driver::net::Net) service — lives in
//! the `lib/virtio_net` crate and is re-exported here as [`VirtioNet`].
//! It is hoisted into `lib/*` (rather than kept in this `drivers/*`
//! crate) so a user-space driver *process* can link the engine directly:
//! a process crate may depend on `lib/*` but never on another `drivers/*`
//! crate (`AGENTS.md` §17.4). The concrete driver is the user-space
//! process crate `drivers/network/virtio_net_driver`, which brings the
//! device up over that shared engine in its own address space.
//!
//! # Public surface
//!
//! This crate is the driver's discovery *identity*: [`BIND_KEYS`] is the
//! single §18.3 bind table the signed manifest is authored from and the
//! device manager (or the in-kernel bootstrap-floor catalogue) resolves a
//! discovered virtio-net node against. [`VirtioNet`] and
//! [`VIRTIO_NET_DEVICE_ID`] are re-exported so a driver process and the
//! kernel's bootstrap-floor discovery share one definition of the engine
//! and the device id (§2.2).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::{DriverBindKey, HwMatchKey};

pub use tairix_virtio_net::VirtioNet;

/// The virtio device id of a network device (virtio 1.1 §5.1 — `virtio-net`
/// is device type 1), re-exported from `lib/virtio_net` (its single
/// definition, shared with the kernel's bootstrap-floor discovery, §2.2).
/// This driver's [`BIND_KEYS`] match key is built from it, so a discovered
/// virtio node whose probed device id is 1 binds this driver and nothing
/// else.
pub use tairix_virtio_net::VIRTIO_NET_DEVICE_ID;

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

#[cfg(test)]
mod tests {
    use super::*;

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
