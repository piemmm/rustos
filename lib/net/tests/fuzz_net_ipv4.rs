//! Deterministic fuzz harness for the IPv4 codec, emit-side
//! fragmentation, and the fragment reassembler.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. Parsing, fragment planning, and reassembly never panic.
//! 2. An accepted option-free parse round-trips through the writer.
//! 3. A fragment plan covers its payload exactly once within the MTU.
//! 4. The reassembler's budgets hold after every push.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing
//! from the same seeded stream until `RUSTOS_FUZZ_BUDGET_SECS` elapses
//! under `cargo xtask fuzz`.

use rustos_abi::time::Duration64;
use rustos_net::frag::{FragKey, PushOutcome, Reassembler, ReassemblyConfig};
use rustos_net::ipv4::{self, Ipv4Header};
use rustos_net::{IpAddr, Ipv4Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn exercise_parse(bytes: &[u8]) {
    if let Some((header, options, payload)) = Ipv4Header::parse(bytes) {
        if !options.is_empty() {
            return;
        }
        let mut out = vec![0u8; ipv4::IPV4_HEADER_LEN + payload.len()];
        let header_len = header
            .write(&mut out, payload.len())
            .expect("a parsed option-free IPv4 header must re-encode");
        out[header_len..].copy_from_slice(payload);
        let (redecoded, _, redecoded_payload) =
            Ipv4Header::parse(&out).expect("round-trip of an accepted header must parse");
        assert_eq!(header, redecoded);
        assert_eq!(payload, redecoded_payload);
    }
}

fn exercise_fragment(rng: &mut Lcg) {
    let mut header = Ipv4Header::new(
        Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes()),
        Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes()),
        (rng.next_u64() & 0xFF) as u8,
    );
    header.dont_fragment = rng.next_u64() % 4 == 0;
    header.identification = (rng.next_u64() & 0xFFFF) as u16;
    let payload_len = (rng.next_u64() & 0x1_FFFF) as usize;
    let mtu = (rng.next_u64() & 0xFFF) as usize;
    if let Some(parts) = ipv4::fragment(header, payload_len, mtu) {
        let mut expected_start = 0usize;
        for (i, part) in parts.iter().enumerate() {
            assert_eq!(part.payload_start, expected_start);
            assert!(part.payload_end > part.payload_start || payload_len == 0);
            assert!(
                ipv4::IPV4_HEADER_LEN + (part.payload_end - part.payload_start) <= mtu
                    || parts.len() == 1
            );
            assert_eq!(part.header.more_fragments, i + 1 < parts.len());
            expected_start = part.payload_end;
        }
        assert_eq!(expected_start, payload_len);
    }
}

fn exercise_reassembler(rng: &mut Lcg, reassembler: &mut Reassembler, config: &ReassemblyConfig) {
    let key = FragKey {
        source: IpAddr::V4(Ipv4Addr::new(10, 0, (rng.next_u64() % 4) as u8, 1)),
        destination: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
        identification: (rng.next_u64() % 64) as u32,
        protocol: 17,
    };
    let offset = (rng.next_u64() & 0x1_FFFF) as usize;
    let len = (rng.next_u64() & 0x7FF) as usize;
    let more = rng.next_u64() % 2 == 0;
    let data = vec![0xA5u8; len];
    let now = Duration64::from_secs(i64::try_from(rng.next_u64() & 0x3FF).expect("bounded"));
    match reassembler.push(key, offset, more, &data, now) {
        PushOutcome::Complete(payload) => assert!(payload.len() <= rustos_net::frag::MAX_DATAGRAM),
        PushOutcome::Pending | PushOutcome::Rejected(_) => {}
    }
    assert!(reassembler.buffered_bytes() <= config.global_budget);
    assert!(reassembler.len() <= config.max_datagrams);
    reassembler.advance(now);
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
    let mut rng = Lcg::new(rustos_fuzzseed::start(
        "random_inputs_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let config = ReassemblyConfig {
        per_source_budget: 64 * 1024,
        global_budget: 256 * 1024,
        max_datagrams: 32,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    let mut buf = [0u8; 128];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size]);
            exercise_fragment(&mut rng);
            exercise_reassembler(&mut rng, &mut reassembler, &config);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    // Bit-flip every bit of a valid IPv4 packet to walk the
    // accepted/rejected boundary.
    let mut packet = vec![0u8; ipv4::IPV4_HEADER_LEN + 16];
    Ipv4Header::new(
        Ipv4Addr::new(10, 0, 2, 2),
        Ipv4Addr::new(10, 0, 2, 15),
        ipv4::PROTOCOL_ICMP,
    )
    .write(&mut packet, 16)
    .expect("seed packet writes");
    Ipv4Header::parse(&packet).expect("seed packet parses");
    for byte in 0..packet.len() {
        for bit in 0..8u32 {
            packet[byte] ^= 1 << bit;
            exercise_parse(&packet);
            packet[byte] ^= 1 << bit;
        }
    }
}
