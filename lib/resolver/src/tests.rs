//! Host tests for the pure resolver orchestration.
//!
//! These drive [`resolve_name`] against two in-memory fakes — a scripted
//! System Information API transport for the server-set fetch and a scripted
//! [`DnsTransport`] that answers with crafted DNS datagrams — so the whole
//! "fetch servers, then drive the engine" path runs with no kernel and no
//! network. The DNS codec and the resolver state machine themselves are
//! covered by `lib/net`; here we prove the orchestration, the server
//! conversion, and the error mapping.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_abi::net_ipc::{NetAddrFamily, NetResolverServer};
use tairix_abi::sysinfo::{NetInterfaceListRequest, SysinfoQueryId, SysinfoRequestHeader};
use tairix_abi::time::Duration64;
use tairix_abi::Errno;
use tairix_net::addr::{IpAddr, Ipv4Addr, Ipv6Addr};
use tairix_net::dns::{DnsTransport, RecordType, ResolveStatus, Wait};

use super::{configured_servers, resolve_name, ResolveError};

// -- The System Information API fake -------------------------------------

/// A `sysinfod` stand-in answering the `NET_RESOLVER_SERVERS` query from a
/// fixed record set, decoding the request exactly as the real service and
/// paging like it (a short page terminates the walk). It records which
/// query id it saw so a test can prove the resolver used the shared query.
struct SysinfoFake {
    servers: Vec<NetResolverServer>,
    deny: bool,
    seen: core::cell::RefCell<Vec<SysinfoQueryId>>,
}

impl SysinfoFake {
    fn new(servers: Vec<NetResolverServer>) -> Self {
        Self {
            servers,
            deny: false,
            seen: core::cell::RefCell::new(Vec::new()),
        }
    }

    fn denying() -> Self {
        let mut fake = Self::new(Vec::new());
        fake.deny = true;
        fake
    }
}

impl tairix_procinfo::Transport for SysinfoFake {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        let header = SysinfoRequestHeader::from_bytes(request)?;
        self.seen.borrow_mut().push(header.query);
        assert_eq!(header.query, SysinfoQueryId::NET_RESOLVER_SERVERS);
        if self.deny {
            return Err(Errno::PermissionDenied);
        }
        let payload = &request[SysinfoRequestHeader::WIRE_LEN
            ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
        let req = NetInterfaceListRequest::from_bytes(payload)?;
        let offset = req.offset as usize;
        if offset >= self.servers.len() {
            return Ok(Vec::new());
        }
        let take = core::cmp::min(self.servers.len() - offset, req.limit as usize);
        let mut out = Vec::with_capacity(take * NetResolverServer::WIRE_LEN);
        for record in &self.servers[offset..offset + take] {
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }
}

fn v4_record(a: u8, b: u8, c: u8, d: u8) -> NetResolverServer {
    let mut addr = [0u8; 16];
    addr[..4].copy_from_slice(&[a, b, c, d]);
    NetResolverServer {
        family: NetAddrFamily::V4,
        addr,
    }
}

fn v6_record(bytes: [u8; 16]) -> NetResolverServer {
    NetResolverServer {
        family: NetAddrFamily::V6,
        addr: bytes,
    }
}

// -- The DNS transport fake ----------------------------------------------

/// A scripted responder: given the server queried and the encoded query
/// bytes, it yields the datagrams to deliver (empty drops the query so the
/// retransmit deadline fires).
type Responder = Box<dyn FnMut(IpAddr, &[u8]) -> Vec<Vec<u8>>>;

/// A scripted fake UDP transport for the resolver: mirrors the driver's
/// clock/deadline contract (an empty inbox times out and advances the clock
/// to the deadline; a queued datagram is delivered before it). Records the
/// servers it was asked to send to.
struct DnsFake {
    clock: Duration64,
    responder: Responder,
    inbox: Vec<Vec<u8>>,
    sent_to: Vec<IpAddr>,
    send_err: Option<Errno>,
}

impl DnsFake {
    fn new(responder: impl FnMut(IpAddr, &[u8]) -> Vec<Vec<u8>> + 'static) -> Self {
        Self {
            clock: Duration64::from_secs(0),
            responder: Box::new(responder),
            inbox: Vec::new(),
            sent_to: Vec::new(),
            send_err: None,
        }
    }
}

impl DnsTransport for DnsFake {
    fn now(&mut self) -> Duration64 {
        self.clock
    }

    fn send(&mut self, server: IpAddr, query: &[u8]) -> Result<(), Errno> {
        if let Some(err) = self.send_err {
            return Err(err);
        }
        self.sent_to.push(server);
        let mut produced = (self.responder)(server, query);
        self.inbox.append(&mut produced);
        Ok(())
    }

    fn wait(&mut self, deadline: Duration64, buf: &mut [u8]) -> Result<Wait, Errno> {
        if self.inbox.is_empty() {
            self.clock = deadline;
            Ok(Wait::TimedOut)
        } else {
            let datagram = self.inbox.remove(0);
            let len = datagram.len().min(buf.len());
            buf[..len].copy_from_slice(&datagram[..len]);
            Ok(Wait::Datagram(len))
        }
    }
}

/// Build a positive A-record response echoing the encoded query `q`'s id and
/// question, exactly as a recursive server would, answering with `addr`.
fn a_response(q: &[u8], addr: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    // Header: echoed id, then QR|RD|RA flags, qd=1, an=1, ns=0, ar=0.
    out.extend_from_slice(&[q[0], q[1]]);
    out.extend_from_slice(&0x8180u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    // Question: copy the query's question verbatim so the engine's echoed
    // question check matches.
    out.extend_from_slice(&q[12..]);
    // Answer: a compression pointer to the question name at offset 12, then
    // A/IN/ttl/rdlength/rdata.
    out.extend_from_slice(&0xC00Cu16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&300u32.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&addr);
    out
}

fn counter_rng() -> impl FnMut() -> u32 {
    let mut n: u32 = 0;
    move || {
        n = n.wrapping_add(0x9E37_79B9);
        n
    }
}

// -- configured_servers --------------------------------------------------

#[test]
fn configured_servers_converts_and_orders_v4_then_v6() {
    let fake = SysinfoFake::new(alloc::vec![v4_record(10, 0, 2, 3), v6_record([0x26; 16]),]);
    let servers = configured_servers(&fake).expect("ok");
    assert_eq!(
        servers,
        alloc::vec![
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 3)),
            IpAddr::V6(Ipv6Addr::from([0x26; 16])),
        ]
    );
    assert_eq!(
        fake.seen.borrow().as_slice(),
        &[SysinfoQueryId::NET_RESOLVER_SERVERS]
    );
}

#[test]
fn configured_servers_surfaces_a_denial() {
    let fake = SysinfoFake::denying();
    assert_eq!(configured_servers(&fake), Err(Errno::PermissionDenied));
}

// -- resolve_name --------------------------------------------------------

#[test]
fn resolves_a_record_via_the_configured_server() {
    let sysinfo = SysinfoFake::new(alloc::vec![v4_record(10, 0, 2, 3)]);
    let mut udp = DnsFake::new(|_server, q| alloc::vec![a_response(q, [93, 184, 216, 34])]);
    let mut rng = counter_rng();
    let resolution = resolve_name("example.com", RecordType::A, &sysinfo, &mut udp, &mut rng)
        .expect("no transport error");
    assert_eq!(resolution.status, ResolveStatus::Success);
    assert_eq!(
        resolution.addresses.first(),
        Some(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)))
    );
    // The query went to the one configured server.
    assert_eq!(
        udp.sent_to.first(),
        Some(&IpAddr::V4(Ipv4Addr::new(10, 0, 2, 3)))
    );
}

#[test]
fn no_configured_server_is_a_distinct_error_not_a_timeout() {
    let sysinfo = SysinfoFake::new(Vec::new());
    let mut udp = DnsFake::new(|_server, _q| Vec::new());
    let mut rng = counter_rng();
    let result = resolve_name("example.com", RecordType::A, &sysinfo, &mut udp, &mut rng);
    assert_eq!(result, Err(ResolveError::NoServers));
    // Nothing was ever sent — the engine was never driven.
    assert!(udp.sent_to.is_empty());
}

#[test]
fn a_silent_server_resolves_to_a_timeout() {
    // The server never answers, so the engine exhausts its retry budget and
    // concludes a timeout — a Resolution, not an error.
    let sysinfo = SysinfoFake::new(alloc::vec![v4_record(10, 0, 2, 3)]);
    let mut udp = DnsFake::new(|_server, _q| Vec::new());
    let mut rng = counter_rng();
    let resolution = resolve_name("example.com", RecordType::A, &sysinfo, &mut udp, &mut rng)
        .expect("no transport error");
    assert_eq!(resolution.status, ResolveStatus::Timeout);
    assert!(!udp.sent_to.is_empty(), "at least one query was attempted");
}

#[test]
fn an_invalid_name_is_rejected_before_any_query() {
    let sysinfo = SysinfoFake::new(alloc::vec![v4_record(10, 0, 2, 3)]);
    let mut udp = DnsFake::new(|_server, _q| Vec::new());
    let mut rng = counter_rng();
    // A label longer than 63 octets is invalid.
    let long_label = "a".repeat(64);
    let result = resolve_name(&long_label, RecordType::A, &sysinfo, &mut udp, &mut rng);
    assert!(matches!(result, Err(ResolveError::InvalidName(_))));
    assert!(
        udp.sent_to.is_empty(),
        "an invalid name never reaches the wire"
    );
}

#[test]
fn a_transport_send_error_aborts_fail_closed() {
    let sysinfo = SysinfoFake::new(alloc::vec![v4_record(10, 0, 2, 3)]);
    let mut udp = DnsFake::new(|_server, _q| Vec::new());
    udp.send_err = Some(Errno::NetworkUnreachable);
    let mut rng = counter_rng();
    let result = resolve_name("example.com", RecordType::A, &sysinfo, &mut udp, &mut rng);
    assert_eq!(
        result,
        Err(ResolveError::Transport(Errno::NetworkUnreachable))
    );
}

#[test]
fn a_server_source_failure_is_reported() {
    let sysinfo = SysinfoFake::denying();
    let mut udp = DnsFake::new(|_server, _q| Vec::new());
    let mut rng = counter_rng();
    let result = resolve_name("example.com", RecordType::A, &sysinfo, &mut udp, &mut rng);
    assert_eq!(
        result,
        Err(ResolveError::ServerSource(Errno::PermissionDenied))
    );
}
