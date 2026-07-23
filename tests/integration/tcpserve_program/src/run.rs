//! The `Run` entry-point binary of the `tcpserve` TCP-listener fixture — the
//! program the aarch64 listener QEMU vertical runs from the scripted root
//! shell (`plans/NETWORK.md` N6b-2-β-2).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the `net` stream-socket wrappers over `ipc_call`;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! `main` proves the whole server-side `SocketType::Stream` path — the
//! role-swapped mirror of the `tcpecho` client vertical — end to end over the
//! live two-process network:
//!
//! 1. **Bind** two async delivery ports (ordinary unrestricted process
//!    resources, no capability) and **open** a stream socket, delivering the
//!    listener's readiness events to the first port.
//! 2. **Bind** the socket to the shared well-known (**privileged**) port and
//!    **listen**. Binding a privileged port is gated on `CAP_NET_BIND_PRIVILEGED`
//!    (this fixture's manifest requests it, intersected with the launching
//!    administrator's ceiling): a refusal is a real failure, never retried.
//! 3. **Accept** the host client peer's connection over the shared IPv6
//!    link-local wire once it arrives (the client retries through the boot
//!    window while the NIC driver is still autoloading, so the server simply
//!    waits on its delivery ports — never a busy poll).
//! 4. **Echo** every received byte back to the client in order, verifying the
//!    received run matches the shared deterministic stream at its absolute
//!    offset (a corrupted, reordered, or duplicated byte fails closed at its
//!    first wrong position). The host peer injects packet loss, so a passing
//!    run proves RFC 9293 retransmission carried the stream both ways across
//!    the two-process boundary — not just a clean link.
//! 5. The client closes only after it has received and re-verified the whole
//!    echo, so the child's `Closed { PeerClosed }` (with the whole transfer
//!    received) is the server's completion witness: on it the server closes
//!    its own half, prints the `TCPSERVE PASS …` marker, and exits `0`; the
//!    consuming vertical keys its PASS chain on that clean exit.
//!
//! **A failed exchange never exits.** The vertical's guest-side sink arms its
//! PASS chain on this program's audited `exit`, so any shortfall — a refused
//! call, a mismatched or truncated stream, an abortive reset, a peer that
//! closes early — prints the reason to standard error and parks forever off
//! the run queue; the run then times out and the harness reports the failure
//! loudly, with the diagnosis in the serial transcript (fail loud, no CPU
//! burned while failing).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::vec::Vec;

    use tairix_abi::net::{SocketAddr, SocketId, SocketStreamEvent, StreamCloseReason};
    use tairix_abi::net_ipc::NetAddrFamily;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, Origin};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_rt::net::{accept, bind, close, listen, stream_recv, stream_send, stream_socket};
    use tairix_test_netstack_wire as wire;
    use tairix_test_tcpserve::{verify_chunk, PASS_MARKER, TRANSFER_BYTES};

    /// The listener's async delivery-port endpoint id: an app-local,
    /// unrestricted well-known value (not a reserved kernel id), so binding
    /// it needs no capability. The stack posts the listener's `Accepted`
    /// readiness events here.
    const LISTEN_PORT: u64 = 0x_7463_7301;

    /// The accepted child connection's async delivery-port endpoint id: a
    /// second unrestricted app-local value, so the child's stream events
    /// (`Connected`/`Data`/`Closed`) are cleanly separated from the
    /// listener's readiness events.
    const CONN_PORT: u64 = 0x_7463_7302;

    /// Largest delivery message a port must hold: a stream event header plus
    /// a maximum-size data payload.
    const DELIVER_MAX_PAYLOAD: usize = SocketStreamEvent::MAX_WIRE_LEN;

    /// Delivery-port mailbox depth. Generous headroom so a burst of inbound
    /// data events queues rather than back-pressuring the stack mid-transfer;
    /// the stack's own receive window bounds the true in-flight volume.
    const DELIVER_CAPACITY: usize = 64;

    /// Wait-set tokens for the two delivery ports (any distinct non-zero
    /// values identify which source woke us; the server drains both).
    const LISTEN_TOKEN: u64 = 1;
    const CONN_TOKEN: u64 = 2;

    /// One park slice while blocking for the next event: the server gives the
    /// CPU up and is woken when the stack posts an event or this one-shot
    /// timer elapses (whichever first) — never a busy poll.
    const EVENT_PARK_NANOS: u64 = 200_000_000;

    /// Overall deadline for the accept phase (awaiting the client's
    /// connection through the boot-time NIC autoload window) and for the
    /// echo phase (draining and echoing the whole transfer). Generous for the
    /// loss-recovered transfer on QEMU TCG, but bounded so a genuinely dead
    /// connection fails with a reason rather than only via the vertical's
    /// outer timeout.
    const PHASE_TIMEOUT_NANOS: u64 = 120_000_000_000;

    /// The local listen address: the unspecified IPv6 address with the shared
    /// well-known port, so the server accepts a connection to any of its
    /// local addresses (its EUI-64 link-local, auto-configured once the NIC
    /// driver binds) on that port.
    fn listen_addr() -> SocketAddr {
        SocketAddr {
            family: NetAddrFamily::V6,
            addr: [0u8; 16],
            port: wire::GUEST_TCP_PORT,
        }
    }

    /// Terminal failure: report the reason on standard error, then park
    /// forever off the run queue. This program must **never exit** on a
    /// failure — the consuming vertical arms its PASS chain on this process's
    /// audited `exit`, so failing loudly means parking until the harness
    /// times the run out with the reason in the transcript. The spin fallback
    /// runs only if even the park is refused.
    fn fail(reason: &str) -> ! {
        write_stderr_line(reason);
        let _ = tairix_rt::park_forever();
        loop {
            core::hint::spin_loop();
        }
    }

    /// Park on the delivery-port wait-set for the next event, failing if the
    /// deadline passes. `ipc_recv` is non-blocking — an empty port is the
    /// retryable `WouldBlock` — so a caller draining events matches
    /// `Err(Errno::WouldBlock)` and calls this to give the CPU up until the
    /// stack posts an event (or the one-shot timer elapses), never spinning.
    fn park_for_event(
        set: u64,
        deadline_ns: u64,
        timeout_msg: &'static str,
    ) -> Result<(), &'static str> {
        if tairix_rt::clock_get() >= deadline_ns {
            return Err(timeout_msg);
        }
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(set, EVENT_PARK_NANOS, &mut token);
        Ok(())
    }

    /// Open the listener socket, bind the privileged port, and start
    /// listening. Returns the listener socket handle.
    fn open_and_listen() -> Result<SocketId, &'static str> {
        let socket = stream_socket(NetAddrFamily::V6, LISTEN_PORT)
            .map_err(|_| "tcpserve: stream_socket refused")?;
        // Binding to the unspecified address reserves the local port without
        // needing a configured interface, so this succeeds even before the
        // NIC driver has bound. A refusal here is a real, non-transient
        // failure (a missing CAP_NET_BIND_PRIVILEGED yields PermissionDenied).
        let bound = bind(socket, listen_addr()).map_err(|e| match e {
            Errno::PermissionDenied => {
                "tcpserve: privileged bind denied (missing CAP_NET_BIND_PRIVILEGED)"
            }
            _ => "tcpserve: bind refused",
        })?;
        if bound != wire::GUEST_TCP_PORT {
            return Err("tcpserve: bound port did not match the requested well-known port");
        }
        listen(socket).map_err(|_| "tcpserve: listen refused")?;
        Ok(socket)
    }

    /// Block for the listener's `Accepted` readiness and claim the child
    /// connection, returning the child socket handle and the kernel-attested
    /// origin of the stack (captured so every later event can be required to
    /// match it — a delivery port is otherwise an unauthenticated inbox).
    fn accept_connection(
        set: u64,
        listener: SocketId,
        buf: &mut [u8],
    ) -> Result<(SocketId, Origin), &'static str> {
        let deadline = tairix_rt::clock_get().saturating_add(PHASE_TIMEOUT_NANOS);
        loop {
            // Claim any ready connection first: the child may be ready before
            // (or without) a separate readiness event being drained.
            match accept(listener, CONN_PORT) {
                Ok(child) => {
                    let origin = capture_origin(set, deadline)?;
                    return Ok((child, origin));
                }
                Err(Errno::WouldBlock) => {}
                Err(_) => return Err("tcpserve: accept refused"),
            }
            // Drain the listener's readiness events (an `Accepted` for our
            // listener means a connection is queued; anything else on this
            // port is unexpected).
            match stream_recv(LISTEN_PORT, buf) {
                Ok((SocketStreamEvent::Accepted { socket }, _)) if socket == listener => {}
                Ok((SocketStreamEvent::Accepted { .. }, _)) => {
                    return Err("tcpserve: Accepted for a foreign listener")
                }
                Ok(_) => return Err("tcpserve: unexpected event on the listener port"),
                Err(Errno::WouldBlock) => {
                    park_for_event(set, deadline, "tcpserve: timed out awaiting a connection")?;
                }
                Err(_) => return Err("tcpserve: listener event receive failed"),
            }
        }
    }

    /// Capture the stack origin from the child's first delivered event
    /// (`Connected`), which the accept path flushes to the connection port.
    fn capture_origin(set: u64, deadline_ns: u64) -> Result<Origin, &'static str> {
        let mut buf = [0u8; DELIVER_MAX_PAYLOAD];
        loop {
            match stream_recv(CONN_PORT, &mut buf) {
                Ok((SocketStreamEvent::Connected { .. }, origin)) => return Ok(origin),
                Ok((SocketStreamEvent::Data { .. }, origin)) => {
                    // Data may arrive coalesced with the connection flush;
                    // the origin is what we need, and re-draining data here
                    // would drop it, so require Connected to precede data.
                    let _ = origin;
                    return Err("tcpserve: data before the connection's Connected event");
                }
                Ok((SocketStreamEvent::Closed { .. }, _)) => {
                    return Err("tcpserve: connection closed before it established")
                }
                Ok(_) => return Err("tcpserve: unexpected first event on the connection"),
                Err(Errno::WouldBlock) => {
                    park_for_event(set, deadline_ns, "tcpserve: timed out awaiting Connected")?;
                }
                Err(_) => return Err("tcpserve: connection event receive failed"),
            }
        }
    }

    /// Drain the whole echoed transfer: receive every `Data` event on the
    /// child, verify each chunk against the deterministic stream at its
    /// absolute offset, echo the bytes straight back, and complete when the
    /// peer closes after the whole transfer round-tripped.
    fn serve_echo(
        set: u64,
        child: SocketId,
        stack: Origin,
        buf: &mut [u8],
    ) -> Result<(), &'static str> {
        let deadline = tairix_rt::clock_get().saturating_add(PHASE_TIMEOUT_NANOS);
        let mut received: usize = 0;
        let mut pending: Vec<u8> = Vec::new();
        let mut peer_closed = false;
        loop {
            // Drain available inbound events without blocking.
            match stream_recv(CONN_PORT, buf) {
                Ok((event, origin)) => {
                    if origin != stack {
                        return Err("tcpserve: event from an unexpected origin");
                    }
                    match event {
                        SocketStreamEvent::Data { socket, payload } if socket == child => {
                            if verify_chunk(received, payload).is_err() {
                                return Err("tcpserve: received byte did not match the stream");
                            }
                            received = received.saturating_add(payload.len());
                            pending.extend_from_slice(payload);
                        }
                        SocketStreamEvent::Data { .. } => {
                            return Err("tcpserve: data for a foreign socket")
                        }
                        SocketStreamEvent::Closed { socket, reason } if socket == child => {
                            match reason {
                                // The client half-closes cleanly after it has
                                // received and re-verified the whole echo.
                                StreamCloseReason::PeerClosed => peer_closed = true,
                                // A reset, timeout, or refusal is an abortive
                                // teardown before completion — fail closed.
                                _ => return Err("tcpserve: connection aborted before completion"),
                            }
                        }
                        SocketStreamEvent::Closed { .. } => {
                            return Err("tcpserve: close for a foreign socket")
                        }
                        SocketStreamEvent::Connected { .. } => {
                            return Err("tcpserve: a second Connected event")
                        }
                        SocketStreamEvent::Accepted { .. } => {
                            return Err("tcpserve: Accepted event on a connection socket")
                        }
                    }
                }
                Err(Errno::WouldBlock) => {}
                Err(_) => return Err("tcpserve: connection event receive failed"),
            }

            // Echo whatever we have buffered, draining what the send buffer
            // accepts (the rest is retried on the next pass as ACKs free
            // room). A refused send is a real failure.
            if !pending.is_empty() {
                match stream_send(child, &pending) {
                    Ok(0) => {}
                    Ok(accepted) => {
                        pending.drain(..accepted as usize);
                    }
                    Err(_) => return Err("tcpserve: stream_send refused"),
                }
            }

            // The client closes only after receiving and re-verifying the
            // whole echo, so a peer close with the full transfer received (and
            // nothing left to echo) is the completion witness.
            if peer_closed && received >= TRANSFER_BYTES && pending.is_empty() {
                return Ok(());
            }
            if peer_closed && (received < TRANSFER_BYTES) {
                return Err("tcpserve: peer closed before the whole transfer arrived");
            }

            // Nothing to do until the next event or the send buffer drains:
            // park (bounded by the phase deadline) rather than spin.
            park_for_event(set, deadline, "tcpserve: timed out serving the transfer")?;
        }
    }

    /// Run the server, returning `Ok` only when the whole transfer was
    /// received, verified, echoed, and the peer closed cleanly.
    fn run() -> Result<(), &'static str> {
        // Both delivery ports are ordinary process resources (no capability);
        // bind them before opening the socket so no early event is lost.
        if tairix_rt::port_bind(LISTEN_PORT, DELIVER_MAX_PAYLOAD, DELIVER_CAPACITY) < 0 {
            return Err("tcpserve: listener delivery port bind refused");
        }
        if tairix_rt::port_bind(CONN_PORT, DELIVER_MAX_PAYLOAD, DELIVER_CAPACITY) < 0 {
            return Err("tcpserve: connection delivery port bind refused");
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return Err("tcpserve: wait-set create refused");
        }
        let set = set as u64;
        for (port, token) in [(LISTEN_PORT, LISTEN_TOKEN), (CONN_PORT, CONN_TOKEN)] {
            if tairix_rt::waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::Port, port, token) != 0 {
                return Err("tcpserve: wait-set port registration refused");
            }
        }

        let listener = open_and_listen()?;
        let mut buf = [0u8; DELIVER_MAX_PAYLOAD];
        let (child, stack) = accept_connection(set, listener, &mut buf)?;
        serve_echo(set, child, stack, &mut buf)?;
        // Orderly close of our half; the listener is closed too. A refused
        // close is not a data-integrity failure — the transfer already
        // completed and the peer already closed — so it is not fatal.
        let _ = close(child);
        let _ = close(listener);
        Ok(())
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    /// Returns `0` only for a fully verified echoed transfer; every other
    /// outcome diverges into [`fail`].
    fn main() -> i32 {
        match run() {
            Ok(()) => {
                let mut line = [0u8; 64];
                let text = format_pass(&mut line);
                if Stdout.write_all(text.as_bytes()).is_err() {
                    // A PASS the transcript never carried is not a PASS the
                    // vertical may act on: park rather than exit.
                    fail("tcpserve: report write failed");
                }
                0
            }
            Err(reason) => fail(reason),
        }
    }

    /// Render the `TCPSERVE PASS <bytes> bytes` report line into `buf`,
    /// allocation-free, returning the written text.
    fn format_pass(buf: &mut [u8; 64]) -> &str {
        let mut w = Cursor { buf, len: 0 };
        use core::fmt::Write as _;
        // Bounded, well-formed input — the marker plus a small integer — so a
        // formatting overflow is impossible; if it somehow occurred the text
        // is simply the marker, still a valid PASS line.
        let _ = write!(w, "{PASS_MARKER} {TRANSFER_BYTES} bytes\n");
        let len = w.len;
        core::str::from_utf8(&w.buf[..len]).unwrap_or(PASS_MARKER)
    }

    /// A bounded `core::fmt::Write` sink over a fixed buffer; a write past the
    /// end is refused rather than truncating mid-text.
    struct Cursor<'a> {
        buf: &'a mut [u8; 64],
        len: usize,
    }

    impl core::fmt::Write for Cursor<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let bytes = s.as_bytes();
            let end = self.len.checked_add(bytes.len()).ok_or(core::fmt::Error)?;
            if end > self.buf.len() {
                return Err(core::fmt::Error);
            }
            self.buf[self.len..end].copy_from_slice(bytes);
            self.len = end;
            Ok(())
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
