//! The `Run` entry-point binary of the network-stack service, installed
//! at `/System/Services/netstack.app/Run` — the long-running user-space
//! service PID 1 `init` launches to own the network interfaces and serve
//! the `netstack-v1` IPC surface (`plans/NETWORK.md` §2.2).
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the
//! Rust userland runtime `rustos-rt` — never the C ABI, which exists
//! solely for programs *not* written in Rust. `rustos-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `#[global_allocator]`, and the syscall wrappers; `rustos_rt::entry!`
//! names this program's `main`.
//!
//! # What this service does
//!
//! At startup it binds the well-known
//! [`rustos_abi::net_ipc::NETSTACK_ENDPOINT`] (an unrestricted-sender
//! call endpoint — any process may post, but the id is a reserved
//! rendezvous, so binding it needs the manifest's
//! `CAP_IPC_BIND_PRIVILEGED`: a squatter could otherwise receive
//! interface mutations and serve forged network state) and then parks on
//! a wait set, woken by requests and by the engines' one-shot deadlines
//! — never a polling loop. Each request is served by the
//! capability-checked [`rustos_netstack::serve`] dispatcher against the
//! caller's kernel-attested origin.
//!
//! NIC frame-ring channels join this wait set as the device manager
//! binds network drivers to the service; until then the interface table
//! is empty, the deadline is unarmed, and the loop parks solely on the
//! endpoint. The QEMU vertical that wires a live virtio-net driver
//! through this loop is the plan's N3c increment.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the
// optional `rustos-rt` runtime through the default `program` feature. The
// kernel and host tooling build only this crate's *library*, so this module
// (and `rustos-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use rustos_abi::net::{SocketRequest, NETSTACK_SOCKET_ENDPOINT, SOCKET_MAX_REPLY};
    use rustos_abi::net_ipc::{NETSTACK_ENDPOINT, NETSTACK_MAX_REPLY, NETSTACK_MAX_REQUEST};
    use rustos_abi::reply::encode_status_reply;
    use rustos_abi::waitset::{WaitSetOp, WaitSourceKind};
    use rustos_abi::{Duration64, Errno, Origin, RandomFlags, ORIGIN_WIRE_LEN};
    use rustos_caps::CapabilitySet;
    use rustos_netstack::{serve, Caller, Netstack, SocketService};
    use rustos_rt::LogSink;

    /// Outstanding-call capacity of the endpoint (a fail-closed memory
    /// bound).
    const CAPACITY: usize = 8;

    /// Wait-set member token of the admin request endpoint.
    const ENDPOINT_TOKEN: u64 = 1;

    /// Wait-set member token of the socket (data-plane control) endpoint.
    const SOCKET_TOKEN: u64 = 2;

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
        let nanos = rustos_rt::clock_get();
        // Both components are in range by construction: secs fits i64
        // (u64 ns / 1e9 < i64::MAX) and the remainder is < 1e9.
        Duration64::new(
            (nanos / 1_000_000_000) as i64,
            (nanos % 1_000_000_000) as u32,
        )
        .unwrap_or_default()
    }

    /// Nanoseconds from `now` until the engines' earliest deadline;
    /// `u64::MAX` (park indefinitely) when nothing is armed.
    fn timeout_ns(stack: &Netstack) -> u64 {
        let Some(deadline) = stack.next_deadline() else {
            return u64::MAX;
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
        let bound = rustos_rt::call_create(
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
        let set = rustos_rt::waitset_create();
        if set < 0 {
            return 1;
        }
        let set = set as u64;
        if rustos_rt::waitset_ctl(
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
        if rustos_rt::call_create(
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
        if rustos_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            NETSTACK_SOCKET_ENDPOINT,
            SOCKET_TOKEN,
        ) != 0
        {
            return 1;
        }

        let mut stack = Netstack::new();
        let mut sockets = SocketService::new();
        let mut request = [0u8; NETSTACK_MAX_REQUEST];
        let mut socket_request = [0u8; SocketRequest::MAX_WIRE_LEN];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        let mut reply = [0u8; NETSTACK_MAX_REPLY];
        let mut socket_reply = [0u8; SOCKET_MAX_REPLY];
        loop {
            // Park until a request arrives or the engines' one-shot
            // deadline lapses; a lapsed deadline re-arms below after
            // the engines observed the new `now`.
            let mut token = 0u64;
            let woke = rustos_rt::waitset_wait(set, timeout_ns(&stack), &mut token);
            if woke != 0 {
                continue;
            }
            match token {
                ENDPOINT_TOKEN => {
                    serve_admin(&mut stack, &mut request, &mut origin_buf, &mut reply)
                }
                SOCKET_TOKEN => serve_socket(
                    &mut stack,
                    &mut sockets,
                    &mut socket_request,
                    &mut origin_buf,
                    &mut socket_reply,
                ),
                _ => {}
            }
        }
    }

    /// Serve one waiting admin request on [`NETSTACK_ENDPOINT`].
    fn serve_admin(
        stack: &mut Netstack,
        request: &mut [u8],
        origin_buf: &mut [u8; ORIGIN_WIRE_LEN],
        reply: &mut [u8],
    ) {
        let mut ticket: u64 = 0;
        let request_len = match rustos_rt::call_recv(NETSTACK_ENDPOINT, request, &mut ticket) {
            Ok(len) => len,
            // A transient recv error (e.g. an oversize request left
            // queued) must not kill the server; drop it and continue.
            Err(_) => return,
        };
        let Some(caller) = attest(NETSTACK_ENDPOINT, ticket, origin_buf) else {
            return;
        };
        match serve(
            stack,
            &caller,
            &LogSink,
            &request[..request_len],
            reply,
            now(),
        ) {
            Ok(len) => {
                let _ = rustos_rt::call_reply(NETSTACK_ENDPOINT, ticket, &reply[..len]);
            }
            Err(err) => reply_error(NETSTACK_ENDPOINT, ticket, err),
        }
    }

    /// Serve one waiting socket request on [`NETSTACK_SOCKET_ENDPOINT`].
    fn serve_socket(
        stack: &mut Netstack,
        sockets: &mut SocketService,
        request: &mut [u8],
        origin_buf: &mut [u8; ORIGIN_WIRE_LEN],
        reply: &mut [u8],
    ) {
        let mut ticket: u64 = 0;
        let request_len = match rustos_rt::call_recv(NETSTACK_SOCKET_ENDPOINT, request, &mut ticket)
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
            let _ = rustos_rt::random_get(&mut bytes, RandomFlags::empty());
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
                // No NIC is bound to this service yet, so the interface
                // table is empty and a datagram send fails closed before
                // producing any frame: `out.tx` is empty here. The
                // end-to-end data path lands with NIC autobind.
                let _ = rustos_rt::call_reply(NETSTACK_SOCKET_ENDPOINT, ticket, &reply[..out.len]);
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
        match rustos_rt::call_peer_origin(endpoint, ticket, origin_buf) {
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
        let _ = rustos_rt::call_reply(endpoint, ticket, &frame);
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `rustos-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
