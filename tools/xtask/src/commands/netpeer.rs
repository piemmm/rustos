//! Host-side link peer for the QEMU netstack verticals
//! (`plans/NETWORK.md` N3c).
//!
//! The guest's `tairix-netstack` engine pumps a live virtio-net device
//! whose QEMU `dgram` netdev forwards every guest frame as one raw
//! Ethernet datagram to a unix socket the harness binds. This module is
//! the other end of that wire: the *same* pure `lib/net` protocol
//! engine, configured from the shared `tairix-test-netstack-wire`
//! topology, run on a plain host thread with real time.
//!
//! The peer drives the *inbound* half of the vertical's choreography:
//! it resolves the guest itself (a Neighbour Solicitation for its
//! link-local address, emitted by the engine en route to the echo) and
//! pings it over its EUI-64 link-local, retrying until the reply
//! arrives; it answers the guest's own neighbour queries and echo
//! requests as any live host would. The guest exits only after
//! observing the inbound request and its own outbound reply, and
//! [`NetPeer::stop_and_join`] fails the run if the peer's campaign never
//! completed — so neither side can pass alone.

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, NetOffloads};
use tairix_abi::Duration64;
use tairix_net::addr::IpAddr;
use tairix_net::checksum::Pseudo;
use tairix_net::iface::eui64_interface_id;
use tairix_net::stack::{Stack, StackConfig, StackEvent, StackOutput, TxFrame};
use tairix_net::tcp::conn::{Tcb, TcpConfig};
use tairix_net::tcp::TcpSegment;
use tairix_test_netstack_wire as wire;

/// Blocking-receive slice per loop pass; the wire is otherwise idle, so
/// this also paces the peer's timer advancement.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);

/// Interval between campaign-ping retransmissions. The guest may still
/// be booting or mid-DAD when the campaign starts, so unanswered pings
/// are the expected early state, never an error.
const RESEND_INTERVAL: Duration = Duration::from_millis(500);

/// Largest Ethernet frame the wire carries (MTU + header, with slack).
const MAX_FRAME: usize = 2048;

/// Deterministic IPv4-identification seed for the peer's engine (the
/// vertical needs no unpredictability; a fixed seed keeps runs
/// replayable).
const IPV4_IDENT_SEED: u16 = 0x7EE7;

/// A running host-side peer thread bound to one vertical's wire.
pub struct NetPeer {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<(), String>>,
}

impl NetPeer {
    /// Bind `peer_sock` and start the **ICMP-campaign** peer thread (the
    /// two-process ICMP verticals). Call *before* launching QEMU so no early
    /// guest frame is lost; stale socket files from an earlier run are
    /// removed first (QEMU refuses to bind an existing path).
    pub fn spawn(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_peer)
    }

    /// Bind `peer_sock` and start the **passive ICMP echo-responder** peer
    /// thread (the N8b-2b-β `ping` vertical): it answers the guest's
    /// neighbour resolution and every `ICMPv6` echo request the guest `ping`
    /// tool sends over the shared IPv6 link-local wire, and reports success
    /// once it has served at least one such request. Unlike [`Self::spawn`]
    /// it runs **no** outbound campaign of its own — the guest is the active
    /// pinger — so its verdict ([`Self::stop_and_join`]) is `Ok` only when
    /// the guest actually reached it and it answered.
    pub fn spawn_ping_responder(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_ping_responder)
    }

    /// Bind `peer_sock` and start the **passive TCP echo-server** peer
    /// thread (the N5c stream vertical): it answers the guest client's
    /// neighbour resolution, accepts one connection on
    /// [`wire::PEER_TCP_PORT`], echoes every byte it receives back to the
    /// guest, and — crucially — injects deterministic frame loss so the
    /// connection exercises RFC 9293 retransmission both ways. Its verdict
    /// ([`Self::stop_and_join`]) is `Ok` only once it has received and
    /// echoed the whole [`wire::STREAM_TRANSFER_BYTES`] transfer.
    pub fn spawn_tcp_echo(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_tcp_echo_peer)
    }

    /// Bind `peer_sock` and start the **active TCP client** peer thread (the
    /// N6b-2-β-2 listener vertical): it resolves the guest, connects to the
    /// guest `tcpserve` server on [`wire::GUEST_TCP_PORT`], streams the whole
    /// [`wire::STREAM_TRANSFER_BYTES`] deterministic run, verifies the guest
    /// echoes every byte back in order, and — crucially — injects bounded
    /// frame loss so the connection exercises RFC 9293 retransmission both
    /// ways. Its verdict ([`Self::stop_and_join`]) is `Ok` only once it has
    /// received and verified the whole echoed transfer and closed cleanly.
    pub fn spawn_tcp_connect(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_tcp_connect_peer)
    }

    /// Shared socket bring-up + thread spawn for both peer roles: remove any
    /// stale socket files, bind the peer's datagram socket, and run `body`
    /// on a host thread until [`Self::stop_and_join`] signals it. Factored so
    /// the ICMP and TCP peers share one binding path (never two copies).
    fn spawn_with(
        qemu_sock: &Path,
        peer_sock: &Path,
        body: fn(&UnixDatagram, &PathBuf, &AtomicBool) -> Result<(), String>,
    ) -> Result<Self, String> {
        for path in [qemu_sock, peer_sock] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "netstack peer: remove stale {}: {e}",
                        path.display()
                    ))
                }
            }
        }
        let socket = UnixDatagram::bind(peer_sock)
            .map_err(|e| format!("netstack peer: bind {}: {e}", peer_sock.display()))?;
        socket
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|e| format!("netstack peer: set read timeout: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let qemu_sock = qemu_sock.to_path_buf();
        let handle = std::thread::spawn(move || body(&socket, &qemu_sock, &thread_stop));
        Ok(Self { stop, handle })
    }

    /// Signal the peer to stop and collect its verdict: `Ok` only if the
    /// peer's own required exchange completed (the ICMP peer's inbound v6
    /// echo campaign, or the TCP peer's full echoed transfer).
    pub fn stop_and_join(self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| "netstack peer: thread panicked".to_string())?
    }
}

/// The peer's event loop: serve the guest reactively, campaign
/// proactively, and report whether the campaign's required replies
/// arrived.
fn run_peer(socket: &UnixDatagram, qemu_sock: &PathBuf, stop: &AtomicBool) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;

    // The guest (the two-process autoload vertical) has no admin-assigned
    // IPv4 and forms its link-local from the *device* MAC (`GUEST_MAC`,
    // modified EUI-64): the peer pings only that link-local and requires
    // only its reply.
    let guest_v6 = wire::link_local(eui64_interface_id(wire::GUEST_MAC));

    let mut reply_v6 = false;
    let mut sequence: u16 = 0;
    let mut next_send = Instant::now();
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (DAD probes, NS retransmits).
        let out = stack.advance(now(start));
        note_replies(&out, guest_v6, &mut reply_v6);
        send_frames(socket, qemu_sock, &out.frames);

        // The campaign: ping the guest over its link-local until its
        // reply arrives. Refusals (our own DAD still pending, a busy
        // resolution queue) are transient; the cadence retries them.
        if !reply_v6 && Instant::now() >= next_send {
            next_send = Instant::now() + RESEND_INTERVAL;
            sequence = sequence.wrapping_add(1);
            campaign_ping(
                &mut stack,
                socket,
                qemu_sock,
                IpAddr::V6(guest_v6),
                sequence,
                now(start),
            );
        }

        // Serve whatever the guest sent (its neighbour queries, its
        // echo requests, the replies to ours).
        match socket.recv(&mut buf) {
            Ok(len) => {
                let out = stack.on_frame(&buf[..len], now(start));
                note_replies(&out, guest_v6, &mut reply_v6);
                send_frames(socket, qemu_sock, &out.frames);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }
    }

    if reply_v6 {
        Ok(())
    } else {
        Err("netstack peer: inbound v6 echo campaign incomplete".to_string())
    }
}

/// The ICMP echo-responder loop (the `ping` vertical): serve the guest
/// reactively — answer its neighbour resolution and every echo request it
/// sends — and report whether at least one echo request was served.
///
/// It is the passive mirror of [`run_peer`]: it never campaigns. The guest
/// `ping` tool is the active side; it resolves the peer's link-local, sends
/// its echo requests, and the peer's engine auto-answers each (the reply is
/// queued in the same [`Stack::on_frame`] output). Serving a request is the
/// proof the guest's outbound echo crossed the two-process boundary and was
/// answered, so the guest's own reply-receipt (its `icmp_seq=` line) and this
/// verdict together mean neither side can pass alone.
fn run_ping_responder(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;

    let mut served = false;
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (the peer's own DAD probes, NS retransmits
        // for any neighbour resolution it initiated while replying).
        let out = stack.advance(now(start));
        note_served(&out, &mut served);
        send_frames(socket, qemu_sock, &out.frames);

        // Serve whatever the guest sent: its neighbour queries for the peer's
        // link-local, and its echo requests (answered in the same output).
        match socket.recv(&mut buf) {
            Ok(len) => {
                let out = stack.on_frame(&buf[..len], now(start));
                note_served(&out, &mut served);
                send_frames(socket, qemu_sock, &out.frames);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }
    }

    if served {
        Ok(())
    } else {
        Err("netstack peer: no inbound echo request was served".to_string())
    }
}

/// Record whether any echo request addressed to the peer was served in
/// `out`'s events (the passive responder's success witness).
fn note_served(out: &StackOutput, served: &mut bool) {
    if out
        .events
        .iter()
        .any(|event| matches!(event, StackEvent::EchoRequestServed { .. }))
    {
        *served = true;
    }
}

/// Queue one campaign echo request toward `dest` and transmit whatever
/// the engine emits for it (the NS resolution, or the echo itself).
fn campaign_ping(
    stack: &mut Stack,
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    dest: IpAddr,
    sequence: u16,
    now: Duration64,
) {
    if let Ok(out) = stack.send_echo_request(
        dest,
        wire::PEER_ECHO_ID,
        sequence,
        wire::PEER_ECHO_PAYLOAD,
        now,
    ) {
        send_frames(socket, qemu_sock, &out.frames);
    }
}

/// Record any campaign echo reply from the guest's targeted link-local
/// in `out`'s events.
fn note_replies(out: &StackOutput, guest_v6: core::net::Ipv6Addr, v6: &mut bool) {
    for event in &out.events {
        if let StackEvent::EchoReply {
            source,
            identifier,
            payload,
            ..
        } = event
        {
            if *identifier == wire::PEER_ECHO_ID && payload.as_slice() == wire::PEER_ECHO_PAYLOAD {
                if let IpAddr::V6(a) = source {
                    if *a == guest_v6 {
                        *v6 = true;
                    }
                }
            }
        }
    }
}

/// Transmit engine output onto the wire, one frame per datagram. Send
/// errors are tolerated: before QEMU binds its end there is no receiver
/// (the engine's retransmission machinery recovers the loss), and after
/// the guest exits the wire is torn down under us.
fn send_frames(socket: &UnixDatagram, qemu_sock: &PathBuf, frames: &[TxFrame]) {
    for frame in frames {
        // The host peer speaks the raw wire; a live device would consume
        // the transmit-offload metadata, so it is ignored here.
        let _ = socket.send_to(&frame.bytes, qemu_sock);
    }
}

// --- Passive TCP echo server (N5c stream vertical) ---------------------

/// One inbound frame in every [`LOSS_EVERY`] is dropped (never delivered to
/// the peer engine) once the connection is established, up to [`LOSS_DROPS`]
/// total, so the guest client's stream must survive real packet loss.
const LOSS_EVERY: u64 = 5;

/// Total number of inbound frames the echo peer deliberately drops across
/// the connection's life. Bounded so a *specific* segment is never dropped
/// forever (forward progress is guaranteed): each retransmission is a fresh
/// frame, and once the budget is spent every frame is delivered.
const LOSS_DROPS: u32 = 8;

/// Build the host peer's `lib/net` stack (from the shared device facts and
/// wire topology) and the two link-local addresses both TCP peers use — the
/// guest's EUI-64 address and the peer's own — so the echo-server and the
/// active-client loops share one engine-construction definition, never two.
fn peer_stack(start: Instant) -> Result<(Stack, core::net::Ipv6Addr, core::net::Ipv6Addr), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
    };
    let now0 =
        Duration64::from_nanos(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    let stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED),
        now0,
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;
    let guest_v6 = wire::link_local(eui64_interface_id(wire::GUEST_MAC));
    let peer_v6 = wire::link_local(wire::PEER_IID);
    Ok((stack, guest_v6, peer_v6))
}

/// Drain `tcb`'s in-order received bytes into `scratch`, verifying each run
/// against the deterministic stream at the running `verified` offset.
/// Returns the new verified count and whether any byte mismatched — the one
/// echo-verification loop the active client uses (a corrupted, reordered, or
/// duplicated byte is caught at its first wrong position; fail closed).
fn drain_and_verify(tcb: &mut Tcb, scratch: &mut [u8], mut verified: usize) -> (usize, bool) {
    let mut mismatch = false;
    loop {
        let n = tcb.recv(scratch);
        if n == 0 {
            break;
        }
        if wire::verify_chunk(verified, &scratch[..n]).is_err() {
            mismatch = true;
        }
        verified += n;
    }
    (verified, mismatch)
}

/// The peer's TCP echo-server loop: answer the guest client's neighbour
/// resolution and connection, echo every received byte back, inject
/// bounded frame loss to exercise retransmission, and report whether the
/// whole [`wire::STREAM_TRANSFER_BYTES`] transfer was received and echoed.
///
/// Loss is injected on the **inbound** path only, and only after the
/// handshake has established: dropping an inbound frame drops either a
/// guest data segment (forcing the *guest* to retransmit) or a guest ACK of
/// the peer's echo (forcing the *peer* to retransmit), so both directions'
/// recovery is exercised without ever dropping the neighbour-resolution or
/// SYN frames the connection cannot open without.
fn run_tcp_echo_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
) -> Result<(), String> {
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let (mut stack, guest_v6, peer_v6) = peer_stack(start)?;

    // Passive open: the listener learns the guest's ephemeral port from the
    // SYN. The initial sequence number is a fixed value — the vertical needs
    // no unpredictability and a fixed ISN keeps runs replayable (the live
    // stack draws a real CSPRNG ISN; this is only the far end of the wire).
    let mut tcb = Tcb::listen(TcpConfig::default(), wire::PEER_TCP_PORT, 0, 0x5EED_0000);

    let target = wire::STREAM_TRANSFER_BYTES;
    let mut received_total: usize = 0;
    let mut echoed_total: usize = 0;
    let mut pending: Vec<u8> = Vec::new();
    let mut closed_send = false;
    let mut inbound_seen: u64 = 0;
    let mut loss_budget = LOSS_DROPS;
    let mut buf = [0u8; MAX_FRAME];
    let mut scratch = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine + connection output (DAD, ND retransmits, RTO).
        let out = stack.advance(now(start));
        send_frames(socket, qemu_sock, &out.frames);
        tcb.advance(now(start));
        drive_tcp_egress(
            &mut tcb,
            &mut stack,
            socket,
            qemu_sock,
            guest_v6,
            now(start),
        );

        match socket.recv(&mut buf) {
            Ok(len) => {
                // Inject bounded loss once established: drop this inbound
                // frame rather than deliver it, forcing a retransmission.
                let drop =
                    tcb.is_established() && loss_budget > 0 && inbound_seen % LOSS_EVERY == 0;
                inbound_seen += 1;
                if drop {
                    loss_budget -= 1;
                } else {
                    deliver_inbound_frame(
                        &mut stack,
                        &mut tcb,
                        &buf[..len],
                        peer_v6,
                        socket,
                        qemu_sock,
                        now(start),
                    );
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }

        // Drain received stream bytes and queue them straight back (echo).
        loop {
            let n = tcb.recv(&mut scratch);
            if n == 0 {
                break;
            }
            received_total += n;
            pending.extend_from_slice(&scratch[..n]);
        }
        if !pending.is_empty() {
            if let Ok(accepted) = tcb.send(&pending) {
                echoed_total += accepted;
                pending.drain(..accepted);
            }
        }
        // Once the whole transfer is received and every echoed byte is
        // queued, close our send side (FIN) so the connection tears down
        // cleanly after the client's own close.
        if !closed_send && received_total >= target && pending.is_empty() {
            let _ = tcb.close(now(start));
            closed_send = true;
        }
        drive_tcp_egress(
            &mut tcb,
            &mut stack,
            socket,
            qemu_sock,
            guest_v6,
            now(start),
        );
    }

    if echoed_total >= target {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: TCP echo incomplete: received {received_total}, echoed {echoed_total} of {target} bytes"
        ))
    }
}

// --- Active TCP client (N6b-2-β-2 listener vertical) -------------------

/// Ephemeral local TCP port the client peer opens its connection from. A
/// fixed value keeps runs replayable; it is unprivileged and never collides
/// with the guest server's well-known [`wire::GUEST_TCP_PORT`].
const CLIENT_LOCAL_PORT: u16 = 0xC000;

/// The client peer's initial send sequence number. Fixed — the vertical needs
/// no unpredictability and a fixed ISN keeps runs replayable (the live guest
/// stack draws a real CSPRNG ISN; this is only the far end of the wire).
const CLIENT_ISS: u32 = 0x1234_0000;

/// Bytes the client offers to its TCB per pass while the send buffer has
/// room; the TCB's send buffer and the connection's window bound the true
/// in-flight volume.
const CLIENT_SEND_CHUNK: usize = 4096;

/// Fallback local MSS if no egress route is resolvable when the client TCB
/// is built: the shared wire MTU (1500) less the IPv6 (40) and TCP (20) fixed
/// headers, so a full segment plus its options still fits the link. Normally
/// `Stack::tcp_local_mss` supplies this from the discovered link MTU; this is
/// only the safe floor.
const V6_SAFE_MSS: u16 = 1500 - 40 - 20;

/// The client peer's loop: connect to the guest `tcpserve` server, stream the
/// whole deterministic transfer, verify the guest echoes every byte back in
/// order, inject bounded frame loss to exercise retransmission, and report
/// whether the whole [`wire::STREAM_TRANSFER_BYTES`] transfer was echoed back
/// and verified.
///
/// Loss is injected on the **inbound** path only, and only after the
/// handshake has established: dropping an inbound frame drops either a guest
/// echo-data segment (forcing the *guest* to retransmit) or a guest ACK of
/// our data (forcing the *client* to retransmit), so both directions'
/// recovery is exercised without ever dropping the neighbour-resolution or
/// SYN-ACK frames the connection cannot open without.
fn run_tcp_connect_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
) -> Result<(), String> {
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let (mut stack, guest_v6, peer_v6) = peer_stack(start)?;

    // Clamp the connection's local MSS to what the v6 link's MTU can carry
    // (link MTU − IPv6 − TCP headers), exactly as the guest netstack does for
    // an active open (`Stack::tcp_local_mss`, RFC 6691). Without this the TCB
    // advertises — and tries to send — the IPv4-oriented default 1460, whose
    // full segments overflow the 1500-byte v6 MTU and are refused by
    // `send_tcp`; the client advertising the clamped value also bounds the
    // guest's echo TX (each side sends `min(peer_advertised, own local_mss)`),
    // so this one seeding makes both directions fit. A fallback covers the
    // (not-expected) case where no egress is resolvable yet.
    let local_mss = stack
        .tcp_local_mss(IpAddr::V6(guest_v6), now(start))
        .unwrap_or(V6_SAFE_MSS);
    let config = TcpConfig {
        local_mss,
        ..TcpConfig::default()
    };

    // Active open toward the guest server's well-known port. The first
    // outbound SYN parks on neighbour resolution; the engine emits the NS and
    // retransmits the SYN until the guest answers, so no wall-clock race with
    // the guest's NIC autoload is possible.
    let mut tcb = Tcb::connect(
        config,
        CLIENT_LOCAL_PORT,
        wire::GUEST_TCP_PORT,
        CLIENT_ISS,
        now(start),
    );

    let target = wire::STREAM_TRANSFER_BYTES;
    let mut sent: usize = 0;
    let mut verified: usize = 0;
    let mut closed = false;
    let mut mismatch = false;
    let mut inbound_seen: u64 = 0;
    let mut loss_budget = LOSS_DROPS;
    let mut buf = [0u8; MAX_FRAME];
    let mut scratch = [0u8; MAX_FRAME];
    let mut chunk = [0u8; CLIENT_SEND_CHUNK];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine + connection output (DAD, ND retransmits, RTO).
        let out = stack.advance(now(start));
        send_frames(socket, qemu_sock, &out.frames);
        tcb.advance(now(start));

        // Feed more of the deterministic stream into the send buffer once the
        // connection is established (and there is still data to send).
        if tcb.is_established() && sent < target {
            let len = (target - sent).min(CLIENT_SEND_CHUNK);
            wire::fill_chunk(sent, &mut chunk[..len]);
            if let Ok(accepted) = tcb.send(&chunk[..len]) {
                sent += accepted;
            }
        }
        drive_tcp_egress(
            &mut tcb,
            &mut stack,
            socket,
            qemu_sock,
            guest_v6,
            now(start),
        );

        match socket.recv(&mut buf) {
            Ok(len) => {
                // Inject bounded loss once established: drop this inbound
                // frame rather than deliver it, forcing a retransmission.
                let drop =
                    tcb.is_established() && loss_budget > 0 && inbound_seen % LOSS_EVERY == 0;
                inbound_seen += 1;
                if drop {
                    loss_budget -= 1;
                } else {
                    deliver_inbound_frame(
                        &mut stack,
                        &mut tcb,
                        &buf[..len],
                        peer_v6,
                        socket,
                        qemu_sock,
                        now(start),
                    );
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }

        // Drain the echoed bytes and verify each against the deterministic
        // stream (fail closed on the first mismatch).
        let (new_verified, any_mismatch) = drain_and_verify(&mut tcb, &mut scratch, verified);
        verified = new_verified;
        mismatch |= any_mismatch;

        // Once the whole transfer has been sent and the whole echo verified,
        // close our send side (FIN) so the guest server observes PeerClosed
        // and completes its own PASS.
        if !closed && sent >= target && verified >= target {
            let _ = tcb.close(now(start));
            closed = true;
        }
        drive_tcp_egress(
            &mut tcb,
            &mut stack,
            socket,
            qemu_sock,
            guest_v6,
            now(start),
        );
    }

    if mismatch {
        Err(
            "netstack peer: TCP echo verification failed (a byte did not match the stream)"
                .to_string(),
        )
    } else if verified >= target {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: TCP client incomplete: sent {sent}, verified {verified} of {target} echoed bytes"
        ))
    }
}

/// Feed one received frame into the peer stack: answer the guest's neighbour
/// queries and hand any TCP segment addressed to us to the echo connection.
fn deliver_inbound_frame(
    stack: &mut Stack,
    tcb: &mut Tcb,
    frame: &[u8],
    peer_v6: core::net::Ipv6Addr,
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    now: Duration64,
) {
    let out = stack.on_frame(frame, now);
    send_frames(socket, qemu_sock, &out.frames);
    for event in &out.events {
        if let StackEvent::TcpSegment {
            source,
            destination,
            segment,
        } = event
        {
            if let (IpAddr::V6(s), IpAddr::V6(d)) = (source, destination) {
                if *d == peer_v6 {
                    let pseudo = Pseudo::V6 {
                        source: *s,
                        destination: *d,
                    };
                    if let Some(seg) = TcpSegment::parse(pseudo, segment) {
                        tcb.on_segment(&seg, now);
                    }
                }
            }
        }
    }
}

/// Drain the echo connection's outbound segments, folding each through the
/// peer stack toward the guest's link-local and transmitting the frames.
fn drive_tcp_egress(
    tcb: &mut Tcb,
    stack: &mut Stack,
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    guest_v6: core::net::Ipv6Addr,
    now: Duration64,
) {
    let dest = IpAddr::V6(guest_v6);
    tcb.poll_transmit(now, |seg| {
        match stack.send_tcp(dest, &seg.meta, seg.payload, seg.gso_size, now) {
            Ok(out) => {
                send_frames(socket, qemu_sock, &out.frames);
                true
            }
            // A refused fold (e.g. the segment would overflow the path MTU) must
            // NOT commit the segment: returning `false` leaves it uncommitted so
            // `poll_transmit` re-plans it on the next pass instead of silently
            // advancing the send frontier past bytes that never went on the wire
            // (which would strand the peer behind a permanent hole). With the MSS
            // clamped to the link this never triggers; it fails loud (a stall,
            // not stream corruption) if a path MTU ever surprises us.
            Err(_) => false,
        }
    });
}
