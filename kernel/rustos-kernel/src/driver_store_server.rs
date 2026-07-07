//! The kernel-resident `/System` driver-store IPC **server**
//! (Design D D2b-2c — `.junie/next-pi-prompt.md`).
//!
//! The disk-owning driver-store kthread (`crate::shared_block::DriverStoreService`)
//! keeps the read-only signed-bundle `/System` volume mounted for the life
//! of the system. The reactive user-space
//! device manager (`userland/system/devmgr`) reaches that volume through a
//! single capability-gated synchronous IPC call endpoint — the
//! [`rustos_abi::SyscallNumber::IPC_CALL`] surface served by a
//! [`rustos_kernel_ipc::CallEndpoint`] this server drains.
//!
//! This module is the arch-neutral half of that server: it owns no device
//! and no scheduling, only the request→reply translation. [`build_reply`]
//! decodes one [`rustos_abi::driver_store::StoreRequest`] and serves it
//! against a [`SystemFileService`]: a
//! [`StoreRequest::Catalogue`] scans the signed store and frames one opaque
//! `bundle_id` + decoded bind keys per bundle, while a
//! [`StoreRequest::Load`] runs
//! the signed gate and spawns the matched driver with only its node's
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
//! status-framed error reply, never a truncated
//! payload and never a panic. The server adds no authority of its own:
//! every capability and check stays in `kernel/core` behind the
//! [`SystemFileService`] delegation.

use core::convert::Infallible;

use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemSecurity};
use rustos_abi::driver_store::{self, StoreRequest, DRIVER_STORE_ENDPOINT, MAX_REQUEST_LEN};
use rustos_abi::hwtree::HwResource;
use rustos_abi::Errno;
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_devmgr::DriverLoader;
use rustos_drvhost::store::{scan_store, DriverStore};
use rustos_kernel_core::{CooperativeYield, SYSTEM_VOLUME_STORE_PATH};
use rustos_kernel_ipc::{CallEndpoint, CallEndpointLimits, EndpointId, RecvCall};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::Sink;

use crate::driver_spawn_loader::{DriverProcessSpawn, SpawnDriverLoader};
use crate::hwtree_store::HwTreeStore;
use crate::root_mount::RootVolume;
use crate::system_files::SystemFileService;

/// Maximum request payload the driver-store endpoint accepts: the longest
/// of every request encoding (a [`StoreRequest::Catalogue`] is one opcode
/// byte; a [`StoreRequest::Load`] and a [`StoreRequest::Unload`] are each
/// nine) — a validation bound, not a scaling capacity.
///
/// Derived from the shared protocol bound so the server's
/// request cap can never drift from what a valid request encodes.
// `MAX_REQUEST_LEN` (9) is far below `u32::MAX`, so the narrowing cast
// cannot truncate; it is itself a fixed protocol constant.
#[allow(clippy::cast_possible_truncation)]
pub const DRIVER_STORE_MAX_REQUEST: u32 = MAX_REQUEST_LEN as u32;

/// Maximum reply payload the driver-store endpoint emits, and the size of
/// the server's per-reply staging buffer. Comfortably holds a full store
/// catalogue (`bundle_id` + decoded bind keys per bundle); a reply that
/// would exceed it fails closed in-band.
pub const DRIVER_STORE_MAX_REPLY: u32 = 64 * 1024;

/// Maximum number of outstanding calls the endpoint queues before failing
/// closed (a fail-closed memory bound, not a scaling
/// capacity).
pub const DRIVER_STORE_CAPACITY: usize = 16;

/// Create the well-known read-only `/System` driver-store call endpoint
/// ([`DRIVER_STORE_ENDPOINT`]).
///
/// The endpoint restricts callers to those holding
/// [`rustos_abi::CapabilityId::DRV_LOAD`] (the device manager's authority to
/// read the store); binding such a restricted-sender
/// endpoint requires the `creator` to hold
/// [`rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED`]. The server (the
/// disk-owning kthread) is the single bound receiver and re-checks nothing
/// thereafter.
///
/// # Errors
///
/// The [`Errno`] from [`CallEndpoint::create`] if `creator` lacks the bind
/// authority (fail closed).
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

/// Resolves a matched hardware-tree `node_id` to the resource grants the
/// loaded driver receives, read from the **live** inventory.
///
/// A [`StoreRequest::Load`] names the `node_id` the device manager matched
/// against the catalogue; the kernel mints exactly that node's grants and
/// nothing more (no ambient authority, the grants originate
/// kernel-side). Resolution MUST consult the live tree, not a boot snapshot:
/// a user-space bus driver publishes its enumerated children at runtime
/// through `hw_emit_node`, and the device manager loads a driver for such a
/// child the instant it appears. Resolving
/// against a frozen snapshot would fail every runtime-emitted node closed and
/// stall the recursive bus chain.
pub trait HwNodeResolver {
    /// The resource grants of the live non-root node `node_id`, or `None`
    /// when no live non-root node has that id (fail closed). The returned grants are owned so resolution holds no live-tree
    /// lock across the spawn.
    fn resolve_resources(&self, node_id: u32) -> Option<Vec<HwResource>>;
}

impl HwNodeResolver for HwTreeStore {
    fn resolve_resources(&self, node_id: u32) -> Option<Vec<HwResource>> {
        HwTreeStore::resolve_resources(self, node_id)
    }
}

/// The kernel-side mechanism the driver-store server keeps in its trusted
/// base to serve a [`StoreRequest::Load`] (only *policy*,
/// the matching, lives in the user-space device manager).
///
/// It bundles the signed-load inputs ([`SpawnDriverLoader`] needs) the
/// device manager must never see — the driver-signing trust anchors, the
/// gate capability set, and the architecture process-spawn seam — together
/// with the live hardware tree the server resolves a matched `node_id` to
/// its resource grants against.
///
/// All references are borrowed for the life of the serve loop on the
/// disk-owning kthread's frame; the context holds no authority of its own.
pub struct StoreServeContext<'a> {
    /// Driver-signing trust anchor(s) the load gate verifies every bundle
    /// against — the kernel's embedded key(s).
    pub trusted: &'a [Ed25519PublicKey],
    /// The capability set the load gate intersects each manifest request
    /// with; holds `CAP_DRV_LOAD` so a user-space driver
    /// can be admitted, plus the delegatable resource caps a driver's class
    /// may request (`crate::unlock_service::autoload_caps`).
    pub caps: CapabilitySet,
    /// The architecture process-creation seam each verified driver is
    /// spawned through.
    pub spawn: &'a dyn DriverProcessSpawn,
    /// The **live** hardware inventory a matched `node_id` is resolved
    /// against to mint exactly that node's resource grants (no ambient authority, the grants originate
    /// kernel-side). Backed by [`crate::hwtree_store::HW_TREE`] in
    /// production, so a node a user-space bus driver emits at runtime is
    /// resolvable the moment it is published — never a
    /// frozen boot snapshot.
    pub nodes: &'a dyn HwNodeResolver,
}

/// Decode and serve one driver-store [`StoreRequest`] against `service`,
/// returning the status-framed reply bytes to hand back to the caller.
///
/// * [`StoreRequest::Catalogue`] → one entry per accepted store bundle: its
///   opaque `bundle_id` (a stable index into the deterministic store scan)
///   and the bind table the kernel decoded from its signed manifest
///   ([`driver_store::encode_catalogue_reply`]). No bytes
///   and no `/System` path cross to the caller.
/// * [`StoreRequest::Load`] → resolve the named `bundle_id` to its path in
///   the pre-scanned `store` and the matched `node_id` to its resource
///   grants in the live tree, run the full signed load gate, and spawn
///   the driver into its own process with **only** those grants
///   ([`SpawnDriverLoader`]); the reply carries the
///   loaded driver's handle.
///
/// `store` is the one scan of the read-only `/System` store performed once
/// at serve start ([`serve_system_store`]); neither a catalogue nor a load
/// re-reads or re-verifies the whole store, so serving a request is O(1) in
/// the number of bundles, not a full re-scan. A load
/// reads only the single matched bundle's bytes to spawn it.
///
/// Every error — a malformed request, an out-of-range `bundle_id`, an
/// unknown `node_id`, a gate refusal, a reply that will not fit
/// [`DRIVER_STORE_MAX_REPLY`] — is encoded as an in-band error reply rather
/// than dropped. The reply is never silently
/// truncated.
#[must_use]
pub fn build_reply<F>(
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    store: &DriverStore,
    request: &[u8],
    audit: &dyn Sink,
) -> Vec<u8>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let mut buf = alloc::vec![0u8; DRIVER_STORE_MAX_REPLY as usize];
    let written = encode_reply_into(&mut buf, service, ctx, store, request, audit);
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
    store: &DriverStore,
    request: &[u8],
    audit: &dyn Sink,
) -> usize
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let result = match StoreRequest::decode(request) {
        Err(err) => driver_store::encode_error_reply(buf, err),
        Ok(StoreRequest::Catalogue) => catalogue_reply(buf, store),
        Ok(StoreRequest::Load { bundle_id, node_id }) => {
            load_reply(buf, service, ctx, store, bundle_id, node_id, audit)
        }
        Ok(StoreRequest::Unload { handle }) => unload_reply(buf, ctx, handle),
    };
    result.unwrap_or_else(|err| {
        // The chosen reply did not fit `DRIVER_STORE_MAX_REPLY`; report the
        // failure in band. A status-only frame always fits the buffer.
        driver_store::encode_error_reply(buf, err).unwrap_or(0)
    })
}

/// Frame the catalogue from the pre-scanned `store`: one `(bundle_id,
/// bind_keys)` entry per accepted bundle, where `bundle_id` is the bundle's
/// index in the deterministic scan order.
///
/// `store` is the single scan [`serve_system_store`] performed once over the
/// read-only `/System` store; framing the catalogue re-reads nothing. No bundle bytes and no `/System` path leave the
/// kernel — only the opaque id and the decoded keys the device manager
/// matches against.
fn catalogue_reply(buf: &mut [u8], store: &DriverStore) -> Result<usize, Errno> {
    let drivers = store.drivers();
    let mut entries: Vec<(u32, &[rustos_abi::DriverBindKey])> = Vec::with_capacity(drivers.len());
    for (index, driver) in drivers.iter().enumerate() {
        // `bundle_id` is the scan-order index; the scan is deterministic over
        // the static read-only store, so a subsequent `Load` resolves the
        // same id to the same bundle in the same cached scan.
        let bundle_id = u32::try_from(index).map_err(|_| Errno::LengthOutOfRange)?;
        entries.push((bundle_id, driver.bind_keys()));
    }
    driver_store::encode_catalogue_reply(buf, &entries)
}

/// Resolve `bundle_id` and `node_id`, run the signed load gate, spawn the
/// driver, and frame its handle.
///
/// `bundle_id` indexes the same deterministic scan a [`StoreRequest::Catalogue`]
/// exposed; `node_id` names the matched hardware-tree node whose resource
/// requests the loaded driver is granted — and nothing more. Every refusal
/// (unknown id, gate failure) is surfaced in band.
fn load_reply<F>(
    buf: &mut [u8],
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    store: &DriverStore,
    bundle_id: u32,
    node_id: u32,
    audit: &dyn Sink,
) -> Result<usize, Errno>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    match load_matched_driver(service, ctx, store, bundle_id, node_id, audit) {
        Ok(handle) => driver_store::encode_load_reply(buf, handle),
        Err(err) => driver_store::encode_error_reply(buf, err),
    }
}

/// Tear down the driver instance named by `handle` through the kernel
/// teardown seam ([`DriverProcessSpawn::terminate_driver`]) and frame the
/// status-only reply.
///
/// The symmetric partner of [`load_reply`]: the device manager unloads a
/// driver whose matched hardware-tree node has vanished. The endpoint's
/// `CAP_DRV_LOAD` send-capability requirement already gates the caller, so
/// this adds no further check — it drives the *mechanism* the device
/// manager's *policy* selected. Teardown is idempotent; a `handle` naming no
/// live driver surfaces [`Errno::NotFound`] in band rather than failing the
/// frame.
fn unload_reply(buf: &mut [u8], ctx: &StoreServeContext<'_>, handle: u64) -> Result<usize, Errno> {
    match ctx.spawn.terminate_driver(handle) {
        Ok(()) => driver_store::encode_unload_reply(buf),
        Err(err) => driver_store::encode_error_reply(buf, err),
    }
}

/// The load mechanism behind [`load_reply`]: index `bundle_id` into the
/// pre-scanned `store`, resolve `node_id`'s grants from the live tree, and
/// run [`SpawnDriverLoader`].
///
/// Returns the spawned driver's handle, or the fail-closed [`Errno`] of the
/// first failed step: an out-of-range `bundle_id`
/// ([`Errno::NotFound`]), an unknown `node_id` ([`Errno::NotFound`]), or the
/// signed-gate refusal the loader maps from `drvhost::HostError`. Only the
/// one matched bundle's bytes are read (by [`SpawnDriverLoader`]); the store
/// is not re-scanned.
fn load_matched_driver<F>(
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    store: &DriverStore,
    bundle_id: u32,
    node_id: u32,
    audit: &dyn Sink,
) -> Result<u64, Errno>
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    let index = usize::try_from(bundle_id).map_err(|_| Errno::NotFound)?;
    let driver = store.drivers().get(index).ok_or(Errno::NotFound)?;
    // The grants the loaded driver receives originate kernel-side, from the
    // **live** tree's matched node — never from the (untrusted) caller
    // (no ambient authority). Resolving against the live
    // inventory (not a boot snapshot) is what lets a node a user-space bus
    // driver published at runtime through `hw_emit_node` be loaded the moment
    // it appears. An unknown node fails closed.
    let resources = ctx
        .nodes
        .resolve_resources(node_id)
        .ok_or(Errno::NotFound)?;
    // Thread the matched node's id into the loader so the kernel records it
    // against the spawned driver: a child the driver later publishes through
    // `hw_emit_node` is parented under *this* node, and the driver cannot
    // forge its tree position.
    let mut loader =
        SpawnDriverLoader::new(ctx.trusted, service, audit, ctx.spawn, &[], Some(node_id));
    let handle = loader.load(driver.path(), &resources, &ctx.caps)?;
    Ok(handle.as_u64())
}

/// Run [`scan_store`] over the mounted `/System` store reachable through
/// `service`, returning the accepted-bundle view both the catalogue and the
/// load resolve against (one scan definition).
///
/// Called **once**, at [`serve_system_store`] start: the `/System` store is
/// read-only at runtime, so its accepted-bundle view is
/// immutable for the life of the system and is cached rather than rebuilt
/// per request — every bundle is read and bind-decoded exactly once, not
/// once per catalogue and once per load.
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
/// This never blocks: [`CallEndpoint::recv_call`] returns
/// immediately, and the per-arch kthread loop parks between calls. The wake
/// is co-located with the reply so a served caller is always re-readied
/// (a no-op before the boot path installs the wait-queue arch hook), and it
/// is **targeted**: the reply hands back the poster's scheduler id captured
/// at post time, so exactly that caller is unparked — never a broadcast
/// that would spuriously ready every other parked caller (wake-one, not a
/// thundering herd).
pub fn serve_pending<F>(
    service: &SystemFileService<'_, F>,
    ctx: &StoreServeContext<'_>,
    store: &DriverStore,
    endpoint: &CallEndpoint,
    audit: &dyn Sink,
) -> bool
where
    F: FilesystemRead + FilesystemSecurity + ?Sized,
{
    // The in-kernel server owns the request `Vec` directly, so it imposes no
    // buffer bound: `usize::MAX` never yields `TooLarge`, and an empty queue
    // means the kthread should park.
    let RecvCall::Received(call) = endpoint.recv_call(usize::MAX) else {
        return false;
    };
    let reply = build_reply(service, ctx, store, &call.request, audit);
    // A reply failure (oversize / unknown ticket) is itself fail-closed and
    // audited inside `CallEndpoint::reply`; the affected caller's own poll
    // then observes the unanswered ticket fail-closed. On success wake
    // exactly the poster; a poster with no scheduler identity (`0`) falls
    // back to the broadcast so it is still released.
    match endpoint.reply(call.ticket, &reply, audit) {
        Ok(0) => rustos_kernel_core::call_wake(),
        Ok(poster) => rustos_kernel_core::call_wake_task(poster),
        Err(_) => {}
    }
    true
}

/// Serve the read-only `/System` driver-store file-read IPC endpoint over
/// the already-mounted `volume` **for the life of the system** — the
/// never-returning body the disk-owning kthread runs in place of the bare
/// [`crate::shared_block::DriverStoreService::hold`] park (Design D D2b-2,
/// a-2).
///
/// It builds one [`SystemFileService`] over `volume` (the root-backed VFS
/// mounted once), binds the well-known driver-store
/// [`CallEndpoint`] under `binder` (which must hold
/// [`rustos_abi::CapabilityId::IPC_BIND_PRIVILEGED`] — see
/// [`crate::unlock_service::store_endpoint_binder_caps`]), registers it in
/// the kernel call-endpoint registry so the `ipc_call` syscall handler can
/// resolve it, then loops: drain one pending call ([`serve_pending`]) or
/// **park** off the run queue through `coop` when none is pending
/// (never a busy-yield). Every served caller is woken by
/// [`serve_pending`].
///
/// `volume`, the [`SystemFileService`] built over it, and the endpoint all
/// live on the calling kthread's frame for the whole serve loop; the
/// kthread's device bring-up chain stays suspended beneath this call, so the
/// borrowed device backing (DMA pool, MMIO map, IRQ waiter, virtio host)
/// stays live with no `'static` promotion (the
/// metal-proven device-driving model is unchanged).
///
/// `ctx` carries the kernel-side load mechanism (trust anchors, gate
/// capabilities, the architecture spawn seam) and the **live** hardware
/// inventory a matched `node_id` is resolved against, so a
/// [`StoreRequest::Load`] runs the signed gate and spawn on this
/// kthread's frame (the device manager owns
/// matching policy only). The read-only `/System` store is scanned **once**
/// here and cached for the life of the serve loop, so no request triggers a
/// re-scan.
///
/// # Errors
///
/// Returns a stable stage string fail-closed, **without**
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
    // Record this kthread's scheduler id on the endpoint so a posted
    // request wakes exactly this server instead of broadcasting to every
    // parked one (wake-one). The id is published by the admission seam
    // before the body first runs; a degenerate build without it leaves the
    // endpoint unrecorded and posts fall back to the broadcast wake.
    if let Some(task) = crate::unlock_service::store_service_task() {
        endpoint.record_server_task(task);
    }
    // The driver store is now reachable. Wake any reactive observer parked
    // on `hw_tree_wait` (the user-space `devmgr`) so it re-attempts its
    // catalogue fetch: the endpoint binds *after* the boot tree settles, so
    // without this nudge a manager that fetched the catalogue before the
    // bind would have fail-softed to empty and never retried. The node set is unchanged; the re-evaluation is idempotent.
    crate::hwtree_store::HW_TREE.bump();
    // Scan the read-only `/System` store exactly once: it cannot change at
    // runtime, so its accepted-bundle view is immutable
    // and is cached for the life of the serve loop. Every catalogue and every
    // load resolves its `bundle_id` against this one scan; a load reads only
    // the single matched bundle's bytes to spawn it, never the whole store
    // again (the per-request full re-scan this replaces
    // turned each driver load into an O(bundles) re-read).
    let store = scan_store_view(&service, audit);
    loop {
        // Drain every pending call first.
        if serve_pending(&service, ctx, &store, &endpoint, audit) {
            continue;
        }
        // Nothing pending: park off the run queue until a request is posted
        // (a real park, never a busy-yield). Register on
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
                if serve_pending(&service, ctx, &store, &endpoint, audit) {
                    rustos_kernel_core::SERVE_WAITQ.deregister(task);
                    continue;
                }
                coop.park();
                rustos_kernel_core::SERVE_WAITQ.deregister(task);
            }
            // The server's scheduler id was never published (a degenerate
            // build that did not go through `spawn_if_present`). Park bare
            // rather than busy-yield; the dispatch loop's wait-for-interrupt
            // re-step still re-runs the body.
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
        decode_catalogue_reply, decode_load_reply, decode_unload_reply, reply_status, StoreRequest,
        LOAD_REQUEST_LEN, UNLOAD_REQUEST_LEN,
    };
    use rustos_abi::hwtree::{HwResource, HW_NODE_ROOT};
    use rustos_abi::{
        CapabilityId, DriverBindKey, DriverKind, DriverManifest, HwDeviceClass, HwMatchKey, HwNode,
        ABI_VERSION_CURRENT, DRIVER_MANIFEST_MAGIC, DRIVER_MANIFEST_MAX_BIND_KEYS,
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
    /// glue does: the signature covers
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

    /// One recorded `spawn_driver`: the bundle bytes, the granted capability
    /// set, the matched node's resource grants, and the matched node id the
    /// load threaded for the kernel to record against the child.
    type RecordedSpawn = (Vec<u8>, CapabilitySet, Vec<HwResource>, Option<u32>);

    /// Records every `spawn_driver` (so a test can assert the gate forwarded
    /// exactly the matched node's grants) and every `terminate_driver`
    /// handle (so the unload-serving test can assert the server drove the
    /// teardown seam).
    struct RecordingSpawn {
        calls: RefCell<Vec<RecordedSpawn>>,
        terminations: RefCell<Vec<u64>>,
    }

    impl RecordingSpawn {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                terminations: RefCell::new(Vec::new()),
            }
        }
    }

    impl DriverProcessSpawn for RecordingSpawn {
        fn spawn_driver(
            &self,
            _path: &str,
            rxe: &[u8],
            granted: CapabilitySet,
            grants: &[HwResource],
            _args: &[&[u8]],
            node_id: Option<u32>,
        ) -> Result<u64, Errno> {
            self.calls
                .borrow_mut()
                .push((rxe.to_vec(), granted, grants.to_vec(), node_id));
            Ok(0x4242)
        }

        fn terminate_driver(&self, handle: u64) -> Result<(), Errno> {
            self.terminations.borrow_mut().push(handle);
            // Handle 0 stands in for an already-gone driver so the server's
            // fail-closed unload path is exercised too.
            if handle == 0 {
                Err(Errno::NotFound)
            } else {
                Ok(())
            }
        }
    }

    /// A spawn that must never be reached (the load fails before spawning).
    struct NoSpawn;
    impl DriverProcessSpawn for NoSpawn {
        fn spawn_driver(
            &self,
            _path: &str,
            _rxe: &[u8],
            _granted: CapabilitySet,
            _grants: &[HwResource],
            _args: &[&[u8]],
            _node_id: Option<u32>,
        ) -> Result<u64, Errno> {
            panic!("the load must fail closed before spawning");
        }

        fn terminate_driver(&self, _handle: u64) -> Result<(), Errno> {
            panic!("this test never unloads a driver");
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
    /// a matched driver receives.
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

    /// A [`HwNodeResolver`] backed by a fixed `&[HwNode]` slice, standing in
    /// for the global live [`crate::hwtree_store::HW_TREE`] so a test drives
    /// the load gate with an explicit node set.
    struct SliceNodes<'a>(&'a [HwNode]);
    impl HwNodeResolver for SliceNodes<'_> {
        fn resolve_resources(&self, node_id: u32) -> Option<Vec<HwResource>> {
            self.0
                .iter()
                .find(|node| !node.is_root() && node.id() == node_id)
                .map(|node| node.resources().to_vec())
        }
    }

    /// Scan `service` into the cached [`DriverStore`] the serve path builds
    /// once at startup, so a test resolves `bundle_id`s exactly as production
    /// (one scan, not one per request).
    fn store_of(service: &SystemFileService<'_, MockRootFs>) -> DriverStore {
        scan_store_view(service, &NullSink)
    }

    fn ctx<'a>(
        trusted: &'a [Ed25519PublicKey],
        spawn: &'a dyn DriverProcessSpawn,
        nodes: &'a dyn HwNodeResolver,
    ) -> StoreServeContext<'a> {
        StoreServeContext {
            trusted,
            caps: gate_caps(),
            spawn,
            nodes,
        }
    }

    fn catalogue(
        service: &SystemFileService<'_, MockRootFs>,
        ctx: &StoreServeContext<'_>,
    ) -> Vec<u8> {
        let store = store_of(service);
        let mut req = [0u8; 8];
        let n = StoreRequest::Catalogue.encode(&mut req).expect("encode");
        build_reply(service, ctx, &store, &req[..n], &NullSink)
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
        let nodes = SliceNodes(&[]);
        let reply = catalogue(&service, &ctx(&[], &spawn, &nodes));

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
        let nodes = SliceNodes(&[]);
        let reply = catalogue(&service, &ctx(&[], &spawn, &nodes));
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
        let nodes = SliceNodes(&tree);
        let serve_ctx = ctx(&trusted, &spawn, &nodes);
        let store = store_of(&service);

        // The single accepted bundle has scan id 0; load it for node 2.
        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id: 2,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
        // The reply carries the spawned driver's process id as its handle —
        // the unique, teardown-resolvable identity. It must be the recording
        // spawn's pid, not the driver host's per-instance counter (which is
        // `1` for every driver, since a fresh host is built per load, and
        // could neither be unloaded nor distinguished). Reporting the host
        // counter is the bug that left every driver at handle=1 and broke
        // hot-plug unload/reload.
        let handle = decode_load_reply(&reply).expect("the load succeeds and returns a handle");
        assert_eq!(
            handle, 0x4242,
            "the reported handle is the spawned PID, not the host's counter"
        );

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
        assert_eq!(
            calls[0].3,
            Some(2),
            "the matched node id is threaded so the kernel records it against the child (§18.3)"
        );
    }

    #[test]
    fn a_load_resolves_a_node_emitted_after_the_scan_against_the_live_tree() {
        // The chain-breaking regression: a user-space bus driver publishes a
        // child at runtime (`hw_emit_node`) *after* the store scan, and the
        // device manager loads a driver for it. Resolving the matched node's
        // grants against a frozen boot snapshot would fail closed here and
        // stall the recursive bus chain; resolving against the live tree
        // (`HwTreeStore`) loads it.
        use crate::hwtree_store::HwTreeStore;

        let key = HwMatchKey::virtio(0x1234);
        let keys = [DriverBindKey::new(5, key)];
        let payload = b"runtime-emitted-rxe";
        let sk = signing_key();
        let mut fs = MockRootFs::new();
        fs.add_file(
            "/System/Drivers/usb_kbd",
            &build_signed_bundle(&sk, &[CapabilityId::MMIO_MAP], &keys, payload),
        );
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let trusted = [pubkey_of(&sk)];
        let spawn = RecordingSpawn::new();
        let store = store_of(&service);

        // The live tree at scan time holds only the root — the device node
        // does not exist yet (a stale `ctx.tree` snapshot would freeze this).
        let live = HwTreeStore::new();
        live.seed(&[HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root)]);

        // A bus driver publishes the device at runtime; the kernel assigns
        // its id. Build the requested-resource node exactly as an emitter
        // would (id is kernel-owned, supplied as 0).
        let mut emitted = HwNode::new(0, 1, HwDeviceClass::Input);
        emitted.push_match_key(key).expect("key fits");
        emitted
            .push_resource(HwResource::mmio(0x0a00_0000, 0x200))
            .expect("mmio fits");
        emitted
            .push_resource(HwResource::dma(0x3fff_ffff, 0x1000))
            .expect("dma fits");
        let node_id = live.publish_child(1, emitted);

        let serve_ctx = ctx(&trusted, &spawn, &live);
        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
        let handle = decode_load_reply(&reply)
            .expect("a node emitted after the scan still resolves against the live tree");
        assert_ne!(handle, 0, "the runtime-emitted node loads");

        let calls = spawn.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].2,
            alloc::vec![
                HwResource::mmio(0x0a00_0000, 0x200),
                HwResource::dma(0x3fff_ffff, 0x1000)
            ],
            "the runtime-emitted node's grants are minted from the live tree, not a snapshot"
        );
        assert_eq!(
            calls[0].3,
            Some(node_id),
            "the live node id is threaded so a grandchild it emits is parented under it (§18.3)"
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
        let nodes = SliceNodes(&tree);
        let serve_ctx = ctx(&trusted, &spawn, &nodes);
        let store = store_of(&service);

        let req = StoreRequest::Load {
            bundle_id: 99,
            node_id: 2,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
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
        let nodes = SliceNodes(&tree);
        let serve_ctx = ctx(&trusted, &spawn, &nodes);
        let store = store_of(&service);

        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id: 0xDEAD,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
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
        let nodes = SliceNodes(&tree);
        let serve_ctx = ctx(&trusted, &spawn, &nodes);
        let store = store_of(&service);

        let req = StoreRequest::Load {
            bundle_id: 0,
            node_id: 2,
        };
        let mut rbuf = [0u8; LOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
        assert!(
            reply_status(&reply).is_err(),
            "an untrusted bundle never loads"
        );
    }

    #[test]
    fn an_unload_drives_the_teardown_seam_and_frames_ok() {
        // The device manager unloads a driver whose matched node vanished:
        // the server drives the kernel teardown seam with the handle and
        // frames a status-only success reply.
        let mut fs = service_with(&[]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let spawn = RecordingSpawn::new();
        let nodes = SliceNodes(&[]);
        let store = store_of(&service);
        let serve_ctx = ctx(&[], &spawn, &nodes);

        let req = StoreRequest::Unload { handle: 0x4242 };
        let mut rbuf = [0u8; UNLOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
        assert_eq!(decode_unload_reply(&reply), Ok(()));
        assert_eq!(
            spawn.terminations.borrow().as_slice(),
            &[0x4242],
            "the server drove the teardown seam with the requested handle"
        );
    }

    #[test]
    fn an_unload_of_an_already_gone_handle_is_in_band_not_found() {
        // Tearing down a handle naming no live driver is the benign,
        // idempotent miss the device manager may hit when it diffs the same
        // vanished node twice; the server surfaces it in band, never a panic.
        let mut fs = service_with(&[]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let spawn = RecordingSpawn::new();
        let nodes = SliceNodes(&[]);
        let store = store_of(&service);
        let serve_ctx = ctx(&[], &spawn, &nodes);

        // Handle 0 is the recording double's stand-in for an already-gone
        // driver (it returns `NotFound`).
        let req = StoreRequest::Unload { handle: 0 };
        let mut rbuf = [0u8; UNLOAD_REQUEST_LEN];
        let n = req.encode(&mut rbuf).expect("encode");
        let reply = build_reply(&service, &serve_ctx, &store, &rbuf[..n], &NullSink);
        assert_eq!(reply_status(&reply), Err(Errno::NotFound));
        assert_eq!(spawn.terminations.borrow().as_slice(), &[0]);
    }

    #[test]
    fn a_malformed_request_is_an_in_band_error_reply() {
        let mut fs = service_with(&[]);
        let service = SystemFileService::open(&mut fs, "/System/Drivers").expect("mount");
        let spawn = NoSpawn;
        let nodes = SliceNodes(&[]);
        let store = store_of(&service);
        let serve_ctx = ctx(&[], &spawn, &nodes);
        // Opcode 0xFF is unknown → OutOfRange (decode), surfaced in band.
        let reply = build_reply(&service, &serve_ctx, &store, &[0xFF], &NullSink);
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
    /// bind the restricted-sender driver-store endpoint. An `ipc_call` to the store then resolves nothing and fails
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
        let nodes = SliceNodes(&[]);
        let serve_ctx = ctx(&[], &spawn, &nodes);
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
