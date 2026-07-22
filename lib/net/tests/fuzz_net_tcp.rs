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

use tairix_net::checksum::Pseudo;
use tairix_net::tcp::{
    self, SackBlock, SeqNumber, TcpFlags, TcpOptions, TcpSegment, TcpSegmentMeta, Timestamps,
    MAX_HEADER_LEN, MAX_SACK_BLOCKS, TCP_HEADER_LEN,
};
use tairix_net::{Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn pseudo(rng: &mut Lcg) -> Pseudo {
    if rng.next_u64() % 2 == 0 {
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
