//! The kernel-resident `/System` driver-store file-read IPC **server**
//! (Design D D2b-2 — `.junie/next-pi-prompt.md`).
//!
//! The disk-owning driver-store kthread (`crate::shared_block::DriverStoreService`)
//! keeps the read-only signed-bundle `/System` volume mounted for the life
//! of the system (`AGENTS.md` §18.3 / §18.4). The reactive user-space
//! device manager (`userland/system/devmgr`) reaches that volume through a
//! single capability-gated synchronous IPC call endpoint — the
//! [`rustos_abi::SyscallNumber::IPC_CALL`] surface served by a
//! [`rustos_kernel_ipc::CallEndpoint`] this server drains.
//!
//! This module is the arch-neutral half of that server: it owns no device
//! and no scheduling, only the request→reply translation. [`build_reply`]
//! decodes one [`FileRequest`] and serves it against a
//! [`SystemFileService`] (the same `list_store` + `ImageSource::read` the
//! in-kernel autoload uses, `AGENTS.md` §2.2), framing the answer with the
//! shared [`rustos_abi::driver_store`] wire encoders. [`serve_pending`]
//! glues that to the endpoint: drain one received call, build its reply,
//! reply, and wake the parked caller. The per-arch kthread loop
//! (`crate::aarch64::root_unlock`) calls [`serve_pending`] between parks.
//!
//! # Fail closed
//!
//! Every refusal — a malformed request, a read outside the store, a reply
//! that will not fit the endpoint's bound — is delivered **in band** as a
//! status-framed error reply (`AGENTS.md` §5.4 / §2.9), never a truncated
//! payload and never a panic. The server adds no authority of its own:
//! every capability and §5.3 check stays in `kernel/core` behind the
//! [`SystemFileService`] delegation.

use core::convert::Infallible;

use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::driver_store::{
    self, FileRequest, DRIVER_STORE_ENDPOINT, DRIVER_STORE_PATH_MAX, FILE_READ_CHUNK_MAX,
    READ_HEADER_LEN,
};
use rustos_abi::Errno;
use rustos_caps::CapabilitySet;
use rustos_drvhost::ImageSource;
use rustos_kernel_core::{CooperativeYield, SYSTEM_VOLUME_STORE_PATH};
use rustos_kernel_ipc::{CallEndpoint, CallEndpointLimits, EndpointId};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::Sink;

use crate::root_mount::RootVolume;
use crate::system_files::SystemFileService;

/// Maximum request payload the driver-store endpoint accepts: a
/// [`FileRequest::Read`] header plus the longest store-relative path
/// (`AGENTS.md` §24.4 — a validation bound, not a scaling capacity).
///
/// Derived from the shared protocol bounds (`AGENTS.md` §2.2) so the
/// server's request cap can never drift from what a valid request encodes.
// The sum is `13 + 255 = 268`, far below `u32::MAX`, so the narrowing cast
// cannot truncate; the bounds it derives from are themselves fixed protocol
// constants (`AGENTS.md` §24.4).
#[allow(clippy::cast_possible_truncation)]
pub const DRIVER_STORE_MAX_REQUEST: u32 = (READ_HEADER_LEN + DRIVER_STORE_PATH_MAX) as u32;

/// Maximum reply payload the driver-store endpoint emits, and the size of
/// the server's per-reply staging buffer. Comfortably holds a
/// [`FILE_READ_CHUNK_MAX`]-byte read frame and a full store listing; a
/// reply that would exceed it fails closed in-band (`AGENTS.md` §2.9).
pub const DRIVER_STORE_MAX_REPLY: u32 = 64 * 1024;

/// Maximum number of outstanding calls the endpoint queues before failing
/// closed (`AGENTS.md` §24.1 — a fail-closed memory bound, not a scaling
/// capacity).
pub const DRIVER_STORE_CAPACITY: usize = 16;

/// Create the well-known read-only `/System` driver-store call endpoint
/// ([`DRIVER_STORE_ENDPOINT`]).
///
/// The endpoint restricts callers to those holding
/// [`rustos_abi::CapabilityId::DRV_LOAD`] (the device manager's authority to
/// read the store, `AGENTS.md` §5.2); binding such a restricted-sender
/// endpoint requires the `creator` to hold
/// [`rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED`]. The server (the
/// disk-owning kthread) is the single bound receiver and re-checks nothing
/// thereafter (`AGENTS.md` §5.2).
///
/// # Errors
///
/// The [`Errno`] from [`CallEndpoint::create`] if `creator` lacks the bind
/// authority (fail closed, `AGENTS.md` §5.4).
pub fn create_driver_store_endpoint<S: Sink + ?Sized>(
    creator: &TaskCapabilities,
    audit: &S,
) -> Result<CallEndpoint, Errno> {
    let mut required_send = CapabilitySet::empty();
    required_send.insert(rustos_abi::CapabilityId::DRV_LOAD);
    CallEndpoint::create(
        EndpointId(DRIVER_STORE_ENDPOINT),
        creator,
        required_send,
        CapabilitySet::empty(),
        CallEndpointLimits {
            max_request: DRIVER_STORE_MAX_REQUEST,
            max_reply: DRIVER_STORE_MAX_REPLY,
            capacity: DRIVER_STORE_CAPACITY,
        },
        audit,
    )
}

/// Decode and serve one driver-store [`FileRequest`] against `service`,
/// returning the status-framed reply bytes to hand back to the caller.
///
/// * [`FileRequest::List`] → the `/System/Drivers/` store listing
///   ([`driver_store::encode_list_reply`]).
/// * [`FileRequest::Read`] → the requested `[offset, offset+len)` window of
///   the named bundle ([`driver_store::encode_read_reply`]); an
///   offset/length past end yields the shorter available run (a read of
///   zero bytes signals end-of-file).
///
/// Every error — a malformed request, a denied/missing bundle, a reply that
/// will not fit [`DRIVER_STORE_MAX_REPLY`] — is encoded as an in-band error
/// reply rather than dropped (`AGENTS.md` §5.4 / §2.9). The reply is never
/// silently truncated.
#[must_use]
pub fn build_reply<F>(
    service: &SystemFileService<'_, F>,
    request: &[u8],
    audit: &dyn Sink,
) -> Vec<u8>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut buf = alloc::vec![0u8; DRIVER_STORE_MAX_REPLY as usize];
    let written = encode_reply_into(&mut buf, service, request, audit);
    buf.truncate(written);
    buf
}

/// Encode the reply for `request` into `buf`, returning the byte count.
///
/// Factored out of [`build_reply`] so the "the chosen reply did not fit the
/// bound" recovery is expressed once: any encoder [`Errno`] (e.g. a listing
/// larger than [`DRIVER_STORE_MAX_REPLY`]) is reported in-band as a
/// status-only error reply, which always fits.
fn encode_reply_into<F>(
    buf: &mut [u8],
    service: &SystemFileService<'_, F>,
    request: &[u8],
    audit: &dyn Sink,
) -> usize
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let result = match FileRequest::decode(request) {
        Err(err) => driver_store::encode_error_reply(buf, err),
        Ok(FileRequest::List) => {
            let paths = service.list_store(audit);
            let refs: Vec<&str> = paths.iter().map(alloc::string::String::as_str).collect();
            driver_store::encode_list_reply(buf, &refs)
        }
        Ok(FileRequest::Read { path, offset, len }) => read_window(buf, service, path, offset, len),
    };
    result.unwrap_or_else(|err| {
        // The chosen reply did not fit `DRIVER_STORE_MAX_REPLY`; report the
        // failure in band. A status-only frame always fits the buffer.
        driver_store::encode_error_reply(buf, err).unwrap_or(0)
    })
}

/// Read the `[offset, offset+len)` window of the bundle at `path` and frame
/// it as a read reply.
fn read_window<F>(
    buf: &mut [u8],
    service: &SystemFileService<'_, F>,
    path: &str,
    offset: u64,
    len: u32,
) -> Result<usize, Errno>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut image = Vec::new();
    if let Err(err) = service.read(path, &mut image) {
        return driver_store::encode_error_reply(buf, err);
    }
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(image.len());
    let want = (len as usize).min(FILE_READ_CHUNK_MAX as usize);
    let end = start.saturating_add(want).min(image.len());
    driver_store::encode_read_reply(buf, &image[start..end])
}

/// Drain at most one received call from `endpoint`, serve it against
/// `service`, reply, and wake the parked caller. Returns `true` if a call
/// was served, `false` if none was pending (the kthread should park).
///
/// This never blocks (`AGENTS.md` §2.1): [`CallEndpoint::recv_call`] returns
/// immediately, and the per-arch kthread loop parks between calls. The wake
/// is co-located with the reply so a served caller is always re-readied
/// (`crate::rustos_kernel_core::call_wake`, a no-op before the boot path
/// installs the wait-queue arch hook).
pub fn serve_pending<F>(
    service: &SystemFileService<'_, F>,
    endpoint: &CallEndpoint,
    audit: &dyn Sink,
) -> bool
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let Some(call) = endpoint.recv_call() else {
        return false;
    };
    let reply = build_reply(service, &call.request, audit);
    // A reply failure (oversize / unknown ticket) is itself fail-closed and
    // audited inside `CallEndpoint::reply`; the caller is still woken so it
    // re-checks and abandons rather than parking forever (`AGENTS.md` §2.9).
    let _ = endpoint.reply(call.ticket, &reply, audit);
    rustos_kernel_core::call_wake();
    true
}

/// Serve the read-only `/System` driver-store file-read IPC endpoint over
/// the already-mounted `volume` **for the life of the system** — the
/// never-returning body the disk-owning kthread runs in place of the bare
/// [`crate::shared_block::DriverStoreService::hold`] park (Design D D2b-2,
/// a-2).
///
/// It builds one [`SystemFileService`] over `volume` (the root-backed VFS
/// mounted once, `AGENTS.md` §2.16), binds the well-known driver-store
/// [`CallEndpoint`] under `binder` (which must hold
/// [`rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED`] — see
/// [`crate::unlock_service::store_endpoint_binder_caps`]), registers it in
/// the kernel call-endpoint registry so the `ipc_call` syscall handler can
/// resolve it, then loops: drain one pending call ([`serve_pending`]) or
/// **park** off the run queue through `coop` when none is pending
/// (`AGENTS.md` §2.1 — never a busy-yield). Every served caller is woken by
/// [`serve_pending`].
///
/// `volume`, the [`SystemFileService`] built over it, and the endpoint all
/// live on the calling kthread's frame for the whole serve loop; the
/// kthread's device bring-up chain stays suspended beneath this call, so the
/// borrowed device backing (DMA pool, MMIO map, IRQ waiter, virtio host)
/// stays live with no `'static` promotion (`AGENTS.md` §2.17 — the
/// metal-proven device-driving model is unchanged).
///
/// # Errors
///
/// Returns a stable stage string fail-closed (`AGENTS.md` §2.9), **without**
/// entering the serve loop, if the file service cannot open the mounted
/// volume, the endpoint cannot be bound (`binder` lacks the privileged bind
/// authority), or its well-known id is already registered. The caller then
/// parks the kthread without a live endpoint, so an `ipc_call` to the store
/// fails closed with `NotFound` rather than blocking. The success arm never
/// returns (the [`Infallible`] `Ok` is never produced).
pub fn serve_system_store(
    volume: &mut dyn RootVolume,
    binder: &TaskCapabilities,
    coop: &CooperativeYield<'_>,
    audit: &dyn Sink,
) -> Result<Infallible, &'static str> {
    let service = SystemFileService::open(volume, SYSTEM_VOLUME_STORE_PATH)
        .map_err(|_| "driver-store: /System mount unreadable for the file service")?;
    let endpoint = Arc::new(
        create_driver_store_endpoint(binder, audit)
            .map_err(|_| "driver-store: could not bind the file-read endpoint")?,
    );
    rustos_kernel_core::callreg::register(endpoint.clone())
        .map_err(|_| "driver-store: file-read endpoint id already bound")?;
    loop {
        if !serve_pending(&service, &endpoint, audit) {
            coop.park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::system_files::SystemFileService;
    use crate::test_support::MockRootFs;
    use rustos_abi::driver_store::{decode_list_reply, decode_read_reply, reply_status};

    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &rustos_log::Event<'_>) {}
    }

    fn service_with(files: &[(&str, &[u8])]) -> MockRootFs {
        let mut fs = MockRootFs::new();
        for (path, bytes) in files {
            fs.add_file(path, bytes);
        }
        fs
    }

    #[test]
    fn a_list_request_frames_the_store_paths() {
        let mut fs = service_with(&[
            ("/System/Drivers/input/kbd", b"K"),
            ("/System/Drivers/storage/blk", b"B"),
        ]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let mut req = [0u8; 8];
        let n = FileRequest::List.encode(&mut req).expect("encode list");

        let reply = build_reply(&service, &req[..n], &NullSink);
        let mut paths: Vec<&str> = decode_list_reply(&reply)
            .expect("ok frame")
            .map(|r| r.expect("path"))
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            ["/System/Drivers/input/kbd", "/System/Drivers/storage/blk"]
        );
    }

    #[test]
    fn a_read_request_frames_the_requested_window() {
        let mut fs = service_with(&[("/System/Drivers/usb_kbd", b"BUNDLEBYTES")]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let req = FileRequest::Read {
            path: "/System/Drivers/usb_kbd",
            offset: 0,
            len: 6,
        };
        let mut buf = [0u8; 64];
        let n = req.encode(&mut buf).expect("encode read");

        let reply = build_reply(&service, &buf[..n], &NullSink);
        assert_eq!(decode_read_reply(&reply), Ok(&b"BUNDLE"[..]));
    }

    #[test]
    fn a_read_past_end_returns_the_short_available_run() {
        let mut fs = service_with(&[("/System/Drivers/x", b"abc")]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let req = FileRequest::Read {
            path: "/System/Drivers/x",
            offset: 1,
            len: 100,
        };
        let mut buf = [0u8; 64];
        let n = req.encode(&mut buf).expect("encode read");

        let reply = build_reply(&service, &buf[..n], &NullSink);
        assert_eq!(decode_read_reply(&reply), Ok(&b"bc"[..]));
    }

    #[test]
    fn a_read_offset_at_or_past_end_is_an_empty_eof_read() {
        let mut fs = service_with(&[("/System/Drivers/x", b"abc")]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let req = FileRequest::Read {
            path: "/System/Drivers/x",
            offset: 3,
            len: 10,
        };
        let mut buf = [0u8; 64];
        let n = req.encode(&mut buf).expect("encode read");

        let reply = build_reply(&service, &buf[..n], &NullSink);
        assert_eq!(decode_read_reply(&reply), Ok(&[][..]));
    }

    #[test]
    fn a_missing_bundle_is_an_in_band_not_found_error_reply() {
        let mut fs = service_with(&[("/System/Drivers/present", b"x")]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let req = FileRequest::Read {
            path: "/System/Drivers/absent",
            offset: 0,
            len: 10,
        };
        let mut buf = [0u8; 64];
        let n = req.encode(&mut buf).expect("encode read");

        let reply = build_reply(&service, &buf[..n], &NullSink);
        assert_eq!(reply_status(&reply), Err(Errno::NotFound));
    }

    #[test]
    fn a_path_outside_the_store_is_an_in_band_permission_denied() {
        let mut fs = service_with(&[("/System/Security/Users", b"secret")]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let req = FileRequest::Read {
            path: "/System/Security/Users",
            offset: 0,
            len: 10,
        };
        let mut buf = [0u8; 64];
        let n = req.encode(&mut buf).expect("encode read");

        let reply = build_reply(&service, &buf[..n], &NullSink);
        assert_eq!(reply_status(&reply), Err(Errno::PermissionDenied));
    }

    #[test]
    fn a_malformed_request_is_an_in_band_error_reply() {
        let mut fs = service_with(&[]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        // Opcode 0xFF is unknown → OutOfRange (decode), surfaced in band.
        let reply = build_reply(&service, &[0xFF], &NullSink);
        assert_eq!(reply_status(&reply), Err(Errno::OutOfRange));
    }

    /// A [`YieldHandle`] the serve loop must never reach: the fail-closed
    /// bind path returns before the loop, so neither `park` nor `yield_now`
    /// may fire.
    struct UnreachableYielder;
    impl rustos_kernel_core::YieldHandle for UnreachableYielder {
        fn yield_now(&mut self) {
            panic!("serve_system_store must not yield on the fail-closed bind path");
        }
        fn park(&mut self) {
            panic!("serve_system_store must not park on the fail-closed bind path");
        }
    }

    /// `serve_system_store` fails closed — returning a stable stage string
    /// **without** registering the well-known endpoint or entering the serve
    /// loop — when its binder lacks `CAP_IPC_BIND_PRIVILEGED` and so cannot
    /// bind the restricted-sender driver-store endpoint (`AGENTS.md` §5.4 /
    /// §2.9). An `ipc_call` to the store then resolves nothing and fails
    /// closed with `NotFound` rather than blocking forever.
    #[test]
    fn serve_system_store_without_bind_authority_fails_closed_and_registers_nothing() {
        use rustos_caps::CapabilitySet;
        use rustos_kernel_core::CooperativeYield;
        use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
        use rustos_kernel_sec::identity::UserId;

        // Guard against a leaked binding from an earlier aborted run so the
        // assertion reflects this call alone (the registry is process-global).
        rustos_kernel_core::callreg::unregister(EndpointId(DRIVER_STORE_ENDPOINT));

        let mut volume = service_with(&[("/System/Drivers/usb_kbd", b"BUNDLE")]);
        // A binder holding no capabilities — in particular not
        // `IPC_BIND_PRIVILEGED` — may not bind a restricted-sender endpoint.
        let binder = TaskCapabilities::derive(
            TaskId(0x5b4),
            UserId(0),
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            &NullSink,
        );
        let mut yielder = UnreachableYielder;
        let coop = CooperativeYield::new(&mut yielder);

        let err = serve_system_store(&mut volume, &binder, &coop, &NullSink)
            .expect_err("an unprivileged binder cannot bind the store endpoint");
        assert_eq!(err, "driver-store: could not bind the file-read endpoint");
        assert!(
            !rustos_kernel_core::callreg::contains(EndpointId(DRIVER_STORE_ENDPOINT)),
            "a refused bind must leave the registry untouched"
        );
    }
}
