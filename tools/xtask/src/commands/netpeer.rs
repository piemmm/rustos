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

use tairix_abi::driver::net::{
    DeviceFacts, LinkState, MacAddress, McastFilter, NetOffloads, MAC_ADDRESS_LEN,
};
use tairix_abi::Duration64;
use tairix_net::addr::{Ecn, IpAddr, Ipv4Addr, Ipv6Addr, ALL_NODES};
use tairix_net::checksum::Pseudo;
use tairix_net::dhcp::{self, MessageType};
use tairix_net::dhcpv6::{self, Duid, MessageType as Dhcp6MessageType};
use tairix_net::eth::{
    self, ipv6_multicast_mac, BROADCAST, ETHERNET_HEADER_LEN, ETHERTYPE_IPV4, ETHERTYPE_IPV6,
};
use tairix_net::iface::{eui64_interface_id, TempAddrSource};
use tairix_net::ipv4::{Ipv4Header, IPV4_HEADER_LEN};
use tairix_net::ipv6::{Ipv6Header, IPV6_HEADER_LEN, NEXT_HEADER_ICMPV6};
use tairix_net::nd::{ND_HOP_LIMIT, TYPE_ROUTER_ADVERTISEMENT};
use tairix_net::stack::{Stack, StackConfig, StackEvent, StackOutput, TxFrame};
use tairix_net::tcp::conn::{Tcb, TcpConfig};
use tairix_net::tcp::listen::ListenConfig;
use tairix_net::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegment, TcpSegmentMeta};
use tairix_net::udp::{self, PROTOCOL_UDP};
use tairix_qemu::ObserverGate;
use tairix_test_netstack_wire as wire;

/// A fixed key for the stack's neighbour-cache index, so a run's table layout
/// is reproducible.
const STACK_HASH_KEY: tairix_hash::HashSeed =
    tairix_hash::HashSeed::from_words(0x5354_4143_4B00_0001, 0x5354_4143_4B00_0002);

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

/// Longest a transmit may stall before the frame is dropped.
///
/// A counterpart that has stopped draining saturates the wire's socket within
/// ~90 frames, and an unbounded send then parks the peer *inside* its
/// transmit path — so it never reaches the receive path its whole verdict
/// depends on, and the run expires on its ceiling instead. A real NIC drops
/// from a full transmit ring and the engine's retransmission recovers the
/// loss, which is what this makes true. One receive slice is the bound: a
/// frame the wire will not take within the interval the loop services it in
/// is a dropped frame.
const SEND_TIMEOUT: Duration = RECV_TIMEOUT;

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
    /// The harness-driven completion signal, written by the peer thread and
    /// polled by the QEMU runner. Confirmed the instant the campaign verdict
    /// is first met (e.g. the guest's echo reply arrived) — for a vertical
    /// whose success is proven by this out-of-guest observer the guest is
    /// built *not* to self-exit, so teardown can never precede the
    /// observer's confirming (last-in-chain) event and race it — and
    /// abandoned, with its reason, if the thread ends without confirming.
    /// `stop_and_join` still returns the authoritative verdict.
    gate: Arc<ObserverGate>,
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

    /// Bind `peer_sock` and start the **telnet-server** peer thread (the
    /// `plans/TELNET.md` vertical): it accepts the guest `telnet` client's
    /// connection on [`wire::PEER_TELNET_PORT`] and speaks the *server* half of
    /// RFC 854 — offering `SUPPRESS GO AHEAD` and asking for `TERMINAL TYPE`,
    /// `NAWS` and `LINEMODE`, then driving the RFC 1184 `MODE` and `SLC`
    /// exchange — before greeting the session with [`wire::TELNET_BANNER`] and
    /// echoing the operator's probe line back upper-cased. Its verdict
    /// ([`Self::stop_and_join`]) is `Ok` only once **every** step was
    /// witnessed, so a client that ignored the negotiation, declined LINEMODE,
    /// or never reported its window fails the run loud.
    pub fn spawn_telnet(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_telnet_peer)
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

    /// Bind `peer_sock` and start the **SYN-flood** client peer thread (the
    /// N16b connection-exhaustion vertical): the peer fills the guest
    /// listener's half-open backlog with SYNs it never answers, then opens
    /// one real connection — which the listener can therefore admit only
    /// through a stateless RFC 4987 SYN cookie — streams the whole
    /// [`wire::STREAM_TRANSFER_BYTES`] run, and verifies the guest echoes
    /// every byte back. Its verdict ([`Self::stop_and_join`]) is `Ok` only
    /// once the backlog was provably filled *and* the whole transfer came
    /// back verified, so a run that never engaged the cookie brake cannot
    /// pass on the ordinary accept path.
    pub fn spawn_tcp_flood(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_tcp_flood_peer)
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

    /// Bind `peer_sock` and start the **NTP-server** peer thread (the
    /// `plans/TIMESYNC.md` TS-2 vertical): the peer takes its own
    /// [`wire::PEER_STATIC_V6`] on the guest's on-link `/64` and answers each
    /// of the guest's NTP client requests **twice, spoof first** — a
    /// well-formed reply whose origin timestamp does not echo the request's
    /// nonce and which reports [`wire::NTP_SPOOF_SECS`], then the truthful
    /// reply echoing the nonce and reporting [`wire::NTP_FIXTURE_SECS`].
    ///
    /// That ordering is the discriminator: a guest that accepted the spoof
    /// would set its clock to the wrong instant, and a guest that let the
    /// spoof cancel its outstanding transaction would ignore the truthful
    /// reply and never set the clock. Its verdict
    /// ([`Self::stop_and_join`]) is `Ok` once it has served a request with
    /// both replies; the guest's own audit witness (the applied
    /// `wall_secs=`) is what proves which one it believed, so neither side
    /// passes alone.
    pub fn spawn_ntp(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_ntp_peer)
    }

    /// Bind `peer_sock` and start the **DHCP-server-plus-NTP-server** peer
    /// thread (the `plans/TIMESYNC.md` TS-7 vertical): the peer leases the
    /// guest [`wire::DHCP_LEASED_V4`] exactly as [`Self::spawn_dhcp`] does,
    /// but its OFFER and ACK carry RFC 2132 option 42 naming *itself* as the
    /// network time server — and it then answers the guest's NTP requests
    /// spoof-first, as [`Self::spawn_ntp`] does.
    ///
    /// The guest for this vertical has **no** configured time server, so it
    /// can only find a reachable one by reading that option: its built-in
    /// fallback names public-pool hosts, unreachable on an isolated wire. A
    /// guest that ignored option 42 therefore sets no clock and the run fails
    /// loud. Its verdict ([`Self::stop_and_join`]) is `Ok` once it has
    /// offered, acked, and served a time request with both replies; the
    /// guest's own witness (the applied `wall_secs=`) proves which reply it
    /// believed, so neither side passes alone.
    pub fn spawn_dhcp_time(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_dhcp_time_peer)
    }

    /// Bind `peer_sock` and start the **DHCPv6-server** peer thread (the
    /// DHCP D4c vertical): the peer answers the guest's DHCPv6 `Solicit`
    /// with an `Advertise` and its `Request` with a `Reply`, leasing it
    /// [`wire::DHCP6_LEASED_V6`] (RFC 8415 stateful IA_NA). Because DHCPv6
    /// grants no on-link prefix, the peer also acts as the on-link router:
    /// it periodically emits a Router Advertisement naming
    /// [`wire::DHCP6_PREFIX`] on-link (non-autonomous, so the guest forms no
    /// SLAAC address) and itself a default router, so the guest can reach
    /// it. It then — from its own [`wire::DHCP6_SERVER_V6`] in that `/64` —
    /// pings the guest at the *leased* address. The guest holds that address
    /// only if its DHCPv6 client completed the exchange (its `network.conf`
    /// selects `ipv6.method dhcp` and disables IPv4, so it forms no global
    /// address itself), so a broken lease leaves the campaign unanswered and
    /// the run fails loud. Its verdict ([`Self::stop_and_join`]) is `Ok`
    /// only once it has sent both the Advertise and the Reply **and**
    /// received the guest's echo reply at the leased address, so neither the
    /// addressing nor the reachability can pass alone. The IPv6 analogue of
    /// [`Self::spawn_dhcp`].
    pub fn spawn_dhcp6(qemu_sock: &Path, peer_sock: &Path) -> Result<Self, String> {
        Self::spawn_with(qemu_sock, peer_sock, run_dhcp6_peer)
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
        Ok(Self::launch(move |stop, gate| {
            run_bond_peer(&primary, &backup, stop, gate)
        }))
    }

    /// Shared socket bring-up + thread spawn for both peer roles: remove any
    /// stale socket files, bind the peer's datagram socket, and run `body`
    /// on a host thread until [`Self::stop_and_join`] signals it. Factored so
    /// the ICMP and TCP peers share one binding path (never two copies).
    fn spawn_with(
        qemu_sock: &Path,
        peer_sock: &Path,
        body: fn(&UnixDatagram, &PathBuf, &AtomicBool, &ObserverGate) -> Result<(), String>,
    ) -> Result<Self, String> {
        let wire = bind_wire(qemu_sock, peer_sock)?;
        Ok(Self::launch(move |stop, gate| {
            body(&wire.socket, &wire.qemu_sock, stop, gate)
        }))
    }

    /// Run `body` as the observer thread, publishing its verdict through a
    /// fresh gate. The one place a peer thread is launched, so no role can be
    /// left with its abandonment unreported.
    fn launch(
        body: impl FnOnce(&AtomicBool, &ObserverGate) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(ObserverGate::default());
        let thread_stop = Arc::clone(&stop);
        let thread_gate = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            let verdict = body(&thread_stop, &thread_gate);
            // An observer that has stopped watching can never confirm, so the
            // runner must hear why now rather than infer a guest fault when
            // the ceiling expires.
            if let Err(reason) = &verdict {
                thread_gate.abandon(reason.clone());
            }
            verdict
        });
        Self { stop, gate, handle }
    }

    /// The harness-driven completion gate, shared with the running peer
    /// thread. The QEMU runner is handed it
    /// ([`tairix_qemu::Spec::with_completion_gate`]) so it can end the run as
    /// soon as this observer reaches a verdict — a pass on the confirming
    /// event, a failure carrying the peer's own reason if it stops watching
    /// without one — for a vertical whose guest is built not to self-exit.
    #[must_use]
    pub fn observer_gate(&self) -> Arc<ObserverGate> {
        Arc::clone(&self.gate)
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

/// Remove any stale socket files, bind the peer end of one wire, and bound
/// both directions of its blocking — the one binding path every peer role
/// shares (the single wire of the ICMP/TCP peers and each of the bond peer's
/// two), so no role can be left with an unbounded transmit.
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
    socket
        .set_write_timeout(Some(SEND_TIMEOUT))
        .map_err(|e| format!("netstack peer: set write timeout: {e}"))?;
    Ok(Wire {
        socket,
        qemu_sock: qemu_sock.to_path_buf(),
    })
}

#[cfg(test)]
mod wire_tests {
    use super::{bind_wire, SEND_TIMEOUT};
    use std::os::unix::net::UnixDatagram;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use tairix_qemu::ReservedSocket;

    /// Longest the timed hand-over may take before the observer is declared
    /// parked. Orders of magnitude above the one receive slice a bounded
    /// transmit costs, so no amount of host load reaches it — an unbounded
    /// transmit, which never returns at all, is the only way past it.
    const PARKED: Duration = Duration::from_secs(30);

    #[test]
    fn a_saturated_wire_drops_the_frame_instead_of_parking_the_observer() {
        let qemu = ReservedSocket::reserve("net0q").expect("reserve the qemu end");
        let peer = ReservedSocket::reserve("net0p").expect("reserve the peer end");
        let wire = bind_wire(qemu.path(), peer.path()).expect("bind the wire");
        assert_eq!(
            wire.socket
                .write_timeout()
                .expect("read the wire's transmit bound"),
            Some(SEND_TIMEOUT),
            "every role's wire is bound in both directions"
        );

        // `bind_wire` clears the counterpart's path, so the counterpart is
        // bound after it. It never reads, which is what a QEMU descheduled
        // under load looks like from this side.
        let counterpart = UnixDatagram::bind(qemu.path()).expect("bind the counterpart");

        // The whole sequence runs on a worker: without a transmit bound it
        // never returns, and a hung test diagnoses nothing.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let frame = [0u8; 1514];
            let mut accepted = 0u32;
            while wire.socket.send_to(&frame, &wire.qemu_sock).is_ok() {
                accepted += 1;
                if accepted == u32::MAX {
                    break;
                }
            }
            let start = Instant::now();
            let refused = wire.socket.send_to(&frame, &wire.qemu_sock).is_err();
            let _ = tx.send((accepted, refused, start.elapsed()));
        });

        let (accepted, refused, waited) = rx.recv_timeout(PARKED).expect(
            "a transmit onto a saturated wire must be refused, not park the observer: a \
             parked observer never reaches the receive path its verdict depends on, so the \
             run expires on its ceiling and reports the guest as the fault",
        );
        assert!(
            accepted > 0 && accepted < u32::MAX,
            "the counterpart's queue must saturate at a finite depth, not stay open \
             forever (accepted {accepted})"
        );
        assert!(refused, "a saturated wire refuses the frame");
        assert!(
            waited < PARKED,
            "the refusal took {waited:?}, which is not a bounded transmit"
        );
        drop(counterpart);
    }
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
fn run_bond_peer(
    primary: &Wire,
    backup: &Wire,
    stop: &AtomicBool,
    _succeeded: &ObserverGate,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
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
fn run_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    succeeded: &ObserverGate,
) -> Result<(), String> {
    let guest_v6 = wire::link_local(eui64_interface_id(wire::GUEST_MAC));
    run_v6_campaign(socket, qemu_sock, stop, succeeded, None, guest_v6)
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
    succeeded: &ObserverGate,
) -> Result<(), String> {
    run_v6_campaign(
        socket,
        qemu_sock,
        stop,
        succeeded,
        Some((wire::PEER_STATIC_V6, wire::STATIC_PREFIX_LEN)),
        wire::GUEST_STATIC_V6,
    )
}

/// Shared IPv6 ICMP-campaign event loop: serve the guest reactively and ping
/// `guest_v6` until its echo reply arrives. When `peer_static` is `Some`, the
/// peer additionally assigns itself that static address (DAD runs first), which
/// the engine then prefers as the source for an on-link destination in the same
/// prefix. The one definition both campaign roles share, so the link-local and
/// static verticals cannot drift in their choreography.
fn run_v6_campaign(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    succeeded: &ObserverGate,
    peer_static: Option<(core::net::Ipv6Addr, u8)>,
    guest_v6: core::net::Ipv6Addr,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
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

        // Mirror the campaign verdict into the harness completion gate the
        // instant the guest's reply first arrives, so the QEMU runner can end
        // the run as soon as this out-of-guest observer has its proof — the
        // guest for this vertical is built not to self-exit, precisely so its
        // teardown cannot precede (and lose the race to) the reply leaving the
        // machine. Idempotent: `reply_v6` only ever goes false -> true.
        if reply_v6 {
            succeeded.confirm();
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
    succeeded: &ObserverGate,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
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
                        let frame = dhcp_server::build_frame(kind, &request, None, ident);
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

        // Mirror the campaign verdict into the harness completion gate the
        // instant the guest's reply first arrives, so the QEMU runner can end
        // the run as soon as this out-of-guest observer has its proof — the
        // guest for this vertical is built not to self-exit, precisely so its
        // teardown cannot precede (and lose the race to) the reply leaving the
        // machine. Idempotent: `reply` only ever goes false -> true.
        if reply {
            succeeded.confirm();
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

/// The DHCP-server-plus-NTP-server peer loop: lease the guest an address whose
/// option 42 names *this peer*, then answer the time queries that follow.
///
/// The vertical it serves has **no** configured time server, so the only way
/// the guest can find a reachable one is by reading option 42 out of its own
/// lease: the built-in fallback names public-pool hosts, which cannot resolve
/// on an isolated wire. A guest that ignored the option therefore never sets
/// its clock and the run fails loud, which is what makes this peer a
/// discriminator rather than a tautology.
///
/// The DHCP exchange is [`run_dhcp_peer`]'s, minus the echo campaign the
/// addressing vertical needs (this one's proof is the guest's own applied
/// instant), and the time replies are the shared spoof-first pair. Its verdict
/// is `Ok` once it has offered, acked, and served a request with both replies.
fn run_dhcp_time_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    succeeded: &ObserverGate,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
        Box::new(FixedTempSource),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;
    stack
        .set_ipv4_config(wire::DHCP_SERVER_V4, wire::DHCP_PREFIX_LEN, None)
        .map_err(|e| format!("netstack peer: server address assignment: {e:?}"))?;

    let mut offered = false;
    let mut acked = false;
    let mut served = 0u32;
    let mut ident: u16 = 0xD4C0;
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (the ARP the guest's queries provoke).
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        send_frames(socket, qemu_sock, &out.frames);

        match socket.recv(&mut buf) {
            Ok(len) => {
                if let Some(request) = dhcp_server::parse_frame(&buf[..len]) {
                    let reply_kind = match request.message_type {
                        MessageType::Discover => Some(MessageType::Offer),
                        MessageType::Request => Some(MessageType::Ack),
                        _ => None,
                    };
                    if let Some(kind) = reply_kind {
                        ident = ident.wrapping_add(1);
                        let frame = dhcp_server::build_frame(
                            kind,
                            &request,
                            Some(wire::DHCP_SERVER_V4),
                            ident,
                        );
                        let _ = socket.send_to(&frame, qemu_sock);
                        match kind {
                            MessageType::Offer => offered = true,
                            MessageType::Ack => acked = true,
                            _ => {}
                        }
                    }
                } else if let Some(request) = parse_ntp_frame(&buf[..len]) {
                    answer_ntp_spoof_first(socket, qemu_sock, &request);
                    served = served.saturating_add(1);
                } else {
                    // Neither DHCP nor NTP: the guest's ARP for the peer.
                    let mut out = StackOutput::default();
                    stack.on_frame(&buf[..len], now(start), &mut out);
                    send_frames(socket, qemu_sock, &out.frames);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }

        // The peer cannot know which reply the guest believed, so its own gate
        // is only "the chain got this far"; the guest's serial witness (the
        // exact applied instant) is what proves the rest. Neither side passes
        // alone.
        if acked && served > 0 {
            succeeded.confirm();
        }
    }

    if offered && acked && served > 0 {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: DHCP-plus-time exchange incomplete (offered={offered}, acked={acked}, ntp_served={served})"
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
    fn write_reply(
        kind: MessageType,
        request: &Request,
        time_server: Option<Ipv4Addr>,
        out: &mut [u8; dhcp::MAX_MESSAGE_LEN],
    ) {
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
        // Option 42 only where the vertical is about it, so the plain DHCP
        // vertical's wire stays exactly what it was and this option is the
        // one difference the time vertical turns on.
        if let Some(server) = time_server {
            put_option(out, &mut pos, dhcp::opt::NTP_SERVER, &server.octets());
        }
        out[pos] = dhcp::opt::END;
    }

    /// Build the full Ethernet frame carrying a server→client reply,
    /// link-layer broadcast (the client has no address yet). Frames the
    /// DHCP message as UDP(67→68)/IPv4(`server`→`255.255.255.255`)/Ethernet
    /// with the production `lib/net` writers, so the guest's client decodes
    /// it exactly as it would a real server's.
    pub fn build_frame(
        kind: MessageType,
        request: &Request,
        time_server: Option<Ipv4Addr>,
        ident: u16,
    ) -> Vec<u8> {
        let mut message = [0u8; dhcp::MAX_MESSAGE_LEN];
        write_reply(kind, request, time_server, &mut message);

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
                let frame = build_frame(kind, &request, None, 0x4321);
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
                assert!(
                    reply.ntp_servers.is_empty(),
                    "no time server is advertised unless the vertical asks for one"
                );
            }
        }

        /// The time-server option round-trips through the production client
        /// codec, so the vertical's whole premise — that the guest can read
        /// option 42 out of its lease — is checked without QEMU.
        #[test]
        fn the_advertised_time_server_round_trips_through_the_client_codec() {
            let chaddr = MacAddress(wire::GUEST_MAC);
            let xid = 0x00C0_FFEE;
            let request = Request {
                message_type: MessageType::Request,
                xid,
                chaddr,
            };
            let frame = build_frame(
                MessageType::Ack,
                &request,
                Some(wire::DHCP_SERVER_V4),
                0x1111,
            );
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
            let reply =
                DhcpReply::parse(datagram.payload, xid, chaddr).expect("a valid server reply");
            assert_eq!(
                reply.ntp_servers.as_slice(),
                &[wire::DHCP_SERVER_V4],
                "the guest learns the peer as its time server"
            );
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
            let frame = build_frame(MessageType::Offer, &request, None, 7);
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

/// Interval between the DHCPv6 peer's unsolicited Router Advertisements.
/// DHCPv6 grants no on-link prefix, so the guest learns the leased address's
/// `/64` is on-link (and the peer is a default router) only from an RA; the
/// peer re-emits one on this cadence so the guest picks it up regardless of
/// when its link-local came up. Paced, never a spin (the receive timeout
/// bounds the loop).
const RA_INTERVAL: Duration = Duration::from_millis(300);

/// The DHCPv6-server peer loop (the DHCP D4c vertical): lease the guest an
/// IPv6 address (RFC 8415 stateful IA_NA), advertise the on-link prefix so
/// it can be reached, then prove reachability at that leased address.
///
/// The peer takes its own [`wire::DHCP6_SERVER_V6`] in the shared on-link
/// `/64` and runs a minimal DHCPv6 server — it answers the guest's `Solicit`
/// with an `Advertise` of [`wire::DHCP6_LEASED_V6`] and its `Request` (or a
/// Renew/Rebind) with a `Reply`, both to the client's link-local. Because
/// DHCPv6 conveys no on-link prefix (RFC 8415 leaves that to Router
/// Advertisements), the peer also acts as the on-link router: it periodically
/// emits an RA naming [`wire::DHCP6_PREFIX`] on-link and non-autonomous (so
/// the guest forms no SLAAC address, only the DHCPv6 one) and itself a default
/// router, giving the guest the route it needs to answer. Once the lease is
/// granted the peer pings the guest at the leased address until the reply
/// arrives. The guest's planted `network.conf` selects `ipv6.method dhcp` and
/// disables IPv4, so it forms *no* global address itself: the leased address
/// is its only reachable one, and a broken lease leaves the campaign
/// unanswered (fail loud). DHCPv6 request frames are handled by the server and
/// never fed to the peer's own `lib/net` engine (which holds no DHCPv6 server);
/// every other frame (the guest's neighbour queries, its echo replies) is fed
/// to the engine, which resolves and answers it. Its verdict is `Ok` only once
/// it advertised, replied, **and** saw the guest's echo reply at the leased
/// address, so neither the addressing nor the reachability can pass alone.
fn run_dhcp6_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    succeeded: &ObserverGate,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
        Box::new(FixedTempSource),
        now(start),
    )
    .map_err(|e| format!("netstack peer: engine construction: {e:?}"))?;
    // The peer's own global address in the shared /64: this both gives the
    // campaign an on-link global source for the leased address and installs
    // the connected route the peer resolves the guest over.
    stack
        .add_ipv6_static(wire::DHCP6_SERVER_V6, wire::DHCP6_PREFIX_LEN, now(start))
        .map_err(|e| format!("netstack peer: server address assignment: {e:?}"))?;

    let server_duid = Duid::ll_ethernet(MacAddress(wire::PEER_MAC));
    let peer_ll = wire::link_local(wire::PEER_IID);
    let leased = IpAddr::V6(wire::DHCP6_LEASED_V6);
    let mut advertised = false;
    let mut replied = false;
    let mut reply = false;
    let mut sequence: u16 = 0;
    let mut next_send = Instant::now();
    let mut next_ra = Instant::now();
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (the peer's own DAD probes, NS retransmits
        // for the leased address the campaign is resolving).
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        note_reply(&out, leased, &mut reply);
        send_frames(socket, qemu_sock, &out.frames);

        // Re-emit the on-link/default-router RA on its cadence, so the guest
        // learns the route back regardless of when its link-local came up.
        if Instant::now() >= next_ra {
            next_ra = Instant::now() + RA_INTERVAL;
            let frame = dhcp6_server::build_router_advertisement(peer_ll);
            let _ = socket.send_to(&frame, qemu_sock);
        }

        // Once the guest has been leased its address, ping it there until the
        // reply arrives. Refusals (the server's NS for the guest still
        // pending) are transient; the cadence retries them.
        if replied && !reply && Instant::now() >= next_send {
            next_send = Instant::now() + RESEND_INTERVAL;
            sequence = sequence.wrapping_add(1);
            campaign_ping(&mut stack, socket, qemu_sock, leased, sequence, now(start));
        }

        match socket.recv(&mut buf) {
            Ok(len) => {
                if let Some(request) = dhcp6_server::parse_frame(&buf[..len]) {
                    // A DHCPv6 client request: answer it, and never feed it to
                    // the engine (which has no DHCPv6 server).
                    let reply_kind = match request.message_type {
                        Dhcp6MessageType::Solicit => Some(Dhcp6MessageType::Advertise),
                        Dhcp6MessageType::Request
                        | Dhcp6MessageType::Renew
                        | Dhcp6MessageType::Rebind => Some(Dhcp6MessageType::Reply),
                        _ => None,
                    };
                    if let Some(kind) = reply_kind {
                        let frame =
                            dhcp6_server::build_frame(kind, &request, &server_duid, peer_ll);
                        let _ = socket.send_to(&frame, qemu_sock);
                        match kind {
                            Dhcp6MessageType::Advertise => advertised = true,
                            Dhcp6MessageType::Reply => replied = true,
                            _ => {}
                        }
                    }
                } else {
                    // Not DHCPv6: the guest's neighbour queries, or its echo
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

        // Mirror the campaign verdict into the harness completion gate the
        // instant the guest's reply first arrives, so the QEMU runner can end
        // the run as soon as this out-of-guest observer has its proof — the
        // guest for this vertical is built not to self-exit, precisely so its
        // teardown cannot precede (and lose the race to) the reply leaving the
        // machine. Idempotent: `reply` only ever goes false -> true.
        if reply {
            succeeded.confirm();
        }
    }

    if advertised && replied && reply {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: DHCPv6 exchange incomplete (advertised={advertised}, replied={replied}, reply={reply})"
        ))
    }
}

/// A minimal DHCPv6 **server** codec for the harness peer, plus the
/// on-link Router Advertisement the guest needs to reach its lease.
///
/// TAIRiX ships no DHCPv6 server — only the RFC 8415 *client*
/// ([`tairix_net::dhcpv6`]) — so the server side of the exchange lives here,
/// in the test peer that is its only consumer. It encodes and decodes the
/// *same* wire layout the production client codec exposes (that module's
/// public option registry, message types, and DUID), never a second copy of
/// the format; the round-trip unit tests additionally parse every reply this
/// module builds back through the real [`tairix_net::dhcpv6::Dhcp6Reply::parse`],
/// so the two sides cannot drift. The Router Advertisement is likewise the
/// router half of ND (the engine is a host and refuses to emit one), built
/// here and checked against the production [`tairix_net::nd::NdMessage::parse`].
mod dhcp6_server {
    use super::{
        dhcpv6, eth, ipv6_multicast_mac, udp, wire, Dhcp6MessageType, Duid, Ipv6Addr, Ipv6Header,
        MacAddress, Pseudo, ALL_NODES, ETHERNET_HEADER_LEN, ETHERTYPE_IPV6, ND_HOP_LIMIT,
        NEXT_HEADER_ICMPV6, PROTOCOL_UDP, TYPE_ROUTER_ADVERTISEMENT,
    };

    /// The parts of a client Solicit / Request the server acts on.
    pub struct Request {
        /// The DHCPv6 message type.
        pub message_type: Dhcp6MessageType,
        /// The 24-bit transaction id to echo in the reply.
        pub xid: u32,
        /// The client Identifier DUID to echo in the reply.
        pub client_duid: Duid,
        /// The IAID from the client's IA_NA, echoed in the reply's IA_NA.
        pub iaid: u32,
        /// The client's source address (its link-local) — the reply's
        /// destination.
        pub client_addr: Ipv6Addr,
        /// The client's source MAC — the reply frame's link-layer
        /// destination (the reply is unicast back to the requester).
        pub client_mac: MacAddress,
    }

    /// Decode the DHCPv6 client message an Ethernet frame carries, or
    /// `None` if the frame is not a client→server DHCPv6 request (fail
    /// closed). Every layer is parsed with the production `lib/net`
    /// decoders, so the server accepts exactly the frames a real client
    /// emits.
    pub fn parse_frame(frame: &[u8]) -> Option<Request> {
        let eth_frame = eth::EthernetFrame::parse(frame)?;
        if eth_frame.ethertype != ETHERTYPE_IPV6 {
            return None;
        }
        let (ip, payload) = Ipv6Header::parse(eth_frame.payload)?;
        if ip.next_header != PROTOCOL_UDP {
            return None;
        }
        let datagram = udp::UdpDatagram::parse(
            Pseudo::V6 {
                source: ip.source,
                destination: ip.destination,
            },
            payload,
        )?;
        if datagram.destination_port != dhcpv6::SERVER_PORT
            || datagram.source_port != dhcpv6::CLIENT_PORT
        {
            return None;
        }
        parse_message(datagram.payload, ip.source, eth_frame.source)
    }

    /// Decode a client→server DHCPv6 message (the UDP payload). Total,
    /// bounded, and fail-closed: a truncated header, an unknown message
    /// type, or a missing Client Identifier / IA_NA yields `None`.
    pub fn parse_message(
        payload: &[u8],
        client_addr: Ipv6Addr,
        client_mac: MacAddress,
    ) -> Option<Request> {
        let header = payload.get(..dhcpv6::MESSAGE_HEADER_LEN)?;
        let message_type = Dhcp6MessageType::from_code(header[0])?;
        let xid = u32::from_be_bytes([0, header[1], header[2], header[3]]);
        let mut client_duid = None;
        let mut iaid = None;
        walk_options(
            &payload[dhcpv6::MESSAGE_HEADER_LEN..],
            |code, data| match code {
                // First of each option wins; a duplicate is ignored (bounded,
                // deterministic), mirroring the production reply decoder.
                dhcpv6::opt::CLIENT_ID if client_duid.is_none() => {
                    client_duid = Duid::from_bytes(data);
                }
                // The IA_NA body opens with the 4-octet IAID; a truncated option
                // leaves `iaid` unset (fail closed) and a later IA_NA may still
                // supply it.
                dhcpv6::opt::IA_NA if iaid.is_none() => {
                    iaid = data
                        .get(0..4)
                        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
                }
                _ => {}
            },
        );
        Some(Request {
            message_type,
            xid,
            client_duid: client_duid?,
            iaid: iaid?,
            client_addr,
            client_mac,
        })
    }

    /// A total, bounded walk over a DHCPv6 option region (mirrors the
    /// production decoder), invoking `visit` for each well-formed
    /// `(code, data)` TLV and stopping at the first truncation.
    fn walk_options(region: &[u8], mut visit: impl FnMut(u16, &[u8])) {
        let mut i = 0usize;
        while i + 4 <= region.len() {
            let code = u16::from_be_bytes([region[i], region[i + 1]]);
            let len = usize::from(u16::from_be_bytes([region[i + 2], region[i + 3]]));
            i += 4;
            let Some(data) = region.get(i..i + len) else {
                break;
            };
            visit(code, data);
            i += len;
        }
    }

    /// Append one DHCPv6 option `(2-octet code, 2-octet length, body)` to
    /// `out`, the wire shape RFC 8415 §21.1 defines.
    fn put_option(out: &mut Vec<u8>, code: u16, data: &[u8]) {
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(
            &u16::try_from(data.len())
                .expect("a DHCPv6 option body never exceeds 65535 bytes")
                .to_be_bytes(),
        );
        out.extend_from_slice(data);
    }

    /// Encode the server→client DHCPv6 message body (the 4-octet header plus
    /// the options) for an `Advertise` or `Reply`: the echoed Client
    /// Identifier, the server's Server Identifier, and an `IA_NA` (with the
    /// echoed IAID and T1/T2 left to the client) carrying one IA Address —
    /// [`wire::DHCP6_LEASED_V6`] with the [`wire::DHCP6_LEASE_SECS`] preferred
    /// and valid lifetimes. A success status is implicit (no Status Code
    /// option), which the client reads as [`dhcpv6::status::SUCCESS`].
    fn write_message(kind: Dhcp6MessageType, request: &Request, server_duid: &Duid) -> Vec<u8> {
        let xid = request.xid & 0x00FF_FFFF;
        let xid_bytes = xid.to_be_bytes();
        let mut msg = vec![kind.code(), xid_bytes[1], xid_bytes[2], xid_bytes[3]];
        put_option(
            &mut msg,
            dhcpv6::opt::CLIENT_ID,
            request.client_duid.as_slice(),
        );
        put_option(&mut msg, dhcpv6::opt::SERVER_ID, server_duid.as_slice());

        // The IA Address option (RFC 8415 §21.6): the leased address and its
        // preferred/valid lifetimes, no encapsulated status (success).
        let mut ia_addr = Vec::with_capacity(24);
        ia_addr.extend_from_slice(&wire::DHCP6_LEASED_V6.octets());
        ia_addr.extend_from_slice(&wire::DHCP6_LEASE_SECS.to_be_bytes());
        ia_addr.extend_from_slice(&wire::DHCP6_LEASE_SECS.to_be_bytes());

        // The IA_NA option (RFC 8415 §21.4): the echoed IAID, T1/T2 left to
        // the client (zero), then the encapsulated IA Address.
        let mut ia_na = Vec::with_capacity(12 + 4 + ia_addr.len());
        ia_na.extend_from_slice(&request.iaid.to_be_bytes());
        ia_na.extend_from_slice(&0u32.to_be_bytes()); // T1
        ia_na.extend_from_slice(&0u32.to_be_bytes()); // T2
        put_option(&mut ia_na, dhcpv6::opt::IA_ADDR, &ia_addr);
        put_option(&mut msg, dhcpv6::opt::IA_NA, &ia_na);
        msg
    }

    /// Build the full Ethernet frame carrying a server→client `Advertise` or
    /// `Reply`, unicast back to the requesting client. Frames the DHCPv6
    /// message as UDP(547→546)/IPv6(`peer_ll`→`client`)/Ethernet with the
    /// production `lib/net` writers, so the guest's client decodes it exactly
    /// as it would a real server's.
    pub fn build_frame(
        kind: Dhcp6MessageType,
        request: &Request,
        server_duid: &Duid,
        peer_ll: Ipv6Addr,
    ) -> Vec<u8> {
        let message = write_message(kind, request, server_duid);

        let source = peer_ll;
        let destination = request.client_addr;
        let mut datagram = vec![0u8; udp::UDP_HEADER_LEN + message.len()];
        udp::write(
            Pseudo::V6 {
                source,
                destination,
            },
            dhcpv6::SERVER_PORT,
            dhcpv6::CLIENT_PORT,
            &message,
            &mut datagram,
        )
        .expect("the UDP buffer is sized for the DHCPv6 message");

        let mut header = Ipv6Header::new(source, destination, PROTOCOL_UDP);
        header.hop_limit = ND_HOP_LIMIT;
        let mut packet = vec![0u8; super::IPV6_HEADER_LEN + datagram.len()];
        header
            .write(&mut packet, datagram.len())
            .expect("the IPv6 header fits the sized packet");
        packet[super::IPV6_HEADER_LEN..].copy_from_slice(&datagram);

        let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
        eth::write_header(
            &mut frame,
            request.client_mac,
            MacAddress(wire::PEER_MAC),
            ETHERTYPE_IPV6,
        )
        .expect("the Ethernet header fits the sized frame");
        frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
        frame
    }

    /// Build the full Ethernet frame carrying the peer's Router
    /// Advertisement, multicast to all nodes.
    ///
    /// TAIRiX's `lib/net` engine is a host and deliberately refuses to emit a
    /// Router Advertisement, so — as with the DHCPv6 server messages — the
    /// router half of the exchange is built here. The RA names
    /// [`wire::DHCP6_PREFIX`] on-link and **non**-autonomous (the guest
    /// installs the on-link route but forms no SLAAC address, so its only
    /// global address stays the DHCPv6 lease) and sets a non-zero router
    /// lifetime so the peer is adopted as a default router. It is sourced
    /// from `peer_ll` at hop limit 255, exactly as RFC 4861 requires.
    pub fn build_router_advertisement(peer_ll: Ipv6Addr) -> Vec<u8> {
        // The ICMPv6 message: 4-octet header (type, code, checksum), then the
        // RA fixed fields and options.
        let mut msg = vec![TYPE_ROUTER_ADVERTISEMENT, 0, 0, 0];
        // RA fixed fields (RFC 4861 §4.2): cur_hop_limit, flags (M set —
        // addresses come from DHCPv6), router lifetime, reachable/retrans (0).
        msg.push(64); // cur_hop_limit
        msg.push(0x80); // Managed-address flag
        msg.extend_from_slice(&RA_ROUTER_LIFETIME_SECS.to_be_bytes());
        msg.extend_from_slice(&0u32.to_be_bytes()); // reachable time
        msg.extend_from_slice(&0u32.to_be_bytes()); // retrans timer

        // Source link-layer address option (type 1, length 1 unit = 8 bytes).
        msg.push(1);
        msg.push(1);
        msg.extend_from_slice(&wire::PEER_MAC);

        // Prefix Information option (type 3, length 4 units = 32 bytes): the
        // shared /64, on-link (L) but not autonomous (A cleared).
        msg.push(3);
        msg.push(4);
        msg.push(wire::DHCP6_PREFIX_LEN);
        msg.push(0x80); // L (on-link) set, A (autonomous) clear
        msg.extend_from_slice(&RA_PREFIX_LIFETIME_SECS.to_be_bytes()); // valid
        msg.extend_from_slice(&RA_PREFIX_LIFETIME_SECS.to_be_bytes()); // preferred
        msg.extend_from_slice(&0u32.to_be_bytes()); // reserved2
        msg.extend_from_slice(&wire::DHCP6_PREFIX.octets());

        // Seal the ICMPv6 checksum over the IPv6 pseudo-header + message.
        let destination = ALL_NODES;
        let upper_len = u16::try_from(msg.len()).expect("the RA message fits a u16 length");
        let mut sum = Pseudo::V6 {
            source: peer_ll,
            destination,
        }
        .seed(NEXT_HEADER_ICMPV6, upper_len);
        sum.push(&msg);
        let checksum = sum.finish();
        msg[2..4].copy_from_slice(&checksum.to_be_bytes());

        let mut header = Ipv6Header::new(peer_ll, destination, NEXT_HEADER_ICMPV6);
        header.hop_limit = ND_HOP_LIMIT;
        let mut packet = vec![0u8; super::IPV6_HEADER_LEN + msg.len()];
        header
            .write(&mut packet, msg.len())
            .expect("the IPv6 header fits the sized packet");
        packet[super::IPV6_HEADER_LEN..].copy_from_slice(&msg);

        let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
        eth::write_header(
            &mut frame,
            ipv6_multicast_mac(&destination),
            MacAddress(wire::PEER_MAC),
            ETHERTYPE_IPV6,
        )
        .expect("the Ethernet header fits the sized frame");
        frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
        frame
    }

    /// The router lifetime the RA advertises (seconds): non-zero so the guest
    /// adopts the peer as a default router. Ample for the short vertical.
    const RA_ROUTER_LIFETIME_SECS: u16 = 1800;

    /// The advertised prefix's valid and preferred lifetimes (seconds).
    const RA_PREFIX_LIFETIME_SECS: u32 = 86_400;

    #[cfg(test)]
    mod tests {
        use super::*;
        use tairix_net::dhcpv6::{Dhcp6Reply, MessageSpec};
        use tairix_net::nd::{NdMessage, ND_HOP_LIMIT};

        /// Build a client message the way the production client would (via
        /// [`tairix_net::dhcpv6::write_message`]), so the server codec is
        /// tested against real client output, never a hand-rolled fixture.
        fn client_message(message_type: Dhcp6MessageType, xid: u32, iaid: u32) -> Vec<u8> {
            let spec = MessageSpec {
                message_type,
                transaction_id: xid,
                client_duid: Duid::ll_ethernet(MacAddress(wire::GUEST_MAC)),
                server_id: None,
                iaid,
                elapsed_centis: 0,
                ia_addr: None,
                request_options: true,
            };
            let mut buf = [0u8; dhcpv6::MAX_MESSAGE_LEN];
            let len = dhcpv6::write_message(&spec, &mut buf).expect("client message encodes");
            buf[..len].to_vec()
        }

        #[test]
        fn parses_a_real_client_solicit() {
            let guest_ll = wire::link_local([0, 0, 0, 0, 0, 0, 0, 0x15]);
            let message = client_message(Dhcp6MessageType::Solicit, 0x0012_3456, 0x0A0B_0C0D);
            let request = parse_message(&message, guest_ll, MacAddress(wire::GUEST_MAC))
                .expect("a Solicit is a valid request");
            assert_eq!(request.message_type, Dhcp6MessageType::Solicit);
            assert_eq!(request.xid, 0x0012_3456);
            assert_eq!(request.iaid, 0x0A0B_0C0D);
            assert_eq!(
                request.client_duid,
                Duid::ll_ethernet(MacAddress(wire::GUEST_MAC))
            );
        }

        /// The Advertise and Reply the server builds round-trip through the
        /// real client parser under the client's own xid + DUID, carrying the
        /// leased address, its lifetimes, the echoed IAID, and a success
        /// status — so the server and the production client agree on every
        /// field.
        #[test]
        fn advertise_and_reply_round_trip_through_the_client_parser() {
            let guest_duid = Duid::ll_ethernet(MacAddress(wire::GUEST_MAC));
            let server_duid = Duid::ll_ethernet(MacAddress(wire::PEER_MAC));
            let guest_ll = wire::link_local([0, 0, 0, 0, 0, 0, 0, 0x15]);
            let xid = 0x00AB_CDEF;
            let iaid = 0x1122_3344;
            let request = Request {
                message_type: Dhcp6MessageType::Solicit,
                xid,
                client_duid: guest_duid,
                iaid,
                client_addr: guest_ll,
                client_mac: MacAddress(wire::GUEST_MAC),
            };
            let peer_ll = wire::link_local(wire::PEER_IID);
            for kind in [Dhcp6MessageType::Advertise, Dhcp6MessageType::Reply] {
                let frame = build_frame(kind, &request, &server_duid, peer_ll);
                // Peel the frame back to the DHCPv6 payload with the same
                // production decoders the guest uses.
                let eth_frame = eth::EthernetFrame::parse(&frame).expect("valid Ethernet frame");
                assert_eq!(eth_frame.destination, MacAddress(wire::GUEST_MAC));
                let (ip, payload) =
                    Ipv6Header::parse(eth_frame.payload).expect("valid IPv6 packet");
                assert_eq!(ip.destination, guest_ll);
                assert_eq!(ip.source, peer_ll);
                let datagram = udp::UdpDatagram::parse(
                    Pseudo::V6 {
                        source: ip.source,
                        destination: ip.destination,
                    },
                    payload,
                )
                .expect("valid UDP datagram");
                assert_eq!(datagram.source_port, dhcpv6::SERVER_PORT);
                assert_eq!(datagram.destination_port, dhcpv6::CLIENT_PORT);
                let reply = Dhcp6Reply::parse(datagram.payload, xid, &guest_duid)
                    .expect("a valid server reply");
                assert_eq!(reply.message_type, kind);
                assert_eq!(reply.server_id, Some(server_duid));
                assert_eq!(reply.iaid, Some(iaid));
                assert_eq!(reply.top_status, dhcpv6::status::SUCCESS);
                assert_eq!(reply.ia_status, dhcpv6::status::SUCCESS);
                let leased = reply
                    .addresses
                    .first_usable()
                    .expect("the reply leases a usable address");
                assert_eq!(leased.addr, wire::DHCP6_LEASED_V6);
                assert_eq!(leased.preferred_lifetime, wire::DHCP6_LEASE_SECS);
                assert_eq!(leased.valid_lifetime, wire::DHCP6_LEASE_SECS);
            }
        }

        /// The server never mistakes its own reply for a client request: a
        /// built Advertise frame (server→client, UDP 547→546) is not parsed
        /// as a request, so the peer cannot answer itself in a loop.
        #[test]
        fn a_server_reply_is_not_parsed_as_a_client_request() {
            let request = Request {
                message_type: Dhcp6MessageType::Solicit,
                xid: 1,
                client_duid: Duid::ll_ethernet(MacAddress(wire::GUEST_MAC)),
                iaid: 1,
                client_addr: wire::link_local([0, 0, 0, 0, 0, 0, 0, 0x15]),
                client_mac: MacAddress(wire::GUEST_MAC),
            };
            let peer_ll = wire::link_local(wire::PEER_IID);
            let frame = build_frame(
                Dhcp6MessageType::Advertise,
                &request,
                &Duid::ll_ethernet(MacAddress(wire::PEER_MAC)),
                peer_ll,
            );
            assert!(parse_frame(&frame).is_none());
        }

        /// A frame that is not DHCPv6 (wrong UDP port / too short) is rejected
        /// (fail closed), so the server only answers genuine client requests.
        #[test]
        fn a_non_dhcp6_frame_is_rejected() {
            assert!(parse_frame(&[]).is_none());
            assert!(parse_message(
                &[0u8; 2],
                wire::link_local(wire::PEER_IID),
                MacAddress(wire::GUEST_MAC)
            )
            .is_none());
        }

        /// The Router Advertisement the peer builds round-trips through the
        /// production [`NdMessage::parse`]: hop limit 255, one on-link,
        /// non-autonomous prefix naming the shared `/64`, a non-zero router
        /// lifetime, and the peer's link-layer address — exactly the facts the
        /// guest's stack installs an on-link route and default router from.
        #[test]
        fn router_advertisement_round_trips_through_nd_parse() {
            let peer_ll = wire::link_local(wire::PEER_IID);
            let frame = build_router_advertisement(peer_ll);
            let eth_frame = eth::EthernetFrame::parse(&frame).expect("valid Ethernet frame");
            assert_eq!(eth_frame.destination, ipv6_multicast_mac(&ALL_NODES));
            let (ip, payload) = Ipv6Header::parse(eth_frame.payload).expect("valid IPv6 packet");
            assert_eq!(ip.source, peer_ll);
            assert_eq!(ip.destination, ALL_NODES);
            assert_eq!(ip.hop_limit, ND_HOP_LIMIT);
            assert_eq!(ip.next_header, NEXT_HEADER_ICMPV6);
            // The ICMPv6 message: type/code/checksum header, then the RA body
            // `NdMessage::parse` reads. The checksum must verify (folding the
            // pseudo-header + message to zero).
            let mut verify = Pseudo::V6 {
                source: ip.source,
                destination: ip.destination,
            }
            .seed(NEXT_HEADER_ICMPV6, u16::try_from(payload.len()).unwrap());
            verify.push(payload);
            assert_eq!(verify.finish(), 0, "the RA ICMPv6 checksum verifies");
            let message =
                NdMessage::parse(payload[0], payload[1], ip.hop_limit, true, &payload[4..])
                    .expect("a valid Router Advertisement");
            match message {
                NdMessage::RouterAdvertisement {
                    managed,
                    router_lifetime,
                    source_ll,
                    prefixes,
                    ..
                } => {
                    assert!(managed, "the RA sets the Managed flag (DHCPv6)");
                    assert_eq!(router_lifetime, RA_ROUTER_LIFETIME_SECS);
                    assert_eq!(source_ll, Some(MacAddress(wire::PEER_MAC)));
                    let prefix = prefixes.first().expect("the RA carries a prefix");
                    assert_eq!(prefix.prefix, wire::DHCP6_PREFIX);
                    assert_eq!(prefix.prefix_len, wire::DHCP6_PREFIX_LEN);
                    assert!(prefix.on_link, "the prefix is on-link");
                    assert!(
                        !prefix.autonomous,
                        "the prefix is not autonomous (no SLAAC)"
                    );
                }
                other => panic!("expected a Router Advertisement, got {other:?}"),
            }
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
    _succeeded: &ObserverGate,
) -> Result<(), String> {
    let facts = DeviceFacts {
        mac: MacAddress(wire::PEER_MAC),
        mtu: 1500,
        link: LinkState::Up,
        offloads: NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let mut stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
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
/// (the engine's retransmission machinery recovers the loss), after the
/// guest exits the wire is torn down under us, and a counterpart that has
/// stopped draining refuses the frame once [`SEND_TIMEOUT`] elapses rather
/// than parking this thread.
fn send_frames(socket: &UnixDatagram, qemu_sock: &PathBuf, frames: &[TxFrame]) {
    for frame in frames {
        // The host peer speaks the raw wire; a live device would consume
        // the transmit-offload metadata, so it is ignored here.
        let _ = socket.send_to(&frame.bytes, qemu_sock);
    }
}

// --- NTP-server peer (plans/TIMESYNC.md TS-2 vertical) -----------------

/// Seconds from the NTP epoch (1900-01-01) to the Unix epoch (1970-01-01).
const NTP_UNIX_DELTA_SECS: i64 = 2_208_988_800;

/// Encoded length of the NTP header (RFC 5905 §7.3).
const NTP_PACKET_LEN: usize = 48;

/// Byte offset of the origin timestamp within the NTP header — the field a
/// reply must echo the request's nonce in (RFC 5905 §7.3).
const NTP_ORIGIN_TS_AT: usize = 24;

/// Byte offset of the receive timestamp within the NTP header.
const NTP_RECEIVE_TS_AT: usize = 32;

/// Byte offset of the transmit timestamp within the NTP header.
const NTP_TRANSMIT_TS_AT: usize = 40;

/// The UDP port NTP is served on (RFC 5905 §7.2).
const NTP_PORT: u16 = 123;

/// The parts of a client request the fixture server acts on.
struct NtpRequest {
    /// The client's CSPRNG nonce, carried in its transmit timestamp — the
    /// value a genuine reply must echo as its origin timestamp.
    nonce: u64,
    /// The client's source address; the reply's destination.
    client_addr: IpAddr,
    /// The address the request was *addressed to* — the reply's source. Taken
    /// from the wire rather than from a scenario constant, so the same
    /// responder serves the IPv6 static-addressing vertical and the IPv4
    /// DHCP-learned one without knowing which it is in.
    server_addr: IpAddr,
    /// The client's source port; the reply's destination port.
    client_port: u16,
    /// The client's source MAC — the reply frame's link-layer destination.
    client_mac: MacAddress,
}

/// Decode the NTP client request an Ethernet frame carries, or `None` if the
/// frame is not one (fail closed). Every layer is parsed with the production
/// `lib/net` decoders, so the server accepts exactly the frames a real client
/// emits — over either address family, since a guest addressed by DHCP asks
/// over IPv4 and a statically addressed one over IPv6.
fn parse_ntp_frame(frame: &[u8]) -> Option<NtpRequest> {
    let eth_frame = eth::EthernetFrame::parse(frame)?;
    let (source, destination, payload) = match eth_frame.ethertype {
        ETHERTYPE_IPV6 => {
            let (ip, payload) = Ipv6Header::parse(eth_frame.payload)?;
            if ip.next_header != PROTOCOL_UDP {
                return None;
            }
            (IpAddr::V6(ip.source), IpAddr::V6(ip.destination), payload)
        }
        ETHERTYPE_IPV4 => {
            let (ip, _options, payload) = Ipv4Header::parse(eth_frame.payload)?;
            if ip.protocol != PROTOCOL_UDP {
                return None;
            }
            (IpAddr::V4(ip.source), IpAddr::V4(ip.destination), payload)
        }
        _ => return None,
    };
    let datagram = udp::UdpDatagram::parse(udp_pseudo(source, destination), payload)?;
    if datagram.destination_port != NTP_PORT {
        return None;
    }
    let header = datagram.payload.get(..NTP_PACKET_LEN)?;
    // Mode 3 is a client request; anything else is not ours to answer.
    if header[0] & 0b111 != 3 {
        return None;
    }
    let mut nonce = [0u8; 8];
    nonce.copy_from_slice(&header[NTP_TRANSMIT_TS_AT..NTP_TRANSMIT_TS_AT + 8]);
    Some(NtpRequest {
        nonce: u64::from_be_bytes(nonce),
        client_addr: source,
        server_addr: destination,
        client_port: datagram.source_port,
        client_mac: eth_frame.source,
    })
}

/// The UDP checksum pseudo-header for a `source`→`destination` pair, which
/// must be of one family (a mixed pair cannot arrive from a parsed frame).
fn udp_pseudo(source: IpAddr, destination: IpAddr) -> Pseudo {
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => Pseudo::V4 {
            source,
            destination,
        },
        (IpAddr::V6(source), IpAddr::V6(destination)) => Pseudo::V6 {
            source,
            destination,
        },
        // One family per packet: a parsed frame never mixes them.
        _ => unreachable!("an IP packet's addresses are of one family"),
    }
}

/// The 64-bit NTP timestamp denoting `unix_secs`, wrapping into whatever era
/// that lands in exactly as a server on the wire would.
fn ntp_timestamp(unix_secs: i64) -> u64 {
    let field = u32::try_from((unix_secs + NTP_UNIX_DELTA_SECS).rem_euclid(1 << 32))
        .expect("reduced modulo 2^32");
    u64::from(field) << 32
}

/// Build the full Ethernet frame carrying one server reply.
///
/// `origin` is the origin timestamp the reply claims (the request's nonce for
/// the truthful reply, a different value for the spoof) and `unix_secs` the
/// instant it reports. Framed as UDP(123→client)/IPv6(peer→client)/Ethernet
/// with the production `lib/net` writers, so the guest decodes it exactly as
/// it would a real server's.
fn build_ntp_reply(request: &NtpRequest, origin: u64, unix_secs: i64) -> Vec<u8> {
    let mut message = [0u8; NTP_PACKET_LEN];
    // Leap 0 (no warning), version 4, mode 4 (server), stratum 2.
    message[0] = (4 << 3) | 4;
    message[1] = 2;
    // Poll and precision the client does not read; a plausible reference id.
    message[2] = 6;
    message[3] = 0xEC;
    message[12..16].copy_from_slice(b"FIXT");
    let stamp = ntp_timestamp(unix_secs);
    message[NTP_ORIGIN_TS_AT..NTP_ORIGIN_TS_AT + 8].copy_from_slice(&origin.to_be_bytes());
    message[NTP_RECEIVE_TS_AT..NTP_RECEIVE_TS_AT + 8].copy_from_slice(&stamp.to_be_bytes());
    message[NTP_TRANSMIT_TS_AT..NTP_TRANSMIT_TS_AT + 8].copy_from_slice(&stamp.to_be_bytes());

    let source = request.server_addr;
    let destination = request.client_addr;
    let mut datagram = vec![0u8; udp::UDP_HEADER_LEN + message.len()];
    udp::write(
        udp_pseudo(source, destination),
        NTP_PORT,
        request.client_port,
        &message,
        &mut datagram,
    )
    .expect("the UDP buffer is sized for the NTP header");

    let (packet, ethertype) = match (source, destination) {
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let mut header = Ipv6Header::new(source, destination, PROTOCOL_UDP);
            header.hop_limit = ND_HOP_LIMIT;
            let mut packet = vec![0u8; IPV6_HEADER_LEN + datagram.len()];
            header
                .write(&mut packet, datagram.len())
                .expect("the IPv6 header fits the sized packet");
            packet[IPV6_HEADER_LEN..].copy_from_slice(&datagram);
            (packet, ETHERTYPE_IPV6)
        }
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let header = Ipv4Header::new(source, destination, PROTOCOL_UDP);
            let mut packet = vec![0u8; IPV4_HEADER_LEN + datagram.len()];
            header
                .write(&mut packet, datagram.len())
                .expect("the IPv4 header fits the sized packet");
            packet[IPV4_HEADER_LEN..].copy_from_slice(&datagram);
            (packet, ETHERTYPE_IPV4)
        }
        _ => unreachable!("an IP packet's addresses are of one family"),
    };

    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    eth::write_header(
        &mut frame,
        request.client_mac,
        MacAddress(wire::PEER_MAC),
        ethertype,
    )
    .expect("the Ethernet header fits the sized frame");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// Answer one NTP client request the way every time vertical's peer does:
/// **spoof first**, then the truthful reply.
///
/// The spoof is well-formed but its origin timestamp does not echo the
/// request's nonce, and it reports a plainly different instant. A guest whose
/// nonce gate works drops it *without* ending the transaction, so the truthful
/// reply that follows still lands; a guest that believed it lands on the wrong
/// instant, and one that let it cancel the transaction sets no clock at all.
/// That ordering is the whole discriminator, so it has one definition.
fn answer_ntp_spoof_first(socket: &UnixDatagram, qemu_sock: &PathBuf, request: &NtpRequest) {
    let spoof = build_ntp_reply(request, request.nonce ^ u64::MAX, wire::NTP_SPOOF_SECS);
    let _ = socket.send_to(&spoof, qemu_sock);
    let truth = build_ntp_reply(request, request.nonce, wire::NTP_FIXTURE_SECS);
    let _ = socket.send_to(&truth, qemu_sock);
}

/// The NTP-server peer loop: answer the guest's time queries spoof-first.
///
/// The peer assigns itself [`wire::PEER_STATIC_V6`] on the guest's on-link
/// `/64` (DAD runs first) and feeds every non-NTP frame to its own `lib/net`
/// engine, which answers the guest's neighbour discovery. An NTP client
/// request is answered by this server and never fed to the engine: the engine
/// holds no NTP server, and a datagram to an unbound port would draw a
/// spurious port-unreachable back at the guest.
///
/// Each request draws **two** replies, spoof first — see [`NetPeer::spawn_ntp`]
/// for why that ordering is the discriminator. Its verdict is `Ok` once a
/// request has been served with both.
fn run_ntp_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    succeeded: &ObserverGate,
) -> Result<(), String> {
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let (mut stack, _guest_ll) = peer_stack(start)?;
    stack
        .add_ipv6_static(wire::PEER_STATIC_V6, wire::STATIC_PREFIX_LEN, now(start))
        .map_err(|e| format!("netstack peer: static address assignment: {e:?}"))?;

    let mut served = 0u32;
    let mut buf = [0u8; MAX_FRAME];

    while !stop.load(Ordering::Acquire) {
        // Timer-due engine output (DAD probes, neighbour retransmits).
        let mut out = StackOutput::default();
        stack.advance(now(start), &mut out);
        send_frames(socket, qemu_sock, &out.frames);

        match socket.recv(&mut buf) {
            Ok(len) => {
                if let Some(request) = parse_ntp_frame(&buf[..len]) {
                    answer_ntp_spoof_first(socket, qemu_sock, &request);
                    served = served.saturating_add(1);
                    succeeded.confirm();
                } else {
                    let mut out = StackOutput::default();
                    stack.on_frame(&buf[..len], now(start), &mut out);
                    send_frames(socket, qemu_sock, &out.frames);
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }
    }

    if served > 0 {
        Ok(())
    } else {
        Err("netstack peer: the guest sent no NTP request".to_string())
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
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: McastFilter::Unfiltered,
    };
    let now0 =
        Duration64::from_nanos(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    let stack = Stack::new(
        &StackConfig::new(facts, wire::PEER_IID, IPV4_IDENT_SEED, STACK_HASH_KEY),
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
    _succeeded: &ObserverGate,
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
                let drop = tcb.is_established()
                    && loss_budget > 0
                    && inbound_seen.is_multiple_of(LOSS_EVERY);
                inbound_seen += 1;
                if drop {
                    loss_budget -= 1;
                } else {
                    deliver_inbound_frame(
                        &mut stack,
                        Some(&mut tcb),
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

// --- Telnet server (the plans/TELNET.md vertical) ----------------------

/// Run the telnet-server peer: accept the guest client's connection, speak the
/// server half of RFC 854 through the *same* `nvt`/`option`/`linemode`
/// vocabulary the client's own codec exposes, and verify the whole exchange.
///
/// The server logic is test-only (this plan ships no telnet *server*), so it
/// lives beside its one consumer here rather than in a shipped crate — the
/// DHCP-server precedent — and it encodes and decodes through the client's
/// public wire vocabulary so the two sides cannot drift.
fn run_telnet_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    succeeded: &ObserverGate,
) -> Result<(), String> {
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let (mut stack, guest_v6) = peer_stack(start)?;
    // A fixed initial sequence number: the vertical needs no unpredictability
    // and a fixed ISN keeps runs replayable (the live stack draws a real
    // CSPRNG one; this is only the far end of the wire).
    let mut tcb = Tcb::listen(TcpConfig::default(), wire::PEER_TELNET_PORT, 0, 0x7E1E_7000);
    let mut server = telnet_server::Server::new();
    let mut buf = [0u8; MAX_FRAME];
    let mut scratch = [0u8; MAX_FRAME];
    let mut opened = false;

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
            Ok(len) => deliver_inbound_frame(
                &mut stack,
                Some(&mut tcb),
                &buf[..len],
                socket,
                qemu_sock,
                now(start),
                |_, ecn| ecn,
            ),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }

        // The connection is up: open the negotiation exactly once.
        if !opened && tcb.is_established() {
            opened = true;
            server.open();
        }
        loop {
            let n = tcb.recv(&mut scratch);
            if n == 0 {
                break;
            }
            server.feed(&scratch[..n]);
        }
        let outbound = server.take_wire();
        if !outbound.is_empty() {
            // The send buffer is generous relative to a telnet exchange, so a
            // short accept means the guest stopped reading; the verdict below
            // is what reports that, not a silent truncation here.
            match tcb.send(&outbound) {
                Ok(accepted) if accepted == outbound.len() => {}
                Ok(accepted) => {
                    return Err(format!(
                        "netstack peer: telnet send truncated at {accepted} of {} bytes",
                        outbound.len()
                    ))
                }
                Err(_) => return Err(String::from("netstack peer: telnet send refused")),
            }
        }
        if server.satisfied() {
            succeeded.confirm();
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

    server.verdict()
}

/// The server half of RFC 854, for the telnet vertical only.
///
/// It is deliberately a *checker* as much as a server: every step of the
/// exchange it drives is recorded, and [`telnet_server::Server::verdict`]
/// refuses the run unless the client completed all of them. Encoding and
/// decoding go through
/// `tairix_telnet`'s public wire vocabulary — the same `nvt`, `option`,
/// `subneg` and `linemode` definitions the client itself uses — so a change to
/// the protocol on one side cannot silently pass on the other.
mod telnet_server {
    use tairix_telnet::linemode::{mode, slc_flag, sub, SLC_MAX};
    use tairix_telnet::nvt::{self, NvtEvent, Parser, DO, DONT, WILL, WONT};
    use tairix_telnet::option;
    use tairix_telnet::subneg::cmd;
    use tairix_test_netstack_wire as wire;

    /// What the client must have done for the run to pass.
    // One bool per negotiation step, so a failed run names the step it missed.
    // Folding them into a state machine would lose exactly that: the steps are
    // independent and arrive in whatever order the client chooses.
    #[allow(clippy::struct_excessive_bools)]
    #[derive(Debug, Default)]
    struct Witnessed {
        /// It agreed to do LINEMODE (`WILL LINEMODE`).
        linemode: bool,
        /// It stated a `MODE` mask, which we acknowledged.
        mode: bool,
        /// It exported its SLC table.
        slc: bool,
        /// It reported its window over NAWS.
        naws: bool,
        /// It named its terminal type.
        terminal_type: bool,
        /// It accepted our `WILL SUPPRESS GO AHEAD`.
        suppress_go_ahead: bool,
        /// The probe line arrived, whole.
        probe: bool,
        /// We answered it.
        echoed: bool,
    }

    /// The server's state for one connection.
    pub struct Server {
        parser: Parser,
        seen: Witnessed,
        /// The line being assembled from the client's data bytes.
        line: Vec<u8>,
        /// A `CR` held from the previous read, so the NVT line ending is
        /// recognised across any chunking.
        pending_cr: bool,
        /// The banner is sent once, and only once the negotiation is done.
        greeted: bool,
        wire: Vec<u8>,
    }

    impl Server {
        /// A server that has said nothing yet.
        pub fn new() -> Self {
            Self {
                parser: Parser::new(),
                seen: Witnessed::default(),
                line: Vec::new(),
                pending_cr: false,
                greeted: false,
                wire: Vec::new(),
            }
        }

        /// Open the negotiation, as a server does the moment a client connects.
        ///
        /// It offers to suppress Go Ahead (so the session is full duplex) and
        /// asks the client for its terminal type, its window size and
        /// LINEMODE. It deliberately does **not** offer `WILL ECHO`: this
        /// vertical exercises the client-side editing LINEMODE asks for, and a
        /// server that echoed as well would take the echo back off it.
        pub fn open(&mut self) {
            nvt::push_negotiate(WILL, option::SUPPRESS_GO_AHEAD, &mut self.wire);
            nvt::push_negotiate(DO, option::TERMINAL_TYPE, &mut self.wire);
            nvt::push_negotiate(DO, option::NAWS, &mut self.wire);
            nvt::push_negotiate(DO, option::LINEMODE, &mut self.wire);
        }

        /// Bytes to transmit, taken.
        pub fn take_wire(&mut self) -> Vec<u8> {
            core::mem::take(&mut self.wire)
        }

        /// Whether every step has been witnessed, so the run may end.
        pub fn satisfied(&self) -> bool {
            let s = &self.seen;
            s.linemode
                && s.mode
                && s.slc
                && s.naws
                && s.terminal_type
                && s.suppress_go_ahead
                && s.probe
                && s.echoed
        }

        /// The run's verdict: `Ok` only when the client completed the whole
        /// exchange, and otherwise an error naming the first step it did not.
        pub fn verdict(&self) -> Result<(), String> {
            let s = &self.seen;
            for (done, what) in [
                (s.suppress_go_ahead, "accept DO SUPPRESS GO AHEAD"),
                (s.terminal_type, "report its terminal type"),
                (s.naws, "report its window size over NAWS"),
                (s.linemode, "agree to WILL LINEMODE"),
                (s.mode, "state a LINEMODE MODE mask"),
                (s.slc, "export its LINEMODE SLC table"),
                (s.probe, "send the probe line"),
                (s.echoed, "receive the echoed probe line"),
            ] {
                if !done {
                    return Err(format!("netstack peer: the telnet client did not {what}"));
                }
            }
            Ok(())
        }

        /// Fold received bytes: answer the negotiation, assemble the data into
        /// lines, and echo a completed line back upper-cased.
        pub fn feed(&mut self, bytes: &[u8]) {
            // The parser borrows its own subnegotiation buffer while reporting,
            // so the events are collected before they are folded.
            let mut events: Vec<Event> = Vec::new();
            self.parser
                .feed(bytes, |event| events.push(Event::from(event)));
            for event in events {
                match event {
                    Event::Data(data) => self.on_data(&data),
                    Event::Negotiate(verb, opt) => self.on_negotiate(verb, opt),
                    Event::Subnegotiation(opt, params) => self.on_subnegotiation(opt, &params),
                    // A command carries nothing this server acts on; the
                    // client's own tests cover what it sends them for.
                    Event::Other => {}
                }
            }
            self.greet_when_ready();
        }

        /// Fold the client's answers to our requests.
        fn on_negotiate(&mut self, verb: u8, opt: u8) {
            match (verb, opt) {
                (DO, option::SUPPRESS_GO_AHEAD) => self.seen.suppress_go_ahead = true,
                (WILL, option::LINEMODE) => self.seen.linemode = true,
                // A `WILL` for something we asked for needs no answer; anything
                // else the client offers is refused, as a server that does not
                // implement it must.
                (WILL, option::TERMINAL_TYPE | option::NAWS) => {}
                (WILL, other) => nvt::push_negotiate(DONT, other, &mut self.wire),
                (DO, other) => nvt::push_negotiate(WONT, other, &mut self.wire),
                _ => {}
            }
            // The client's terminal type only arrives if we ask for it, which
            // we can do the moment it agrees.
            if verb == WILL && opt == option::TERMINAL_TYPE {
                nvt::push_subnegotiation(option::TERMINAL_TYPE, &[cmd::SEND], &mut self.wire);
            }
        }

        /// Fold a subnegotiation.
        fn on_subnegotiation(&mut self, opt: u8, params: &[u8]) {
            match opt {
                option::TERMINAL_TYPE => {
                    // `IS <type>`, with a non-empty type.
                    if params.first() == Some(&cmd::IS) && params.len() > 1 {
                        self.seen.terminal_type = true;
                    }
                }
                option::NAWS => {
                    // Exactly four octets: width then height, big-endian, both
                    // non-zero — a report, never a fabricated placeholder.
                    if let [wh, wl, hh, hl] = *params {
                        let width = u16::from_be_bytes([wh, wl]);
                        let height = u16::from_be_bytes([hh, hl]);
                        if width > 0 && height > 0 {
                            self.seen.naws = true;
                        }
                    }
                }
                option::LINEMODE => self.on_linemode(params),
                _ => {}
            }
        }

        /// Fold a LINEMODE subnegotiation: acknowledge a `MODE` mask, and
        /// acknowledge an exported SLC table function by function.
        fn on_linemode(&mut self, params: &[u8]) {
            match params.split_first() {
                Some((&sub::MODE, [mask])) => {
                    // An acknowledgement of our own statement is not a
                    // statement; we make none, so any ack here is a protocol
                    // error the verdict will catch through the missing step.
                    if mask & mode::MODE_ACK == 0 {
                        self.seen.mode = true;
                        nvt::push_subnegotiation(
                            option::LINEMODE,
                            &[sub::MODE, mask | mode::MODE_ACK],
                            &mut self.wire,
                        );
                    }
                }
                Some((&sub::SLC, triplets)) if triplets.len() >= 3 => {
                    self.seen.slc = true;
                    let mut reply = vec![sub::SLC];
                    for triplet in triplets.as_chunks::<3>().0 {
                        let (function, flags, value) = (triplet[0], triplet[1], triplet[2]);
                        // Only acknowledge what the client stated; an already
                        // acknowledged triplet is never answered again, which
                        // is what ends the exchange.
                        if function == 0 || function > SLC_MAX || flags & slc_flag::ACK != 0 {
                            continue;
                        }
                        reply.extend_from_slice(&[function, flags | slc_flag::ACK, value]);
                    }
                    if reply.len() > 1 {
                        nvt::push_subnegotiation(option::LINEMODE, &reply, &mut self.wire);
                    }
                }
                _ => {}
            }
        }

        /// Assemble data bytes into NVT lines and answer a completed one.
        fn on_data(&mut self, data: &[u8]) {
            for &byte in data {
                if self.pending_cr {
                    self.pending_cr = false;
                    // `CR LF` and `CR NUL` both end a line here: the client
                    // sends whichever its `crlf` toggle selects.
                    if byte == b'\n' || byte == 0 {
                        self.finish_line();
                        continue;
                    }
                    self.line.push(b'\r');
                }
                if byte == b'\r' {
                    self.pending_cr = true;
                    continue;
                }
                if byte == b'\n' {
                    self.finish_line();
                    continue;
                }
                // A hostile client is not this vertical's subject, but an
                // unbounded buffer is still unbounded: a line longer than the
                // probe cannot be the probe, so it is dropped.
                if self.line.len() < 256 {
                    self.line.push(byte);
                }
            }
        }

        /// Answer one completed line.
        fn finish_line(&mut self) {
            let line = core::mem::take(&mut self.line);
            let text = String::from_utf8_lossy(&line).into_owned();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return;
            }
            if trimmed == wire::TELNET_PROBE {
                self.seen.probe = true;
            }
            let answer = format!("ECHO[{}]\r\n", trimmed.to_uppercase());
            self.wire.extend_from_slice(answer.as_bytes());
            if trimmed == wire::TELNET_PROBE {
                self.seen.echoed = true;
            }
        }

        /// Greet the session once the whole option exchange has completed, so
        /// the banner on the transcript witnesses the negotiation and not
        /// merely a TCP connection.
        fn greet_when_ready(&mut self) {
            if self.greeted {
                return;
            }
            let s = &self.seen;
            if !(s.suppress_go_ahead && s.terminal_type && s.naws && s.linemode && s.mode && s.slc)
            {
                return;
            }
            self.greeted = true;
            self.wire.extend_from_slice(wire::TELNET_BANNER.as_bytes());
            self.wire.extend_from_slice(b"\r\n");
        }
    }

    /// An owned [`NvtEvent`], so the parser's borrow of its own buffer ends
    /// before the server folds the event and mutates itself.
    enum Event {
        Data(Vec<u8>),
        Negotiate(u8, u8),
        Subnegotiation(u8, Vec<u8>),
        Other,
    }

    impl From<NvtEvent<'_>> for Event {
        fn from(event: NvtEvent<'_>) -> Self {
            match event {
                NvtEvent::Data(bytes) => Self::Data(bytes.to_vec()),
                NvtEvent::Negotiate { verb, option } => Self::Negotiate(verb, option),
                NvtEvent::Subnegotiation { option, params } => {
                    Self::Subnegotiation(option, params.to_vec())
                }
                _ => Self::Other,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Server, Witnessed};
        use tairix_telnet::command::Config;
        use tairix_telnet::linemode::{mode, sub};
        use tairix_telnet::nvt::{Parser, DO, IAC, SB, SE, WILL};
        use tairix_telnet::option;
        use tairix_telnet::session::Session;
        use tairix_test_netstack_wire as wire;

        /// Every event in `bytes`, as the real client parser sees it.
        fn events(bytes: &[u8]) -> Vec<String> {
            let mut parser = Parser::new();
            let mut out = Vec::new();
            parser.feed(bytes, |event| out.push(format!("{event:?}")));
            out
        }

        #[test]
        fn every_frame_the_server_emits_reparses_through_the_client_codec() {
            let mut server = Server::new();
            server.open();
            let opened = server.take_wire();
            assert!(!opened.is_empty());
            assert!(
                events(&opened)
                    .iter()
                    .all(|event| !event.contains("Refused")),
                "{:?}",
                events(&opened)
            );
        }

        /// The strongest check available on the host: run the *real* client
        /// session against this server and confirm the exchange completes. If
        /// either side drifts, the vertical would fail on a QEMU boot; this
        /// fails here in milliseconds instead.
        #[test]
        fn the_real_client_completes_the_whole_exchange() {
            let config = Config::default();
            let mut client = Session::new(&config, "XTERM", 38_400);
            client.begin(&config);
            client.set_terminal_size(tairix_abi::TerminalSize::new(24, 80).ok());
            let mut server = Server::new();
            server.open();

            // Ten rounds is ample for an exchange that settles in three; a
            // bound, so a drifting negotiation ends the test rather than
            // looping.
            for _ in 0..10 {
                let to_server = client.take_wire();
                if !to_server.is_empty() {
                    server.feed(&to_server);
                }
                let to_client = server.take_wire();
                if !to_client.is_empty() {
                    client.on_network(&to_client);
                }
                let _ = client.take_screen();
                let _ = client.take_trace();
            }
            // Now the operator types the probe line, exactly as the vertical's
            // serial script does.
            let mut typed = wire::TELNET_PROBE.as_bytes().to_vec();
            typed.push(b'\r');
            client.on_keyboard(&typed);
            server.feed(&client.take_wire());
            let answer = server.take_wire();
            client.on_network(&answer);
            let screen = String::from_utf8_lossy(&client.take_screen()).into_owned();

            assert!(
                server.verdict().is_ok(),
                "{:?}",
                server.verdict().expect_err("checked")
            );
            assert!(server.satisfied());
            assert!(
                screen.contains(wire::TELNET_ECHO),
                "the client displayed the peer's answer: {screen:?}"
            );
        }

        #[test]
        fn a_client_that_does_nothing_fails_the_verdict() {
            let server = Server::new();
            let err = server.verdict().expect_err("nothing was witnessed");
            assert!(err.contains("SUPPRESS GO AHEAD"), "{err}");
        }

        #[test]
        fn the_banner_waits_for_the_whole_negotiation() {
            let mut server = Server::new();
            server.open();
            let _ = server.take_wire();
            // A client that only accepts Go Ahead suppression is not yet
            // negotiated, so the banner must not appear.
            server.feed(&[IAC, DO, option::SUPPRESS_GO_AHEAD]);
            let wire_bytes = server.take_wire();
            assert!(
                !String::from_utf8_lossy(&wire_bytes).contains(wire::TELNET_BANNER),
                "the banner is a witness for the whole exchange"
            );
        }

        #[test]
        fn a_mode_statement_is_acknowledged_exactly_once() {
            let mut server = Server::new();
            server.feed(&[
                IAC,
                SB,
                option::LINEMODE,
                sub::MODE,
                mode::EDIT | mode::TRAPSIG,
                IAC,
                SE,
            ]);
            let first = server.take_wire();
            let acked = events(&first)
                .iter()
                .any(|event| event.contains("Subnegotiation"));
            assert!(acked, "{:?}", events(&first));
            // An acknowledgement from the client is never answered.
            server.feed(&[
                IAC,
                SB,
                option::LINEMODE,
                sub::MODE,
                mode::EDIT | mode::MODE_ACK,
                IAC,
                SE,
            ]);
            assert!(server.take_wire().is_empty());
        }

        #[test]
        fn a_naws_report_of_zero_is_not_accepted() {
            let mut server = Server::new();
            server.feed(&[IAC, SB, option::NAWS, 0, 0, 0, 0, IAC, SE]);
            let err = server.verdict().expect_err("a zero grid is not a report");
            assert!(err.contains("NAWS") || err.contains("SUPPRESS"), "{err}");
        }

        #[test]
        fn an_option_the_server_never_asked_for_is_refused() {
            let mut server = Server::new();
            // 37 is AUTHENTICATION, which this server does not implement.
            server.feed(&[IAC, WILL, 37]);
            let refusal = events(&server.take_wire());
            assert!(
                refusal.iter().any(|event| event.contains("254")),
                "expected a DONT: {refusal:?}"
            );
        }

        #[test]
        fn the_witness_record_starts_empty() {
            let seen = Witnessed::default();
            assert!(!seen.linemode && !seen.probe && !seen.echoed);
        }
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
    _succeeded: &ObserverGate,
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
                    Some(&mut tcb),
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

// --- SYN-flood client (N16b connection-exhaustion vertical) ------------

/// First source port the flood opens from. The flood walks upward from
/// here, so every SYN presents a distinct 4-tuple and occupies its own
/// half-open backlog slot; the range stays clear of
/// [`CLIENT_LOCAL_PORT`] and of the guest's well-known
/// [`wire::GUEST_TCP_PORT`].
const FLOOD_FIRST_PORT: u16 = 0xD000;

/// How many SYNs the flood sends, once the guest's listener is confirmed
/// up, before it opens its real connection.
///
/// One more than the listener's half-open backlog, so the backlog is
/// provably full and the *next* SYN — the real one — can only be answered
/// with a stateless cookie. Read from the engine's own default rather than
/// restated, so the two cannot drift.
fn flood_syns() -> u32 {
    u32::try_from(ListenConfig::default().max_half_open).unwrap_or(u32::MAX) + 1
}

/// SYNs emitted per loop pass once the listener is confirmed up.
///
/// The backlog must be filled well inside the listener's half-open timeout,
/// or early entries expire as later ones arrive and it never actually fills.
/// The peer's receive timeout paces the loop at roughly 20 passes a second,
/// so a one-SYN-per-pass flood would take longer than that timeout; a burst
/// fills it in well under a second.
const FLOOD_BURST: u32 = 64;

/// Sequence number the flood's Nth spoofed SYN carries. Distinct per port
/// so a reply is attributable in a transcript; the flood never completes
/// these handshakes, so the value is otherwise immaterial.
fn flood_iss(index: u32) -> u32 {
    0x5A5A_0000u32.wrapping_add(index)
}

/// The flood peer's loop: fill the guest listener's half-open backlog with
/// SYNs it never answers, then open one *real* connection — which the
/// listener can therefore only admit through a stateless RFC 4987 SYN
/// cookie — stream the deterministic transfer, and verify the guest echoes
/// every byte back.
///
/// The spoofed SYNs are hand-built rather than driven by [`Tcb`]s: the point
/// is precisely that they are never completed, so there is no connection
/// state to keep. Their SYN-ACKs arrive and are simply not delivered to any
/// TCB — the peer stack is stateless for TCP and answers nothing itself — so
/// each occupies a backlog slot until the guest expires it, exactly as a real
/// flood does.
///
/// The verdict requires both halves: the backlog must have been filled *and*
/// the whole transfer echoed back over the cookie-admitted connection. A run
/// where the flood never landed, or where the real connection failed, fails
/// loud rather than passing on the ordinary accept path.
fn run_tcp_flood_peer(
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    stop: &AtomicBool,
    _succeeded: &ObserverGate,
) -> Result<(), String> {
    let start = Instant::now();
    let now = |t0: Instant| {
        Duration64::from_nanos(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX))
    };
    let (mut stack, guest_v6) = peer_stack(start)?;
    let dest = IpAddr::V6(guest_v6);

    let local_mss = stack.tcp_local_mss(dest, now(start)).unwrap_or(V6_SAFE_MSS);
    let config = TcpConfig {
        local_mss,
        ..TcpConfig::default()
    };

    let target = wire::STREAM_TRANSFER_BYTES;
    let mut flood = FloodProgress::new(flood_syns());
    let mut tcb: Option<Tcb> = None;
    let mut sent: usize = 0;
    let mut verified: usize = 0;
    let mut mismatch = false;
    let mut closed = false;
    let mut buf = [0u8; MAX_FRAME];
    let mut scratch = [0u8; MAX_FRAME];
    let mut chunk = [0u8; CLIENT_SEND_CHUNK];

    while !stop.load(Ordering::Acquire) {
        flush_engine(&mut stack, socket, qemu_sock, now(start));

        // Phase 1: fill the half-open backlog (see `emit_flood_pass`).
        emit_flood_pass(&mut stack, socket, qemu_sock, dest, &mut flood, now(start));

        // Phase 2: the real connection, opened only once the backlog is
        // provably full. Its SYN therefore meets a full backlog and can be
        // admitted only by a cookie.
        if flood.is_full() && tcb.is_none() {
            tcb = Some(Tcb::connect(
                config,
                CLIENT_LOCAL_PORT,
                wire::GUEST_TCP_PORT,
                CLIENT_ISS,
                now(start),
            ));
        }

        if let Some(tcb) = tcb.as_mut() {
            tcb.advance(now(start));
            offer_transfer(tcb, target, &mut sent, &mut chunk);
            drive_tcp_egress(tcb, &mut stack, socket, qemu_sock, guest_v6, now(start));
        }

        match socket.recv(&mut buf) {
            Ok(len) => {
                // No loss injection here: this vertical proves the cookie
                // path, and the flood already stresses the listener. The
                // retransmission path has its own vertical (N6b-2-β-2).
                // During phase 1 there is no connection: the guest's
                // unanswered SYN-ACKs are dropped, which is precisely what
                // keeps each backlog slot occupied.
                deliver_inbound_frame(
                    &mut stack,
                    tcb.as_mut(),
                    &buf[..len],
                    socket,
                    qemu_sock,
                    now(start),
                    |seg, ecn| {
                        flood.note_segment(seg);
                        ecn
                    },
                );
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("netstack peer: socket receive: {e}")),
        }

        if let Some(tcb) = tcb.as_mut() {
            let (new_verified, any_mismatch) = drain_and_verify(tcb, &mut scratch, verified);
            verified = new_verified;
            mismatch |= any_mismatch;
            if !closed && sent >= target && verified >= target {
                let _ = tcb.close(now(start));
                closed = true;
            }
            drive_tcp_egress(tcb, &mut stack, socket, qemu_sock, guest_v6, now(start));
        }
    }

    flood.verdict()?;
    if mismatch {
        return Err(
            "netstack peer: cookie-admitted echo verification failed (a byte did not match)"
                .to_string(),
        );
    }
    if verified >= target {
        Ok(())
    } else {
        Err(format!(
            "netstack peer: cookie-admitted connection incomplete: sent {sent}, verified \
             {verified} of {target} echoed bytes"
        ))
    }
}

/// Offer the next slice of the deterministic transfer to an established
/// connection's send buffer, advancing `sent` by whatever it accepted. A
/// connection still handshaking, or one whose buffer is full, accepts
/// nothing and is retried next pass.
fn offer_transfer(tcb: &mut Tcb, target: usize, sent: &mut usize, chunk: &mut [u8]) {
    if !tcb.is_established() || *sent >= target {
        return;
    }
    let len = (target - *sent).min(chunk.len());
    wire::fill_chunk(*sent, &mut chunk[..len]);
    if let Ok(accepted) = tcb.send(&chunk[..len]) {
        *sent += accepted;
    }
}

/// The flood's progress: how many SYNs have gone out, how many of those met
/// a live listener, and whether the listener has been seen answering at all.
struct FloodProgress {
    /// SYNs that must land *after* the listener is up for the backlog to be
    /// provably full.
    quota: u32,
    /// SYNs emitted in total. The source port is derived from this, so every
    /// SYN presents a distinct 4-tuple whether it was a pre-listener probe
    /// or part of the confirmed flood.
    emitted: u32,
    /// SYNs emitted since the listener came up. Only these are known to have
    /// met a live socket, so only these count toward [`Self::quota`].
    flooded: u32,
    /// Whether a SYN-ACK has come back for any flood port. Before that the
    /// guest is still booting, unlocking, and logging in, so SYNs land on no
    /// socket and are silently dropped — flooding then would fill nothing.
    listener_up: bool,
}

impl FloodProgress {
    /// A flood that has sent nothing and seen no listener.
    fn new(quota: u32) -> Self {
        Self {
            quota,
            emitted: 0,
            flooded: 0,
            listener_up: false,
        }
    }

    /// Whether enough SYNs have landed on the live listener to have filled
    /// its bounded half-open backlog.
    fn is_full(&self) -> bool {
        self.flooded >= self.quota
    }

    /// Fold one segment addressed to the peer: a SYN-ACK to a flood port is
    /// the guest's listener answering, so from here the flood lands on a
    /// live socket and can actually fill the backlog.
    fn note_segment(&mut self, seg: &TcpSegment<'_>) {
        if seg.destination_port >= FLOOD_FIRST_PORT
            && seg.flags.contains(TcpFlags::SYN)
            && seg.flags.contains(TcpFlags::ACK)
        {
            self.listener_up = true;
        }
    }

    /// Fail closed unless the flood provably filled a live listener's
    /// backlog, naming which half fell short.
    fn verdict(&self) -> Result<(), String> {
        if !self.listener_up {
            return Err(format!(
                "netstack peer: the guest listener never answered any of {} probe SYNs, so the \
                 flood never met a live socket",
                self.emitted
            ));
        }
        if !self.is_full() {
            return Err(format!(
                "netstack peer: SYN flood incomplete: {} of {} SYNs landed after the listener \
                 came up, so the backlog was never provably full",
                self.flooded, self.quota
            ));
        }
        Ok(())
    }
}

/// Emit this pass's share of the flood.
///
/// Until a SYN-ACK proves the listener is up this probes at one SYN per
/// pass: the guest is still booting, unlocking, and logging in, so anything
/// sent now lands on no socket and is silently dropped — flooding then would
/// fill nothing. Once it answers, the flood bursts, because the backlog has
/// to fill inside the listener's half-open timeout or early entries expire
/// as later ones arrive and it never actually fills.
///
/// A refused fold (the first SYNs park on neighbour resolution) ends the
/// pass with the counters untouched; the caller retries next pass, having
/// serviced inbound frames in between — the peer cannot fold a single SYN
/// until it has answered the neighbour exchange.
fn emit_flood_pass(
    stack: &mut Stack,
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    dest: IpAddr,
    progress: &mut FloodProgress,
    now: Duration64,
) {
    let burst = if progress.listener_up { FLOOD_BURST } else { 1 };
    for _ in 0..burst {
        if progress.is_full()
            || !emit_bare_syn(stack, socket, qemu_sock, dest, progress.emitted, now)
        {
            return;
        }
        progress.emitted += 1;
        if progress.listener_up {
            progress.flooded += 1;
        }
    }
}

/// Emit one bare SYN toward the guest listener from the `index`th flood
/// source port, returning whether it went on the wire.
///
/// `false` means the stack refused the fold — normally because the first
/// segment is parked on neighbour resolution — and the caller retries; the
/// SYN is deliberately option-free, since a cookie carries only an MSS
/// index and the flood's SYNs are never completed anyway.
fn emit_bare_syn(
    stack: &mut Stack,
    socket: &UnixDatagram,
    qemu_sock: &PathBuf,
    dest: IpAddr,
    index: u32,
    now: Duration64,
) -> bool {
    let meta = TcpSegmentMeta {
        source_port: FLOOD_FIRST_PORT.wrapping_add(
            u16::try_from(index % u32::from(u16::MAX - FLOOD_FIRST_PORT)).unwrap_or(0),
        ),
        destination_port: wire::GUEST_TCP_PORT,
        seq: SeqNumber::new(flood_iss(index)),
        ack: SeqNumber::new(0),
        flags: TcpFlags::SYN,
        window: 1024,
        urgent: 0,
        options: TcpOptions::default(),
    };
    let mut out = StackOutput::default();
    if stack
        .send_tcp(dest, &meta, &[], None, Ecn::NotEct, now, &mut out)
        .is_err()
    {
        return false;
    }
    let sent = !out.frames.is_empty();
    send_frames(socket, qemu_sock, &out.frames);
    sent
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
    _succeeded: &ObserverGate,
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
                let drop = tcb.is_established()
                    && loss_budget > 0
                    && inbound_seen.is_multiple_of(LOSS_EVERY);
                inbound_seen += 1;
                if drop {
                    loss_budget -= 1;
                } else {
                    deliver_inbound_frame(
                        &mut stack,
                        Some(&mut tcb),
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
/// `tcb` is [`None`] for a peer phase that holds no connection yet — the
/// SYN-flood peer's backlog-filling phase, which must still service inbound
/// frames so neighbour resolution completes and its hand-built SYNs can be
/// folded at all. `on_seg` still sees every segment addressed to the peer in
/// that phase (the flood watches for SYN-ACKs, its proof the guest's listener
/// is up), but nothing acknowledges them — which is precisely what keeps each
/// half-open backlog slot occupied.
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
    tcb: Option<&mut Tcb>,
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
    let mut tcb = tcb;
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
                        if let Some(tcb) = tcb.as_deref_mut() {
                            tcb.on_segment(&seg, delivered_ecn, now);
                        }
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
