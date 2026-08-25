//! Deterministic fuzz harness for the DNS stub-resolver engine.
//!
//! Invariants, for any input bits a hostile server crafts:
//!
//! 1. [`DnsResponse::parse`] never panics, on any bytes, for any query.
//! 2. A parsed response's answer matches the query's type and, for an
//!    address answer, stays within its fixed capacity ([`MAX_ADDRESSES`]) —
//!    never an attacker-sized allocation.
//! 3. [`write_query`] never panics and always fits [`MAX_QUERY_LEN`].
//! 4. Driving the [`DnsResolver`] state machine with arbitrary datagrams at
//!    arbitrary times never panics and always yields a coherent
//!    next-deadline decision.
//! 5. Rendering any decoded name is total and emits printable ASCII only —
//!    a hostile `PTR` answer never reaches a terminal as control bytes.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_abi::time::Duration64;
use tairix_net::dns::{
    write_query, Answer, DnsResolver, DnsResponse, Name, QuerySpec, RecordType, MAX_ADDRESSES,
    MAX_QUERY_LEN,
};
use tairix_net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn record_type(rng: &mut Lcg) -> RecordType {
    match rng.next_u64() % 3 {
        0 => RecordType::A,
        1 => RecordType::Aaaa,
        _ => RecordType::Ptr,
    }
}

/// A random but always-valid query name drawn from a small label alphabet,
/// so the resolver and codec explore real names rather than only rejecting
/// at the encoder.
fn query_name(rng: &mut Lcg) -> Name {
    const NAMES: [&str; 5] = [
        "example.com",
        "www.example.com",
        "a.b.c.example.org",
        "host",
        "tairix.test",
    ];
    // A third of the draws are reverse names, so the `PTR` path explores
    // real `in-addr.arpa` / `ip6.arpa` questions rather than only rejecting.
    match rng.next_u64() % 3 {
        0 => Name::reverse(IpAddr::V4(Ipv4Addr::from(rng.next_u32().to_be_bytes()))),
        1 => {
            let mut octets = [0u8; 16];
            rng.fill(&mut octets);
            Name::reverse(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => Name::encode(NAMES[rng.index(NAMES.len())]).expect("fixed names encode"),
    }
}

/// Feed arbitrary bytes to the parser and assert the bounded invariants.
fn exercise_parse(bytes: &[u8], spec: &QuerySpec) {
    if let Some(resp) = DnsResponse::parse(bytes, spec) {
        assert_eq!(resp.id, spec.id);
        assert!(resp.answer.addresses().len() <= MAX_ADDRESSES);
        match spec.record_type {
            RecordType::Ptr => {
                assert!(matches!(resp.answer, Answer::Pointer(_)));
                assert!(resp.answer.addresses().is_empty());
            }
            RecordType::A | RecordType::Aaaa => {
                assert!(matches!(resp.answer, Answer::Addresses(_)));
                assert!(resp.answer.pointer().is_none());
            }
        }
        if let Some(name) = resp.answer.pointer() {
            let rendered = format!("{name}");
            assert!(
                rendered.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                "a decoded name renders as printable ASCII only"
            );
        }
    }
}

/// Encode a random query and confirm it is a bounded, non-panicking encode.
fn exercise_write(rng: &mut Lcg) {
    let spec = QuerySpec {
        id: rng.next_u16(),
        name: query_name(rng),
        record_type: record_type(rng),
        recursion_desired: rng.next_u64() & 1 == 0,
    };
    let mut buf = [0u8; MAX_QUERY_LEN];
    let n = write_query(&spec, &mut buf).expect("MAX_QUERY_LEN suffices");
    assert!(n <= MAX_QUERY_LEN);
}

/// Drive the resolver with arbitrary datagrams and time, asserting it never
/// panics and always yields a coherent next-deadline decision.
fn exercise_resolver(rng: &mut Lcg) {
    let mut csprng = Lcg::new(rng.next_u64());
    let mut rand = || csprng.next_u32();
    let servers = [
        IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
        IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    ];
    let name = query_name(rng);
    let rtype = record_type(rng);
    let mut resolver = DnsResolver::new(name, rtype, &servers[..rng.index(servers.len() + 1)]);
    let mut now = 0i64;
    let _ = resolver.poll(Duration64::from_secs(now), &mut rand);
    let mut buf = [0u8; 128];
    for _ in 0..8 {
        now = now.saturating_add(i64::from(rng.next_u32() % 200));
        // Sometimes a datagram, sometimes a timer poll.
        if rng.next_u64() & 1 == 0 {
            let size = rng.index(buf.len() + 1);
            rng.fill(&mut buf[..size]);
            let _ = resolver.on_response(Duration64::from_secs(now), &buf[..size], &mut rand);
        } else {
            let _ = resolver.poll(Duration64::from_secs(now), &mut rand);
        }
        if let Some(d) = resolver.next_deadline() {
            assert!(d.secs() >= 0);
        }
    }
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

    fn next_u32(&mut self) -> u32 {
        (self.next_u64() & 0xFFFF_FFFF) as u32
    }

    fn next_u16(&mut self) -> u16 {
        (self.next_u64() & 0xFFFF) as u16
    }

    /// A bounded index in `[0, modulus)`; `modulus` must be non-zero.
    fn index(&mut self, modulus: usize) -> usize {
        (self.next_u64() & 0xFFFF) as usize % modulus
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
    let mut buf = [0u8; 320];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let spec = QuerySpec {
                id: rng.next_u16(),
                name: query_name(&mut rng),
                record_type: record_type(&mut rng),
                recursion_desired: true,
            };
            let size = rng.index(buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size], &spec);
            exercise_write(&mut rng);
            exercise_resolver(&mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}
