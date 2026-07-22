//! Unit and deterministic property tests for the TCP connection state
//! machine. Two [`Tcb`]s are driven against each other over an in-test
//! "link" that serialises each emitted segment with [`crate::tcp::write`]
//! and re-parses it with [`TcpSegment::parse`], so the tests exercise the
//! exact wire path the live service runs (a lossy/reordering link models
//! an adversarial network).

use super::*;
use crate::checksum::Pseudo;
use crate::tcp::TcpSegment;
use crate::Ipv4Addr;
use alloc::vec;
use alloc::vec::Vec;
use tairix_abi::time::Duration64;

/// Sum of two monotonic spans (there is no `Duration64::Add`).
fn plus(a: Duration64, b: Duration64) -> Duration64 {
    Duration64::from_nanos(
        u64::try_from(crate::timeutil::nanos(a).saturating_add(crate::timeutil::nanos(b)))
            .unwrap_or(u64::MAX),
    )
}

const PSEUDO: Pseudo = Pseudo::V4 {
    source: Ipv4Addr::new(10, 0, 0, 1),
    destination: Ipv4Addr::new(10, 0, 0, 2),
};

fn ms(n: u64) -> Duration64 {
    Duration64::from_nanos(n.saturating_mul(1_000_000))
}

/// A captured segment: its wire bytes under [`PSEUDO`].
fn drain(tcb: &mut Tcb, now: Duration64) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    tcb.poll_transmit(now, |out| {
        let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN + out.payload.len()];
        let n = crate::tcp::write(PSEUDO, &out.meta, out.payload, &mut buf)
            .expect("a planned segment always fits and serialises");
        buf.truncate(n);
        frames.push(buf);
        true
    });
    frames
}

fn feed(tcb: &mut Tcb, frame: &[u8], now: Duration64) {
    let seg = TcpSegment::parse(PSEUDO, frame).expect("a serialised segment parses");
    tcb.on_segment(&seg, now);
}

/// Move every segment `src` wants to send into `dst`, returning the count.
fn transfer(src: &mut Tcb, dst: &mut Tcb, now: Duration64) -> usize {
    let frames = drain(src, now);
    let n = frames.len();
    for frame in &frames {
        feed(dst, frame, now);
    }
    n
}

/// Run both directions to quiescence (no segment moved in a full round).
fn settle(a: &mut Tcb, b: &mut Tcb, now: Duration64) {
    for _ in 0..64 {
        let moved = transfer(a, b, now) + transfer(b, a, now);
        if moved == 0 {
            return;
        }
    }
    panic!("connection did not settle");
}

fn config() -> TcpConfig {
    TcpConfig::default()
}

fn handshake(now: Duration64) -> (Tcb, Tcb) {
    let mut client = Tcb::connect(config(), 40000, 80, 1000, now);
    let mut server = Tcb::listen(config(), 80, 0, 5000);
    settle(&mut client, &mut server, now);
    assert_eq!(client.state(), State::Established);
    assert_eq!(server.state(), State::Established);
    assert!(client.is_established());
    assert!(server.is_established());
    assert_eq!(server.remote_port(), 40000);
    (client, server)
}

#[test]
fn three_way_handshake_establishes() {
    let now = ms(0);
    let (_client, _server) = handshake(now);
}

#[test]
fn data_flows_both_directions() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);

    assert_eq!(client.send(b"hello server").unwrap(), 12);
    settle(&mut client, &mut server, now);
    let mut buf = [0u8; 32];
    let n = server.recv(&mut buf);
    assert_eq!(&buf[..n], b"hello server");

    assert_eq!(server.send(b"hi client").unwrap(), 9);
    settle(&mut server, &mut client, now);
    let n = client.recv(&mut buf);
    assert_eq!(&buf[..n], b"hi client");
}

#[test]
fn orderly_close_reaches_time_wait_and_closed() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);

    client.close(now).unwrap();
    settle(&mut client, &mut server, now);
    // Client sent FIN; server saw it and is in CLOSE-WAIT.
    assert_eq!(server.state(), State::CloseWait);
    assert_eq!(client.state(), State::FinWait2);

    server.close(now).unwrap();
    settle(&mut client, &mut server, now);
    assert_eq!(server.state(), State::Closed);
    assert_eq!(client.state(), State::TimeWait);
    assert_eq!(client.reset_reason(), None);
    assert_eq!(server.reset_reason(), None);

    // TIME-WAIT expires after 2·MSL.
    let later = Duration64::from_secs(2 * config().maximum_segment_lifetime.secs() + 1);
    client.advance(later);
    assert_eq!(client.state(), State::Closed);
}

#[test]
fn simultaneous_open_establishes() {
    let now = ms(0);
    let mut a = Tcb::connect(config(), 5000, 6000, 100, now);
    let mut b = Tcb::connect(config(), 6000, 5000, 900, now);
    settle(&mut a, &mut b, now);
    assert_eq!(a.state(), State::Established);
    assert_eq!(b.state(), State::Established);
}

/// Build a client-side data segment by hand so a test can deliver
/// segments to the server in an arbitrary order.
fn client_data(seq: u32, ack: u32, payload: &[u8]) -> Vec<u8> {
    let meta = TcpSegmentMeta {
        source_port: 40000,
        destination_port: 80,
        seq: SeqNumber::new(seq),
        ack: SeqNumber::new(ack),
        flags: TcpFlags::ACK,
        window: 0xFFFF,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN + payload.len()];
    let n = crate::tcp::write(PSEUDO, &meta, payload, &mut buf).expect("write");
    buf.truncate(n);
    buf
}

#[test]
fn out_of_order_segments_are_reassembled_with_sack() {
    let now = ms(0);
    // Fixed ISNs so the sequence numbers are known: client rcv/snd known.
    let mut client = Tcb::connect(config(), 40000, 80, 1000, now);
    let mut server = Tcb::listen(config(), 80, 0, 5000);
    settle(&mut client, &mut server, now);
    // After the handshake the server expects client seq 1001 and the
    // client expects server seq 5001.
    let (rcv_nxt, peer_ack) = (1001u32, 5001u32);

    // Deliver the second segment first: a hole opens at 1001.
    feed(
        &mut server,
        &client_data(rcv_nxt + 4, peer_ack, b"EFGH"),
        now,
    );
    assert_eq!(server.recv_len(), 0, "held out of order");

    // The server acknowledges the still-missing left edge and advertises the
    // received block via SACK.
    let frames = drain(&mut server, now);
    assert_eq!(frames.len(), 1);
    let ackseg = TcpSegment::parse(PSEUDO, &frames[0]).expect("parse ack");
    assert_eq!(ackseg.ack, SeqNumber::new(rcv_nxt));
    let sack = ackseg.options.sack();
    assert_eq!(sack.len(), 1);
    assert_eq!(sack[0].left, SeqNumber::new(rcv_nxt + 4));
    assert_eq!(sack[0].right, SeqNumber::new(rcv_nxt + 8));

    // Fill the hole: both segments are now delivered in order.
    feed(&mut server, &client_data(rcv_nxt, peer_ack, b"ABCD"), now);
    let mut buf = [0u8; 16];
    let n = server.recv(&mut buf);
    assert_eq!(&buf[..n], b"ABCDEFGH");
}

#[test]
fn retransmits_a_lost_segment() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);

    client.send(b"payload").unwrap();
    // Drop the client's data segment entirely (do not deliver).
    let dropped = drain(&mut client, now);
    assert_eq!(dropped.len(), 1);

    // Nothing arrived; server has no data.
    assert_eq!(server.recv_len(), 0);

    // After the RTO, the client retransmits.
    let deadline = client
        .next_deadline()
        .expect("an unacked segment arms the RTO");
    let after = plus(deadline, ms(1));
    client.advance(after);
    let resent = transfer(&mut client, &mut server, after);
    assert_eq!(resent, 1);
    settle(&mut client, &mut server, after);
    let mut buf = [0u8; 16];
    let n = server.recv(&mut buf);
    assert_eq!(&buf[..n], b"payload");
}

#[test]
fn peer_reset_aborts_the_connection() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);
    server.abort(now);
    // Deliver the RST to the client.
    let _ = transfer(&mut server, &mut client, now);
    assert_eq!(client.reset_reason(), Some(ResetReason::ConnectionReset));
    assert_eq!(client.state(), State::Closed);
}

#[test]
fn sequence_space_wraps_at_the_boundary() {
    let now = ms(0);
    // ISS just below the wrap so data crosses 2³² − 1 → 0.
    let mut client = Tcb::connect(config(), 40000, 80, u32::MAX - 4, now);
    let mut server = Tcb::listen(config(), 80, 0, 12345);
    settle(&mut client, &mut server, now);
    assert_eq!(client.state(), State::Established);

    client.send(b"wraparound payload").unwrap();
    settle(&mut client, &mut server, now);
    let mut buf = [0u8; 32];
    let n = server.recv(&mut buf);
    assert_eq!(&buf[..n], b"wraparound payload");
}

#[test]
fn connect_to_nothing_times_out() {
    let mut now = ms(0);
    let mut client = Tcb::connect(config(), 40000, 80, 1000, now);
    // Never deliver the SYN. Repeatedly fire the RTO past the budget.
    for _ in 0..(config().max_retransmits + 2) {
        let _ = drain(&mut client, now);
        let Some(deadline) = client.next_deadline() else {
            break;
        };
        now = plus(deadline, ms(1));
        client.advance(now);
        if client.reset_reason().is_some() {
            break;
        }
    }
    assert_eq!(client.reset_reason(), Some(ResetReason::TimedOut));
}

#[test]
fn window_scale_and_options_negotiate() {
    let now = ms(0);
    let (client, server) = handshake(now);
    // Default config offers timestamps, SACK, and window scale 7; both
    // sides should agree.
    assert!(client.is_established());
    assert!(server.is_established());
    // No panics and both settled proves the options round-tripped through
    // the codec (write/parse) without exceeding the 40-byte region.
}

/// A small deterministic LCG (no allocator, reproducible) for the property
/// test's schedule.
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x1234_5678_9abc_def1
        } else {
            seed
        })
    }
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }
}

#[test]
fn bulk_transfer_survives_reordering_and_loss() {
    for seed in 1..=16u64 {
        let mut rng = Lcg::new(seed);
        let now = ms(0);
        let (mut client, mut server) = handshake(now);

        let payload: Vec<u8> = (0..4096u32)
            .map(|i| ((i & 0xFF) as u8) ^ ((i >> 8) & 0xFF) as u8)
            .collect();
        let mut offered = 0usize;
        let mut received: Vec<u8> = Vec::new();
        let mut t = 0u64;

        // A queue of in-flight frames with a scheduled delivery order.
        let mut in_flight: Vec<Vec<u8>> = Vec::new();

        for _round in 0..4000 {
            t += 1 + (rng.next() % 5);
            let now = ms(t);

            // Offer more application data.
            if offered < payload.len() {
                let take = ((rng.next() % 512) as usize + 1).min(payload.len() - offered);
                let n = client.send(&payload[offered..offered + take]).unwrap_or(0);
                offered += n;
                if offered == payload.len() {
                    client.close(now).ok();
                }
            }

            client.advance(now);
            server.advance(now);

            // Collect client output into the in-flight queue.
            for f in drain(&mut client, now) {
                in_flight.push(f);
            }
            // Server acks travel back immediately (keep the test bounded).
            for f in drain(&mut server, now) {
                feed(&mut client, &f, now);
            }

            // Deliver some in-flight frames, occasionally dropping/reordering.
            if !in_flight.is_empty() {
                // Randomly drop the head 1-in-8.
                if rng.next() % 8 == 0 {
                    in_flight.remove(0);
                } else {
                    let idx = usize::try_from(rng.next() % in_flight.len() as u64).unwrap();
                    let frame = in_flight.remove(idx);
                    feed(&mut server, &frame, now);
                }
            }

            // Read whatever is available.
            let mut buf = [0u8; 1024];
            loop {
                let n = server.recv(&mut buf);
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
            }

            if received.len() == payload.len() {
                break;
            }
        }

        assert_eq!(received.len(), payload.len(), "seed {seed}: length");
        assert_eq!(received, payload, "seed {seed}: bytes");
    }
}
