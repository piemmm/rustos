//! Unit and adversarial tests for the demultiplexing TCP listener and the
//! stateless SYN-cookie defence. A live client [`Tcb`] is driven against a
//! [`Listener`] over an in-test "link" that serialises each emitted segment
//! with [`crate::tcp::write`] and re-parses it with [`TcpSegment::parse`], so
//! the tests exercise the exact wire path the live service runs.

use super::*;
use crate::checksum::Pseudo;
use crate::tcp::conn::Tcb;
use crate::tcp::TcpSegment;
use crate::{Ipv4Addr, Ipv6Addr};
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::time::Duration64;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
const LOCAL_PORT: u16 = 80;

/// A deterministic keyed MAC standing in for the `lib/crypto`-backed secret
/// the live service injects: a simple mixing of the tuple bytes, the
/// counter, and a fixed key. Adequate for exercising the engine's cookie
/// logic; the real secret is a cryptographic MAC.
struct TestSecret {
    key: u64,
}

impl CookieSecret for TestSecret {
    fn mac(&self, tuple: &[u8], counter: u32) -> u32 {
        let mut h = self.key ^ (u64::from(counter).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for &b in tuple {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        }
        (((h >> 32) ^ h) & 0xFFFF_FFFF) as u32
    }
}

fn secret() -> TestSecret {
    TestSecret {
        key: 0xDEAD_BEEF_CAFE_F00D,
    }
}

fn peer(a: u8, b: u8, c: u8, d: u8, port: u16) -> Peer {
    Peer {
        addr: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        port,
    }
}

fn pseudo(peer: Peer) -> Pseudo {
    match (LOCAL, peer.addr) {
        (IpAddr::V4(s), IpAddr::V4(d)) => Pseudo::V4 {
            source: d,
            destination: s,
        },
        (IpAddr::V6(s), IpAddr::V6(d)) => Pseudo::V6 {
            source: d,
            destination: s,
        },
        _ => panic!("mixed families"),
    }
}

fn ms(n: u64) -> Duration64 {
    Duration64::from_nanos(n.saturating_mul(1_000_000))
}

/// Serialise every segment the client wants to send, under its pseudo-header.
fn drain(tcb: &mut Tcb, peer: Peer, now: Duration64) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    tcb.poll_transmit(now, |out| {
        frames.push(serialise(peer, &out));
        true
    });
    frames
}

fn serialise(peer: Peer, out: &OutSegment<'_>) -> Vec<u8> {
    let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN + out.payload.len()];
    let n = crate::tcp::write(pseudo(peer), &out.meta, out.payload, &mut buf)
        .expect("a planned segment always fits");
    buf.truncate(n);
    buf
}

/// Feed a wire frame (as if arriving from `peer`) to the listener, returning
/// the frames the listener emitted back to that peer.
fn to_listener(
    listener: &mut Listener,
    peer: Peer,
    frame: &[u8],
    now: Duration64,
    secret: &dyn CookieSecret,
) -> Vec<Vec<u8>> {
    let seg = TcpSegment::parse(pseudo(peer), frame).expect("frame parses");
    let mut out = Vec::new();
    listener.on_segment(LOCAL, peer, &seg, now, secret, |p, seg| {
        assert_eq!(p, peer);
        out.push(serialise(peer, &seg));
        true
    });
    out
}

fn feed_client(tcb: &mut Tcb, peer: Peer, frame: &[u8], now: Duration64) {
    let seg = TcpSegment::parse(pseudo(peer), frame).expect("frame parses");
    tcb.on_segment(&seg, crate::addr::Ecn::NotEct, now);
}

fn listen_config(max_half_open: usize, max_accept: usize) -> ListenConfig {
    ListenConfig {
        max_half_open,
        max_accept,
        half_open_timeout: Duration64::from_secs(10),
        template: TcpConfig::default(),
    }
}

/// Run a full three-way handshake between a fresh client [`Tcb`] and the
/// listener, returning the [`Connection`] the listener accepted.
fn establish(
    listener: &mut Listener,
    peer: Peer,
    now: Duration64,
    secret: &dyn CookieSecret,
) -> Connection {
    let mut client = Tcb::connect(TcpConfig::default(), peer.port, LOCAL_PORT, 0x1000, now);
    for _ in 0..8 {
        let mut moved = 0;
        for frame in drain(&mut client, peer, now) {
            moved += 1;
            for reply in to_listener(listener, peer, &frame, now, secret) {
                feed_client(&mut client, peer, &reply, now);
                moved += 1;
            }
        }
        if moved == 0 {
            break;
        }
        if listener.pending() > 0 {
            break;
        }
    }
    listener.accept().expect("handshake accepted")
}

#[test]
fn handshake_completes_and_accepts() {
    let now = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(64, 64));
    let p = peer(10, 0, 0, 2, 40000);
    let conn = establish(&mut listener, p, now, &secret);
    assert_eq!(conn.peer, p);
    assert!(conn.tcb.is_established());
    assert_eq!(conn.tcb.remote_port(), 40000);
    // A full-state handshake keeps no residual half-open state.
    assert_eq!(listener.half_open_len(), 0);
    assert_eq!(listener.stats().accepted, 1);
    assert_eq!(listener.stats().cookies_sent, 0);
}

#[test]
fn accepted_connection_is_a_usable_established_tcb() {
    let now = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(64, 64));
    let p = peer(10, 0, 0, 2, 40001);
    let mut server = establish(&mut listener, p, now, &secret).tcb;
    // The caller owns the accepted TCB: it is established and can enqueue and
    // plan outbound data like any other connection.
    assert!(server.send(b"hello").is_ok());
    let frames = drain(&mut server, p, now);
    assert!(!frames.is_empty(), "the accepted TCB plans a data segment");
}

#[test]
fn syn_flood_falls_back_to_cookies_with_bounded_memory() {
    let now = ms(0);
    let secret = secret();
    let max_half_open = 16;
    let mut listener = Listener::new(LOCAL_PORT, listen_config(max_half_open, 64));

    // A flood of spoofed SYNs from distinct sources: the first `max_half_open`
    // allocate half-open state, every later one is answered with a stateless
    // cookie and allocates nothing.
    for i in 0..1000u32 {
        let p = peer(198, 51, ((i >> 8) & 0xff) as u8, (i & 0xff) as u8, 5000);
        let mut client = Tcb::connect(TcpConfig::default(), p.port, LOCAL_PORT, 0x2000 + i, now);
        let syn = drain(&mut client, p, now).remove(0);
        let replies = to_listener(&mut listener, p, &syn, now, &secret);
        assert_eq!(replies.len(), 1, "every SYN earns exactly one SYN-ACK");
        // Half-open state never exceeds the configured bound.
        assert!(listener.half_open_len() <= max_half_open);
    }
    assert_eq!(listener.half_open_len(), max_half_open);
    assert_eq!(listener.stats().half_open_started, max_half_open as u64);
    assert!(listener.stats().cookies_sent >= 900);
    // The flood never completed a handshake, so nothing is queued to accept.
    assert_eq!(listener.pending(), 0);
    assert!(listener.accept().is_none());
}

#[test]
fn syn_cookie_round_trip_reconstructs_connection() {
    let now = ms(0);
    let secret = secret();
    // A zero-length backlog forces every SYN onto the cookie path.
    let mut listener = Listener::new(LOCAL_PORT, listen_config(0, 64));
    let p = peer(203, 0, 113, 7, 50000);

    let mut client = Tcb::connect(TcpConfig::default(), p.port, LOCAL_PORT, 0x3000, now);
    // Client SYN -> cookie SYN-ACK -> client ACK.
    let syn = drain(&mut client, p, now).remove(0);
    let synack = to_listener(&mut listener, p, &syn, now, &secret);
    assert_eq!(synack.len(), 1);
    assert_eq!(listener.half_open_len(), 0, "cookie path holds no state");
    assert_eq!(listener.stats().cookies_sent, 1);
    for frame in &synack {
        feed_client(&mut client, p, frame, now);
    }
    let ack = drain(&mut client, p, now).remove(0);
    let replies = to_listener(&mut listener, p, &ack, now, &secret);
    assert!(replies.is_empty(), "a valid cookie is accepted silently");
    let conn = listener.accept().expect("cookie handshake accepted");
    assert!(conn.tcb.is_established());
    assert_eq!(conn.tcb.remote_port(), p.port);
    assert_eq!(listener.stats().cookies_accepted, 1);
    assert!(client.is_established());
}

#[test]
fn tampered_cookie_ack_is_refused_with_rst() {
    let now = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(0, 64));
    let p = peer(203, 0, 113, 8, 50001);

    // A bare ACK that never followed a cookie SYN-ACK: its acknowledgement
    // number is not a valid cookie, so the listener refuses it with a RST and
    // reconstructs nothing.
    let mut client = Tcb::connect(TcpConfig::default(), p.port, LOCAL_PORT, 0x4000, now);
    let syn = drain(&mut client, p, now).remove(0);
    let synack = to_listener(&mut listener, p, &syn, now, &secret);
    for frame in &synack {
        feed_client(&mut client, p, frame, now);
    }
    let ack = drain(&mut client, p, now).remove(0);
    // Corrupt the acknowledgement field so the cookie no longer validates.
    let mut forged = TcpSegment::parse(pseudo(p), &ack).expect("parse");
    forged.ack = forged.ack.add(0x0055_0000);
    let meta = TcpSegmentMeta {
        source_port: forged.source_port,
        destination_port: forged.destination_port,
        seq: forged.seq,
        ack: forged.ack,
        flags: forged.flags,
        window: forged.window,
        urgent: forged.urgent,
        options: forged.options,
    };
    let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN];
    let n = crate::tcp::write(pseudo(p), &meta, &[], &mut buf).expect("write");
    buf.truncate(n);
    let replies = to_listener(&mut listener, p, &buf, now, &secret);
    assert_eq!(replies.len(), 1, "a bad cookie earns a RST");
    let rst = TcpSegment::parse(pseudo(p), &replies[0]).expect("parse");
    assert!(rst.flags.rst());
    assert_eq!(listener.pending(), 0);
    assert_eq!(listener.stats().cookies_rejected, 1);
}

#[test]
fn stale_cookie_from_an_expired_counter_is_rejected() {
    let early = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(0, 64));
    let p = peer(203, 0, 113, 9, 50002);

    let mut client = Tcb::connect(TcpConfig::default(), p.port, LOCAL_PORT, 0x5000, early);
    let syn = drain(&mut client, p, early).remove(0);
    let synack = to_listener(&mut listener, p, &syn, early, &secret);
    for frame in &synack {
        feed_client(&mut client, p, frame, early);
    }
    let ack = drain(&mut client, p, early).remove(0);
    // Return the ACK far in the future — several counter ticks later — so the
    // cookie has expired and no longer validates.
    let late = Duration64::from_secs(600);
    let replies = to_listener(&mut listener, p, &ack, late, &secret);
    assert_eq!(replies.len(), 1);
    assert!(TcpSegment::parse(pseudo(p), &replies[0])
        .unwrap()
        .flags
        .rst());
    assert_eq!(listener.stats().cookies_accepted, 0);
    assert_eq!(listener.stats().cookies_rejected, 1);
}

#[test]
fn accept_queue_exhaustion_fails_closed() {
    let now = ms(0);
    let secret = secret();
    // Room for two accepted connections, generous half-open backlog. Drive
    // three fresh handshakes WITHOUT accepting; the third must be refused
    // once the accept queue is full.
    let mut listener = Listener::new(LOCAL_PORT, listen_config(64, 2));
    let mut refused = 0;
    for i in 0..3u8 {
        let p = peer(192, 0, 2, 20 + i, 40200 + u16::from(i));
        let mut client = Tcb::connect(
            TcpConfig::default(),
            p.port,
            LOCAL_PORT,
            0x6000 + u32::from(i),
            now,
        );
        for _ in 0..8 {
            let mut moved = 0;
            for frame in drain(&mut client, p, now) {
                moved += 1;
                for reply in to_listener(&mut listener, p, &frame, now, &secret) {
                    feed_client(&mut client, p, &reply, now);
                    moved += 1;
                    if TcpSegment::parse(pseudo(p), &reply).unwrap().flags.rst() {
                        refused += 1;
                    }
                }
            }
            if moved == 0 {
                break;
            }
        }
    }
    assert_eq!(listener.pending(), 2, "accept queue is bounded");
    assert_eq!(listener.stats().accept_overflow, 1);
    assert!(refused >= 1, "the excess handshake was refused with a RST");
}

#[test]
fn half_open_expires_and_frees_its_slot() {
    let now = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(64, 64));
    let p = peer(198, 51, 100, 5, 40300);

    let mut client = Tcb::connect(TcpConfig::default(), p.port, LOCAL_PORT, 0x7000, now);
    let syn = drain(&mut client, p, now).remove(0);
    let _ = to_listener(&mut listener, p, &syn, now, &secret);
    assert_eq!(listener.half_open_len(), 1);

    // The client never completes; past the half-open timeout the slot frees.
    let later = Duration64::from_secs(30);
    listener.advance(later, |_, _| true);
    assert_eq!(listener.half_open_len(), 0);
    assert_eq!(listener.stats().half_open_expired, 1);
}

#[test]
fn peer_rst_drops_half_open() {
    let now = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(64, 64));
    let p = peer(198, 51, 100, 6, 40400);

    let mut client = Tcb::connect(TcpConfig::default(), p.port, LOCAL_PORT, 0x8000, now);
    let syn = drain(&mut client, p, now).remove(0);
    let synack = to_listener(&mut listener, p, &syn, now, &secret);
    assert_eq!(listener.half_open_len(), 1);
    // The client resets instead of completing.
    for frame in &synack {
        feed_client(&mut client, p, frame, now);
    }
    client.abort(now);
    let rst = drain(&mut client, p, now).remove(0);
    let _ = to_listener(&mut listener, p, &rst, now, &secret);
    assert_eq!(
        listener.half_open_len(),
        0,
        "a RST reaps the half-open slot"
    );
    assert_eq!(listener.pending(), 0);
}

#[test]
fn data_only_segment_to_listener_is_dropped() {
    let now = ms(0);
    let secret = secret();
    let mut listener = Listener::new(LOCAL_PORT, listen_config(64, 64));
    let p = peer(198, 51, 100, 7, 40500);
    // A pure data segment (no SYN, no ACK) to a listening port with no state
    // is neither answered nor recorded (RFC 9293 §3.10.7.2).
    let meta = TcpSegmentMeta {
        source_port: p.port,
        destination_port: LOCAL_PORT,
        seq: SeqNumber::new(1234),
        ack: SeqNumber::new(0),
        flags: TcpFlags::PSH,
        window: 1024,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN + 4];
    let n = crate::tcp::write(pseudo(p), &meta, b"junk", &mut buf).expect("write");
    buf.truncate(n);
    let replies = to_listener(&mut listener, p, &buf, now, &secret);
    assert!(replies.is_empty());
    assert_eq!(listener.half_open_len(), 0);
    assert_eq!(listener.pending(), 0);
}

#[test]
fn ipv6_handshake_completes() {
    // The listener is family-agnostic: repeat the base handshake over IPv6.
    let now = ms(0);
    let secret = secret();
    let local6 = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
    let peer6 = Peer {
        addr: IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2)),
        port: 41000,
    };
    let ps = Pseudo::V6 {
        source: match peer6.addr {
            IpAddr::V6(a) => a,
            IpAddr::V4(_) => unreachable!(),
        },
        destination: match local6 {
            IpAddr::V6(a) => a,
            IpAddr::V4(_) => unreachable!(),
        },
    };
    let mut listener = Listener::new(LOCAL_PORT, listen_config(0, 64));
    let mut client = Tcb::connect(TcpConfig::default(), peer6.port, LOCAL_PORT, 0x9000, now);

    let ser = |out: &OutSegment<'_>| {
        let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN + out.payload.len()];
        let n = crate::tcp::write(ps, &out.meta, out.payload, &mut buf).expect("write");
        buf.truncate(n);
        buf
    };
    // client SYN
    let mut syn = Vec::new();
    client.poll_transmit(now, |o| {
        syn = ser(&o);
        true
    });
    let seg = TcpSegment::parse(ps, &syn).unwrap();
    let mut synack = Vec::new();
    listener.on_segment(local6, peer6, &seg, now, &secret, |_, o| {
        synack = ser(&o);
        true
    });
    client.on_segment(
        &TcpSegment::parse(ps, &synack).unwrap(),
        crate::addr::Ecn::NotEct,
        now,
    );
    let mut ack = Vec::new();
    client.poll_transmit(now, |o| {
        ack = ser(&o);
        true
    });
    let seg = TcpSegment::parse(ps, &ack).unwrap();
    listener.on_segment(local6, peer6, &seg, now, &secret, |_, _| true);
    let conn = listener.accept().expect("v6 cookie handshake accepted");
    assert!(conn.tcb.is_established());
}
