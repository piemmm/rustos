//! The freestanding driver-process serve loop of the `netchan-v1` device
//! channel (`plans/NETWORK.md` N4d).
//!
//! This is the I/O half of the contract's driver side: everything a NIC
//! driver process must do *around* an opened [`Net`] device to serve the
//! network stack, written once for every such driver rather than copied per
//! device. It claims a
//! reserved device-channel endpoint bound restricted-sender, publishes the
//! [`NETCHAN_NODE_COMPATIBLE`] node so `devmgr` hands the endpoint to the
//! stack, and then parks — never busy-polls — on a wait set over two
//! sources:
//!
//! * a **call** wake decodes one request and drives the pure
//!   [`NetChannelServer`]; `Attach` maps the granted frame region, `Service`
//!   drives one device doorbell over it, `Detach` unmaps it;
//! * an **interrupt** wake acknowledges the device (deasserting the line so
//!   it never storms) and, when a region is attached, wakes the stack with a
//!   single notify so it issues the next `Service`.
//!
//! Compiled only for the bare-metal targets a driver binary is built for.

use tairix_abi::driver::net::Net;
use tairix_abi::driver::net_channel::{
    is_net_channel_endpoint, AttachParams, NetChannelNotify, NetChannelRequest,
    NETCHAN_NODE_COMPATIBLE, NET_CHANNEL_ENDPOINT_BASE, NET_CHANNEL_MAX_REPLY,
    NET_CHANNEL_MAX_REQUEST,
};
use tairix_abi::hwtree::HW_NODE_ROOT;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, Errno, HwDeviceClass, HwMatchKey, HwNode, HwResource};
use tairix_caps::CapabilitySet;
use tairix_log::{log, Event, EventId, Level};
use tairix_rt::LogSink;

use crate::exit;
use crate::NetChannelServer;

/// Diagnostic event id: the one-shot "device channel published, serving"
/// beacon a NIC driver emits once its device is live and its endpoint is
/// bound.
const NETCHAN_READY: EventId = EventId(4180);

/// Wait-set token for a device-channel call doorbell on the claimed
/// endpoint.
const CALL_TOKEN: u64 = 1;

/// Wait-set token for "the device interrupt fired".
const IRQ_TOKEN: u64 = 2;

/// Outstanding-call capacity of the device-channel endpoint. The stack
/// issues one control request at a time (it blocks on the reply); a small
/// queue absorbs a doorbell racing the previous reply — a fail-closed
/// memory bound.
const ENDPOINT_CAPACITY: usize = 4;

/// Wait forever on the serve wait-set (a doorbell or an interrupt arrives
/// whenever there is work).
const WAIT_FOREVER_NS: u64 = u64::MAX;

/// One mapping of the shared frame region the stack granted in `Attach`.
struct Region {
    /// Base virtual address of the [`shm_map`](tairix_rt::shm_map)ping.
    base: u64,
    /// Full mapped byte length — page-rounded by the kernel, so possibly
    /// larger than the ring geometry — released verbatim by the matching
    /// `shm_unmap`.
    len: usize,
    /// The exclusive ring view: the first `geometry.region_len()` bytes of
    /// the mapping (a subset of `len`), which the `Service` doorbell binds
    /// the frame rings across.
    bytes: &'static mut [u8],
}

/// Serve the `netchan-v1` device channel over the opened device `net` for
/// the life of the driver process.
///
/// `irq_handle` is the bound device interrupt (from `irq_bind` on the line
/// the driver's matched node granted) the loop parks on alongside the call
/// endpoint. Never returns on the success path; every set-up refusal returns
/// a reserved [`exit`](crate::exit) code so the driver ends fail-closed with
/// a diagnosable reason rather than degrading into a busy re-poll.
pub fn serve<N: Net>(net: N, irq_handle: u64) -> i32 {
    let Some(endpoint) = claim_channel_endpoint() else {
        return exit::NO_SERVICE;
    };
    if emit_netchan_node(endpoint).is_none() {
        return exit::NO_SERVICE;
    }

    let set = tairix_rt::waitset_create();
    if set < 0 {
        return exit::NO_SERVICE;
    }
    #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
    let set = set as u64;
    if tairix_rt::waitset_ctl(
        set,
        WaitSetOp::Add,
        WaitSourceKind::Endpoint,
        endpoint,
        CALL_TOKEN,
    ) != 0
        || tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Irq,
            irq_handle,
            IRQ_TOKEN,
        ) != 0
    {
        return exit::NO_SERVICE;
    }

    log(
        &LogSink,
        &Event {
            level: Level::Info,
            id: NETCHAN_READY,
            message: "netchan: device channel published, serving",
            fields: &[],
        },
    );

    serve_loop(NetChannelServer::new(net), set, endpoint)
}

/// Claim the first free id in the reserved device-channel endpoint block
/// and bind it **restricted-sender requiring `CAP_NET_RAW`**: the kernel
/// admits a caller only if it holds that capability, so only the network
/// stack can post to this driver (defence in depth atop the
/// `CAP_IPC_BIND_PRIVILEGED` gate the reserved-id bind already demands).
/// `recv_caps` is empty — endpoint ownership already restricts receive to
/// this task. Returns the claimed id, or [`None`] if the whole block was
/// already taken (every id squatted on — fail closed).
fn claim_channel_endpoint() -> Option<u64> {
    let mut send_caps = CapabilitySet::empty();
    send_caps.insert(CapabilityId::NET_RAW);
    let recv_caps = CapabilitySet::empty();
    let mut id = NET_CHANNEL_ENDPOINT_BASE;
    while is_net_channel_endpoint(id) {
        let bound = tairix_rt::call_create(
            id,
            &send_caps,
            &recv_caps,
            NET_CHANNEL_MAX_REQUEST,
            NET_CHANNEL_MAX_REPLY,
            ENDPOINT_CAPACITY,
        );
        if bound == 0 {
            return Some(id);
        }
        id += 1;
    }
    None
}

/// Publish the `netchan` hardware-tree node carrying the claimed
/// device-channel endpoint as a grant request, so `devmgr` observes it
/// (a hardware-tree generation bump) and hands the endpoint to the network
/// stack over the capability-gated admin surface. Returns the
/// kernel-assigned node id, or [`None`] on any refusal.
///
/// The node names [`HW_NODE_ROOT`] as its parent; the kernel re-parents it
/// under the *discovered node this driver was loaded for*, which is what
/// lets `devmgr` recover the NIC's stable bus location from the published
/// channel.
fn emit_netchan_node(endpoint: u64) -> Option<u32> {
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Network);
    let key = HwMatchKey::compatible(NETCHAN_NODE_COMPATIBLE).ok()?;
    node.push_match_key(key).ok()?;
    node.push_resource(HwResource::endpoint(endpoint)).ok()?;
    let emit = tairix_rt::hw_emit_node(&node);
    if emit < 0 {
        return None;
    }
    // `emit >= 0` is the kernel-assigned node id.
    u32::try_from(emit).ok()
}

/// Park on the wait set and serve device-channel doorbells and device
/// interrupts for the life of the driver. Never returns on the success
/// path; a wait-set fault exits fail-closed.
fn serve_loop<N: Net>(mut server: NetChannelServer<N>, set: u64, endpoint: u64) -> i32 {
    let mut region: Option<Region> = None;
    let mut request = [0u8; NET_CHANNEL_MAX_REQUEST];
    loop {
        let mut token = 0u64;
        let woke = tairix_rt::waitset_wait(set, WAIT_FOREVER_NS, &mut token);
        if woke < 0 {
            return exit::NO_SERVICE;
        }
        if woke != 0 {
            // A spurious/lapsed wake with no ready source; re-park.
            continue;
        }
        match token {
            IRQ_TOKEN => on_interrupt(&mut server),
            CALL_TOKEN => serve_call(&mut server, endpoint, &mut request, &mut region),
            _ => {}
        }
    }
}

/// Acknowledge the device interrupt (deasserting the line so it never
/// storms while the stack has not yet serviced) and, when a frame region
/// is attached, wake the stack with a single receive-frames notify. The
/// driver never services the rings here — it only rings the stack's
/// doorbell; the stack owns the region and issues the next `Service`.
fn on_interrupt<N: Net>(server: &mut NetChannelServer<N>) {
    server.net_mut().ack_interrupt();
    if let Some(notify_endpoint) = server.notify_endpoint() {
        let _ = tairix_rt::ipc_send(notify_endpoint, &NetChannelNotify::encode());
    }
}

/// Serve one device-channel doorbell on the claimed endpoint: receive the
/// request, drive the pure [`NetChannelServer`], and reply. A transient
/// recv error simply drops the doorbell (the stack retries); a decode
/// failure is answered with the typed error so the stack sees the exact
/// refusal.
fn serve_call<N: Net>(
    server: &mut NetChannelServer<N>,
    endpoint: u64,
    request: &mut [u8; NET_CHANNEL_MAX_REQUEST],
    region: &mut Option<Region>,
) {
    let mut ticket = 0u64;
    let Ok(request_len) = tairix_rt::call_recv(endpoint, request, &mut ticket) else {
        return;
    };
    match NetChannelRequest::decode(&request[..request_len]) {
        Ok(NetChannelRequest::Facts) => {
            let reply = server.facts_reply();
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(NetChannelRequest::Attach(params)) => {
            let status = attach(server, params, region);
            let _ = tairix_rt::call_reply(endpoint, ticket, &status);
        }
        Ok(NetChannelRequest::Service) => {
            let reply = match region.as_mut() {
                Some(region) => server.service_reply(region.bytes),
                // Detached (or region lost): the server answers
                // `NotConnected` before it ever touches the slice.
                None => server.service_reply(&mut []),
            };
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(NetChannelRequest::Detach) => {
            let reply = server.detach();
            if let Some(region) = region.take() {
                let _ = tairix_rt::shm_unmap(region.base, region.len);
            }
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Err(err) => {
            let _ = tairix_rt::call_reply(endpoint, ticket, &encode_status_reply(Err(err)));
        }
    }
}

/// Map the frame region the stack granted, validate its length against the
/// agreed geometry, and attach the pure server. On any refusal the region
/// is unmapped and no attach state is kept (fail closed — a rejected
/// attach never half-binds).
fn attach<N: Net>(
    server: &mut NetChannelServer<N>,
    params: AttachParams,
    region: &mut Option<Region>,
) -> [u8; STATUS_REPLY_LEN] {
    // A re-attach without a prior detach releases the old mapping first.
    if let Some(previous) = region.take() {
        let _ = tairix_rt::shm_unmap(previous.base, previous.len);
    }
    let mut len_out = 0u64;
    let mapped = tairix_rt::shm_map(params.region_grant, &mut len_out);
    if mapped < 0 {
        return encode_status_reply(Err(Errno::from_syscall(mapped)));
    }
    // A non-negative result is the base virtual address of the mapping;
    // `len_out` is the kernel's own record of the mapped byte length.
    // The kernel maps whole pages, so a region whose agreed geometry is
    // not a page multiple is mapped rounded *up* — `map_len` is that
    // actual mapped length (used for the exact `shm_unmap`), which must
    // be at least the geometry needs.
    let (Ok(base), Ok(addr), Ok(map_len)) = (
        u64::try_from(mapped),
        usize::try_from(mapped),
        usize::try_from(len_out),
    ) else {
        return encode_status_reply(Err(Errno::DeviceFault));
    };
    let expected = params.geometry.region_len();
    if map_len < expected {
        let _ = tairix_rt::shm_unmap(base, map_len);
        return encode_status_reply(Err(Errno::BufferTooSmall));
    }
    // SAFETY: `shm_map` mapped `map_len` bytes (>= `expected`, verified
    // above) of zeroed, cacheable, RW (non-executable) memory into this
    // process at `addr`. The ring view binds only the first `expected`
    // bytes — exactly the agreed geometry — so the exclusive `&mut [u8]`
    // over `expected` bytes is a sound subset of the mapping (any
    // page-rounding tail beyond it is left untouched). The region is
    // owned by this process until the matching `shm_unmap` (on detach, a
    // re-attach, or a rejected attach below) releases the full `map_len`,
    // and nothing else in this address space aliases it. The stack maps
    // the same frames through its own grant and never touches ring bytes
    // across a `Service` doorbell.
    let bytes = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, expected) };
    let status = server.attach(params);
    if server.is_attached() {
        *region = Some(Region {
            base,
            len: map_len,
            bytes,
        });
    } else {
        // The server refused (geometry too small for the device); drop the
        // mapping it will never use.
        let _ = tairix_rt::shm_unmap(base, map_len);
    }
    status
}
