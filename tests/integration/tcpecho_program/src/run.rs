//! The `Run` entry-point binary of the `tcpecho` stream-socket fixture — the
//! program the aarch64 stream QEMU vertical runs from the scripted root shell
//! (`plans/NETWORK.md` N5c).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the `net` stream-socket client wrappers over `ipc_call`;
//! `tairix_rt::entry!` names this program's `main`.
//!
//! `main` proves the whole `SocketType::Stream` path end to end over the live
//! two-process network:
//!
//! 1. **Bind** an async delivery port (an ordinary unrestricted process
//!    resource, no capability) and **open** a stream socket, delivering the
//!    connection's `SocketStreamEvent`s to that port.
//! 2. **Connect** to the host peer's passive TCP echo server over the shared
//!    IPv6 link-local wire, retrying while the interface is still coming up at
//!    boot (`NetworkUnreachable` — the driver may not be bound yet), so the
//!    fixture never races the two-process autoload.
//! 3. **Stream** a fixed, deterministic byte run to the peer and **verify**
//!    the peer echoes every byte back in order, re-deriving each expected byte
//!    from the shared generator (no whole-transfer buffer). The peer injects
//!    packet loss, so a passing run proves RFC 9293 retransmission carried the
//!    stream across the two-process boundary — not just a clean link.
//! 4. On success **close** the socket, print the `TCPECHO PASS …` marker, and
//!    exit `0`; the consuming vertical keys its PASS chain on that clean exit.
//!
//! **A failed transfer never exits.** The vertical's guest-side sink arms its
//! PASS chain on this program's audited `exit`, so any shortfall — a refused
//! call, a mismatched or truncated echo, an abortive close — prints the reason
//! to standard error and parks forever off the run queue; the run then times
//! out and the harness reports the failure loudly, with the diagnosis in the
//! serial transcript (fail loud, no CPU burned while failing).
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    use tairix_abi::net::{SocketAddr, SocketId, SocketStreamEvent, StreamCloseReason};
    use tairix_abi::net_ipc::NetAddrFamily;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{Errno, Origin};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_rt::net::{close, connect, stream_recv, stream_send, stream_socket};
    use tairix_test_netstack_wire as wire;
    use tairix_test_tcpecho::{fill_chunk, verify_chunk, PASS_MARKER, TRANSFER_BYTES};

    /// The client's async delivery-port endpoint id: an app-local,
    /// unrestricted well-known value (not a reserved kernel id), so binding it
    /// needs no capability. The stack sends this socket's stream events here.
    const DELIVER_PORT: u64 = 0x_7463_7000;

    /// Largest delivery message the port must hold: a stream event header plus
    /// a maximum-size data payload.
    const DELIVER_MAX_PAYLOAD: usize = SocketStreamEvent::MAX_WIRE_LEN;

    /// Delivery-port mailbox depth. Generous headroom so a burst of echoed
    /// data events queues rather than back-pressuring the stack mid-transfer;
    /// the stack's own receive window bounds the true in-flight volume.
    const DELIVER_CAPACITY: usize = 64;

    /// Bytes offered per [`stream_send`] call. The stack accepts into the
    /// connection's send buffer; a chunk this size keeps each call's marshalled
    /// request modest while the whole transfer fits the send window.
    const SEND_CHUNK: usize = 4096;

    /// How many times to retry [`connect`] while the interface is still
    /// coming up at boot (the virtio-net driver may not be bound to the stack
    /// yet, so the stack has no egress and answers `NetworkUnreachable`).
    const CONNECT_ATTEMPTS: u32 = 400;

    /// One-shot park between connect retries and between send back-pressure
    /// retries (a tickless timed wait, never a busy spin).
    const RETRY_PARK_NANOS: u64 = 25_000_000;

    /// Wait-set token for the delivery port (the client waits on exactly one
    /// source, so any non-zero token identifies it).
    const DELIVER_TOKEN: u64 = 1;

    /// One park slice while blocking for the next stream event: the client
    /// gives the CPU up and is woken when the stack posts an event or this
    /// one-shot timer elapses (whichever first) — never a busy poll.
    const EVENT_PARK_NANOS: u64 = 200_000_000;

    /// Overall deadline for one blocking-receive phase (awaiting the
    /// handshake, or draining the whole echoed transfer). Generous for the
    /// loss-recovered transfer on QEMU TCG, but bounded so a genuinely dead
    /// connection fails with a reason rather than only via the vertical's
    /// outer timeout.
    const PHASE_TIMEOUT_NANOS: u64 = 120_000_000_000;

    /// The peer's stream endpoint: its IPv6 link-local address (formed from
    /// the shared peer interface identifier) and the shared TCP port.
    fn peer_addr() -> SocketAddr {
        SocketAddr {
            family: NetAddrFamily::V6,
            addr: wire::link_local(wire::PEER_IID).octets(),
            port: wire::PEER_TCP_PORT,
        }
    }

    /// Park for `nanos` on a process-local wait-set with no registered
    /// sources: it simply parks the task until the one-shot timeout elapses,
    /// so the retry cadence gives the CPU up rather than spinning. Best-effort
    /// — a refused wait-set (which should not happen) degrades to an immediate
    /// return, and the bounded retry loop above still terminates.
    fn park(set: u64, nanos: u64) {
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(set, nanos, &mut token);
    }

    /// Terminal failure: report the reason on standard error, then park
    /// forever off the run queue. This program must **never exit** on a
    /// failure — the consuming vertical arms its PASS chain on this process's
    /// audited `exit`, so failing loudly means parking until the harness times
    /// the run out with the reason in the transcript. The spin fallback runs
    /// only if even the park is refused.
    fn fail(reason: &str) -> ! {
        write_stderr_line(reason);
        let _ = tairix_rt::park_forever();
        loop {
            core::hint::spin_loop();
        }
    }

    /// Open the socket and connect to the peer, retrying through the boot-time
    /// window in which the stack has no bound interface yet. Returns the
    /// connected socket handle.
    fn open_and_connect(set: u64) -> Result<SocketId, &'static str> {
        let socket = stream_socket(NetAddrFamily::V6, DELIVER_PORT)
            .map_err(|_| "tcpecho: stream_socket refused")?;
        let peer = peer_addr();
        for _ in 0..CONNECT_ATTEMPTS {
            match connect(socket, peer) {
                Ok(()) => return Ok(socket),
                // The interface is not up yet (driver still binding): wait and
                // retry. Any other refusal is a real, non-transient failure.
                Err(Errno::NetworkUnreachable) => park(set, RETRY_PARK_NANOS),
                Err(_) => return Err("tcpecho: connect refused"),
            }
        }
        Err("tcpecho: network never came up (connect kept failing)")
    }

    /// Park on the delivery-port wait-set for the next event, failing if the
    /// deadline passes. `ipc_recv` is non-blocking — an empty port is the
    /// retryable `WouldBlock` — so a caller draining events matches
    /// `Err(Errno::WouldBlock)` and calls this to give the CPU up until the
    /// stack posts an event (or the one-shot timer elapses), never spinning.
    fn park_for_event(set: u64, deadline_ns: u64) -> Result<(), &'static str> {
        if tairix_rt::clock_get() >= deadline_ns {
            return Err("tcpecho: timed out waiting for a stream event");
        }
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(set, EVENT_PARK_NANOS, &mut token);
        Ok(())
    }

    /// Block for events until the connection is established, returning the
    /// kernel-attested origin of the stack (captured from the first event so
    /// every later event can be required to match it — the delivery port is
    /// otherwise an unauthenticated inbox).
    fn await_connected(set: u64, socket: SocketId, buf: &mut [u8]) -> Result<Origin, &'static str> {
        let deadline = tairix_rt::clock_get().saturating_add(PHASE_TIMEOUT_NANOS);
        loop {
            // The `event` borrow of `buf` never escapes a match arm (each arm
            // returns an owned `Origin`/`&'static str` or parks and re-loops),
            // so the next iteration is free to re-borrow `buf`.
            match stream_recv(DELIVER_PORT, buf) {
                Ok((SocketStreamEvent::Connected { socket: s }, origin)) if s == socket => {
                    return Ok(origin)
                }
                Ok((SocketStreamEvent::Connected { .. }, _)) => {
                    return Err("tcpecho: Connected for a foreign socket")
                }
                Ok((SocketStreamEvent::Data { .. }, _)) => {
                    return Err("tcpecho: data before the connection was established")
                }
                Ok((SocketStreamEvent::Closed { .. }, _)) => {
                    return Err("tcpecho: connection closed before it established")
                }
                Ok((SocketStreamEvent::Accepted { .. }, _)) => {
                    return Err("tcpecho: unexpected Accepted event on a client socket")
                }
                Err(Errno::WouldBlock) => park_for_event(set, deadline)?,
                Err(_) => return Err("tcpecho: event receive failed"),
            }
        }
    }

    /// Stream the whole deterministic transfer to the peer, retrying a
    /// momentarily-full send buffer with a one-shot park rather than a spin.
    fn send_all(socket: SocketId, set: u64) -> Result<(), &'static str> {
        let mut chunk = [0u8; SEND_CHUNK];
        let mut sent_bytes = 0usize;
        while sent_bytes < TRANSFER_BYTES {
            let len = core::cmp::min(SEND_CHUNK, TRANSFER_BYTES - sent_bytes);
            fill_chunk(sent_bytes, &mut chunk[..len]);
            match stream_send(socket, &chunk[..len]) {
                Ok(0) => park(set, RETRY_PARK_NANOS),
                Ok(accepted) => sent_bytes += accepted as usize,
                Err(_) => return Err("tcpecho: stream_send refused"),
            }
        }
        Ok(())
    }

    /// Receive and verify the whole echoed transfer, requiring every event to
    /// come from the same stack origin `await_connected` captured and every
    /// byte to match the deterministic stream at its absolute offset.
    fn receive_and_verify(
        set: u64,
        socket: SocketId,
        stack: Origin,
        buf: &mut [u8],
    ) -> Result<(), &'static str> {
        let deadline = tairix_rt::clock_get().saturating_add(PHASE_TIMEOUT_NANOS);
        let mut received = 0usize;
        while received < TRANSFER_BYTES {
            // As in `await_connected`, the `event` borrow of `buf` is confined
            // to the match arm; the `WouldBlock` arm parks and re-loops.
            match stream_recv(DELIVER_PORT, buf) {
                Ok((event, origin)) => {
                    if origin != stack {
                        return Err("tcpecho: event from an unexpected origin");
                    }
                    match event {
                        SocketStreamEvent::Data { socket: s, payload } if s == socket => {
                            if verify_chunk(received, payload).is_err() {
                                return Err("tcpecho: echoed byte did not match the sent stream");
                            }
                            received += payload.len();
                        }
                        SocketStreamEvent::Data { .. } => {
                            return Err("tcpecho: data for a foreign socket")
                        }
                        SocketStreamEvent::Closed { reason, .. } => {
                            return Err(match reason {
                                StreamCloseReason::PeerClosed => {
                                    "tcpecho: peer closed before echoing the whole transfer"
                                }
                                _ => "tcpecho: connection reset before the transfer completed",
                            })
                        }
                        SocketStreamEvent::Connected { .. } => {
                            return Err("tcpecho: a second Connected event")
                        }
                        SocketStreamEvent::Accepted { .. } => {
                            return Err("tcpecho: unexpected Accepted event on a client socket")
                        }
                    }
                }
                Err(Errno::WouldBlock) => park_for_event(set, deadline)?,
                Err(_) => return Err("tcpecho: event receive failed"),
            }
        }
        Ok(())
    }

    /// Run the client, returning `Ok` only when the whole transfer was echoed
    /// back and verified byte-for-byte.
    fn run() -> Result<(), &'static str> {
        // The delivery port is an ordinary process resource (no capability);
        // bind it before opening the socket so no early event is lost.
        if tairix_rt::port_bind(DELIVER_PORT, DELIVER_MAX_PAYLOAD, DELIVER_CAPACITY) < 0 {
            return Err("tcpecho: delivery port bind refused");
        }
        let Ok(set) = u64::try_from(tairix_rt::waitset_create()) else {
            return Err("tcpecho: wait-set create refused");
        };
        // Register the delivery port so a blocking receive parks on the
        // wait-set until the stack posts an event, rather than polling.
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            DELIVER_PORT,
            DELIVER_TOKEN,
        ) != 0
        {
            return Err("tcpecho: wait-set port registration refused");
        }

        let socket = open_and_connect(set)?;
        let mut buf = [0u8; DELIVER_MAX_PAYLOAD];
        let stack = await_connected(set, socket, &mut buf)?;
        send_all(socket, set)?;
        receive_and_verify(set, socket, stack, &mut buf)?;
        // Orderly close of our half; the peer's teardown follows. A refused
        // close is not a data-integrity failure — the transfer already
        // verified — so it is reported but does not fail the run.
        let _ = close(socket);
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
                    fail("tcpecho: report write failed");
                }
                0
            }
            Err(reason) => fail(reason),
        }
    }

    /// Render the `TCPECHO PASS <bytes> bytes` report line into `buf`,
    /// allocation-free, returning the written text.
    fn format_pass(buf: &mut [u8; 64]) -> &str {
        use core::fmt::Write as _;

        let mut w = Cursor { buf, len: 0 };
        // Bounded, well-formed input — the marker plus a small integer — so a
        // formatting overflow is impossible; if it somehow occurred the text
        // is simply the marker, still a valid PASS line.
        let _ = writeln!(w, "{PASS_MARKER} {TRANSFER_BYTES} bytes");
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
