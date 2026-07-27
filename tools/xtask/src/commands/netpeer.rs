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

use tairix_abi::driver::net::{DeviceFacts, LinkState, MacAddress, NetOffloads, MAC_ADDRESS_LEN};
use tairix_abi::Duration64;
use tairix_net::addr::{Ecn, IpAddr, Ipv4Addr};
use tairix_net::checksum::Pseudo;
use tairix_net::dhcp::{self, MessageType};
use tairix_net::eth::{self, BROADCAST, ETHERNET_HEADER_LEN, ETHERTYPE_IPV4};
use tairix_net::iface::{eui64_interface_id, TempAddrSource};
use tairix_net::ipv4::{Ipv4Header, IPV4_HEADER_LEN};
use tairix_net::stack::{Stack, StackConfig, StackEvent, StackOutput, TxFrame};
use tairix_net::tcp::conn::{Tcb, TcpConfig};
use tairix_net::tcp::TcpSegment;
use tairix_net::udp::{self, PROTOCOL_UDP};
use tairix_test_netstack_wire as wire;

/// A fixed temporary-address source for the harness peer stacks: they
/// do not exercise RFC 8981 privacy addresses, so the engine never
/// consults it.
#[derive(Debug)]
struct FixedTempSource;

impl TempAddrSource for FixedTempSource {
    fn fill_random(&mut self, out: &mut [u8]) {
        out.fill(0xA5);
    }
}

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

    /// Bind `peer_sock` and start the **ECN-verifying passive TCP echo-server**
    /// peer thread (the N13 ECN vertical): like [`Self::spawn_tcp_echo`] it
    /// accepts the guest client's connection on [`wire::PEER_TCP_PORT`] and
    /// echoes the whole transfer, but its connection is ECN-capable and it
    /// verifies RFC 3168 Explicit Congestion Notification on the live wire —
    /// the guest's SYN carries ECE+CWR (ECN setup), the guest's data segments
    /// carry ECT(0) in the IP header, and, after the peer echoes ECE for an
    /// injected congestion mark, the guest reduces its window and sets CWR on
    /// a subsequent segment. Its verdict ([`Self::stop_and_join`]) is `Ok`
    /// only once all three were witnessed **and** the whole
    /// [`wire::STREAM_TRANSFER_BYTES`] transfer was received and echoed, so a
    /// stack that silently ignored the toggle (never negotiating, never
    /// marking, never responding) fails the run loud.
    pub fn spawn_tcp_echo_ecn(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_tcp_echo_ecn_peer)
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

    /// Bind `peer_sock` and start the **static-addressing** ICMP-campaign
    /// peer thread (the N9b-3-2-β-2-ii-b `match.node` vertical): the peer
    /// takes its own static address in the shared on-link `/64` and pings the
    /// guest's *static* address (the one the guest holds only if its planted
    /// `network.conf` bound the NIC by `match.node` and assigned it). Its
    /// verdict ([`Self::stop_and_join`]) is `Ok` only once the guest replied
    /// at that static address, so a mis-bind cannot pass.
    pub fn spawn_static(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_static_peer)
    }

    /// Bind `peer_sock` and start the **DHCPv4-server** peer thread (the
    /// DHCP D3 vertical): the peer answers the guest's DHCP `DISCOVER` with
    /// an `OFFER` of [`wire::DHCP_LEASED_V4`] and its `REQUEST` with an
    /// `ACK`, then — from its own [`wire::DHCP_SERVER_V4`] — pings the guest
    /// at the *leased* address. The guest holds that address only if its
    /// DHCP client completed the exchange (its `network.conf` selects
    /// `ipv4.method dhcp` and disables IPv6, so it forms no address itself),
    /// so a broken lease leaves the campaign unanswered and the run fails
    /// loud. Its verdict ([`Self::stop_and_join`]) is `Ok` only once it has
    /// sent both the OFFER and the ACK **and** received the guest's echo
    /// reply at the leased address, so neither the addressing nor the
    /// reachability can pass alone.
    pub fn spawn_dhcp(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_dhcp_peer)
    }

    /// Bind **both** wires' peer sockets and start the **bond-failover**
    /// peer thread (the N9b-3-2-β-2-ii-b-bond vertical): the guest binds
    /// two NICs as the members of one active-backup bond, and this peer
    /// serves *both* wires at once — it replies on whichever wire a frame
    /// arrived on and campaigns on both — so it follows the bond's active
    /// member across the mid-flow failover (`set_link net0 off`) without
    /// knowing which member is live. It pings the bond's *static* address
    /// ([`wire::GUEST_STATIC_V6`]); its verdict ([`Self::stop_and_join`]) is
    /// `Ok` only once the guest replied, and the guest's own witnesses
    /// (`BOND_CONFIG_APPLIED`, `BOND_FAILOVER`, a post-failover
    /// `INBOUND_ECHO_SERVED`) prove the failover was exercised, so neither
    /// side can pass alone.
    pub fn spawn_bond(
        primary_qemu_sock: &Path,
        primary_peer_sock: &Path,
        backup_qemu_sock: &Path,
        backup_peer_sock: &Path,
    ) -> Result<Self, String> {
        let primary = bind_wire(primary_qemu_sock, primary_peer_sock)?;
        let backup = bind_wire(backup_qemu_sock, backup_peer_sock)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || run_bond_peer(&primary, &backup, &thread_stop));
        Ok(Self { stop, handle })
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
        let wire = bind_wire(qemu_sock, peer_sock)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || body(&wire.socket, &wire.qemu_sock, &thread_stop));
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

/// One emulated wire the peer serves: the bound datagram socket QEMU sends
/// this wire's guest frames to, and the QEMU-end socket path the peer sends
/// its frames back to.
struct Wire {
    socket: UnixDatagram,
    qemu_sock: PathBuf,
}

/// Remove any stale socket files, bind the peer end of one wire, and set its
/// read timeout — the one binding path every peer role shares (the single
/// wire of the ICMP/TCP peers and each of the bond peer's two).
fn bind_wire(qemu_sock: &Path, peer_sock: &Path) -> Result<Wire, String> {
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
    Ok(Wire {
        socket,
        qemu_sock: qemu_sock.to_path_buf(),
    })
}

/// The peer's event loop for the **bond-failover** vertical
/// (`plans/NETWORK.md` N9b-3-2-β-2-ii-b-bond): serve the guest's bond over
/// *both* member wires and ping its static address until a reply arrives,
/// surviving the mid-flow failover the harness triggers by dropping the
/// primary member's carrier.
///
/// One `lib/net` engine, one MAC, one static address ([`wire::PEER_STATIC_V6`])
/// in the guest bond's `/64`: the peer is a single host multi-homed onto the
/// two wires that both reach the same guest bond. Because active-backup
/// failover keeps the bond's MAC and address stable and only changes *which
/// member carries the frames*, the peer need not track the active member —
/// it transmits every frame on **both** wires (the down member simply drops
/// it) and services frames arriving on **either**, replying to the wire each
/// arrived on. So before the failover the guest answers over the primary
/// wire and after it over the backup wire, and the peer follows without
/// knowing which is live. Its verdict is `Ok` once the guest's echo reply
/// (to the bond's static address) arrives; the guest's own witnesses prove
/// the failover was exercised.
fn run_bond_peer(primary: &Wire, backup: &Wire, stop: &AtomicBool) -> Result<(), String> {
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
        Box::new(FixedTempSource),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;
    // The peer shares the guest bond's on-link `/64`, so it reaches the
    // bond's static address directly (no router).
    stack
        .add_ipv6_static(wire::PEER_STATIC_V6, wire::STATIC_PREFIX_LEN, now(start))
        .map_err(|e| format!("netstack peer: static address assignment: {e:?}"))?;

    let guest_v6 = wire::GUEST_STATIC_V6;
    let mut reply_v6 = false;
    let mut sequence: u16 = 0;
    let mut next_send = Instant::now();
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (DAD probes, NS retransmits) goes out both
        // wires — a member that is down drops it harmlessly.
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        note_reply(&out, IpAddr::V6(guest_v6), &mut reply_v6);
        send_frames_dual(primary, backup, &out.frames);

        // The campaign: keep pinging the bond's static address on the
        // cadence, on both wires so it reaches whichever member is active.
        // Unlike the single-wire campaign this never stops at the first
        // reply: the whole point is that the guest keeps answering *across*
        // the mid-flow failover, so the guest can witness a served echo
        // after the primary member is dropped. Transient refusals (our own
        // DAD still pending) are simply retried on the next tick.
        if Instant::now() >= next_send {
            next_send = Instant::now() + RESEND_INTERVAL;
            sequence = sequence.wrapping_add(1);
            let mut out = StackOutput::default();
            if stack
                .send_echo_request(
                    IpAddr::V6(guest_v6),
                    wire::PEER_ECHO_ID,
                    sequence,
                    wire::PEER_ECHO_PAYLOAD,
                    now(start),
                    &mut out,
                )
                .is_ok()
            {
                send_frames_dual(primary, backup, &out.frames);
            }
        }

        // Service frames arriving on either wire; a reply staged for one
        // goes back out the wire it arrived on (the active member), so the
        // exchange follows the bond across the failover.
        for wire_end in [primary, backup] {
            match wire_end.socket.recv(&mut buf) {
                Ok(len) => {
                    let mut out = StackOutput::default();
                    stack.on_frame(&buf[..len], now(start), &mut out);
                    note_reply(&out, IpAddr::V6(guest_v6), &mut reply_v6);
                    send_frames(&wire_end.socket, &wire_end.qemu_sock, &out.frames);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
            }
        }
    }

    if reply_v6 {
        Ok(())
    } else {
        Err("netstack peer: bond static-address echo campaign incomplete".to_string())
    }
}

/// Transmit engine output onto **both** bond wires, one frame per datagram
/// per wire. The active member delivers the frames to the guest; the
/// down/backup member's QEMU end drops them (link down or simply the member
/// the bond is not transmitting on), so sending on both reaches the guest
/// regardless of which member is currently active.
fn send_frames_dual(primary: &Wire, backup: &Wire, frames: &[TxFrame]) {
    send_frames(&primary.socket, &primary.qemu_sock, frames);
    send_frames(&backup.socket, &backup.qemu_sock, frames);
}

/// The peer's event loop for the link-local ICMP campaign (the two-process
/// autoload vertical): serve the guest reactively, campaign proactively, and
/// report whether the campaign's required replies arrived.
///
/// The guest has no admin-assigned IPv4 and forms its link-local from the
/// *device* MAC (`GUEST_MAC`, modified EUI-64): the peer pings only that
/// link-local (from its own `PEER_IID` link-local — no extra static address)
/// and requires only its reply.
fn run_peer(socket: &UnixDatagram, qemu_sock: &PathBuf, stop: &AtomicBool) -> Result<(), String> {
    let guest_v6 = wire::link_local(eui64_interface_id(wire::GUEST_MAC));
    run_v6_campaign(socket, qemu_sock, stop, None, guest_v6)
}

/// The peer's event loop for the **static-addressing** vertical
/// (`plans/NETWORK.md` N9b-3-2-β-2-ii-b): give the peer its own static
/// address in the shared on-link `/64` ([`wire::PEER_STATIC_V6`]) and ping
/// the guest's **static** address ([`wire::GUEST_STATIC_V6`]) — the address
/// the guest only holds if its planted `network.conf` bound the NIC by
/// `match.node` and assigned the static address. Requiring the guest's
/// static address (never the link-local it always forms) is what makes a
/// `match.node` mis-bind fail the campaign loud rather than pass anyway.
fn run_static_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
) -> Result<(), String> {
    run_v6_campaign(
        socket,
        qemu_sock,
        stop,
        Some((wire::PEER_STATIC_V6, wire::STATIC_PREFIX_LEN)),
        wire::GUEST_STATIC_V6,
    )
}

/// Shared IPv6 ICMP-campaign event loop: serve the guest reactively and ping
/// `guest_v6` until its echo reply arrives. When `peer_static` is `Some`, the
/// peer additionally assigns itself that static address (DAD runs first),
/// which the engine then prefers as the source for an on-link destination in
/// the same prefix. The one definition both campaign roles share (§2.2), so
/// the link-local and static verticals cannot drift in their choreography.
fn run_v6_campaign(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    peer_static: Option<(core::net::Ipv6Addr, u8)>,
    guest_v6: core::net::Ipv6Addr,
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
        Box::new(FixedTempSource),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;

    // The static-addressing vertical gives the peer an address in the same
    // on-link `/64` as the guest's static address, so it reaches it directly.
    if let Some((addr, prefix)) = peer_static {
        stack
            .add_ipv6_static(addr, prefix, now(start))
            .map_err(|e| format!("netstack peer: static address assignment: {e:?}"))?;
    }

    let mut reply_v6 = false;
    let mut sequence: u16 = 0;
    let mut next_send = Instant::now();
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (DAD probes, NS retransmits).
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        note_reply(&out, IpAddr::V6(guest_v6), &mut reply_v6);
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
                let mut out = StackOutput::default();
                stack.on_frame(&buf[..len], now(start), &mut out);
                note_reply(&out, IpAddr::V6(guest_v6), &mut reply_v6);
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

/// The DHCPv4-server peer loop (the DHCP D3 vertical): lease the guest an
/// IPv4 address, then prove reachability at that leased address.
///
/// The peer takes its own [`wire::DHCP_SERVER_V4`] in the shared `/24` and
/// runs a minimal DHCP server — it answers the guest's `DISCOVER` with an
/// `OFFER` of [`wire::DHCP_LEASED_V4`] and its `REQUEST` with an `ACK`, both
/// broadcast because the client has no address yet — then, once the lease is
/// granted, pings the guest at the leased address until the reply arrives.
/// The guest's planted `network.conf` selects `ipv4.method dhcp` and disables
/// IPv6, so it forms *no* address itself: the leased address is its only
/// reachable one, and a broken lease leaves the campaign unanswered (fail
/// loud). Non-DHCP frames (the guest's ARP for the server, its echo replies)
/// are fed to the peer's own `lib/net` engine, which resolves and answers
/// them; a DHCP request frame is handled by the server and never fed to the
/// engine (the engine holds no DHCP server, and a UDP datagram to an unbound
/// port would draw a spurious port-unreachable toward `0.0.0.0`). Its verdict
/// is `Ok` only once it offered, acked, **and** saw the guest's echo reply at
/// the leased address, so neither the addressing nor the reachability can
/// pass alone.
fn run_dhcp_peer(
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
        Box::new(FixedTempSource),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;
    stack
        .set_ipv4_config(wire::DHCP_SERVER_V4, wire::DHCP_PREFIX_LEN, None)
        .map_err(|e| format!("netstack peer: server address assignment: {e:?}"))?;

    let leased = IpAddr::V4(wire::DHCP_LEASED_V4);
    let mut offered = false;
    let mut acked = false;
    let mut reply = false;
    let mut ident: u16 = 0xD4C0;
    let mut sequence: u16 = 0;
    let mut next_send = Instant::now();
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (ARP retransmits for the leased address the
        // campaign is resolving).
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        note_reply(&out, leased, &mut reply);
        send_frames(socket, qemu_sock, &out.frames);

        // Once the guest has been acknowledged its lease, ping it at the
        // leased address until the reply arrives. Refusals (the server's ARP
        // for the guest still pending) are transient; the cadence retries.
        if acked && !reply && Instant::now() >= next_send {
            next_send = Instant::now() + RESEND_INTERVAL;
            sequence = sequence.wrapping_add(1);
            campaign_ping(&mut stack, socket, qemu_sock, leased, sequence, now(start));
        }

        match socket.recv(&mut buf) {
            Ok(len) => {
                if let Some(request) = dhcp_server::parse_frame(&buf[..len]) {
                    // A DHCP client request: answer it, and never feed it to
                    // the engine (which has no DHCP server).
                    let reply_kind = match request.message_type {
                        MessageType::Discover => Some(MessageType::Offer),
                        MessageType::Request => Some(MessageType::Ack),
                        _ => None,
                    };
                    if let Some(kind) = reply_kind {
                        ident = ident.wrapping_add(1);
                        let frame = dhcp_server::build_frame(kind, &request, ident);
                        let _ = socket.send_to(&frame, qemu_sock);
                        match kind {
                            MessageType::Offer => offered = true,
                            MessageType::Ack => acked = true,
                            _ => {}
                        }
                    }
                } else {
                    // Not DHCP: the guest's ARP for the server, or its echo
                    // reply — let the engine resolve/answer it.
                    let mut out = StackOutput::default();
                    stack.on_frame(&buf[..len], now(start), &mut out);
                    note_reply(&out, leased, &mut reply);
                    send_frames(socket, qemu_sock, &out.frames);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }
    }

    if offered && acked && reply {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: DHCP exchange incomplete (offered={offered}, acked={acked}, reply={reply})"
        ))
    }
}

/// A minimal DHCPv4 **server** codec for the harness peer.
///
/// TAIRiX ships no DHCP server — only the RFC 2131 *client*
/// ([`tairix_net::dhcp`]) — so the server side of the exchange lives here,
/// in the test peer that is its only consumer. It encodes and decodes the
/// *same* wire layout the production client codec exposes (that module's
/// public header offsets, magic cookie, and option registry), never a
/// second copy of the format; the round-trip unit tests additionally parse
/// every reply this module builds back through the real
/// [`tairix_net::dhcp::DhcpReply::parse`], so the two sides cannot drift.
mod dhcp_server {
    use super::{
        dhcp, eth, udp, wire, Ipv4Addr, Ipv4Header, MacAddress, MessageType, Pseudo, BROADCAST,
        ETHERNET_HEADER_LEN, ETHERTYPE_IPV4, IPV4_HEADER_LEN, MAC_ADDRESS_LEN, PROTOCOL_UDP,
    };

    /// The parts of a client `DISCOVER` / `REQUEST` the server acts on.
    pub struct Request {
        /// The DHCP message type (option 53).
        pub message_type: MessageType,
        /// The transaction id to echo in the reply.
        pub xid: u32,
        /// The client hardware address to echo in the reply's `chaddr`.
        pub chaddr: MacAddress,
    }

    /// Decode the DHCP client message an Ethernet frame carries, or `None`
    /// if the frame is not a client→server DHCP request (fail closed). Every
    /// layer is parsed with the production `lib/net` decoders, so the server
    /// accepts exactly the frames a real client emits.
    pub fn parse_frame(frame: &[u8]) -> Option<Request> {
        let eth_frame = eth::EthernetFrame::parse(frame)?;
        if eth_frame.ethertype != ETHERTYPE_IPV4 {
            return None;
        }
        let (ip, _options, payload) = Ipv4Header::parse(eth_frame.payload)?;
        if ip.protocol != PROTOCOL_UDP {
            return None;
        }
        let datagram = udp::UdpDatagram::parse(
            Pseudo::V4 {
                source: ip.source,
                destination: ip.destination,
            },
            payload,
        )?;
        if datagram.destination_port != dhcp::SERVER_PORT
            || datagram.source_port != dhcp::CLIENT_PORT
        {
            return None;
        }
        parse_message(datagram.payload)
    }

    /// Decode a client→server DHCP message (the UDP payload). Total,
    /// bounded, and fail-closed: a truncated header, wrong `op`/`htype`/
    /// `hlen`, missing cookie, or absent message-type option yields `None`.
    pub fn parse_message(payload: &[u8]) -> Option<Request> {
        let header = payload.get(..dhcp::OPTIONS_OFFSET)?;
        if header[0] != dhcp::OP_BOOTREQUEST || header[1] != dhcp::HTYPE_ETHERNET {
            return None;
        }
        if usize::from(header[2]) != MAC_ADDRESS_LEN {
            return None;
        }
        if header[dhcp::BOOTP_HEADER_LEN..dhcp::OPTIONS_OFFSET] != dhcp::MAGIC_COOKIE {
            return None;
        }
        let xid = u32::from_be_bytes([
            header[dhcp::XID_OFFSET],
            header[dhcp::XID_OFFSET + 1],
            header[dhcp::XID_OFFSET + 2],
            header[dhcp::XID_OFFSET + 3],
        ]);
        let mut mac = [0u8; MAC_ADDRESS_LEN];
        mac.copy_from_slice(&header[dhcp::CHADDR_OFFSET..dhcp::CHADDR_OFFSET + MAC_ADDRESS_LEN]);
        let message_type = message_type_of(&payload[dhcp::OPTIONS_OFFSET..])?;
        Some(Request {
            message_type,
            xid,
            chaddr: MacAddress(mac),
        })
    }

    /// Walk the option region for the message-type option (53). Bounded by
    /// the region length; a truncated length/value ends the walk.
    fn message_type_of(region: &[u8]) -> Option<MessageType> {
        let mut i = 0;
        while i < region.len() {
            let code = region[i];
            i += 1;
            match code {
                dhcp::opt::PAD => continue,
                dhcp::opt::END => break,
                _ => {}
            }
            let len = usize::from(*region.get(i)?);
            i += 1;
            let data = region.get(i..i + len)?;
            if code == dhcp::opt::MESSAGE_TYPE {
                if let [value] = data {
                    return MessageType::from_code(*value);
                }
            }
            i += len;
        }
        None
    }

    /// Append one `code`/`data` TLV option at `*pos`, advancing it.
    fn put_option(out: &mut [u8], pos: &mut usize, code: u8, data: &[u8]) {
        out[*pos] = code;
        out[*pos + 1] =
            u8::try_from(data.len()).expect("a DHCP option value never exceeds 255 bytes");
        out[*pos + 2..*pos + 2 + data.len()].copy_from_slice(data);
        *pos += 2 + data.len();
    }

    /// Encode a server→client `OFFER` or `ACK` into `out`. The lease
    /// (address, mask, router, lease time) comes from the shared wire
    /// constants, so the value the peer offers and the value the guest is
    /// pinged at are one and the same. The trailing bytes stay zero (`PAD`),
    /// padding the message to its fixed length.
    fn write_reply(kind: MessageType, request: &Request, out: &mut [u8; dhcp::MAX_MESSAGE_LEN]) {
        out.fill(0);
        out[0] = dhcp::OP_BOOTREPLY;
        out[1] = dhcp::HTYPE_ETHERNET;
        out[2] = dhcp::HLEN_ETHERNET;
        out[dhcp::XID_OFFSET..dhcp::XID_OFFSET + 4].copy_from_slice(&request.xid.to_be_bytes());
        out[dhcp::YIADDR_OFFSET..dhcp::YIADDR_OFFSET + 4]
            .copy_from_slice(&wire::DHCP_LEASED_V4.octets());
        out[dhcp::CHADDR_OFFSET..dhcp::CHADDR_OFFSET + MAC_ADDRESS_LEN]
            .copy_from_slice(&request.chaddr.0);
        out[dhcp::BOOTP_HEADER_LEN..dhcp::OPTIONS_OFFSET].copy_from_slice(&dhcp::MAGIC_COOKIE);
        let mut pos = dhcp::OPTIONS_OFFSET;
        put_option(out, &mut pos, dhcp::opt::MESSAGE_TYPE, &[kind.code()]);
        put_option(
            out,
            &mut pos,
            dhcp::opt::SERVER_ID,
            &wire::DHCP_SERVER_V4.octets(),
        );
        put_option(
            out,
            &mut pos,
            dhcp::opt::SUBNET_MASK,
            &wire::DHCP_SUBNET_MASK.octets(),
        );
        put_option(
            out,
            &mut pos,
            dhcp::opt::ROUTER,
            &wire::DHCP_SERVER_V4.octets(),
        );
        put_option(
            out,
            &mut pos,
            dhcp::opt::LEASE_TIME,
            &wire::DHCP_LEASE_SECS.to_be_bytes(),
        );
        out[pos] = dhcp::opt::END;
    }

    /// Build the full Ethernet frame carrying a server→client reply,
    /// link-layer broadcast (the client has no address yet). Frames the
    /// DHCP message as UDP(67→68)/IPv4(`server`→`255.255.255.255`)/Ethernet
    /// with the production `lib/net` writers, so the guest's client decodes
    /// it exactly as it would a real server's.
    pub fn build_frame(kind: MessageType, request: &Request, ident: u16) -> Vec<u8> {
        let mut message = [0u8; dhcp::MAX_MESSAGE_LEN];
        write_reply(kind, request, &mut message);

        let source = wire::DHCP_SERVER_V4;
        let destination = Ipv4Addr::BROADCAST;
        let mut datagram = vec![0u8; udp::UDP_HEADER_LEN + message.len()];
        udp::write(
            Pseudo::V4 {
                source,
                destination,
            },
            dhcp::SERVER_PORT,
            dhcp::CLIENT_PORT,
            &message,
            &mut datagram,
        )
        .expect("the UDP buffer is sized for the DHCP message");

        let mut header = Ipv4Header::new(source, destination, PROTOCOL_UDP);
        header.identification = ident;
        let mut packet = vec![0u8; IPV4_HEADER_LEN + datagram.len()];
        header
            .write(&mut packet, datagram.len())
            .expect("the IPv4 header fits the sized packet");
        packet[IPV4_HEADER_LEN..].copy_from_slice(&datagram);

        let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
        eth::write_header(
            &mut frame,
            BROADCAST,
            MacAddress(wire::PEER_MAC),
            ETHERTYPE_IPV4,
        )
        .expect("the Ethernet header fits the sized frame");
        frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
        frame
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tairix_net::dhcp::{DhcpReply, MessageSpec};

        /// Build a client message the way the production client would (via
        /// [`tairix_net::dhcp::write_message`]), so the server codec is
        /// tested against real client output, never a hand-rolled fixture.
        fn client_message(message_type: MessageType, xid: u32, chaddr: MacAddress) -> Vec<u8> {
            let spec = MessageSpec {
                message_type,
                xid,
                secs: 0,
                broadcast: true,
                client_addr: Ipv4Addr::UNSPECIFIED,
                chaddr,
                requested_addr: None,
                server_id: None,
            };
            let mut buf = [0u8; dhcp::MAX_MESSAGE_LEN];
            let len = dhcp::write_message(&spec, &mut buf).expect("client message encodes");
            buf[..len].to_vec()
        }

        #[test]
        fn parses_a_real_client_discover() {
            let chaddr = MacAddress(wire::GUEST_MAC);
            let xid = 0x1234_5678;
            let message = client_message(MessageType::Discover, xid, chaddr);
            let request = parse_message(&message).expect("a DISCOVER is a valid request");
            assert_eq!(request.message_type, MessageType::Discover);
            assert_eq!(request.xid, xid);
            assert_eq!(request.chaddr.0, wire::GUEST_MAC);
        }

        /// The OFFER and ACK the server builds round-trip through the real
        /// client parser under the client's own `xid`/`chaddr`, carrying the
        /// leased address, mask, router, server id, and lease time — so the
        /// server and the production client agree on every field.
        #[test]
        fn offer_and_ack_round_trip_through_the_client_parser() {
            let chaddr = MacAddress(wire::GUEST_MAC);
            let xid = 0x0BAD_F00D;
            let request = Request {
                message_type: MessageType::Discover,
                xid,
                chaddr,
            };
            for kind in [MessageType::Offer, MessageType::Ack] {
                let frame = build_frame(kind, &request, 0x4321);
                // Peel the frame back to the DHCP payload with the same
                // production decoders the guest uses.
                let eth_frame = eth::EthernetFrame::parse(&frame).expect("valid Ethernet frame");
                let (ip, _opts, payload) =
                    Ipv4Header::parse(eth_frame.payload).expect("valid IPv4 packet");
                let datagram = udp::UdpDatagram::parse(
                    Pseudo::V4 {
                        source: ip.source,
                        destination: ip.destination,
                    },
                    payload,
                )
                .expect("valid UDP datagram");
                assert_eq!(datagram.source_port, dhcp::SERVER_PORT);
                assert_eq!(datagram.destination_port, dhcp::CLIENT_PORT);
                let reply =
                    DhcpReply::parse(datagram.payload, xid, chaddr).expect("a valid server reply");
                assert_eq!(reply.message_type, kind);
                assert_eq!(reply.your_addr, wire::DHCP_LEASED_V4);
                assert_eq!(reply.server_id, Some(wire::DHCP_SERVER_V4));
                assert_eq!(reply.subnet_mask, Some(wire::DHCP_SUBNET_MASK));
                assert_eq!(reply.routers.first(), Some(wire::DHCP_SERVER_V4));
                assert_eq!(reply.lease_secs, Some(wire::DHCP_LEASE_SECS));
            }
        }

        /// The server never mistakes its own reply for a client request: a
        /// built OFFER frame (a BOOTREPLY, UDP 67→68) is not parsed as a
        /// request, so the peer cannot answer itself in a loop.
        #[test]
        fn a_server_reply_is_not_parsed_as_a_client_request() {
            let request = Request {
                message_type: MessageType::Discover,
                xid: 1,
                chaddr: MacAddress(wire::GUEST_MAC),
            };
            let frame = build_frame(MessageType::Offer, &request, 7);
            assert!(parse_frame(&frame).is_none());
        }

        /// A frame that is not DHCP (wrong UDP port) is rejected (fail
        /// closed), so the server only ever answers genuine client requests.
        #[test]
        fn a_non_dhcp_frame_is_rejected() {
            // A truncated / empty frame is not a DHCP request.
            assert!(parse_frame(&[]).is_none());
            assert!(parse_message(&[0u8; 4]).is_none());
        }
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
        Box::new(FixedTempSource),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;

    let mut served = false;
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (the peer's own DAD probes, NS retransmits
        // for any neighbour resolution it initiated while replying).
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        note_served(&out, &mut served);
        send_frames(socket, qemu_sock, &out.frames);

        // Serve whatever the guest sent: its neighbour queries for the peer's
        // link-local, and its echo requests (answered in the same output).
        match socket.recv(&mut buf) {
            Ok(len) => {
                let mut out = StackOutput::default();
                stack.on_frame(&buf[..len], now(start), &mut out);
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
    let mut out = StackOutput::default();
    if stack
        .send_echo_request(
            dest,
            wire::PEER_ECHO_ID,
            sequence,
            wire::PEER_ECHO_PAYLOAD,
            now,
            &mut out,
        )
        .is_ok()
    {
        send_frames(socket, qemu_sock, &out.frames);
    }
}

/// Record any campaign echo reply from the guest's `expect` address in
/// `out`'s events. The one definition every campaign role uses — the IPv6
/// link-local/static campaigns pass an [`IpAddr::V6`], the DHCP campaign an
/// [`IpAddr::V4`] — so a reply is matched by the same identity/payload check
/// regardless of family (never a per-family copy).
fn note_reply(out: &StackOutput, expect: IpAddr, seen: &mut bool) {
    for event in &out.events {
        if let StackEvent::EchoReply {
            source,
            identifier,
            payload,
            ..
        } = event
        {
            if *identifier == wire::PEER_ECHO_ID
                && payload.as_slice() == wire::PEER_ECHO_PAYLOAD
                && *source == expect
            {
                *seen = true;
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
/// wire topology) and the guest's EUI-64 link-local address the TCP peers
/// send toward, so the echo-server and the active-client loops share one
/// engine-construction definition, never two. (The peer's own link-local is
/// the shared-IID constant `deliver_inbound_frame` derives itself, so it is
/// not returned here.)
fn peer_stack(start: Instant) -> Result<(Stack, core::net::Ipv6Addr), String> {
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
        Box::new(FixedTempSource),
        now0,
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;
    let guest_v6 = wire::link_local(eui64_interface_id(wire::GUEST_MAC));
    Ok((stack, guest_v6))
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
    let (mut stack, guest_v6) = peer_stack(start)?;

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
        flush_engine(&mut stack, socket, qemu_sock, now(start));
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
                        socket,
                        qemu_sock,
                        now(start),
                        // No ECN observation or injection on the plain
                        // echo/client wire: deliver the on-wire codepoint
                        // unchanged.
                        |_, ecn| ecn,
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

// --- ECN-verifying passive TCP echo server (N13 ECN vertical) ----------

/// Number of congestion marks (ECT(0)→CE rewrites on inbound guest data
/// segments) the ECN peer injects once the connection is proven ECN-capable.
/// A small, bounded nudge: each makes the peer echo ECE, and the guest's
/// once-per-window sender reduction sets CWR on the next fresh data segment,
/// so one is enough to witness the response; a few make the observation
/// robust against a mark landing on a segment the guest happens to drop.
const ECN_CE_INJECTIONS: u32 = 3;

/// The ECN peer's live-wire observations, accumulated one guest segment at a
/// time by [`EcnObservations::observe_and_mark`] and checked by
/// [`EcnObservations::negotiated_and_responded`] for the run's verdict.
#[derive(Default)]
struct EcnObservations {
    /// The guest's SYN offered ECN (both ECE and CWR set).
    syn_ecn_setup: bool,
    /// A guest data segment arrived IP-marked ECT(0).
    ect0_data_seen: bool,
    /// How many congestion marks the peer has injected so far.
    ce_injected: u32,
    /// The guest set CWR on a segment sent after a congestion mark.
    cwr_after_ce: bool,
}

impl EcnObservations {
    /// Observe one inbound guest segment (parsed, with the IP ECN codepoint
    /// `ecn` it arrived carrying) and return the codepoint to deliver to the
    /// peer connection. Records the SYN offer, ECT(0) data, and post-mark CWR,
    /// and injects a bounded congestion mark ([`Ecn::Ce`]) on the guest's
    /// ECT(0) data so the peer echoes ECE and the guest's once-per-window
    /// reduction sets CWR on its next fresh data. `ect0_data_seen` only turns
    /// true for a data segment past the handshake, so a mark never touches the
    /// SYN or a pure ACK.
    fn observe_and_mark(&mut self, seg: &TcpSegment<'_>, ecn: Ecn) -> Ecn {
        if seg.flags.syn() && seg.flags.ece() && seg.flags.cwr() {
            self.syn_ecn_setup = true;
        }
        if !seg.payload.is_empty() && ecn == Ecn::Ect0 {
            self.ect0_data_seen = true;
        }
        if self.ce_injected > 0 && seg.flags.cwr() {
            self.cwr_after_ce = true;
        }
        if self.ect0_data_seen && !seg.payload.is_empty() && self.ce_injected < ECN_CE_INJECTIONS {
            self.ce_injected += 1;
            return Ecn::Ce;
        }
        ecn
    }

    /// Whether the guest negotiated ECN, marked its data ECT(0), and responded
    /// to a congestion mark with CWR — the three on-wire facts the vertical
    /// requires.
    fn negotiated_and_responded(&self) -> bool {
        self.syn_ecn_setup && self.ect0_data_seen && self.cwr_after_ce
    }
}

/// The ECN vertical's echo-server loop: an ECN-capable passive open that
/// echoes the whole transfer (like [`run_tcp_echo_peer`]) while verifying RFC
/// 3168 Explicit Congestion Notification on the live wire.
///
/// It observes three things on the guest's segments and injects one stimulus:
///
/// * the guest's SYN carries ECE+CWR — the ECN-setup handshake the guest only
///   sends because its planted `system.conf` turned `net.tcp.ecn` on;
/// * the guest's data segments carry ECT(0) in the IP header — it marks its
///   packets ECN-capable rather than Not-ECT;
/// * after the peer echoes ECE for an injected congestion mark (delivering the
///   guest's data to the peer connection as [`Ecn::Ce`], so the receiver
///   echoes ECE), the guest reduces its window and sets CWR on a subsequent
///   segment — the sender-side congestion response.
///
/// The verdict requires all three **and** the full echoed transfer, so a
/// stack that ignored the toggle — never negotiating, marking, or responding
/// — fails the run loud rather than passing on a plain transfer.
fn run_tcp_echo_ecn_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
) -> Result<(), String> {
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let (mut stack, guest_v6) = peer_stack(start)?;

    // An ECN-capable passive open: the listener negotiates ECN in its
    // SYN-ACK when the guest's SYN offers it, so the connection is ECN-capable
    // end to end. The fixed ISN keeps runs replayable (this is only the far
    // end of the wire; the live guest stack draws a real CSPRNG ISN).
    let mut tcb = Tcb::listen(
        TcpConfig {
            enable_ecn: true,
            ..TcpConfig::default()
        },
        wire::PEER_TCP_PORT,
        0,
        0x5EED_0000,
    );

    let target = wire::STREAM_TRANSFER_BYTES;
    let mut received_total: usize = 0;
    let mut echoed_total: usize = 0;
    let mut pending: Vec<u8> = Vec::new();
    let mut closed_send = false;
    let mut buf = [0u8; MAX_FRAME];
    let mut scratch = [0u8; MAX_FRAME];

    // Live-wire ECN observations, accumulated by the delivery closure below.
    let mut ecn = EcnObservations::default();

    while !stop.load(Ordering::Acquire) {
        flush_engine(&mut stack, socket, qemu_sock, now(start));
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
                deliver_inbound_frame(
                    &mut stack,
                    &mut tcb,
                    &buf[..len],
                    socket,
                    qemu_sock,
                    now(start),
                    |seg, cp| ecn.observe_and_mark(seg, cp),
                );
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
        // Once the whole transfer is received and echoed, close our send side
        // (FIN) so the connection tears down cleanly after the client's close.
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

    if ecn.negotiated_and_responded() && echoed_total >= target {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: ECN verification incomplete: syn_ecn_setup={}, \
             ect0_data_seen={}, cwr_after_ce={} (ce_injected={}), \
             echoed {echoed_total} of {target} bytes",
            ecn.syn_ecn_setup, ecn.ect0_data_seen, ecn.cwr_after_ce, ecn.ce_injected
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
    let (mut stack, guest_v6) = peer_stack(start)?;

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
        flush_engine(&mut stack, socket, qemu_sock, now(start));
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
                        socket,
                        qemu_sock,
                        now(start),
                        // No ECN observation or injection on the plain
                        // echo/client wire: deliver the on-wire codepoint
                        // unchanged.
                        |_, ecn| ecn,
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
///
/// `on_seg` is called for every TCP segment addressed to the peer, with the
/// parsed segment and the on-wire IP ECN codepoint it arrived carrying; it
/// returns the ECN codepoint to deliver to the connection. The plain echo/
/// client peers pass it through unchanged (`|_, ecn| ecn`); the ECN vertical
/// uses it both to *observe* the guest's negotiation/ECT(0)/CWR and to
/// *inject* a congestion mark (returning [`Ecn::Ce`]) so the guest's
/// sender-side response is exercised — one delivery path, no second copy.
fn deliver_inbound_frame(
    stack: &mut Stack,
    tcb: &mut Tcb,
    frame: &[u8],
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    now: Duration64,
    mut on_seg: impl FnMut(&TcpSegment<'_>, Ecn) -> Ecn,
) {
    // The peer's own link-local address — the destination a TCP segment
    // addressed to the connection carries. It is the one the peer stack forms
    // from the shared wire IID, so it is derived here rather than threaded in.
    let peer_v6 = wire::link_local(wire::PEER_IID);
    let mut out = StackOutput::default();
    stack.on_frame(frame, now, &mut out);
    send_frames(socket, qemu_sock, &out.frames);
    for event in &out.events {
        if let StackEvent::TcpSegment {
            source,
            destination,
            ecn,
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
                        let delivered_ecn = on_seg(&seg, *ecn);
                        tcb.on_segment(&seg, delivered_ecn, now);
                    }
                }
            }
        }
    }
}

/// Run the peer stack's due timers and transmit whatever they emit (DAD,
/// ND retransmits, RTO) — the shared "flush engine output" step every
/// peer loop begins with, so no loop repeats the reused-output dance.
fn flush_engine(stack: &mut Stack, socket: &UnixDatagram, qemu_sock: &PathBuf, now: Duration64) {
    let mut out = StackOutput::default();
    stack.advance(now, &mut out);
    send_frames(socket, qemu_sock, &out.frames);
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
        let mut out = StackOutput::default();
        match stack.send_tcp(
            dest,
            &seg.meta,
            seg.payload,
            seg.gso_size,
            seg.ecn,
            now,
            &mut out,
        ) {
            Ok(()) => {
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
