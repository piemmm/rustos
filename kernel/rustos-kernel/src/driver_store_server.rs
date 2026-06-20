//! The kernel-resident `/System` driver-store IPC **server**
//! (Design D D2b-2c — `.junie/next-pi-prompt.md`).
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
//! decodes one [`rustos_abi::driver_store::StoreRequest`] and serves it
//! against a [`SystemFileService`] (`AGENTS.md` §2.2): a
//! [`StoreRequest::Catalogue`] scans the signed store and frames one opaque
//! `bundle_id` + decoded bind keys per bundle, while a
//! [`StoreRequest::Load`] runs
//! the signed §8 gate and spawns the matched driver with only its node's
//! grants. The answer is framed with the shared
//! [`rustos_abi::driver_store`] wire encoders. [`serve_pending`] glues that
//! to the endpoint: drain one received call, build its reply, reply, and
//! wake the parked caller. The per-arch kthread loop
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
use rustos_abi::driver_store::{self, StoreRequest, DRIVER_STORE_ENDPOINT, LOAD_REQUEST_LEN};
use rustos_abi::{Errno, HwNode};
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_devmgr::DriverLoader;
use rustos_drvhost::store::{scan_store, DriverStore};
use rustos_kernel_core::{CooperativeYield, SYSTEM_VOLUME_STORE_PATH};
use rustos_kernel_ipc::{CallEndpoint, CallEndpointLimits, EndpointId};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::Sink;

use crate::driver_spawn_loader::{DriverProcessSpawn, SpawnDriverLoader};
use crate::root_mount::RootVolume;
use crate::system_files::SystemFileService;

/// Maximum request payload the driver-store endpoint accepts: a
/// [`StoreRequest::Load`] (the single longest request — a
/// [`StoreRequest::Catalogue`] is one opcode byte, `AGENTS.md` §24.4 — a
/// validation bound, not a scaling capacity).
///
/// Derived from the shared protocol bound (`AGENTS.md` §2.2) so the server's
/// request cap can never drift from what a valid request encodes.
// `LOAD_REQUEST_LEN` (9) is far below `u32::MAX`, so the narrowing cast
// cannot truncate; it is itself a fixed protocol constant (`AGENTS.md` §24.4).
#[allow(clippy::cast_possible_truncation)]
pub const DRIVER_STORE_MAX_REQUEST: u32 = LOAD_REQUEST_LEN as u32;

/// Maximum reply payload the driver-store endpoint emits, and the size of
/// the server's per-reply staging buffer. Comfortably holds a full store
/// catalogue (`bundle_id` + decoded bind keys per bundle); a reply that
/// would exceed it fails closed in-band (`AGENTS.md` §2.9).
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

/// The kernel-side mechanism the driver-store server keeps in its trusted
/// base to serve a [`StoreRequest::Load`] (`AGENTS.md` §4 — only *policy*,
/// the matching, lives in the user-space device manager).
///
/// It bundles the signed-load inputs ([`SpawnDriverLoader`] needs) the
/// device manager must never see — the driver-signing trust anchors, the
/// gate capability set, and the architecture process-spawn seam — together
/// with the discovered hardware tree the server resolves a matched
/// `node_id` to its resource grants against (`AGENTS.md` §18.3 / §18.1).
///
/// All references are borrowed for the life of the serve loop on the
/// disk-owning kthread's frame; the context holds no authority of its own.
pub struct StoreServeContext<'a> {
    /// Driver-signing trust anchor(s) the §8 load gate verifies every bundle
    /// against — the kernel's embedded key(s) (`AGENTS.md` §8 / §9).
    pub trusted: &'a [Ed25519PublicKey],
    /// The capability set the load gate intersects each manifest request
    /// with (`AGENTS.md` §5.2); holds `CAP_DRV_LOAD` so a user-space driver
    /// can be admitted, plus the delegatable resource caps a driver's class
    /// may request (`crate::unlock_service::autoload_caps`).
    pub caps: CapabilitySet,
    /// The architecture process-creation seam each verified driver is
    /// spawned through (`AGENTS.md` §17.1 / §2.2).
    pub spawn: &'a dyn DriverProcessSpawn,
    /// The discovered hardware tree a matched `node_id` is resolved against
    /// to mint exactly that node's resource grants (`AGENTS.md` §18.1 /
    /// §18.3 — no ambient authority, the grants originate kernel-side).
    pub tree: &'a [HwNode],
}

/// Decode and serve one driver-store [`StoreRequest`] against `service`,
/// returning the status-framed reply bytes to hand back to the caller.
///
/// * [`StoreRequest::Catalogue`] → one entry per accepted store bundle: its
///   opaque `bundle_id` (a stable index into the deterministic store scan)
///   and the bind table the kernel decoded from its signed manifest
///   ([`driver_store::encode_catalogue_reply`], `AGENTS.md` §18.6). No bytes
///   and no `/System` path cross to the caller.
/// * [`StoreRequest::Load`] → re-scan the store, resolve the named
///   `bundle_id` to its path and the matched `node_id` to its resource
///   grants, run the full signed §8 load gate, and spawn the driver into its
///   own process with **only** those grants ([`SpawnDriverLoader`],
///   `AGENTS.md` §18.3 / §4); the reply carries the loaded driver's handle.
///
/// Every error — a malformed request, an out-of-range `bundle_id`, an
/// unknown `node_id`, a gate refusal, a reply that will not fit
/// [`DRIVER_STORE_MAX_REPLY`] — is encoded as an in-band error reply rather
/// than dropped (`AGENTS.md` §5.4 / §2.9). The reply is never silently
/// truncated.
#[must_use]
pub fn build_reply<F>(
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    request: &[u8],
    audit: &dyn Sink,
) -> Vec<u8>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut buf = alloc::vec![0u8; DRIVER_STORE_MAX_REPLY as usize];
    let written = encode_reply_into(&mut buf, service, ctx, request, audit);
    buf.truncate(written);
    buf
}

/// Encode the reply for `request` into `buf`, returning the byte count.
///
/// Factored out of [`build_reply`] so the "the chosen reply did not fit the
/// bound" recovery is expressed once: any encoder [`Errno`] (e.g. a
/// catalogue larger than [`DRIVER_STORE_MAX_REPLY`]) is reported in-band as
/// a status-only error reply, which always fits.
fn encode_reply_into<F>(
    buf: &mut [u8],
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    request: &[u8],
    audit: &dyn Sink,
) -> usize
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let result = match StoreRequest::decode(request) {
        Err(err) => driver_store::encode_error_reply(buf, err),
        Ok(StoreRequest::Catalogue) => catalogue_reply(buf, service, audit),
        Ok(StoreRequest::Load { bundle_id, node_id }) => {
            load_reply(buf, service, ctx, bundle_id, node_id, audit)
        }
    };
    result.unwrap_or_else(|err| {
        // The chosen reply did not fit `DRIVER_STORE_MAX_REPLY`; report the
        // failure in band. A status-only frame always fits the buffer.
        driver_store::encode_error_reply(buf, err).unwrap_or(0)
    })
}

/// Scan the signed store and frame the catalogue: one `(bundle_id,
/// bind_keys)` entry per accepted bundle, where `bundle_id` is the bundle's
/// index in the deterministic scan order (`AGENTS.md` §18.6).
///
/// The scan reads each bundle's bytes through `service` and decodes its bind
/// table fail-closed; a malformed bundle is skipped inside the scan and
/// never appears as a candidate (`AGENTS.md` §18.4 / §5.4). No bundle bytes
/// and no `/System` path leave the kernel — only the opaque id and the
/// decoded keys the device manager matches against (`AGENTS.md` §4).
fn catalogue_reply<F>(
    buf: &mut [u8],
    service: &SystemFileService<'_, F>,
    audit: &dyn Sink,
) -> Result<usize, Errno>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let store = scan_store_view(service, audit);
    let drivers = store.drivers();
    let mut entries: Vec<(u32, &[rustos_abi::DriverBindKey])> = Vec::with_capacity(drivers.len());
    for (index, driver) in drivers.iter().enumerate() {
        // `bundle_id` is the scan-order index; the scan is deterministic over
        // the static read-only store, so a subsequent `Load` re-scan resolves
        // the same id to the same bundle (`AGENTS.md` §18.6 / §2.16).
        let bundle_id = u32::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
        entries.push((bundle_id, driver.bind_keys()));
    }
    driver_store::encode_catalogue_reply(buf, &entries)
}

/// Resolve `bundle_id` and `node_id`, run the signed §8 load gate, spawn the
/// driver, and frame its handle (`AGENTS.md` §18.3 / §4).
///
/// `bundle_id` indexes the same deterministic scan a [`StoreRequest::Catalogue`]
/// exposed; `node_id` names the matched hardware-tree node whose resource
/// requests the loaded driver is granted — and nothing more. Every refusal
/// (unknown id, gate failure) is surfaced in band (`AGENTS.md` §5.4 / §2.9).
fn load_reply<F>(
    buf: &mut [u8],
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    bundle_id: u32,
    node_id: u32,
    audit: &dyn Sink,
) -> Result<usize, Errno>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    match load_matched_driver(service, ctx, bundle_id, node_id, audit) {
        Ok(handle) => driver_store::encode_load_reply(buf, handle),
        Err(err) => driver_store::encode_error_reply(buf, err),
    }
}

/// The load mechanism behind [`load_reply`]: index `bundle_id` into the
/// store scan, resolve `node_id`'s grants, and run [`SpawnDriverLoader`].
///
/// Returns the spawned driver's handle, or the fail-closed [`Errno`] of the
/// first failed step (`AGENTS.md` §5.4): an out-of-range `bundle_id`
/// ([`Errno::NotFound`]), an unknown `node_id` ([`Errno::NotFound`]), or the
/// signed-gate refusal the loader maps from `drvhost::HostError`.
fn load_matched_driver<F>(
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    bundle_id: u32,
    node_id: u32,
    audit: &dyn Sink,
) -> Result<u64, Errno>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let store = scan_store_view(service, audit);
    let index = usize::try_from(bundle_id).map_err(|_| Errno::NotFound)?;
    let driver = store.drivers().get(index).ok_or(Errno::NotFound)?;
    // The grants the loaded driver receives originate kernel-side, from the
    // discovered tree's matched node — never from the (untrusted) caller
    // (`AGENTS.md` §4 — no ambient authority). An unknown node fails closed.
    let node = ctx
        .tree
        .iter()
        .find(|node| !node.is_root() && node.id() == node_id)
        .ok_or(Errno::NotFound)?;
    let resources = node.resources();
    let mut loader = SpawnDriverLoader::new(ctx.trusted, service, audit, ctx.spawn, &[]);
    let handle = loader.load(driver.path(), resources, &ctx.caps)?;
    Ok(handle.as_u64())
}

/// Run [`scan_store`] over the mounted `/System` store reachable through
/// `service`, returning the accepted-bundle view both the catalogue and the
/// load resolve against (`AGENTS.md` §2.2 — one scan definition).
fn scan_store_view<F>(service: &SystemFileService<'_, F>, audit: &dyn Sink) -> DriverStore
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let paths = service.list_store(audit);
    let refs: Vec<&str> = paths.iter().map(alloc::string::String::as_str).collect();
    scan_store(service, &refs, audit)
}

/// Drain at most one received call from `endpoint`, serve it against
/// `service` and `ctx`, reply, and wake the parked caller. Returns `true` if
/// a call was served, `false` if none was pending (the kthread should park).
///
/// This never blocks (`AGENTS.md` §2.1): [`CallEndpoint::recv_call`] returns
/// immediately, and the per-arch kthread loop parks between calls. The wake
/// is co-located with the reply so a served caller is always re-readied
/// (`crate::rustos_kernel_core::call_wake`, a no-op before the boot path
/// installs the wait-queue arch hook).
pub fn serve_pending<F>(
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    endpoint: &CallEndpoint,
    audit: &dyn Sink,
) -> bool
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let Some(call) = endpoint.recv_call() else {
        return false;
    };
    let reply = build_reply(service, ctx, &call.request, audit);
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
/// `ctx` carries the kernel-side load mechanism (trust anchors, gate
/// capabilities, the architecture spawn seam) and the discovered hardware
/// tree a matched `node_id` is resolved against, so a [`StoreRequest::Load`]
/// runs the signed §8 gate and spawn on this kthread's frame (`AGENTS.md`
/// §18.3 / §4 — the device manager owns matching policy only).
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
    ctx: &StoreServeContext<'_>,
    binder: &TaskCapabilities,
    coop: &CooperativeYield<'_>,
    audit: &dyn Sink,
) -> Result<Infallible, &'static str> {
    let service = SystemFileService::open(volume, SYSTEM_VOLUME_STORE_PATH)
        .map_err(|_| "driver-store: /System mount unreadable for the file service")?;
    let endpoint = Arc::new(
        create_driver_store_endpoint(binder, audit)
            .map_err(|_| "driver-store: could not bind the driver-store endpoint")?,
    );
    rustos_kernel_core::callreg::register(endpoint.clone())
        .map_err(|_| "driver-store: driver-store endpoint id already bound")?;
    // The driver store is now reachable. Wake any reactive observer parked
    // on `hw_tree_wait` (the user-space `devmgr`) so it re-attempts its
    // catalogue fetch: the endpoint binds *after* the boot tree settles, so
    // without this nudge a manager that fetched the catalogue before the
    // bind would have fail-softed to empty and never retried (`AGENTS.md`
    // §18.4). The node set is unchanged; the re-evaluation is idempotent.
    crate::hwtree_store::HW_TREE.bump();
    loop {
        // Drain every pending call first.
        if serve_pending(&service, ctx, &endpoint, audit) {
            continue;
        }
        // Nothing pending: park off the run queue until a request is posted
        // (`AGENTS.md` §2.1 — a real park, never a busy-yield). Register on
        // `SERVE_WAITQ` *before* the final drain so a request posted in the
        // window between the empty drain and the park is not lost: the
        // `ipc_call` handler's `serve_wake` unparks this task by id, and the
        // scheduler's wake-pending token converts a concurrent park into a
        // re-ready (the same lost-wakeup interlock `hw_tree_wait` relies on).
        match crate::unlock_service::store_service_task() {
            Some(task) => {
                rustos_kernel_core::SERVE_WAITQ.register(task, rustos_kernel_core::NO_DEADLINE);
                // Re-check after registering: if a call arrived in the
                // register window, serve it and skip the park.
                if serve_pending(&service, ctx, &endpoint, audit) {
                    rustos_kernel_core::SERVE_WAITQ.deregister(task);
                    continue;
                }
                coop.park();
                rustos_kernel_core::SERVE_WAITQ.deregister(task);
            }
            // The server's scheduler id was never published (a degenerate
            // build that did not go through `spawn_if_present`). Park bare
            // rather than busy-yield; the dispatch loop's wait-for-interrupt
            // re-step still re-runs the body (`AGENTS.md` §2.9).
            None => coop.park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::driver_store::{
        decode_catalogue_reply, decode_load_reply, reply_status, StoreRequest,
    };
    use rustos_abi::hwtree::HwResource;
    use rustos_abi::{
        CapabilityId, DriverBindKey, DriverKind, DriverManifest, HwDeviceClass, HwMatchKey,
        ABI_VERSION_CURRENT, DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_BIND_KEYS, HW_NODE_ROOT,
    };

    use crate::system_files::SystemFileService;
    use crate::test_support::MockRootFs;

    struct NullSink;
    impl Sink for NullSink {
        fn write_event(&self, _event: &rustos_log::Event<'_>) {}
    }

    /// Deterministic driver-signing seed for the test trust anchor; a
    /// distinct key models an untrusted signer.
    const TEST_SEED: [u8; 32] = *b"rustos-store-srv-test-signing/v1";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&TEST_SEED)
    }

    fn untrusted_key() -> SigningKey {
        let mut seed = TEST_SEED;
        seed[0] ^= 0xFF;
        SigningKey::from_bytes(&seed)
    }

    fn pubkey_of(sk: &SigningKey) -> Ed25519PublicKey {
        Ed25519PublicKey::from_bytes(&sk.verifying_key().to_bytes()).expect("well-formed key")
    }

    /// Build a signed `kind = UserSpace` `.rxe` bundle exactly as the build
    /// glue does (`AGENTS.md` §2.2): the signature covers
    /// `header[..WIRE_LEN-64] || cap_body || bind_table || payload`.
    fn build_signed_bundle(
        sk: &SigningKey,
        caps: &[CapabilityId],
        bind_keys: &[DriverBindKey],
        payload: &[u8],
    ) -> Vec<u8> {
        let signer_pubkey: [u8; 32] = sk.verifying_key().to_bytes();
        let mut manifest = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count: u8::try_from(bind_keys.len()).expect("bind keys fit u8"),
            capability_count: u16::try_from(caps.len()).expect("caps fit u16"),
            syscall_table_hash: rustos_kernel_syscall::SYSCALL_TABLE_HASH,
            signer_pubkey,
            signature: [0u8; 64],
        };
        let mut cap_body = Vec::new();
        for c in caps {
            cap_body.extend_from_slice(&c.as_u16().to_le_bytes());
        }
        let mut bind_body = Vec::new();
        for k in bind_keys {
            bind_body.extend_from_slice(&k.to_le_bytes());
        }
        let header = manifest.to_le_bytes();
        let signed_end = DriverManifest::WIRE_LEN - 64;
        let mut message = Vec::new();
        message.extend_from_slice(&header[..signed_end]);
        message.extend_from_slice(&cap_body);
        message.extend_from_slice(&bind_body);
        message.extend_from_slice(payload);
        manifest.signature = sk.sign(&message).to_bytes();
        let mut out = Vec::new();
        out.extend_from_slice(&manifest.to_le_bytes());
        out.extend_from_slice(&cap_body);
        out.extend_from_slice(&bind_body);
        out.extend_from_slice(payload);
        out
    }

    /// Records every `spawn_driver` so a test can assert the gate forwarded
    /// exactly the matched node's grants.
    struct RecordingSpawn {
        calls: RefCell<Vec<(Vec<u8>, CapabilitySet, Vec<HwResource>)>>,
    }

    impl RecordingSpawn {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl DriverProcessSpawn for RecordingSpawn {
        fn spawn_driver(
            &self,
            rxe: &[u8],
            granted: CapabilitySet,
            grants: &[HwResource],
            _args: &[&[u8]],
        ) -> Result<u64, Errno> {
            self.calls
                .borrow_mut()
                .push((rxe.to_vec(), granted, grants.to_vec()));
            Ok(0x4242)
        }
    }

    /// A spawn that must never be reached (the load fails before spawning).
    struct NoSpawn;
    impl DriverProcessSpawn for NoSpawn {
        fn spawn_driver(
            &self,
            _rxe: &[u8],
            _granted: CapabilitySet,
            _grants: &[HwResource],
            _args: &[&[u8]],
        ) -> Result<u64, Errno> {
            panic!("the load must fail closed before spawning");
        }
    }

    fn service_with(files: &[(&str, &[u8])]) -> MockRootFs {
        let mut fs = MockRootFs::new();
        for (path, bytes) in files {
            fs.add_file(path, bytes);
        }
        fs
    }

    fn gate_caps() -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        set.insert(CapabilityId::DRV_LOAD);
        set.insert(CapabilityId::MMIO_MAP);
        set.insert(CapabilityId::MEM_DMA);
        set
    }

    /// A keyboard node (id 2) keyed by `key`, carrying the MMIO + DMA grants
    /// a matched driver receives (`AGENTS.md` §18.3).
    fn keyboard_node(key: HwMatchKey) -> HwNode {
        let mut node = HwNode::new(2, 1, HwDeviceClass::Input);
        node.push_match_key(key).expect("key fits");
        node.push_resource(HwResource::mmio(0x0a00_0000, 0x200))
            .expect("mmio fits");
        node.push_resource(HwResource::dma(0x3fff_ffff, 0x1000))
            .expect("dma fits");
        node
    }

    fn tree_with(key: HwMatchKey) -> [HwNode; 2] {
        [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            keyboard_node(key),
        ]
    }

    fn ctx<'a>(
        trusted: &'a [Ed25519PublicKey],
        spawn: &'a dyn DriverProcessSpawn,
        tree: &'a [HwNode],
    ) -> StoreServeContext<'a> {
        StoreServeContext {
            trusted,
            caps: gate_caps(),
            spawn,
            tree,
        }
    }

    fn catalogue(
        service: &SystemFileService<'_, MockRootFs>,
        ctx: &StoreServeContext<'_>,
    ) -> Vec<u8> {
        let mut req = [0u8; 8];
        let n = StoreRequest::Catalogue.encode(&mut req).expect("encode");
        build_reply(service, ctx, &req[..n], &NullSink)
    }

    #[test]
    fn a_catalogue_request_frames_accepted_bundles_with_their_bind_keys() {
        let kbd_keys = [DriverBindKey::new(5, HwMatchKey::virtio(0x1234))];
        let blk_keys = [DriverBindKey::new(3, HwMatchKey::virtio(2))];
        let sk = signing_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/kbd",
            &build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &kbd_keys, b"kbd"),
        );
        fs.add_file(
            "/System/Drivers/blk",
            &build_signed_bundle(&sk, &[], &blk_keys, b"blk"),
        );
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let spawn = NoSpawn;
        let tree: [HwNode; 0] = [];
        let reply = catalogue(&service, &ctx(&[], &spawn, &tree));

        let mut kbuf =
            [DriverBindKey::new(0, HwMatchKey::virtio(0)); DRIVER_MANIFEST_MAX_BIND_KEYS as usize];
        let mut seen: Vec<(u32, DriverBindKey)> = decode_catalogue_reply(&reply)
            .expect("ok frame")
            .map(|entry| {
                let entry = entry.expect("entry");
                let n = entry.decode_keys(&mut kbuf).expect("keys");
                assert_eq!(n, 1, "each test bundle declares one bind key");
                (entry.bundle_id, kbuf[0])
            })
            .collect();
        seen.sort_by_key(|(id, _)| *id);
        // Two accepted bundles, with stable scan-order ids 0 and 1.
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, 0);
        assert_eq!(seen[1].0, 1);
        let keys: Vec<HwMatchKey> = seen.iter().map(|(_, k)| k.key).collect();
        assert!(keys.contains(&HwMatchKey::virtio(0x1234)));
        assert!(keys.contains(&HwMatchKey::virtio(2)));
    }

    #[test]
    fn a_malformed_bundle_is_skipped_from_the_catalogue() {
        let keys = [DriverBindKey::new(5, HwMatchKey::virtio(0x1234))];
        let sk = signing_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/good",
            &build_signed_bundle(&sk, &[], &keys, b"ok"),
        );
        fs.add_file("/System/Drivers/bad", b"not-a-bundle");
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let spawn = NoSpawn;
        let tree: [HwNode; 0] = [];
        let reply = catalogue(&service, &ctx(&[], &spawn, &tree));
        let count = decode_catalogue_reply(&reply).expect("ok frame").count();
        assert_eq!(count, 1, "the malformed bundle is skipped, never fatal");
    }

    #[test]
    fn a_load_spawns_the_matched_signed_driver_with_the_nodes_resources() {
        let key = HwMatchKey::virtio(0x1234);
        let keys = [DriverBindKey::new(5, key)];
        let payload = b"the-usb-kbd-rxe-bytes";
        let sk = signing_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/usb_kbd",
            &build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &keys, payload),
        );
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let trusted = [pubkey_of(&sk)];
        let spawn = RecordingSpawn::new();
        let tree = tree_with(key);
        let serve_ctx = ctx(&trusted, &spawn, &tree);

        // The single accepted bundle has scan id 0; load it for node 2.
        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id: 2,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &rbuf[..n], &NullSink);
        // The reply carries the load gate's minted driver handle (non-zero);
        // the spawn pid is informational, so the handle value is the host's,
        // not the recording spawn's pid.
        let handle = decode_load_reply(&reply).expect("the load succeeds and returns a handle");
        assert_ne!(handle, 0, "a successful load reports a non-zero handle");

        let calls = spawn.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, payload, "the verified payload is spawned");
        assert!(
            calls[0].1.contains(CapabilityId::MMIO_MAP),
            "the manifest∩caller grant reaches the spawn"
        );
        assert_eq!(
            calls[0].2,
            alloc::vec![
                HwResource::mmio(0x0a00_0000, 0x200),
                HwResource::dma(0x3fff_ffff, 0x1000)
            ],
            "the matched node's resource requests are minted, and nothing more"
        );
    }

    #[test]
    fn a_load_with_an_out_of_range_bundle_id_is_in_band_not_found() {
        let key = HwMatchKey::virtio(0x1234);
        let keys = [DriverBindKey::new(5, key)];
        let sk = signing_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/usb_kbd",
            &build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &keys, b"x"),
        );
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let trusted = [pubkey_of(&sk)];
        let spawn = NoSpawn;
        let tree = tree_with(key);
        let serve_ctx = ctx(&trusted, &spawn, &tree);

        let req = StoreRequest::Load {
            bundle_id: 99,
            node_id: 2,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &rbuf[..n], &NullSink);
        assert_eq!(reply_status(&reply), Err(Errno::NotFound));
    }

    #[test]
    fn a_load_with_an_unknown_node_id_is_in_band_not_found() {
        let key = HwMatchKey::virtio(0x1234);
        let keys = [DriverBindKey::new(5, key)];
        let sk = signing_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/usb_kbd",
            &build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &keys, b"x"),
        );
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let trusted = [pubkey_of(&sk)];
        let spawn = NoSpawn;
        let tree = tree_with(key);
        let serve_ctx = ctx(&trusted, &spawn, &tree);

        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id: 0xDEAD,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &rbuf[..n], &NullSink);
        assert_eq!(reply_status(&reply), Err(Errno::NotFound));
    }

    #[test]
    fn an_untrusted_bundle_load_fails_closed_in_band() {
        let key = HwMatchKey::virtio(0x1234);
        let keys = [DriverBindKey::new(5, key)];
        // Bundle signed by an untrusted key: the gate refuses it.
        let sk = untrusted_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/usb_kbd",
            &build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &keys, b"x"),
        );
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        // Trust anchor is the *production* signing key, not `sk`.
        let trusted = [pubkey_of(&signing_key())];
        let spawn = NoSpawn;
        let tree = tree_with(key);
        let serve_ctx = ctx(&trusted, &spawn, &tree);

        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id: 2,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &rbuf[..n], &NullSink);
        assert!(
            reply_status(&reply).is_err(),
            "an untrusted bundle never loads"
        );
    }

    #[test]
    fn a_malformed_request_is_an_in_band_error_reply() {
        let mut fs = service_with(&[]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let spawn = NoSpawn;
        let tree: [HwNode; 0] = [];
        // Opcode 0xFF is unknown → OutOfRange (decode), surfaced in band.
        let reply = build_reply(&service, &ctx(&[], &spawn, &tree), &[0xFF], &NullSink);
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

        let spawn = NoSpawn;
        let tree: [HwNode; 0] = [];
        let serve_ctx = ctx(&[], &spawn, &tree);
        let err = serve_system_store(&mut volume, &serve_ctx, &binder, &coop, &NullSink)
            .expect_err("an unprivileged binder cannot bind the store endpoint");
        assert_eq!(
            err,
            "driver-store: could not bind the driver-store endpoint"
        );
        assert!(
            !rustos_kernel_core::callreg::contains(EndpointId(DRIVER_STORE_ENDPOINT)),
            "a refused bind must leave the registry untouched"
        );
    }
}
