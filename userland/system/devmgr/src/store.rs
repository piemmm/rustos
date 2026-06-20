//! The reactive device manager's read-only `/System` driver-store client
//! (Design D D2b — `.junie/next-pi-prompt.md`).
//!
//! Under Design D the one bootstrap-floor disk is owned for the life of the
//! system by the never-returning kernel driver-store service, which keeps
//! the read-only signed-bundle `/System` volume mounted (`AGENTS.md` §18.3 /
//! §18.4). The user-space device manager reaches that volume through a
//! single capability-gated synchronous IPC call endpoint — the
//! [`rustos_abi::SyscallNumber::IPC_CALL`] surface served by the kernel over
//! the well-known [`rustos_abi::driver_store::DRIVER_STORE_ENDPOINT`].
//!
//! This module is the client half of the [`rustos_abi::driver_store`] wire
//! protocol: it encodes a [`FileRequest`], issues exactly one synchronous
//! call through the [`DriverStoreCall`] seam, and decodes the status-framed
//! reply. The seam abstracts the one `ipc_call` transition so the protocol
//! logic — request framing, reply decoding, fail-closed error surfacing — is
//! exercised on the host against a scripted double, independently of the
//! freestanding `rustos_rt::ipc_call` syscall it binds in production
//! (`AGENTS.md` §2.2). The kernel server re-checks the caller's
//! [`rustos_abi::CapabilityId::DRV_LOAD`] on every call (`AGENTS.md` §5.2);
//! this client adds no authority.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::driver_store::{decode_list_reply, FileRequest};
use rustos_abi::Errno;

/// The single synchronous driver-store IPC call the client issues,
/// abstracted so the protocol logic is host-testable against a scripted
/// double (`AGENTS.md` §2.2).
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
    /// ([`Errno::BufferTooSmall`]). The client surfaces it fail-closed
    /// (`AGENTS.md` §2.9), never a truncated reply.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// List the installed `/System/Drivers/` bundle paths by issuing one
/// [`FileRequest::List`] call through `call` and decoding the framed reply
/// into owned paths (`AGENTS.md` §18.3 / §18.6).
///
/// `reply_buf` is the caller-owned buffer the reply is received into; a
/// listing that does not fit it fails closed
/// ([`Errno::BufferTooSmall`] from the transport), never truncated
/// (`AGENTS.md` §2.9 / §24.1).
///
/// # Errors
///
/// The [`Errno`] from the transport ([`DriverStoreCall::call`]) or the
/// in-band error the kernel framed (an unreadable store), or a decode
/// failure on a malformed reply — each surfaced fail-closed.
pub fn list_store<C: DriverStoreCall + ?Sized>(
    call: &mut C,
    reply_buf: &mut [u8],
) -> Result<Vec<String>, Errno> {
    // A `List` request is a single opcode byte (`FileRequest::encode`).
    let mut request = [0u8; 1];
    let request_len = FileRequest::List.encode(&mut request)?;
    let reply_len = call.call(&request[..request_len], reply_buf)?;

    let mut paths = Vec::new();
    for entry in decode_list_reply(&reply_buf[..reply_len])? {
        // A truncated or non-UTF-8 path entry fails the whole listing closed
        // rather than yielding a partial inventory (`AGENTS.md` §2.9).
        paths.push(String::from(entry?));
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec;

    use rustos_abi::driver_store::{encode_error_reply, encode_list_reply, FileRequest};

    /// A scripted [`DriverStoreCall`]: it decodes the request, asserts it is
    /// a `List`, and frames the canned `paths` as a successful list reply —
    /// the in-memory analogue of the kernel server's `build_reply` for a
    /// list request, so the client's framing/decoding round-trips against a
    /// real wire reply.
    struct ListingStore {
        paths: Vec<String>,
    }

    impl DriverStoreCall for ListingStore {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            assert_eq!(
                FileRequest::decode(request),
                Ok(FileRequest::List),
                "the client issues a List request"
            );
            let refs: Vec<&str> = self.paths.iter().map(String::as_str).collect();
            encode_list_reply(reply, &refs)
        }
    }

    /// A store whose listing fails in band: it frames an error reply carrying
    /// `err`, exactly as the kernel server does for an unreadable store.
    struct FailingStore {
        err: Errno,
    }

    impl DriverStoreCall for FailingStore {
        fn call(&mut self, _request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            encode_error_reply(reply, self.err)
        }
    }

    /// A transport that itself fails closed (e.g. the endpoint is unbound),
    /// returning the raw [`Errno`] before any reply is framed.
    struct UnboundEndpoint;

    impl DriverStoreCall for UnboundEndpoint {
        fn call(&mut self, _request: &[u8], _reply: &mut [u8]) -> Result<usize, Errno> {
            Err(Errno::NotFound)
        }
    }

    #[test]
    fn list_store_decodes_the_framed_bundle_paths() {
        let mut store = ListingStore {
            paths: vec![
                String::from("/System/Drivers/input/usb_kbd"),
                String::from("/System/Drivers/storage/virtio_blk"),
            ],
        };
        let mut buf = [0u8; 512];
        let paths = list_store(&mut store, &mut buf).expect("a readable store lists");
        assert_eq!(
            paths,
            vec![
                String::from("/System/Drivers/input/usb_kbd"),
                String::from("/System/Drivers/storage/virtio_blk"),
            ]
        );
    }

    #[test]
    fn list_store_of_an_empty_store_is_an_empty_listing() {
        let mut store = ListingStore { paths: Vec::new() };
        let mut buf = [0u8; 64];
        assert!(list_store(&mut store, &mut buf)
            .expect("an empty store lists cleanly")
            .is_empty());
    }

    #[test]
    fn list_store_surfaces_an_in_band_error_reply_fail_closed() {
        // The kernel framed an error reply (e.g. the store was unreadable);
        // the client surfaces that `Errno`, never a partial listing
        // (`AGENTS.md` §2.9).
        let mut store = FailingStore {
            err: Errno::PermissionDenied,
        };
        let mut buf = [0u8; 64];
        assert_eq!(
            list_store(&mut store, &mut buf),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn list_store_surfaces_a_transport_failure_fail_closed() {
        // The endpoint was never bound (the kernel store service did not
        // register it): the transport itself fails closed and the client
        // propagates it rather than blocking or fabricating an empty store.
        let mut store = UnboundEndpoint;
        let mut buf = [0u8; 64];
        assert_eq!(list_store(&mut store, &mut buf), Err(Errno::NotFound));
    }
}
