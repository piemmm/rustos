//! Deterministic fuzz harness for the Neighbour Discovery codecs and
//! their neighbour-table glue.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. ND message parsing never panics, for any type/code/hop-limit.
//! 2. An accepted parse of a host-emitted message round-trips.
//! 3. Applying parsed messages to the neighbour table never panics and
//!    never grows the table beyond its capacity.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing
//! from the same seeded stream until `RUSTOS_FUZZ_BUDGET_SECS` elapses
//! under `cargo xtask fuzz`.

use rustos_abi::driver::net::MacAddress;
use rustos_abi::time::Duration64;
use rustos_net::nd::{self, NdMessage};
use rustos_net::neigh::{NeighborConfig, NeighborTable};
use rustos_net::Ipv6Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Neighbour-table capacity the harness asserts is never exceeded.
const TABLE_CAPACITY: usize = 8;

fn exercise(
    message_type: u8,
    code: u8,
    hop_limit: u8,
    dest_is_multicast: bool,
    body: &[u8],
    table: &mut NeighborTable,
    now: Duration64,
) {
    let Some(message) = NdMessage::parse(message_type, code, hop_limit, dest_is_multicast, body)
    else {
        return;
    };
    // Host-emitted kinds must round-trip; router-only kinds refuse to
    // write (this host never emits them).
    let mut out = [0u8; 512];
    if let Some(len) = message.write_body(&mut out) {
        let redecoded = NdMessage::parse(
            message.message_type(),
            0,
            nd::ND_HOP_LIMIT,
            false,
            &out[..len],
        )
        .expect("round-trip of a host-emitted message must parse");
        assert_eq!(message, redecoded);
    }
    // Feeding any validated message into the table must stay total and
    // bounded.
    let source = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 0x77);
    nd::apply_neighbor_solicitation(&message, source, table, now);
    nd::apply_neighbor_advertisement(&message, table, now);
    nd::apply_redirect(&message, table, now);
    assert!(table.len() <= TABLE_CAPACITY);
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
    let mut table = NeighborTable::new(TABLE_CAPACITY, NeighborConfig::default());
    let mut buf = [0u8; 256];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for step in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            // Bias toward the real ND types and the valid hop
            // limit/code so validated paths are actually reached.
            let message_type = match rng.next_u64() % 8 {
                0 => nd::TYPE_ROUTER_SOLICITATION,
                1 => nd::TYPE_ROUTER_ADVERTISEMENT,
                2 => nd::TYPE_NEIGHBOR_SOLICITATION,
                3 => nd::TYPE_NEIGHBOR_ADVERTISEMENT,
                4 => nd::TYPE_REDIRECT,
                _ => (rng.next_u64() & 0xFF) as u8,
            };
            let code = if rng.next_u64() % 4 == 0 {
                (rng.next_u64() & 0xFF) as u8
            } else {
                0
            };
            let hop_limit = if rng.next_u64() % 4 == 0 {
                (rng.next_u64() & 0xFF) as u8
            } else {
                nd::ND_HOP_LIMIT
            };
            let now = Duration64::from_secs(i64::try_from(step & 0x3FF).expect("bounded"));
            exercise(
                message_type,
                code,
                hop_limit,
                rng.next_u64() % 2 == 0,
                &buf[..size],
                &mut table,
                now,
            );
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    // Bit-flip a valid Neighbour Solicitation body (target + source
    // link-layer option) to walk the accepted/rejected boundary.
    let ns = NdMessage::NeighborSolicitation {
        target: Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1),
        source_ll: Some(MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01])),
    };
    let mut body = [0u8; 64];
    let len = ns.write_body(&mut body).expect("seed writes");
    let mut body = body[..len].to_vec();
    NdMessage::parse(
        nd::TYPE_NEIGHBOR_SOLICITATION,
        0,
        nd::ND_HOP_LIMIT,
        false,
        &body,
    )
    .expect("seed parses");

    let mut table = NeighborTable::new(TABLE_CAPACITY, NeighborConfig::default());
    let now = Duration64::from_secs(1);
    for byte in 0..body.len() {
        for bit in 0..8u32 {
            body[byte] ^= 1 << bit;
            for message_type in [
                nd::TYPE_ROUTER_SOLICITATION,
                nd::TYPE_ROUTER_ADVERTISEMENT,
                nd::TYPE_NEIGHBOR_SOLICITATION,
                nd::TYPE_NEIGHBOR_ADVERTISEMENT,
                nd::TYPE_REDIRECT,
            ] {
                exercise(
                    message_type,
                    0,
                    nd::ND_HOP_LIMIT,
                    false,
                    &body,
                    &mut table,
                    now,
                );
            }
            body[byte] ^= 1 << bit;
        }
    }
}
