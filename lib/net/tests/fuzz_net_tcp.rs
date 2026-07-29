//! Deterministic fuzz harness for the TCP segment codec.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. Parsing never panics, for either family, on any bytes.
//! 2. A parsed segment's payload lies within the input bytes and past a
//!    header of at least the fixed 20 bytes.
//! 3. A segment produced by [`tcp::write`] parses back to the same header
//!    fields, options, and payload under the same pseudo-header
//!    (round-trip), and its computed checksum verifies.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing
//! from the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses
//! under `cargo xtask fuzz`.

use tairix_abi::time::Duration64;
use tairix_net::checksum::Pseudo;
use tairix_net::tcp::conn::{OutSegment, State, Tcb, TcpConfig};
use tairix_net::tcp::{
    self, SackBlock, SeqNumber, TcpFlags, TcpOptions, TcpSegment, TcpSegmentMeta, Timestamps,
    MAX_HEADER_LEN, MAX_SACK_BLOCKS, TCP_HEADER_LEN,
};
use tairix_net::{Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn pseudo(rng: &mut Lcg) -> Pseudo {
    if rng.next_u64().is_multiple_of(2) {
        Pseudo::V4 {
            source: Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes()),
            destination: Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes()),
        }
    } else {
        let mut octets = [0u8; 16];
        rng.fill(&mut octets);
        let source = Ipv6Addr::from(octets);
        rng.fill(&mut octets);
        Pseudo::V6 {
            source,
            destination: Ipv6Addr::from(octets),
        }
    }
}

fn exercise_parse(p: Pseudo, bytes: &[u8]) {
    if let Some(seg) = TcpSegment::parse(p, bytes) {
        // A parsed segment always has at least the fixed header, and its
        // payload lies wholly within the input.
        assert!(bytes.len() >= TCP_HEADER_LEN);
        assert!(seg.payload.len() <= bytes.len() - TCP_HEADER_LEN);
    }
}

fn random_options(rng: &mut Lcg) -> TcpOptions {
    let mut opts = TcpOptions::new();
    let bits = rng.next_u64();
    if bits & 1 != 0 {
        opts.mss = Some((rng.next_u64() & 0xFFFF) as u16);
    }
    if bits & 2 != 0 {
        opts.window_scale = Some((rng.next_u64() & 0xFF) as u8);
    }
    if bits & 4 != 0 {
        opts.sack_permitted = true;
    }
    if bits & 8 != 0 {
        opts.timestamps = Some(Timestamps {
            value: (rng.next_u64() & 0xFFFF_FFFF) as u32,
            echo: (rng.next_u64() & 0xFFFF_FFFF) as u32,
        });
    }
    if bits & 16 != 0 {
        let count = ((rng.next_u64() & 0x3) as usize % MAX_SACK_BLOCKS) + 1;
        let mut blocks = [SackBlock {
            left: SeqNumber::new(0),
            right: SeqNumber::new(0),
        }; MAX_SACK_BLOCKS];
        for block in blocks.iter_mut().take(count) {
            *block = SackBlock {
                left: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
                right: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
            };
        }
        assert!(opts.set_sack(&blocks[..count]));
    }
    opts
}

fn exercise_round_trip(rng: &mut Lcg, p: Pseudo) {
    let options = random_options(rng);
    let meta = TcpSegmentMeta {
        source_port: (rng.next_u64() & 0xFFFF) as u16,
        destination_port: (rng.next_u64() & 0xFFFF) as u16,
        seq: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
        ack: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
        flags: TcpFlags::from_bits((rng.next_u64() & 0xFF) as u8),
        window: (rng.next_u64() & 0xFFFF) as u16,
        urgent: (rng.next_u64() & 0xFFFF) as u16,
        options,
    };
    let len = (rng.next_u64() & 0x1FF) as usize;
    let mut payload = vec![0u8; len];
    rng.fill(&mut payload);
    let mut out = vec![0u8; MAX_HEADER_LEN + len];
    let n = match tcp::write(p, &meta, &payload, &mut out) {
        Ok(n) => n,
        // A random option mix can exceed the 40-byte region; the codec
        // correctly refuses it. That is a valid outcome, not a round-trip.
        Err(tcp::WriteError::OptionsTooLarge) => return,
        Err(other) => panic!("unexpected write error: {other:?}"),
    };
    let seg = TcpSegment::parse(p, &out[..n]).expect("a freshly written segment parses");
    assert_eq!(seg.source_port, meta.source_port);
    assert_eq!(seg.destination_port, meta.destination_port);
    assert_eq!(seg.seq, meta.seq);
    assert_eq!(seg.ack, meta.ack);
    assert_eq!(seg.flags, meta.flags);
    assert_eq!(seg.window, meta.window);
    assert_eq!(seg.urgent, meta.urgent);
    assert_eq!(seg.options.mss, meta.options.mss);
    assert_eq!(seg.options.window_scale, meta.options.window_scale);
    assert_eq!(seg.options.sack_permitted, meta.options.sack_permitted);
    assert_eq!(seg.options.timestamps, meta.options.timestamps);
    assert_eq!(seg.options.sack(), meta.options.sack());
    assert_eq!(seg.payload, &payload[..]);
}

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in the sibling harnesses so failures reproduce one way.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, buf.len() - i);
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

#[test]
fn random_inputs_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 128];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let p = pseudo(&mut rng);
            let size = ((rng.next_u64() & 0x7F) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(p, &buf[..size]);
            exercise_round_trip(&mut rng, p);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn corrupted_fields_never_panic() {
    // Bit-flip every bit of a valid segment to walk the accept/reject
    // boundary of the data-offset, option, and checksum checks.
    let p = Pseudo::V4 {
        source: Ipv4Addr::new(10, 0, 2, 2),
        destination: Ipv4Addr::new(10, 0, 2, 15),
    };
    let mut opts = TcpOptions::new();
    opts.mss = Some(1460);
    opts.timestamps = Some(Timestamps { value: 42, echo: 7 });
    let meta = TcpSegmentMeta {
        source_port: 4444,
        destination_port: 80,
        seq: SeqNumber::new(0x0102_0304),
        ack: SeqNumber::new(0x0506_0708),
        flags: TcpFlags::SYN | TcpFlags::ACK,
        window: 0xFFFF,
        urgent: 0,
        options: opts,
    };
    let mut segment = vec![0u8; MAX_HEADER_LEN + 16];
    let n = tcp::write(p, &meta, &[0xA5; 16], &mut segment).expect("seed segment writes");
    segment.truncate(n);
    TcpSegment::parse(p, &segment).expect("seed segment parses");
    for byte in 0..segment.len() {
        for bit in 0..8u32 {
            segment[byte] ^= 1 << bit;
            exercise_parse(p, &segment);
            segment[byte] ^= 1 << bit;
        }
    }
}

// ---------------------------------------------------------------------------
// The N5b connection state-machine driver.
//
// Two live TCBs are driven against each other while a hostile injector feeds
// them attacker-crafted (but parseable) segments. Invariants, for any schedule
// and any injected bytes:
//
//   1. No operation ever panics (`send`/`recv`/`close`/`on_segment`/`advance`/
//      `poll_transmit`), for any state or input.
//   2. Every segment the engine emits is a well-formed segment that parses
//      back under the same pseudo-header (so the wire path is always valid).
//   3. `next_deadline` never yields an already-armed-in-the-past-only view
//      that stalls the machine: after firing every due timer, the connection
//      keeps making progress or is terminal (Closed).
// ---------------------------------------------------------------------------

const DRIVER_PSEUDO: Pseudo = Pseudo::V4 {
    source: Ipv4Addr::new(10, 0, 0, 1),
    destination: Ipv4Addr::new(10, 0, 0, 2),
};

fn driver_config() -> TcpConfig {
    TcpConfig {
        send_buffer: 8 * 1024,
        receive_buffer: 8 * 1024,
        ..TcpConfig::default()
    }
}

fn ms(n: u64) -> Duration64 {
    Duration64::from_nanos(n.saturating_mul(1_000_000))
}

/// Drain a TCB's outbound segments, verifying each parses back, returning the
/// serialised frames.
fn driver_drain(tcb: &mut Tcb, now: Duration64) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    tcb.poll_transmit(now, |out: OutSegment<'_>| {
        let mut buf = vec![0u8; MAX_HEADER_LEN + out.payload.len()];
        let n = tcp::write(DRIVER_PSEUDO, &out.meta, out.payload, &mut buf)
            .expect("a planned segment always serialises");
        // Invariant 2: it parses back.
        assert!(
            TcpSegment::parse(DRIVER_PSEUDO, &buf[..n]).is_some(),
            "engine emitted an unparseable segment"
        );
        buf.truncate(n);
        frames.push(buf);
        true
    });
    frames
}

fn driver_feed(tcb: &mut Tcb, frame: &[u8], now: Duration64) {
    if let Some(seg) = TcpSegment::parse(DRIVER_PSEUDO, frame) {
        tcb.on_segment(&seg, tairix_net::addr::Ecn::NotEct, now);
    }
}

/// A parseable but arbitrary segment aimed at the server, so `on_segment`
/// sees hostile flag/seq/ack combinations (RST/SYN injection, blind data).
fn injected(rng: &mut Lcg, base_seq: u32) -> Vec<u8> {
    // Bias the sequence near the plausible window so the acceptability and
    // RFC 5961 paths are actually reached, not always rejected up front.
    let seq = base_seq.wrapping_add((rng.next_u64() % 4096) as u32);
    let meta = TcpSegmentMeta {
        source_port: 40000,
        destination_port: 80,
        seq: SeqNumber::new(seq),
        ack: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
        flags: TcpFlags::from_bits((rng.next_u64() & 0xFF) as u8),
        window: (rng.next_u64() & 0xFFFF) as u16,
        urgent: 0,
        options: TcpOptions::new(),
    };
    let len = (rng.next_u64() % 32) as usize;
    let payload: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
    let mut buf = vec![0u8; MAX_HEADER_LEN + len];
    match tcp::write(DRIVER_PSEUDO, &meta, &payload, &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

/// A parseable ACK aimed at the *client* (the sender running RFC 6675
/// recovery), carrying arbitrary SACK blocks near its send space, so the
/// scoreboard's `record`/`is_lost`/`NextSeg` path is driven by hostile
/// selective acknowledgements — never trusting them to stay in window.
fn injected_sack_ack(rng: &mut Lcg, client_isn: u32, server_isn: u32) -> Vec<u8> {
    let seq = server_isn
        .wrapping_add(1)
        .wrapping_add((rng.next_u64() % 4096) as u32);
    let ack = client_isn
        .wrapping_add(1)
        .wrapping_add((rng.next_u64() % 8192) as u32);
    let mut options = TcpOptions::new();
    let count = ((rng.next_u64() & 0x3) as usize % MAX_SACK_BLOCKS) + 1;
    let mut blocks = [SackBlock {
        left: SeqNumber::new(0),
        right: SeqNumber::new(0),
    }; MAX_SACK_BLOCKS];
    for block in blocks.iter_mut().take(count) {
        let left = client_isn
            .wrapping_add(1)
            .wrapping_add((rng.next_u64() % 16384) as u32);
        let span = (rng.next_u64() % 4096) as u32;
        *block = SackBlock {
            left: SeqNumber::new(left),
            right: SeqNumber::new(left.wrapping_add(span)),
        };
    }
    assert!(options.set_sack(&blocks[..count]));
    let meta = TcpSegmentMeta {
        source_port: 80,
        destination_port: 40000,
        seq: SeqNumber::new(seq),
        ack: SeqNumber::new(ack),
        flags: TcpFlags::ACK,
        window: (rng.next_u64() & 0xFFFF) as u16,
        urgent: 0,
        options,
    };
    let mut buf = vec![0u8; MAX_HEADER_LEN];
    match tcp::write(DRIVER_PSEUDO, &meta, &[], &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

fn drive_once(rng: &mut Lcg) {
    let now0 = ms(0);
    let client_isn = (rng.next_u64() & 0xFFFF_FFFF) as u32;
    let server_isn = (rng.next_u64() & 0xFFFF_FFFF) as u32;
    let mut client = Tcb::connect(driver_config(), 40000, 80, client_isn, now0);
    let mut server = Tcb::listen(driver_config(), 80, 0, server_isn);

    // The server's rcv_nxt starts near client_isn+1 once the SYN is seen.
    let injection_base = client_isn.wrapping_add(1);

    let mut t = 0u64;
    for _ in 0..600 {
        t += 1 + (rng.next_u64() % 7);
        let now = ms(t);

        // Random application actions.
        match rng.next_u64() % 8 {
            0..=2 => {
                let len = (rng.next_u64() % 300) as usize;
                let data: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
                let _ = client.send(&data);
            }
            3 => {
                let len = (rng.next_u64() % 300) as usize;
                let data: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
                let _ = server.send(&data);
            }
            4 => {
                let _ = client.close(now);
            }
            5 => {
                let _ = server.close(now);
            }
            6 => client.abort(now),
            _ => {}
        }

        client.advance(now);
        server.advance(now);

        // Deliver legitimate traffic in both directions, dropping ~1/4.
        for f in driver_drain(&mut client, now) {
            if !rng.next_u64().is_multiple_of(4) {
                driver_feed(&mut server, &f, now);
            }
        }
        for f in driver_drain(&mut server, now) {
            if !rng.next_u64().is_multiple_of(4) {
                driver_feed(&mut client, &f, now);
            }
        }

        // Injected hostile segments at the server.
        if rng.next_u64().is_multiple_of(3) {
            let f = injected(rng, injection_base);
            if !f.is_empty() {
                driver_feed(&mut server, &f, now);
            }
        }

        // Injected hostile SACK-bearing ACKs at the client sender.
        if rng.next_u64().is_multiple_of(3) {
            let f = injected_sack_ack(rng, client_isn, server_isn);
            if !f.is_empty() {
                driver_feed(&mut client, &f, now);
            }
        }

        // Drain whatever the endpoints delivered.
        let mut buf = [0u8; 512];
        while client.recv(&mut buf) != 0 {}
        while server.recv(&mut buf) != 0 {}

        // Invariant 3: a reset connection emits at most its queued RST, then
        // falls silent (it never keeps generating segments).
        if matches!(client.state(), State::Closed) && client.reset_reason().is_some() {
            let first = driver_drain(&mut client, now).len();
            assert!(first <= 1, "a reset connection emitted more than one RST");
            assert_eq!(
                driver_drain(&mut client, now).len(),
                0,
                "a reset connection kept emitting segments"
            );
        }
    }
}

#[test]
fn state_machine_driver_never_panics() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "state_machine_driver_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..64 {
            drive_once(&mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// The N6b-2 listener + SYN-cookie driver.
//
// A bounded listener is flooded with hostile, arbitrary (but parseable)
// SYN/ACK/RST/data segments from a churn of random peers, interleaved with a
// few genuine handshakes. Invariants, for any schedule and any injected bytes:
//
//   1. No listener operation ever panics.
//   2. Half-open state never exceeds `max_half_open`, and the accept queue
//      never exceeds `max_accept` — a SYN flood consumes bounded memory.
//   3. Every segment the listener emits parses back under its pseudo-header.
//   4. A cookie the listener itself minted, replayed honestly, is accepted;
//      an off-path forgery is not (checked in the deterministic unit tests).
// ---------------------------------------------------------------------------

const LISTEN_PORT: u16 = 80;
const LISTEN_LOCAL: tairix_net::IpAddr = tairix_net::IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

/// A deterministic keyed MAC standing in for the `lib/crypto`-backed secret.
struct FuzzSecret(u64);

impl tairix_net::tcp::listen::CookieSecret for FuzzSecret {
    fn mac(&self, tuple: &[u8], counter: u32) -> u32 {
        let mut h = self.0 ^ (u64::from(counter).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for &b in tuple {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        }
        (((h >> 32) ^ h) & 0xFFFF_FFFF) as u32
    }
}

fn listen_pseudo(peer: tairix_net::tcp::listen::Peer) -> Pseudo {
    match peer.addr {
        tairix_net::IpAddr::V4(a) => Pseudo::V4 {
            source: a,
            destination: Ipv4Addr::new(10, 0, 0, 1),
        },
        tairix_net::IpAddr::V6(_) => unreachable!("driver uses v4 peers"),
    }
}

fn random_peer(rng: &mut Lcg) -> tairix_net::tcp::listen::Peer {
    let bytes = (rng.next_u64() & 0xFFFF_FFFF) as u32;
    tairix_net::tcp::listen::Peer {
        addr: tairix_net::IpAddr::V4(Ipv4Addr::from(bytes.to_be_bytes())),
        port: ((rng.next_u64() & 0xFFFF) as u16) | 1,
    }
}

/// Feed one hostile arbitrary segment to the listener, asserting bounds and
/// that every reply parses.
fn listener_inject(
    listener: &mut tairix_net::tcp::listen::Listener,
    rng: &mut Lcg,
    peer: tairix_net::tcp::listen::Peer,
    secret: &FuzzSecret,
    now: Duration64,
    max_half_open: usize,
    max_accept: usize,
) {
    let ps = listen_pseudo(peer);
    let meta = TcpSegmentMeta {
        source_port: peer.port,
        destination_port: LISTEN_PORT,
        seq: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
        ack: SeqNumber::new((rng.next_u64() & 0xFFFF_FFFF) as u32),
        flags: TcpFlags::from_bits((rng.next_u64() & 0xFF) as u8),
        window: (rng.next_u64() & 0xFFFF) as u16,
        urgent: 0,
        options: random_options(rng),
    };
    let len = (rng.next_u64() % 8) as usize;
    let payload: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
    let mut buf = vec![0u8; MAX_HEADER_LEN + len];
    let Ok(n) = tcp::write(ps, &meta, &payload, &mut buf) else {
        return;
    };
    let Some(seg) = TcpSegment::parse(ps, &buf[..n]) else {
        return;
    };
    listener.on_segment(LISTEN_LOCAL, peer, &seg, now, secret, |p, out| {
        let mut rb = vec![0u8; MAX_HEADER_LEN + out.payload.len()];
        let rn = tcp::write(listen_pseudo(p), &out.meta, out.payload, &mut rb)
            .expect("listener reply serialises");
        assert!(
            TcpSegment::parse(listen_pseudo(p), &rb[..rn]).is_some(),
            "listener emitted an unparseable segment"
        );
        true
    });
    assert!(
        listener.half_open_len() <= max_half_open,
        "half-open bound broken"
    );
    assert!(listener.pending() <= max_accept, "accept bound broken");
}

fn listener_drive_once(rng: &mut Lcg) {
    let secret = FuzzSecret(rng.next_u64() | 1);
    let max_half_open = ((rng.next_u64() % 8) as usize) + 1;
    let max_accept = ((rng.next_u64() % 8) as usize) + 1;
    let mut listener = tairix_net::tcp::listen::Listener::new(
        LISTEN_PORT,
        tairix_net::tcp::listen::ListenConfig {
            max_half_open,
            max_accept,
            half_open_timeout: ms(5000),
            template: driver_config(),
        },
    );

    let mut t = 0u64;
    let mut honest = tairix_net::tcp::listen::Peer {
        addr: tairix_net::IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42)),
        port: 55555,
    };
    for _ in 0..300 {
        t += 1 + (rng.next_u64() % 9);
        let now = ms(t);
        // A churn of hostile peers, mostly SYNs to drive the flood.
        for _ in 0..(rng.next_u64() % 6) {
            let peer = random_peer(rng);
            listener_inject(
                &mut listener,
                rng,
                peer,
                &secret,
                now,
                max_half_open,
                max_accept,
            );
        }
        // Occasionally complete an honest cookie handshake and accept it.
        if rng.next_u64().is_multiple_of(5) {
            honest.port = honest.port.wrapping_add(1) | 1;
            let ps = listen_pseudo(honest);
            let mut client = Tcb::connect(driver_config(), honest.port, LISTEN_PORT, 0x1234, now);
            for _ in 0..4 {
                let mut frames = Vec::new();
                client.poll_transmit(now, |out| {
                    let mut b = vec![0u8; MAX_HEADER_LEN + out.payload.len()];
                    let n = tcp::write(ps, &out.meta, out.payload, &mut b).expect("write");
                    b.truncate(n);
                    frames.push(b);
                    true
                });
                if frames.is_empty() {
                    break;
                }
                for f in frames {
                    if let Some(seg) = TcpSegment::parse(ps, &f) {
                        listener.on_segment(LISTEN_LOCAL, honest, &seg, now, &secret, |p, out| {
                            let mut rb = vec![0u8; MAX_HEADER_LEN + out.payload.len()];
                            let rn = tcp::write(listen_pseudo(p), &out.meta, out.payload, &mut rb)
                                .expect("reply serialises");
                            if let Some(s) = TcpSegment::parse(ps, &rb[..rn]) {
                                client.on_segment(&s, tairix_net::addr::Ecn::NotEct, now);
                            }
                            true
                        });
                    }
                }
                if listener.pending() > 0 {
                    break;
                }
            }
        }
        listener.advance(now, |_, _| true);
        // Drain some accepted connections so the queue churns.
        while listener.accept().is_some() {}
        assert!(listener.half_open_len() <= max_half_open);
        assert!(listener.pending() <= max_accept);
    }
}

#[test]
fn listener_driver_never_panics() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "listener_driver_never_panics",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..64 {
            listener_drive_once(&mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
