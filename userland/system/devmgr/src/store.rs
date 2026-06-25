//! The reactive device manager's read-only `/System` driver-store client
//! (Design D D2b-2c — `.junie/next-pi-prompt.md`).
//!
//! Under Design D the one bootstrap-floor disk is owned for the life of the
//! system by the never-returning kernel driver-store service, which keeps
//! the read-only signed-bundle `/System` volume mounted. The user-space device manager reaches that service through a
//! single capability-gated synchronous IPC call endpoint — the
//! [`rustos_abi::SyscallNumber::IPC_CALL`] surface served by the kernel over
//! the well-known [`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`].
//!
//! This module is the client half of the [`rustos_abi::driver_store`] wire
//! protocol. It owns *policy* only: it
//! [`fetch_catalogue`]s the store (opaque `bundle_id` + the bind table the
//! kernel decoded from each bundle's signed manifest), and — once it has
//! matched a node — asks the kernel to [`load_driver`] one bundle for one
//! node. The bundle bytes, signature verification, and spawn all stay in the
//! kernel TCB; this client never sees a `/System` path or an image byte. The
//! [`DriverStoreCall`] seam abstracts the one `ipc_call` transition so the
//! framing/decoding is host-tested against a scripted double, independently
//! of the freestanding `rustos_rt::ipc_call` syscall it binds in production. The kernel re-checks the caller's
//! [`rustos_abi::CapabilityId::DRV_LOAD`] on every call;
//! this client adds no authority.

extern crate alloc;

use alloc::vec::Vec;

use rustos_abi::driver_store::{
    decode_catalogue_reply, decode_load_reply, decode_unload_reply, StoreRequest, LOAD_REQUEST_LEN,
    UNLOAD_REQUEST_LEN,
};
use rustos_abi::{DriverBindKey, Errno, DRIVER_MANIFEST_MAX_BIND_KEYS};
use rustos_devmatch::DriverCandidate;

/// The single synchronous driver-store IPC call the client issues,
/// abstracted so the protocol logic is host-testable against a scripted
/// double.
///
/// The production implementation (the freestanding `devmgr` `Run` binary)
/// backs it with `rustos_rt::ipc_call` to
/// [`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`]; the host tests back
/// it with an in-memory store.
pub trait DriverStoreCall {
    /// Post `request` to the driver-store endpoint and copy the reply into
    /// `reply`, returning the number of reply bytes written.
    ///
    /// # Errors
    ///
    /// The [`Errno`] the call failed with — a missing send capability
    /// ([`Errno::PermissionDenied`]), an unbound endpoint
    /// ([`Errno::NotFound`]), or a reply larger than `reply`
    /// ([`Errno::BufferTooSmall`]). The client surfaces it fail-closed, never a truncated reply.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// One installed driver bundle, as the catalogue exposes it: the opaque
/// kernel `bundle_id` to name in a [`load_driver`] request, and the bind
/// table the kernel decoded from the bundle's signed manifest.
///
/// The device manager matches [`Self::candidate`] against each discovered
/// hardware-tree node with [`rustos_devmatch::resolve`]; the matched
/// bundle's [`Self::bundle_id`] is what it then asks the kernel to load.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogueDriver {
    /// The opaque kernel bundle id, valid only against the kernel that
    /// issued this catalogue (a stable index into the kernel's deterministic
    /// store scan).
    pub bundle_id: u32,
    /// The bundle's decoded bind table.
    pub bind_keys: Vec<DriverBindKey>,
}

impl CatalogueDriver {
    /// Borrow this bundle as a [`DriverCandidate`] for the matcher.
    ///
    /// The `path` field is unused by [`rustos_devmatch::resolve`] (matching
    /// is pure bind-key comparison); the device manager keys the load on
    /// [`Self::bundle_id`], so the kernel resolves the path itself and no
    /// `/System` path ever crosses to user space.
    #[must_use]
    pub fn candidate(&self) -> DriverCandidate<'_> {
        DriverCandidate {
            path: "",
            bind_keys: &self.bind_keys,
        }
    }
}

/// Fetch the installed driver catalogue by issuing one
/// [`StoreRequest::Catalogue`] call through `call` and decoding the framed
/// reply into owned [`CatalogueDriver`] records.
///
/// `reply_buf` is the caller-owned buffer the reply is received into; a
/// catalogue that does not fit it fails closed ([`Errno::BufferTooSmall`]
/// from the transport), never truncated.
///
/// # Errors
///
/// The [`Errno`] from the transport ([`DriverStoreCall::call`]) or the
/// in-band error the kernel framed (an unreadable store), or a decode
/// failure on a malformed reply — each surfaced fail-closed.
pub fn fetch_catalogue<C: DriverStoreCall + ?Sized>(
    call: &mut C,
    reply_buf: &mut [u8],
) -> Result<Vec<CatalogueDriver>, Errno> {
    let mut request = [0u8; 1];
    let request_len = StoreRequest::Catalogue.encode(&mut request)?;
    let reply_len = call.call(&request[..request_len], reply_buf)?;

    let mut drivers = Vec::new();
    let mut keys = [DriverBindKey::new(0, rustos_abi::HwMatchKey::virtio(0));
        DRIVER_MANIFEST_MAX_BIND_KEYS as usize];
    for entry in decode_catalogue_reply(&reply_buf[..reply_len])? {
        // A truncated entry fails the whole catalogue closed rather than
        // yielding a partial inventory.
        let entry = entry?;
        let count = entry.decode_keys(&mut keys)?;
        drivers.push(CatalogueDriver {
            bundle_id: entry.bundle_id,
            bind_keys: keys[..count].to_vec(),
        });
    }
    Ok(drivers)
}

/// Ask the kernel to load the bundle `bundle_id` for the matched hardware-
/// tree node `node_id`, returning the loaded driver's handle.
///
/// The kernel re-reads the bundle, re-runs the full signed gate, and
/// spawns it with **only** the resources `node_id` requested — the device
/// manager supplies no bytes and no grants. `reply`
/// is the caller-owned buffer the framed reply is received into.
///
/// # Errors
///
/// The [`Errno`] from the transport ([`DriverStoreCall::call`]) or the
/// in-band error the kernel framed (an unknown id, or the load gate's
/// refusal) — surfaced fail-closed.
pub fn load_driver<C: DriverStoreCall + ?Sized>(
    call: &mut C,
    bundle_id: u32,
    node_id: u32,
    reply: &mut [u8],
) -> Result<u64, Errno> {
    let mut request = [0u8; LOAD_REQUEST_LEN];
    let request_len = StoreRequest::Load { bundle_id, node_id }.encode(&mut request)?;
    let reply_len = call.call(&request[..request_len], reply)?;
    decode_load_reply(&reply[..reply_len])
}

/// Ask the kernel to unload the driver instance named by `handle` (the value
/// a prior [`load_driver`] returned), the symmetric partner of
/// [`load_driver`].
///
/// The kernel reaps the driver's task and reclaims its grants, served
/// endpoints, IRQ bindings, and address space. `reply` is the caller-owned
/// buffer the framed status-only reply is received into. Teardown is
/// idempotent: unloading a handle naming no live driver surfaces a benign
/// [`Errno::NotFound`].
///
/// # Errors
///
/// The [`Errno`] from the transport ([`DriverStoreCall::call`]) or the
/// in-band error the kernel framed (e.g. [`Errno::NotFound`] for an
/// already-gone handle) — surfaced fail-closed.
pub fn unload_driver<C: DriverStoreCall + ?Sized>(
    call: &mut C,
    handle: u64,
    reply: &mut [u8],
) -> Result<(), Errno> {
    let mut request = [0u8; UNLOAD_REQUEST_LEN];
    let request_len = StoreRequest::Unload { handle }.encode(&mut request)?;
    let reply_len = call.call(&request[..request_len], reply)?;
    decode_unload_reply(&reply[..reply_len])
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;

    use rustos_abi::driver_store::{
        encode_catalogue_reply, encode_error_reply, encode_load_reply, encode_unload_reply,
    };
    use rustos_abi::HwMatchKey;

    fn bind(priority: u16, virtio: u32) -> DriverBindKey {
        DriverBindKey::new(priority, HwMatchKey::virtio(virtio))
    }

    /// A scripted store serving a fixed catalogue and a fixed load handle —
    /// the in-memory analogue of the kernel server's `build_reply`, so the
    /// client's framing/decoding round-trips against a real wire reply.
    struct ScriptedStore {
        catalogue: Vec<(u32, Vec<DriverBindKey>)>,
        last_load: Option<(u32, u32)>,
        last_unload: Option<u64>,
    }

    impl DriverStoreCall for ScriptedStore {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            match StoreRequest::decode(request)? {
                StoreRequest::Catalogue => {
                    let entries: Vec<(u32, &[DriverBindKey])> = self
                        .catalogue
                        .iter()
                        .map(|(id, keys)| (*id, keys.as_slice()))
                        .collect();
                    encode_catalogue_reply(reply, &entries)
                }
                StoreRequest::Load { bundle_id, node_id } => {
                    self.last_load = Some((bundle_id, node_id));
                    encode_load_reply(reply, 0xAB00 + u64::from(bundle_id))
                }
                StoreRequest::Unload { handle } => {
                    self.last_unload = Some(handle);
                    encode_unload_reply(reply)
                }
            }
        }
    }

    /// A store that fails every call in band (e.g. an unreadable store).
    struct FailingStore(Errno);

    impl DriverStoreCall for FailingStore {
        fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            encode_error_reply(reply, self.0)
        }
    }

    /// A transport that itself fails closed before any reply is framed.
    struct UnboundEndpoint;

    impl DriverStoreCall for UnboundEndpoint {
        fn call(&mut self, _request: &[u8], _reply: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::NotFound)
        }
    }

    #[test]
    fn fetch_catalogue_decodes_each_bundle_and_its_bind_keys() {
        let mut store = ScriptedStore {
            catalogue: vec![
                (10, vec![bind(5, 0x1234)]),
                (20, vec![bind(4, 16), bind(7, 1)]),
            ],
            last_load: None,
            last_unload: None,
        };
        let mut buf = [0u8; 1024];
        let drivers = fetch_catalogue(&mut store, &mut buf).expect("a readable store");
        assert_eq!(drivers.len(), 2);
        assert_eq!(drivers[0].bundle_id, 10);
        assert_eq!(drivers[0].bind_keys, vec![bind(5, 0x1234)]);
        assert_eq!(drivers[1].bundle_id, 20);
        assert_eq!(drivers[1].bind_keys, vec![bind(4, 16), bind(7, 1)]);
        // The candidate view lends the decoded bind keys for the matcher.
        assert_eq!(drivers[1].candidate().bind_keys, drivers[1].bind_keys);
    }

    #[test]
    fn fetch_catalogue_of_an_empty_store_is_empty() {
        let mut store = ScriptedStore {
            catalogue: Vec::new(),
            last_load: None,
            last_unload: None,
        };
        let mut buf = [0u8; 64];
        assert!(fetch_catalogue(&mut store, &mut buf)
            .expect("an empty store")
            .is_empty());
    }

    #[test]
    fn fetch_catalogue_surfaces_an_in_band_error_fail_closed() {
        let mut store = FailingStore(Errno::PermissionDenied);
        let mut buf = [0u8; 64];
        assert_eq!(
            fetch_catalogue(&mut store, &mut buf),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn fetch_catalogue_surfaces_a_transport_failure_fail_closed() {
        let mut store = UnboundEndpoint;
        let mut buf = [0u8; 64];
        assert_eq!(fetch_catalogue(&mut store, &mut buf), Err(Errno::NotFound));
    }

    #[test]
    fn load_driver_round_trips_the_request_and_handle() {
        let mut store = ScriptedStore {
            catalogue: Vec::new(),
            last_load: None,
            last_unload: None,
        };
        let mut buf = [0u8; 64];
        let handle = load_driver(&mut store, 7, 0x0001_3002, &mut buf).expect("the load succeeds");
        assert_eq!(handle, 0xAB00 + 7);
        assert_eq!(store.last_load, Some((7, 0x0001_3002)));
    }

    #[test]
    fn load_driver_surfaces_the_gate_refusal_fail_closed() {
        // The kernel framed the gate's refusal in band; the client
        // surfaces it rather than fabricating a handle.
        let mut store = FailingStore(Errno::SignatureInvalid);
        let mut buf = [0u8; 64];
        assert_eq!(
            load_driver(&mut store, 0, 2, &mut buf),
            Err(Errno::SignatureInvalid)
        );
    }

    #[test]
    fn unload_driver_round_trips_the_handle() {
        let mut store = ScriptedStore {
            catalogue: Vec::new(),
            last_load: None,
            last_unload: None,
        };
        let mut buf = [0u8; 64];
        unload_driver(&mut store, 0xDEAD_BEEF, &mut buf).expect("the unload succeeds");
        assert_eq!(store.last_unload, Some(0xDEAD_BEEF));
    }

    #[test]
    fn unload_driver_surfaces_an_already_gone_handle_fail_closed() {
        // The kernel reports a handle naming no live driver as `NotFound`;
        // the client surfaces it (the device manager treats it as benign).
        let mut store = FailingStore(Errno::NotFound);
        let mut buf = [0u8; 64];
        assert_eq!(
            unload_driver(&mut store, 0x1234, &mut buf),
            Err(Errno::NotFound)
        );
    }
}
