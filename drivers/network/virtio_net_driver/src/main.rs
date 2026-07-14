//! The `Run` entry-point binary of the virtio-net driver, installed as a
//! signed `/System/Drivers/` bundle and **autoloaded into user space** by
//! `devmgr` when a virtio-net device is discovered (`plans/NETWORK.md` N4d,
//! `AGENTS.md` §4 / §18).
//!
//! This is the "drivers in user space" steady state for networking: the
//! process owns the NIC (its register window, DMA, and interrupt line) and
//! serves the `netchan-v1` device-channel contract to the network stack
//! (`userland/net/netstack`), which runs in its own address space and owns
//! the shared frame-ring region. The two never link each other — the driver
//! is the *server* of a claimed reserved endpoint and the stack is the
//! *client* — so any NIC driver serves any stack build (`AGENTS.md` §17.4).
//!
//! It is a **pure-Rust** program: RustOS is Rust-only, so it links the Rust
//! userland runtime `rustos-rt`, never the C ABI (which exists solely for
//! non-Rust programs). `rustos-rt` provides `_start`, the per-process stack
//! canary, the panic handler, the allocator, and the syscall wrappers;
//! `rustos_rt::entry!` names this program's `main`. It links no `drivers/*`
//! crate — the virtio-MMIO transport and the virtio-net device engine are
//! `lib/*` crates — so the layering holds and the kernel never pulls the
//! userland runtime into its graph.
//!
//! # What `main` does
//!
//! 1. Builds the rt-backed `RtDriverHost` from the grants the kernel minted
//!    for this driver's matched node (a register window, a DMA constraint, and
//!    the device interrupt line — and no more), and maps the single register
//!    window the node named (never a build-time board constant).
//! 2. Brings the virtio-net device online over the bus-agnostic virtio-MMIO
//!    transport (`VirtioNet::open`) and binds the granted interrupt line.
//! 3. Claims the first free id in the reserved device-channel endpoint block
//!    and binds it **restricted-sender requiring `CAP_NET_RAW`**, so the
//!    kernel refuses every caller but the stack at dispatch (defence in depth
//!    atop the reserved-bind `CAP_IPC_BIND_PRIVILEGED` gate).
//! 4. Publishes a `netchan` hardware-tree node carrying the claimed endpoint
//!    as an `HwResource::endpoint` grant request, so `devmgr` observes it
//!    and hands the stack the endpoint over the admin surface.
//! 5. Parks on a wait set over **two** sources — the device-channel call
//!    endpoint (the stack's `Facts`/`Attach`/`Service`/`Detach` doorbells) and
//!    the device interrupt — and never busy-polls (`AGENTS.md` §2.23):
//!    * a call wake decodes one `NetChannelRequest` and drives the pure
//!      `NetChannelServer`; `Attach` maps the granted frame region, `Service`
//!      drives one device doorbell over that region, `Detach` unmaps it;
//!    * an interrupt wake acknowledges the device (deasserting the line so it
//!      never storms) and, when a region is attached, wakes the stack with a
//!      single `NetChannelNotify` `ipc_send` so it issues the next `Service`.
//!
//! A bring-up failure exits with a reserved fail-closed code, leaving the
//! system without this NIC rather than wedged; the spawning supervisor decides
//! whether to relaunch. On the host it is an inert stub so `cargo build
//! --workspace`, clippy, and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::driver::net::Net;
    use rustos_abi::driver::net_channel::{
        is_net_channel_endpoint, NetChannelNotify, NetChannelRequest, NET_CHANNEL_ENDPOINT_BASE,
        NET_CHANNEL_MAX_REPLY, NET_CHANNEL_MAX_REQUEST,
    };
    use rustos_abi::driver::sole_register_window;
    use rustos_abi::driver::virtio::VirtioHost;
    use rustos_abi::hwtree::HW_NODE_ROOT;
    use rustos_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
    use rustos_abi::waitset::{WaitSetOp, WaitSourceKind};
    use rustos_abi::{
        CapabilityId, Errno, HwDeviceClass, HwMatchKey, HwNode, HwResource, MmioMapper,
    };
    use rustos_caps::CapabilitySet;
    use rustos_drvrt::{RtDriverHost, RtGrantSyscalls};
    use rustos_log::{log, Event, EventId, Level};
    use rustos_rt::LogSink;
    use rustos_virtio::MmioTransport;
    use rustos_virtio_net::{NetChannelServer, VirtioNet};

    /// Exit code when the rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused or the
    /// delivery did not fit). A reserved, fail-closed value.
    const EXIT_NO_HOST: i32 = 80;

    /// Exit code when the delivered grants do not name the single register
    /// window (and interrupt line) this driver needs — an unbound or
    /// mis-provisioned node. A reserved, fail-closed value.
    const EXIT_NO_RESOURCES: i32 = 81;

    /// Exit code when the device bring-up failed (the register window could
    /// not be mapped, the window is not a virtio-MMIO device, the device
    /// rejected the virtio init sequence, or the granted interrupt line could
    /// not be bound — the serve loop parks on it, so a driver that cannot bind
    /// it would degrade into the busy re-poll the charter forbids). A
    /// reserved, fail-closed value.
    const EXIT_BRINGUP_FAILED: i32 = 82;

    /// Exit code when the device channel could not be stood up (no free
    /// reserved endpoint id, the bind was refused, the `netchan` node could
    /// not be published, or the wait-set could not be built). A reserved
    /// value.
    const EXIT_NO_SERVICE: i32 = 83;

    /// Diagnostic event id: the one-shot "device channel published, serving"
    /// beacon.
    const NET_DRV_READY: EventId = EventId(4180);

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

    /// The capability set the driver host re-checks up front before issuing a
    /// `mmio_map` / `dma_alloc` / `irq_bind` trap, so a missing grant fails
    /// fast without a round trip. It mirrors the resources the matched node
    /// requested — the register window (`CAP_MMIO_MAP`), the DMA region
    /// (`CAP_MEM_DMA`), and the device interrupt line the serve loop parks on
    /// (`CAP_IRQ_BIND`) — plus the authority to claim and bind the reserved
    /// device-channel endpoint (`CAP_IPC_ENDPOINT`, `CAP_IPC_BIND_PRIVILEGED`)
    /// and publish the `netchan` node (`CAP_HW_EMIT`). The kernel is the
    /// authority and re-checks every trap regardless.
    fn driver_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::MMIO_MAP);
        caps.insert(CapabilityId::MEM_DMA);
        caps.insert(CapabilityId::IRQ_BIND);
        caps.insert(CapabilityId::SHM);
        caps.insert(CapabilityId::IPC_ENDPOINT);
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
        caps.insert(CapabilityId::HW_EMIT);
        caps.insert(CapabilityId::LOG_EMIT);
        caps
    }

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`); an unrecognised code fails closed as [`Errno::DeviceFault`]
    /// rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::DeviceFault)
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
            let bound = rustos_rt::call_create(
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
    fn emit_netchan_node(endpoint: u64) -> Option<u32> {
        let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Network);
        let key = HwMatchKey::compatible(b"rustos,netchan").ok()?;
        node.push_match_key(key).ok()?;
        node.push_resource(HwResource::endpoint(endpoint)).ok()?;
        let emit = rustos_rt::hw_emit_node(&node);
        if emit < 0 {
            return None;
        }
        // `emit >= 0` is the kernel-assigned node id.
        u32::try_from(emit).ok()
    }

    /// This driver's mapping of one shared frame region granted by the stack
    /// in `Attach`.
    struct Region {
        /// Base virtual address of the [`shm_map`](rustos_rt::shm_map)ping.
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

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// On success this never returns: the device-channel serve loop runs for
    /// the life of the driver process.
    fn main() -> i32 {
        // The QEMU `virt` virtio interconnect snoops the CPU caches, so the
        // DMA carve is coherent kernel-side and no cache-maintenance shim is
        // supplied here (`coherency = None`, keeping the program neutral).
        let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
            return EXIT_NO_HOST;
        };
        let Ok((base, len)) = sole_register_window(host.resources()) else {
            return EXIT_NO_RESOURCES;
        };
        let Some(irq_line) = host.irq_line() else {
            return EXIT_NO_RESOURCES;
        };
        let Ok(window) = host.map_window(base, len) else {
            return EXIT_BRINGUP_FAILED;
        };
        let Ok(transport) = MmioTransport::new(window) else {
            return EXIT_BRINGUP_FAILED;
        };
        let vhost: &dyn VirtioHost = &host;
        let Ok(net) = VirtioNet::open(transport, vhost) else {
            return EXIT_BRINGUP_FAILED;
        };

        // Bind the device interrupt line the serve loop parks on. This is the
        // audited readiness witness, issued once the device is live: a driver
        // that cannot bind it must exit rather than degrade into a busy
        // re-poll.
        let irq_ret = rustos_rt::irq_bind(irq_line);
        if irq_ret <= 0 {
            return EXIT_BRINGUP_FAILED;
        }
        #[allow(clippy::cast_sign_loss)] // `irq_ret > 0` is the minted IrqHandle.
        let irq_handle = irq_ret as u64;

        let Some(endpoint) = claim_channel_endpoint() else {
            return EXIT_NO_SERVICE;
        };
        if emit_netchan_node(endpoint).is_none() {
            return EXIT_NO_SERVICE;
        }

        let set = rustos_rt::waitset_create();
        if set < 0 {
            return EXIT_NO_SERVICE;
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
        let set = set as u64;
        if rustos_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            endpoint,
            CALL_TOKEN,
        ) != 0
            || rustos_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::Irq,
                irq_handle,
                IRQ_TOKEN,
            ) != 0
        {
            return EXIT_NO_SERVICE;
        }

        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: NET_DRV_READY,
                message: "virtio-net: device channel published, serving",
                fields: &[],
            },
        );

        serve_loop(NetChannelServer::new(net), set, endpoint)
    }

    /// Park on the wait set and serve device-channel doorbells and device
    /// interrupts for the life of the driver. Never returns on the success
    /// path; a wait-set fault exits fail-closed.
    fn serve_loop<N: Net>(mut server: NetChannelServer<N>, set: u64, endpoint: u64) -> i32 {
        let mut region: Option<Region> = None;
        let mut request = [0u8; NET_CHANNEL_MAX_REQUEST];
        loop {
            let mut token = 0u64;
            let woke = rustos_rt::waitset_wait(set, WAIT_FOREVER_NS, &mut token);
            if woke < 0 {
                return EXIT_NO_SERVICE;
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
            let _ = rustos_rt::ipc_send(notify_endpoint, &NetChannelNotify::encode());
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
        let Ok(request_len) = rustos_rt::call_recv(endpoint, request, &mut ticket) else {
            return;
        };
        match NetChannelRequest::decode(&request[..request_len]) {
            Ok(NetChannelRequest::Facts) => {
                let reply = server.facts_reply();
                let _ = rustos_rt::call_reply(endpoint, ticket, &reply);
            }
            Ok(NetChannelRequest::Attach(params)) => {
                let status = attach(server, params, region);
                let _ = rustos_rt::call_reply(endpoint, ticket, &status);
            }
            Ok(NetChannelRequest::Service) => {
                let reply = match region.as_mut() {
                    Some(region) => server.service_reply(region.bytes),
                    // Detached (or region lost): the server answers
                    // `NotConnected` before it ever touches the slice.
                    None => server.service_reply(&mut []),
                };
                let _ = rustos_rt::call_reply(endpoint, ticket, &reply);
            }
            Ok(NetChannelRequest::Detach) => {
                let reply = server.detach();
                if let Some(region) = region.take() {
                    let _ = rustos_rt::shm_unmap(region.base, region.len);
                }
                let _ = rustos_rt::call_reply(endpoint, ticket, &reply);
            }
            Err(err) => {
                let _ = rustos_rt::call_reply(endpoint, ticket, &encode_status_reply(Err(err)));
            }
        }
    }

    /// Map the frame region the stack granted, validate its length against the
    /// agreed geometry, and attach the pure server. On any refusal the region
    /// is unmapped and no attach state is kept (fail closed — a rejected
    /// attach never half-binds).
    fn attach<N: Net>(
        server: &mut NetChannelServer<N>,
        params: rustos_abi::driver::net_channel::AttachParams,
        region: &mut Option<Region>,
    ) -> [u8; STATUS_REPLY_LEN] {
        // A re-attach without a prior detach releases the old mapping first.
        if let Some(previous) = region.take() {
            let _ = rustos_rt::shm_unmap(previous.base, previous.len);
        }
        let mut len_out = 0u64;
        let mapped = rustos_rt::shm_map(params.region_grant, &mut len_out);
        if mapped < 0 {
            return encode_status_reply(Err(errno_from(mapped)));
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
            let _ = rustos_rt::shm_unmap(base, map_len);
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
            let _ = rustos_rt::shm_unmap(base, map_len);
        }
        status
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
