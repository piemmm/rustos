//! Host-side link peer for the QEMU netstack verticals
//! (`plans/NETWORK.md` N3c).
//!
//! The guest's `rustos-netstack` engine pumps a live virtio-net device
//! whose QEMU `dgram` netdev forwards every guest frame as one raw
//! Ethernet datagram to a unix socket the harness binds. This module is
//! the other end of that wire: the *same* pure `lib/net` protocol
//! engine, configured from the shared `rustos-test-netstack-wire`
//! topology, run on a plain host thread with real time.
//!
//! The peer drives the *inbound* half of the vertical's choreography:
//! it resolves the guest itself (ARP for the guest's IPv4 address, a
//! Neighbour Solicitation for its link-local address — both emitted by
//! the engine en route to the echo) and pings it over v4 *and* v6,
//! retrying until each reply arrives; it answers the guest's own
//! neighbour queries and echo requests as any live host would. The
//! guest exits only after observing both inbound requests and both of
//! its own outbound replies, and [`NetPeer::stop_and_join`] fails the
//! run if the peer's campaign never completed — so neither side can
//! pass alone.

use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustos_abi::driver::net::{DeviceFacts, LinkState, MacAddress, NetOffloads};
use rustos_abi::Duration64;
use rustos_net::addr::IpAddr;
use rustos_net::stack::{Stack, StackConfig, StackEvent, StackOutput};
use rustos_test_netstack_wire as wire;

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
    /// Bind `peer_sock` and start the peer thread. Call *before*
    /// launching QEMU so no early guest frame is lost; stale socket
    /// files from an earlier run are removed first (QEMU refuses to
    /// bind an existing path).
    pub fn spawn(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
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
        let handle = std::thread::spawn(move || run_peer(&socket, &qemu_sock, &thread_stop));
        Ok(Self { stop, handle })
    }

    /// Signal the peer to stop and collect its verdict: `Ok` only if
    /// its inbound v4+v6 echo campaign completed.
    pub fn stop_and_join(self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .join()
            .map_err(|_| "netstack peer: thread panicked".to_string())?
    }
}

/// The peer's event loop: serve the guest reactively, campaign
/// proactively, and report whether both campaign replies arrived.
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
    stack
        .set_ipv4_config(wire::PEER_V4, wire::V4_PREFIX, None)
        .map_err(|e| format!("netstack peer: IPv4 configuration: {e:?}"))?;

    let guest_v6 = wire::link_local(wire::GUEST_IID);
    let mut reply_v4 = false;
    let mut reply_v6 = false;
    let mut sequence: u16 = 0;
    let mut next_send = Instant::now();
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (DAD probes, ARP/NS retransmits).
        let out = stack.advance(now(start));
        note_replies(&out, guest_v6, &mut reply_v4, &mut reply_v6);
        send_frames(socket, qemu_sock, &out.frames);

        // The campaign: ping the guest over each family still missing
        // its reply. Refusals (our own DAD still pending, a busy
        // resolution queue) are transient; the cadence retries them.
        if !(reply_v4 && reply_v6) && Instant::now() >= next_send {
            next_send = Instant::now() + RESEND_INTERVAL;
            sequence = sequence.wrapping_add(1);
            if !reply_v4 {
                campaign_ping(
                    &mut stack,
                    socket,
                    qemu_sock,
                    IpAddr::V4(wire::GUEST_V4),
                    sequence,
                    now(start),
                );
            }
            if !reply_v6 {
                campaign_ping(
                    &mut stack,
                    socket,
                    qemu_sock,
                    IpAddr::V6(guest_v6),
                    sequence,
                    now(start),
                );
            }
        }

        // Serve whatever the guest sent (its neighbour queries, its
        // echo requests, the replies to ours).
        match socket.recv(&mut buf) {
            Ok(len) => {
                let out = stack.on_frame(&buf[..len], now(start));
                note_replies(&out, guest_v6, &mut reply_v4, &mut reply_v6);
                send_frames(socket, qemu_sock, &out.frames);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }
    }

    if reply_v4 && reply_v6 {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: inbound echo campaign incomplete (v4 replied: {reply_v4}, v6 replied: {reply_v6})"
        ))
    }
}

/// Queue one campaign echo request toward `dest` and transmit whatever
/// the engine emits for it (the ARP/NS resolution, or the echo itself).
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

/// Record any campaign echo replies in `out`'s events.
fn note_replies(out: &StackOutput, guest_v6: core::net::Ipv6Addr, v4: &mut bool, v6: &mut bool) {
    for event in &out.events {
        if let StackEvent::EchoReply {
            source,
            identifier,
            payload,
            ..
        } = event
        {
            if *identifier == wire::PEER_ECHO_ID && payload.as_slice() == wire::PEER_ECHO_PAYLOAD {
                match source {
                    IpAddr::V4(a) if *a == wire::GUEST_V4 => *v4 = true,
                    IpAddr::V6(a) if *a == guest_v6 => *v6 = true,
                    _ => {}
                }
            }
        }
    }
}

/// Transmit engine output onto the wire, one frame per datagram. Send
/// errors are tolerated: before QEMU binds its end there is no receiver
/// (the engine's retransmission machinery recovers the loss), and after
/// the guest exits the wire is torn down under us.
fn send_frames(socket: &UnixDatagram, qemu_sock: &PathBuf, frames: &[Vec<u8>]) {
    for frame in frames {
        let _ = socket.send_to(frame, qemu_sock);
    }
}
