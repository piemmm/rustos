//! The `Run` entry-point binary of the network-stack service, installed
//! at `/System/Services/netstack.app/Run` — the long-running user-space
//! service PID 1 `init` launches to own the network interfaces and serve
//! the `netstack-v1` IPC surface (`plans/NETWORK.md` §2.2).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the
//! Rust userland runtime `tairix-rt` — never the C ABI, which exists
//! solely for programs *not* written in Rust. `tairix-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `#[global_allocator]`, and the syscall wrappers; `tairix_rt::entry!`
//! names this program's `main`.
//!
//! # What this service does
//!
//! At startup it binds the well-known
//! [`tairix_abi::net_ipc::NETSTACK_ENDPOINT`] (an unrestricted-sender
//! call endpoint — any process may post, but the id is a reserved
//! rendezvous, so binding it needs the manifest's
//! `CAP_IPC_BIND_PRIVILEGED`: a squatter could otherwise receive
//! interface mutations and serve forged network state) and then parks on
//! a wait set, woken by requests and by the engines' one-shot deadlines
//! — never a polling loop. Each request is served by the
//! capability-checked [`tairix_netstack::serve`] dispatcher against the
//! caller's kernel-attested origin.
//!
//! PID 1 `init` launches this service at boot (its `DEFAULT_CONFIG`,
//! after `sysinfod` and before `devmgr`). NIC frame-ring channels join
//! this wait set as the device manager binds network drivers to the
//! service through the `BindDriver` admin op (`plans/NETWORK.md` N4d);
//! until then the interface table is empty, the deadline is unarmed, and
//! the loop parks solely on the endpoints.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the
// optional `tairix-rt` runtime through the default `program` feature. The
// kernel and host tooling build only this crate's *library*, so this module
// (and `tairix-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
extern crate alloc;

#[cfg(all(freestanding, feature = "program"))]
mod program {
    use alloc::vec::Vec;

    use tairix_abi::driver::net::LinkState;
    use tairix_abi::driver::net_channel::{
        notify_endpoint_for, NET_CHANNEL_ENDPOINT_COUNT, NET_CHANNEL_NOTIFY_LEN,
    };
    use tairix_abi::driver::net_ring::RingGeometry;
    use tairix_abi::driver::BufferClass;
    use tairix_abi::net::{SocketRequest, NETSTACK_SOCKET_ENDPOINT, SOCKET_MAX_REPLY};
    use tairix_abi::net_ipc::{
        NetBondConfigMsg, NetIfKind, NetInterfaceConfigMsg, NetstackRequest, IF_NAME_LEN,
        NETSTACK_ENDPOINT, NETSTACK_MAX_REPLY, NETSTACK_MAX_REQUEST,
    };
    use tairix_abi::reply::encode_status_reply;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{CapabilityId, Duration64, Errno, Origin, RandomFlags, ORIGIN_WIRE_LEN};
    use tairix_caps::CapabilitySet;
    use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
    use tairix_net::iface::eui64_interface_id;
    use tairix_net::stack::StackEvent;
    use tairix_netstack::{
        events, queue_tx, serve, Caller, CryptoCookieSecret, Delivery, FrameBatch,
        NetChannelClient, NetChannelTransport, Netstack, SocketService, StreamIo,
    };
    use tairix_rt::LogSink;

    /// Outstanding-call capacity of the endpoint (a fail-closed memory
    /// bound).
    const CAPACITY: usize = 8;

    /// Wait-set member token of the admin request endpoint.
    const ENDPOINT_TOKEN: u64 = 1;

    /// Wait-set member token of the socket (data-plane control) endpoint.
    const SOCKET_TOKEN: u64 = 2;

    /// First wait-set token of a bound NIC channel's notify port. Channel
    /// slot `i` (`0..MAX_CHANNELS`) owns `CHANNEL_TOKEN_BASE + i`, so a
    /// wake's token names its slot directly.
    const CHANNEL_TOKEN_BASE: u64 = 3;

    /// Most NIC device channels the stack serves at once — the reserved
    /// device-channel endpoint block's width (a fixed shared-resource bound,
    /// not a scaling capacity: it sizes the notify-port id space and the
    /// slot table, both bounded by the endpoint block itself).
    const MAX_CHANNELS: usize = NET_CHANNEL_ENDPOINT_COUNT as usize;

    /// Slots the notify port queues: the driver rings a single coalescing
    /// doorbell, so a tiny queue absorbs one racing the previous drain — a
    /// fail-closed memory bound.
    const NOTIFY_CAPACITY: usize = 4;

    /// Slots per frame ring. A fixed layout parameter of the shared
    /// device-channel region (agreed with the driver through the geometry),
    /// not a per-machine scaling capacity: it bounds the pinned frame region
    /// and the per-doorbell work, and the engine's own retransmission
    /// recovers a frame dropped on a momentarily-full ring.
    const NET_RING_SLOTS: u32 = 16;

    /// Sensitivity class of the frame rings. Link-layer frames are not
    /// treated as secrets (confidentiality is an upper-layer concern, e.g.
    /// TLS), matching every other frame-ring consumer; the shared region is
    /// still kernel-zeroed on map and on free regardless.
    const FRAME_CLASS: BufferClass = BufferClass::NonSensitive;

    /// The stack side of one bound NIC driver's `netchan-v1` device channel:
    /// the managed interface alias, the notify port the driver rings on
    /// receive (drained on each wake), and the channel client that owns the
    /// stack's mapping of the shared frame region.
    struct Channel {
        /// The managed interface's admin-chosen alias.
        iface: [u8; IF_NAME_LEN],
        /// The stack-owned notify mailbox the driver `ipc_send`s a wake to.
        notify: u64,
        /// The channel client (owns the `'static` frame-region mapping and
        /// the `ipc_call` doorbell transport).
        client: NetChannelClient<'static, RtNetChannelTransport>,
    }

    /// The channel client's doorbell transport: one `ipc_call` to the NIC
    /// driver process's device endpoint. The kernel gates the endpoint
    /// restricted-sender on `CAP_NET_RAW`, so only this stack may post.
    struct RtNetChannelTransport {
        /// The NIC driver's claimed device-channel endpoint id.
        endpoint: u64,
    }

    impl NetChannelTransport for RtNetChannelTransport {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(self.endpoint, request, reply).map_err(errno_from)
        }
    }

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// The monotonic clock as the engine's `now`.
    fn now() -> Duration64 {
        let nanos = tairix_rt::clock_get();
        // Both components are in range by construction: secs fits i64
        // (u64 ns / 1e9 < i64::MAX) and the remainder is < 1e9.
        Duration64::new(
            (nanos / 1_000_000_000) as i64,
            (nanos % 1_000_000_000) as u32,
        )
        .unwrap_or_default()
    }

    /// Nanoseconds from `now` until the engines' earliest deadline —
    /// folding the per-interface deadlines and every connected stream's
    /// TCP timer — or `u64::MAX` (park indefinitely) when nothing is armed.
    fn timeout_ns(stack: &Netstack, sockets: &SocketService) -> u64 {
        let deadline = match (stack.next_deadline(), sockets.stream_next_deadline()) {
            (Some(a), Some(b)) => {
                if (a.secs(), a.subsec_nanos()) <= (b.secs(), b.subsec_nanos()) {
                    a
                } else {
                    b
                }
            }
            (Some(d), None) | (None, Some(d)) => d,
            (None, None) => return u64::MAX,
        };
        let current = now();
        let deadline_ns =
            deadline.secs().max(0) as u64 * 1_000_000_000 + u64::from(deadline.subsec_nanos());
        let now_ns =
            current.secs().max(0) as u64 * 1_000_000_000 + u64::from(current.subsec_nanos());
        deadline_ns.saturating_sub(now_ns).max(1)
    }

    /// Bind the endpoint and serve requests for the life of the service.
    ///
    /// The endpoint is unrestricted-sender (empty `send_caps`), so any
    /// process may post — per-operation gating is enforced by the
    /// dispatcher against each caller's attested origin, not by the
    /// transport. `recv_caps` is empty: endpoint ownership already
    /// restricts receive to this task.
    fn main() -> i32 {
        let empty = CapabilitySet::empty();
        let bound = tairix_rt::call_create(
            NETSTACK_ENDPOINT,
            &empty,
            &empty,
            NETSTACK_MAX_REQUEST,
            NETSTACK_MAX_REPLY,
            CAPACITY,
        );
        if bound != 0 {
            // Could not publish the endpoint (already bound, or no
            // registry): fail closed; PID 1 supervises and relaunches.
            return 1;
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return 1;
        }
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            NETSTACK_ENDPOINT,
            ENDPOINT_TOKEN,
        ) != 0
        {
            return 1;
        }

        // The socket (data-plane control) endpoint: a second reserved
        // rendezvous, unrestricted-sender like the admin one — the socket
        // dispatcher gates every call on `CAP_NET` against the caller's
        // attested origin.
        if tairix_rt::call_create(
            NETSTACK_SOCKET_ENDPOINT,
            &empty,
            &empty,
            SocketRequest::MAX_WIRE_LEN,
            SOCKET_MAX_REPLY,
            CAPACITY,
        ) != 0
        {
            return 1;
        }
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            NETSTACK_SOCKET_ENDPOINT,
            SOCKET_TOKEN,
        ) != 0
        {
            return 1;
        }

        // This task's own never-reused id, used to name its per-channel
        // notify ports (the `notify_endpoint_for` naming rule). Without it
        // the notify-port id space cannot be formed, so fail closed.
        let Ok(origin) = tairix_rt::self_origin() else {
            return 1;
        };
        let pid = origin.pid();

        // Draw the per-boot SYN-cookie key from the platform CSPRNG (a
        // blocking draw — the key is long-lived and must be unpredictable).
        // It is never persisted and is dropped at shutdown, so paged-out or
        // captured cookies cannot be forged after a reboot.
        let mut cookie_key = [0u8; 32];
        let _ = tairix_rt::random_get(&mut cookie_key, RandomFlags::empty());
        let secret = CryptoCookieSecret::new(cookie_key);

        let mut stack = Netstack::new();
        let mut sockets = SocketService::new();
        // The bound NIC channels, one per slot in the reserved endpoint
        // block. A fixed table (not a growable capacity): the channel count
        // is bounded by the endpoint block itself.
        let mut channels: [Option<Channel>; MAX_CHANNELS] = core::array::from_fn(|_| None);
        let mut request = [0u8; NETSTACK_MAX_REQUEST];
        let mut socket_request = [0u8; SocketRequest::MAX_WIRE_LEN];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        let mut reply = [0u8; NETSTACK_MAX_REPLY];
        let mut socket_reply = [0u8; SOCKET_MAX_REPLY];
        loop {
            // Park until a request arrives, a driver rings a notify port, or
            // the engines' one-shot deadline lapses; a lapsed deadline
            // (a non-zero wake) pumps every channel so the engines emit
            // their timer-due frames (DAD, SLAAC RS, IGMP, retransmits),
            // then re-arms below against the new `now`.
            let mut token = 0u64;
            let woke = tairix_rt::waitset_wait(set, timeout_ns(&stack, &sockets), &mut token);
            if woke != 0 {
                pump_all(&mut stack, &mut sockets, &mut channels, &secret, now());
                continue;
            }
            match token {
                ENDPOINT_TOKEN => {
                    serve_admin(
                        &mut stack,
                        &sockets,
                        &mut channels,
                        pid,
                        set,
                        &mut request,
                        &mut origin_buf,
                        &mut reply,
                    );
                    // An admin mutation (a bind, a per-interface or bond
                    // configuration) can queue engine output that must reach
                    // the wire now, not at some unrelated later event: a
                    // freshly-assigned address's duplicate-address-detection
                    // probe and multicast-listener report, a bond's presence
                    // re-announcement. Flush every channel once so that
                    // output is transmitted immediately — the admin request
                    // is the event that produced it (event-driven, never a
                    // poll). A read-only query queues nothing, so this is a
                    // cheap no-op for it.
                    pump_all(&mut stack, &mut sockets, &mut channels, &secret, now());
                }
                SOCKET_TOKEN => serve_socket(
                    &mut stack,
                    &mut sockets,
                    &mut channels,
                    &secret,
                    &mut socket_request,
                    &mut origin_buf,
                    &mut socket_reply,
                ),
                other => serve_notify(&mut stack, &mut sockets, &mut channels, &secret, other),
            }
        }
    }

    /// A NIC driver rang the notify port for the channel token `token`
    /// names: drain the doorbell and pump that interface once (deliver any
    /// received datagrams to their sockets). An unknown token is ignored —
    /// a stale wake for a channel that is gone is harmless.
    fn serve_notify(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channels: &mut [Option<Channel>],
        secret: &CryptoCookieSecret,
        token: u64,
    ) {
        let Some(index) = token.checked_sub(CHANNEL_TOKEN_BASE) else {
            return;
        };
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let Some(Some(channel)) = channels.get_mut(index) else {
            return;
        };
        // Drain the coalescing doorbell so the wait-set member is not
        // immediately ready again; the notify carries no data, only "there
        // is receive work", which the pump discovers itself.
        drain_notify(channel.notify);
        // A driver rings the notify port on *any* device interrupt, a
        // config-change (link) interrupt included, so this wake is exactly
        // where a member's link-down/up is discovered live. Handle the
        // change after the pump releases its borrow of `channel`, so the
        // failover announcement can go out the *other* member's channel.
        let link_change = pump_channel(stack, sockets, channel, secret, now());
        if let Some((iface, link)) = link_change {
            handle_link_change(stack, sockets, channels, secret, iface, link, now());
        }
    }

    /// Apply a member NIC's live link change to the bond and transmit any
    /// resulting presence re-announcement (a failover), auditing it. A
    /// change that produces no announcement (a plain interface, or a bond
    /// with no path change) is silent. Callable only where the whole
    /// `channels` table is in hand, because the announcement egresses the
    /// newly-selected member — a *different* channel from the one that
    /// reported the change.
    fn handle_link_change(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channels: &mut [Option<Channel>],
        secret: &CryptoCookieSecret,
        iface: [u8; IF_NAME_LEN],
        link: LinkState,
        now: Duration64,
    ) {
        let announcements = stack.on_member_link_change(iface, link, now);
        if !announcements.is_empty() {
            audit(
                events::BOND_FAILOVER,
                Level::Info,
                "netstack: bond transmit path changed on a member link report (presence \
                 re-announced)",
            );
            transmit_batch(stack, sockets, channels, secret, &announcements);
        }
    }

    /// Serve one waiting admin request on [`NETSTACK_ENDPOINT`].
    ///
    /// `BindDriver` is intercepted here rather than in the pure [`serve`]
    /// dispatcher: provisioning a NIC channel needs shared memory, a bound
    /// notify port, and IPC the engine cannot perform. It is
    /// capability-checked (`CAP_NET_ADMIN`) against the caller's attested
    /// origin **before any state is touched**, exactly as [`serve`] gates
    /// every other admin op; every other request goes to [`serve`]
    /// unchanged.
    #[allow(clippy::too_many_arguments)]
    fn serve_admin(
        stack: &mut Netstack,
        sockets: &SocketService,
        channels: &mut [Option<Channel>],
        pid: u64,
        set: u64,
        request: &mut [u8],
        origin_buf: &mut [u8; ORIGIN_WIRE_LEN],
        reply: &mut [u8],
    ) {
        let mut ticket: u64 = 0;
        let request_len = match tairix_rt::call_recv(NETSTACK_ENDPOINT, request, &mut ticket) {
            Ok(len) => len,
            // A transient recv error (e.g. an oversize request left
            // queued) must not kill the server; drop it and continue.
            Err(_) => return,
        };
        let Some(caller) = attest(NETSTACK_ENDPOINT, ticket, origin_buf) else {
            return;
        };
        if let Ok(NetstackRequest::BindDriver {
            endpoint_id,
            iface,
            node_location,
        }) = NetstackRequest::from_bytes(&request[..request_len])
        {
            let result = serve_bind_driver(
                stack,
                channels,
                &caller,
                pid,
                set,
                endpoint_id,
                iface,
                node_location,
            );
            let _ = tairix_rt::call_reply(NETSTACK_ENDPOINT, ticket, &encode_status_reply(result));
            return;
        }
        // The per-interface configuration is a *separate* framed message (its
        // own magic), wider than the 64-byte request enum, so it is
        // intercepted here like `BindDriver` and matched by its magic before
        // the request decode. It is a pure state mutation, but decoding it
        // needs the wider frame, so the interception lives in the transport.
        if let Ok(msg) = NetInterfaceConfigMsg::from_bytes(&request[..request_len]) {
            let result = serve_interface_config(stack, channels, &caller, &msg);
            let _ = tairix_rt::call_reply(NETSTACK_ENDPOINT, ticket, &encode_status_reply(result));
            return;
        }
        // The bond configuration is a third self-identifying framed message
        // (its own magic), decoded before the request enum like the two
        // above. It composes/reconfigures a bond over member interfaces.
        if let Ok(msg) = NetBondConfigMsg::from_bytes(&request[..request_len]) {
            let result = serve_bond_config(stack, &caller, &msg);
            let _ = tairix_rt::call_reply(NETSTACK_ENDPOINT, ticket, &encode_status_reply(result));
            return;
        }
        match serve(
            stack,
            sockets,
            &caller,
            &LogSink,
            &request[..request_len],
            reply,
            now(),
        ) {
            Ok(len) => {
                let _ = tairix_rt::call_reply(NETSTACK_ENDPOINT, ticket, &reply[..len]);
            }
            Err(err) => reply_error(NETSTACK_ENDPOINT, ticket, err),
        }
    }

    /// Capability-check and carry out a `BindDriver`: gate on
    /// `CAP_NET_ADMIN` against the caller's attested origin (fail closed,
    /// audited), then provision the channel. The interface stays unbound on
    /// any refusal.
    // The bind carries the whole channel context (stack, channel table,
    // caller, ids, endpoint, alias, hardware location) as flat arguments;
    // a struct would only obscure the one call site.
    #[allow(clippy::too_many_arguments)]
    fn serve_bind_driver(
        stack: &mut Netstack,
        channels: &mut [Option<Channel>],
        caller: &Caller,
        pid: u64,
        set: u64,
        endpoint_id: u64,
        iface: [u8; IF_NAME_LEN],
        node_location: u64,
    ) -> Result<(), Errno> {
        if !caller.capabilities().holds(CapabilityId::NET_ADMIN) {
            audit(
                events::DRIVER_BIND_DENIED,
                Level::Warn,
                "netstack bind driver denied: caller lacks CAP_NET_ADMIN",
            );
            return Err(Errno::PermissionDenied);
        }
        match bind_driver(
            stack,
            channels,
            pid,
            set,
            endpoint_id,
            iface,
            node_location,
            now(),
        ) {
            Ok(()) => {
                audit(
                    events::DRIVER_BOUND,
                    Level::Info,
                    "netstack: NIC driver device channel bound to interface",
                );
                Ok(())
            }
            Err(err) => {
                // Report the exact provisioning step's errno so a failed
                // bind is diagnosable rather than opaque (fail loud); the
                // interface stays unbound regardless.
                audit_errno(
                    events::DRIVER_BIND_FAILED,
                    Level::Warn,
                    "netstack bind driver failed: provisioning refused (interface left unbound)",
                    err,
                );
                Err(err)
            }
        }
    }

    /// Capability-check and apply a per-interface configuration: gate on
    /// `CAP_NET_ADMIN` against the caller's attested origin (fail closed,
    /// audited) **before any state is touched**, then apply the whole
    /// message atomically ([`Netstack::apply_interface_config`]). A refusal
    /// (validation, an unmatched interface, an alias clash) leaves the
    /// interface untouched and is reported to the caller.
    fn serve_interface_config(
        stack: &mut Netstack,
        channels: &mut [Option<Channel>],
        caller: &Caller,
        msg: &NetInterfaceConfigMsg,
    ) -> Result<(), Errno> {
        if !caller.capabilities().holds(CapabilityId::NET_ADMIN) {
            audit(
                events::REQUEST_DENIED,
                Level::Warn,
                "netstack interface config denied: caller lacks CAP_NET_ADMIN",
            );
            return Err(Errno::PermissionDenied);
        }
        match stack.apply_interface_config(msg, now()) {
            Ok(renamed) => {
                // A driver channel is bound to an interface by *name*
                // (`service_interface` looks it up by name each pump). When
                // the apply renamed the interface to its admin alias, the
                // bound channel still holds the pre-rename name, so retarget
                // it here — otherwise the renamed interface can never be
                // pumped again (no DAD, no RX, no replies): it goes dark.
                if let Some((old, new)) = renamed {
                    if let Some(channel) = channels.iter_mut().flatten().find(|c| c.iface == old) {
                        channel.iface = new;
                    }
                }
                audit(
                    events::INTERFACE_CONFIG_APPLIED,
                    Level::Info,
                    "netstack: per-interface network configuration applied",
                );
                Ok(())
            }
            Err(err) => {
                // Report the exact refusal so a rejected configuration is
                // diagnosable (fail loud); the interface stays untouched.
                audit_errno(
                    events::ADMIN_REFUSED,
                    Level::Warn,
                    "netstack interface config refused (interface left untouched)",
                    err,
                );
                Err(err)
            }
        }
    }

    /// Capability-check and compose (or reconfigure) a bond: gate on
    /// `CAP_NET_ADMIN` against the caller's attested origin (fail closed,
    /// audited) **before any state is touched**, then apply the whole bond
    /// atomically ([`Netstack::apply_bond_config`]). A refusal (a member
    /// not present yet, an alias clash, validation) leaves the bond
    /// untouched and is reported to the caller.
    fn serve_bond_config(
        stack: &mut Netstack,
        caller: &Caller,
        msg: &NetBondConfigMsg,
    ) -> Result<(), Errno> {
        if !caller.capabilities().holds(CapabilityId::NET_ADMIN) {
            audit(
                events::REQUEST_DENIED,
                Level::Warn,
                "netstack bond config denied: caller lacks CAP_NET_ADMIN",
            );
            return Err(Errno::PermissionDenied);
        }
        match stack.apply_bond_config(msg, now()) {
            Ok(()) => {
                audit(
                    events::BOND_CONFIG_APPLIED,
                    Level::Info,
                    "netstack: bond interface composed/reconfigured",
                );
                Ok(())
            }
            Err(err) => {
                audit_errno(
                    events::BOND_CONFIG_REFUSED,
                    Level::Warn,
                    "netstack bond config refused (bond left untouched)",
                    err,
                );
                Err(err)
            }
        }
    }

    /// Serve one waiting socket request on [`NETSTACK_SOCKET_ENDPOINT`].
    fn serve_socket(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channels: &mut [Option<Channel>],
        secret: &CryptoCookieSecret,
        request: &mut [u8],
        origin_buf: &mut [u8; ORIGIN_WIRE_LEN],
        reply: &mut [u8],
    ) {
        let mut ticket: u64 = 0;
        let request_len = match tairix_rt::call_recv(NETSTACK_SOCKET_ENDPOINT, request, &mut ticket)
        {
            Ok(len) => len,
            Err(_) => return,
        };
        let Some(caller) = attest(NETSTACK_SOCKET_ENDPOINT, ticket, origin_buf) else {
            return;
        };
        // Ephemeral ports are drawn from the kernel CSPRNG; a momentarily
        // unavailable draw yields zero, which the bounded port search
        // simply treats as one exhausted candidate.
        let mut entropy = || {
            let mut bytes = [0u8; 4];
            let _ = tairix_rt::random_get(&mut bytes, RandomFlags::empty());
            u32::from_le_bytes(bytes)
        };
        match sockets.serve(
            stack,
            &caller,
            &LogSink,
            &mut entropy,
            &request[..request_len],
            reply,
            now(),
        ) {
            Ok(out) => {
                let _ = tairix_rt::call_reply(NETSTACK_SOCKET_ENDPOINT, ticket, &reply[..out.len]);
                // An `Accept` hands back the bytes the connection already
                // buffered (its one-shot Connected and any early data); send
                // them to the new child's delivery port.
                emit_deliveries(&out.deliveries);
                // Transmit any frames the operation produced (the datagram
                // itself, a neighbour resolution, an IGMP/MLD report, or an
                // ACK opened by an accept) out their interfaces and pump so
                // the driver doorbell sends them and any received reply is
                // delivered.
                transmit_batch(stack, sockets, channels, secret, &out.tx);
            }
            Err(err) => reply_error(NETSTACK_SOCKET_ENDPOINT, ticket, err),
        }
    }

    /// Read and decode the caller's kernel-attested origin for `ticket`
    /// on `endpoint`, replying a typed error and returning [`None`] when
    /// it cannot be attested (fail closed — never serve an unattested
    /// request).
    fn attest(
        endpoint: u64,
        ticket: u64,
        origin_buf: &mut [u8; ORIGIN_WIRE_LEN],
    ) -> Option<Caller> {
        match tairix_rt::call_peer_origin(endpoint, ticket, origin_buf) {
            Ok(n) => match Origin::from_bytes(&origin_buf[..n]) {
                Ok(origin) => Some(Caller::new(origin)),
                Err(err) => {
                    reply_error(endpoint, ticket, err);
                    None
                }
            },
            Err(ret) => {
                reply_error(endpoint, ticket, errno_from(ret));
                None
            }
        }
    }

    /// Answer `ticket` on `endpoint` with the status frame carrying `err`.
    fn reply_error(endpoint: u64, ticket: u64, err: Errno) {
        let frame = encode_status_reply(Err(err));
        let _ = tairix_rt::call_reply(endpoint, ticket, &frame);
    }

    /// Emit one field-less audit record through the system log.
    fn audit(id: EventId, level: Level, message: &'static str) {
        log(
            &LogSink,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Emit an audit record carrying the `errno` of the decision it reports,
    /// so a provisioning failure states its cause (fail loud) rather than
    /// leaving an opaque "refused".
    fn audit_errno(id: EventId, level: Level, message: &'static str, err: Errno) {
        log(
            &LogSink,
            &Event {
                level,
                id,
                message,
                fields: &[Field {
                    key: "errno",
                    value: FieldValue::SignedInt(i64::from(err.as_i32())),
                }],
            },
        );
    }

    /// Provision a NIC driver process's device channel into a managed
    /// interface: query the driver's facts, size and create the shared
    /// frame region, grant it, bind a notify port, attach, derive the
    /// interface's IPv6/IPv4 identity, and add it to the table. Every
    /// resource acquired is released on any later failure, so a rejected
    /// bind never half-provisions (fail closed).
    ///
    /// The driver is the channel *server* (it owns the device); the stack
    /// is the *client* that owns the frame region — so any NIC driver
    /// serves any stack build.
    // Each argument is an independent provisioning input (stack, channel
    // table, ids, endpoint, alias, hardware location, clock); bundling them
    // would only obscure the single call site.
    #[allow(clippy::too_many_arguments)]
    fn bind_driver(
        stack: &mut Netstack,
        channels: &mut [Option<Channel>],
        pid: u64,
        set: u64,
        endpoint_id: u64,
        iface: [u8; IF_NAME_LEN],
        node_location: u64,
        now: Duration64,
    ) -> Result<(), Errno> {
        // A free slot in the bounded channel table (its width is the
        // reserved endpoint block, so a full table means every channel is
        // in use — fail closed).
        let index = channels
            .iter()
            .position(Option::is_none)
            .ok_or(Errno::LimitExceeded)?;

        // Learn the device before sizing anything: the facts fix the ring
        // geometry both sides bind over.
        let mut transport = RtNetChannelTransport {
            endpoint: endpoint_id,
        };
        let facts = NetChannelClient::query_facts(&mut transport)?;
        facts.validate()?;
        // The receive ring holds a link frame; the transmit ring is sized
        // for a segmentation-offload super-frame when the device
        // negotiated it (`for_device` is the one definition both sides
        // derive, so the driver's attach validation agrees).
        let geometry = RingGeometry::for_device(&facts, NET_RING_SLOTS)?;
        let region_len = geometry.region_len();

        // Create and map the shared frame region (owner mapping), then mint
        // the driver's grant handle for it.
        let mut region_id = 0u64;
        let base = tairix_rt::shm_create(region_len, &mut region_id);
        if base < 0 {
            return Err(errno_from(base));
        }
        // SAFETY: `shm_create` mapped exactly `region_len` bytes of zeroed,
        // cacheable, RW (non-executable), guard-bracketed memory into this
        // process at `base`, owned by this process. Nothing else in this
        // address space aliases the region, so a single exclusive
        // `&'static mut [u8]` over exactly `region_len` bytes is sound; it
        // lives as long as the channel (the whole service lifetime) and is
        // released only by the `shm_unmap` on a failure path below. The
        // driver maps the same frames through its own grant and never
        // touches ring bytes across a `Service` doorbell.
        let region: &'static mut [u8] =
            unsafe { core::slice::from_raw_parts_mut(base as usize as *mut u8, region_len) };

        let grant = tairix_rt::shm_grant(region_id, endpoint_id);
        if grant < 0 {
            let _ = tairix_rt::shm_unmap(base as u64, region_len);
            return Err(errno_from(grant));
        }

        // Bind the notify mailbox the driver rings on receive. Its id is
        // the non-reserved, per-(pid, slot) `notify_endpoint_for` name, so
        // the bind needs no privilege and cannot collide.
        let notify = notify_endpoint_for(pid, index as u64);
        if tairix_rt::port_bind(notify, NET_CHANNEL_NOTIFY_LEN, NOTIFY_CAPACITY) != 0 {
            let _ = tairix_rt::shm_unmap(base as u64, region_len);
            return Err(Errno::AlreadyExists);
        }

        // Attach hands the region and notify port to the driver; on refusal
        // the mapping is released (the driver never saw a usable channel).
        let client = match NetChannelClient::attach(
            transport,
            region,
            geometry,
            FRAME_CLASS,
            grant as u64,
            notify,
        ) {
            Ok(client) => client,
            Err(err) => {
                let _ = tairix_rt::shm_unmap(base as u64, region_len);
                return Err(err);
            }
        };

        // Join the notify port to the wait set before adding the interface,
        // so a failure to add the interface only has to undo the membership.
        let token = CHANNEL_TOKEN_BASE + index as u64;
        if tairix_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::Port, notify, token) != 0 {
            let _ = client.detach();
            let _ = tairix_rt::shm_unmap(base as u64, region_len);
            return Err(Errno::DeviceFault);
        }

        // Derive the interface's IPv6 identity from the device MAC (modified
        // EUI-64) and a CSPRNG IPv4 identification seed (entropy stays at
        // the service seam; the engine is pure), then add the interface.
        let interface_id = eui64_interface_id(*facts.mac.as_octets());
        let ipv4_ident_seed = draw_ident_seed();
        if let Err(err) = stack.add_interface(
            iface,
            NetIfKind::Ethernet,
            facts,
            interface_id,
            ipv4_ident_seed,
            node_location,
            now,
        ) {
            let _ =
                tairix_rt::waitset_ctl(set, WaitSetOp::Del, WaitSourceKind::Port, notify, token);
            let _ = client.detach();
            let _ = tairix_rt::shm_unmap(base as u64, region_len);
            return Err(err);
        }

        channels[index] = Some(Channel {
            iface,
            notify,
            client,
        });
        Ok(())
    }

    /// Draw a CSPRNG 16-bit IPv4 identification seed. A momentarily
    /// unavailable draw yields zero, which the engine simply treats as the
    /// starting counter value — never a blocking wait.
    fn draw_ident_seed() -> u16 {
        let mut bytes = [0u8; 2];
        let _ = tairix_rt::random_get(&mut bytes, RandomFlags::empty());
        u16::from_le_bytes(bytes)
    }

    /// Bounded pump rounds per channel: a doorbell can leave inbound
    /// frames the current `service_interface` did not re-harvest (a
    /// SYN-ACK on the RX ring), and driving a segment can produce egress
    /// frames that need another doorbell, so the pump iterates until the
    /// interface is quiet — never unbounded (a hostile flood cannot pin it).
    const PUMP_ROUNDS: usize = 32;

    /// Stage each interface's outbound frame batch onto its channel's TX
    /// ring and pump it, so the driver doorbell transmits it. A batch for
    /// an interface with no bound channel is dropped — its link is gone.
    fn transmit_batch(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channels: &mut [Option<Channel>],
        secret: &CryptoCookieSecret,
        batch: &FrameBatch,
    ) {
        for (name, frames) in batch {
            // Resolve the frame's target channel: a bond tag becomes its
            // selected member; a member/plain tag is itself. A batch for an
            // interface with no bound channel (or a bond with no eligible
            // member) is dropped — its link is gone.
            let target = stack.egress_member(*name, 0).unwrap_or(*name);
            let Some(channel) = channels.iter_mut().flatten().find(|c| c.iface == target) else {
                continue;
            };
            let _ = queue_tx(&mut channel.client, frames);
            // A link change observed while draining a TX batch is left for
            // the next notify/timer pump to act on (the channels table is
            // borrowed here); `facts.link` is untouched until handled, so it
            // is re-observed then, never lost.
            let _ = pump_channel(stack, sockets, channel, secret, now());
        }
    }

    /// Emit the stream events a connection produced to their clients'
    /// async ports, and stage its egress frames onto their bound channels,
    /// pumping each so the driver transmits them.
    fn distribute(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channels: &mut [Option<Channel>],
        secret: &CryptoCookieSecret,
        io: &StreamIo,
    ) {
        emit_deliveries(&io.deliveries);
        transmit_batch(stack, sockets, channels, secret, &io.tx);
    }

    /// `ipc_send` each stream/datagram delivery to its socket's async port
    /// (best-effort: a client that is gone simply drops it).
    fn emit_deliveries(deliveries: &[Delivery]) {
        for delivery in deliveries {
            let _ = tairix_rt::ipc_send(delivery.deliver_port, &delivery.datagram);
        }
    }

    /// Pump one channel-backed interface to quiescence: transmit staged
    /// frames, doorbell the driver, harvest received frames, and route each
    /// engine event to the socket layer — a datagram to its bound socket, a
    /// TCP segment to its connection (whose response frames are re-queued
    /// onto this channel and re-transmitted in the same pump). A ring or
    /// device fault leaves the interface in place; the next wake retries.
    ///
    /// Returns the interface's live link change, if the driver's service
    /// report showed one during this pump, for the caller to feed to
    /// [`handle_link_change`] once it holds the whole channels table (a
    /// failover announcement egresses a *different* member's channel).
    fn pump_channel(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channel: &mut Channel,
        secret: &CryptoCookieSecret,
        now: Duration64,
    ) -> Option<([u8; IF_NAME_LEN], LinkState)> {
        let mut link_change = None;
        for _ in 0..PUMP_ROUNDS {
            let Ok(outcome) = stack.service_interface(channel.iface, &mut channel.client, now)
            else {
                return link_change;
            };
            if let Some(link) = outcome.link_change {
                link_change = Some((channel.iface, link));
            }
            let mut staged = false;
            let mut saw_event = false;
            for event in &outcome.events {
                saw_event = true;
                match event {
                    StackEvent::EchoRequestServed { .. } => audit(
                        events::INBOUND_ECHO_SERVED,
                        Level::Info,
                        "netstack: inbound echo request served (reply queued)",
                    ),
                    StackEvent::UdpDatagram { .. } | StackEvent::EchoReply { .. } => {
                        emit_deliveries(&sockets.deliver(event));
                    }
                    StackEvent::TcpSegment {
                        source,
                        destination,
                        segment,
                    } => {
                        let io = sockets.on_tcp_segment(
                            stack,
                            *source,
                            *destination,
                            segment,
                            now,
                            secret,
                        );
                        emit_deliveries(&io.deliveries);
                        for (name, frames) in &io.tx {
                            // A connection's frames are tagged by its logical
                            // interface; resolve a bond to its active member
                            // so a reply staged here lands on the member this
                            // pump drives.
                            let target = stack.egress_member(*name, 0).unwrap_or(*name);
                            if target == channel.iface && !frames.is_empty() {
                                let _ = queue_tx(&mut channel.client, frames);
                                staged = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !saw_event && !staged {
                break;
            }
        }
        link_change
    }

    /// Pump every bound channel (a deadline lapse): first advance every
    /// connected stream's TCP timers (retransmit, delayed ACK, persist,
    /// user timeout, TIME-WAIT) and distribute their frames/events, then
    /// pump each channel so each interface's engine emits its own timer-due
    /// work (DAD, SLAAC RS, IGMP/MLD, neighbour retransmits).
    fn pump_all(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        channels: &mut [Option<Channel>],
        secret: &CryptoCookieSecret,
        now: Duration64,
    ) {
        let io = sockets.advance_streams(stack, now);
        distribute(stack, sockets, channels, secret, &io);
        // Advance every bond's failover health monitor (admitting recovered
        // members past their up-delay) and transmit any gratuitous
        // announcements a resulting path change produced, then audit it.
        let announcements = stack.advance_bonds(now);
        if !announcements.is_empty() {
            audit(
                events::BOND_FAILOVER,
                Level::Info,
                "netstack: bond transmit path changed (presence re-announced)",
            );
            transmit_batch(stack, sockets, channels, secret, &announcements);
        }
        // Pump each channel; collect any live link change a driver report
        // surfaced so it can be applied after this borrow of `channels`
        // ends (a failover announcement egresses a *different* member's
        // channel). Bounded by the channel count.
        let mut link_changes: Vec<([u8; IF_NAME_LEN], LinkState)> = Vec::new();
        for channel in channels.iter_mut().flatten() {
            if let Some(change) = pump_channel(stack, sockets, channel, secret, now) {
                link_changes.push(change);
            }
        }
        for (iface, link) in link_changes {
            handle_link_change(stack, sockets, channels, secret, iface, link, now);
        }
    }

    /// Drain a channel's notify mailbox: the wake carries no data (only
    /// "there is receive work"), so every queued doorbell is consumed to
    /// reset the wait-set member; the pump discovers the work itself.
    fn drain_notify(notify: u64) {
        let mut frame = [0u8; NET_CHANNEL_NOTIFY_LEN];
        let mut sender = [0u8; ORIGIN_WIRE_LEN];
        while tairix_rt::ipc_recv(notify, &mut frame, &mut sender).is_ok() {}
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `tairix-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
