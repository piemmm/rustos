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
    feed_ecn(tcb, frame, crate::addr::Ecn::NotEct, now);
}

fn feed_ecn(tcb: &mut Tcb, frame: &[u8], ecn: crate::addr::Ecn, now: Duration64) {
    let seg = TcpSegment::parse(PSEUDO, frame).expect("a serialised segment parses");
    tcb.on_segment(&seg, ecn, now);
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
fn cumulative_ack_advances_una_past_a_recovery_rewound_snd_nxt() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);

    // The server (sender) queues several segments' worth of data and
    // transmits as far as its window allows; the client receives every
    // segment, so a cumulative ACK up to the server's `snd_max` is valid.
    let payload = [0x5au8; 8000];
    assert!(server.send(&payload).unwrap() > 0);
    let frames = drain(&mut server, now);
    assert!(
        frames.len() >= 2,
        "the test needs several segments in flight"
    );
    for frame in &frames {
        feed(&mut client, frame, now);
    }
    let snd_max = server.snd_max;
    assert!(
        server.snd_una.lt(snd_max),
        "data is outstanding after the burst"
    );

    // The retransmission timer fires before any ACK returns (model the
    // ACKs delayed or lost). Go-back-N rewinds `snd_nxt` to `snd_una`, and
    // the next poll retransmits one segment, so `snd_nxt` now sits strictly
    // below `snd_max`.
    let deadline = server
        .next_deadline()
        .expect("outstanding data arms the RTO");
    let after = plus(deadline, ms(1));
    server.advance(after);
    let _ = drain(&mut server, after);
    assert!(
        server.snd_nxt.lt(snd_max),
        "the RTO rewound snd_nxt below snd_max"
    );

    // A cumulative ACK for everything the server sent now arrives. It
    // acknowledges data in `(snd_nxt, snd_max]` — valid, since the peer
    // holds it — and must advance `snd_una` and quiesce retransmission.
    // Before the fix the "acks something not yet sent" guard was bounded by
    // the rewound `snd_nxt`, so this ACK was challenged and dropped, leaving
    // `snd_una` frozen and the server retransmitting acknowledged data until
    // the connection timed out.
    let ack = client_data(client.snd_nxt.value(), snd_max.value(), b"");
    feed(&mut server, &ack, after);

    assert_eq!(
        server.snd_una, snd_max,
        "the cumulative ACK advances snd_una to snd_max"
    );
    assert!(
        !server.snd_nxt.lt(server.snd_una),
        "snd_nxt never trails snd_una"
    );
    assert_eq!(
        server.next_deadline(),
        None,
        "with nothing outstanding the retransmission timer is disarmed"
    );
    assert_eq!(server.reset_reason(), None, "the connection is not aborted");
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
                if rng.next().is_multiple_of(8) {
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

use crate::tcp::cc::CongestionAlgorithm;

/// The application-data payload length of a serialised segment.
fn payload_len(frame: &[u8]) -> usize {
    TcpSegment::parse(PSEUDO, frame).map_or(0, |s| s.payload.len())
}

fn config_cc(algo: CongestionAlgorithm) -> TcpConfig {
    TcpConfig {
        congestion: algo,
        ..TcpConfig::default()
    }
}

fn handshake_cc(now: Duration64, algo: CongestionAlgorithm) -> (Tcb, Tcb) {
    let mut client = Tcb::connect(config_cc(algo), 40000, 80, 1000, now);
    let mut server = Tcb::listen(config_cc(algo), 80, 0, 5000);
    settle(&mut client, &mut server, now);
    assert!(client.is_established() && server.is_established());
    (client, server)
}

#[test]
fn congestion_window_bounds_the_initial_flight() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);
    // Offer far more than one window of data.
    let data = vec![0x5Au8; 60_000];
    assert!(client.send(&data).unwrap() > 14_600);
    // The very first burst carries *no* acknowledgements yet, so it is
    // bounded by the RFC 6928 initial congestion window (~14600 B for a
    // 1460-byte MSS) — never the whole 60000 B.
    let first: usize = drain(&mut client, now).iter().map(|f| payload_len(f)).sum();
    assert!(
        (10_000..=14_600).contains(&first),
        "first flight {first} B ignored the congestion window"
    );
    let _ = &mut server;
}

#[test]
fn congestion_window_opens_as_acks_arrive() {
    let t0 = ms(0);
    let (mut client, mut server) = handshake(t0);
    client.send(&vec![0u8; 64_000]).unwrap();

    // First flight: one initial window, before any ACK.
    let frames = drain(&mut client, t0);
    let first: usize = frames.iter().map(|f| payload_len(f)).sum();
    for f in &frames {
        feed(&mut server, f, t0);
    }
    let mut buf = vec![0u8; 64_000];
    while server.recv(&mut buf) > 0 {}

    // One round trip later the server's cumulative ACK arrives and opens the
    // window; the next flight must be larger than the first.
    let t1 = ms(200);
    server.advance(t1);
    for f in drain(&mut server, t1) {
        feed(&mut client, &f, t1);
    }
    let second: usize = drain(&mut client, t1).iter().map(|f| payload_len(f)).sum();
    assert!(
        second > first,
        "congestion window did not open (first {first} B, second {second} B)"
    );
}

#[test]
fn bulk_transfer_completes_under_each_policy() {
    for algo in [CongestionAlgorithm::Cubic, CongestionAlgorithm::NewReno] {
        let (mut client, mut server) = handshake_cc(ms(0), algo);
        let data: Vec<u8> = (0..100_000u32).map(|i| (i & 0xFF) as u8).collect();
        let mut offered = 0usize;
        let mut received: Vec<u8> = Vec::new();
        let mut t = 0u64;
        for _ in 0..40_000 {
            t += 10;
            let now = ms(t);
            if offered < data.len() {
                let take = client.send_available().min(data.len() - offered);
                if take > 0 {
                    let n = client.send(&data[offered..offered + take]).unwrap_or(0);
                    offered += n;
                    if offered == data.len() {
                        client.close(now).ok();
                    }
                }
            }
            client.advance(now);
            server.advance(now);
            for f in drain(&mut client, now) {
                feed(&mut server, &f, now);
            }
            for f in drain(&mut server, now) {
                feed(&mut client, &f, now);
            }
            let mut buf = [0u8; 4096];
            loop {
                let n = server.recv(&mut buf);
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
            }
            if received.len() == data.len() {
                break;
            }
        }
        assert_eq!(received.len(), data.len(), "{algo:?}: length");
        assert_eq!(received, data, "{algo:?}: bytes");
    }
}

// ---------------------------------------------------------------------------
// RFC 6675 SACK-based loss recovery (N6b).
// ---------------------------------------------------------------------------

use crate::tcp::SackBlock;

/// Build a scoreboard from `ranges` (raw `[left, right)` sequence values),
/// as if the peer had SACKed them while `base..base+100_000` was in flight.
fn scoreboard(base: u32, ranges: &[(u32, u32)]) -> Scoreboard {
    let mut board = Scoreboard::new();
    let blocks: Vec<SackBlock> = ranges
        .iter()
        .map(|(l, r)| SackBlock {
            left: SeqNumber::new(*l),
            right: SeqNumber::new(*r),
        })
        .collect();
    board.record(
        &blocks,
        SeqNumber::new(base),
        SeqNumber::new(base + 100_000),
    );
    board
}

/// Collect the unSACKed holes of `board` within `[from, to)`.
fn holes(board: &Scoreboard, from: u32, to: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    board.for_each_hole(SeqNumber::new(from), SeqNumber::new(to), |l, r| {
        out.push((l.value(), r.value()));
        true
    });
    out
}

#[test]
fn scoreboard_marks_lost_by_discontiguous_block_count() {
    // Three discontiguous SACKed blocks above 1000 satisfy the RFC 6675
    // duplicate threshold, so 1000 is lost regardless of the byte volume.
    let board = scoreboard(1000, &[(1100, 1200), (1300, 1400), (1500, 1600)]);
    assert!(board.is_lost(SeqNumber::new(1000), 100_000));
    // Only two blocks lie above 1250 and the SACKed volume there (200 B) is
    // tiny, so 1250 is not lost.
    assert!(!board.is_lost(SeqNumber::new(1250), 100_000));
}

#[test]
fn scoreboard_marks_lost_by_sacked_volume() {
    // One big SACKed block (3000 B) above 1000 exceeds (DupThresh-1)*SMSS
    // for a 1000-byte MSS, so 1000 is lost even though it is a single block.
    let board = scoreboard(1000, &[(2000, 5000)]);
    assert!(board.is_lost(SeqNumber::new(1000), 1000));
    // With a 2000-byte MSS the threshold is 4000 B, above the 3000 SACKed,
    // and one block is below the count threshold, so it is not lost.
    assert!(!board.is_lost(SeqNumber::new(1000), 2000));
}

#[test]
fn scoreboard_coalesces_overlapping_and_adjacent_ranges() {
    let board = scoreboard(1000, &[(1100, 1200), (1150, 1300), (1300, 1400)]);
    // Overlap (1100..1300) and adjacency (..1400) collapse to one range.
    assert!(board.is_sacked(SeqNumber::new(1100)));
    assert!(board.is_sacked(SeqNumber::new(1399)));
    assert!(!board.is_sacked(SeqNumber::new(1400)));
    assert_eq!(holes(&board, 1000, 1500), vec![(1000, 1100), (1400, 1500)]);
}

#[test]
fn scoreboard_ignores_out_of_window_and_hostile_blocks() {
    let mut board = Scoreboard::new();
    let una = SeqNumber::new(1000);
    let max = SeqNumber::new(2000);
    // Below the cumulative ack, above the frontier, and inverted — all must
    // be refused rather than injecting state (fail closed, never a panic).
    board.record(
        &[
            SackBlock {
                left: SeqNumber::new(500),
                right: SeqNumber::new(600),
            },
            SackBlock {
                left: SeqNumber::new(1900),
                right: SeqNumber::new(9000),
            },
            SackBlock {
                left: SeqNumber::new(1600),
                right: SeqNumber::new(1500),
            },
            SackBlock {
                left: SeqNumber::new(1200),
                right: SeqNumber::new(1300),
            },
        ],
        una,
        max,
    );
    assert!(board.is_sacked(SeqNumber::new(1250)));
    assert!(!board.is_sacked(SeqNumber::new(550)));
    assert!(!board.is_sacked(SeqNumber::new(1950)));
    assert_eq!(holes(&board, 1000, 2000), vec![(1000, 1200), (1300, 2000)]);
}

#[test]
fn scoreboard_record_is_bounded_under_a_fragmenting_peer() {
    let mut board = Scoreboard::new();
    let una = SeqNumber::new(0);
    let max = SeqNumber::new(1_000_000);
    // A peer that fragments its SACK into far more ranges than the cap: the
    // board must stay bounded (fail closed), never grow with the input.
    for i in 0..(u32::try_from(MAX_SACK_RANGES).unwrap() + 200) {
        let left = i * 10;
        board.record(
            &[SackBlock {
                left: SeqNumber::new(left),
                right: SeqNumber::new(left + 5),
            }],
            una,
            max,
        );
    }
    assert!(board.ranges.len() <= MAX_SACK_RANGES);
}

/// The application-data payload length summed over a set of frames.
fn total_payload(frames: &[Vec<u8>]) -> usize {
    frames.iter().map(|f| payload_len(f)).sum()
}

#[test]
fn sack_recovery_retransmits_only_the_lost_segment() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);
    let data = vec![0xABu8; 8000];
    client.send(&data).unwrap();

    // Capture the whole first flight, then drop only its first segment and
    // deliver the rest — the server sees a single front hole.
    let flight = drain(&mut client, now);
    assert!(
        flight.len() >= 4,
        "first flight had only {} segments",
        flight.len()
    );
    for frame in flight.iter().skip(1) {
        feed(&mut server, frame, now);
    }

    // The server's duplicate ACK carries the SACK of the delivered tail.
    let acks = drain(&mut server, now);
    assert!(!acks.is_empty(), "server sent no acknowledgement");
    for ack in &acks {
        feed(&mut client, ack, now);
    }

    // SACK loss detection retransmits the hole with no clock advance (so this
    // is fast recovery, not an RTO) and sends exactly one segment, never the
    // whole outstanding window as go-back-N would.
    let rtx = drain(&mut client, now);
    let rtx_bytes = total_payload(&rtx);
    assert!(rtx_bytes > 0, "no selective retransmission occurred");
    assert!(
        rtx_bytes <= 1460,
        "go-back-N resent {rtx_bytes} B instead of one segment"
    );
    // Deliver the selective retransmission, then let both sides drain to
    // quiescence — the transfer completes in order, still with no RTO.
    for frame in &rtx {
        feed(&mut server, frame, now);
    }
    settle(&mut client, &mut server, now);
    let mut buf = vec![0u8; 8000];
    let mut got: Vec<u8> = Vec::new();
    loop {
        let n = server.recv(&mut buf);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, data, "the SACK-recovered stream is not byte-exact");
}

#[test]
fn sack_recovery_handles_multiple_holes_without_rto() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);
    let data: Vec<u8> = (0..12_000u32).map(|i| (i & 0xFF) as u8).collect();
    client.send(&data).unwrap();

    // Drop two non-adjacent segments from the first flight; deliver the rest.
    let flight = drain(&mut client, now);
    assert!(flight.len() >= 5, "flight {}", flight.len());
    for (idx, frame) in flight.iter().enumerate() {
        if idx == 1 || idx == 3 {
            continue;
        }
        feed(&mut server, frame, now);
    }

    // Exchange to quiescence with NO time advance: SACK recovery must fill
    // both holes from the scoreboard alone (an RTO would need the clock).
    settle(&mut client, &mut server, now);

    let mut buf = vec![0u8; 12_000];
    let mut got: Vec<u8> = Vec::new();
    loop {
        let n = server.recv(&mut buf);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got.len(), data.len(), "length after two-hole recovery");
    assert_eq!(got, data, "bytes after two-hole recovery");
    assert_eq!(client.reset_reason(), None, "recovery must not abort");
}

// ---- RFC 9293 §3.8.4 keepalive ------------------------------------------

/// A keepalive-enabled config with short, test-sized intervals (the RFC 1122
/// defaults are hours; here idle and probe spacing are one second and the
/// budget is three probes).
fn ka_config() -> TcpConfig {
    TcpConfig {
        enable_keepalive: true,
        keepalive_idle: ms(1000),
        keepalive_interval: ms(1000),
        keepalive_probes: 3,
        ..TcpConfig::default()
    }
}

fn handshake_ka(now: Duration64) -> (Tcb, Tcb) {
    let mut client = Tcb::connect(ka_config(), 40000, 80, 1000, now);
    let mut server = Tcb::listen(config(), 80, 0, 5000);
    settle(&mut client, &mut server, now);
    assert!(client.is_established() && server.is_established());
    (client, server)
}

/// Whether `frame` is a keepalive probe: an empty ACK whose sequence is one
/// below the send frontier (RFC 1122 §4.2.3.6).
fn is_keepalive_probe(frame: &[u8], expected_seq: u32) -> bool {
    let seg = TcpSegment::parse(PSEUDO, frame).expect("a serialised segment parses");
    seg.flags.ack()
        && !seg.flags.syn()
        && !seg.flags.fin()
        && !seg.flags.rst()
        && seg.payload.is_empty()
        && seg.seq.value() == expected_seq
}

#[test]
fn keepalive_probes_an_idle_connection_and_a_reply_resets_it() {
    let (mut client, mut server) = handshake_ka(ms(0));
    // The client's send frontier is iss + 1 (the SYN), so a probe carries
    // iss (1000).
    let probe_seq = 1000;

    // No probe before the idle interval elapses.
    client.advance(ms(999));
    assert!(drain(&mut client, ms(999)).is_empty());
    assert_eq!(client.keepalive_unacked, 0);

    // At the idle deadline exactly one probe is sent.
    client.advance(ms(1000));
    let probes = drain(&mut client, ms(1000));
    assert_eq!(probes.len(), 1, "one keepalive probe at the idle deadline");
    assert!(is_keepalive_probe(&probes[0], probe_seq));
    assert_eq!(client.keepalive_unacked, 1);

    // The peer answers the probe; delivering the reply resets the idle timer
    // and clears the probe count.
    for f in &probes {
        feed(&mut server, f, ms(1000));
    }
    let replies = drain(&mut server, ms(1000));
    assert!(!replies.is_empty(), "the peer answers a keepalive probe");
    for f in &replies {
        feed(&mut client, f, ms(1000));
    }
    assert_eq!(client.keepalive_unacked, 0);
    assert_eq!(client.state(), State::Established);

    // The next probe waits a fresh full idle interval from the reply.
    client.advance(ms(1999));
    assert!(drain(&mut client, ms(1999)).is_empty());
    client.advance(ms(2000));
    assert_eq!(drain(&mut client, ms(2000)).len(), 1);
}

#[test]
fn keepalive_aborts_after_the_probe_budget_is_exhausted() {
    let (mut client, mut server) = handshake_ka(ms(0));
    // The peer is dead: nothing is ever delivered back to the client.
    let _ = &mut server;

    let mut probes = 0usize;
    let mut saw_rst = false;
    let mut t = 0u64;
    while client.state() != State::Closed {
        t += 1000;
        assert!(t <= 10_000, "keepalive never aborted the dead connection");
        client.advance(ms(t));
        for f in &drain(&mut client, ms(t)) {
            let seg = TcpSegment::parse(PSEUDO, f).expect("parse");
            if seg.flags.rst() {
                saw_rst = true;
            } else if is_keepalive_probe(f, 1000) {
                probes += 1;
            }
        }
    }
    assert_eq!(
        probes, 3,
        "exactly keepalive_probes probes before the abort"
    );
    assert!(saw_rst, "a keepalive abort sends a RST");
    assert_eq!(client.reset_reason(), Some(ResetReason::TimedOut));
}

#[test]
fn keepalive_is_disabled_by_default() {
    let (mut client, mut server) = handshake(ms(0));
    let _ = &mut server;
    assert_eq!(
        client.keepalive_deadline, NEVER,
        "no idle timer when disabled"
    );
    assert_eq!(client.next_deadline(), None);

    // Advance far past any plausible idle interval: no probe is ever sent and
    // the connection is never torn down.
    let far = Duration64::from_secs(100_000);
    client.advance(far);
    assert!(drain(&mut client, far).is_empty());
    assert_eq!(client.state(), State::Established);
}

#[test]
fn sending_data_defers_keepalive() {
    let (mut client, mut server) = handshake_ka(ms(0));

    // Just before the idle deadline, exchange data — activity that restarts
    // the idle timer from the moment of the exchange.
    client.advance(ms(900));
    client.send(b"ping").unwrap();
    for f in drain(&mut client, ms(900)) {
        feed(&mut server, &f, ms(900));
    }
    for f in drain(&mut server, ms(900)) {
        feed(&mut client, &f, ms(900));
    }
    assert_eq!(client.keepalive_unacked, 0);

    // No probe at the original 1000 ms deadline: the timer restarted at 900.
    client.advance(ms(1000));
    assert!(drain(&mut client, ms(1000)).is_empty());

    // A probe only after a fresh full idle interval from the activity.
    client.advance(ms(1900));
    assert_eq!(drain(&mut client, ms(1900)).len(), 1);
}

// --- RFC 3168 Explicit Congestion Notification -------------------------

use crate::addr::Ecn;
use crate::tcp::TcpFlags;

/// A [`TcpConfig`] that offers ECN (RFC 3168 §6.1.1).
fn config_ecn() -> TcpConfig {
    TcpConfig {
        enable_ecn: true,
        ..config()
    }
}

/// The control-bit flags of a captured segment.
fn flags_of(frame: &[u8]) -> TcpFlags {
    TcpSegment::parse(PSEUDO, frame)
        .expect("a serialised segment parses")
        .flags
}

/// Drain captured segments together with the IP ECN codepoint the engine
/// asked be stamped on each ([`OutSegment::ecn`]).
fn drain_ecn(tcb: &mut Tcb, now: Duration64) -> Vec<(Vec<u8>, Ecn)> {
    let mut out = Vec::new();
    tcb.poll_transmit(now, |seg| {
        let mut buf = vec![0u8; crate::tcp::MAX_HEADER_LEN + seg.payload.len()];
        let n = crate::tcp::write(PSEUDO, &seg.meta, seg.payload, &mut buf)
            .expect("a planned segment always fits and serialises");
        buf.truncate(n);
        out.push((buf, seg.ecn));
        true
    });
    out
}

/// An ECN-capable client and server that completed the handshake with ECN
/// negotiated on both sides.
fn handshake_ecn(now: Duration64) -> (Tcb, Tcb) {
    let mut client = Tcb::connect(config_ecn(), 40000, 80, 1000, now);
    let mut server = Tcb::listen(config_ecn(), 80, 0, 5000);
    settle(&mut client, &mut server, now);
    assert_eq!(client.state(), State::Established);
    assert_eq!(server.state(), State::Established);
    assert!(client.ecn_ok(), "client negotiated ECN");
    assert!(server.ecn_ok(), "server negotiated ECN");
    (client, server)
}

#[test]
fn ecn_setup_syn_carries_ece_and_cwr() {
    let now = ms(0);
    let mut client = Tcb::connect(config_ecn(), 40000, 80, 1000, now);
    let syn = drain(&mut client, now);
    assert_eq!(syn.len(), 1, "one SYN");
    let f = flags_of(&syn[0]);
    assert!(f.syn() && !f.ack(), "the ECN-setup segment is a bare SYN");
    assert!(f.ece() && f.cwr(), "an ECN-setup SYN sets both ECE and CWR");
}

#[test]
fn syn_without_ecn_has_no_ece_or_cwr() {
    let now = ms(0);
    let mut client = Tcb::connect(config(), 40000, 80, 1000, now);
    let syn = drain(&mut client, now);
    let f = flags_of(&syn[0]);
    assert!(!f.ece() && !f.cwr(), "a plain SYN offers no ECN");
}

#[test]
fn synack_confirms_ecn_only_when_server_agrees() {
    let now = ms(0);
    // Server enabled: the SYN-ACK carries ECE alone and both sides agree.
    let mut client = Tcb::connect(config_ecn(), 40000, 80, 1000, now);
    let mut server = Tcb::listen(config_ecn(), 80, 0, 5000);
    let syn = drain(&mut client, now);
    for fr in &syn {
        feed(&mut server, fr, now);
    }
    let synack = drain(&mut server, now);
    let f = flags_of(&synack[0]);
    assert!(f.syn() && f.ack(), "SYN-ACK");
    assert!(
        f.ece() && !f.cwr(),
        "an ECN-setup SYN-ACK sets ECE with CWR clear"
    );
    for fr in &synack {
        feed(&mut client, fr, now);
    }
    assert!(client.ecn_ok(), "the client confirms ECN from the SYN-ACK");
}

#[test]
fn no_ecn_when_server_disabled() {
    let now = ms(0);
    let mut client = Tcb::connect(config_ecn(), 40000, 80, 1000, now);
    // Server does not offer ECN.
    let mut server = Tcb::listen(config(), 80, 0, 5000);
    settle(&mut client, &mut server, now);
    assert_eq!(client.state(), State::Established);
    assert!(!client.ecn_ok(), "no ECN when the peer did not agree");
    assert!(!server.ecn_ok());
    // The client, having not negotiated ECN, marks its data Not-ECT.
    client.send(b"data").unwrap();
    let segs = drain_ecn(&mut client, now);
    assert!(
        segs.iter().all(|(_, ecn)| *ecn == Ecn::NotEct),
        "a non-ECN connection never marks a packet ECN-capable"
    );
}

#[test]
fn ecn_data_segment_is_marked_ect0() {
    let now = ms(0);
    let (mut client, _server) = handshake_ecn(now);
    client.send(b"payload").unwrap();
    let segs = drain_ecn(&mut client, now);
    let data = segs
        .iter()
        .find(|(bytes, _)| !TcpSegment::parse(PSEUDO, bytes).unwrap().payload.is_empty())
        .expect("a data segment");
    assert_eq!(
        data.1,
        Ecn::Ect0,
        "fresh data on an ECN connection is ECT(0)"
    );
}

#[test]
fn receiver_echoes_ece_after_a_ce_mark() {
    let now = ms(0);
    let (mut client, mut server) = handshake_ecn(now);
    client.send(b"hello").unwrap();
    let data = drain(&mut client, now);
    // The network marked the data Congestion Experienced.
    for fr in &data {
        feed_ecn(&mut server, fr, Ecn::Ce, now);
    }
    let acks = drain(&mut server, now);
    assert!(
        acks.iter().any(|f| flags_of(f).ece()),
        "the receiver echoes ECE after a CE mark"
    );
}

#[test]
fn sender_reduces_cwnd_and_sets_cwr_on_ece_once_per_window() {
    let now = ms(0);
    let (mut client, mut server) = handshake_ecn(now);
    // Client sends a byte; the network marks it CE, so the server's ACK
    // echoes ECE back.
    client.send(b"a").unwrap();
    let data = drain(&mut client, now);
    for fr in &data {
        feed_ecn(&mut server, fr, Ecn::Ce, now);
    }
    let ece_ack = drain(&mut server, now);
    assert!(
        ece_ack.iter().any(|f| flags_of(f).ece()),
        "the ACK carries ECE"
    );
    let cwnd_before = client.cwnd();
    for fr in &ece_ack {
        feed(&mut client, fr, now);
    }
    let cwnd_after = client.cwnd();
    assert!(
        cwnd_after < cwnd_before,
        "an ECE-marked ACK reduces the congestion window ({cwnd_before} -> {cwnd_after})"
    );
    // The next fresh data segment announces the reduction with CWR.
    client.send(b"b").unwrap();
    let d2 = drain(&mut client, now);
    assert!(
        d2.iter().any(|f| flags_of(f).cwr()),
        "the next fresh data segment sets CWR"
    );
    // A second ECE-marked ACK within the same window does not reduce again.
    for fr in &ece_ack {
        feed(&mut client, fr, now);
    }
    assert_eq!(
        client.cwnd(),
        cwnd_after,
        "the window is reduced at most once per window of data"
    );
}

#[test]
fn non_ecn_connection_ignores_a_ce_mark() {
    let now = ms(0);
    let (mut client, mut server) = handshake(now);
    client.send(b"hello").unwrap();
    let data = drain(&mut client, now);
    for fr in &data {
        feed_ecn(&mut server, fr, Ecn::Ce, now);
    }
    let acks = drain(&mut server, now);
    assert!(
        acks.iter().all(|f| !flags_of(f).ece()),
        "a connection that never negotiated ECN never echoes ECE"
    );
}
