//! Unit tests for the `DHCPv4` client engine.

use super::*;
use alloc::vec::Vec;
use tairix_abi::time::Duration64;

const MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
const SERVER: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
const LEASED: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 50);
const MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const DNS: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
const NTP: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 123);

fn secs(s: i64) -> Duration64 {
    Duration64::from_secs(s)
}

/// A counting RNG: returns a fixed, distinct value each call, so `xid`s are
/// deterministic and the backoff jitter is reproducible in tests.
fn counter() -> impl FnMut() -> u32 {
    let mut n: u32 = 0x1000_0000;
    move || {
        n = n.wrapping_add(0x0101_0101);
        n
    }
}

/// Build a server reply (OFFER/ACK/NAK) for `xid`/`MAC` with the given
/// options, exercising the same encoding a real server would.
fn server_reply(
    msg: MessageType,
    xid: u32,
    yiaddr: Ipv4Addr,
    server_id: Option<Ipv4Addr>,
    lease_secs: Option<u32>,
) -> Vec<u8> {
    let mut out = alloc::vec![0u8; 512];
    out[0] = 2; // BOOTREPLY
    out[1] = 1; // Ethernet
    out[2] = 6; // hlen
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    out[16..20].copy_from_slice(&yiaddr.octets());
    out[28..34].copy_from_slice(&MAC.0);
    out[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let mut i = 240;
    let mut opt = |i: &mut usize, code: u8, data: &[u8]| {
        out[*i] = code;
        out[*i + 1] = u8::try_from(data.len()).expect("option data fits a byte");
        out[*i + 2..*i + 2 + data.len()].copy_from_slice(data);
        *i += 2 + data.len();
    };
    opt(&mut i, 53, &[msg.code()]);
    if let Some(id) = server_id {
        opt(&mut i, 54, &id.octets());
    }
    opt(&mut i, 1, &MASK.octets());
    opt(&mut i, 3, &SERVER.octets());
    opt(&mut i, 6, &DNS.octets());
    opt(&mut i, opt::NTP_SERVER, &NTP.octets());
    if let Some(l) = lease_secs {
        opt(&mut i, 51, &l.to_be_bytes());
    }
    out[i] = 255;
    out.truncate(i + 1);
    out
}

/// Extract the single send action from a set of actions.
fn one_send(actions: &[Action]) -> SendAction {
    actions
        .iter()
        .find_map(|a| match a {
            Action::Send(s) => Some(*s),
            _ => None,
        })
        .expect("a send action")
}

fn lease_action(actions: &[Action]) -> Lease {
    actions
        .iter()
        .find_map(|a| match a {
            Action::Configured(l) => Some(*l),
            _ => None,
        })
        .expect("a configured action")
}

#[test]
fn message_type_round_trips() {
    for code in 0u8..=9 {
        match MessageType::from_code(code) {
            Some(mt) => assert_eq!(mt.code(), code),
            None => assert!(code == 0 || code == 9),
        }
    }
}

#[test]
fn write_message_encodes_a_discover() {
    let spec = MessageSpec {
        message_type: MessageType::Discover,
        xid: 0xDEAD_BEEF,
        secs: 3,
        broadcast: true,
        client_addr: Ipv4Addr::UNSPECIFIED,
        chaddr: MAC,
        requested_addr: None,
        server_id: None,
    };
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    let n = write_message(&spec, &mut buf).expect("write");
    assert_eq!(n, MAX_MESSAGE_LEN);
    assert_eq!(buf[0], 1); // BOOTREQUEST
    assert_eq!(buf[1], 1); // Ethernet
    assert_eq!(buf[2], 6); // hlen
    assert_eq!(&buf[4..8], &0xDEAD_BEEFu32.to_be_bytes());
    assert_eq!(u16::from_be_bytes([buf[10], buf[11]]), 0x8000); // broadcast flag
    assert_eq!(&buf[28..34], &MAC.0);
    assert_eq!(&buf[236..240], &[99, 130, 83, 99]);
    // The options carry the message type (option 53 = 1).
    assert_eq!(&buf[240..243], &[53, 1, 1]);
}

#[test]
fn write_message_fails_closed_on_short_buffer() {
    let spec = MessageSpec {
        message_type: MessageType::Discover,
        xid: 0,
        secs: 0,
        broadcast: false,
        client_addr: Ipv4Addr::UNSPECIFIED,
        chaddr: MAC,
        requested_addr: None,
        server_id: None,
    };
    let mut tiny = [0u8; MAX_MESSAGE_LEN - 1];
    assert_eq!(
        write_message(&spec, &mut tiny),
        Err(WriteError::BufferTooSmall)
    );
}

#[test]
fn parse_accepts_a_well_formed_offer() {
    let bytes = server_reply(MessageType::Offer, 0x1234, LEASED, Some(SERVER), Some(3600));
    let reply = DhcpReply::parse(&bytes, 0x1234, MAC).expect("parse");
    assert_eq!(reply.message_type, MessageType::Offer);
    assert_eq!(reply.your_addr, LEASED);
    assert_eq!(reply.server_id, Some(SERVER));
    assert_eq!(reply.subnet_mask, Some(MASK));
    assert_eq!(reply.routers.first(), Some(SERVER));
    assert_eq!(reply.dns_servers.first(), Some(DNS));
    assert_eq!(reply.ntp_servers.first(), Some(NTP));
    assert_eq!(reply.lease_secs, Some(3600));
}

#[test]
fn the_parameter_request_list_asks_for_the_time_servers() {
    let spec = MessageSpec {
        message_type: MessageType::Discover,
        xid: 1,
        secs: 0,
        broadcast: true,
        client_addr: Ipv4Addr::UNSPECIFIED,
        chaddr: MAC,
        requested_addr: None,
        server_id: None,
    };
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    write_message(&spec, &mut buf).expect("write");
    let mut i = OPTIONS_OFFSET;
    let mut asked = None;
    while i + 2 <= buf.len() {
        let code = buf[i];
        if code == opt::END {
            break;
        }
        let len = usize::from(buf[i + 1]);
        if code == opt::PARAMETER_REQUEST_LIST {
            asked = buf.get(i + 2..i + 2 + len).map(<[u8]>::to_vec);
            break;
        }
        i += 2 + len;
    }
    let asked = asked.expect("the request carries a parameter request list");
    assert!(
        asked.contains(&opt::NTP_SERVER),
        "a server only supplies option 42 when it is asked for: {asked:?}"
    );
}

#[test]
fn a_time_server_option_past_the_fixed_bound_is_dropped_whole() {
    // Five addresses offered, four the fixed capacity admits: the excess is
    // ignored rather than sizing anything on the server's word.
    let mut bytes = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    let end = bytes.len() - 1;
    let mut wide = Vec::new();
    for last in 1..=5u8 {
        wide.extend_from_slice(&Ipv4Addr::new(10, 0, 0, last).octets());
    }
    bytes.truncate(end);
    bytes.push(opt::NTP_SERVER);
    bytes.push(u8::try_from(wide.len()).expect("fits"));
    bytes.extend_from_slice(&wide);
    bytes.push(opt::END);
    let reply = DhcpReply::parse(&bytes, 0x1234, MAC).expect("parse");
    assert_eq!(
        reply.ntp_servers.len(),
        MAX_ADDRESSES,
        "the list is capped at its fixed bound"
    );
    assert_eq!(reply.ntp_servers.first(), Some(NTP), "wire order is kept");
}

#[test]
fn a_truncated_trailing_time_server_address_is_ignored() {
    let mut bytes = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    let end = bytes.len() - 1;
    bytes.truncate(end);
    bytes.push(opt::NTP_SERVER);
    bytes.push(6);
    bytes.extend_from_slice(&[10, 0, 0, 9, 10, 0]);
    bytes.push(opt::END);
    let reply = DhcpReply::parse(&bytes, 0x1234, MAC).expect("parse");
    assert_eq!(
        reply.ntp_servers.as_slice(),
        &[NTP, Ipv4Addr::new(10, 0, 0, 9)],
        "the whole address parses; the two-octet tail is dropped"
    );
}

#[test]
fn parse_rejects_wrong_transaction_id() {
    let bytes = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    assert!(DhcpReply::parse(&bytes, 0x9999, MAC).is_none());
}

#[test]
fn parse_rejects_wrong_hardware_address() {
    let bytes = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    let other = MacAddress([0, 0, 0, 0, 0, 1]);
    assert!(DhcpReply::parse(&bytes, 0x1234, other).is_none());
}

#[test]
fn parse_rejects_bootrequest_op() {
    let mut bytes = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    bytes[0] = 1; // BOOTREQUEST, not a reply
    assert!(DhcpReply::parse(&bytes, 0x1234, MAC).is_none());
}

#[test]
fn parse_rejects_bad_cookie() {
    let mut bytes = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    bytes[236] ^= 0xFF;
    assert!(DhcpReply::parse(&bytes, 0x1234, MAC).is_none());
}

#[test]
fn parse_rejects_message_without_a_type() {
    // A reply whose only option is END carries no message type (opt 53).
    let mut bytes = alloc::vec![0u8; 241];
    bytes[0] = 2;
    bytes[1] = 1;
    bytes[2] = 6;
    bytes[4..8].copy_from_slice(&0x1234u32.to_be_bytes());
    bytes[28..34].copy_from_slice(&MAC.0);
    bytes[236..240].copy_from_slice(&[99, 130, 83, 99]);
    bytes[240] = 255;
    assert!(DhcpReply::parse(&bytes, 0x1234, MAC).is_none());
}

#[test]
fn parse_is_total_on_truncation() {
    let full = server_reply(MessageType::Ack, 0x1234, LEASED, Some(SERVER), Some(60));
    for len in 0..full.len() {
        // Must never panic for any prefix.
        let _ = DhcpReply::parse(&full[..len], 0x1234, MAC);
    }
}

#[test]
fn parse_honours_option_overload() {
    // Place the message-type option in the `file` field and set overload.
    let mut bytes = alloc::vec![0u8; 512];
    bytes[0] = 2;
    bytes[1] = 1;
    bytes[2] = 6;
    bytes[4..8].copy_from_slice(&0x55u32.to_be_bytes());
    bytes[16..20].copy_from_slice(&LEASED.octets());
    bytes[28..34].copy_from_slice(&MAC.0);
    bytes[236..240].copy_from_slice(&[99, 130, 83, 99]);
    // Main options: overload = file (bit 0), then END.
    bytes[240] = 52;
    bytes[241] = 1;
    bytes[242] = 0b01;
    bytes[243] = 255;
    // The `file` field (offset 108) carries the message type + lease.
    bytes[108] = 53;
    bytes[109] = 1;
    bytes[110] = MessageType::Ack.code();
    bytes[111] = 51;
    bytes[112] = 4;
    bytes[113..117].copy_from_slice(&1200u32.to_be_bytes());
    bytes[117] = 255;
    let reply = DhcpReply::parse(&bytes, 0x55, MAC).expect("overloaded parse");
    assert_eq!(reply.message_type, MessageType::Ack);
    assert_eq!(reply.lease_secs, Some(1200));
}

#[test]
fn address_list_is_bounded() {
    // Six routers offered, only MAX_ADDRESSES surfaced.
    let mut list = AddressList::default();
    let mut data = Vec::new();
    for i in 0..6u8 {
        data.extend_from_slice(&[10, 0, 0, i]);
    }
    list.extend_from_bytes(&data);
    assert_eq!(list.len(), MAX_ADDRESSES);
}

/// Drive a client from INIT to BOUND with a `lease_secs` lease, returning
/// the client and the committed lease. `t` is the (constant) instant of
/// the exchange.
fn acquire(lease_secs: u32, t: Duration64) -> (DhcpClient, Lease) {
    let mut rng = counter();
    let mut client = DhcpClient::new(MAC);
    let discover = client.poll(t, &mut rng);
    assert_eq!(client.state(), State::Selecting);
    assert_eq!(one_send(&discover).spec.message_type, MessageType::Discover);
    assert_eq!(one_send(&discover).destination, Destination::Broadcast);

    let xid = client.transaction_id();
    let offer = server_reply(
        MessageType::Offer,
        xid,
        LEASED,
        Some(SERVER),
        Some(lease_secs),
    );
    let offer = DhcpReply::parse(&offer, xid, MAC).expect("offer parses");
    let req = client.on_reply(t, &offer);
    assert_eq!(client.state(), State::Requesting);
    let send = one_send(&req);
    assert_eq!(send.spec.message_type, MessageType::Request);
    assert_eq!(send.spec.requested_addr, Some(LEASED));
    assert_eq!(send.spec.server_id, Some(SERVER));
    assert_eq!(send.destination, Destination::Broadcast);

    let ack = server_reply(
        MessageType::Ack,
        xid,
        LEASED,
        Some(SERVER),
        Some(lease_secs),
    );
    let ack = DhcpReply::parse(&ack, xid, MAC).expect("ack parses");
    let actions = client.on_reply(t, &ack);
    assert_eq!(client.state(), State::Bound);
    let lease = lease_action(&actions);
    assert_eq!(lease.addr, LEASED);
    assert_eq!(lease.lease_secs, lease_secs);
    (client, lease)
}

#[test]
fn full_acquisition_reaches_bound_with_config() {
    let (client, lease) = acquire(3600, secs(0));
    assert_eq!(lease.addr, LEASED);
    assert_eq!(lease.subnet_mask, Some(MASK));
    assert_eq!(lease.router, Some(SERVER));
    assert_eq!(lease.dns_servers.first(), Some(DNS));
    assert_eq!(lease.ntp_servers.first(), Some(NTP));
    assert_eq!(lease.server_id, Some(SERVER));
    // T1 = lease/2 = 1800s.
    assert_eq!(client.next_deadline(), Some(secs(1800)));
}

#[test]
fn offer_without_server_id_is_ignored() {
    let mut rng = counter();
    let mut client = DhcpClient::new(MAC);
    client.poll(secs(0), &mut rng);
    let xid = client.transaction_id();
    let offer = server_reply(MessageType::Offer, xid, LEASED, None, Some(60));
    let offer = DhcpReply::parse(&offer, xid, MAC).expect("parses");
    let actions = client.on_reply(secs(0), &offer);
    assert!(actions.is_empty());
    assert_eq!(client.state(), State::Selecting);
}

#[test]
fn nak_in_requesting_restarts_from_init() {
    let mut rng = counter();
    let mut client = DhcpClient::new(MAC);
    client.poll(secs(0), &mut rng);
    let xid = client.transaction_id();
    let offer = server_reply(MessageType::Offer, xid, LEASED, Some(SERVER), Some(60));
    let offer = DhcpReply::parse(&offer, xid, MAC).expect("parses");
    client.on_reply(secs(0), &offer);
    let nak = server_reply(
        MessageType::Nak,
        xid,
        Ipv4Addr::UNSPECIFIED,
        Some(SERVER),
        None,
    );
    let nak = DhcpReply::parse(&nak, xid, MAC).expect("parses");
    let actions = client.on_reply(secs(0), &nak);
    assert!(actions.is_empty());
    assert_eq!(client.state(), State::Init);
    // The next poll re-DISCOVERs with a fresh transaction id.
    let old_xid = client.transaction_id();
    let redo = client.poll(secs(0), &mut rng);
    assert_eq!(one_send(&redo).spec.message_type, MessageType::Discover);
    assert_ne!(client.transaction_id(), old_xid);
}

#[test]
fn selecting_retransmits_discover_with_growing_backoff() {
    let mut rng = counter();
    let mut client = DhcpClient::new(MAC);
    client.poll(secs(0), &mut rng);
    let first = client.next_deadline().expect("armed");
    // No work before the deadline.
    assert!(client.poll(secs(1), &mut rng).is_empty());
    // At the deadline a fresh DISCOVER goes out and the next deadline grows.
    let acts = client.poll(first, &mut rng);
    assert_eq!(one_send(&acts).spec.message_type, MessageType::Discover);
    let second = client.next_deadline().expect("armed");
    assert!(second > first);
    assert_eq!(client.state(), State::Selecting);
}

#[test]
fn requesting_gives_up_after_retries_and_restarts() {
    let mut rng = counter();
    let mut client = DhcpClient::new(MAC);
    client.poll(secs(0), &mut rng);
    let xid = client.transaction_id();
    let offer = server_reply(MessageType::Offer, xid, LEASED, Some(SERVER), Some(60));
    let offer = DhcpReply::parse(&offer, xid, MAC).expect("parses");
    client.on_reply(secs(0), &offer);
    assert_eq!(client.state(), State::Requesting);
    // Drive the retransmit deadline forward far enough, several times.
    let mut saw_discover = false;
    for _ in 0..10 {
        let d = client.next_deadline().expect("armed");
        let acts = client.poll(secs(d.secs() + 1), &mut rng);
        if acts
            .iter()
            .any(|a| matches!(a, Action::Send(s) if s.spec.message_type == MessageType::Discover))
        {
            saw_discover = true;
            break;
        }
    }
    assert!(saw_discover, "eventually restarts acquisition");
    assert_eq!(client.state(), State::Selecting);
}

#[test]
fn bound_renews_at_t1_by_unicast() {
    let (mut client, _) = acquire(100, secs(0));
    // T1 = 50s.
    assert_eq!(client.next_deadline(), Some(secs(50)));
    let mut rng = counter();
    let acts = client.poll(secs(50), &mut rng);
    assert_eq!(client.state(), State::Renewing);
    let send = one_send(&acts);
    assert_eq!(send.spec.message_type, MessageType::Request);
    assert_eq!(send.destination, Destination::Server(SERVER));
    // Renew form: ciaddr set, no requested-address / server-id options.
    assert_eq!(send.spec.client_addr, LEASED);
    assert_eq!(send.spec.requested_addr, None);
    assert_eq!(send.spec.server_id, None);
    assert!(!send.spec.broadcast);
}

#[test]
fn renewing_ack_returns_to_bound() {
    let (mut client, _) = acquire(100, secs(0));
    let mut rng = counter();
    client.poll(secs(50), &mut rng);
    assert_eq!(client.state(), State::Renewing);
    let xid = client.transaction_id();
    let ack = server_reply(MessageType::Ack, xid, LEASED, Some(SERVER), Some(100));
    let ack = DhcpReply::parse(&ack, xid, MAC).expect("parses");
    let acts = client.on_reply(secs(50), &ack);
    assert_eq!(client.state(), State::Bound);
    assert_eq!(lease_action(&acts).addr, LEASED);
    // The renewal re-anchored the lease at 50s, so T1 is now 50 + 50 = 100s.
    assert_eq!(client.next_deadline(), Some(secs(100)));
}

#[test]
fn renewing_to_rebinding_at_t2() {
    let (mut client, _) = acquire(100, secs(0));
    let mut rng = counter();
    client.poll(secs(50), &mut rng); // -> Renewing
                                     // T2 = 87s (lease*7/8).
    let acts = client.poll(secs(87), &mut rng);
    assert_eq!(client.state(), State::Rebinding);
    let send = one_send(&acts);
    assert_eq!(send.spec.message_type, MessageType::Request);
    assert_eq!(send.destination, Destination::Broadcast);
    assert!(send.spec.broadcast);
}

#[test]
fn rebinding_expiry_deconfigures_and_reacquires() {
    let (mut client, _) = acquire(100, secs(0));
    let mut rng = counter();
    client.poll(secs(50), &mut rng); // Renewing
    client.poll(secs(87), &mut rng); // Rebinding
    let acts = client.poll(secs(100), &mut rng); // expiry
    assert!(acts.iter().any(|a| matches!(a, Action::Deconfigured)));
    assert!(acts
        .iter()
        .any(|a| matches!(a, Action::Send(s) if s.spec.message_type == MessageType::Discover)));
    assert_eq!(client.state(), State::Selecting);
    assert!(client.lease().is_none());
}

#[test]
fn renewing_nak_deconfigures() {
    let (mut client, _) = acquire(100, secs(0));
    let mut rng = counter();
    client.poll(secs(50), &mut rng); // Renewing
    let xid = client.transaction_id();
    let nak = server_reply(
        MessageType::Nak,
        xid,
        Ipv4Addr::UNSPECIFIED,
        Some(SERVER),
        None,
    );
    let nak = DhcpReply::parse(&nak, xid, MAC).expect("parses");
    let acts = client.on_reply(secs(50), &nak);
    assert!(acts.iter().any(|a| matches!(a, Action::Deconfigured)));
    assert_eq!(client.state(), State::Init);
    assert!(client.lease().is_none());
}

#[test]
fn infinite_lease_arms_no_renewal() {
    let (client, lease) = acquire(INFINITE_LEASE_SECS, secs(0));
    assert_eq!(lease.lease_secs, INFINITE_LEASE_SECS);
    assert_eq!(client.state(), State::Bound);
    assert_eq!(client.next_deadline(), None);
}

#[test]
fn ack_without_lease_time_is_not_committed() {
    let mut rng = counter();
    let mut client = DhcpClient::new(MAC);
    client.poll(secs(0), &mut rng);
    let xid = client.transaction_id();
    let offer = server_reply(MessageType::Offer, xid, LEASED, Some(SERVER), Some(60));
    let offer = DhcpReply::parse(&offer, xid, MAC).expect("parses");
    client.on_reply(secs(0), &offer);
    // An ACK carrying no lease-time option is unusable.
    let ack = server_reply(MessageType::Ack, xid, LEASED, Some(SERVER), None);
    let ack = DhcpReply::parse(&ack, xid, MAC).expect("parses");
    let acts = client.on_reply(secs(0), &ack);
    assert!(acts.is_empty());
    assert_eq!(client.state(), State::Requesting);
}

#[test]
fn renewal_times_default_and_option_supplied() {
    // Defaults: T1 = lease/2, T2 = lease*7/8.
    assert_eq!(renewal_times(100, None, None), (50, 87));
    // Consistent server values are honoured.
    assert_eq!(renewal_times(100, Some(40), Some(80)), (40, 80));
    // Inconsistent values (T1 >= lease, T2 <= T1) fall back / are ordered.
    assert_eq!(renewal_times(100, Some(200), None), (50, 87));
    let (t1, t2) = renewal_times(100, Some(60), Some(50));
    assert!(t1 < t2, "the ordering is always made strict: {t1} < {t2}");
}

#[test]
fn stale_reply_for_a_past_transaction_is_ignored() {
    let (mut client, _) = acquire(100, secs(0));
    // A reply carrying a transaction id other than the current one is
    // dropped without effect even if fed directly.
    let stale = DhcpReply {
        message_type: MessageType::Nak,
        xid: client.transaction_id().wrapping_add(1),
        your_addr: Ipv4Addr::UNSPECIFIED,
        server_id: None,
        subnet_mask: None,
        routers: AddressList::default(),
        dns_servers: AddressList::default(),
        ntp_servers: AddressList::default(),
        lease_secs: None,
        renewal_secs: None,
        rebinding_secs: None,
    };
    let acts = client.on_reply(secs(10), &stale);
    assert!(acts.is_empty());
    assert_eq!(client.state(), State::Bound);
}
