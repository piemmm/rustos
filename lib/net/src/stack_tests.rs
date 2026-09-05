//! Unit and end-to-end tests for the host engine: two [`Stack`]
//! instances wired back-to-back over an in-memory link, plus a
//! hand-rolled router for the RA/SLAAC path.

use super::*;
use crate::ipv6::HBH_ROUTER_ALERT_LEN;
use crate::test_support::temp_source;
// `admit` is the `RxAdmit` seam every driver's harvest path calls.
use alloc::collections::VecDeque;
use tairix_abi::driver::net_ring::RxAdmit;

const MAC_A: MacAddress = MacAddress([0x02, 0xAA, 0, 0, 0, 0x01]);
const MAC_B: MacAddress = MacAddress([0x02, 0xBB, 0, 0, 0, 0x02]);
const ROUTER_MAC: MacAddress = MacAddress([0x02, 0xCC, 0, 0, 0, 0x03]);
const IID_A: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0xA1];
const IID_B: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0xB2];
const V4_A: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const V4_B: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

fn t(secs: i64) -> Duration64 {
    Duration64::from_secs(secs)
}

fn facts(mac: MacAddress) -> DeviceFacts {
    DeviceFacts {
        mac,
        mtu: 1500,
        link: LinkState::Up,
        offloads: tairix_abi::driver::net::NetOffloads::empty(),
        rx_queues: 1,
        max_tx_frame: 1500 + tairix_abi::driver::net::ETHERNET_HEADER_LEN,
        multicast_filter: tairix_abi::driver::net::McastFilter::Unfiltered,
    }
}

fn stack(mac: MacAddress, iid: [u8; 8]) -> Stack {
    Stack::new(
        &StackConfig::new(facts(mac), iid, 0x1234, STACK_HASH_KEY),
        temp_source(),
        t(0),
    )
    .expect("valid facts")
}

fn link_local(iid: [u8; 8]) -> Ipv6Addr {
    let mut octets = [0u8; 16];
    octets[0] = 0xFE;
    octets[1] = 0x80;
    octets[8..].copy_from_slice(&iid);
    Ipv6Addr::from(octets)
}

/// The emitted frames' bytes, dropping the transmit-offload metadata a
/// live device would consume — most tests pump raw bytes back through
/// `on_frame` and inspect them, so this keeps the harness on `Vec<u8>`.
/// Takes the `frames` field (not the whole [`StackOutput`]) so a caller
/// may still read `events` from the same output.
fn tx_bytes(frames: Vec<TxFrame>) -> Vec<Vec<u8>> {
    frames.into_iter().map(|f| f.bytes).collect()
}

/// Run both stacks' timers forward to `now` and exchange every frame
/// until the link is quiet, collecting each side's events.
fn pump(
    a: &mut Stack,
    b: &mut Stack,
    now: Duration64,
    events_a: &mut Vec<StackEvent>,
    events_b: &mut Vec<StackEvent>,
) {
    let mut to_b: VecDeque<TxFrame> = VecDeque::new();
    let mut to_a: VecDeque<TxFrame> = VecDeque::new();
    let out_a = a.advance_collect(now);
    events_a.extend(out_a.events);
    to_b.extend(out_a.frames);
    let out_b = b.advance_collect(now);
    events_b.extend(out_b.events);
    to_a.extend(out_b.frames);
    // Bounded exchange: control-plane conversations settle quickly.
    for _ in 0..64 {
        if to_a.is_empty() && to_b.is_empty() {
            break;
        }
        if let Some(frame) = to_b.pop_front() {
            let out = b.on_frame_collect(&frame.bytes, now);
            events_b.extend(out.events);
            to_a.extend(out.frames);
        }
        if let Some(frame) = to_a.pop_front() {
            let out = a.on_frame_collect(&frame.bytes, now);
            events_a.extend(out.events);
            to_b.extend(out.frames);
        }
    }
    assert!(
        to_a.is_empty() && to_b.is_empty(),
        "conversation did not settle"
    );
}

/// Bring both stacks through DAD (t0: DAD NS, t1: preferred + RS).
fn bring_up(a: &mut Stack, b: &mut Stack) -> (Vec<StackEvent>, Vec<StackEvent>) {
    let mut events_a = Vec::new();
    let mut events_b = Vec::new();
    pump(a, b, t(0), &mut events_a, &mut events_b);
    pump(a, b, t(1), &mut events_a, &mut events_b);
    assert!(events_a.contains(&StackEvent::AddressPreferred {
        addr: link_local(IID_A)
    }));
    assert!(events_b.contains(&StackEvent::AddressPreferred {
        addr: link_local(IID_B)
    }));
    (events_a, events_b)
}

/// A counting RNG for the DHCP client: distinct, deterministic values.
fn dhcp_counter() -> alloc::boxed::Box<dyn FnMut() -> u32> {
    let mut n: u32 = 0x1000_0000;
    alloc::boxed::Box::new(move || {
        n = n.wrapping_add(0x0101_0101);
        n
    })
}

/// A DHCPv4-configured client stack, IPv6 disabled so only DHCP frames
/// egress (the test inspects them by index).
fn dhcp_client() -> Stack {
    let mut c = stack(MAC_A, IID_A);
    c.set_ipv6_enabled(false, t(0));
    c.enable_dhcp(dhcp_counter());
    c
}

const DHCP_SERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DHCP_LEASED: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 50);
const DHCP_MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
/// The two recursive DNS servers the test server leases (option 6).
const DHCP_DNS_1: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const DHCP_DNS_2: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 4);
/// The two network time servers the test server leases (option 42).
const DHCP_NTP_1: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 5);
const DHCP_NTP_2: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 6);

/// The DHCP message bytes carried by a single emitted frame (Ethernet +
/// IPv4 + UDP stripped), plus the UDP ports, or `None` if it is not a
/// UDP datagram.
fn dhcp_out(frame: &[u8]) -> Option<(u16, u16, Vec<u8>)> {
    let eth = EthernetFrame::parse(frame)?;
    let (hdr, _o, payload) = Ipv4Header::parse(eth.payload)?;
    let pseudo = crate::udp::Pseudo::V4 {
        source: hdr.source,
        destination: hdr.destination,
    };
    let dg = UdpDatagram::parse(pseudo, payload)?;
    Some((dg.source_port, dg.destination_port, dg.payload.to_vec()))
}

/// Build a server→client DHCP reply frame (broadcast at layer 2, since the
/// client has no address yet): BOOTREPLY with the given type and options,
/// wrapped in UDP(67→68)/IPv4(server→255.255.255.255)/Ethernet.
fn dhcp_server_frame(msg: u8, xid: u32, lease_secs: Option<u32>) -> Vec<u8> {
    let mut dhcp = alloc::vec![0u8; 240];
    dhcp[0] = 2; // BOOTREPLY
    dhcp[1] = 1; // htype Ethernet
    dhcp[2] = 6; // hlen
    dhcp[4..8].copy_from_slice(&xid.to_be_bytes());
    dhcp[16..20].copy_from_slice(&DHCP_LEASED.octets());
    dhcp[28..34].copy_from_slice(&MAC_A.0);
    dhcp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let mut opt = |code: u8, data: &[u8]| {
        dhcp.push(code);
        dhcp.push(u8::try_from(data.len()).expect("fits"));
        dhcp.extend_from_slice(data);
    };
    opt(53, &[msg]);
    opt(54, &DHCP_SERVER.octets());
    opt(1, &DHCP_MASK.octets());
    opt(3, &DHCP_SERVER.octets());
    // DNS servers (option 6): two addresses, concatenated four-octet each.
    let mut dns = [0u8; 8];
    dns[..4].copy_from_slice(&DHCP_DNS_1.octets());
    dns[4..].copy_from_slice(&DHCP_DNS_2.octets());
    opt(6, &dns);
    // Time servers (option 42), same four-octet concatenation.
    let mut ntp = [0u8; 8];
    ntp[..4].copy_from_slice(&DHCP_NTP_1.octets());
    ntp[4..].copy_from_slice(&DHCP_NTP_2.octets());
    opt(42, &ntp);
    if let Some(l) = lease_secs {
        opt(51, &l.to_be_bytes());
    }
    dhcp.push(255);
    let src = DHCP_SERVER;
    let dst = Ipv4Addr::BROADCAST;
    let mut udpbuf = alloc::vec![0u8; crate::udp::UDP_HEADER_LEN + dhcp.len()];
    crate::udp::write(
        crate::udp::Pseudo::V4 {
            source: src,
            destination: dst,
        },
        67,
        68,
        &dhcp,
        &mut udpbuf,
    )
    .expect("udp");
    let ip = Ipv4Header::new(src, dst, crate::udp::PROTOCOL_UDP);
    let mut packet = alloc::vec![0u8; IPV4_HEADER_LEN + udpbuf.len()];
    ip.write(&mut packet, udpbuf.len()).expect("ipv4");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udpbuf);
    let mut frame = alloc::vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, BROADCAST, MAC_B, ETHERTYPE_IPV4).expect("eth");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// The client's outstanding transaction id, read from the DISCOVER it
/// emits at bring-up.
fn discover_xid(discover: &[u8]) -> u32 {
    let (src, dst, msg) = dhcp_out(discover).expect("udp");
    assert_eq!((src, dst), (68, 67), "DISCOVER is client 68 → server 67");
    u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]])
}

/// The DHCP message-type (option 53) carried in a client message's option
/// region (from offset 240, past the cookie), by scanning the TLVs.
fn dhcp_msg_type(msg: &[u8]) -> Option<u8> {
    let mut i = 240;
    while i < msg.len() {
        match msg[i] {
            255 => return None,
            0 => i += 1,
            code => {
                let len = *msg.get(i + 1)? as usize;
                if code == 53 {
                    return msg.get(i + 2).copied();
                }
                i += 2 + len;
            }
        }
    }
    None
}

#[test]
fn dhcp_acquires_a_lease_end_to_end() {
    let mut c = dhcp_client();
    // First advance emits a broadcast DISCOVER.
    let out = c.advance_collect(t(0));
    assert_eq!(out.frames.len(), 1, "one DISCOVER frame");
    let xid = discover_xid(&out.frames[0].bytes);
    assert!(c.iface().ipv4().is_none(), "no address before the lease");

    // The server OFFERs; the client REQUESTs it.
    let offer = dhcp_server_frame(2, xid, Some(3600));
    let out = c.on_frame_collect(&offer, t(0));
    let request = out
        .frames
        .iter()
        .find(|f| dhcp_out(&f.bytes).map(|(s, d, _)| (s, d)) == Some((68, 67)))
        .expect("a REQUEST frame");
    let (_s, _d, req_msg) = dhcp_out(&request.bytes).expect("udp");
    assert_eq!(
        dhcp_msg_type(&req_msg),
        Some(3),
        "the client message is a REQUEST"
    );

    // The server ACKs; the lease is applied and audited.
    let ack = dhcp_server_frame(5, xid, Some(3600));
    let out = c.on_frame_collect(&ack, t(0));
    assert_eq!(
        c.iface().ipv4(),
        Some((DHCP_LEASED, 24)),
        "the leased address and /24 mask are applied"
    );
    assert!(out.events.iter().any(|e| matches!(
        e,
        StackEvent::DhcpLeaseAcquired { address, prefix_len, router }
            if *address == DHCP_LEASED && *prefix_len == 24 && *router == Some(DHCP_SERVER)
    )));
    // The default route through the leased router reaches an off-link host.
    assert_eq!(c.next_hop_v4(Ipv4Addr::new(8, 8, 8, 8)), Some(DHCP_SERVER));
}

#[test]
fn dhcp_reply_is_intercepted_before_the_address_filter() {
    // A DHCP reply arrives at 255.255.255.255 while the client has no
    // address; the normal IPv4 path would drop it, but the client
    // consumes it and it never surfaces as an ordinary UDP datagram.
    let mut c = dhcp_client();
    let out = c.advance_collect(t(0));
    let xid = discover_xid(&out.frames[0].bytes);
    let offer = dhcp_server_frame(2, xid, Some(3600));
    let out = c.on_frame_collect(&offer, t(0));
    assert!(
        !out.events
            .iter()
            .any(|e| matches!(e, StackEvent::UdpDatagram { .. })),
        "a DHCP reply is never surfaced as an ordinary datagram"
    );
}

#[test]
fn dhcp_lease_expiry_withdraws_the_address() {
    let mut c = dhcp_client();
    let xid = discover_xid(&c.advance_collect(t(0)).frames[0].bytes);
    c.on_frame_collect(&dhcp_server_frame(2, xid, Some(3600)), t(0));
    c.on_frame_collect(&dhcp_server_frame(5, xid, Some(3600)), t(0));
    assert!(c.iface().ipv4().is_some(), "lease held");
    // Drive the timers with no server answering: RENEWING at T1 (1800 s),
    // REBINDING at T2 (3150 s), then withdrawal at expiry (3600 s).
    c.advance_collect(t(1800));
    c.advance_collect(t(3150));
    let out = c.advance_collect(t(3600));
    assert!(
        c.iface().ipv4().is_none(),
        "the lease is withdrawn at expiry"
    );
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, StackEvent::DhcpLeaseLost)));
}

#[test]
fn disable_dhcp_withdraws_the_lease_and_stops_the_client() {
    let mut c = dhcp_client();
    let xid = discover_xid(&c.advance_collect(t(0)).frames[0].bytes);
    c.on_frame_collect(&dhcp_server_frame(2, xid, Some(3600)), t(0));
    c.on_frame_collect(&dhcp_server_frame(5, xid, Some(3600)), t(0));
    assert!(c.dhcp_active() && c.iface().ipv4().is_some());
    c.disable_dhcp();
    assert!(!c.dhcp_active(), "the client is stopped");
    assert!(c.iface().ipv4().is_none(), "its lease is withdrawn");
}

#[test]
fn dhcp_dns_servers_are_surfaced_from_the_lease() {
    let mut c = dhcp_client();
    assert!(
        c.dhcp_dns_servers().is_empty(),
        "no learned servers before a lease"
    );
    let xid = discover_xid(&c.advance_collect(t(0)).frames[0].bytes);
    c.on_frame_collect(&dhcp_server_frame(2, xid, Some(3600)), t(0));
    c.on_frame_collect(&dhcp_server_frame(5, xid, Some(3600)), t(0));
    assert_eq!(
        c.dhcp_dns_servers(),
        alloc::vec![IpAddr::V4(DHCP_DNS_1), IpAddr::V4(DHCP_DNS_2)],
        "the lease's DNS servers are surfaced in wire order"
    );

    // Withdrawal (a lost lease) returns the client to INIT and clears the
    // learned servers — they are derived from the current lease, not a
    // stale copy.
    c.advance_collect(t(1800));
    c.advance_collect(t(3150));
    c.advance_collect(t(3600));
    assert!(
        c.dhcp_dns_servers().is_empty(),
        "the learned servers are gone once the lease is withdrawn"
    );
}

// --- DHCPv6 (RFC 8415) --------------------------------------------------

/// A DHCPv6-configured client stack (IPv6 enabled — DHCPv6 rides on the
/// link-local). The engine's transaction id + jitter come from the same
/// deterministic counter the v4 tests use.
fn dhcp6_client() -> Stack {
    let mut c = stack(MAC_A, IID_A);
    c.enable_dhcp6(dhcp_counter(), t(0));
    c
}

/// The leased IA_NA address the test server hands out.
const DHCP6_LEASED: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x1234);
/// The two recursive DNS servers the test server leases (RFC 3646 option 23).
const DHCP6_DNS_1: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x53);
const DHCP6_DNS_2: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x54);
/// The two network time servers the test server leases (RFC 5908 option 56).
const DHCP6_NTP_1: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x7B);
const DHCP6_NTP_2: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x7C);
/// A time server offered in the RFC 4075 option 31 form option 56 supersedes.
const DHCP6_SNTP: Ipv6Addr = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 0x1F);
/// The client's IA identifier (derived from `MAC_A`'s low four octets).
const DHCP6_IAID: u32 = 1;
/// The test server's DUID (a DUID-LL of `MAC_B`).
const SERVER_DUID6: [u8; 10] = [0, 3, 0, 1, 0x02, 0xBB, 0, 0, 0, 0x02];

/// The DHCPv6 message bytes carried by one emitted frame (the Ethernet,
/// IPv6, and UDP headers stripped) plus the UDP ports, or `None` if it is
/// not a UDP datagram.
fn dhcp6_out(frame: &[u8]) -> Option<(u16, u16, Vec<u8>)> {
    let eth = EthernetFrame::parse(frame)?;
    if eth.ethertype != ETHERTYPE_IPV6 {
        return None;
    }
    let (hdr, payload) = Ipv6Header::parse(eth.payload)?;
    if hdr.next_header != crate::udp::PROTOCOL_UDP {
        return None;
    }
    let pseudo = crate::udp::Pseudo::V6 {
        source: hdr.source,
        destination: hdr.destination,
    };
    let dg = UdpDatagram::parse(pseudo, payload)?;
    Some((dg.source_port, dg.destination_port, dg.payload.to_vec()))
}

/// The 24-bit transaction id from a DHCPv6 message's header.
fn dhcp6_xid(msg: &[u8]) -> u32 {
    u32::from_be_bytes([0, msg[1], msg[2], msg[3]])
}

/// Append a DHCPv6 option (2-byte code, 2-byte length, body).
fn push_opt6(out: &mut Vec<u8>, code: u16, data: &[u8]) {
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(&u16::try_from(data.len()).expect("fits").to_be_bytes());
    out.extend_from_slice(data);
}

/// Build a server→client DHCPv6 message (Advertise `2` or Reply `7`) with
/// one `IA_NA`/IAADDR, wrapped in UDP(547→546)/IPv6(server-LL→client-LL)/
/// Ethernet.
fn dhcp6_server_frame(mt: u8, xid: u32, preferred: u32, valid: u32, t1: u32, t2: u32) -> Vec<u8> {
    let mut msg = alloc::vec![mt];
    msg.extend_from_slice(&xid.to_be_bytes()[1..4]);
    // Client Identifier (echoing our DUID-LL) and Server Identifier.
    push_opt6(
        &mut msg,
        1,
        crate::dhcpv6::Duid::ll_ethernet(MAC_A).as_slice(),
    );
    push_opt6(&mut msg, 2, &SERVER_DUID6);
    // IA_NA: IAID, T1, T2, then the encapsulated IA Address.
    let mut ia = Vec::new();
    ia.extend_from_slice(&DHCP6_IAID.to_be_bytes());
    ia.extend_from_slice(&t1.to_be_bytes());
    ia.extend_from_slice(&t2.to_be_bytes());
    let mut iaddr = Vec::new();
    iaddr.extend_from_slice(&DHCP6_LEASED.octets());
    iaddr.extend_from_slice(&preferred.to_be_bytes());
    iaddr.extend_from_slice(&valid.to_be_bytes());
    push_opt6(&mut ia, 5, &iaddr);
    push_opt6(&mut msg, 3, &ia);
    // DNS Recursive Name Server (option 23): two addresses, 16 octets each.
    let mut dns = [0u8; 32];
    dns[..16].copy_from_slice(&DHCP6_DNS_1.octets());
    dns[16..].copy_from_slice(&DHCP6_DNS_2.octets());
    push_opt6(&mut msg, 23, &dns);
    // NTP Server (option 56): one `SRV_ADDR` sub-option per server.
    let mut ntp = Vec::new();
    for server in [DHCP6_NTP_1, DHCP6_NTP_2] {
        push_opt6(&mut ntp, 1, &server.octets());
    }
    push_opt6(&mut msg, 56, &ntp);
    // The superseded RFC 4075 spelling as well, so the reply exercises the
    // precedence rule rather than only one option at a time.
    push_opt6(&mut msg, 31, &DHCP6_SNTP.octets());
    // UDP 547 → 546 over IPv6, server link-local → client link-local.
    let src = link_local(IID_B);
    let dst = link_local(IID_A);
    let mut udpbuf = alloc::vec![0u8; crate::udp::UDP_HEADER_LEN + msg.len()];
    crate::udp::write(
        crate::udp::Pseudo::V6 {
            source: src,
            destination: dst,
        },
        547,
        546,
        &msg,
        &mut udpbuf,
    )
    .expect("udp");
    let ip = Ipv6Header::new(src, dst, crate::udp::PROTOCOL_UDP);
    let packet = ipv6_packet(&ip, &udpbuf).expect("ipv6");
    let mut frame = alloc::vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV6).expect("eth");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// Advance the client past link-local DAD until its first Solicit egresses,
/// returning that Solicit's transaction id.
fn dhcp6_solicit_xid(c: &mut Stack) -> u32 {
    let mut xid = None;
    for s in 0..=2 {
        for f in c.advance_collect(t(s)).frames {
            if let Some((546, 547, msg)) = dhcp6_out(&f.bytes) {
                if msg[0] == 1 {
                    xid = Some(dhcp6_xid(&msg));
                }
            }
        }
    }
    xid.expect("a Solicit egressed")
}

/// The transaction id of the Request the client emits in response to
/// `frames` (its Solicit → Advertise → Request step).
fn dhcp6_request_xid(frames: &[TxFrame]) -> u32 {
    let request = frames
        .iter()
        .find_map(|f| {
            let (sp, dp, msg) = dhcp6_out(&f.bytes)?;
            (sp == 546 && dp == 547 && msg[0] == 3).then_some(msg)
        })
        .expect("a Request egressed");
    dhcp6_xid(&request)
}

/// Drive a fresh DHCPv6 client through Solicit/Advertise/Request/Reply to a
/// committed lease at `t(2)`.
fn dhcp6_drive_to_bound(c: &mut Stack) {
    let sol_xid = dhcp6_solicit_xid(c);
    let advertise = dhcp6_server_frame(2, sol_xid, 3600, 7200, 1800, 2880);
    let out = c.on_frame_collect(&advertise, t(2));
    let req_xid = dhcp6_request_xid(&out.frames);
    let reply = dhcp6_server_frame(7, req_xid, 3600, 7200, 1800, 2880);
    c.on_frame_collect(&reply, t(2));
}

/// Whether the interface holds a DHCPv6-leased address.
fn has_dhcp6_addr(c: &Stack) -> bool {
    c.iface()
        .ipv6_addresses()
        .iter()
        .any(|a| a.origin == crate::iface::AddrOrigin::Dhcp)
}

#[test]
fn dhcp6_acquires_a_lease_end_to_end() {
    let mut c = dhcp6_client();
    let sol_xid = dhcp6_solicit_xid(&mut c);
    assert!(!has_dhcp6_addr(&c), "no leased address before the lease");

    // The server Advertises; the client Requests the offered address.
    let advertise = dhcp6_server_frame(2, sol_xid, 3600, 7200, 1800, 2880);
    let out = c.on_frame_collect(&advertise, t(2));
    let req_xid = dhcp6_request_xid(&out.frames);

    // The server Replies; the lease is applied as a /128 and audited.
    let reply = dhcp6_server_frame(7, req_xid, 3600, 7200, 1800, 2880);
    let out = c.on_frame_collect(&reply, t(2));
    assert!(
        c.iface()
            .ipv6_addresses()
            .iter()
            .any(|a| a.addr == DHCP6_LEASED
                && a.prefix_len == 128
                && a.origin == crate::iface::AddrOrigin::Dhcp),
        "the leased IA_NA address is applied as a host /128"
    );
    assert!(out.events.iter().any(|e| matches!(
        e,
        StackEvent::Dhcp6LeaseAcquired { address, valid_lifetime }
            if *address == DHCP6_LEASED && *valid_lifetime == 7200
    )));
}

#[test]
fn dhcp6_reply_is_intercepted_before_the_address_filter() {
    // A DHCPv6 reply is consumed by the client and never surfaces as an
    // ordinary datagram.
    let mut c = dhcp6_client();
    let sol_xid = dhcp6_solicit_xid(&mut c);
    let advertise = dhcp6_server_frame(2, sol_xid, 3600, 7200, 1800, 2880);
    let out = c.on_frame_collect(&advertise, t(2));
    assert!(
        !out.events
            .iter()
            .any(|e| matches!(e, StackEvent::UdpDatagram { .. })),
        "a DHCPv6 reply is never surfaced as an ordinary datagram"
    );
}

#[test]
fn dhcp6_lease_expiry_withdraws_the_address() {
    let mut c = dhcp6_client();
    dhcp6_drive_to_bound(&mut c);
    assert!(has_dhcp6_addr(&c), "lease held");
    // Drive the timers with no server answering: RENEWING at T1 (1802 s),
    // REBINDING at T2 (2882 s), then withdrawal at expiry (7202 s).
    c.advance_collect(t(1802));
    c.advance_collect(t(2882));
    let out = c.advance_collect(t(7202));
    assert!(!has_dhcp6_addr(&c), "the lease is withdrawn at expiry");
    assert!(out
        .events
        .iter()
        .any(|e| matches!(e, StackEvent::Dhcp6LeaseLost)));
}

#[test]
fn disable_dhcp6_withdraws_the_lease_and_stops_the_client() {
    let mut c = dhcp6_client();
    dhcp6_drive_to_bound(&mut c);
    assert!(c.dhcp6_active() && has_dhcp6_addr(&c));
    c.disable_dhcp6();
    assert!(!c.dhcp6_active(), "the client is stopped");
    assert!(!has_dhcp6_addr(&c), "its lease is withdrawn");
}

#[test]
fn dhcp6_dns_servers_are_surfaced_from_the_lease() {
    let mut c = dhcp6_client();
    assert!(
        c.dhcp_dns_servers().is_empty(),
        "no learned servers before a lease"
    );
    dhcp6_drive_to_bound(&mut c);
    assert_eq!(
        c.dhcp_dns_servers(),
        alloc::vec![IpAddr::V6(DHCP6_DNS_1), IpAddr::V6(DHCP6_DNS_2)],
        "the DHCPv6 lease's DNS servers are surfaced in wire order"
    );
    c.disable_dhcp6();
    assert!(
        c.dhcp_dns_servers().is_empty(),
        "the learned servers are gone once the lease is withdrawn"
    );
}

#[test]
fn dhcp6_ntp_servers_prefer_the_rfc_5908_option_over_the_one_it_supersedes() {
    let mut c = dhcp6_client();
    assert!(
        c.dhcp_ntp_servers().is_empty(),
        "no learned time servers before a lease"
    );
    dhcp6_drive_to_bound(&mut c);
    assert_eq!(
        c.dhcp_ntp_servers(),
        alloc::vec![IpAddr::V6(DHCP6_NTP_1), IpAddr::V6(DHCP6_NTP_2)],
        "the option-56 servers are surfaced and the option-31 one ignored"
    );
    c.disable_dhcp6();
    assert!(
        c.dhcp_ntp_servers().is_empty(),
        "the learned time servers are gone once the lease is withdrawn"
    );
}

#[test]
fn dhcp_ntp_servers_are_surfaced_from_the_lease() {
    let mut c = dhcp_client();
    assert!(
        c.dhcp_ntp_servers().is_empty(),
        "no learned time servers before a lease"
    );
    let xid = discover_xid(&c.advance_collect(t(0)).frames[0].bytes);
    c.on_frame_collect(&dhcp_server_frame(2, xid, Some(3600)), t(0));
    c.on_frame_collect(&dhcp_server_frame(5, xid, Some(3600)), t(0));
    assert_eq!(
        c.dhcp_ntp_servers(),
        alloc::vec![IpAddr::V4(DHCP_NTP_1), IpAddr::V4(DHCP_NTP_2)],
        "the lease's time servers are surfaced in wire order"
    );

    // Withdrawal (a lost lease) returns the client to INIT: the servers are
    // derived from the current lease, never a stale copy.
    c.advance_collect(t(1800));
    c.advance_collect(t(3150));
    c.advance_collect(t(3600));
    assert!(
        c.dhcp_ntp_servers().is_empty(),
        "the learned time servers are gone once the lease is withdrawn"
    );
}

#[test]
fn dhcp_dns_servers_are_v4_then_v6_across_both_families() {
    // A dual-stack interface running both DHCP clients surfaces the IPv4
    // lease's servers first, then the IPv6 lease's (the aggregation order
    // the netstack resolver set relies on).
    let mut c = stack(MAC_A, IID_A);
    c.enable_dhcp(dhcp_counter());
    c.enable_dhcp6(dhcp_counter(), t(0));
    // With IPv6 enabled the first advance also emits link-local bring-up
    // frames, so locate the DISCOVER rather than assume a frame index.
    let out = c.advance_collect(t(0));
    let discover = out
        .frames
        .iter()
        .find(|f| dhcp_out(&f.bytes).map(|(s, d, _)| (s, d)) == Some((68, 67)))
        .expect("a DISCOVER frame");
    let xid = discover_xid(&discover.bytes);
    c.on_frame_collect(&dhcp_server_frame(2, xid, Some(3600)), t(0));
    c.on_frame_collect(&dhcp_server_frame(5, xid, Some(3600)), t(0));
    dhcp6_drive_to_bound(&mut c);
    assert_eq!(
        c.dhcp_dns_servers(),
        alloc::vec![
            IpAddr::V4(DHCP_DNS_1),
            IpAddr::V4(DHCP_DNS_2),
            IpAddr::V6(DHCP6_DNS_1),
            IpAddr::V6(DHCP6_DNS_2),
        ],
        "v4 servers precede v6 servers"
    );
}

#[test]
fn construction_refuses_bad_device_facts() {
    let mut bad = facts(MAC_A);
    bad.mtu = 0;
    assert_eq!(
        Stack::new(
            &StackConfig::new(bad, IID_A, 0, STACK_HASH_KEY),
            temp_source(),
            t(0)
        )
        .err(),
        Some(StackError::BadDeviceFacts)
    );
}

#[test]
fn ipv4_ping_resolves_arp_and_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    let mut events_a = Vec::new();
    let mut events_b = Vec::new();

    // The echo request parks on ARP resolution and the request goes
    // out after the exchange settles.
    let out = a
        .send_echo_request_collect(IpAddr::V4(V4_B), 0x77, 1, b"tairix-stack", t(2))
        .expect("send");
    assert!(out.events.is_empty());
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(2));
            events_a.extend(out_a.events);
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert_eq!(
        events_a,
        [StackEvent::EchoReply {
            source: IpAddr::V4(V4_B),
            identifier: 0x77,
            sequence: 1,
            payload: b"tairix-stack".to_vec(),
        }]
    );
    // The responder observed the inbound request it answered.
    assert!(events_b.contains(&StackEvent::EchoRequestServed {
        source: IpAddr::V4(V4_A),
        identifier: 0x77,
        sequence: 1,
    }));
    // Both ends resolved each other.
    assert!(matches!(
        a.neighbors.entry(IpAddr::V4(V4_B)),
        Some((_, Some(mac))) if mac == MAC_B
    ));
    assert!(matches!(
        b.neighbors.entry(IpAddr::V4(V4_A)),
        Some((_, Some(mac))) if mac == MAC_A
    ));
}

#[test]
fn ipv6_link_local_ping_resolves_nd_and_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    let (mut events_a, mut events_b) = bring_up(&mut a, &mut b);
    events_a.clear();
    events_b.clear();

    let out = a
        .send_echo_request_collect(IpAddr::V6(link_local(IID_B)), 0x42, 7, b"ping6", t(3))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(3));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(3));
            events_a.extend(out_a.events);
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert_eq!(
        events_a,
        [StackEvent::EchoReply {
            source: IpAddr::V6(link_local(IID_B)),
            identifier: 0x42,
            sequence: 7,
            payload: b"ping6".to_vec(),
        }]
    );
    // The responder observed the inbound request it answered.
    assert!(events_b.contains(&StackEvent::EchoRequestServed {
        source: IpAddr::V6(link_local(IID_A)),
        identifier: 0x42,
        sequence: 7,
    }));
    assert!(matches!(
        a.neighbors.entry(IpAddr::V6(link_local(IID_B))),
        Some((_, Some(mac))) if mac == MAC_B
    ));
    assert!(matches!(
        b.neighbors.entry(IpAddr::V6(link_local(IID_A))),
        Some((_, Some(mac))) if mac == MAC_A
    ));
}

// --- Hand-rolled router frames (RA emission is router behaviour the
// --- host codec deliberately does not provide).

const ROUTER_LL: Ipv6Addr = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 0xCC);

fn slaac_prefix() -> Ipv6Addr {
    Ipv6Addr::new(0x2001, 0x0DB8, 0, 0, 0, 0, 0, 0)
}

/// Build a Router Advertisement frame from the fake router: one
/// on-link + autonomous /64 prefix, source link-layer option.
fn router_advertisement_frame(dest: Ipv6Addr, dest_mac: MacAddress) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(64); // cur hop limit
    body.push(0); // flags
    body.extend_from_slice(&1800u16.to_be_bytes()); // router lifetime
    body.extend_from_slice(&0u32.to_be_bytes()); // reachable
    body.extend_from_slice(&0u32.to_be_bytes()); // retrans
                                                 // Source link-layer option.
    body.extend_from_slice(&[1, 1]);
    body.extend_from_slice(ROUTER_MAC.as_octets());
    // Prefix information option (type 3, len 4).
    body.push(3);
    body.push(4);
    body.push(64); // prefix length
    body.push(0xC0); // on-link | autonomous
    body.extend_from_slice(&3600u32.to_be_bytes()); // valid
    body.extend_from_slice(&1800u32.to_be_bytes()); // preferred
    body.extend_from_slice(&0u32.to_be_bytes()); // reserved
    body.extend_from_slice(&slaac_prefix().octets());
    icmpv6_frame_from_router(dest, dest_mac, crate::nd::TYPE_ROUTER_ADVERTISEMENT, &body)
}

/// Wrap an `ICMPv6` body in IPv6 + Ethernet from the fake router.
fn icmpv6_frame_from_router(
    dest: Ipv6Addr,
    dest_mac: MacAddress,
    message_type: u8,
    body: &[u8],
) -> Vec<u8> {
    let message = IcmpMessage {
        message_type,
        code: 0,
        body,
    };
    let context = IcmpContext::V6 {
        source: ROUTER_LL,
        destination: dest,
    };
    let mut icmp = vec![0u8; crate::icmp::ICMP_FIXED_HEADER_LEN + body.len()];
    message.write(context, &mut icmp).expect("icmp fits");
    let mut header = Ipv6Header::new(ROUTER_LL, dest, NEXT_HEADER_ICMPV6);
    header.hop_limit = ND_HOP_LIMIT;
    let packet = ipv6_packet(&header, &icmp).expect("packet fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, dest_mac, ROUTER_MAC, ETHERTYPE_IPV6).expect("header fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

#[test]
fn router_advertisement_configures_slaac_and_default_route() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);

    let ra = router_advertisement_frame(ALL_NODES, ipv6_multicast_mac(&ALL_NODES));
    let out = a.on_frame_collect(&ra, t(2));
    assert!(out.frames.is_empty());
    // The SLAAC address forms and completes DAD.
    let mut expected = slaac_prefix().octets();
    expected[8..].copy_from_slice(&IID_A);
    let slaac_addr = Ipv6Addr::from(expected);
    assert!(a.iface().is_tentative(slaac_addr));
    a.advance_collect(t(2)); // DAD transmit
    let out = a.advance_collect(t(3)); // DAD completion
    assert!(out
        .events
        .contains(&StackEvent::AddressPreferred { addr: slaac_addr }));

    // An off-link destination now routes via the advertised router.
    let off_link = Ipv6Addr::new(0x2001, 0x0DB8, 0xFF, 0, 0, 0, 0, 1);
    // (2001:db8:ff::1 is outside the /64, so it uses the default
    // router learned from the RA.)
    let out = a
        .send_echo_request_collect(IpAddr::V6(off_link), 1, 1, b"x", t(3))
        .expect("routed via default router");
    // The parked echo triggers a unicast NS to the router (its MAC is
    // already learned from the RA's source option, entry Stale).
    assert!(!out.frames.is_empty());
}

#[test]
fn hostile_ra_from_non_link_local_source_is_ignored() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    let mut frame = router_advertisement_frame(ALL_NODES, ipv6_multicast_mac(&ALL_NODES));
    // Rewrite the source to a global address (and refresh the
    // checksum by rebuilding is overkill: the checksum covers the
    // pseudo-header, so this corruption must fail the parse or the
    // source rule — either way, no SLAAC address may form).
    frame[ETHERNET_HEADER_LEN + 8] = 0x20;
    a.on_frame_collect(&frame, t(2));
    a.advance_collect(t(2));
    a.advance_collect(t(3));
    let mut expected = slaac_prefix().octets();
    expected[8..].copy_from_slice(&IID_A);
    assert!(!a.iface().is_assigned(Ipv6Addr::from(expected)));
}

#[test]
fn dad_probe_from_another_node_fails_our_tentative_address() {
    let mut a = stack(MAC_A, IID_A);
    a.advance_collect(t(0)); // link-local DAD NS out (still tentative)
    let target = link_local(IID_A);
    // Another node's DAD probe for the same address: NS from the
    // unspecified source to the solicited-node group.
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&target.octets());
    let group = solicited_node_multicast(&target);
    let message = IcmpMessage {
        message_type: crate::nd::TYPE_NEIGHBOR_SOLICITATION,
        code: 0,
        body: &body,
    };
    let context = IcmpContext::V6 {
        source: Ipv6Addr::from([0u8; 16]),
        destination: group,
    };
    let mut icmp = vec![0u8; crate::icmp::ICMP_FIXED_HEADER_LEN + body.len()];
    message.write(context, &mut icmp).expect("fits");
    let mut header = Ipv6Header::new(Ipv6Addr::from([0u8; 16]), group, NEXT_HEADER_ICMPV6);
    header.hop_limit = ND_HOP_LIMIT;
    let packet = ipv6_packet(&header, &icmp).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv6_multicast_mac(&group),
        MAC_B,
        ETHERTYPE_IPV6,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);

    let out = a.on_frame_collect(&frame, t(0));
    assert_eq!(out.events, [StackEvent::DadFailed { addr: target }]);
    assert!(a.iface().v6_disabled());
}

#[test]
fn multicast_echo_request_is_refused() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // An echo request to all-nodes must not be answered.
    let echo = IcmpEcho {
        kind: crate::icmp::EchoKind::Request,
        identifier: 1,
        sequence: 1,
        payload: b"amplify?",
    };
    let context = IcmpContext::V6 {
        source: link_local(IID_B),
        destination: ALL_NODES,
    };
    let mut message = vec![0u8; echo.wire_len()];
    echo.write(context, &mut message).expect("fits");
    let mut header = Ipv6Header::new(link_local(IID_B), ALL_NODES, NEXT_HEADER_ICMPV6);
    header.hop_limit = 64;
    let packet = ipv6_packet(&header, &message).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv6_multicast_mac(&ALL_NODES),
        MAC_B,
        ETHERTYPE_IPV6,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let out = a.on_frame_collect(&frame, t(2));
    assert!(out.frames.is_empty());
    assert!(out.events.is_empty());
}

#[test]
fn unknown_ipv4_protocol_gets_rate_limited_protocol_unreachable() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    // Prime A's neighbour cache: B's ARP request teaches A the
    // sender's binding, so the errors below transmit immediately.
    let request = ArpPacket {
        operation: OP_REQUEST,
        sender_hardware: MAC_B,
        sender_protocol: V4_B,
        target_hardware: MacAddress([0; 6]),
        target_protocol: V4_A,
    };
    let mut arp = [0u8; crate::arp::ARP_PACKET_LEN];
    request.write(&mut arp).expect("fits");
    let mut arp_frame = vec![0u8; ETHERNET_HEADER_LEN + arp.len()];
    write_header(&mut arp_frame, BROADCAST, MAC_B, ETHERTYPE_ARP).expect("fits");
    arp_frame[ETHERNET_HEADER_LEN..].copy_from_slice(&arp);
    let out = a.on_frame_collect(&arp_frame, t(1));
    assert_eq!(out.frames.len(), 1, "A answers the ARP request");
    // Craft a datagram with an unknown transport protocol.
    let header = Ipv4Header::new(V4_B, V4_A, 253);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + 4];
    header.write(&mut packet, 4).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);

    let mut sent = 0;
    for _ in 0..40 {
        let out = a.on_frame_collect(&frame, t(2));
        sent += out.frames.len();
    }
    let counters = a.counters();
    assert!(counters.icmp_errors_sent > 0, "some errors emitted");
    assert!(
        counters.icmp_errors_suppressed > 0,
        "the token bucket capped the rest"
    );
    // Every emitted error left as exactly one frame (the neighbour
    // was primed above).
    assert_eq!(
        u64::try_from(sent).expect("fits"),
        counters.icmp_errors_sent
    );
}

/// A UDP datagram to `destination`:`port` from `V4_B`, framed for `a`.
fn udp_frame_to(destination: Ipv4Addr, port: u16) -> Vec<u8> {
    let mut udp = [0u8; crate::udp::UDP_HEADER_LEN];
    udp[2..4].copy_from_slice(&port.to_be_bytes());
    let len = u16::try_from(crate::udp::UDP_HEADER_LEN).expect("small");
    udp[4..6].copy_from_slice(&len.to_be_bytes());
    let header = Ipv4Header::new(V4_B, destination, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp.len()];
    header.write(&mut packet, udp.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, BROADCAST, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// Whether `out` carried a delivered UDP datagram.
fn delivered_udp(out: &StackOutput) -> bool {
    out.events
        .iter()
        .any(|event| matches!(event, StackEvent::UdpDatagram { .. }))
}

#[test]
fn a_udp_socket_receives_ipv4_broadcast_on_a_port_it_holds() {
    // A broadcast datagram reaches every host on the segment, so it is
    // delivered where a datagram consumer holds the port and dropped where
    // none does — rather than dropped unconditionally, which left a bound
    // socket unable to receive broadcast at all.
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    let limited = Ipv4Addr::BROADCAST;
    let directed = Ipv4Addr::new(10, 0, 2, 255);

    for destination in [limited, directed] {
        assert!(
            !delivered_udp(&a.on_frame_collect(&udp_frame_to(destination, 9999), t(1))),
            "{destination:?} must not be delivered while nothing holds the port"
        );
    }
    a.set_datagram_ports(&[9999]);
    for destination in [limited, directed] {
        assert!(
            delivered_udp(&a.on_frame_collect(&udp_frame_to(destination, 9999), t(2))),
            "{destination:?} must reach the socket bound to its port"
        );
    }
    // Another port stays shed, and unbinding closes the door again.
    assert!(!delivered_udp(
        &a.on_frame_collect(&udp_frame_to(limited, 137), t(3))
    ));
    a.set_datagram_ports(&[]);
    assert!(!delivered_udp(
        &a.on_frame_collect(&udp_frame_to(limited, 9999), t(4))
    ));
}

#[test]
fn broadcast_is_never_accepted_for_tcp_icmp_or_an_unknown_protocol() {
    // RFC 1122 makes a broadcast TCP segment something a host MUST discard,
    // an echo request to a broadcast address is the smurf amplifier, and an
    // unknown protocol must draw no ICMP error to an ambiguous destination.
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    a.set_datagram_ports(&[9999]);
    // Prime the neighbour cache so a permitted reply would leave at once
    // rather than parking, otherwise an absent frame proves nothing.
    let request = ArpPacket {
        operation: OP_REQUEST,
        sender_hardware: MAC_B,
        sender_protocol: V4_B,
        target_hardware: MacAddress([0; 6]),
        target_protocol: V4_A,
    };
    let mut arp = [0u8; crate::arp::ARP_PACKET_LEN];
    request.write(&mut arp).expect("fits");
    let mut arp_frame = vec![0u8; ETHERNET_HEADER_LEN + arp.len()];
    write_header(&mut arp_frame, BROADCAST, MAC_B, ETHERTYPE_ARP).expect("fits");
    arp_frame[ETHERNET_HEADER_LEN..].copy_from_slice(&arp);
    assert_eq!(a.on_frame_collect(&arp_frame, t(1)).frames.len(), 1);

    for destination in [Ipv4Addr::BROADCAST, Ipv4Addr::new(10, 0, 2, 255)] {
        for protocol in [PROTOCOL_TCP, crate::stack::PROTOCOL_ICMP, 253] {
            let payload = [0u8; 8];
            let header = Ipv4Header::new(V4_B, destination, protocol);
            let mut packet = vec![0u8; IPV4_HEADER_LEN + payload.len()];
            header.write(&mut packet, payload.len()).expect("fits");
            packet[IPV4_HEADER_LEN..].copy_from_slice(&payload);
            let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
            write_header(&mut frame, BROADCAST, MAC_B, ETHERTYPE_IPV4).expect("fits");
            frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
            let out = a.on_frame_collect(&frame, t(2));
            assert!(
                out.events.is_empty(),
                "protocol {protocol} to {destination:?} must surface nothing"
            );
            assert!(
                out.frames.is_empty(),
                "protocol {protocol} to {destination:?} must draw no answer"
            );
        }
    }
}

#[test]
fn a_thirty_one_bit_prefix_has_no_broadcast_address() {
    // RFC 3021: a /31 point-to-point link has no broadcast address — its
    // other address is the peer's unicast, so treating it as broadcast
    // would accept traffic addressed to the peer.
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(Ipv4Addr::new(10, 0, 0, 0), 31, None)
        .expect("configure A");
    a.set_datagram_ports(&[9999]);
    let peer = Ipv4Addr::new(10, 0, 0, 1);
    assert!(
        !delivered_udp(&a.on_frame_collect(&udp_frame_to(peer, 9999), t(1))),
        "the peer's own address is not this host's broadcast"
    );
    // The limited broadcast still works on such a link.
    assert!(delivered_udp(&a.on_frame_collect(
        &udp_frame_to(Ipv4Addr::BROADCAST, 9999),
        t(2)
    )));
}

#[test]
fn everything_the_stack_accepts_the_receive_pre_filter_admits() {
    // The safety property the pre-filter rests on. It may admit *more* than
    // the stack accepts — that is its bias — but never less, or it silently
    // drops traffic the stack wanted. Both sides are driven from one
    // published policy over the same destinations, so a future widening of
    // this acceptance rule that forgets to widen the filter fails here
    // rather than on someone's network.
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    // A datagram consumer on the probe port, so the broadcast destinations
    // below are ones the stack genuinely accepts and the filter must admit.
    a.set_datagram_ports(&[9999]);
    let filter = crate::rxfilter::RxClassifier::new(a.rx_filter_policy());

    let mut checked = 0;
    for destination in [
        V4_A,
        V4_B,
        Ipv4Addr::new(224, 0, 0, 1),
        Ipv4Addr::new(224, 0, 0, 251),
        Ipv4Addr::BROADCAST,
        Ipv4Addr::new(10, 0, 2, 255),
    ] {
        // A UDP datagram is the acceptance oracle: one the destination filter
        // admits always surfaces a `UdpDatagram` event, whether or not
        // anything above consumes it, and one it refuses surfaces nothing.
        // (An ICMP probe would not do — the stack can accept a destination
        // and *then* decline to answer, which reads the same as a refusal.)
        let mut udp = [0u8; crate::udp::UDP_HEADER_LEN];
        udp[2..4].copy_from_slice(&9999u16.to_be_bytes());
        // Length covers the header alone; a zero checksum is "unchecksummed",
        // which IPv4 permits.
        let len = u16::try_from(crate::udp::UDP_HEADER_LEN).expect("small");
        udp[4..6].copy_from_slice(&len.to_be_bytes());
        let header = Ipv4Header::new(V4_B, destination, PROTOCOL_UDP);
        let mut packet = vec![0u8; IPV4_HEADER_LEN + udp.len()];
        header.write(&mut packet, udp.len()).expect("fits");
        packet[IPV4_HEADER_LEN..].copy_from_slice(&udp);
        let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
        write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV4).expect("fits");
        frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);

        let out = a.on_frame_collect(&frame, t(2));
        if out
            .events
            .iter()
            .any(|event| matches!(event, StackEvent::UdpDatagram { .. }))
        {
            checked += 1;
            assert!(
                filter.admit(&frame),
                "the stack accepted {destination:?} but the pre-filter sheds it"
            );
        }
    }
    // The table must actually exercise the property: our own address and the
    // all-systems group are both accepted, so a run that asserted nothing
    // would mean the oracle stopped working.
    assert!(
        checked >= 4,
        "the acceptance oracle matched {checked} destinations, so this proved nothing"
    );
}

#[test]
fn an_unknown_protocol_to_a_group_address_draws_no_icmp_error() {
    // RFC 1122 forbids an ICMP error about a datagram addressed to a group,
    // and the reason is reflection: every host joins the all-systems group,
    // so one frame naming a spoofed source would make the whole segment
    // answer it. The rate limiter bounds each host's share; it does not stop
    // the amplification.
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    // Prime the neighbour cache so a permitted error would transmit at once
    // rather than parking — otherwise an absent frame proves nothing.
    let request = ArpPacket {
        operation: OP_REQUEST,
        sender_hardware: MAC_B,
        sender_protocol: V4_B,
        target_hardware: MacAddress([0; 6]),
        target_protocol: V4_A,
    };
    let mut arp = [0u8; crate::arp::ARP_PACKET_LEN];
    request.write(&mut arp).expect("fits");
    let mut arp_frame = vec![0u8; ETHERNET_HEADER_LEN + arp.len()];
    write_header(&mut arp_frame, BROADCAST, MAC_B, ETHERTYPE_ARP).expect("fits");
    arp_frame[ETHERNET_HEADER_LEN..].copy_from_slice(&arp);
    assert_eq!(a.on_frame_collect(&arp_frame, t(1)).frames.len(), 1);

    let unknown_protocol = 253;
    let frame_to = |destination: Ipv4Addr| {
        let header = Ipv4Header::new(V4_B, destination, unknown_protocol);
        let mut packet = vec![0u8; IPV4_HEADER_LEN + 4];
        header.write(&mut packet, 4).expect("fits");
        let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
        write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV4).expect("fits");
        frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
        frame
    };

    let out = a.on_frame_collect(&frame_to(Ipv4Addr::new(224, 0, 0, 1)), t(2));
    assert!(
        out.frames.is_empty(),
        "a group destination must draw no error"
    );
    assert_eq!(a.counters().icmp_errors_sent, 0);

    // The unicast case still reports, so the suppression above is the
    // destination check and not the whole path going quiet.
    let out = a.on_frame_collect(&frame_to(V4_A), t(2));
    assert_eq!(out.frames.len(), 1, "a unicast destination still reports");
    assert_eq!(a.counters().icmp_errors_sent, 1);
}

#[test]
fn unknown_ipv6_upper_protocol_reports_parameter_problem_pointer() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // B sends A a datagram with an unknown upper protocol; A's report
    // must point at byte 6 (the fixed header's next-header field).
    // The report may park behind an NS/NA exchange, so run the
    // conversation to quiescence and inspect B's events.
    let header = Ipv6Header::new(link_local(IID_B), link_local(IID_A), 200);
    let packet = ipv6_packet(&header, &[0xAB; 4]).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV6).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let mut events_b = Vec::new();
    let mut to_b: VecDeque<Vec<u8>> = tx_bytes(a.on_frame_collect(&frame, t(2)).frames).into();
    for _ in 0..16 {
        let Some(frame) = to_b.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            to_b.extend(tx_bytes(a.on_frame_collect(&reply, t(2)).frames));
        }
    }
    assert_eq!(
        events_b,
        [StackEvent::IcmpErrorReceived {
            source: IpAddr::V6(link_local(IID_A)),
            kind: IcmpErrorKind::ParameterProblem {
                code: PARAM_PROBLEM_NEXT_HEADER,
                pointer: 6,
            },
        }]
    );
}

#[test]
fn send_refusals_are_typed() {
    let mut a = stack(MAC_A, IID_A);
    // No v4 configuration.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Off-subnet with no gateway.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1, 1, b"x", t(0)),
        Err(SendError::NoRoute)
    );
    // Non-unicast destinations.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 1, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V6(ALL_NODES), 1, 1, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    // No usable v6 source before DAD completes.
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V6(link_local(IID_B)), 1, 1, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    // Link down refuses everything.
    a.set_link(LinkState::Down);
    assert_eq!(
        a.send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(0)),
        Err(SendError::LinkDown)
    );
}

#[test]
fn oversize_v6_echo_is_source_fragmented_and_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // 1600 bytes exceeds the 1500-byte link, so the request — and B's
    // 1600-byte reply — are source-fragmented (RFC 8200 §4.5) and each
    // reassembled at the far end into the whole datagram.
    let payload = vec![0x5Au8; 1600];
    let out = a
        .send_echo_request_collect(IpAddr::V6(link_local(IID_B)), 0x33, 7, &payload, t(2))
        .expect("fragmented send");
    let mut events_a = out.events;
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..32 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(2));
            events_a.extend(out_a.events);
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert!(
        events_a.contains(&StackEvent::EchoReply {
            source: IpAddr::V6(link_local(IID_B)),
            identifier: 0x33,
            sequence: 7,
            payload: payload.clone(),
        }),
        "the reassembled 1600-byte echo reply is delivered"
    );
}

#[test]
fn unreachable_neighbor_drops_parked_packets_with_event() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    let out = a
        .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(0))
        .expect("parked");
    assert_eq!(out.frames.len(), 1, "first ARP request");
    // Nothing answers: three multicast solicitations, then failure.
    let mut events = Vec::new();
    for secs in 1..8 {
        events.extend(a.advance_collect(t(secs)).events);
    }
    assert!(events.contains(&StackEvent::NeighborUnreachable {
        ip: IpAddr::V4(V4_B)
    }));
    assert!(a.counters().pending_dropped > 0);
}

#[test]
fn next_deadline_spans_all_components() {
    let a = stack(MAC_A, IID_A);
    // Bring-up work (link-local DAD) is due immediately.
    assert_eq!(a.next_deadline(), Some(t(0)));
}

#[test]
fn ipv4_udp_datagram_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    let mut events_b = Vec::new();

    // The datagram parks on ARP resolution; it flows once the exchange
    // settles, exactly like the echo path.
    let out = a
        .send_datagram_collect(IpAddr::V4(V4_B), 5000, 7, b"udp-payload", t(2))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(2));
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert!(events_b.contains(&StackEvent::UdpDatagram {
        source: IpAddr::V4(V4_A),
        destination: IpAddr::V4(V4_B),
        source_port: 5000,
        destination_port: 7,
        payload: b"udp-payload".to_vec(),
    }));
}

#[test]
fn ipv6_udp_datagram_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    let mut events_b = Vec::new();

    let out = a
        .send_datagram_collect(IpAddr::V6(link_local(IID_B)), 6000, 9, b"udp6", t(3))
        .expect("send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..16 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(3));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(3));
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert!(events_b.contains(&StackEvent::UdpDatagram {
        source: IpAddr::V6(link_local(IID_A)),
        destination: IpAddr::V6(link_local(IID_B)),
        source_port: 6000,
        destination_port: 9,
        payload: b"udp6".to_vec(),
    }));
}

#[test]
fn send_datagram_refusals_are_typed() {
    let mut a = stack(MAC_A, IID_A);
    // No v4 configuration yet: no usable source address.
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(V4_B), 1, 2, b"x", t(0)),
        Err(SendError::NoSourceAddress)
    );
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // The limited broadcast and the unspecified address are refused:
    // neither is a meaningful datagram destination; fail closed.
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(Ipv4Addr::BROADCAST), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 1, 2, b"x", t(0)),
        Err(SendError::NotUnicast)
    );
    // Off-subnet unicast with no gateway.
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 1, 2, b"x", t(0)),
        Err(SendError::NoRoute)
    );
    // Link down refuses everything.
    a.set_link(LinkState::Down);
    assert_eq!(
        a.send_datagram_collect(IpAddr::V4(V4_B), 1, 2, b"x", t(0)),
        Err(SendError::LinkDown)
    );
}

#[test]
fn oversize_v6_udp_is_source_fragmented_and_round_trips() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // A 1600-byte datagram exceeds the 1500-byte link and is
    // source-fragmented (RFC 8200 §4.5); the receiver reassembles it.
    let payload = vec![0xA5u8; 1600];
    let out = a
        .send_datagram_collect(IpAddr::V6(link_local(IID_B)), 6000, 9, &payload, t(2))
        .expect("fragmented send");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    let mut events_b = Vec::new();
    for _ in 0..32 {
        let Some(frame) = frames.pop_front() else {
            break;
        };
        let out_b = b.on_frame_collect(&frame, t(2));
        events_b.extend(out_b.events);
        for reply in tx_bytes(out_b.frames) {
            let out_a = a.on_frame_collect(&reply, t(2));
            frames.extend(tx_bytes(out_a.frames));
        }
    }
    assert!(
        events_b.contains(&StackEvent::UdpDatagram {
            source: IpAddr::V6(link_local(IID_A)),
            destination: IpAddr::V6(link_local(IID_B)),
            source_port: 6000,
            destination_port: 9,
            payload: payload.clone(),
        }),
        "the reassembled 1600-byte datagram is delivered"
    );
}

#[test]
fn corrupt_udp_checksum_is_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Hand-build a UDP-over-IPv4 datagram to A, then corrupt its payload
    // so the checksum no longer verifies: it must be dropped, not surfaced.
    let mut udp_msg = vec![0u8; udp::UDP_HEADER_LEN + 4];
    udp::write(
        udp::Pseudo::V4 {
            source: V4_B,
            destination: V4_A,
        },
        1111,
        2222,
        &[1, 2, 3, 4],
        &mut udp_msg,
    )
    .expect("write");
    udp_msg[udp::UDP_HEADER_LEN] ^= 0xFF;
    let header = Ipv4Header::new(V4_B, V4_A, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp_msg.len()];
    header.write(&mut packet, udp_msg.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp_msg);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_B, MAC_A, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let out = a.on_frame_collect(&frame, t(0));
    assert!(out.events.is_empty());
    assert!(a.counters().rx_dropped >= 1);
}

// ---- Receive-checksum offload (RX_CSUM_VALIDATED) --------------------

/// A stack whose device advertised (and it opted into) receive-checksum
/// validation.
fn stack_with_rx_csum(mac: MacAddress, iid: [u8; 8]) -> Stack {
    let mut f = facts(mac);
    f.offloads = tairix_abi::driver::net::NetOffloads::RX_CSUM_VALIDATED;
    Stack::new(
        &StackConfig::new(f, iid, 0x1234, STACK_HASH_KEY),
        temp_source(),
        t(0),
    )
    .expect("valid facts")
}

/// A UDP/IPv4 datagram from `V4_B` to `V4_A` (Ethernet destination
/// `MAC_A`). When `corrupt`, the first payload byte is flipped *after*
/// the checksum is computed, so the on-wire checksum no longer verifies.
fn v4_udp_frame_to_a(payload: &[u8], corrupt: bool) -> Vec<u8> {
    let mut udp_msg = vec![0u8; udp::UDP_HEADER_LEN + payload.len()];
    udp::write(
        udp::Pseudo::V4 {
            source: V4_B,
            destination: V4_A,
        },
        1111,
        2222,
        payload,
        &mut udp_msg,
    )
    .expect("write");
    if corrupt {
        udp_msg[udp::UDP_HEADER_LEN] ^= 0xFF;
    }
    let header = Ipv4Header::new(V4_B, V4_A, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp_msg.len()];
    header.write(&mut packet, udp_msg.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp_msg);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_A, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

#[test]
fn rx_checksum_offload_matches_the_software_path_byte_for_byte() {
    // The offloaded path (device-validated, fold skipped) must produce
    // output identical to the canonical software path — the oracle.
    let mut soft = stack(MAC_A, IID_A);
    let mut off = stack_with_rx_csum(MAC_A, IID_A);
    soft.set_ipv4_config(V4_A, 24, None)
        .expect("configure soft");
    off.set_ipv4_config(V4_A, 24, None).expect("configure off");
    let frame = v4_udp_frame_to_a(b"conformance", false);
    let soft_out = soft.on_frame_collect(&frame, t(0));
    let off_out = off.on_frame_meta_collect(&frame, RxMeta::validated(), t(0));
    assert_eq!(soft_out, off_out);
    assert!(matches!(
        soft_out.events.first(),
        Some(StackEvent::UdpDatagram { .. })
    ));
}

#[test]
fn rx_validated_offload_skips_the_fold_but_still_delivers() {
    // A frame the device validated is accepted even if its on-wire
    // checksum is (now) wrong: the software fold is skipped. Trust is in
    // the device; the offload is what is being exercised.
    let mut off = stack_with_rx_csum(MAC_A, IID_A);
    off.set_ipv4_config(V4_A, 24, None).expect("configure");
    let corrupt = v4_udp_frame_to_a(b"trust-the-nic", true);
    let out = off.on_frame_meta_collect(&corrupt, RxMeta::validated(), t(0));
    assert!(matches!(
        out.events.first(),
        Some(StackEvent::UdpDatagram { .. })
    ));
    // The *same* corrupt frame without the device's assurance is folded
    // in software and dropped — the offload is never assumed.
    let out = off.on_frame_meta_collect(&corrupt, RxMeta::none(), t(0));
    assert!(out.events.is_empty());
}

#[test]
fn rx_validated_claim_is_ignored_when_offload_not_negotiated() {
    // A stack that did not negotiate the offload folds in software even
    // when a frame claims the device validated it: a corrupt frame is
    // dropped. A per-frame claim is honoured only under a negotiated
    // offload.
    let mut soft = stack(MAC_A, IID_A);
    soft.set_ipv4_config(V4_A, 24, None).expect("configure");
    let corrupt = v4_udp_frame_to_a(b"no-offload", true);
    let out = soft.on_frame_meta_collect(&corrupt, RxMeta::validated(), t(0));
    assert!(out.events.is_empty());
}

#[test]
fn frames_for_other_hosts_are_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Unicast Ethernet destination that is not ours.
    let header = Ipv4Header::new(V4_B, V4_A, PROTOCOL_ICMP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN];
    header.write(&mut packet, 0).expect("fits");
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(&mut frame, MAC_B, MAC_B, ETHERTYPE_IPV4).expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    let out = a.on_frame_collect(&frame, t(0));
    assert!(out.frames.is_empty() && out.events.is_empty());
    assert_eq!(a.counters().rx_dropped, 1);
}

// ---- Multicast membership (IGMPv2 / MLDv2) --------------------------

const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 1, 2, 3);

/// A link-local-scope IPv6 multicast group used by the transmit tests.
const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x1234);

/// A UDP datagram from `V4_B` to the IPv4 multicast group `GROUP_V4`,
/// addressed to the group's multicast MAC.
fn v4_group_udp_frame(payload: &[u8]) -> Vec<u8> {
    let mut udp_msg = vec![0u8; udp::UDP_HEADER_LEN + payload.len()];
    udp::write(
        udp::Pseudo::V4 {
            source: V4_B,
            destination: GROUP_V4,
        },
        5000,
        7000,
        payload,
        &mut udp_msg,
    )
    .expect("write");
    let header = Ipv4Header::new(V4_B, GROUP_V4, PROTOCOL_UDP);
    let mut packet = vec![0u8; IPV4_HEADER_LEN + udp_msg.len()];
    header.write(&mut packet, udp_msg.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&udp_msg);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv4_multicast_mac(&GROUP_V4),
        MAC_B,
        ETHERTYPE_IPV4,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// An IGMP General Query from `V4_B` to the all-systems group.
fn igmp_general_query_frame(max_resp_deciseconds: u8) -> Vec<u8> {
    let all_systems = Ipv4Addr::new(224, 0, 0, 1);
    let message = IgmpMessage::MembershipQuery {
        max_resp_deciseconds,
        group: Ipv4Addr::UNSPECIFIED,
    };
    let mut body = [0u8; crate::igmp::IGMP_MESSAGE_LEN];
    message.write(&mut body).expect("write");
    let mut header = Ipv4Header::new(V4_B, all_systems, PROTOCOL_IGMP);
    header.ttl = 1;
    let mut packet = vec![0u8; IPV4_HEADER_LEN + body.len()];
    header.write(&mut packet, body.len()).expect("fits");
    packet[IPV4_HEADER_LEN..].copy_from_slice(&body);
    let mut frame = vec![0u8; ETHERNET_HEADER_LEN + packet.len()];
    write_header(
        &mut frame,
        ipv4_multicast_mac(&all_systems),
        MAC_B,
        ETHERTYPE_IPV4,
    )
    .expect("fits");
    frame[ETHERNET_HEADER_LEN..].copy_from_slice(&packet);
    frame
}

/// The first IGMP message in `frames`, with its Ethernet destination,
/// TTL, and IPv4 options length.
fn first_igmp(frames: &[TxFrame]) -> Option<(MacAddress, u8, usize, IgmpMessage)> {
    for f in frames {
        let Some(eth) = EthernetFrame::parse(&f.bytes) else {
            continue;
        };
        if eth.ethertype != ETHERTYPE_IPV4 {
            continue;
        }
        let Some((header, options, payload)) = Ipv4Header::parse(eth.payload) else {
            continue;
        };
        if header.protocol != PROTOCOL_IGMP {
            continue;
        }
        let Some(message) = IgmpMessage::parse(payload) else {
            continue;
        };
        return Some((eth.destination, header.ttl, options.len(), message));
    }
    None
}

#[test]
fn joining_v4_group_emits_igmp_report_with_router_alert() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    assert!(a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join"));
    let out = a.advance_collect(t(0));
    let (dest_mac, ttl, options_len, message) = first_igmp(&out.frames).expect("igmp report");
    assert_eq!(message, IgmpMessage::V2Report { group: GROUP_V4 });
    assert_eq!(ttl, 1, "membership messages never leave the link");
    assert_eq!(options_len, 4, "Router Alert option present (IHL = 6)");
    assert_eq!(dest_mac, ipv4_multicast_mac(&GROUP_V4));
}

#[test]
fn counters_track_bytes_and_count_each_emitted_frame_once() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    // Drive the membership traffic and tally what actually reached the wire.
    let mut emitted_frames = 0u64;
    let mut emitted_bytes = 0u64;
    for s in 0..40 {
        let out = a.advance_collect(t(s));
        emitted_frames += out.frames.len() as u64;
        emitted_bytes += out.frames.iter().map(|f| f.bytes.len() as u64).sum::<u64>();
    }
    assert!(
        emitted_frames > 0,
        "a v4 join emits at least one IGMP report"
    );
    let counters = a.counters();
    // Every emitted frame — IGMP reports included — is counted exactly once.
    // This guards against the IGMP/MLD transmit double-count.
    assert_eq!(counters.tx_frames, emitted_frames);
    assert_eq!(counters.tx_bytes, emitted_bytes);

    // A received frame's whole Ethernet length is counted, dropped or not.
    let frame = v4_group_udp_frame(b"hello");
    let before = a.counters();
    let _ = a.on_frame_collect(&frame, t(40));
    let after = a.counters();
    assert_eq!(after.rx_frames, before.rx_frames + 1);
    assert_eq!(after.rx_bytes, before.rx_bytes + frame.len() as u64);
}

#[test]
fn joined_group_receives_udp_and_others_are_dropped() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    // Not a member yet: the multicast datagram is dropped.
    let out = a.on_frame_collect(&v4_group_udp_frame(b"hi"), t(0));
    assert!(out.events.is_empty());
    assert!(a.counters().rx_dropped >= 1);
    // After joining, the same datagram is delivered.
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    let out = a.on_frame_collect(&v4_group_udp_frame(b"hi"), t(0));
    assert!(out.events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram { destination: IpAddr::V4(d), payload, .. }
            if *d == GROUP_V4 && payload == b"hi"
    )));
}

#[test]
fn igmp_general_query_triggers_a_delayed_response() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    // Drain the unsolicited join reports.
    for s in 0..40 {
        let _ = a.advance_collect(t(s));
    }
    // A General Query schedules — but does not immediately send — a report.
    let out = a.on_frame_collect(&igmp_general_query_frame(20), t(100));
    assert!(first_igmp(&out.frames).is_none(), "response is delayed");
    assert!(a.next_deadline().is_some());
    // Past the 2-second window the response is emitted.
    let out = a.advance_collect(t(103));
    let (_mac, _ttl, _opts, message) = first_igmp(&out.frames).expect("query response");
    assert_eq!(message, IgmpMessage::V2Report { group: GROUP_V4 });
}

#[test]
fn leaving_v4_group_emits_leave_to_all_routers() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    a.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    for s in 0..40 {
        let _ = a.advance_collect(t(s));
    }
    assert!(a.leave_multicast(IpAddr::V4(GROUP_V4), t(40)));
    let out = a.advance_collect(t(40));
    let (dest_mac, _ttl, _opts, message) = first_igmp(&out.frames).expect("leave");
    assert_eq!(message, IgmpMessage::LeaveGroup { group: GROUP_V4 });
    assert_eq!(dest_mac, ipv4_multicast_mac(&Ipv4Addr::new(224, 0, 0, 2)));
}

#[test]
fn joining_a_non_multicast_group_fails_closed() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("configure");
    assert_eq!(
        a.join_multicast(IpAddr::V4(V4_B), t(0)),
        Err(McastError::NotMulticast)
    );
    assert_eq!(
        a.join_multicast(IpAddr::V6(link_local(IID_B)), t(0)),
        Err(McastError::NotMulticast)
    );
}

#[test]
fn ipv6_address_reports_its_solicited_node_group_via_mld() {
    let mut a = stack(MAC_A, IID_A);
    let _ = a.advance_collect(t(0)); // link-local DAD solicitation
    let out = a.advance_collect(t(1)); // preferred -> join solicited-node -> MLD report
    let frame = out
        .frames
        .iter()
        .find(|f| {
            EthernetFrame::parse(&f.bytes).is_some_and(|eth| {
                eth.ethertype == ETHERTYPE_IPV6
                    && Ipv6Header::parse(eth.payload)
                        .is_some_and(|(h, _)| h.next_header == NEXT_HEADER_HOP_BY_HOP)
            })
        })
        .expect("mld report frame");
    let eth = EthernetFrame::parse(&frame.bytes).expect("eth");
    assert_eq!(eth.destination, ipv6_multicast_mac(&ALL_MLDV2_ROUTERS));
    let (header, payload) = Ipv6Header::parse(eth.payload).expect("ipv6");
    assert_eq!(header.destination, ALL_MLDV2_ROUTERS);
    assert_eq!(header.hop_limit, 1);
    // Hop-by-Hop header: names ICMPv6 and carries the Router Alert option.
    assert_eq!(payload[0], NEXT_HEADER_ICMPV6);
    assert_eq!(payload[2], 5, "Router Alert option type");
    // The ICMPv6 message after the 8-byte Hop-by-Hop header is a report.
    assert_eq!(payload[HBH_ROUTER_ALERT_LEN], TYPE_MLDV2_REPORT);
}

#[test]
fn ipv4_multicast_datagram_transmit_reaches_a_member() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("configure A");
    b.set_ipv4_config(V4_B, 24, None).expect("configure B");
    // The receiver must be a member; the sender need not be, and needs
    // no route to the group.
    b.join_multicast(IpAddr::V4(GROUP_V4), t(0)).expect("join");
    let out = a
        .send_datagram_collect(IpAddr::V4(GROUP_V4), 5000, 7000, b"mcast4", t(0))
        .expect("send");
    assert_eq!(out.frames.len(), 1, "one unfragmented multicast frame");
    let eth = EthernetFrame::parse(&out.frames[0].bytes).expect("eth");
    assert_eq!(eth.destination, ipv4_multicast_mac(&GROUP_V4));
    let (header, _opts, _payload) = Ipv4Header::parse(eth.payload).expect("ipv4");
    assert_eq!(header.destination, GROUP_V4);
    assert_eq!(header.ttl, 1, "multicast data stays on the local link");
    // The member delivers it as a verbatim datagram event.
    let mut events = Vec::new();
    for frame in &out.frames {
        events.extend(b.on_frame_collect(&frame.bytes, t(0)).events);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram {
            destination: IpAddr::V4(d),
            source_port: 5000,
            destination_port: 7000,
            payload,
            ..
        } if *d == GROUP_V4 && payload == b"mcast4"
    )));
}

#[test]
fn ipv6_multicast_datagram_transmit_reaches_a_member() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    b.join_multicast(IpAddr::V6(GROUP_V6), t(3)).expect("join");
    let out = a
        .send_datagram_collect(IpAddr::V6(GROUP_V6), 6000, 9000, b"mcast6", t(3))
        .expect("send");
    assert_eq!(out.frames.len(), 1, "one multicast frame, no resolution");
    let eth = EthernetFrame::parse(&out.frames[0].bytes).expect("eth");
    assert_eq!(eth.destination, ipv6_multicast_mac(&GROUP_V6));
    let (header, _payload) = Ipv6Header::parse(eth.payload).expect("ipv6");
    assert_eq!(header.destination, GROUP_V6);
    assert_eq!(
        header.hop_limit, 1,
        "multicast data stays on the local link"
    );
    let mut events = Vec::new();
    for frame in &out.frames {
        events.extend(b.on_frame_collect(&frame.bytes, t(3)).events);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram {
            destination: IpAddr::V6(d),
            payload,
            ..
        } if *d == GROUP_V6 && payload == b"mcast6"
    )));
}

#[test]
fn ipv6_multicast_datagram_source_fragments_when_oversize() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    b.join_multicast(IpAddr::V6(GROUP_V6), t(3)).expect("join");
    // A multicast group needs no neighbour resolution, so an oversize
    // datagram emits its fragments (RFC 8200 §4.5) directly and
    // deterministically — no ND exchange to interleave.
    let payload = vec![0x7Eu8; 2000];
    let out = a
        .send_datagram_collect(IpAddr::V6(GROUP_V6), 6000, 9000, &payload, t(3))
        .expect("send");
    assert!(
        out.frames.len() >= 2,
        "an oversize multicast datagram is fragmented"
    );
    // Every fragment is an IPv6 packet to the group MAC whose next header
    // is the Fragment header, at hop limit 1.
    for frame in &out.frames {
        let eth = EthernetFrame::parse(&frame.bytes).expect("eth");
        assert_eq!(eth.destination, ipv6_multicast_mac(&GROUP_V6));
        let (header, frag_payload) = Ipv6Header::parse(eth.payload).expect("ipv6");
        assert_eq!(header.destination, GROUP_V6);
        assert_eq!(header.hop_limit, 1, "multicast data stays on the link");
        assert_eq!(header.next_header, crate::ipv6::NEXT_HEADER_FRAGMENT);
        assert!(frag_payload.len() >= crate::ipv6::FRAGMENT_HEADER_LEN);
    }
    // The member reassembles the fragments into the original datagram.
    let mut events = Vec::new();
    for frame in &out.frames {
        events.extend(b.on_frame_collect(&frame.bytes, t(3)).events);
    }
    assert!(events.iter().any(|event| matches!(
        event,
        StackEvent::UdpDatagram {
            destination: IpAddr::V6(d),
            payload: delivered,
            ..
        } if *d == GROUP_V6 && *delivered == payload
    )));
}

// --- TCP over the engine (N5c Layer 1) ---------------------------------
//
// The engine is stateless for TCP: it demultiplexes a checksum-valid
// inbound segment to a `StackEvent::TcpSegment` and originates a segment
// (folding the pseudo-header checksum over the source it selects) through
// `send_tcp`. The connection state machine is the `tcp::conn::Tcb`. These
// tests wire two `Tcb`s across two back-to-back `Stack`s, exactly as the
// live `netstack` does, and prove a real handshake and bidirectional data
// transfer flow through the new demux/originate paths.

use crate::tcp::conn::{State, Tcb, TcpConfig};
use crate::tcp::TcpSegment;

/// A fixed key for the stack's neighbour-cache index, so a run's table layout
/// is reproducible.
const STACK_HASH_KEY: tairix_hash::HashSeed =
    tairix_hash::HashSeed::from_words(0x5354_4143_4B00_0001, 0x5354_4143_4B00_0002);

const A_PORT: u16 = 40000;
const B_PORT: u16 = 80;

/// One side of the TCP link: its engine, its connection, and the peer
/// address its segments are sent to.
struct TcpSide {
    stack: Stack,
    tcb: Tcb,
    peer: IpAddr,
}

impl TcpSide {
    /// Drain the connection's outbound segments through the engine,
    /// returning the frames to hand the peer's engine.
    fn transmit(&mut self, now: Duration64) -> Vec<Vec<u8>> {
        let stack = &mut self.stack;
        let peer = self.peer;
        let mut frames = Vec::new();
        self.tcb.poll_transmit(now, |seg| {
            // A momentary resolution/route miss parks the segment in the
            // engine, which emits it on resolution; treat it as sent.
            if let Ok(out) = stack.send_tcp_collect(peer, &seg.meta, seg.payload, seg.gso_size, now)
            {
                frames.extend(tx_bytes(out.frames));
            }
            true
        });
        frames
    }

    /// Feed one frame into the engine, feeding any surfaced TCP segment
    /// into the connection and returning the engine's reply frames.
    fn receive(&mut self, frame: &[u8], now: Duration64) -> Vec<Vec<u8>> {
        let out = self.stack.on_frame_collect(frame, now);
        for event in &out.events {
            if let StackEvent::TcpSegment {
                source,
                destination,
                ecn,
                segment,
            } = event
            {
                let pseudo = match (source, destination) {
                    (IpAddr::V4(s), IpAddr::V4(d)) => Pseudo::V4 {
                        source: *s,
                        destination: *d,
                    },
                    (IpAddr::V6(s), IpAddr::V6(d)) => Pseudo::V6 {
                        source: *s,
                        destination: *d,
                    },
                    _ => continue,
                };
                if let Some(seg) = TcpSegment::parse(pseudo, segment) {
                    self.tcb.on_segment(&seg, *ecn, now);
                }
            }
        }
        tx_bytes(out.frames)
    }
}

/// Run the two sides until the link is quiet at `now`: transmit both
/// connections' due segments and the engines' timer output, then exchange
/// every frame (feeding surfaced TCP segments into the connections) until
/// nothing more moves. A frame `X.transmit()` produces is destined for the
/// *peer*, so it is always enqueued onto the peer's inbound queue.
fn drive_tcp(a: &mut TcpSide, b: &mut TcpSide, now: Duration64) {
    let mut to_b: VecDeque<Vec<u8>> = VecDeque::new();
    let mut to_a: VecDeque<Vec<u8>> = VecDeque::new();
    a.tcb.advance(now);
    b.tcb.advance(now);
    to_b.extend(tx_bytes(a.stack.advance_collect(now).frames));
    to_a.extend(tx_bytes(b.stack.advance_collect(now).frames));
    to_b.extend(a.transmit(now));
    to_a.extend(b.transmit(now));
    for _ in 0..256 {
        if to_a.is_empty() && to_b.is_empty() {
            // Both sides may still owe a segment (e.g. an ACK unlocked by
            // a delivered segment); pull once more before concluding.
            to_b.extend(a.transmit(now));
            to_a.extend(b.transmit(now));
            if to_a.is_empty() && to_b.is_empty() {
                break;
            }
        }
        if let Some(frame) = to_b.pop_front() {
            to_a.extend(b.receive(&frame, now));
            to_a.extend(b.transmit(now));
        }
        if let Some(frame) = to_a.pop_front() {
            to_b.extend(a.receive(&frame, now));
            to_b.extend(a.transmit(now));
        }
    }
}

fn tcp_pair() -> (TcpSide, TcpSide) {
    let mut sa = stack(MAC_A, IID_A);
    let mut sb = stack(MAC_B, IID_B);
    sa.set_ipv4_config(V4_A, 24, None).expect("configure A");
    sb.set_ipv4_config(V4_B, 24, None).expect("configure B");
    let cfg = TcpConfig::default();
    let a = TcpSide {
        stack: sa,
        tcb: Tcb::connect(cfg, A_PORT, B_PORT, 1000, t(0)),
        peer: IpAddr::V4(V4_B),
    };
    let b = TcpSide {
        stack: sb,
        // Passive open: the listener learns the client port from the SYN.
        tcb: Tcb::listen(cfg, B_PORT, 0, 5000),
        peer: IpAddr::V4(V4_A),
    };
    (a, b)
}

/// An IPv6 link-local TCP pair, brought through DAD so each side's
/// link-local address is preferred and reachable. Each connection's
/// `TcpConfig.local_mss` is seeded from [`Stack::tcp_local_mss`] — exactly
/// as the netstack seeds a real connection — so the segment size accounts
/// for the 40-byte IPv6 header against the 1500-byte link MTU.
fn tcp_pair_v6() -> (TcpSide, TcpSide) {
    let mut sa = stack(MAC_A, IID_A);
    let mut sb = stack(MAC_B, IID_B);
    bring_up(&mut sa, &mut sb);
    let a_addr = IpAddr::V6(link_local(IID_A));
    let b_addr = IpAddr::V6(link_local(IID_B));
    let a_mss = sa.tcp_local_mss(b_addr, t(1)).expect("A reaches B");
    let b_mss = sb.tcp_local_mss(a_addr, t(1)).expect("B reaches A");
    let a = TcpSide {
        stack: sa,
        tcb: Tcb::connect(
            TcpConfig {
                local_mss: a_mss,
                ..TcpConfig::default()
            },
            A_PORT,
            B_PORT,
            1000,
            t(1),
        ),
        peer: b_addr,
    };
    let b = TcpSide {
        stack: sb,
        // Passive open: the listener learns the client port from the SYN.
        tcb: Tcb::listen(
            TcpConfig {
                local_mss: b_mss,
                ..TcpConfig::default()
            },
            B_PORT,
            0,
            5000,
        ),
        peer: a_addr,
    };
    (a, b)
}

#[test]
fn tcp_handshake_and_bidirectional_data_over_the_engine() {
    let (mut a, mut b) = tcp_pair();
    // A time step past the default delayed-ACK (100 ms) each round, so an
    // owed ACK is always released, keeping the conversation progressing.
    let mut clock = 1u64;
    let mut step = || {
        let now = Duration64::from_nanos(clock * 200_000_000);
        clock += 1;
        now
    };

    // Handshake.
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
        if a.tcb.is_established() && b.tcb.is_established() {
            break;
        }
    }
    assert_eq!(a.tcb.state(), State::Established, "client established");
    assert_eq!(b.tcb.state(), State::Established, "server established");
    // The listener learned the client's port from the SYN.
    assert_eq!(b.tcb.remote_port(), A_PORT);

    // Client -> server data.
    a.tcb.send(b"hello over tcp").expect("client send");
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
    }
    let mut got = [0u8; 64];
    let n = b.tcb.recv(&mut got);
    assert_eq!(&got[..n], b"hello over tcp", "server received client data");

    // Server -> client data.
    b.tcb.send(b"and back again").expect("server send");
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
    }
    let mut got = [0u8; 64];
    let n = a.tcb.recv(&mut got);
    assert_eq!(&got[..n], b"and back again", "client received server data");

    // Orderly close from the client: the server observes the peer FIN.
    a.tcb.close(step()).expect("client close");
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
    }
    assert!(
        matches!(
            b.tcb.state(),
            State::CloseWait | State::LastAck | State::Closing | State::Closed
        ),
        "server saw the peer FIN, state = {:?}",
        b.tcb.state()
    );
}

#[test]
fn tcp_bulk_transfer_over_the_engine() {
    let (mut a, mut b) = tcp_pair();
    let mut clock = 1u64;
    let mut step = || {
        let now = Duration64::from_nanos(clock * 200_000_000);
        clock += 1;
        now
    };
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
        if a.tcb.is_established() && b.tcb.is_established() {
            break;
        }
    }
    assert!(a.tcb.is_established() && b.tcb.is_established());

    // A payload well past one MSS, so it exercises segmentation, windowing,
    // and cumulative ACKs through the real engine.
    let payload: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
    let mut offered = 0usize;
    let mut received: Vec<u8> = Vec::new();
    for _ in 0..200 {
        if offered < payload.len() {
            offered += a.tcb.send(&payload[offered..]).expect("send");
        }
        drive_tcp(&mut a, &mut b, step());
        let mut buf = [0u8; 4096];
        loop {
            let n = b.tcb.recv(&mut buf);
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }
        if received.len() == payload.len() {
            break;
        }
    }
    assert_eq!(received, payload, "bulk stream arrived intact and in order");
}

#[test]
fn tcp_bulk_transfer_over_ipv6_respects_the_link_mtu() {
    // Regression (N5c): a full-size data segment on an IPv6 link must fit
    // the 1500-byte MTU once the 40-byte IPv6 header and the TCP options
    // are added. Before the send segment size was clamped to the path MSS
    // (link MTU minus the family's IP header and the fixed TCP header) and
    // reduced by the carried option bytes, every full-size segment
    // overflowed the MTU and was silently dropped by `send_tcp` — only each
    // burst's short trailing segment reached the peer and a bulk transfer
    // stalled to the user timeout. This drives a multi-segment transfer
    // over a real IPv6 link and asserts every byte arrives, in order.
    let (mut a, mut b) = tcp_pair_v6();
    let mut clock = 2u64;
    let mut step = || {
        let now = Duration64::from_nanos(clock * 200_000_000);
        clock += 1;
        now
    };
    for _ in 0..8 {
        drive_tcp(&mut a, &mut b, step());
        if a.tcb.is_established() && b.tcb.is_established() {
            break;
        }
    }
    assert!(
        a.tcb.is_established() && b.tcb.is_established(),
        "v6 handshake established"
    );

    let payload: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
    let mut offered = 0usize;
    let mut received: Vec<u8> = Vec::new();
    for _ in 0..200 {
        if offered < payload.len() {
            offered += a.tcb.send(&payload[offered..]).expect("send");
        }
        drive_tcp(&mut a, &mut b, step());
        let mut buf = [0u8; 4096];
        loop {
            let n = b.tcb.recv(&mut buf);
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buf[..n]);
        }
        if received.len() == payload.len() {
            break;
        }
    }
    assert_eq!(
        received, payload,
        "the whole IPv6 bulk stream arrived intact and in order"
    );
}

/// Engine-level transmit-checksum-offload conformance: a stack that
/// negotiated `TX_CSUM_TCP` emits a TCP frame carrying only the partial
/// (pseudo-header) checksum and a [`TxOffload::PartialChecksum`] descriptor
/// whose offsets address the transport checksum within the Ethernet frame;
/// completing that fold reproduces, byte-for-byte, the frame a stack
/// without the offload emits with a full software checksum.
#[test]
fn tcp_v4_tx_checksum_offload_matches_the_software_path() {
    use crate::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegmentMeta};

    // Resolve B's MAC in `a`'s neighbour cache by exchanging one echo
    // round with a fresh peer B (the echo path drives ARP), so a later
    // `send_tcp` emits synchronously rather than parking.
    fn resolve_b(a: &mut Stack) {
        let mut b = stack(MAC_B, IID_B);
        b.set_ipv4_config(V4_B, 24, None).expect("cfg b");
        let out = a
            .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(2))
            .expect("echo");
        let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
        for _ in 0..8 {
            let Some(f) = frames.pop_front() else { break };
            let ob = b.on_frame_collect(&f, t(2));
            for r in tx_bytes(ob.frames) {
                frames.extend(tx_bytes(a.on_frame_collect(&r, t(2)).frames));
            }
        }
    }

    let mut off_facts = facts(MAC_A);
    off_facts.offloads = tairix_abi::driver::net::NetOffloads::TX_CSUM_TCP;
    let mut off = Stack::new(
        &StackConfig::new(off_facts, IID_A, 0x1234, STACK_HASH_KEY),
        temp_source(),
        t(0),
    )
    .expect("valid");
    let mut soft = stack(MAC_A, IID_A);
    off.set_ipv4_config(V4_A, 24, None).expect("cfg off");
    soft.set_ipv4_config(V4_A, 24, None).expect("cfg soft");
    resolve_b(&mut off);
    resolve_b(&mut soft);

    let meta = TcpSegmentMeta {
        source_port: A_PORT,
        destination_port: B_PORT,
        seq: SeqNumber::new(1),
        ack: SeqNumber::new(0),
        flags: TcpFlags::SYN,
        window: 1000,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let off_out = off
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, b"tcp-tx-offload", None, t(3))
        .expect("send off");
    let soft_out = soft
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, b"tcp-tx-offload", None, t(3))
        .expect("send soft");
    assert_eq!(off_out.frames.len(), 1);
    assert_eq!(soft_out.frames.len(), 1);

    // The offloaded stack attaches the eth(14)+ipv4(20) checksum offsets;
    // the software stack attaches none.
    assert_eq!(
        off_out.frames[0].offload,
        TxOffload::PartialChecksum {
            csum_start: 34,
            csum_offset: 16,
        }
    );
    assert_eq!(soft_out.frames[0].offload, TxOffload::None);

    // Complete the device's fold over the offloaded frame and compare.
    let mut completed = off_out.frames[0].bytes.clone();
    let start = 34usize;
    let sum = crate::internet_checksum(&completed[start..]);
    completed[start + 16..start + 18].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(completed, soft_out.frames[0].bytes);
}

/// Engine-level TCP-segmentation-offload conformance: a stack that
/// negotiated segmentation emits one over-size super-segment plus a
/// [`TxOffload::TcpSegment`] descriptor; splitting that super-segment as
/// the device is contractually required to (per-`gso_size` payloads, the
/// header replicated with the sequence advanced, PSH only on the last
/// segment, each segment's checksum recomputed) reproduces, TCP-segment
/// for TCP-segment, exactly the segments the stack emits per-MSS with no
/// offload. Compared at the TCP layer so the check is independent of the
/// per-segment IPv4 identification the device assigns.
#[test]
#[allow(clippy::too_many_lines)]
fn tcp_v4_tx_segmentation_offload_matches_the_software_path() {
    use crate::checksum::Checksum;
    use crate::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegmentMeta, PROTOCOL_TCP};

    fn resolve_b(a: &mut Stack) {
        let mut b = stack(MAC_B, IID_B);
        b.set_ipv4_config(V4_B, 24, None).expect("cfg b");
        let out = a
            .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(2))
            .expect("echo");
        let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
        for _ in 0..8 {
            let Some(f) = frames.pop_front() else { break };
            let ob = b.on_frame_collect(&f, t(2));
            for r in tx_bytes(ob.frames) {
                frames.extend(tx_bytes(a.on_frame_collect(&r, t(2)).frames));
            }
        }
    }

    // The TCP segment (strip the 14-byte Ethernet + 20-byte IPv4 header).
    const IP_TCP: usize = 34;
    const MSS: u16 = 100;

    let mut facts_off = facts(MAC_A);
    facts_off.offloads = tairix_abi::driver::net::NetOffloads::from_bits(
        tairix_abi::driver::net::NetOffloads::TX_CSUM_TCP.bits()
            | tairix_abi::driver::net::NetOffloads::TX_SEGMENT_TCP.bits(),
    )
    .expect("defined bits");
    let mut off = Stack::new(
        &StackConfig::new(facts_off, IID_A, 0x4321, STACK_HASH_KEY),
        temp_source(),
        t(0),
    )
    .expect("valid");
    let mut soft = stack(MAC_A, IID_A);
    off.set_ipv4_config(V4_A, 24, None).expect("cfg off");
    soft.set_ipv4_config(V4_A, 24, None).expect("cfg soft");
    resolve_b(&mut off);
    resolve_b(&mut soft);

    let base = SeqNumber::new(1);
    let payload: Vec<u8> = (0..250u32).map(|i| (i % 253) as u8).collect();

    // Reference: the per-MSS segments the software path would emit, each
    // built with the seq/flags the device must reproduce (PSH on the last).
    let mut reference: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0usize;
    while offset < payload.len() {
        let end = (offset + usize::from(MSS)).min(payload.len());
        let last = end == payload.len();
        let mut flags = TcpFlags::ACK;
        if last {
            flags = flags | TcpFlags::PSH;
        }
        let meta = TcpSegmentMeta {
            source_port: A_PORT,
            destination_port: B_PORT,
            seq: base.add(u32::try_from(offset).expect("offset fits u32")),
            ack: SeqNumber::new(0),
            flags,
            window: 2000,
            urgent: 0,
            options: TcpOptions::new(),
        };
        let out = soft
            .send_tcp_collect(IpAddr::V4(V4_B), &meta, &payload[offset..end], None, t(3))
            .expect("send soft segment");
        assert_eq!(out.frames.len(), 1);
        reference.push(out.frames[0].bytes[IP_TCP..].to_vec());
        offset = end;
    }
    assert_eq!(
        reference.len(),
        3,
        "250 bytes over MSS 100 is three segments"
    );

    // Offloaded: one super-segment (PSH set — it ends the data run).
    let meta = TcpSegmentMeta {
        source_port: A_PORT,
        destination_port: B_PORT,
        seq: base,
        ack: SeqNumber::new(0),
        flags: TcpFlags::ACK | TcpFlags::PSH,
        window: 2000,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let off_out = off
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, &payload, Some(MSS), t(3))
        .expect("send super-segment");
    assert_eq!(off_out.frames.len(), 1, "TSO emits one frame");
    let TxOffload::TcpSegment {
        csum_start,
        csum_offset,
        gso_size,
        hdr_len,
        ipv6,
    } = off_out.frames[0].offload
    else {
        panic!("expected a TcpSegment offload");
    };
    assert_eq!(
        (csum_start, csum_offset, gso_size, hdr_len, ipv6),
        (34, 16, MSS, 54, false)
    );

    // Software-segment the super-frame exactly as the device must, then
    // compare each produced TCP segment to the reference.
    let super_tcp = &off_out.frames[0].bytes[IP_TCP..];
    // The super-segment's checksum field holds the length-0 pseudo-header
    // partial sum (Linux `CHECKSUM_PARTIAL` for GSO): the device adds each
    // segment's own length when it splits.
    let field = u16::from_be_bytes([super_tcp[16], super_tcp[17]]);
    let expected_partial = Checksum::ipv4_pseudo(V4_A, V4_B, PROTOCOL_TCP, 0).partial();
    assert_eq!(
        field, expected_partial,
        "GSO super-segment carries the length-0 partial checksum"
    );
    let tcp_header_len = usize::from(hdr_len) - IP_TCP;
    let header = &super_tcp[..tcp_header_len];
    let body = &super_tcp[tcp_header_len..];
    let mut produced: Vec<Vec<u8>> = Vec::new();
    let mut off = 0usize;
    while off < body.len() {
        let end = (off + usize::from(gso_size)).min(body.len());
        let last = end == body.len();
        let mut seg = header.to_vec();
        // Advance the sequence number by the payload already segmented.
        let seq = base.add(u32::try_from(off).expect("offset fits u32"));
        seg[4..8].copy_from_slice(&seq.value().to_be_bytes());
        // The device sets PSH/FIN only on the final segment.
        if !last {
            seg[13] &= !(0x08 | 0x01);
        }
        // Zero the checksum field, append the payload slice, recompute the
        // complete checksum over the per-segment pseudo-header.
        seg[16..18].copy_from_slice(&[0, 0]);
        seg.extend_from_slice(&body[off..end]);
        let tcp_len = u16::try_from(seg.len()).expect("segment fits");
        let mut sum = Checksum::ipv4_pseudo(V4_A, V4_B, PROTOCOL_TCP, tcp_len);
        sum.push(&seg);
        seg[16..18].copy_from_slice(&sum.finish().to_be_bytes());
        produced.push(seg);
        off = end;
    }
    assert_eq!(
        produced, reference,
        "device segmentation reproduces the per-MSS software segments byte-for-byte"
    );
}

#[test]
fn ipv6_disabled_stack_forms_no_link_local_and_ignores_ra() {
    let mut config = StackConfig::new(facts(MAC_A), IID_A, 0x1234, STACK_HASH_KEY);
    config.iface.ipv6_enabled = false;
    let mut a = Stack::new(&config, temp_source(), t(0)).expect("valid facts");
    assert!(a.iface().ipv6_addresses().is_empty());
    // No DAD NS is emitted at bring-up for a disabled family.
    assert!(a.advance_collect(t(0)).frames.is_empty());
    // An inbound RA cannot SLAAC-configure a disabled interface: it is
    // dropped before parsing, so no address forms and nothing is sent.
    let ra = router_advertisement_frame(ALL_NODES, ipv6_multicast_mac(&ALL_NODES));
    let before = a.counters().rx_dropped;
    let out = a.on_frame_collect(&ra, t(2));
    assert!(out.frames.is_empty());
    assert_eq!(a.counters().rx_dropped, before + 1);
    assert!(a.iface().ipv6_addresses().is_empty());
}

#[test]
fn re_enabling_ipv6_on_a_stack_brings_the_link_local_up() {
    let mut config = StackConfig::new(facts(MAC_A), IID_A, 0x1234, STACK_HASH_KEY);
    config.iface.ipv6_enabled = false;
    let mut a = Stack::new(&config, temp_source(), t(0)).expect("valid facts");
    a.set_ipv6_enabled(true, t(0));
    // Bring-up proceeds exactly as for a natively-enabled interface.
    assert!(!a.advance_collect(t(0)).frames.is_empty()); // DAD NS
    a.advance_collect(t(1)); // DAD completion
    assert!(a.iface().is_assigned(link_local(IID_A)));
}

#[test]
fn disabling_ipv6_at_runtime_flushes_addresses_and_clears_routes() {
    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    bring_up(&mut a, &mut b);
    // Learn a router + SLAAC prefix, then complete the SLAAC DAD.
    let ra = router_advertisement_frame(ALL_NODES, ipv6_multicast_mac(&ALL_NODES));
    a.on_frame_collect(&ra, t(2));
    a.advance_collect(t(2));
    a.advance_collect(t(3));
    assert!(!a.iface().ipv6_addresses().is_empty());
    a.set_ipv6_enabled(false, t(4));
    assert!(a.iface().ipv6_addresses().is_empty());
    // An off-link v6 destination now has no route (the router list was
    // cleared), so origination fails closed rather than using a stale one.
    let off_link = Ipv6Addr::new(0x2001, 0x0DB8, 0xFF, 0, 0, 0, 0, 1);
    assert!(a
        .send_echo_request_collect(IpAddr::V6(off_link), 1, 1, b"x", t(4))
        .is_err());
}

#[test]
fn ipv4_disabled_stack_refuses_assignment() {
    let mut config = StackConfig::new(facts(MAC_A), IID_A, 0x1234, STACK_HASH_KEY);
    config.ipv4_enabled = false;
    let mut a = Stack::new(&config, temp_source(), t(0)).expect("valid facts");
    assert_eq!(
        a.set_ipv4_config(V4_A, 24, None),
        Err(crate::iface::AddrError::V4Disabled)
    );
    assert!(a.iface().ipv4().is_none());
}

#[test]
fn disabling_ipv4_at_runtime_drops_the_assignment_and_refuses_more() {
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("assign");
    assert!(a.iface().ipv4().is_some());
    a.set_ipv4_enabled(false);
    assert!(a.iface().ipv4().is_none());
    assert_eq!(
        a.set_ipv4_config(V4_A, 24, None),
        Err(crate::iface::AddrError::V4Disabled)
    );
    // Re-enabling permits assignment again (no auto-config restores it).
    a.set_ipv4_enabled(true);
    assert_eq!(a.set_ipv4_config(V4_A, 24, None), Ok(()));
    assert_eq!(a.iface().ipv4(), Some((V4_A, 24)));
}

#[test]
fn set_mtu_overrides_the_link_mtu_used_for_egress() {
    // A device that reported 1500 at bring-up, with an on-link v4
    // destination configured so the MSS query has a route.
    let mut a = stack(MAC_A, IID_A);
    a.set_ipv4_config(V4_A, 24, None).expect("assign v4");
    let v4_dest = IpAddr::V4(V4_B);
    // Default v4 MSS = 1500 - 20 (IPv4) - 20 (TCP) = 1460.
    assert_eq!(a.tcp_local_mss(v4_dest, t(0)), Some(1460));
    // Lowering the MTU to 1400 lowers the MSS by the same 100 bytes.
    a.set_mtu(1400);
    assert_eq!(a.tcp_local_mss(v4_dest, t(0)), Some(1360));
    // Raising it past the device value is honoured too (a jumbo link).
    a.set_mtu(9000);
    assert_eq!(a.tcp_local_mss(v4_dest, t(0)), Some(8960));
}

/// Engine-level RFC 3168 ECN carriage: a segment the caller marks ECT(0)
/// is emitted with that codepoint in its IPv4 header, and a received
/// CE-marked TCP datagram surfaces its codepoint to the service so the
/// connection can echo ECE.
#[test]
fn tcp_v4_ecn_is_stamped_on_emit_and_surfaced_on_receive() {
    use crate::addr::Ecn;
    use crate::tcp::{SeqNumber, TcpFlags, TcpOptions, TcpSegmentMeta};

    let mut a = stack(MAC_A, IID_A);
    let mut b = stack(MAC_B, IID_B);
    a.set_ipv4_config(V4_A, 24, None).expect("cfg a");
    b.set_ipv4_config(V4_B, 24, None).expect("cfg b");
    // Resolve both neighbour caches with one echo round trip A <-> B, so a
    // later `send_tcp` emits synchronously rather than parking on ARP.
    let out = a
        .send_echo_request_collect(IpAddr::V4(V4_B), 1, 1, b"x", t(2))
        .expect("echo");
    let mut frames: VecDeque<Vec<u8>> = tx_bytes(out.frames).into();
    for _ in 0..8 {
        let Some(f) = frames.pop_front() else { break };
        let ob = b.on_frame_collect(&f, t(2));
        for r in tx_bytes(ob.frames) {
            frames.extend(tx_bytes(a.on_frame_collect(&r, t(2)).frames));
        }
    }

    let meta = TcpSegmentMeta {
        source_port: A_PORT,
        destination_port: B_PORT,
        seq: SeqNumber::new(1),
        ack: SeqNumber::new(0),
        flags: TcpFlags::SYN,
        window: 1000,
        urgent: 0,
        options: TcpOptions::new(),
    };

    // Emit: the ECT(0) request lands in the IPv4 TOS byte.
    let ect = a
        .send_tcp_ecn_collect(IpAddr::V4(V4_B), &meta, b"d", None, Ecn::Ect0, t(3))
        .expect("send ect");
    assert_eq!(ect.frames.len(), 1);
    let eth = EthernetFrame::parse(&ect.frames[0].bytes).expect("eth");
    let (hdr, _o, _p) = Ipv4Header::parse(eth.payload).expect("ipv4");
    assert_eq!(hdr.ecn, Ecn::Ect0, "the datagram carries ECT(0)");

    // A Not-ECT send (the default path) stays Not-ECT.
    let plain = a
        .send_tcp_collect(IpAddr::V4(V4_B), &meta, b"d", None, t(3))
        .expect("send plain");
    let eth = EthernetFrame::parse(&plain.frames[0].bytes).expect("eth");
    let (hdr, _o, _p) = Ipv4Header::parse(eth.payload).expect("ipv4");
    assert_eq!(hdr.ecn, Ecn::NotEct, "an unmarked datagram is Not-ECT");

    // Receive: a CE-marked TCP datagram from B surfaces its codepoint.
    let ce = b
        .send_tcp_ecn_collect(IpAddr::V4(V4_A), &meta, b"d", None, Ecn::Ce, t(3))
        .expect("send ce");
    let recv = a.on_frame_collect(&ce.frames[0].bytes, t(3));
    let ecn = recv
        .events
        .iter()
        .find_map(|e| match e {
            StackEvent::TcpSegment { ecn, .. } => Some(*ecn),
            _ => None,
        })
        .expect("a TCP segment event");
    assert_eq!(ecn, Ecn::Ce, "the received CE mark is surfaced");
}

/// The device-facing group set must be exactly what the receive path
/// accepts: a NIC programmed with less drops frames the stack would have
/// answered, and one programmed with more admits traffic it will only throw
/// away.
#[test]
fn the_device_group_set_matches_what_the_receive_path_accepts() {
    use crate::eth::{ipv4_multicast_mac, ipv6_multicast_mac};

    let mut s = stack(MacAddress::new([0x02, 0, 0, 0, 0, 1]), [1; 8]);
    let mut out = StackOutput::default();
    s.advance(t(0), &mut out);
    let mut macs = Vec::new();

    // A fresh interface already needs all-nodes and the solicited-node group
    // of its tentative link-local: DAD listens on the latter, so a filter
    // without it would pass DAD against a duplicate that could not answer.
    s.multicast_macs(&mut macs);
    assert!(macs.contains(&ipv6_multicast_mac(&ALL_NODES)));
    for info in s.iface().ipv6_addresses() {
        assert!(
            macs.contains(&ipv6_multicast_mac(&solicited_node_multicast(&info.addr))),
            "the solicited-node group of {:?} is admitted",
            info.addr
        );
    }

    // Joining a group of either family adds exactly its link-layer address.
    let v4_group = Ipv4Addr::new(224, 0, 0, 251);
    let v6_group = Ipv6Addr::new(0xFF02, 0, 0, 0, 0, 0, 0, 0xFB);
    s.join_multicast(IpAddr::V4(v4_group), t(1)).expect("joins");
    s.join_multicast(IpAddr::V6(v6_group), t(1)).expect("joins");
    let before = s.multicast_revision();
    s.multicast_macs(&mut macs);
    assert!(macs.contains(&ipv4_multicast_mac(&v4_group)));
    assert!(macs.contains(&ipv6_multicast_mac(&v6_group)));

    // Leaving takes it back out, and the revision moves so a mirroring
    // device is reprogrammed.
    s.leave_multicast(IpAddr::V6(v6_group), t(2));
    assert_ne!(s.multicast_revision(), before);
    s.multicast_macs(&mut macs);
    assert!(!macs.contains(&ipv6_multicast_mac(&v6_group)));

    // Every entry is a group address — a unicast one would widen a device's
    // filter to another host — and none repeats.
    for (i, mac) in macs.iter().enumerate() {
        assert!(crate::eth::is_group_mac(*mac), "{mac:?} is a group address");
        assert!(!macs[..i].contains(mac), "{mac:?} appears once");
    }
}

#[test]
fn the_multicast_revision_is_stable_while_nothing_changes() {
    let mut s = stack(MacAddress::new([0x02, 0, 0, 0, 0, 2]), [2; 8]);
    let mut out = StackOutput::default();
    // Bring-up itself moves the revision (the link-local completing DAD
    // joins its solicited-node group), so settle first.
    for secs in 0..60 {
        s.advance(t(secs), &mut out);
    }
    let settled = s.multicast_revision();
    // The frame pump consults this every pass; a value that drifted on its
    // own would reprogram the device's filter forever.
    for secs in 60..80 {
        s.advance(t(secs), &mut out);
    }
    assert_eq!(s.multicast_revision(), settled);

    // Disabling IPv6 drops its groups, so the revision must move.
    s.set_ipv6_enabled(false, t(80));
    assert_ne!(s.multicast_revision(), settled);
    let mut macs = Vec::new();
    s.multicast_macs(&mut macs);
    assert!(macs.is_empty(), "no family is on, so nothing is admitted");
}
